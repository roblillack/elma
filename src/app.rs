//! Core application state and controller logic.
//!
//! [`App`] encapsulates all user interface state (selected message, scheduled
//! actions, progress indicators) and synchronises with the configured backend.
//! The type is intentionally synchronous from the TUI's perspective yet internally
//! manages asynchronous commit results via channels so the UI thread never blocks.

use crate::backend::{
    ActionStatus, BackendEvent, MailBackend, OutgoingAttachment, OutgoingMessage,
};
use crate::clock;
use crate::model::{
    Action, ActionType, MailboxKind, Message, MessageAttachment, MessageContent,
    MessageContentPart, MessageId, MessageStatus, format_size, padded_sender,
};
use anyhow::{Context, Result, anyhow};
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use shell_words::split as shell_split;
use std::{
    cmp::{max, min},
    collections::VecDeque,
    env, fs,
    io::{self, Cursor, Write},
    ops::{Deref, DerefMut},
    path::PathBuf,
    process::{Command, ExitStatus},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, TryRecvError},
    },
    thread,
    time::Instant,
};
use tdoc::{
    Document, Paragraph, Span,
    formatter::{Formatter, FormattingStyle},
    html, markdown,
    writer::Writer,
};
use tempfile::NamedTempFile;
use time::{OffsetDateTime, format_description::well_known::Rfc2822};

const PAGE_JUMP: isize = 5;
const PROGRESS_SEGMENTS: usize = 5;

/// A file this size or larger is worth asking about before it is attached.
///
/// Reading a file attaches its whole content to the message in memory, and
/// providers reject what they consider too big only after the upload finishes
/// -- Gmail's ceiling is 25 MB of *encoded* message.  Anything under this is
/// small enough that asking would only be in the way.
const LARGE_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

/// Whether the progress indicator represents a read (loading) or write (committing) operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressMode {
    /// Loading data from the backend (red text on gray background).
    Read,
    /// Committing changes to the backend (white text on red background).
    Write,
}
const ACCOUNT_SHORTCUT_KEYS: [char; 36] = [
    '1', '2', '3', '4', '5', '6', '7', '8', '9', '0', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i',
    'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z',
];

fn placeholder_id(seq: u32) -> MessageId {
    u64::MAX - seq as u64
}

fn placeholder_message(seq: u32) -> Message {
    Message {
        id: placeholder_id(seq),
        sent: OffsetDateTime::UNIX_EPOCH,
        sender: String::new(),
        recipients: Vec::new(),
        subject: String::new(),
        size: 0,
        starred: false,
        important: false,
        answered: false,
        forwarded: false,
        status: MessageStatus::Read,
        labels: Vec::new(),
        uid: 0,
        seq,
        has_attachments: false,
    }
}

fn loaded_message_count(messages: &[Message]) -> usize {
    messages.iter().filter(|msg| !msg.is_placeholder()).count()
}

fn ensure_placeholder_capacity(messages: &mut Vec<Message>, len: usize) {
    let mut current_len = messages.len();
    if current_len >= len {
        return;
    }
    while current_len < len {
        let seq = current_len as u32 + 1;
        messages.push(placeholder_message(seq));
        current_len += 1;
    }
}

fn last_loaded_index(messages: &[Message]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, msg)| (!msg.is_placeholder()).then_some(idx))
}

fn resequence_messages(messages: &mut [Message]) {
    for (index, message) in messages.iter_mut().enumerate() {
        let new_seq = index as u32 + 1;
        message.seq = new_seq;
        if message.is_placeholder() {
            message.id = placeholder_id(new_seq);
        }
    }
}

/// Data required to bootstrap an account within the application.
pub struct AccountDescriptor {
    pub name: String,
    pub backend: Arc<dyn MailBackend>,
}

impl AccountDescriptor {
    pub fn new<S: Into<String>>(name: S, backend: Arc<dyn MailBackend>) -> Self {
        Self {
            name: name.into(),
            backend,
        }
    }
}

/// Tracks the lifecycle of a batch currently being committed.
struct CommitBatchState {
    actions: Vec<Action>,
    receiver: Receiver<ActionStatus>,
    completed: usize,
    failed: Vec<(Action, String)>,
    finished: bool,
    /// When true this batch contains flag-only changes (star, important,
    /// read/unread) that were applied immediately.  Finalisation must NOT
    /// remove messages from the mailbox view for these batches.
    immediate: bool,
    /// Messages that were optimistically removed from the mailbox view when
    /// this batch was committed.  Stored so they can be re-inserted if the
    /// backend reports a failure.
    removed_messages: Vec<Message>,
}

impl CommitBatchState {
    /// Create a new batch around `actions` with progress updates streaming from `receiver`.
    fn new(actions: Vec<Action>, receiver: Receiver<ActionStatus>) -> Self {
        Self {
            actions,
            receiver,
            completed: 0,
            failed: Vec::new(),
            finished: false,
            immediate: false,
            removed_messages: Vec::new(),
        }
    }

    fn new_immediate(actions: Vec<Action>, receiver: Receiver<ActionStatus>) -> Self {
        Self {
            actions,
            receiver,
            completed: 0,
            failed: Vec::new(),
            finished: false,
            immediate: true,
            removed_messages: Vec::new(),
        }
    }

    /// Number of actions contained in the batch.
    fn len(&self) -> usize {
        self.actions.len()
    }
}

/// Aggregate progress across all active batches.
#[derive(Debug)]
struct CommitProgress {
    total: usize,
    completed: usize,
}

const MAILBOX_LOAD_CHUNK: usize = 64;

enum MailboxLoadUpdate {
    Started {
        total: usize,
    },
    Batch(Vec<Message>),
    Finished {
        events: Receiver<BackendEvent>,
        status: Option<String>,
    },
    Failed(String),
}

struct MailboxLoaderState {
    receiver: Receiver<MailboxLoadUpdate>,
}

/// How far along a background mailbox load is, in the terms the overlay uses.
///
/// Deliberately coarser than what the backend knows: everything from opening
/// the socket to receiving the first batch of headers is one wait as far as the
/// user is concerned, because [`MailBackend::load_mailbox`] does not report back
/// until it has all of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoadPhase {
    /// First contact with this account: opening a socket, authenticating and
    /// asking for the mailbox. On a cold start this is nearly all of the wait.
    Connecting,
    /// A folder being opened on an account that is already up.  Distinct from
    /// [`Self::Connecting`] because saying "connecting" on every folder switch
    /// reads as though the session were being dropped and rebuilt each time,
    /// which is not what happens: the socket stays put and only the mailbox
    /// changes.  True whether or not the backend has to quietly re-establish a
    /// stale session underneath, because it describes the request, not the
    /// transport.
    Opening,
    /// The mailbox size is known and headers are arriving.
    Receiving { loaded: usize, total: usize },
    /// The load ended without producing anything; the message is the reason.
    Failed(String),
}

/// A mailbox load the user is waiting on, described well enough to explain the
/// wait while the message list is still empty.
pub(crate) struct LoadingState {
    pub(crate) phase: LoadPhase,
    started: Instant,
}

impl LoadingState {
    pub(crate) fn in_phase(phase: LoadPhase) -> Self {
        Self {
            phase,
            started: Instant::now(),
        }
    }

    pub(crate) fn elapsed(&self) -> std::time::Duration {
        clock::elapsed(self.started)
    }
}

/// What the UI does with a message body once the worker delivers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageLoadPurpose {
    /// Show it in the message viewer.
    View,
    Reply {
        reply_all: bool,
    },
    Forward,
    /// Reopen it in compose for further editing.
    Draft,
}

impl MessageLoadPurpose {
    /// Whether compose needs the attachment bytes, which may mean a download.
    fn needs_attachments(self) -> bool {
        matches!(self, Self::Forward | Self::Draft)
    }
}

/// Everything a single background message load produces.
struct LoadedMessage {
    content: MessageContent,
    /// Attachments rebuilt for compose; empty unless the load feeds compose.
    attachments: Vec<OutgoingAttachment>,
    /// How many attachments could not be recovered at all.
    unavailable: usize,
}

/// A message body being fetched on a worker thread.
struct MessageLoadOperation {
    /// The list entry the load started from; headers are taken from here.
    message: Message,
    purpose: MessageLoadPurpose,
    /// Body the viewer had already rendered, reused so a reply quotes exactly
    /// what was on screen.
    cached_document: Option<Document>,
    receiver: Receiver<Result<LoadedMessage, String>>,
    started: Instant,
    /// What the progress indicator says while this runs.
    label: String,
}

/// Whether an in-flight submission is a send or a draft save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutgoingKind {
    Send,
    Draft,
}

impl OutgoingKind {
    fn label(self) -> &'static str {
        match self {
            Self::Send => "Sending message",
            Self::Draft => "Saving draft",
        }
    }
}

/// A composed message handed to the backend on a worker thread.
///
/// Compose stays open and locked until the worker reports back: on success it
/// closes, on failure the text is still there to retry with.
struct OutgoingOperation {
    kind: OutgoingKind,
    /// Draft this submission replaces; deleted once the backend accepts it.
    draft_id: Option<MessageId>,
    receiver: Receiver<Result<(), String>>,
    started: Instant,
}

struct SearchState {
    input: TextFieldState,
    focused: bool,
    filtered_indices: Vec<usize>,
    pre_search_selected: Option<MessageId>,
}

fn compute_filtered_indices(messages: &[Message], query: &str) -> Vec<usize> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    if terms.is_empty() {
        return (0..messages.len()).collect();
    }
    messages
        .iter()
        .enumerate()
        .filter(|(_, msg)| {
            if msg.is_placeholder() {
                return false;
            }
            let sender_lower = msg.sender.to_ascii_lowercase();
            let subject_lower = msg.subject.to_ascii_lowercase();
            let recipients_lower: String = msg
                .recipients
                .iter()
                .map(|r| r.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(" ");
            terms.iter().all(|term| {
                sender_lower.contains(term.as_str())
                    || subject_lower.contains(term.as_str())
                    || recipients_lower.contains(term.as_str())
            })
        })
        .map(|(idx, _)| idx)
        .collect()
}

/// Per-account state tracked by the UI.
pub struct AccountState {
    name: String,
    backend: Arc<dyn MailBackend>,
    mailbox: MailboxState,
    message_view: Option<MessageViewState>,
    commit_batches: VecDeque<CommitBatchState>,
    commit_progress: Option<CommitProgress>,
    mailbox_loader: Option<MailboxLoaderState>,
    mailbox_load_progress: Option<CommitProgress>,
    /// What the in-flight load is doing, for the overlay that explains the wait.
    /// Outlives `mailbox_loader` on failure so the reason stays on screen.
    loading: Option<LoadingState>,
    /// Whether a load has ever populated this account, so switching to one that
    /// failed on startup can retry rather than showing a permanently empty list.
    loaded: bool,
    /// Whether the backend has ever answered for this account, which is the
    /// point from which there is a session to reuse.  Earlier than `loaded`:
    /// that one waits for the whole mailbox, this one only for the first sign
    /// the server is there, which is what decides whether the next load is a
    /// connect or just a folder change.
    connected: bool,
    /// Message body being fetched for this account, if any.
    message_loader: Option<MessageLoadOperation>,
    scheduled_actions: Vec<Action>,
    current_mailbox: MailboxKind,
    search: Option<SearchState>,
}

/// Which screen the UI is currently rendering.
pub enum ActiveView {
    Mailbox,
    Message,
    Compose,
}

/// Result of attempting to schedule a move/delete action.
#[derive(Debug, PartialEq, Eq)]
enum ScheduleOutcome {
    /// New action was added to the queue.
    Added,
    /// An existing action for the same message was replaced with this one.
    Replaced,
    /// The same action was already scheduled; no change made.
    AlreadyScheduled,
}

#[derive(Clone, Copy)]
enum NavigationTarget {
    Mailbox(MailboxKind),
    Account(usize),
}

#[derive(Clone, Copy)]
struct PendingNavigation {
    target: NavigationTarget,
}

/// Central controller that wires backend state into the TUI.
///
/// `App` owns the inbox cache, scheduled actions, and message viewer.  It reacts
/// to backend events every tick, meaning the UI can remain event-driven without
/// juggling shared mutable state.
///
/// # Examples
///
/// ```
/// # use elma_rs::app::App;
/// # use elma_rs::backend::mock::MockBackend;
/// # fn demo() -> anyhow::Result<()> {
/// let backend = Box::new(MockBackend::default());
/// let mut app = App::new(backend)?;
/// // Drive a single key press; normally the main loop handles this.
/// use crossterm::event::{KeyCode, KeyEvent};
/// app.handle_key(KeyEvent::from(KeyCode::Char('$')))?;
/// # Ok(()) }
/// ```
pub struct App {
    accounts: Vec<AccountState>,
    active_account: usize,
    compose: Option<ComposeState>,
    should_quit: bool,
    pending_shortcut: Option<ShortcutMenuState>,
    pending_navigation: Option<PendingNavigation>,
    save_attachment: Option<SaveAttachmentDialog>,
    /// Send or draft-save running on a worker thread.
    outgoing: Option<OutgoingOperation>,
    /// Set when something took the screen away from the renderer, so the next
    /// frame has to be painted in full rather than diffed against a screen that
    /// no longer holds what the renderer last drew.
    needs_full_redraw: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveAttachmentFocus {
    Folder,
    List,
}

pub(crate) struct SaveAttachmentDialog {
    folder: TextFieldState,
    selected: usize,
    focus: SaveAttachmentFocus,
    status: Option<String>,
    operation: Option<SaveAttachmentOperation>,
}

struct SaveAttachmentOperation {
    receiver: Receiver<Result<PathBuf, String>>,
    started: Instant,
    filename: String,
    /// Set when the user dismisses the dialog so the worker stops before it
    /// writes anything to disk.
    cancel: Arc<AtomicBool>,
}

impl SaveAttachmentDialog {
    fn new(folder: String) -> Self {
        let mut field = TextFieldState::default();
        field.value = folder;
        field.cursor = text_len(&field.value);
        Self {
            folder: field,
            selected: 0,
            focus: SaveAttachmentFocus::List,
            status: None,
            operation: None,
        }
    }

    /// Folder value and cursor position, shaped like
    /// [`ComposeState::field_data`] so both feed the same field renderer.
    pub(crate) fn folder_data(&self) -> (&str, usize) {
        (&self.folder.value[..], self.folder.cursor)
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn focus(&self) -> SaveAttachmentFocus {
        self.focus
    }

    pub(crate) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub(crate) fn active_operation(&self) -> Option<(&str, std::time::Duration)> {
        self.operation
            .as_ref()
            .map(|op| (op.filename.as_str(), clock::elapsed(op.started)))
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.operation.is_some()
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            SaveAttachmentFocus::Folder => SaveAttachmentFocus::List,
            SaveAttachmentFocus::List => SaveAttachmentFocus::Folder,
        };
    }

    fn set_status<S: Into<String>>(&mut self, status: S) {
        self.status = Some(status.into());
    }

    fn clear_status(&mut self) {
        self.status = None;
    }

    /// Ask the running save operation to stop before it touches the disk.
    ///
    /// The download itself cannot be interrupted, but the worker checks this
    /// flag before writing, so dismissing the dialog leaves no partial file
    /// behind.
    fn cancel_operation(&mut self) {
        if let Some(op) = self.operation.as_ref() {
            op.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Cached inbox view derived from backend events.
struct MailboxState {
    messages: Vec<Message>,
    selected: Option<usize>,
    events: Receiver<BackendEvent>,
    event_count: usize,
    status_line: Option<String>,
    scroll_top: usize,
}

/// Snapshot of the currently opened message.
pub(crate) struct MessageViewState {
    pub(crate) message_id: MessageId,
    pub(crate) message_index: usize,
    pub(crate) message: Message,
    pub(crate) content: MessageContent,
    pub(crate) document: Option<Document>,
    pub(crate) raw_html: Option<String>,
    pub(crate) scroll: u16,
    pub(crate) unformatted: bool,
    pub(crate) info_line: Option<String>,
    pub(crate) read_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeField {
    To,
    Cc,
    Bcc,
    Subject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeButton {
    Attach,
    Cancel,
    Edit,
    Draft,
    Send,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeFocus {
    Field(ComposeField),
    Attachments,
    Body,
    Button(ComposeButton),
}

#[derive(Default)]
struct TextFieldState {
    value: String,
    cursor: usize,
}

/// One attach request -- a path from the prompt, or a whole terminal drop --
/// working its way into the message.
///
/// A drop can hold several files and any of them may be big enough to ask
/// about, so the request cannot be finished in one go: it parks in `asking`
/// until the user answers, and the tallies survive that wait so the closing
/// summary can still say what the whole request did.
#[derive(Default)]
struct AttachBatch {
    /// Paths not looked at yet.
    queue: VecDeque<String>,
    /// The oversized file waiting for a yes or no, with its size on disk.
    asking: Option<(String, u64)>,
    attached: usize,
    /// Bytes attached so far, for the summary.
    bytes: usize,
    declined: usize,
}

pub(crate) struct ComposeState {
    to: TextFieldState,
    cc: TextFieldState,
    bcc: TextFieldState,
    subject: TextFieldState,
    attachments: Vec<OutgoingAttachment>,
    attachment_selected: usize,
    attachment_prompt: Option<TextFieldState>,
    attach: AttachBatch,
    body: Document,
    /// Serialized size of [`Self::body`], measured whenever it changes.
    ///
    /// Serializing renders the whole document, and the compose dialog redraws
    /// on every tick, so the size cannot be computed where it is displayed.
    body_bytes: usize,
    draft_id: Option<MessageId>,
    focus: ComposeFocus,
    status: Option<String>,
    body_scroll: usize,
    body_view_height: usize,
}

impl Default for ComposeState {
    fn default() -> Self {
        Self {
            to: TextFieldState::default(),
            cc: TextFieldState::default(),
            bcc: TextFieldState::default(),
            subject: TextFieldState::default(),
            attachments: Vec::new(),
            attachment_selected: 0,
            attachment_prompt: None,
            attach: AttachBatch::default(),
            body: Document::new(),
            body_bytes: 0,
            draft_id: None,
            focus: ComposeFocus::Field(ComposeField::To),
            status: None,
            body_scroll: 0,
            body_view_height: 0,
        }
    }
}

const COMPOSE_BUTTON_SEQUENCE: [ComposeButton; 5] = [
    ComposeButton::Attach,
    ComposeButton::Cancel,
    ComposeButton::Edit,
    ComposeButton::Draft,
    ComposeButton::Send,
];

impl ComposeState {
    fn new() -> Self {
        Self::default()
    }

    fn from_draft(
        draft_id: MessageId,
        to: String,
        cc: String,
        bcc: String,
        subject: String,
        body: Document,
    ) -> Self {
        let mut state = Self {
            draft_id: Some(draft_id),
            ..Default::default()
        };
        state.to.value = to;
        state.to.cursor = text_len(&state.to.value);
        state.cc.value = cc;
        state.cc.cursor = text_len(&state.cc.value);
        state.bcc.value = bcc;
        state.bcc.cursor = text_len(&state.bcc.value);
        state.subject.value = subject;
        state.subject.cursor = text_len(&state.subject.value);
        state.assign_body(body);
        state.body_scroll = 0;
        state.body_view_height = 0;
        state.focus = ComposeFocus::Body;
        state
    }

    pub(crate) fn focus(&self) -> ComposeFocus {
        self.focus
    }

    fn set_focus(&mut self, focus: ComposeFocus) {
        self.focus = focus;
    }

    fn focus_sequence(&self) -> Vec<ComposeFocus> {
        let mut sequence = vec![
            ComposeFocus::Field(ComposeField::To),
            ComposeFocus::Field(ComposeField::Cc),
            ComposeFocus::Field(ComposeField::Bcc),
            ComposeFocus::Field(ComposeField::Subject),
        ];
        if !self.attachments.is_empty() {
            sequence.push(ComposeFocus::Attachments);
        }
        sequence.push(ComposeFocus::Body);
        for button in COMPOSE_BUTTON_SEQUENCE {
            sequence.push(ComposeFocus::Button(button));
        }
        sequence
    }

    fn focus_next(&mut self) {
        let sequence = self.focus_sequence();
        let current_idx = sequence
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = (current_idx + 1) % sequence.len();
        self.focus = sequence[next];
    }

    fn focus_prev(&mut self) {
        let sequence = self.focus_sequence();
        let current_idx = sequence
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let prev = if current_idx == 0 {
            sequence.len() - 1
        } else {
            current_idx - 1
        };
        self.focus = sequence[prev];
    }

    fn focus_button_next(&mut self) {
        let next = match self.focus {
            ComposeFocus::Button(current) => {
                let index = COMPOSE_BUTTON_SEQUENCE
                    .iter()
                    .position(|button| *button == current)
                    .unwrap_or(0);
                COMPOSE_BUTTON_SEQUENCE[(index + 1) % COMPOSE_BUTTON_SEQUENCE.len()]
            }
            _ => COMPOSE_BUTTON_SEQUENCE[0],
        };
        self.focus = ComposeFocus::Button(next);
    }

    fn focus_button_prev(&mut self) {
        let prev = match self.focus {
            ComposeFocus::Button(current) => {
                let index = COMPOSE_BUTTON_SEQUENCE
                    .iter()
                    .position(|button| *button == current)
                    .unwrap_or(0);
                if index == 0 {
                    *COMPOSE_BUTTON_SEQUENCE.last().unwrap()
                } else {
                    COMPOSE_BUTTON_SEQUENCE[index - 1]
                }
            }
            _ => *COMPOSE_BUTTON_SEQUENCE.last().unwrap(),
        };
        self.focus = ComposeFocus::Button(prev);
    }

    fn clear_status(&mut self) {
        self.status = None;
    }

    fn set_status<S: Into<String>>(&mut self, status: S) {
        self.status = Some(status.into());
    }

    pub(crate) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub(crate) fn draft_id(&self) -> Option<MessageId> {
        self.draft_id
    }

    pub(crate) fn is_editing_draft(&self) -> bool {
        self.draft_id.is_some()
    }

    pub(crate) fn field_data(&self, field: ComposeField) -> (&str, usize) {
        match field {
            ComposeField::To => (&self.to.value[..], self.to.cursor),
            ComposeField::Cc => (&self.cc.value[..], self.cc.cursor),
            ComposeField::Bcc => (&self.bcc.value[..], self.bcc.cursor),
            ComposeField::Subject => (&self.subject.value[..], self.subject.cursor),
        }
    }

    pub(crate) fn is_field_focused(&self, field: ComposeField) -> bool {
        matches!(self.focus, ComposeFocus::Field(active) if active == field)
    }

    fn field_state_mut(&mut self, field: ComposeField) -> &mut TextFieldState {
        match field {
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Bcc => &mut self.bcc,
            ComposeField::Subject => &mut self.subject,
        }
    }

    fn serialize_body_markdown(&self) -> Result<String> {
        let mut buffer = Vec::new();
        markdown::write(&mut buffer, &self.body)
            .context("failed to convert FTML body to Markdown")?;
        String::from_utf8(buffer).context("Markdown serialization produced invalid UTF-8")
    }

    fn serialize_body_plain(&self) -> Result<String> {
        document_to_plain_text(&self.body)
    }

    fn serialize_body_html(&self) -> Result<String> {
        let writer = Writer::new();
        writer
            .write_to_string(&self.body)
            .context("failed to convert FTML body to HTML")
    }

    pub(crate) fn to_outgoing(&self) -> Result<OutgoingMessage> {
        let text_body = self.serialize_body_plain()?;
        let html_body = self.serialize_body_html()?;
        Ok(OutgoingMessage {
            to: split_addresses(&self.to.value),
            cc: split_addresses(&self.cc.value),
            bcc: split_addresses(&self.bcc.value),
            subject: self.subject.value.clone(),
            text_body,
            html_body,
            attachments: self.attachments.clone(),
        })
    }

    pub(crate) fn attachments(&self) -> &[OutgoingAttachment] {
        &self.attachments
    }

    /// Rough size of the message as it will go on the wire.
    ///
    /// Attachments are base64-encoded when the MIME body is built, and the
    /// encoded figure is the one a provider measures against its limit, so
    /// showing what the files take on disk would understate the message by a
    /// third.  Headers and MIME boundaries are left out: they are noise next to
    /// anything worth watching the size of.
    pub(crate) fn message_size(&self) -> usize {
        let attachments: usize = self
            .attachments
            .iter()
            .map(|attachment| base64_size(attachment.size()))
            .sum();
        self.body_bytes + attachments
    }

    /// The oversized file waiting for an answer: its name, its size on disk,
    /// and what the message would weigh once it is in.
    pub(crate) fn large_attachment_question(&self) -> Option<(String, usize, usize)> {
        self.attach.asking.as_ref().map(|(path, size)| {
            let size = *size as usize;
            let expanded = expand_user_path(path);
            let name = expanded
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            (name, size, self.message_size() + base64_size(size))
        })
    }

    fn is_asking_about_large_attachment(&self) -> bool {
        self.attach.asking.is_some()
    }

    pub(crate) fn is_attachments_focused(&self) -> bool {
        matches!(self.focus, ComposeFocus::Attachments)
    }

    pub(crate) fn attachment_selected(&self) -> Option<usize> {
        if self.attachments.is_empty() {
            None
        } else {
            Some(self.attachment_selected.min(self.attachments.len() - 1))
        }
    }

    pub(crate) fn attachment_prompt(&self) -> Option<(&str, usize)> {
        self.attachment_prompt
            .as_ref()
            .map(|state| (state.value.as_str(), state.cursor))
    }

    pub(crate) fn is_attachment_prompt_active(&self) -> bool {
        self.attachment_prompt.is_some()
    }

    fn attachment_prompt_mut(&mut self) -> Option<&mut TextFieldState> {
        self.attachment_prompt.as_mut()
    }

    fn open_attachment_prompt(&mut self) {
        self.attachment_prompt = Some(TextFieldState::default());
    }

    fn close_attachment_prompt(&mut self) {
        self.attachment_prompt = None;
    }

    fn add_attachment(&mut self, attachment: OutgoingAttachment) {
        self.attachments.push(attachment);
        self.attachment_selected = self.attachments.len() - 1;
    }

    /// Replace the attachment list, used when seeding compose from an existing
    /// message (reopening a draft, forwarding).
    fn set_attachments(&mut self, attachments: Vec<OutgoingAttachment>) {
        self.attachments = attachments;
        self.attachment_selected = 0;
    }

    fn remove_selected_attachment(&mut self) -> Option<OutgoingAttachment> {
        if self.attachments.is_empty() {
            return None;
        }
        let idx = self.attachment_selected.min(self.attachments.len() - 1);
        let removed = self.attachments.remove(idx);
        if self.attachments.is_empty() {
            self.attachment_selected = 0;
            if matches!(self.focus, ComposeFocus::Attachments) {
                self.focus = ComposeFocus::Body;
            }
        } else if self.attachment_selected >= self.attachments.len() {
            self.attachment_selected = self.attachments.len() - 1;
        }
        Some(removed)
    }

    fn select_attachment_next(&mut self) -> bool {
        if self.attachments.is_empty() {
            return false;
        }
        if self.attachment_selected + 1 < self.attachments.len() {
            self.attachment_selected += 1;
            true
        } else {
            false
        }
    }

    fn select_attachment_prev(&mut self) -> bool {
        if self.attachments.is_empty() {
            return false;
        }
        if self.attachment_selected > 0 {
            self.attachment_selected -= 1;
            true
        } else {
            false
        }
    }

    fn select_attachment_first(&mut self) {
        self.attachment_selected = 0;
    }

    fn select_attachment_last(&mut self) {
        if !self.attachments.is_empty() {
            self.attachment_selected = self.attachments.len() - 1;
        }
    }

    pub(crate) fn body(&self) -> &Document {
        &self.body
    }

    pub(crate) fn set_body(&mut self, document: Document) {
        self.assign_body(document);
        self.body_scroll = 0;
        self.body_view_height = 0;
    }

    /// Replace the body, keeping its measured size in step.
    ///
    /// The one place the document is assigned, so [`Self::body_bytes`] cannot
    /// drift away from what it describes.  A body that fails to serialize
    /// counts as nothing rather than aborting the edit -- the number is a
    /// display aid, and [`Self::to_outgoing`] reports the real failure when the
    /// message is actually sent.
    fn assign_body(&mut self, document: Document) {
        self.body = document;
        let plain = self.serialize_body_plain().map_or(0, |text| text.len());
        let html = self.serialize_body_html().map_or(0, |text| text.len());
        self.body_bytes = plain + html;
    }

    pub(crate) fn set_field_text<S: Into<String>>(&mut self, field: ComposeField, value: S) {
        let text = value.into();
        let state = self.field_state_mut(field);
        state.value = text;
        state.cursor = text_len(&state.value);
    }

    pub(crate) fn is_body_focused(&self) -> bool {
        matches!(self.focus, ComposeFocus::Body)
    }

    pub(crate) fn body_markdown(&self) -> Result<String> {
        self.serialize_body_markdown()
    }

    pub(crate) fn update_body_from_markdown(&mut self, source: &str) -> Result<()> {
        let document = markdown::parse(Cursor::new(source))
            .map_err(|err| anyhow!("Failed to parse Markdown: {err}"))?;
        self.assign_body(document);
        self.body_scroll = 0;
        Ok(())
    }

    pub(crate) fn body_scroll(&self) -> usize {
        self.body_scroll
    }

    pub(crate) fn set_body_scroll(&mut self, value: usize) {
        self.body_scroll = value;
    }

    pub(crate) fn set_body_view_height(&mut self, height: usize) {
        self.body_view_height = height;
        if height == 0 {
            self.body_scroll = 0;
        }
    }

    pub(crate) fn scroll_body_lines(&mut self, delta: isize) {
        let current = self.body_scroll as isize;
        let next = (current + delta).max(0);
        self.body_scroll = next as usize;
    }

    pub(crate) fn scroll_body_pages(&mut self, pages: isize) {
        let page = self.body_view_height.max(1) as isize;
        let delta = page.saturating_mul(pages);
        self.scroll_body_lines(delta);
    }
}

impl TextFieldState {
    fn insert(&mut self, ch: char) -> bool {
        if ch == '\n' {
            return false;
        }
        insert_char_at(&mut self.value, &mut self.cursor, ch);
        true
    }

    /// Insert pasted text at the cursor.
    ///
    /// These are single-line fields, so newlines become spaces (pasting a
    /// two-line address block should not glue the lines together) and other
    /// control characters are dropped. Enclosing line breaks are stripped --
    /// terminals routinely append one to a paste -- but spaces are kept, since
    /// a trailing space in a pasted `"addr, "` is deliberate.
    fn insert_str(&mut self, text: &str) -> bool {
        let mut inserted = false;
        let mut pending_space = false;
        for ch in text.trim_matches(['\r', '\n']).chars() {
            let ch = if ch == '\n' || ch == '\r' || ch == '\t' {
                // Collapse runs of line breaks into a single space.
                if pending_space {
                    continue;
                }
                pending_space = true;
                ' '
            } else if ch.is_control() {
                continue;
            } else {
                pending_space = false;
                ch
            };

            if self.insert(ch) {
                inserted = true;
            }
        }
        inserted
    }

    fn backspace(&mut self) -> bool {
        remove_char_before(&mut self.value, &mut self.cursor)
    }

    fn delete(&mut self) -> bool {
        remove_char_at(&mut self.value, &mut self.cursor)
    }

    fn move_left(&mut self) -> bool {
        move_cursor_left(&mut self.cursor)
    }

    fn move_right(&mut self) -> bool {
        let mut cursor = self.cursor;
        let moved = move_cursor_right(&self.value, &mut cursor);
        if moved {
            self.cursor = cursor;
        }
        moved
    }

    fn move_home(&mut self) -> bool {
        move_cursor_home(&mut self.cursor)
    }

    fn move_end(&mut self) -> bool {
        let mut cursor = self.cursor;
        let moved = move_cursor_end(&self.value, &mut cursor);
        if moved {
            self.cursor = cursor;
        }
        moved
    }
}

fn document_from_message_content(content: &MessageContent) -> Document {
    if let Some(html_part) = content
        .parts
        .iter()
        .find(|part| mime_type_matches(part, "text/html"))
    {
        let html = String::from_utf8_lossy(&html_part.content);
        if let Ok(document) = html::parse(Cursor::new(html.as_ref())) {
            return document;
        }
    }

    if let Some(plain_part) = content
        .parts
        .iter()
        .find(|part| mime_type_matches(part, "text/plain"))
    {
        let text = String::from_utf8_lossy(&plain_part.content);
        if let Ok(document) = markdown::parse(Cursor::new(text.as_ref())) {
            return document;
        }

        let mut paragraph = Paragraph::new_text();
        paragraph
            .content_mut()
            .push(Span::new_text(text.into_owned()));
        return Document::new().with_paragraphs(vec![paragraph]);
    }

    Document::new()
}

fn document_to_plain_text(document: &Document) -> Result<String> {
    let mut buffer = Vec::new();
    {
        let mut formatter = Formatter::new_ascii(&mut buffer);
        formatter.style = plain_text_style();
        formatter
            .write_document(document)
            .context("failed to render FTML document as plain text")?;
    }
    String::from_utf8(buffer).context("plain text serialization produced invalid UTF-8")
}

fn plain_text_style() -> FormattingStyle {
    let mut style = FormattingStyle::ascii();
    style.wrap_width = 80;
    style.enable_osc8_hyperlinks = false;
    style
}

fn mime_type_matches(part: &MessageContentPart, expected: &str) -> bool {
    part.content_type
        .split(';')
        .next()
        .map(|value| value.trim())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn prefix_subject(subject: &str, prefix: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.is_empty() {
        return prefix.trim_end().to_string();
    }

    let prefix_lower = prefix.to_ascii_lowercase();
    let subject_lower = trimmed.to_ascii_lowercase();
    if subject_lower.starts_with(&prefix_lower) {
        trimmed.to_string()
    } else {
        format!("{} {}", prefix.trim_end(), trimmed)
    }
}

fn text_paragraph(content: impl Into<String>) -> Paragraph {
    Paragraph::new_text().with_content(vec![Span::new_text(content.into())])
}

fn build_reply_document(original: &Document, sender: &str, sent: OffsetDateTime) -> Document {
    let mut document = Document::new();
    document.add_paragraph(Paragraph::new_text());

    let date_str = sent.format(&Rfc2822).unwrap_or_else(|_| sent.to_string());
    let header = if sender.trim().is_empty() {
        format!("On {date_str}, the sender wrote:")
    } else {
        format!("On {date_str}, {sender} wrote:")
    };

    document.add_paragraph(text_paragraph(header));
    document.add_paragraph(Paragraph::new_text());

    let quote = Paragraph::new_quote().with_children(original.paragraphs.clone());
    document.add_paragraph(quote);

    document
}

fn build_forward_document(original: &Document, message: &Message) -> Document {
    let mut document = Document::new();
    document.add_paragraph(Paragraph::new_text());
    document.add_paragraph(text_paragraph("---------- Forwarded message ---------"));

    if !message.sender.trim().is_empty() {
        document.add_paragraph(text_paragraph(format!("From: {}", message.sender.trim())));
    }

    let date_str = message
        .sent
        .format(&Rfc2822)
        .unwrap_or_else(|_| message.sent.to_string());
    document.add_paragraph(text_paragraph(format!("Date: {date_str}")));

    if !message.recipients.is_empty() {
        document.add_paragraph(text_paragraph(format!(
            "To: {}",
            message.recipients.join(", ")
        )));
    }

    if !message.subject.trim().is_empty() {
        document.add_paragraph(text_paragraph(format!(
            "Subject: {}",
            message.subject.trim()
        )));
    }

    document.add_paragraph(Paragraph::new_text());
    let quote = Paragraph::new_quote().with_children(original.paragraphs.clone());
    document.add_paragraph(quote);

    document
}

fn split_addresses(input: &str) -> Vec<String> {
    input
        .split([',', ';'])
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect()
}

fn text_len(text: &str) -> usize {
    text.chars().count()
}

pub(crate) fn byte_index_for(text: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }

    for (count, (idx, _)) in text.char_indices().enumerate() {
        if count == char_index {
            return idx;
        }
    }

    text.len()
}

fn insert_char_at(text: &mut String, cursor: &mut usize, ch: char) {
    let idx = byte_index_for(text, *cursor);
    text.insert(idx, ch);
    *cursor += 1;
}

fn remove_char_before(text: &mut String, cursor: &mut usize) -> bool {
    if *cursor == 0 {
        return false;
    }
    let start_idx = byte_index_for(text, *cursor - 1);
    let end_idx = byte_index_for(text, *cursor);
    if start_idx < end_idx {
        text.drain(start_idx..end_idx);
        *cursor -= 1;
        true
    } else {
        false
    }
}

fn remove_char_at(text: &mut String, cursor: &mut usize) -> bool {
    let len = text_len(text);
    if *cursor >= len {
        return false;
    }
    let start_idx = byte_index_for(text, *cursor);
    let end_idx = byte_index_for(text, *cursor + 1);
    if start_idx < end_idx {
        text.drain(start_idx..end_idx);
        true
    } else {
        false
    }
}

fn move_cursor_left(cursor: &mut usize) -> bool {
    if *cursor == 0 {
        false
    } else {
        *cursor -= 1;
        true
    }
}

fn move_cursor_right(text: &str, cursor: &mut usize) -> bool {
    let len = text_len(text);
    if *cursor >= len {
        false
    } else {
        *cursor += 1;
        true
    }
}

fn move_cursor_home(cursor: &mut usize) -> bool {
    if *cursor == 0 {
        false
    } else {
        *cursor = 0;
        true
    }
}

fn move_cursor_end(text: &str, cursor: &mut usize) -> bool {
    let len = text_len(text);
    if *cursor == len {
        false
    } else {
        *cursor = len;
        true
    }
}

struct ShortcutMenuState {
    menu: ShortcutMenu,
}

pub(crate) struct ShortcutMenu {
    title: &'static str,
    items: Vec<ShortcutItem>,
}

#[derive(Clone)]
struct ShortcutItem {
    key: char,
    description: String,
    action: ShortcutAction,
}

#[derive(Clone, Copy)]
enum ShortcutAction {
    SwitchMailbox(MailboxKind),
    SwitchAccount(usize),
}

pub(crate) struct ShortcutEntry<'a> {
    pub(crate) key: char,
    pub(crate) description: &'a str,
}

impl ShortcutMenuState {
    fn mailbox_menu() -> Self {
        let items = vec![
            ShortcutItem::mailbox('i', "Inbox", MailboxKind::Inbox),
            ShortcutItem::mailbox('s', "Starred", MailboxKind::Starred),
            ShortcutItem::mailbox('I', "Important", MailboxKind::Important),
            ShortcutItem::mailbox('t', "Sent", MailboxKind::Sent),
            ShortcutItem::mailbox('d', "Drafts", MailboxKind::Drafts),
            ShortcutItem::mailbox('a', "Archive", MailboxKind::Archive),
            ShortcutItem::mailbox('S', "Spam", MailboxKind::Spam),
            ShortcutItem::mailbox('T', "Trash", MailboxKind::Trash),
        ];

        Self {
            menu: ShortcutMenu {
                title: "Go to",
                items,
            },
        }
    }

    fn account_menu(items: Vec<ShortcutItem>) -> Self {
        Self {
            menu: ShortcutMenu {
                title: "Go to account",
                items,
            },
        }
    }

    fn menu(&self) -> &ShortcutMenu {
        &self.menu
    }

    fn action_for(&self, key: char) -> Option<ShortcutAction> {
        self.menu
            .items
            .iter()
            .find(|item| item.matches(key))
            .map(|item| item.action)
    }
}

impl ShortcutMenu {
    pub(crate) fn title(&self) -> &'static str {
        self.title
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = ShortcutEntry<'_>> + '_ {
        self.items.iter().map(|item| ShortcutEntry {
            key: item.key(),
            description: item.description(),
        })
    }
}

impl ShortcutItem {
    fn mailbox(key: char, description: &'static str, mailbox: MailboxKind) -> Self {
        Self {
            key,
            description: description.to_string(),
            action: ShortcutAction::SwitchMailbox(mailbox),
        }
    }

    fn matches(&self, key: char) -> bool {
        self.key == key
    }

    fn key(&self) -> char {
        self.key
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn account(key: char, description: String, index: usize) -> Self {
        Self {
            key,
            description,
            action: ShortcutAction::SwitchAccount(index),
        }
    }
}
impl App {
    /// Build the application state around the configured accounts.
    /// Build the application state and start loading every account.
    ///
    /// Nothing here touches the network.  Each account begins empty with its
    /// inbox load already running on its own thread, so the caller can put a
    /// frame on screen immediately and the accounts connect in parallel instead
    /// of one after another.  Until a load reports back, the account shows the
    /// overlay from [`Self::loading_overlay`] rather than an empty list.
    pub fn new(descriptors: Vec<AccountDescriptor>) -> Result<Self> {
        if descriptors.is_empty() {
            return Err(anyhow!("no accounts configured"));
        }

        let accounts = descriptors
            .into_iter()
            .map(|descriptor| {
                // Replaced by the loader's channel once the mailbox is up; until
                // then a disconnected receiver just yields no events.
                let (_tx, placeholder_rx) = std::sync::mpsc::channel();
                AccountState {
                    name: descriptor.name,
                    backend: descriptor.backend,
                    mailbox: MailboxState {
                        messages: Vec::new(),
                        selected: None,
                        events: placeholder_rx,
                        event_count: 0,
                        status_line: None,
                        scroll_top: 0,
                    },
                    message_view: None,
                    commit_batches: VecDeque::new(),
                    commit_progress: None,
                    mailbox_loader: None,
                    mailbox_load_progress: None,
                    loading: None,
                    loaded: false,
                    connected: false,
                    message_loader: None,
                    scheduled_actions: Vec::new(),
                    current_mailbox: MailboxKind::Inbox,
                    search: None,
                }
            })
            .collect::<Vec<_>>();

        let mut app = Self {
            accounts,
            active_account: 0,
            compose: None,
            should_quit: false,
            pending_shortcut: None,
            pending_navigation: None,
            save_attachment: None,
            outgoing: None,
            needs_full_redraw: false,
        };

        for index in 0..app.accounts.len() {
            app.begin_mailbox_load_for(index, MailboxKind::Inbox, None)?;
        }

        Ok(app)
    }

    /// Whether the screen has to be repainted from scratch, clearing the request.
    ///
    /// The renderer only writes the cells that changed since the last frame, so
    /// anything that hands the terminal to another program -- the editor -- has
    /// to say so, or the first frame back paints a diff against a screen that
    /// was wiped meanwhile.
    pub fn take_full_redraw(&mut self) -> bool {
        std::mem::take(&mut self.needs_full_redraw)
    }

    fn current_account(&self) -> &AccountState {
        &self.accounts[self.active_account]
    }

    fn current_account_mut(&mut self) -> &mut AccountState {
        &mut self.accounts[self.active_account]
    }

    fn mailbox_mut(&mut self) -> &mut MailboxState {
        &mut self.current_account_mut().mailbox
    }

    fn set_message_view(&mut self, view: Option<MessageViewState>) {
        self.current_account_mut().message_view = view;
    }

    /// Whether the main loop should terminate.
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Determine which view should be rendered by the UI.
    pub fn active_view(&self) -> ActiveView {
        if self.compose.is_some() {
            ActiveView::Compose
        } else if self.current_account().message_view.is_some() {
            ActiveView::Message
        } else {
            ActiveView::Mailbox
        }
    }

    /// Entry point for keyboard handling used by the main event loop.
    ///
    /// The method polls backend state before dispatching so the UI reacts to new
    /// messages even while the user is idle.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        self.poll_backend_events();

        if self.process_pending_navigation(key)? {
            return Ok(());
        }

        if self.save_attachment.is_some() {
            return self.handle_save_attachment_key(key);
        }

        let active_view = self.active_view();
        if matches!(active_view, ActiveView::Compose) {
            return self.handle_compose_key(key);
        }

        // Route keys to the focused search input before anything else.
        if matches!(active_view, ActiveView::Mailbox)
            && self.search.as_ref().is_some_and(|s| s.focused)
        {
            return self.handle_search_key(key);
        }

        if self.process_pending_shortcut(key)? {
            return Ok(());
        }

        if self.pending_shortcut.is_none()
            && self.compose.is_none()
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            match key.code {
                KeyCode::Char('g') => {
                    self.open_mailbox_menu();
                    return Ok(());
                }
                KeyCode::Char('G') => {
                    self.open_account_menu();
                    return Ok(());
                }
                _ => {}
            }
        }

        match self.active_view() {
            ActiveView::Mailbox => self.handle_mailbox_key(key),
            ActiveView::Message => self.handle_message_key(key),
            ActiveView::Compose => self.handle_compose_key(key),
        }
    }

    fn process_pending_shortcut(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(state) = self.pending_shortcut.as_ref() else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => {
                self.pending_shortcut = None;
                self.current_account_mut().mailbox.status_line =
                    Some("Shortcut cancelled.".to_string());
                Ok(true)
            }
            KeyCode::Char(ch) => {
                let action = state.action_for(ch);
                self.pending_shortcut = None;
                match action {
                    Some(ShortcutAction::SwitchMailbox(target)) => {
                        self.begin_navigation(NavigationTarget::Mailbox(target))?;
                    }
                    Some(ShortcutAction::SwitchAccount(index)) => {
                        self.begin_navigation(NavigationTarget::Account(index))?;
                    }
                    None => {
                        self.current_account_mut().mailbox.status_line =
                            Some(format!("Unknown go to target: {ch}"));
                    }
                }
                Ok(true)
            }
            _ => {
                self.pending_shortcut = None;
                Ok(true)
            }
        }
    }

    fn open_mailbox_menu(&mut self) {
        self.pending_shortcut = Some(ShortcutMenuState::mailbox_menu());
        self.current_account_mut().mailbox.status_line =
            Some("Go to: press the highlighted key.".to_string());
    }

    fn open_account_menu(&mut self) {
        let items: Vec<ShortcutItem> = self
            .accounts
            .iter()
            .enumerate()
            .zip(ACCOUNT_SHORTCUT_KEYS.iter())
            .map(|((idx, account), key)| {
                let mut description = account.name.clone();
                if idx == self.active_account {
                    description.push_str(" (current)");
                }
                ShortcutItem::account(*key, description, idx)
            })
            .collect();

        if items.is_empty() {
            self.current_account_mut().mailbox.status_line =
                Some("No accounts configured.".to_string());
            return;
        }

        if self.accounts.len() > ACCOUNT_SHORTCUT_KEYS.len() {
            self.current_account_mut().mailbox.status_line = Some(format!(
                "Showing first {} accounts. Press highlighted key.",
                ACCOUNT_SHORTCUT_KEYS.len()
            ));
        } else {
            self.current_account_mut().mailbox.status_line =
                Some("Switch account: press the highlighted key.".to_string());
        }

        self.pending_shortcut = Some(ShortcutMenuState::account_menu(items));
    }

    fn process_pending_navigation(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(pending) = self.pending_navigation else {
            return Ok(false);
        };

        if self.scheduled_actions.is_empty() {
            self.pending_navigation = None;
            self.execute_navigation(pending.target)?;
            return Ok(true);
        }

        match key.code {
            KeyCode::Esc => {
                self.pending_navigation = None;
                self.mailbox.status_line = Some("Switch cancelled.".to_string());
                Ok(true)
            }
            KeyCode::Char('y' | 'Y') => {
                let queued = self.scheduled_actions.len();
                self.commit_actions()?;
                self.pending_navigation = None;
                self.execute_navigation(pending.target)?;
                if queued > 0 {
                    let message = match pending.target {
                        NavigationTarget::Mailbox(kind) => {
                            format!("Queued {queued} actions; opened {kind}.")
                        }
                        NavigationTarget::Account(_) => {
                            format!("Queued {queued} actions; switched to {}.", self.name)
                        }
                    };
                    self.mailbox.status_line = Some(message);
                }
                Ok(true)
            }
            KeyCode::Char('n' | 'N') => {
                let target = pending.target;
                let discarded = self.discard_scheduled_actions()?;
                self.pending_navigation = None;
                self.execute_navigation(target)?;
                if discarded > 0 {
                    let message = match target {
                        NavigationTarget::Mailbox(kind) => {
                            format!("Discarded {discarded} scheduled actions; opened {kind}.")
                        }
                        NavigationTarget::Account(_) => {
                            format!(
                                "Discarded {discarded} scheduled actions; switched to {}.",
                                self.name
                            )
                        }
                    };
                    self.mailbox.status_line = Some(message);
                }
                Ok(true)
            }
            _ => {
                self.mailbox
                    .status_line
                    .get_or_insert_with(|| "Press y/n or Esc.".to_string());
                Ok(true)
            }
        }
    }

    fn begin_navigation(&mut self, target: NavigationTarget) -> Result<()> {
        if self.scheduled_actions.is_empty() {
            return self.execute_navigation(target);
        }

        let count = self.scheduled_actions.len();
        self.pending_navigation = Some(PendingNavigation { target });
        self.mailbox.status_line = Some(format!(
            "{count} scheduled actions. Apply now? (y/n, Esc cancels)"
        ));
        Ok(())
    }

    fn execute_navigation(&mut self, target: NavigationTarget) -> Result<()> {
        match target {
            NavigationTarget::Mailbox(kind) => self.switch_mailbox(kind),
            NavigationTarget::Account(index) => self.switch_account(index),
        }
    }

    /// Run `f` with `index` as the active account, restoring it afterwards.
    ///
    /// The mailbox helpers all reach their state through `Deref`, which resolves
    /// to whichever account is active.  Pointing that at a background account
    /// for the duration of an update reuses them as they are, rather than
    /// growing an index-taking twin of each one that could drift.
    fn with_account<T>(&mut self, index: usize, f: impl FnOnce(&mut Self) -> T) -> T {
        let previous = self.active_account;
        self.active_account = index;
        let result = f(self);
        self.active_account = previous;
        result
    }

    fn begin_mailbox_load(&mut self, target: MailboxKind, status: Option<String>) -> Result<()> {
        self.begin_mailbox_load_for(self.active_account, target, status)
    }

    /// Start loading `target` for the account at `index` on a worker thread.
    fn begin_mailbox_load_for(
        &mut self,
        index: usize,
        target: MailboxKind,
        status: Option<String>,
    ) -> Result<()> {
        let backend = Arc::clone(&self.accounts[index].backend);
        let status_for_finish = status.clone();
        let (sender, receiver) = std::sync::mpsc::channel();

        thread::spawn(move || match backend.load_mailbox(target) {
            Ok((mut snapshot, events)) => {
                snapshot.messages.sort_by_key(|msg| msg.seq);
                let total = snapshot.total;
                if sender.send(MailboxLoadUpdate::Started { total }).is_err() {
                    return;
                }
                if !snapshot.messages.is_empty() {
                    for chunk in snapshot.messages.chunks(MAILBOX_LOAD_CHUNK) {
                        if sender
                            .send(MailboxLoadUpdate::Batch(chunk.to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                let _ = sender.send(MailboxLoadUpdate::Finished {
                    events,
                    status: status_for_finish,
                });
            }
            Err(err) => {
                // `{:#}` rather than `{}`: the causes are where a failed login
                // says what the server made of the credentials, and dropping
                // them leaves the status line with only "connecting to ...".
                let _ = sender.send(MailboxLoadUpdate::Failed(format!("{err:#}")));
            }
        });

        {
            let account = &mut self.accounts[index];
            account.mailbox_loader = Some(MailboxLoaderState { receiver });
            account.mailbox_load_progress = Some(CommitProgress {
                total: PROGRESS_SEGMENTS,
                completed: 0,
            });
            // An account the backend has already answered for is connected, so
            // this is a folder being opened rather than a session being built.
            let phase = if account.connected {
                LoadPhase::Opening
            } else {
                LoadPhase::Connecting
            };
            account.loading = Some(LoadingState::in_phase(phase));
            // A body still arriving for the mailbox we are leaving is of no use.
            account.message_loader = None;
            account.message_view = None;
            account.search = None;
            account.current_mailbox = target;
            account.mailbox.messages.clear();
            account.mailbox.selected = None;
            account.mailbox.scroll_top = 0;
            account.mailbox.event_count = 0;
            let (_tx, placeholder_rx) = std::sync::mpsc::channel();
            account.mailbox.events = placeholder_rx;
            account.mailbox.status_line = Some(format!("Loading {target}..."));
        }

        Ok(())
    }

    fn switch_mailbox(&mut self, target: MailboxKind) -> Result<()> {
        if target == self.current_mailbox {
            self.mailbox.status_line = Some(format!("Already viewing {target}."));
            return Ok(());
        }

        self.begin_mailbox_load(target, Some(format!("Opened {target}.")))
    }

    fn reload_current_mailbox(&mut self) -> Result<()> {
        let target = self.current_mailbox;
        self.begin_mailbox_load(target, None)
    }

    fn switch_account(&mut self, index: usize) -> Result<()> {
        if index >= self.accounts.len() {
            self.mailbox.status_line = Some("Unknown account.".to_string());
            return Ok(());
        }

        if index == self.active_account {
            let name = self.accounts[index].name.clone();
            self.mailbox.status_line = Some(format!("Already viewing {name}."));
            return Ok(());
        }

        self.active_account = index;
        self.normalize_scroll();
        self.sync_message_view_state();
        let name = self.name.clone();
        let mailbox = self.current_mailbox;
        self.mailbox.status_line = Some(format!("Switched to {name} ({mailbox})."));

        // An account whose startup load failed would otherwise stay empty for
        // the rest of the session; arriving on it is the natural cue to retry.
        if !self.loaded && self.mailbox_loader.is_none() {
            self.begin_mailbox_load(mailbox, None)?;
        }

        Ok(())
    }

    fn discard_scheduled_actions(&mut self) -> Result<usize> {
        let count = self.scheduled_actions.len();
        if count == 0 {
            return Ok(0);
        }

        self.scheduled_actions.clear();
        self.reload_current_mailbox()?;
        Ok(count)
    }

    pub(crate) fn shortcut_menu(&self) -> Option<&ShortcutMenu> {
        self.pending_shortcut.as_ref().map(|state| state.menu())
    }

    fn remove_message_from_mailbox(&mut self, id: MessageId) -> bool {
        let position = {
            let mailbox = self.mailbox_mut();
            match mailbox.messages.iter().position(|msg| msg.id == id) {
                Some(pos) => pos,
                None => return false,
            }
        };

        let removed_id;
        {
            let mailbox = self.mailbox_mut();
            let removed = mailbox.messages.remove(position);
            removed_id = removed.id;
            resequence_messages(&mut mailbox.messages);

            if let Some(selected) = mailbox.selected {
                if mailbox.messages.is_empty() {
                    mailbox.selected = None;
                } else if selected >= mailbox.messages.len() {
                    mailbox.selected = Some(mailbox.messages.len() - 1);
                } else if position <= selected && selected > 0 {
                    mailbox.selected = Some(selected.saturating_sub(1));
                }
            }
        }

        let should_close = self
            .current_account()
            .message_view
            .as_ref()
            .map(|view| view.message_id == removed_id)
            .unwrap_or(false);
        if should_close {
            self.set_message_view(None);
        }

        if self.search.is_some() {
            self.recompute_search_filter();
        }
        self.normalize_scroll();
        true
    }

    /// Fire the delayed mark-as-read action when the timer expires.
    fn check_read_timer(&mut self) {
        let deadline = match self.message_view.as_ref().and_then(|v| v.read_at) {
            Some(d) => d,
            None => return,
        };
        if Instant::now() < deadline {
            return;
        }

        let view = self.message_view.as_mut().unwrap();
        view.read_at = None;
        let message_id = view.message_id;
        view.message.status = MessageStatus::Read;

        if let Some(slot) = self.selected_loaded_message_mut() {
            slot.status = MessageStatus::Read;
        }

        let action = Action::new(ActionType::MarkAsRead, message_id);
        let _ = self.submit_immediate_actions(vec![action]);
    }

    /// Drain backend event channels and merge them into local state.
    pub fn poll_backend_events(&mut self) {
        self.check_read_timer();
        self.poll_mailbox_loader();
        self.poll_message_loader();
        self.poll_commit_updates();
        self.poll_save_attachment_operation();
        self.poll_outgoing_operation();

        let mut refresh = false;
        let current_id = self
            .current_account()
            .message_view
            .as_ref()
            .map(|view| view.message_id);

        loop {
            let event = {
                let mailbox = self.mailbox_mut();
                mailbox.events.try_recv()
            };

            match event {
                Ok(BackendEvent::NewMessage(message)) => {
                    let mailbox = self.mailbox_mut();
                    mailbox.event_count += 1;
                    let index = if message.seq == 0 {
                        mailbox.messages.len()
                    } else {
                        message.seq.saturating_sub(1) as usize
                    };
                    ensure_placeholder_capacity(&mut mailbox.messages, index + 1);
                    mailbox.messages[index] = message;
                    if mailbox.selected.is_none() {
                        mailbox.selected = last_loaded_index(&mailbox.messages);
                    }
                    refresh = true;
                }
                Ok(BackendEvent::MessageFlagsChanged(message)) => {
                    let mailbox = self.mailbox_mut();
                    if let Some(existing) =
                        mailbox.messages.iter_mut().find(|msg| msg.id == message.id)
                    {
                        // Preserve locally-scheduled status (e.g. Deleted, Archived)
                        // that the backend doesn't know about.
                        let local_status = existing.status;
                        *existing = message;
                        match local_status {
                            MessageStatus::Deleted
                            | MessageStatus::Archived
                            | MessageStatus::PendingInbox
                            | MessageStatus::Spam => {
                                existing.status = local_status;
                            }
                            _ => {}
                        }
                        mailbox.event_count += 1;
                        refresh = true;
                    }
                }
                Ok(BackendEvent::MessageDeleted(id)) => {
                    if self.remove_message_from_mailbox(id) {
                        let mailbox = self.mailbox_mut();
                        mailbox.event_count += 1;
                        refresh = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if refresh {
            self.update_selection_after_refresh(current_id);
            if self.search.is_some() {
                self.recompute_search_filter();
            }
        }

        // Update backfill read-progress: if placeholders remain, keep the
        // indicator alive; otherwise clear it once the mailbox is fully loaded.
        if self.mailbox_load_progress.is_some() {
            let total = self.mailbox.messages.len();
            let loaded = loaded_message_count(&self.mailbox.messages);
            if loaded >= total {
                self.mailbox_load_progress = None;
            } else if let Some(progress) = self.mailbox_load_progress.as_mut() {
                progress.total = total;
                progress.completed = loaded;
            }
        }
    }

    /// Drain loader updates for every account, not just the visible one.
    ///
    /// All accounts load at once on startup, and the ones in the background have
    /// to finish on their own so that switching to them lands on a ready mailbox.
    fn poll_mailbox_loader(&mut self) {
        for index in 0..self.accounts.len() {
            if self.accounts[index].mailbox_loader.is_none() {
                continue;
            }
            self.with_account(index, |app| app.poll_active_mailbox_loader());
        }
    }

    fn poll_active_mailbox_loader(&mut self) {
        loop {
            let update = {
                let Some(loader) = self.mailbox_loader.as_mut() else {
                    return;
                };
                match loader.receiver.try_recv() {
                    Ok(update) => Some(update),
                    Err(TryRecvError::Empty) => return,
                    Err(TryRecvError::Disconnected) => {
                        self.mailbox_loader = None;
                        self.mailbox_load_progress = None;
                        // The worker died without reporting; say so in the
                        // overlay too, or an empty list is all that is left.
                        if let Some(loading) = self.loading.as_mut() {
                            loading.phase =
                                LoadPhase::Failed("the loader stopped unexpectedly".to_string());
                        }
                        self.mailbox
                            .status_line
                            .get_or_insert_with(|| "Mailbox load interrupted.".to_string());
                        return;
                    }
                }
            };

            if let Some(update) = update {
                let should_yield = matches!(update, MailboxLoadUpdate::Batch(_));
                self.apply_mailbox_loader_update(update);
                if should_yield {
                    break;
                }
            }
        }
    }

    fn apply_mailbox_loader_update(&mut self, update: MailboxLoadUpdate) {
        match update {
            MailboxLoadUpdate::Started { total } => {
                let current = self.current_mailbox;
                // The backend got far enough to answer, so there is a session
                // from here on and the next folder change is not a reconnect.
                self.connected = true;
                self.mailbox.messages.clear();
                if total > 0 {
                    ensure_placeholder_capacity(&mut self.mailbox.messages, total);
                    self.mailbox.selected = Some(total.saturating_sub(1));
                } else {
                    self.mailbox.selected = None;
                }
                self.normalize_scroll();

                let completed = loaded_message_count(&self.mailbox.messages);
                let progress = self
                    .mailbox_load_progress
                    .get_or_insert(CommitProgress { total, completed });
                progress.total = total;
                progress.completed = completed;
                if let Some(loading) = self.loading.as_mut() {
                    loading.phase = LoadPhase::Receiving {
                        loaded: completed,
                        total,
                    };
                }
                self.mailbox.status_line =
                    Some(format!("Loading {current}: {completed}/{total} messages"));
            }
            MailboxLoadUpdate::Batch(messages) => {
                let current = self.current_mailbox;
                for message in messages {
                    if message.seq == 0 {
                        continue;
                    }
                    let index = message.seq.saturating_sub(1) as usize;
                    ensure_placeholder_capacity(&mut self.mailbox.messages, index + 1);
                    self.mailbox.messages[index] = message;
                }

                let total_len = self.mailbox.messages.len();
                let completed = loaded_message_count(&self.mailbox.messages);

                if let Some(progress) = self.mailbox_load_progress.as_mut() {
                    if progress.total < total_len {
                        progress.total = total_len;
                    }
                    progress.completed = completed;
                }
                if let Some(loading) = self.loading.as_mut() {
                    loading.phase = LoadPhase::Receiving {
                        loaded: completed,
                        total: total_len,
                    };
                }

                if let Some(last_loaded) = last_loaded_index(&self.mailbox.messages) {
                    self.mailbox.selected = Some(last_loaded);
                    self.normalize_scroll();
                }

                if let Some(progress) = self.mailbox_load_progress.as_ref()
                    && progress.total > 0
                {
                    self.mailbox.status_line = Some(format!(
                        "Loading {current}: {}/{} messages",
                        completed, progress.total
                    ));
                }
            }
            MailboxLoadUpdate::Finished { events, status } => {
                self.mailbox.events = events;
                self.mailbox_loader = None;
                self.loading = None;
                self.loaded = true;
                let loaded = loaded_message_count(&self.mailbox.messages);
                let total = self.mailbox.messages.len();
                if loaded < total {
                    self.mailbox_load_progress = Some(CommitProgress {
                        total,
                        completed: loaded,
                    });
                } else {
                    self.mailbox_load_progress = None;
                }
                self.mailbox.status_line = status.or_else(|| {
                    Some(format!(
                        "Loaded {loaded}/{total} messages in {}.",
                        self.current_mailbox
                    ))
                });
                if let Some(last_loaded) = last_loaded_index(&self.mailbox.messages) {
                    self.mailbox.selected = Some(last_loaded);
                    self.normalize_scroll();
                } else if total > 0 {
                    self.mailbox.selected = Some(total - 1);
                }
            }
            MailboxLoadUpdate::Failed(message) => {
                let target = self.current_mailbox;
                self.mailbox_loader = None;
                self.mailbox_load_progress = None;
                // Kept rather than cleared: with no messages to fall back on,
                // the overlay is the only place the reason is readable in full.
                if let Some(loading) = self.loading.as_mut() {
                    loading.phase = LoadPhase::Failed(message.clone());
                }
                self.mailbox.status_line = Some(format!("Failed to load {target}: {message}"));
            }
        }
        if self.search.is_some() {
            self.recompute_search_filter();
        }
    }

    /// Poll every active commit batch for recently completed actions.
    fn poll_commit_updates(&mut self) {
        if self.commit_batches.is_empty() {
            return;
        }

        let len = self.commit_batches.len();
        for index in 0..len {
            let mut delta_completed = 0usize;
            {
                let batch = self
                    .commit_batches
                    .get_mut(index)
                    .expect("commit batch index out of bounds");

                loop {
                    match batch.receiver.try_recv() {
                        Ok(status) => {
                            delta_completed = delta_completed.saturating_add(1);
                            batch.completed = batch.completed.saturating_add(1);
                            if let Err(error) = status.result {
                                batch.failed.push((status.action, error));
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            batch.finished = true;
                            break;
                        }
                    }
                }
            }

            if delta_completed > 0
                && let Some(progress) = self.commit_progress.as_mut()
            {
                progress.completed = progress
                    .completed
                    .saturating_add(delta_completed)
                    .min(progress.total);
            }
        }

        self.finalize_commit_batches();
    }

    /// Integrate any finished batches back into the inbox state.
    fn finalize_commit_batches(&mut self) {
        loop {
            let ready = matches!(self.commit_batches.front(), Some(batch) if batch.completed >= batch.len() || batch.finished);

            if !ready {
                break;
            }

            let Some(mut batch) = self.commit_batches.pop_front() else {
                break;
            };

            if batch.completed < batch.len() {
                let missing = batch.len().saturating_sub(batch.completed);
                let message = format!("Commit interrupted ({missing} actions pending).");
                let total_actions = batch.actions.len();
                let skip = total_actions.saturating_sub(missing);
                for action in batch.actions.into_iter().skip(skip) {
                    batch.failed.push((action, message.clone()));
                }
            }

            if batch.immediate {
                // Immediate batches: never remove messages from the view.
                // On failure, revert the local flag change.
                if !batch.failed.is_empty() {
                    for (action, _error) in &batch.failed {
                        if let Some(msg) = self
                            .mailbox
                            .messages
                            .iter_mut()
                            .find(|m| m.id == action.message_id)
                        {
                            match action.action_type {
                                ActionType::MarkAsStarred => msg.starred = false,
                                ActionType::MarkAsUnstarred => msg.starred = true,
                                ActionType::MarkAsImportant => msg.important = false,
                                ActionType::MarkAsUnimportant => msg.important = true,
                                ActionType::MarkAsRead => {
                                    msg.status = MessageStatus::New;
                                }
                                ActionType::MoveToInboxUnread => {
                                    msg.status = MessageStatus::Read;
                                }
                                ActionType::MoveToInboxRead => {
                                    msg.status = MessageStatus::New;
                                }
                                _ => {}
                            }
                        }
                    }
                    let summary = format!("Failed to apply {} action(s).", batch.failed.len());
                    self.mailbox.status_line = Some(summary);
                }
            } else {
                // Messages were already optimistically removed in
                // commit_actions().  On failure, re-insert them with their
                // original status restored.
                if batch.failed.is_empty() {
                    self.mailbox.status_line = Some("Actions committed.".to_string());
                } else {
                    let failed_ids: std::collections::HashSet<MessageId> = batch
                        .failed
                        .iter()
                        .map(|(action, _)| action.message_id)
                        .collect();

                    for mut msg in batch.removed_messages {
                        if !failed_ids.contains(&msg.id) {
                            continue;
                        }
                        // Find the original_status from the failed action.
                        if let Some((action, _)) =
                            batch.failed.iter().find(|(a, _)| a.message_id == msg.id)
                            && let Some(original) = action.original_status
                        {
                            msg.status = original;
                        }
                        self.mailbox.messages.push(msg);
                    }

                    resequence_messages(&mut self.mailbox.messages);

                    let summary = format!("Failed to apply {} actions.", batch.failed.len());
                    self.mailbox.status_line = Some(summary);
                    self.scheduled_actions
                        .extend(batch.failed.into_iter().map(|(action, _error)| action));
                }
            }

            self.sync_message_view_state();
            if self.search.is_some() {
                self.recompute_search_filter();
            }
            if let Some(idx) = self.mailbox.selected
                && idx >= self.visible_message_count()
                && self.visible_message_count() > 0
            {
                self.mailbox.selected = Some(self.visible_message_count() - 1);
            }

            if self.visible_message_count() == 0 {
                self.mailbox.selected = None;
                if self.search.is_none() {
                    self.message_view = None;
                }
            }

            self.normalize_scroll();
        }

        if self.commit_batches.is_empty() {
            self.commit_progress = None;
        }
    }

    pub fn on_resize(&mut self) {
        if let Some(view) = &mut self.message_view {
            view.scroll = 0;
        }
    }

    pub(crate) fn inbox_messages(&self) -> &[Message] {
        &self.mailbox.messages
    }

    pub(crate) fn visible_messages(&self) -> Vec<&Message> {
        if let Some(search) = &self.search {
            search
                .filtered_indices
                .iter()
                .filter_map(|&idx| self.mailbox.messages.get(idx))
                .collect()
        } else {
            self.mailbox.messages.iter().collect()
        }
    }

    fn visible_message_count(&self) -> usize {
        if let Some(search) = &self.search {
            search.filtered_indices.len()
        } else {
            self.mailbox.messages.len()
        }
    }

    /// Map the visible selection index to the real index in `mailbox.messages`.
    fn real_selected_index(&self) -> Option<usize> {
        let selected = self.mailbox.selected?;
        if let Some(search) = &self.search {
            search.filtered_indices.get(selected).copied()
        } else {
            Some(selected)
        }
    }

    pub(crate) fn inbox_selected(&self) -> Option<usize> {
        self.mailbox.selected
    }

    pub(crate) fn search_state(&self) -> Option<(&str, usize, bool)> {
        self.search
            .as_ref()
            .map(|s| (s.input.value.as_str(), s.input.cursor, s.focused))
    }

    fn recompute_search_filter(&mut self) {
        if self.search.is_none() {
            return;
        }
        // Remember which real message is currently selected.
        let previously_selected_id = self
            .real_selected_index()
            .and_then(|idx| self.mailbox.messages.get(idx))
            .map(|msg| msg.id);
        let prev_selected = self.mailbox.selected.unwrap_or(0);

        let query = self.search.as_ref().unwrap().input.value.clone();
        let new_indices = compute_filtered_indices(&self.mailbox.messages, &query);
        let filtered_len = new_indices.len();

        // Find the visible position of the previously selected message.
        let mut restored_pos = None;
        if let Some(id) = previously_selected_id {
            for (vis, real_idx) in new_indices.iter().enumerate() {
                if let Some(msg) = self.mailbox.messages.get(*real_idx)
                    && msg.id == id
                {
                    restored_pos = Some(vis);
                    break;
                }
            }
        }

        self.search.as_mut().unwrap().filtered_indices = new_indices;

        if let Some(visible_pos) = restored_pos {
            self.mailbox.selected = Some(visible_pos);
        } else if filtered_len == 0 {
            self.mailbox.selected = None;
        } else {
            self.mailbox.selected = Some(prev_selected.min(filtered_len - 1));
        }

        self.normalize_scroll();
    }

    fn selected_loaded_message(&self) -> Option<&Message> {
        self.real_selected_index()
            .and_then(|idx| self.mailbox.messages.get(idx))
            .filter(|msg| !msg.is_placeholder())
    }

    fn selected_loaded_message_mut(&mut self) -> Option<&mut Message> {
        let idx = self.real_selected_index()?;
        self.mailbox
            .messages
            .get_mut(idx)
            .filter(|msg| !msg.is_placeholder())
    }

    pub(crate) fn inbox_action_bar(&self) -> String {
        let mut text = String::from("^Q:Quit g:GoToMailbox G:Accounts c:Compose");

        if let Some(real_idx) = self.real_selected_index()
            && let Some(msg) = self.mailbox.messages.get(real_idx)
        {
            if msg.is_placeholder() {
                text.push_str(" Loading message...");
                return text;
            }
            text.push_str(" Enter:Open");
            if msg.starred {
                text.push_str(" s:Unstar");
            } else {
                text.push_str(" s:Star");
            }
            if msg.important {
                text.push_str(" -:NotImportant");
            } else {
                text.push_str(" +/=:Important");
            }

            let current_mailbox = self.current_mailbox;
            let in_archive = current_mailbox == MailboxKind::Archive;
            let in_trash = current_mailbox == MailboxKind::Trash;
            let in_spam = current_mailbox == MailboxKind::Spam;

            match msg.status {
                MessageStatus::New | MessageStatus::Read => {
                    text.push_str(" r:Reply y:Archive d:Delete");
                }
                MessageStatus::Deleted => {
                    if in_trash {
                        text.push_str(" r:Reply y:Archive d:Undelete");
                    } else {
                        text.push_str(" r:Reply y:Archive u:Undelete");
                    }
                }
                MessageStatus::Archived => {
                    if in_archive {
                        text.push_str(" r:Reply y:Unarchive d:Delete");
                    } else {
                        text.push_str(" r:Reply u:Unarchive d:Delete");
                    }
                }
                MessageStatus::PendingInbox => {
                    if in_trash {
                        text.push_str(" r:Reply u:KeepDeleted");
                    } else if in_archive {
                        text.push_str(" r:Reply u:KeepArchived");
                    } else if in_spam {
                        text.push_str(" r:Reply u:KeepSpam");
                    } else {
                        text.push_str(" r:Reply");
                    }
                }
                MessageStatus::Spam => {
                    text.push_str(" r:Reply y:Archive d:Delete");
                }
            }

            if msg.status != MessageStatus::PendingInbox {
                if matches!(msg.status, MessageStatus::Spam) || in_spam {
                    text.push_str(" !:NoSpam");
                } else {
                    text.push_str(" !:Spam");
                }
            }
        }

        if !self.scheduled_actions.is_empty() {
            text.push_str(" $:Commit");
        }

        if self.search.is_none() {
            text.push_str(" /:Search");
        }

        text
    }

    /// Renderable text indicator reflecting aggregate commit progress.
    ///
    /// Write operations (committing actions) take precedence over read operations
    /// (loading mailbox data) when both are active simultaneously.
    pub(crate) fn commit_indicator(&self) -> Option<(String, ProgressMode)> {
        let write = self
            .commit_progress
            .as_ref()
            .and_then(Self::format_progress);
        if let Some(text) = write {
            return Some((text, ProgressMode::Write));
        }

        let read = self
            .mailbox_load_progress
            .as_ref()
            .and_then(Self::format_progress);
        read.map(|text| (text, ProgressMode::Read))
    }

    fn format_progress(progress: &CommitProgress) -> Option<String> {
        if progress.total == 0 {
            return None;
        }

        let capped_completed = progress.completed.min(progress.total);
        let filled = (capped_completed * PROGRESS_SEGMENTS).div_ceil(progress.total);

        let mut indicator = String::from("[");
        for idx in 0..PROGRESS_SEGMENTS {
            if idx < filled {
                indicator.push('#');
            } else {
                indicator.push(' ');
            }
        }
        indicator.push(']');

        Some(indicator)
    }

    pub(crate) fn inbox_info_bar(&self) -> String {
        let total = self.mailbox.messages.len();
        let visible = self.visible_message_count();
        let selected = self
            .mailbox
            .selected
            .map(|idx| format!("{}", idx + 1))
            .unwrap_or_else(|| "-".to_string());
        if self.search.is_some() {
            format!(
                "{} • {} — {visible} matches of {total} messages, message {selected}/{visible}, {} scheduled",
                self.name,
                self.current_mailbox,
                self.scheduled_actions.len(),
            )
        } else {
            format!(
                "{} • {} — message {selected}/{total}, {} scheduled actions, got {} events",
                self.name,
                self.current_mailbox,
                self.scheduled_actions.len(),
                self.mailbox.event_count
            )
        }
    }

    pub(crate) fn inbox_status_line(&self) -> Option<&str> {
        self.mailbox.status_line.as_deref()
    }

    pub(crate) fn inbox_scroll_top(&self) -> usize {
        self.mailbox.scroll_top
    }

    pub(crate) fn set_inbox_scroll_top(&mut self, value: usize) {
        self.mailbox.scroll_top = value;
    }

    pub(crate) fn message_view(&self) -> Option<&MessageViewState> {
        self.message_view.as_ref()
    }

    pub(crate) fn compose_state(&self) -> Option<&ComposeState> {
        self.compose.as_ref()
    }

    pub(crate) fn compose_state_mut(&mut self) -> Option<&mut ComposeState> {
        self.compose.as_mut()
    }

    pub(crate) fn compose_action_bar(&self) -> String {
        let label = match self.compose.as_ref().and_then(|state| state.draft_id()) {
            Some(_) => "Edit Draft",
            None => "Compose",
        };
        // Do not advertise keys that are locked out while the backend has the message.
        if let Some((operation, _)) = self.pending_outgoing() {
            return format!("{label} - {operation}...");
        }
        format!("{label} - Tab:Next Shift+Tab:Prev Esc:Cancel Enter:Activate")
    }

    pub(crate) fn compose_status_line(&self) -> Option<&str> {
        self.compose.as_ref().and_then(|state| state.status())
    }

    /// The load the user is waiting on, while there is nothing else to look at.
    ///
    /// Returns `None` the moment the list has a real message in it: once the
    /// mailbox is readable, the remaining progress belongs in the status bar
    /// rather than over the top of what the user came to read.  This covers the
    /// cold start, switching accounts and switching mailboxes alike, because all
    /// three go through [`Self::begin_mailbox_load_for`].
    pub(crate) fn loading_overlay(&self) -> Option<(&str, MailboxKind, &LoadingState)> {
        let loading = self.loading.as_ref()?;
        if self
            .mailbox
            .messages
            .iter()
            .any(|msg| !msg.is_placeholder())
        {
            return None;
        }
        Some((self.name.as_str(), self.current_mailbox, loading))
    }

    /// Label and elapsed time of the message body loading in the background.
    pub(crate) fn pending_message_load(&self) -> Option<(&str, std::time::Duration)> {
        self.message_loader
            .as_ref()
            .map(|op| (op.label.as_str(), clock::elapsed(op.started)))
    }

    /// Label and elapsed time of the send or draft save in flight.
    pub(crate) fn pending_outgoing(&self) -> Option<(&str, std::time::Duration)> {
        self.outgoing
            .as_ref()
            .map(|op| (op.kind.label(), clock::elapsed(op.started)))
    }

    fn open_search(&mut self) {
        if let Some(search) = self.search.as_mut() {
            // Re-focus existing search panel.
            search.focused = true;
            search.input.cursor = text_len(&search.input.value);
            return;
        }
        let pre_search_selected = self
            .real_selected_index()
            .and_then(|idx| self.mailbox.messages.get(idx))
            .map(|msg| msg.id);
        let filtered_indices = (0..self.mailbox.messages.len()).collect();
        self.search = Some(SearchState {
            input: TextFieldState::default(),
            focused: true,
            filtered_indices,
            pre_search_selected,
        });
    }

    fn close_search(&mut self) {
        let pre_search_id = self.search.as_ref().and_then(|s| s.pre_search_selected);
        self.search = None;
        // Restore selection to the pre-search message.
        if let Some(id) = pre_search_id
            && let Some((idx, _)) = self
                .mailbox
                .messages
                .iter()
                .enumerate()
                .find(|(_, msg)| msg.id == id)
        {
            self.mailbox.selected = Some(idx);
        }
        self.normalize_scroll();
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<()> {
        // Allow Ctrl+Q even in search mode.
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return Ok(());
        }

        match key.code {
            KeyCode::Esc => {
                self.close_search();
            }
            KeyCode::Enter => {
                if self
                    .search
                    .as_ref()
                    .is_some_and(|s| s.input.value.trim().is_empty())
                {
                    self.close_search();
                } else if let Some(search) = self.search.as_mut() {
                    search.focused = false;
                }
            }
            KeyCode::Backspace => {
                if let Some(search) = self.search.as_mut() {
                    search.input.backspace();
                }
                self.recompute_search_filter();
            }
            KeyCode::Left => {
                if let Some(search) = self.search.as_mut() {
                    search.input.move_left();
                }
            }
            KeyCode::Right => {
                if let Some(search) = self.search.as_mut() {
                    search.input.move_right();
                }
            }
            KeyCode::Home => {
                if let Some(search) = self.search.as_mut() {
                    search.input.move_home();
                }
            }
            KeyCode::End => {
                if let Some(search) = self.search.as_mut() {
                    search.input.move_end();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(search) = self.search.as_mut() {
                    search.input.value.clear();
                    search.input.cursor = 0;
                }
                self.recompute_search_filter();
            }
            KeyCode::Char(ch) => {
                if let Some(search) = self.search.as_mut() {
                    search.input.insert(ch);
                }
                self.recompute_search_filter();
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_mailbox_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return Ok(());
        }

        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-PAGE_JUMP),
            KeyCode::PageDown => self.move_selection(PAGE_JUMP),
            KeyCode::Home => self.select_first(),
            KeyCode::End => self.select_last(),
            KeyCode::Char('s') | KeyCode::Char('S') => self.toggle_star(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.mark_selected_important(true),
            KeyCode::Char('-') => self.mark_selected_important(false),
            KeyCode::Char('y') | KeyCode::Char('Y') => self.schedule_archive(),
            KeyCode::Char('u') | KeyCode::Char('U') => self.toggle_unread(),
            KeyCode::Char('c') | KeyCode::Char('C') => self.open_compose(),
            KeyCode::Char('r') | KeyCode::Char('R') => self.open_reply(false)?,
            KeyCode::Char('a') | KeyCode::Char('A') => self.open_reply(true)?,
            KeyCode::Char('f') | KeyCode::Char('F') => self.open_forward()?,
            KeyCode::Char('!') => {
                if let Some(real_idx) = self.real_selected_index()
                    && let Some(msg) = self.mailbox.messages.get(real_idx)
                {
                    if self.current_mailbox == MailboxKind::Spam
                        || msg.status == MessageStatus::Spam
                    {
                        self.schedule_move_to_inbox();
                    } else {
                        self.schedule_move_to_spam();
                    }
                }
            }
            KeyCode::Char('$') => self.commit_actions()?,
            KeyCode::Enter => self.open_selected_entry()?,
            KeyCode::Right => self.open_selected_message()?,
            KeyCode::Char('d') | KeyCode::Char('D') => self.schedule_delete(),
            KeyCode::Char('#') => self.schedule_delete(),
            KeyCode::Backspace | KeyCode::Delete => self.schedule_delete(),
            KeyCode::Char('/') => self.open_search(),
            KeyCode::Esc if self.search.is_some() => {
                self.close_search();
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_message_key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            self.open_compose();
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.open_reply(false)?;
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.open_reply(true)?;
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.open_forward()?;
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('s')) {
            let current_id = self.message_view.as_ref().map(|view| view.message_id);
            self.toggle_star();
            if let Some(id) = current_id {
                let starred = self
                    .mailbox
                    .messages
                    .iter()
                    .find(|message| message.id == id)
                    .map(|msg| msg.starred);

                if let Some(starred) = starred
                    && let Some(view) = self.message_view.as_mut()
                    && view.message_id == id
                {
                    view.message.starred = starred;
                }
            }
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('S')) {
            self.open_save_attachment_dialog();
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('+') | KeyCode::Char('=')) {
            let current_id = self.message_view.as_ref().map(|view| view.message_id);
            self.mark_selected_important(true);
            if let Some(id) = current_id {
                let important = self
                    .mailbox
                    .messages
                    .iter()
                    .find(|message| message.id == id)
                    .map(|msg| msg.important);

                if let Some(important) = important
                    && let Some(view) = self.message_view.as_mut()
                    && view.message_id == id
                {
                    view.message.important = important;
                }
            }
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('-')) {
            let current_id = self.message_view.as_ref().map(|view| view.message_id);
            self.mark_selected_important(false);
            if let Some(id) = current_id {
                let important = self
                    .mailbox
                    .messages
                    .iter()
                    .find(|message| message.id == id)
                    .map(|msg| msg.important);

                if let Some(important) = important
                    && let Some(view) = self.message_view.as_mut()
                    && view.message_id == id
                {
                    view.message.important = important;
                }
            }
            return Ok(());
        }

        let Some(view) = self.message_view.as_mut() else {
            return Ok(());
        };

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left => {
                self.message_view = None;
            }
            KeyCode::Char('j') | KeyCode::Char('J') => {
                self.open_adjacent_message(1)?;
            }
            KeyCode::Char('k') | KeyCode::Char('K') => {
                self.open_adjacent_message(-1)?;
            }
            KeyCode::Down => {
                view.scroll = view.scroll.saturating_add(1);
            }
            KeyCode::Up => {
                view.scroll = view.scroll.saturating_sub(1);
            }
            KeyCode::Char(' ') => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    view.scroll = view.scroll.saturating_sub(5);
                } else {
                    view.scroll = view.scroll.saturating_add(5);
                }
            }
            KeyCode::PageDown => {
                view.scroll = view.scroll.saturating_add(5);
            }
            KeyCode::PageUp => {
                view.scroll = view.scroll.saturating_sub(5);
            }
            KeyCode::Char('.') | KeyCode::Char('u') | KeyCode::Char('U') => {
                view.unformatted = !view.unformatted;
                view.info_line = Some(if view.unformatted {
                    "Showing raw HTML".to_string()
                } else {
                    "Showing formatted FTML".to_string()
                });
            }
            _ => {}
        }

        Ok(())
    }

    fn open_compose(&mut self) {
        // A reply/forward/draft body still on its way would replace this blank
        // message -- and anything typed into it -- the moment it lands.
        self.cancel_compose_bound_load();
        self.compose = Some(ComposeState::new());
        self.message_view = None;
        self.mailbox.status_line = Some("Compose mode active.".to_string());
    }

    /// Drop a pending load whose result would open the compose view.
    fn cancel_compose_bound_load(&mut self) {
        if self
            .message_loader
            .as_ref()
            .is_some_and(|op| op.purpose != MessageLoadPurpose::View)
        {
            self.current_account_mut().message_loader = None;
        }
    }

    pub(crate) fn save_attachment_dialog(&self) -> Option<&SaveAttachmentDialog> {
        self.save_attachment.as_ref()
    }

    pub(crate) fn save_attachment_attachments(&self) -> &[MessageAttachment] {
        self.message_view
            .as_ref()
            .map(|view| view.content.attachments.as_slice())
            .unwrap_or(&[])
    }

    fn open_save_attachment_dialog(&mut self) {
        let attachments_empty = self
            .message_view
            .as_ref()
            .is_none_or(|view| view.content.attachments.is_empty());
        if attachments_empty {
            if let Some(view) = self.message_view.as_mut() {
                view.info_line = Some("This message has no attachments.".to_string());
            }
            return;
        }

        let folder = default_download_dir().to_string_lossy().into_owned();
        self.save_attachment = Some(SaveAttachmentDialog::new(folder));
    }

    fn close_save_attachment_dialog(&mut self) {
        self.save_attachment = None;
    }

    fn handle_save_attachment_key(&mut self, key: KeyEvent) -> Result<()> {
        let total = self.save_attachment_attachments().len();
        let Some(dialog) = self.save_attachment.as_mut() else {
            return Ok(());
        };

        if dialog.is_busy() {
            if matches!(key.code, KeyCode::Esc) {
                dialog.cancel_operation();
                self.close_save_attachment_dialog();
            }
            return Ok(());
        }

        match key.code {
            KeyCode::Esc => {
                self.close_save_attachment_dialog();
                return Ok(());
            }
            KeyCode::Tab | KeyCode::BackTab => {
                dialog.toggle_focus();
                return Ok(());
            }
            KeyCode::Enter => {
                return self.commit_save_attachment();
            }
            _ => {}
        }

        match dialog.focus {
            SaveAttachmentFocus::Folder => match key.code {
                KeyCode::Left => {
                    dialog.folder.move_left();
                }
                KeyCode::Right => {
                    dialog.folder.move_right();
                }
                KeyCode::Home => {
                    dialog.folder.move_home();
                }
                KeyCode::End => {
                    dialog.folder.move_end();
                }
                KeyCode::Backspace if dialog.folder.backspace() => {
                    dialog.clear_status();
                }
                KeyCode::Backspace => {}
                KeyCode::Delete if dialog.folder.delete() => {
                    dialog.clear_status();
                }
                KeyCode::Delete => {}
                KeyCode::Up => {
                    dialog.focus = SaveAttachmentFocus::List;
                }
                KeyCode::Down => {
                    dialog.focus = SaveAttachmentFocus::List;
                }
                KeyCode::Char(ch) => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.modifiers.contains(KeyModifiers::ALT)
                    {
                        return Ok(());
                    }
                    if dialog.folder.insert(ch) {
                        dialog.clear_status();
                    }
                }
                _ => {}
            },
            SaveAttachmentFocus::List => match key.code {
                KeyCode::Up => {
                    if dialog.selected > 0 {
                        dialog.selected -= 1;
                        dialog.clear_status();
                    } else {
                        dialog.focus = SaveAttachmentFocus::Folder;
                    }
                }
                KeyCode::Down => {
                    if dialog.selected + 1 < total {
                        dialog.selected += 1;
                        dialog.clear_status();
                    } else {
                        dialog.focus = SaveAttachmentFocus::Folder;
                    }
                }
                KeyCode::Home => {
                    dialog.selected = 0;
                }
                KeyCode::End => {
                    dialog.selected = total.saturating_sub(1);
                }
                _ => {}
            },
        }

        Ok(())
    }

    fn commit_save_attachment(&mut self) -> Result<()> {
        if self
            .save_attachment
            .as_ref()
            .is_some_and(|dialog| dialog.is_busy())
        {
            return Ok(());
        }

        let (folder_text, selected) = match self.save_attachment.as_ref() {
            Some(dialog) => (dialog.folder.value.clone(), dialog.selected),
            None => return Ok(()),
        };

        // Clone only the attachment we are about to write; cloning the whole
        // vector would copy every other attachment's bytes as well.
        let selected_attachment = self.message_view.as_ref().and_then(|view| {
            let attachments = &view.content.attachments;
            let idx = selected.min(attachments.len().saturating_sub(1));
            attachments.get(idx).map(|att| (idx, att.clone()))
        });

        let Some((idx, attachment)) = selected_attachment else {
            self.close_save_attachment_dialog();
            return Ok(());
        };

        let folder = expand_user_path(&folder_text);
        let base_name = attachment
            .filename
            .as_deref()
            .map(sanitize_filename)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| fallback_attachment_name(idx, &attachment.mime_type));

        let backend = Arc::clone(&self.backend);
        let (tx, rx) = std::sync::mpsc::channel::<Result<PathBuf, String>>();
        let cancel = Arc::new(AtomicBool::new(false));

        let filename_for_op = base_name.clone();
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            let result = (|| -> Result<PathBuf, String> {
                let bytes = match attachment.data {
                    Some(bytes) => bytes,
                    None => match attachment.blob_id.as_deref() {
                        Some(blob_id) => backend
                            .fetch_attachment_blob(blob_id)
                            .map_err(|err| format!("Failed to download attachment: {err}"))?,
                        None => {
                            return Err("Attachment content is not available.".to_string());
                        }
                    },
                };

                // Last chance to bail out: the download may have taken a while
                // and the user may have dismissed the dialog meanwhile.
                if worker_cancel.load(Ordering::Relaxed) {
                    return Err("Save cancelled.".to_string());
                }

                fs::create_dir_all(&folder)
                    .map_err(|err| format!("Cannot create folder '{}': {err}", folder.display()))?;

                let target = unique_path_in(&folder, &base_name);
                fs::write(&target, &bytes)
                    .map_err(|err| format!("Failed to write '{}': {err}", target.display()))?;
                Ok(target)
            })();
            let _ = tx.send(result);
        });

        if let Some(dialog) = self.save_attachment.as_mut() {
            dialog.clear_status();
            dialog.operation = Some(SaveAttachmentOperation {
                receiver: rx,
                started: Instant::now(),
                filename: filename_for_op,
                cancel,
            });
        }

        Ok(())
    }

    fn poll_save_attachment_operation(&mut self) {
        let Some(dialog) = self.save_attachment.as_mut() else {
            return;
        };
        let Some(op) = dialog.operation.as_ref() else {
            return;
        };

        match op.receiver.try_recv() {
            Ok(Ok(path)) => {
                dialog.operation = None;
                dialog.set_status(format!("Saved to {}", path.display()));
            }
            Ok(Err(err)) => {
                dialog.operation = None;
                dialog.set_status(err);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                dialog.operation = None;
                dialog.set_status("Download worker exited unexpectedly.");
            }
        }
    }

    /// Start a background load of `message`'s body.
    ///
    /// Nothing here touches the network on the UI thread: the worker fetches the
    /// body and, for compose-bound loads, the attachment bytes that backends
    /// like JMAP only hand out as blob pointers.  [`Self::poll_message_loader`]
    /// picks the result up on a later tick.
    ///
    /// Moving on to another message supersedes the request, but the worker
    /// cannot be cancelled -- it runs to completion and its result is dropped
    /// when the channel goes away.  Backends serialise their own I/O, so this
    /// costs at most one redundant fetch.
    fn begin_message_load(
        &mut self,
        message: Message,
        purpose: MessageLoadPurpose,
        mut cached: Option<MessageContent>,
        cached_document: Option<Document>,
    ) {
        // Repeating the keystroke that started a load should not start a second.
        if self
            .message_loader
            .as_ref()
            .is_some_and(|op| op.message.id == message.id && op.purpose == purpose)
        {
            return;
        }

        let needs_attachments = purpose.needs_attachments();

        // With the body already in hand and no attachment bytes to fetch there
        // is nothing to wait for.  Replying to the message on screen is the
        // common case and should not cost a tick of latency.
        if !needs_attachments && let Some(content) = cached.take() {
            self.deliver_loaded_message(
                message,
                purpose,
                cached_document,
                LoadedMessage {
                    content,
                    attachments: Vec::new(),
                    unavailable: 0,
                },
            );
            return;
        }

        let backend = Arc::clone(&self.backend);
        let message_id = message.id;
        let (sender, receiver) = std::sync::mpsc::channel();

        thread::spawn(move || {
            let result = (|| -> Result<LoadedMessage, String> {
                let content = match cached {
                    Some(content) => content,
                    None => backend
                        .load_message(message_id)
                        .map_err(|err| err.to_string())?,
                };
                let (attachments, unavailable) = if needs_attachments {
                    restore_compose_attachments(backend.as_ref(), &content)
                } else {
                    (Vec::new(), 0)
                };
                Ok(LoadedMessage {
                    content,
                    attachments,
                    unavailable,
                })
            })();
            let _ = sender.send(result);
        });

        let label = message_load_label(purpose, &message);
        self.current_account_mut().message_loader = Some(MessageLoadOperation {
            message,
            purpose,
            cached_document,
            receiver,
            started: Instant::now(),
            label,
        });
    }

    fn poll_message_loader(&mut self) {
        let result = {
            let Some(op) = self.message_loader.as_ref() else {
                return;
            };
            match op.receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    Err("the loader thread exited unexpectedly".to_string())
                }
            }
        };

        let Some(op) = self.current_account_mut().message_loader.take() else {
            return;
        };
        self.finish_message_load(op, result);
    }

    fn finish_message_load(
        &mut self,
        op: MessageLoadOperation,
        result: Result<LoadedMessage, String>,
    ) {
        let loaded = match result {
            Ok(loaded) => loaded,
            Err(err) => {
                let text = format!("Failed to load message: {err}");
                if let Some(view) = self.message_view.as_mut() {
                    view.info_line = Some(text.clone());
                }
                self.mailbox.status_line = Some(text);
                return;
            }
        };

        self.deliver_loaded_message(op.message, op.purpose, op.cached_document, loaded);
    }

    /// Hand a loaded body to whatever asked for it.
    fn deliver_loaded_message(
        &mut self,
        message: Message,
        purpose: MessageLoadPurpose,
        cached_document: Option<Document>,
        loaded: LoadedMessage,
    ) {
        self.sync_attachment_indicator(message.id, marks_as_having_attachments(&loaded.content));

        if purpose == MessageLoadPurpose::View {
            self.show_loaded_message(message.id, loaded.content);
            return;
        }

        let document =
            cached_document.unwrap_or_else(|| document_from_message_content(&loaded.content));

        match purpose {
            MessageLoadPurpose::View => unreachable!("handled above"),
            MessageLoadPurpose::Reply { reply_all } => {
                self.show_reply_compose(&message, document, reply_all);
            }
            MessageLoadPurpose::Forward => {
                self.show_forward_compose(&message, document, loaded);
            }
            MessageLoadPurpose::Draft => {
                self.show_draft_compose(&message, document, loaded);
            }
        }
    }

    /// Start loading the selected message so compose can be seeded from it.
    fn begin_compose_from_selected(&mut self, purpose: MessageLoadPurpose) {
        let Some(message) = self.selected_loaded_message().cloned() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        // The open viewer already holds the body, so reuse it; only blob-backed
        // attachments then still need the network.
        let (cached, cached_document) = match self
            .message_view
            .as_ref()
            .filter(|view| view.message_id == message.id)
        {
            Some(view) => (Some(view.content.clone()), view.document.clone()),
            None => (None, None),
        };

        self.begin_message_load(message, purpose, cached, cached_document);
    }

    fn open_reply(&mut self, reply_all: bool) -> Result<()> {
        self.begin_compose_from_selected(MessageLoadPurpose::Reply { reply_all });
        Ok(())
    }

    fn show_reply_compose(&mut self, message: &Message, document: Document, reply_all: bool) {
        let mut compose = ComposeState::new();
        let subject = prefix_subject(&message.subject, "Re:");
        compose.set_field_text(ComposeField::Subject, subject);

        let reply_body = build_reply_document(&document, &message.sender, message.sent);
        compose.set_body(reply_body);
        compose.set_focus(ComposeFocus::Body);

        let mut primary_recipient = message.sender.trim().to_string();
        if primary_recipient.is_empty()
            && let Some(first) = message.recipients.first()
        {
            primary_recipient = first.trim().to_string();
        }

        if !primary_recipient.is_empty() {
            compose.set_field_text(ComposeField::To, primary_recipient.clone());
        }

        if reply_all {
            let primary_lower = primary_recipient.to_ascii_lowercase();
            let cc_list: Vec<String> = message
                .recipients
                .iter()
                .map(|recipient| recipient.trim().to_string())
                .filter(|recipient| !recipient.is_empty())
                .filter(|recipient| recipient.to_ascii_lowercase() != primary_lower)
                .collect();
            if !cc_list.is_empty() {
                compose.set_field_text(ComposeField::Cc, cc_list.join(", "));
            }
        }

        self.compose = Some(compose);
        self.message_view = None;
        let status = if reply_all {
            format!("Reply all to '{}'.", message.subject)
        } else {
            format!("Replying to '{}'.", message.subject)
        };
        self.mailbox.status_line = Some(status);
    }

    fn open_forward(&mut self) -> Result<()> {
        self.begin_compose_from_selected(MessageLoadPurpose::Forward);
        Ok(())
    }

    fn show_forward_compose(
        &mut self,
        message: &Message,
        document: Document,
        loaded: LoadedMessage,
    ) {
        let mut compose = ComposeState::new();
        let subject = prefix_subject(&message.subject, "Fwd:");
        compose.set_field_text(ComposeField::Subject, subject);
        compose.set_body(build_forward_document(&document, message));
        compose.set_attachments(loaded.attachments);
        compose.set_focus(ComposeFocus::Body);

        self.compose = Some(compose);
        self.message_view = None;
        self.mailbox.status_line = Some(draft_status_line(
            &format!("Forwarding '{}'.", message.subject),
            loaded.unavailable,
        ));
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return Ok(());
        }

        // The backend already has this message: editing it now would change
        // something in flight, and cancelling would throw away text that is
        // still needed if the submission fails.  The progress line explains the
        // wait; every other key is ignored until the worker reports back.
        if self.outgoing.is_some() {
            return Ok(());
        }

        let Some(compose) = self.compose.as_mut() else {
            return Ok(());
        };

        if compose.is_asking_about_large_attachment() {
            return self.handle_large_attachment_key(key);
        }

        if compose.is_attachment_prompt_active() {
            return self.handle_attachment_prompt_key(key);
        }

        match key.code {
            KeyCode::Esc => {
                self.cancel_compose();
                return Ok(());
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    compose.focus_prev();
                } else {
                    compose.focus_next();
                }
                return Ok(());
            }
            KeyCode::BackTab => {
                compose.focus_prev();
                return Ok(());
            }
            _ => {}
        }

        match compose.focus() {
            ComposeFocus::Field(field) => match key.code {
                KeyCode::Up => {
                    compose.focus_prev();
                    return Ok(());
                }
                KeyCode::Down => {
                    compose.focus_next();
                    return Ok(());
                }
                KeyCode::Enter => {
                    compose.focus_next();
                    return Ok(());
                }
                KeyCode::Left => {
                    let _ = compose.field_state_mut(field).move_left();
                    return Ok(());
                }
                KeyCode::Right => {
                    let _ = compose.field_state_mut(field).move_right();
                    return Ok(());
                }
                KeyCode::Home => {
                    let _ = compose.field_state_mut(field).move_home();
                    return Ok(());
                }
                KeyCode::End => {
                    let _ = compose.field_state_mut(field).move_end();
                    return Ok(());
                }
                KeyCode::Backspace => {
                    if compose.field_state_mut(field).backspace() {
                        compose.clear_status();
                    }
                    return Ok(());
                }
                KeyCode::Delete => {
                    if compose.field_state_mut(field).delete() {
                        compose.clear_status();
                    }
                    return Ok(());
                }
                KeyCode::Char(ch) => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.modifiers.contains(KeyModifiers::ALT)
                    {
                        return Ok(());
                    }

                    if compose.field_state_mut(field).insert(ch) {
                        compose.clear_status();
                    }
                    return Ok(());
                }
                _ => {}
            },
            ComposeFocus::Attachments => match key.code {
                KeyCode::Up => {
                    if !compose.select_attachment_prev() {
                        compose.focus_prev();
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if !compose.select_attachment_next() {
                        compose.focus_next();
                    }
                    return Ok(());
                }
                KeyCode::Home => {
                    compose.select_attachment_first();
                    return Ok(());
                }
                KeyCode::End => {
                    compose.select_attachment_last();
                    return Ok(());
                }
                KeyCode::Delete | KeyCode::Backspace => {
                    if let Some(removed) = compose.remove_selected_attachment() {
                        compose.set_status(format!("Removed attachment '{}'.", removed.filename));
                    }
                    return Ok(());
                }
                _ => {}
            },
            ComposeFocus::Body => match key.code {
                KeyCode::Up => {
                    compose.focus_prev();
                    return Ok(());
                }
                KeyCode::Down => {
                    compose.focus_next();
                    return Ok(());
                }
                KeyCode::PageDown => {
                    compose.scroll_body_pages(1);
                    return Ok(());
                }
                KeyCode::PageUp => {
                    compose.scroll_body_pages(-1);
                    return Ok(());
                }
                KeyCode::Enter => {
                    return self.edit_compose_body();
                }
                KeyCode::Char('e' | 'E') => {
                    return self.edit_compose_body();
                }
                _ => {}
            },
            ComposeFocus::Button(button) => match key.code {
                KeyCode::Left => {
                    compose.focus_button_prev();
                    return Ok(());
                }
                KeyCode::Right => {
                    compose.focus_button_next();
                    return Ok(());
                }
                KeyCode::Up => {
                    compose.set_focus(ComposeFocus::Body);
                    return Ok(());
                }
                KeyCode::Down => {
                    return Ok(());
                }
                KeyCode::Enter => {
                    return self.activate_compose_button(button);
                }
                KeyCode::Char(ch) => {
                    let target = match ch {
                        'a' | 'A' => Some(ComposeButton::Attach),
                        'c' | 'C' => Some(ComposeButton::Cancel),
                        'e' | 'E' => Some(ComposeButton::Edit),
                        'd' | 'D' => Some(ComposeButton::Draft),
                        's' | 'S' => Some(ComposeButton::Send),
                        _ => None,
                    };
                    if let Some(target) = target {
                        return self.activate_compose_button(target);
                    }
                }
                _ => {}
            },
        }

        Ok(())
    }

    fn activate_compose_button(&mut self, button: ComposeButton) -> Result<()> {
        match button {
            ComposeButton::Attach => {
                self.open_attach_prompt();
                Ok(())
            }
            ComposeButton::Cancel => {
                self.cancel_compose();
                Ok(())
            }
            ComposeButton::Edit => self.edit_compose_body(),
            ComposeButton::Draft => self.submit_outgoing(OutgoingKind::Draft),
            ComposeButton::Send => self.submit_outgoing(OutgoingKind::Send),
        }
    }

    fn open_attach_prompt(&mut self) {
        if let Some(compose) = self.compose.as_mut() {
            compose.clear_status();
            compose.open_attachment_prompt();
        }
    }

    /// Answer the question about an oversized file.
    ///
    /// Nothing else in compose responds until it is answered: attaching reads
    /// the whole file into the message, which is exactly the decision being
    /// asked about.  Keys that mean neither yes nor no are ignored rather than
    /// guessed at.
    fn handle_large_attachment_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => self.answer_large_attachment(true),
            KeyCode::Esc | KeyCode::Char('n' | 'N') => self.answer_large_attachment(false),
            _ => Ok(()),
        }
    }

    fn handle_attachment_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(compose) = self.compose.as_mut() else {
            return Ok(());
        };

        match key.code {
            KeyCode::Esc => {
                compose.close_attachment_prompt();
                return Ok(());
            }
            KeyCode::Enter => {
                let path_text = compose
                    .attachment_prompt()
                    .map(|(value, _)| value.trim().to_string())
                    .unwrap_or_default();
                if path_text.is_empty() {
                    compose.close_attachment_prompt();
                    return Ok(());
                }
                compose.close_attachment_prompt();
                return self.attach_paths(vec![path_text]);
            }
            KeyCode::Left => {
                if let Some(state) = compose.attachment_prompt_mut() {
                    state.move_left();
                }
                return Ok(());
            }
            KeyCode::Right => {
                if let Some(state) = compose.attachment_prompt_mut() {
                    state.move_right();
                }
                return Ok(());
            }
            KeyCode::Home => {
                if let Some(state) = compose.attachment_prompt_mut() {
                    state.move_home();
                }
                return Ok(());
            }
            KeyCode::End => {
                if let Some(state) = compose.attachment_prompt_mut() {
                    state.move_end();
                }
                return Ok(());
            }
            KeyCode::Backspace => {
                if let Some(state) = compose.attachment_prompt_mut() {
                    state.backspace();
                }
                return Ok(());
            }
            KeyCode::Delete => {
                if let Some(state) = compose.attachment_prompt_mut() {
                    state.delete();
                }
                return Ok(());
            }
            KeyCode::Char(ch) => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    return Ok(());
                }
                if let Some(state) = compose.attachment_prompt_mut() {
                    state.insert(ch);
                }
                return Ok(());
            }
            _ => {}
        }

        Ok(())
    }

    /// Attach every path in `paths`, asking about the large ones on the way.
    ///
    /// The single entry point for attaching, whether the paths came from the
    /// prompt or from a terminal drop.
    fn attach_paths(&mut self, paths: Vec<String>) -> Result<()> {
        if let Some(compose) = self.compose.as_mut() {
            // Dropping onto the open prompt answers the question it was asking.
            compose.close_attachment_prompt();
            compose.attach = AttachBatch {
                queue: paths.into(),
                ..AttachBatch::default()
            };
        }
        self.drain_attach_queue()
    }

    /// Attach queued files until one needs the user's say-so, or the batch ends.
    fn drain_attach_queue(&mut self) -> Result<()> {
        while let Some(path) = self
            .compose
            .as_mut()
            .and_then(|compose| compose.attach.queue.pop_front())
        {
            // A file that cannot be measured is left to the read below, which
            // reports why in the status line instead of guessing here.
            let size = fs::metadata(expand_user_path(&path)).map_or(0, |meta| meta.len());
            if size >= LARGE_ATTACHMENT_BYTES {
                if let Some(compose) = self.compose.as_mut() {
                    compose.attach.asking = Some((path, size));
                }
                return Ok(());
            }

            self.attach_file_from_path(&path)?;
        }

        self.finish_attach_batch();
        Ok(())
    }

    /// Take the answer to the question [`Self::drain_attach_queue`] parked on,
    /// then carry on with the rest of the batch.
    fn answer_large_attachment(&mut self, attach: bool) -> Result<()> {
        let Some((path, size)) = self
            .compose
            .as_mut()
            .and_then(|compose| compose.attach.asking.take())
        else {
            return Ok(());
        };

        if attach {
            self.attach_file_from_path(&path)?;
        } else if let Some(compose) = self.compose.as_mut() {
            let name = expand_user_path(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or(path);
            compose.attach.declined += 1;
            compose.set_status(format!(
                "Skipped '{name}' ({}).",
                format_size(size as usize).trim()
            ));
        }

        self.drain_attach_queue()
    }

    /// Report what a batch did, once nothing is left to ask about.
    ///
    /// A batch that handled a single file has already said so in its own words,
    /// naming the file; only a drop of several needs summarising.
    fn finish_attach_batch(&mut self) {
        let Some(compose) = self.compose.as_mut() else {
            return;
        };
        let batch = std::mem::take(&mut compose.attach);
        if batch.attached + batch.declined < 2 {
            return;
        }

        let mut status = match batch.attached {
            0 => String::new(),
            n => format!("Attached {n} files ({}).", format_size(batch.bytes).trim()),
        };
        if batch.declined > 0 {
            if !status.is_empty() {
                status.push(' ');
            }
            status.push_str(&format!("Skipped {}.", batch.declined));
        }
        compose.set_status(status);
    }

    /// Read `path_text` and add it to the compose attachment list.
    ///
    /// Returns whether an attachment was added; failures are reported in the
    /// compose status line.
    fn attach_file_from_path(&mut self, path_text: &str) -> Result<bool> {
        let Some(compose) = self.compose.as_mut() else {
            return Ok(false);
        };

        let path = expand_user_path(path_text);
        let data = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                compose.set_status(format!("Failed to attach '{}': {err}", path.display()));
                return Ok(false);
            }
        };

        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let mime_type = guess_mime_type(&path);
        let size = data.len();

        let attachment = OutgoingAttachment {
            filename: filename.clone(),
            mime_type,
            data,
        };
        compose.add_attachment(attachment);
        compose.attach.attached += 1;
        compose.attach.bytes += size;
        compose.set_status(format!(
            "Attached '{filename}' ({}).",
            format_size(size).trim()
        ));
        Ok(true)
    }

    /// Handle a bracketed-paste payload.
    ///
    /// A paste that is really a drag-and-drop of files becomes attachments;
    /// anything else is typed into whichever text input currently has focus.
    /// Bracketed paste is enabled terminal-wide, so this is the *only* path by
    /// which pasted text reaches any field -- dropping it here means the paste
    /// silently disappears.
    pub(crate) fn handle_paste_text(&mut self, text: &str) -> Result<()> {
        // Compose is locked while the backend has the message; see handle_compose_key.
        if self.outgoing.is_some() {
            return Ok(());
        }

        // A question about an oversized file owns the compose view until it is
        // answered; a paste arriving now has nowhere to go.
        if self
            .compose
            .as_ref()
            .is_some_and(|compose| compose.is_asking_about_large_attachment())
        {
            return Ok(());
        }

        if self.compose.is_some()
            && self.save_attachment.is_none()
            && let Some(paths) = dropped_file_paths(text)
        {
            return self.attach_paths(paths);
        }

        if let Some(dialog) = self.save_attachment.as_mut() {
            if !dialog.is_busy()
                && matches!(dialog.focus, SaveAttachmentFocus::Folder)
                && dialog.folder.insert_str(text)
            {
                dialog.clear_status();
            }
            return Ok(());
        }

        if let Some(compose) = self.compose.as_mut() {
            if let Some(prompt) = compose.attachment_prompt_mut() {
                prompt.insert_str(text);
                return Ok(());
            }

            match compose.focus() {
                ComposeFocus::Field(field) => {
                    if compose.field_state_mut(field).insert_str(text) {
                        compose.clear_status();
                    }
                }
                _ => {
                    compose.set_status(
                        "Pasted text goes into the header fields; press Enter on the body to edit it in $EDITOR."
                            .to_string(),
                    );
                }
            }
            return Ok(());
        }

        if self.search.as_ref().is_some_and(|search| search.focused) {
            if let Some(search) = self.search.as_mut() {
                search.input.insert_str(text);
            }
            self.recompute_search_filter();
        }

        Ok(())
    }

    fn edit_compose_body(&mut self) -> Result<()> {
        let markdown_source = match self.compose.as_ref() {
            Some(compose) => match compose.body_markdown() {
                Ok(content) => content,
                Err(err) => {
                    if let Some(compose) = self.compose.as_mut() {
                        compose.set_status(format!("Failed to prepare editor content: {err}"));
                    }
                    return Ok(());
                }
            },
            None => return Ok(()),
        };

        let editor_command = env::var("EDITOR")
            .or_else(|_| env::var("VISUAL"))
            .unwrap_or_else(|_| "vi".to_string());
        let mut argv =
            shell_split(&editor_command).unwrap_or_else(|_| vec![editor_command.clone()]);
        if argv.is_empty() {
            argv.push(editor_command);
        }

        let mut temp_file = match NamedTempFile::new() {
            Ok(file) => file,
            Err(err) => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.set_status(format!("Failed to create temp file: {err}"));
                }
                return Ok(());
            }
        };

        if let Err(err) = temp_file.write_all(markdown_source.as_bytes()) {
            if let Some(compose) = self.compose.as_mut() {
                compose.set_status(format!("Failed to write temp file: {err}"));
            }
            return Ok(());
        }

        if let Err(err) = temp_file.flush() {
            if let Some(compose) = self.compose.as_mut() {
                compose.set_status(format!("Failed to flush temp file: {err}"));
            }
            return Ok(());
        }

        let temp_path: PathBuf = temp_file.path().to_path_buf();

        let mut command = match argv.split_first() {
            Some((program, args)) => {
                let mut command = Command::new(program);
                command.args(args);
                command
            }
            None => Command::new("vi"),
        };
        command.arg(&temp_path);

        let editor_status = self.run_editor_command(&mut io::stdout(), &mut command);

        let status = match editor_status {
            Ok(status) => status,
            Err(err) => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.set_status(format!("Failed to launch editor: {err}"));
                }
                return Ok(());
            }
        };

        if !status.success() {
            if let Some(compose) = self.compose.as_mut() {
                compose.set_status("Editor cancelled message update.");
            }
            return Ok(());
        }

        let edited_markdown = match fs::read_to_string(&temp_path) {
            Ok(contents) => contents,
            Err(err) => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.set_status(format!("Failed to read editor output: {err}"));
                }
                return Ok(());
            }
        };

        if let Some(compose) = self.compose.as_mut() {
            match compose.update_body_from_markdown(&edited_markdown) {
                Ok(()) => {
                    compose.clear_status();
                    compose.set_status("Message updated.");
                    compose.set_focus(ComposeFocus::Body);
                }
                Err(err) => {
                    compose.set_status(format!("{err}"));
                }
            }
        }

        Ok(())
    }

    /// Run the editor with the terminal handed over to it, then take it back.
    ///
    /// The renderer owns three modes the editor cannot be expected to leave the
    /// way it found them: raw mode, the alternate screen, and bracketed paste.
    /// The editor needs a cooked terminal on the main screen to be usable at
    /// all, and it draws over whatever the renderer had put there.  Bracketed
    /// paste is the subtle one -- it is a terminal mode rather than a termios
    /// flag, so restoring raw mode does not bring it back, and vim, neovim and
    /// emacs all emit the disable sequence when they exit whether or not they
    /// turned it on themselves.  Losing it means
    /// [`Event::Paste`](crossterm::event::Event::Paste) never arrives again,
    /// and [`Self::handle_paste_text`] is the only route by which a terminal
    /// file drop becomes an attachment.
    ///
    /// Every mode has to be restored on every path out, including a child that
    /// exits non-zero and an editor that never launches at all.  Failures to
    /// restore go unreported on purpose: the terminal they would be reported on
    /// is the one that just failed.
    fn run_editor_command<W: Write>(
        &mut self,
        terminal: &mut W,
        command: &mut Command,
    ) -> io::Result<ExitStatus> {
        // Hand over a terminal in the state a child expects to find one:
        // cooked, on the main screen, with a cursor it can see.
        let _ = disable_raw_mode();
        let _ = execute!(terminal, DisableBracketedPaste, LeaveAlternateScreen, Show);

        let status = command.status();

        let _ = enable_raw_mode();
        let _ = execute!(terminal, EnterAlternateScreen, EnableBracketedPaste);

        // Re-entering the alternate screen clears it, so the renderer's record
        // of what is on screen is now wrong in every cell.
        self.needs_full_redraw = true;

        status
    }

    fn cancel_compose(&mut self) {
        if let Some(state) = self.compose.take() {
            let message = if state.is_editing_draft() {
                "Draft edit cancelled."
            } else {
                "Compose cancelled."
            };
            self.mailbox.status_line = Some(message.to_string());
        }
    }

    /// Hand the composed message to the backend on a worker thread.
    ///
    /// Uploading a message with attachments can take minutes, which is why this
    /// never calls into the backend directly: the UI keeps redrawing (and the
    /// progress line keeps ticking) while the worker runs, and
    /// [`Self::poll_outgoing_operation`] applies the outcome.
    fn submit_outgoing(&mut self, kind: OutgoingKind) -> Result<()> {
        if self.outgoing.is_some() {
            return Ok(());
        }

        let (draft_id, message) = match self.compose.as_ref() {
            Some(compose) => {
                let draft_id = compose.draft_id();
                match compose.to_outgoing() {
                    Ok(message) => (draft_id, message),
                    Err(err) => {
                        if let Some(compose) = self.compose.as_mut() {
                            let what = match kind {
                                OutgoingKind::Send => "message",
                                OutgoingKind::Draft => "draft",
                            };
                            compose.set_status(format!("Failed to prepare {what}: {err}"));
                        }
                        return Ok(());
                    }
                }
            }
            None => return Ok(()),
        };

        if kind == OutgoingKind::Send
            && message.to.is_empty()
            && message.cc.is_empty()
            && message.bcc.is_empty()
        {
            if let Some(compose) = self.compose.as_mut() {
                compose.set_status("Add at least one recipient.");
            }
            return Ok(());
        }

        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = std::sync::mpsc::channel();

        thread::spawn(move || {
            let result = match kind {
                OutgoingKind::Send => backend.send_message(message),
                OutgoingKind::Draft => backend.save_draft(message),
            };
            let _ = sender.send(result.map_err(|err| err.to_string()));
        });

        if let Some(compose) = self.compose.as_mut() {
            compose.clear_status();
        }
        self.outgoing = Some(OutgoingOperation {
            kind,
            draft_id,
            receiver,
            started: Instant::now(),
        });

        Ok(())
    }

    fn poll_outgoing_operation(&mut self) {
        let result = {
            let Some(op) = self.outgoing.as_ref() else {
                return;
            };
            match op.receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    Err("the worker thread exited unexpectedly".to_string())
                }
            }
        };

        let Some(op) = self.outgoing.take() else {
            return;
        };
        self.finish_outgoing(op, result);
    }

    fn finish_outgoing(&mut self, op: OutgoingOperation, result: Result<(), String>) {
        match result {
            Ok(()) => {
                let mut status = match (op.kind, op.draft_id.is_some()) {
                    (OutgoingKind::Send, _) => "Message sent.".to_string(),
                    (OutgoingKind::Draft, true) => "Draft updated.".to_string(),
                    (OutgoingKind::Draft, false) => "Draft saved.".to_string(),
                };

                // The stored copy this replaces is only safe to delete now that
                // the backend has accepted the new one.
                if let Some(id) = op.draft_id {
                    self.remove_message_from_mailbox(id);
                    if let Err(err) = self.submit_actions(vec![Action::new(ActionType::Delete, id)])
                    {
                        status = match op.kind {
                            OutgoingKind::Send => {
                                format!("Message sent but failed to remove draft: {err}")
                            }
                            OutgoingKind::Draft => {
                                format!("Draft saved but failed to remove previous copy: {err}")
                            }
                        };
                    }
                }

                self.compose = None;
                self.mailbox.status_line = Some(status);
            }
            Err(err) => {
                let text = match op.kind {
                    OutgoingKind::Send => format!("Failed to send: {err}"),
                    OutgoingKind::Draft => format!("Failed to save draft: {err}"),
                };
                // Keep the message: compose is still open for a retry.
                match self.compose.as_mut() {
                    Some(compose) => compose.set_status(text),
                    None => self.mailbox.status_line = Some(text),
                }
            }
        }
    }

    fn open_selected_entry(&mut self) -> Result<()> {
        if !self.try_open_selected_draft()? {
            self.open_selected_message()?;
        }
        Ok(())
    }

    fn try_open_selected_draft(&mut self) -> Result<bool> {
        let Some(idx) = self.real_selected_index() else {
            return Ok(false);
        };

        let Some(message) = self.mailbox.messages.get(idx).cloned() else {
            return Ok(false);
        };

        if !self.is_draft_message(&message) {
            return Ok(false);
        }

        self.begin_message_load(message, MessageLoadPurpose::Draft, None, None);
        Ok(true)
    }

    fn show_draft_compose(&mut self, message: &Message, body: Document, loaded: LoadedMessage) {
        let mut compose = ComposeState::from_draft(
            message.id,
            message.recipients.join(", "),
            String::new(),
            String::new(),
            message.subject.clone(),
            body,
        );

        // Without this the attachments are dropped on the floor and re-saving
        // the draft silently discards them.
        compose.set_attachments(loaded.attachments);

        self.compose = Some(compose);
        self.message_view = None;
        self.mailbox.status_line = Some(draft_status_line("Editing draft.", loaded.unavailable));
    }

    fn is_draft_message(&self, message: &Message) -> bool {
        if self.current_mailbox == MailboxKind::Drafts {
            return true;
        }

        message
            .labels
            .iter()
            .any(|label| label.to_ascii_lowercase().contains("draft"))
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.visible_message_count();
        if len == 0 {
            self.mailbox.selected = None;
            return;
        }
        let current = self.mailbox.selected.unwrap_or(len.saturating_sub(1)) as isize;
        let max_index = len as isize - 1;
        let next = min(max(0, current + delta), max_index) as usize;
        self.mailbox.selected = Some(next);
    }

    fn select_first(&mut self) {
        if self.visible_message_count() == 0 {
            self.mailbox.selected = None;
        } else {
            self.mailbox.selected = Some(0);
        }
    }

    fn select_last(&mut self) {
        let len = self.visible_message_count();
        if len == 0 {
            self.mailbox.selected = None;
        } else {
            self.mailbox.selected = Some(len - 1);
        }
    }

    /// Schedule a move/delete action with dedup logic.
    ///
    /// - Same action type for the same message already exists → no-op, return `AlreadyScheduled`.
    /// - Different move action for same message exists → replace in-place, return `Replaced`.
    /// - No existing action → push new action, return `Added`.
    fn schedule_action(
        &mut self,
        action_type: ActionType,
        message_id: MessageId,
        current_status: MessageStatus,
    ) -> ScheduleOutcome {
        if let Some(pos) = self
            .scheduled_actions
            .iter()
            .position(|a| a.message_id == message_id)
        {
            if self.scheduled_actions[pos].action_type == action_type {
                // Same action already scheduled — keep it (idempotent).
                // Use 'u' to explicitly undo a scheduled action.
                return ScheduleOutcome::AlreadyScheduled;
            }
            // Different action → replace, keep original_status from first scheduling
            let original = self.scheduled_actions[pos].original_status;
            self.scheduled_actions[pos].action_type = action_type;
            self.scheduled_actions[pos].original_status = original.or(Some(current_status));
            return ScheduleOutcome::Replaced;
        }

        self.scheduled_actions.push(Action::with_original_status(
            action_type,
            message_id,
            current_status,
        ));
        ScheduleOutcome::Added
    }

    fn toggle_star(&mut self) {
        if self.mailbox.selected.is_none() {
            return;
        }

        let Some(msg) = self.selected_loaded_message_mut() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        msg.starred = !msg.starred;
        let action_type = if msg.starred {
            ActionType::MarkAsStarred
        } else {
            ActionType::MarkAsUnstarred
        };
        let message_id = msg.id;

        let action = Action::new(action_type, message_id);
        if let Err(_err) = self.submit_immediate_actions(vec![action]) {
            self.mailbox.status_line = Some("Failed to apply star change.".to_string());
        }
        self.mailbox.status_line = None;
        self.sync_message_view_state();
    }

    fn mark_selected_important(&mut self, important: bool) {
        if self.mailbox.selected.is_none() {
            return;
        }

        let Some(msg) = self.selected_loaded_message_mut() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        if msg.important == important {
            return;
        }

        msg.important = important;
        let action_type = if important {
            ActionType::MarkAsImportant
        } else {
            ActionType::MarkAsUnimportant
        };
        let message_id = msg.id;

        let action = Action::new(action_type, message_id);
        if let Err(_err) = self.submit_immediate_actions(vec![action]) {
            self.mailbox.status_line = Some("Failed to apply importance change.".to_string());
        }
        self.mailbox.status_line = None;
        self.sync_message_view_state();
    }

    fn schedule_archive(&mut self) {
        if self.current_mailbox == MailboxKind::Archive {
            self.schedule_move_to_inbox();
            return;
        }

        let Some(idx) = self.mailbox.selected else {
            return;
        };

        let Some(msg) = self.selected_loaded_message_mut() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        let message_id = msg.id;
        let current_status = msg.status;

        match self.schedule_action(ActionType::Archive, message_id, current_status) {
            ScheduleOutcome::AlreadyScheduled => {
                self.advance_selection_after_action(idx);
            }
            ScheduleOutcome::Added | ScheduleOutcome::Replaced => {
                if let Some(msg) = self
                    .mailbox
                    .messages
                    .iter_mut()
                    .find(|m| m.id == message_id)
                {
                    msg.status = MessageStatus::Archived;
                }
                self.advance_selection_after_action(idx);
            }
        }
        self.sync_message_view_state();
    }

    fn schedule_delete(&mut self) {
        if self.current_mailbox == MailboxKind::Trash {
            self.schedule_move_to_inbox();
            return;
        }

        let Some(idx) = self.mailbox.selected else {
            return;
        };

        let Some(msg) = self.selected_loaded_message_mut() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        let message_id = msg.id;
        let current_status = msg.status;

        match self.schedule_action(ActionType::Delete, message_id, current_status) {
            ScheduleOutcome::AlreadyScheduled => {
                self.advance_selection_after_action(idx);
            }
            ScheduleOutcome::Added | ScheduleOutcome::Replaced => {
                if let Some(msg) = self
                    .mailbox
                    .messages
                    .iter_mut()
                    .find(|m| m.id == message_id)
                {
                    msg.status = MessageStatus::Deleted;
                }
                self.advance_selection_after_action(idx);
            }
        }
        self.sync_message_view_state();
    }

    fn schedule_move_to_spam(&mut self) {
        if self.current_mailbox == MailboxKind::Spam {
            self.schedule_move_to_inbox();
            return;
        }

        let Some(idx) = self.mailbox.selected else {
            return;
        };

        let Some(msg) = self.selected_loaded_message_mut() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        let message_id = msg.id;
        let current_status = msg.status;

        match self.schedule_action(ActionType::MoveToSpam, message_id, current_status) {
            ScheduleOutcome::AlreadyScheduled => {
                self.advance_selection_after_action(idx);
            }
            ScheduleOutcome::Added | ScheduleOutcome::Replaced => {
                if let Some(msg) = self
                    .mailbox
                    .messages
                    .iter_mut()
                    .find(|m| m.id == message_id)
                {
                    msg.status = MessageStatus::Spam;
                }
                self.advance_selection_after_action(idx);
            }
        }
        self.mailbox.status_line = None;
        self.sync_message_view_state();
    }

    fn schedule_move_to_inbox(&mut self) {
        let Some(idx) = self.mailbox.selected else {
            return;
        };

        let Some(msg) = self.selected_loaded_message_mut() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        let restore_unread = matches!(msg.status, MessageStatus::New);
        let action_type = if restore_unread {
            ActionType::MoveToInboxUnread
        } else {
            ActionType::MoveToInboxRead
        };
        let message_id = msg.id;
        let current_status = msg.status;

        match self.schedule_action(action_type, message_id, current_status) {
            ScheduleOutcome::AlreadyScheduled => {
                self.advance_selection_after_action(idx);
            }
            ScheduleOutcome::Added | ScheduleOutcome::Replaced => {
                if let Some(msg) = self
                    .mailbox
                    .messages
                    .iter_mut()
                    .find(|m| m.id == message_id)
                {
                    msg.status = MessageStatus::PendingInbox;
                }
                self.advance_selection_after_action(idx);
            }
        }
        self.mailbox.status_line = None;
        self.sync_message_view_state();
    }

    fn toggle_unread(&mut self) {
        if self.mailbox.selected.is_none() {
            return;
        }

        let Some(msg) = self.selected_loaded_message_mut() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        match msg.status {
            MessageStatus::New | MessageStatus::Read => {
                // Immediate flag flip — apply right away
                let new_status = if msg.status == MessageStatus::New {
                    MessageStatus::Read
                } else {
                    MessageStatus::New
                };
                msg.status = new_status;
                let action_type = if new_status == MessageStatus::New {
                    ActionType::MoveToInboxUnread
                } else {
                    ActionType::MoveToInboxRead
                };
                let message_id = msg.id;
                let action = Action::new(action_type, message_id);
                if let Err(_err) = self.submit_immediate_actions(vec![action]) {
                    self.mailbox.status_line =
                        Some("Failed to apply read/unread change.".to_string());
                }
            }
            MessageStatus::Deleted | MessageStatus::Archived | MessageStatus::Spam => {
                let message_id = msg.id;
                let current_status = msg.status;
                let idx = self.mailbox.selected.unwrap();

                // If there's a scheduled action for this message, undo it.
                if let Some(pos) = self
                    .scheduled_actions
                    .iter()
                    .position(|a| a.message_id == message_id)
                {
                    let removed = self.scheduled_actions.remove(pos);
                    let original_status = removed.original_status.unwrap_or(current_status);
                    if let Some(msg) = self
                        .mailbox
                        .messages
                        .iter_mut()
                        .find(|m| m.id == message_id)
                    {
                        msg.status = original_status;
                    }
                } else {
                    // Genuinely deleted/archived on server — rescue via scheduled action
                    match self.schedule_action(
                        ActionType::MoveToInboxRead,
                        message_id,
                        current_status,
                    ) {
                        ScheduleOutcome::AlreadyScheduled => {}
                        ScheduleOutcome::Added | ScheduleOutcome::Replaced => {
                            if let Some(msg) = self
                                .mailbox
                                .messages
                                .iter_mut()
                                .find(|m| m.id == message_id)
                            {
                                msg.status = MessageStatus::PendingInbox;
                            }
                        }
                    }
                }
                self.advance_selection_after_action(idx);
            }
            MessageStatus::PendingInbox => {
                let message_id = msg.id;
                let idx = self.mailbox.selected.unwrap();
                if let Some(pos) = self
                    .scheduled_actions
                    .iter()
                    .position(|a| a.message_id == message_id)
                {
                    let removed = self.scheduled_actions.remove(pos);
                    let original_status = removed.original_status.unwrap_or(MessageStatus::Read);
                    if let Some(msg) = self
                        .mailbox
                        .messages
                        .iter_mut()
                        .find(|m| m.id == message_id)
                    {
                        msg.status = original_status;
                    }
                }
                self.advance_selection_after_action(idx);
            }
        }
        self.mailbox.status_line = None;
        self.sync_message_view_state();
    }

    fn advance_selection_after_action(&mut self, current_idx: usize) {
        let len = self.visible_message_count();
        if len == 0 {
            self.mailbox.selected = None;
            return;
        }
        let next_idx = if current_idx + 1 >= len {
            len.saturating_sub(1)
        } else {
            current_idx + 1
        };
        self.mailbox.selected = Some(next_idx.min(len.saturating_sub(1)));
    }

    /// Submit immediate actions (star, important, read/unread) to the backend
    /// with priority and track them as an immediate batch.  Finalisation will
    /// NOT remove messages from the mailbox view for these batches.
    fn submit_immediate_actions(&mut self, actions: Vec<Action>) -> Result<()> {
        if actions.is_empty() {
            return Ok(());
        }

        let action_count = actions.len();
        if let Some(progress) = self.commit_progress.as_mut() {
            progress.total += action_count;
        } else {
            self.commit_progress = Some(CommitProgress {
                total: action_count,
                completed: 0,
            });
        }

        let receiver = match self.backend.apply_immediate_actions(actions.clone()) {
            Ok(receiver) => receiver,
            Err(err) => {
                if let Some(progress) = self.commit_progress.as_mut() {
                    progress.total = progress.total.saturating_sub(action_count);
                    progress.completed = progress.completed.min(progress.total);
                    if progress.total == 0 {
                        self.commit_progress = None;
                    }
                }
                return Err(err);
            }
        };

        self.commit_batches
            .push_back(CommitBatchState::new_immediate(actions, receiver));

        Ok(())
    }

    /// Hand the current batch of scheduled actions to the backend.
    ///
    /// The backend responds via the channel returned from
    /// [`MailBackend::apply_actions`]; until those updates arrive the UI can
    /// continue scheduling new work.
    fn submit_actions(&mut self, actions: Vec<Action>) -> Result<()> {
        if actions.is_empty() {
            return Ok(());
        }

        let action_count = actions.len();
        if let Some(progress) = self.commit_progress.as_mut() {
            progress.total += action_count;
        } else {
            self.commit_progress = Some(CommitProgress {
                total: action_count,
                completed: 0,
            });
        }

        let receiver = match self.backend.apply_actions(actions.clone()) {
            Ok(receiver) => receiver,
            Err(err) => {
                if let Some(progress) = self.commit_progress.as_mut() {
                    progress.total = progress.total.saturating_sub(action_count);
                    progress.completed = progress.completed.min(progress.total);
                    if progress.total == 0 {
                        self.commit_progress = None;
                    }
                }

                return Err(err);
            }
        };

        self.commit_batches
            .push_back(CommitBatchState::new(actions, receiver));

        Ok(())
    }

    fn commit_actions(&mut self) -> Result<()> {
        if self.scheduled_actions.is_empty() {
            return Ok(());
        }

        let actions = std::mem::take(&mut self.scheduled_actions);
        if let Err(err) = self.submit_actions(actions.clone()) {
            self.scheduled_actions.extend(actions);
            return Err(err.context("failed to queue actions with backend"));
        }

        // Optimistically remove committed messages from the mailbox view.
        let committed_ids: std::collections::HashSet<MessageId> =
            actions.iter().map(|a| a.message_id).collect();

        // Remember which message the cursor is on so we can restore it.
        // Use real_selected_index() to map through filtered indices when
        // search is active.
        let selected_id = self
            .real_selected_index()
            .and_then(|idx| self.mailbox.messages.get(idx))
            .map(|msg| msg.id);

        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for msg in self.mailbox.messages.drain(..) {
            if committed_ids.contains(&msg.id) {
                removed.push(msg);
            } else {
                kept.push(msg);
            }
        }
        self.mailbox.messages = kept;

        if let Some(batch) = self.commit_batches.back_mut() {
            batch.removed_messages = removed;
        }

        resequence_messages(&mut self.mailbox.messages);

        // Restore selection: find the previously selected message in the
        // remaining list.  If it was removed, clamp the index.
        if self.mailbox.messages.is_empty() {
            self.mailbox.selected = None;
            self.message_view = None;
        } else if let Some(id) = selected_id {
            if let Some((new_idx, _)) = self
                .mailbox
                .messages
                .iter()
                .enumerate()
                .find(|(_, m)| m.id == id)
            {
                self.mailbox.selected = Some(new_idx);
            } else {
                // Selected message was removed — keep the same position, clamped.
                let idx = self.mailbox.selected.unwrap_or(0);
                self.mailbox.selected = Some(idx.min(self.mailbox.messages.len() - 1));
            }
        }

        // Recompute the search filter since messages were removed and
        // resequenced — the old filtered_indices are now stale.
        if self.search.is_some() {
            self.recompute_search_filter();
        }

        self.sync_message_view_state();
        self.normalize_scroll();

        Ok(())
    }

    fn open_selected_message(&mut self) -> Result<()> {
        if self.mailbox.selected.is_none() {
            return Ok(());
        }

        let message = match self.selected_loaded_message() {
            Some(msg) => msg.clone(),
            None => {
                self.mailbox
                    .status_line
                    .get_or_insert_with(|| "Message is still loading.".to_string());
                return Ok(());
            }
        };

        self.begin_message_load(message, MessageLoadPurpose::View, None, None);
        Ok(())
    }

    /// Correct the message list's attachment marker from a body we just parsed.
    ///
    /// Until a message is opened the marker comes from a backend guess — the
    /// IMAP BODYSTRUCTURE or JMAP's `hasAttachment` — which can disagree with
    /// what the parsed MIME tree yields. Every load runs through here, so
    /// replying to, forwarding, or reopening a draft corrects the list as
    /// well, not just opening the message.
    fn sync_attachment_indicator(&mut self, message_id: MessageId, has_attachments: bool) {
        if let Some(slot) = self
            .mailbox
            .messages
            .iter_mut()
            .find(|message| message.id == message_id)
        {
            slot.has_attachments = has_attachments;
        }

        if let Some(view) = self.message_view.as_mut()
            && view.message_id == message_id
        {
            view.message.has_attachments = has_attachments;
        }
    }

    /// Install a body that finished loading in the message viewer.
    ///
    /// The mailbox may have moved on while the load ran, so the list entry is
    /// looked up again by id rather than trusting the index we started with.
    fn show_loaded_message(&mut self, message_id: MessageId, content: MessageContent) {
        let found = self
            .mailbox
            .messages
            .iter()
            .enumerate()
            .find(|(_, msg)| msg.id == message_id)
            .map(|(idx, msg)| (idx, msg.clone()));

        let Some((real_idx, mut message)) = found else {
            self.mailbox.status_line = Some("Message is no longer in this mailbox.".to_string());
            return;
        };

        let has_attachments = marks_as_having_attachments(&content);
        message.has_attachments = has_attachments;
        self.sync_attachment_indicator(message_id, has_attachments);

        let raw_html = content
            .part("text/html")
            .map(|part| String::from_utf8_lossy(&part.content).into_owned());
        let document = raw_html
            .as_ref()
            .and_then(|html| html::parse(Cursor::new(html)).ok());

        // Schedule mark-as-read after 3 seconds if the message is unread.
        let read_at = if message.status == MessageStatus::New {
            Some(Instant::now() + std::time::Duration::from_secs(3))
        } else {
            None
        };

        // Defocus search when opening a message so mailbox keys work on return.
        if let Some(search) = self.search.as_mut() {
            search.focused = false;
        }

        self.message_view = Some(MessageViewState {
            message_id: message.id,
            message_index: real_idx,
            message,
            content,
            document,
            raw_html,
            scroll: 0,
            unformatted: false,
            info_line: None,
            read_at,
        });
    }

    fn open_adjacent_message(&mut self, offset: isize) -> Result<()> {
        let Some(current) = self.message_view.as_ref() else {
            return Ok(());
        };
        let len = self.visible_message_count() as isize;
        if len == 0 {
            return Ok(());
        }
        // Find the current visible position.
        let visible_pos = if let Some(search) = &self.search {
            search
                .filtered_indices
                .iter()
                .position(|&idx| idx == current.message_index)
                .unwrap_or(0)
        } else {
            current.message_index
        };
        let next_visible = visible_pos as isize + offset;
        if next_visible < 0 || next_visible >= len {
            return Ok(());
        }
        self.mailbox.selected = Some(next_visible as usize);
        self.open_selected_message()
    }

    fn sync_message_view_state(&mut self) {
        let message_id = match self.message_view.as_ref().map(|view| view.message_id) {
            Some(id) => id,
            None => return,
        };

        let update = self
            .mailbox
            .messages
            .iter()
            .enumerate()
            .find(|(_, msg)| msg.id == message_id)
            .map(|(idx, msg)| (idx, msg.clone()));

        match update {
            Some((idx, message)) => {
                if let Some(view) = self.message_view.as_mut() {
                    view.message_index = idx;
                    view.message = message;
                }
            }
            None => {
                self.message_view = None;
            }
        }
    }

    fn update_selection_after_refresh(&mut self, current_id: Option<MessageId>) {
        if self.mailbox.messages.is_empty() {
            self.mailbox.selected = None;
            self.message_view = None;
            self.mailbox.scroll_top = 0;
            return;
        }

        if let Some(id) = current_id {
            if let Some((idx, _)) = self
                .mailbox
                .messages
                .iter()
                .enumerate()
                .find(|(_, msg)| msg.id == id)
            {
                self.mailbox.selected = Some(idx);
            }
        } else if self.mailbox.selected.is_none() {
            self.mailbox.selected = last_loaded_index(&self.mailbox.messages)
                .or_else(|| Some(self.mailbox.messages.len() - 1));
        }

        self.sync_message_view_state();
        self.normalize_scroll();
    }

    pub(crate) fn formatted_message_row(
        &self,
        message: &Message,
        now: OffsetDateTime,
    ) -> MessageRow {
        if message.is_placeholder() {
            return MessageRow {
                flags: "    ".to_string(),
                date: "Loading".to_string(),
                sender: padded_sender("Loading"),
                size: String::new(),
                subject: "Loading message...".to_string(),
                labels: Vec::new(),
                status: MessageStatus::Read,
                starred: false,
            };
        }

        let display_name = if matches!(
            self.current_mailbox,
            MailboxKind::Sent | MailboxKind::Drafts
        ) {
            message.recipients_display()
        } else {
            message.sender.clone()
        };

        MessageRow {
            flags: message.flag_string(),
            date: message.formatted_received(now),
            sender: padded_sender(&display_name),
            size: format_size(message.size),
            subject: message.subject.clone(),
            labels: message.labels.clone(),
            status: message.status,
            starred: message.starred,
        }
    }
}

pub(crate) struct MessageRow {
    pub(crate) flags: String,
    pub(crate) date: String,
    pub(crate) sender: String,
    pub(crate) size: String,
    pub(crate) subject: String,
    pub(crate) labels: Vec<String>,
    pub(crate) status: MessageStatus,
    pub(crate) starred: bool,
}

impl App {
    fn normalize_scroll(&mut self) {
        let len = self.visible_message_count();
        if len == 0 {
            self.mailbox.scroll_top = 0;
        } else {
            let max_top = len.saturating_sub(1);
            if self.mailbox.scroll_top > max_top {
                self.mailbox.scroll_top = max_top;
            }
        }
    }
}

/// Whether a loaded body earns its message the `@` marker in the list.
///
/// Inline parts are listed among the attachments so the save dialog can offer
/// them, but they are not what the marker means: an embedded signature logo
/// would otherwise mark half the newsletters in a mailbox, and only from the
/// moment each one was opened. Both places that correct the marker after a load
/// ask here, so the two cannot drift apart.
fn marks_as_having_attachments(content: &MessageContent) -> bool {
    content
        .attachments
        .iter()
        .any(|attachment| !attachment.inline)
}

/// What base64 turns `raw` bytes into: four characters per three bytes, plus
/// the line break MIME requires every 76 characters.
fn base64_size(raw: usize) -> usize {
    let encoded = raw.div_ceil(3) * 4;
    encoded + encoded.div_ceil(76) * 2
}

fn default_download_dir() -> PathBuf {
    if let Some(value) = env::var_os("XDG_DOWNLOAD_DIR") {
        let path = PathBuf::from(&value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    if let Some(home) = env::var_os("HOME") {
        let mut path = PathBuf::from(home);
        path.push("Downloads");
        return path;
    }
    PathBuf::from("Downloads")
}

fn sanitize_filename(name: &str) -> String {
    let trimmed = name.trim().trim_matches(['/', '\\']);
    let cleaned: String = trimmed
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | '\0' => '_',
            _ => ch,
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        String::new()
    } else {
        cleaned
    }
}

/// Rebuild compose attachments from a stored message's MIME parts.
///
/// Returns the attachments plus the number that could not be recovered.
/// Backends that hand out blob pointers instead of bytes (JMAP) are downloaded
/// here, one round trip per attachment, so this belongs on a worker thread --
/// [`App::begin_message_load`] is the only caller outside tests.
fn restore_compose_attachments(
    backend: &dyn MailBackend,
    content: &MessageContent,
) -> (Vec<OutgoingAttachment>, usize) {
    let mut restored = Vec::new();
    let mut unavailable = 0usize;

    // Inline parts stay behind. A forward quotes the body as text, so the
    // `cid:` references that put them there are gone -- carrying them over
    // would turn every signature logo into a file attached to the forward.
    for (idx, attachment) in content
        .attachments
        .iter()
        .filter(|attachment| !attachment.inline)
        .enumerate()
    {
        let data = match attachment.data.clone() {
            Some(data) => Some(data),
            None => match attachment.blob_id.as_deref() {
                Some(blob_id) => backend.fetch_attachment_blob(blob_id).ok(),
                None => None,
            },
        };

        match data {
            Some(data) => restored.push(OutgoingAttachment {
                filename: attachment
                    .filename
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| fallback_attachment_name(idx, &attachment.mime_type)),
                mime_type: attachment.mime_type.clone(),
                data,
            }),
            None => unavailable += 1,
        }
    }

    (restored, unavailable)
}

/// Progress-indicator text for a message load in flight.
fn message_load_label(purpose: MessageLoadPurpose, message: &Message) -> String {
    let subject = truncate_label(message.subject.trim(), 40);
    match purpose {
        MessageLoadPurpose::View => format!("Loading '{subject}'"),
        MessageLoadPurpose::Reply { reply_all: false } => format!("Loading '{subject}' to reply"),
        MessageLoadPurpose::Reply { reply_all: true } => {
            format!("Loading '{subject}' to reply to all")
        }
        MessageLoadPurpose::Forward => format!("Loading '{subject}' to forward"),
        MessageLoadPurpose::Draft => format!("Opening draft '{subject}'"),
    }
}

fn truncate_label(value: &str, max_chars: usize) -> String {
    if value.is_empty() {
        return "(no subject)".to_string();
    }
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// Append a warning to `base` when attachments could not be carried over.
///
/// Losing an attachment has to be visible: silently dropping it is how a draft
/// gets sent without the file it was about.
fn draft_status_line(base: &str, unavailable: usize) -> String {
    match unavailable {
        0 => base.to_string(),
        1 => format!("{base} 1 attachment could not be loaded and will not be sent."),
        n => format!("{base} {n} attachments could not be loaded and will not be sent."),
    }
}

/// Name to save an attachment under when the MIME part carries no filename.
///
/// Inline parts of `multipart/related` messages (embedded images, mostly) have
/// no filename, so the extension has to come from the content type instead --
/// saving them all as `.bin` would hide what they actually are.
fn fallback_attachment_name(index: usize, mime_type: &str) -> String {
    format!(
        "attachment-{}.{}",
        index + 1,
        extension_for_mime_type(mime_type)
    )
}

/// Best-effort file extension for a MIME type, mirroring [`guess_mime_type`].
///
/// Deliberately not `mime_guess`'s reverse mapping: that returns every
/// extension registered for a type in alphabetical order, so the first entry is
/// arbitrary (`application/octet-stream` starts at `aaf`, `video/x-matroska` at
/// `mk3d`).  A short table of the types that actually turn up, plus the subtype
/// when it already reads like an extension, does better.
fn extension_for_mime_type(mime_type: &str) -> String {
    let essence = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let mapped = match essence.as_str() {
        "text/plain" => Some("txt"),
        "text/html" => Some("html"),
        "text/calendar" => Some("ics"),
        "application/json" => Some("json"),
        "application/xml" | "text/xml" => Some("xml"),
        "application/pdf" => Some("pdf"),
        "application/zip" => Some("zip"),
        "application/gzip" => Some("gz"),
        "application/x-tar" => Some("tar"),
        "application/x-7z-compressed" => Some("7z"),
        "application/x-xz" => Some("xz"),
        "application/x-rar-compressed" | "application/vnd.rar" => Some("rar"),
        "application/rtf" => Some("rtf"),
        "application/msword" => Some("doc"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        "application/vnd.ms-excel" => Some("xls"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some("xlsx"),
        "application/vnd.ms-powerpoint" => Some("ppt"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => Some("pptx"),
        "image/jpeg" => Some("jpg"),
        "image/svg+xml" => Some("svg"),
        "image/tiff" => Some("tiff"),
        "audio/mpeg" => Some("mp3"),
        "audio/m4a" | "audio/mp4" => Some("m4a"),
        "video/quicktime" => Some("mov"),
        "video/x-matroska" => Some("mkv"),
        "message/rfc822" => Some("eml"),
        _ => None,
    };

    if let Some(ext) = mapped {
        return ext.to_string();
    }

    // Fall back to the subtype when it reads like an extension already
    // (image/png, audio/flac, image/webp, ...).
    let subtype = essence.split('/').nth(1).unwrap_or_default();
    if !subtype.is_empty()
        && subtype.len() <= 5
        && subtype.chars().all(|ch| ch.is_ascii_alphanumeric())
    {
        return subtype.to_string();
    }

    "bin".to_string()
}

fn unique_path_in(folder: &std::path::Path, name: &str) -> PathBuf {
    let candidate = folder.join(name);
    if !candidate.exists() {
        return candidate;
    }

    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), Some(ext.to_string())),
        _ => (name.to_string(), None),
    };

    for counter in 1..=1_000_000 {
        let candidate_name = match ext.as_deref() {
            Some(ext) => format!("{stem} ({counter}).{ext}"),
            None => format!("{stem} ({counter})"),
        };
        let candidate = folder.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    folder.join(name)
}

/// Normalise a path as typed by the user or delivered by a terminal drop.
///
/// Handles surrounding quotes, `file://` URLs (percent-encoded), backslash
/// escapes, and `~`. Unescaping happens *before* tilde expansion so that
/// `~/My\ File.pdf` resolves -- doing it the other way round leaves the
/// backslash in the file name.
fn expand_user_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let unquoted = if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };

    // A `file://` URL is percent-encoded; a plain path is not and must be left
    // alone, or a file literally named "100%20" would break.
    let normalised = match unquoted.strip_prefix("file://") {
        Some(rest) => {
            // An empty or `localhost` authority both mean the local machine.
            let path = rest.strip_prefix("localhost").unwrap_or(rest);
            percent_decode(path)
        }
        None => unescape_path_chars(unquoted),
    };

    if let Some(rest) = normalised.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        let mut buf = PathBuf::from(home);
        buf.push(rest);
        return buf;
    }

    if normalised == "~"
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home);
    }

    PathBuf::from(normalised)
}

/// Strip the backslash escapes terminals add when a dropped path contains
/// characters the shell would otherwise split on.
fn unescape_path_chars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(next) = chars.peek()
            && matches!(next, ' ' | '\t' | '\\' | '\'' | '"' | '(' | ')')
        {
            out.push(*next);
            chars.next();
            continue;
        }
        out.push(ch);
    }
    out
}

/// Decode `%XX` escapes in a `file://` URL path.
///
/// Invalid escapes are passed through verbatim rather than dropped, so a
/// malformed URL still produces a path the user can recognise in the error
/// message.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            let hex = &text[idx + 1..idx + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Content type to send an attached file as, derived from its extension.
///
/// `mime_guess` carries the full extension table -- a hand-written one always
/// misses whatever the user actually attaches (`.rs`, `.toml`, `.heic`, ...).
/// Unknown extensions become `application/octet-stream`.  Text types get an
/// explicit `charset=utf-8`, without which recipients are free to render the
/// part as Latin-1.
fn guess_mime_type(path: &std::path::Path) -> String {
    let guess = mime_guess::from_path(path).first_or_octet_stream();
    if guess.type_() == mime_guess::mime::TEXT
        && guess.get_param(mime_guess::mime::CHARSET).is_none()
    {
        format!("{}; charset=utf-8", guess.essence_str())
    } else {
        guess.essence_str().to_string()
    }
}

/// Recognise a paste that is really a drag-and-drop of files.
///
/// Terminals deliver dropped files as absolute paths, optionally as `file://`
/// URLs, quoted, or backslash-escaped. Requiring *every* candidate to be an
/// absolute path that exists keeps ordinary text pastes out of the attachment
/// path -- including a paste that happens to name a file in the current
/// directory, which is why relative paths are rejected here.
fn dropped_file_paths(text: &str) -> Option<Vec<String>> {
    let candidates = parse_paste_paths(text);
    if candidates.is_empty() {
        return None;
    }

    let all_files = candidates.iter().all(|candidate| {
        let path = expand_user_path(candidate);
        path.is_absolute() && path.is_file()
    });

    if all_files { Some(candidates) } else { None }
}

/// Split pasted text into candidate file paths.
///
/// Terminals that translate drag-and-drop into a paste may deliver one or
/// multiple paths separated by whitespace, quoted to protect embedded spaces,
/// or with literal backslash escapes. This helper returns candidates that
/// [`expand_user_path`] can subsequently normalise.
fn parse_paste_paths(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Fast path: multiple paths separated by newlines.
    let lines: Vec<&str> = trimmed
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    if lines.len() > 1 {
        return lines.into_iter().map(str::to_string).collect();
    }

    // Single-line: shell-split to find the token boundaries of a multi-file
    // drop. A single token is returned raw so that [`expand_user_path`] stays
    // the only place that unescapes -- otherwise a path typed as
    // `/tmp/a\ b.txt` would be unescaped twice, and a path typed with literal
    // spaces would be split into pieces.
    match shell_split(trimmed) {
        Ok(parts) if parts.len() > 1 => parts,
        _ => vec![trimmed.to_string()],
    }
}

/// What the snapshot harness needs and the main loop does not.  See
/// [`crate::test_harness`].
///
/// A `recorder` build of the binary compiles this without using it -- the
/// harness that calls it lives in the demo example, which is its own crate --
/// so dead code is expected there and only there.
#[cfg(any(test, feature = "recorder"))]
#[cfg_attr(not(test), allow(dead_code))]
impl App {
    /// Whether any worker thread still owes an answer that would change the
    /// screen.  The harness drives [`Self::poll_backend_events`] until this is
    /// false, the way the main loop polls every frame.
    pub(crate) fn has_work_in_flight(&self) -> bool {
        self.accounts.iter().any(|account| {
            account.mailbox_loader.is_some()
                || account.message_loader.is_some()
                || !account.commit_batches.is_empty()
        }) || self.outgoing.is_some()
            || self
                .save_attachment
                .as_ref()
                .is_some_and(|dialog| dialog.is_busy())
    }
}

/// Reaching a phase of a mailbox load that is over too quickly to catch.
#[cfg(test)]
impl App {
    /// Put the visible account's running load into `phase`.
    ///
    /// The overlay's whole content is the phase it shows, but only the first
    /// and the last -- connecting, and failing -- stay up long enough for a
    /// test to catch: the moment the header count in `Receiving` is known, the
    /// same poll delivers the messages that replace the overlay.  Setting the
    /// phase is the only way to see what it looks like on the way past.
    pub(crate) fn set_load_phase(&mut self, phase: LoadPhase) {
        if let Some(loading) = self.current_account_mut().loading.as_mut() {
            loading.phase = phase;
        }
    }
}

impl Deref for App {
    type Target = AccountState;

    fn deref(&self) -> &Self::Target {
        &self.accounts[self.active_account]
    }
}

impl DerefMut for App {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.accounts[self.active_account]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        ActionStatus, BackendEvent, MailBackend, MailboxSnapshot, OutgoingMessage,
    };
    use crate::model::{Action, ActionType, Message, MessageId, MessageStatus};
    use std::sync::{Mutex, mpsc};
    use time::OffsetDateTime;

    // -- Test infrastructure --------------------------------------------------

    struct NoopBackend;

    impl MailBackend for NoopBackend {
        fn load_mailbox(
            &self,
            _mailbox: MailboxKind,
        ) -> anyhow::Result<(MailboxSnapshot, mpsc::Receiver<BackendEvent>)> {
            let (_tx, rx) = mpsc::channel();
            Ok((
                MailboxSnapshot {
                    total: 0,
                    messages: vec![],
                },
                rx,
            ))
        }

        fn load_message(&self, _message_id: MessageId) -> anyhow::Result<MessageContent> {
            Ok(MessageContent::default())
        }

        fn apply_actions(
            &self,
            actions: Vec<Action>,
        ) -> anyhow::Result<mpsc::Receiver<ActionStatus>> {
            let (tx, rx) = mpsc::channel();
            for action in actions {
                let _ = tx.send(ActionStatus {
                    action,
                    result: Ok(()),
                });
            }
            Ok(rx)
        }

        fn send_message(&self, _message: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }

        fn save_draft(&self, _message: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn make_message(id: MessageId, status: MessageStatus) -> Message {
        Message {
            id,
            sent: OffsetDateTime::UNIX_EPOCH,
            sender: format!("sender-{id}"),
            recipients: vec![],
            subject: format!("Subject {id}"),
            size: 100,
            starred: false,
            important: false,
            answered: false,
            forwarded: false,
            status,
            labels: vec![],
            uid: id as u32,
            seq: id as u32,
            has_attachments: false,
        }
    }

    /// Backend that captures submitted actions without completing them.
    /// The test drives completion by draining `pending_senders`.
    struct DeferredBackend {
        /// Each `apply_actions` call pushes `(actions, sender)` here.
        pending: Mutex<Vec<(Vec<Action>, mpsc::Sender<ActionStatus>)>>,
    }

    impl DeferredBackend {
        fn new() -> Self {
            Self {
                pending: Mutex::new(Vec::new()),
            }
        }

        /// Complete all pending batches successfully.
        fn complete_all(&self) {
            let batches: Vec<_> = {
                let mut guard = self.pending.lock().unwrap();
                std::mem::take(&mut *guard)
            };
            for (actions, tx) in batches {
                for action in actions {
                    let _ = tx.send(ActionStatus {
                        action,
                        result: Ok(()),
                    });
                }
            }
        }

        /// Complete all pending batches, failing actions whose message ID is
        /// in `fail_ids`.
        fn complete_with_failures(&self, fail_ids: &std::collections::HashSet<MessageId>) {
            let batches: Vec<_> = {
                let mut guard = self.pending.lock().unwrap();
                std::mem::take(&mut *guard)
            };
            for (actions, tx) in batches {
                for action in actions {
                    let result = if fail_ids.contains(&action.message_id) {
                        Err("simulated failure".to_string())
                    } else {
                        Ok(())
                    };
                    let _ = tx.send(ActionStatus { action, result });
                }
            }
        }
    }

    impl MailBackend for DeferredBackend {
        fn load_mailbox(
            &self,
            _mailbox: MailboxKind,
        ) -> anyhow::Result<(MailboxSnapshot, mpsc::Receiver<BackendEvent>)> {
            let (_tx, rx) = mpsc::channel();
            Ok((
                MailboxSnapshot {
                    total: 0,
                    messages: vec![],
                },
                rx,
            ))
        }

        fn load_message(&self, _message_id: MessageId) -> anyhow::Result<MessageContent> {
            Ok(MessageContent::default())
        }

        fn apply_actions(
            &self,
            actions: Vec<Action>,
        ) -> anyhow::Result<mpsc::Receiver<ActionStatus>> {
            let (tx, rx) = mpsc::channel();
            self.pending.lock().unwrap().push((actions, tx));
            Ok(rx)
        }

        fn send_message(&self, _message: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }

        fn save_draft(&self, _message: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_app_with_backend(messages: Vec<Message>, backend: Arc<dyn MailBackend>) -> App {
        let (_tx, events) = mpsc::channel();
        let selected = if messages.is_empty() { None } else { Some(0) };
        let account = AccountState {
            name: "test".to_string(),
            backend,
            mailbox: MailboxState {
                messages,
                selected,
                events,
                event_count: 0,
                status_line: None,
                scroll_top: 0,
            },
            message_view: None,
            commit_batches: VecDeque::new(),
            commit_progress: None,
            mailbox_loader: None,
            mailbox_load_progress: None,
            loading: None,
            loaded: true,
            connected: true,
            message_loader: None,
            scheduled_actions: vec![],
            current_mailbox: MailboxKind::Inbox,
            search: None,
        };

        App {
            accounts: vec![account],
            active_account: 0,
            compose: None,
            should_quit: false,

            pending_shortcut: None,
            pending_navigation: None,
            save_attachment: None,
            outgoing: None,
            needs_full_redraw: false,
        }
    }

    fn test_app(messages: Vec<Message>) -> App {
        test_app_with_backend(messages, Arc::new(NoopBackend))
    }

    // -- schedule_action basics -----------------------------------------------

    #[test]
    fn schedule_adds_new_action() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        let outcome = app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        assert_eq!(outcome, ScheduleOutcome::Added);
        assert_eq!(app.scheduled_actions.len(), 1);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Delete);
    }

    #[test]
    fn schedule_toggles_off_same_action() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        let outcome = app.schedule_action(ActionType::Delete, 1, MessageStatus::Deleted);
        assert_eq!(outcome, ScheduleOutcome::AlreadyScheduled);
        assert_eq!(app.scheduled_actions.len(), 1);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Delete);
    }

    // -- Move-group replacement -----------------------------------------------

    #[test]
    fn schedule_replaces_delete_with_archive() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        let outcome = app.schedule_action(ActionType::Archive, 1, MessageStatus::Deleted);
        assert_eq!(outcome, ScheduleOutcome::Replaced);
        assert_eq!(app.scheduled_actions.len(), 1);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Archive);
    }

    #[test]
    fn schedule_replaces_archive_with_delete() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        app.schedule_action(ActionType::Archive, 1, MessageStatus::Read);
        let outcome = app.schedule_action(ActionType::Delete, 1, MessageStatus::Archived);
        assert_eq!(outcome, ScheduleOutcome::Replaced);
        assert_eq!(app.scheduled_actions.len(), 1);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Delete);
    }

    #[test]
    fn schedule_replaces_with_spam() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        let outcome = app.schedule_action(ActionType::MoveToSpam, 1, MessageStatus::Deleted);
        assert_eq!(outcome, ScheduleOutcome::Replaced);
        assert_eq!(app.scheduled_actions.len(), 1);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::MoveToSpam);
    }

    #[test]
    fn schedule_replaces_move_to_inbox_variants() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Deleted)]);
        app.schedule_action(ActionType::MoveToInboxRead, 1, MessageStatus::Deleted);
        let outcome = app.schedule_action(
            ActionType::MoveToInboxUnread,
            1,
            MessageStatus::PendingInbox,
        );
        assert_eq!(outcome, ScheduleOutcome::Replaced);
        assert_eq!(app.scheduled_actions.len(), 1);
        assert_eq!(
            app.scheduled_actions[0].action_type,
            ActionType::MoveToInboxUnread
        );
    }

    #[test]
    fn replace_preserves_original_status() {
        let mut app = test_app(vec![make_message(1, MessageStatus::New)]);
        app.schedule_action(ActionType::Delete, 1, MessageStatus::New);
        app.schedule_action(ActionType::Archive, 1, MessageStatus::Deleted);
        // The original_status should still be New (from the first scheduling)
        assert_eq!(
            app.scheduled_actions[0].original_status,
            Some(MessageStatus::New)
        );
    }

    // -- Multi-message independence -------------------------------------------

    #[test]
    fn different_messages_are_independent() {
        let mut app = test_app(vec![
            make_message(1, MessageStatus::Read),
            make_message(2, MessageStatus::Read),
        ]);
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        app.schedule_action(ActionType::Delete, 2, MessageStatus::Read);
        assert_eq!(app.scheduled_actions.len(), 2);
    }

    #[test]
    fn replace_only_affects_target_message() {
        let mut app = test_app(vec![
            make_message(1, MessageStatus::Read),
            make_message(2, MessageStatus::Read),
        ]);
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        app.schedule_action(ActionType::Delete, 2, MessageStatus::Read);
        // Replace msg 1 with archive
        app.schedule_action(ActionType::Archive, 1, MessageStatus::Deleted);
        assert_eq!(app.scheduled_actions.len(), 2);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Archive);
        assert_eq!(app.scheduled_actions[0].message_id, 1);
        assert_eq!(app.scheduled_actions[1].action_type, ActionType::Delete);
        assert_eq!(app.scheduled_actions[1].message_id, 2);
    }

    #[test]
    fn same_action_again_is_idempotent_per_message() {
        let mut app = test_app(vec![
            make_message(1, MessageStatus::Read),
            make_message(2, MessageStatus::Read),
        ]);
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        app.schedule_action(ActionType::Delete, 2, MessageStatus::Read);
        // Same action on msg 1 again — no-op
        let outcome = app.schedule_action(ActionType::Delete, 1, MessageStatus::Deleted);
        assert_eq!(outcome, ScheduleOutcome::AlreadyScheduled);
        assert_eq!(app.scheduled_actions.len(), 2);
        assert_eq!(app.scheduled_actions[0].message_id, 1);
        assert_eq!(app.scheduled_actions[1].message_id, 2);
    }

    // -- Complex sequences ----------------------------------------------------

    #[test]
    fn same_action_stays_then_different_replaces() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        // Delete
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        // Same action — no-op
        let outcome = app.schedule_action(ActionType::Delete, 1, MessageStatus::Deleted);
        assert_eq!(outcome, ScheduleOutcome::AlreadyScheduled);
        assert_eq!(app.scheduled_actions.len(), 1);
        // Different action replaces
        let outcome = app.schedule_action(ActionType::Archive, 1, MessageStatus::Deleted);
        assert_eq!(outcome, ScheduleOutcome::Replaced);
        assert_eq!(app.scheduled_actions.len(), 1);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Archive);
    }

    #[test]
    fn replace_then_same_action_is_idempotent() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        // Delete
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        // Replace with archive
        app.schedule_action(ActionType::Archive, 1, MessageStatus::Deleted);
        assert_eq!(app.scheduled_actions.len(), 1);
        // Same action again — stays scheduled
        let outcome = app.schedule_action(ActionType::Archive, 1, MessageStatus::Archived);
        assert_eq!(outcome, ScheduleOutcome::AlreadyScheduled);
        assert_eq!(app.scheduled_actions.len(), 1);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Archive);
    }

    #[test]
    fn multiple_messages_interleaved_actions() {
        let mut app = test_app(vec![
            make_message(1, MessageStatus::Read),
            make_message(2, MessageStatus::New),
            make_message(3, MessageStatus::Read),
        ]);

        // Delete msg 1
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        // Archive msg 2
        app.schedule_action(ActionType::Archive, 2, MessageStatus::New);
        // Spam msg 3
        app.schedule_action(ActionType::MoveToSpam, 3, MessageStatus::Read);
        assert_eq!(app.scheduled_actions.len(), 3);

        // Replace msg 1 delete with archive
        app.schedule_action(ActionType::Archive, 1, MessageStatus::Deleted);
        assert_eq!(app.scheduled_actions.len(), 3);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Archive);

        // Same action on msg 2 — no-op
        let outcome = app.schedule_action(ActionType::Archive, 2, MessageStatus::Archived);
        assert_eq!(outcome, ScheduleOutcome::AlreadyScheduled);
        assert_eq!(app.scheduled_actions.len(), 3);

        // Verify final state: msg 1 archive, msg 2 archive, msg 3 spam
        assert_eq!(app.scheduled_actions[0].message_id, 1);
        assert_eq!(app.scheduled_actions[0].action_type, ActionType::Archive);
        assert_eq!(app.scheduled_actions[1].message_id, 2);
        assert_eq!(app.scheduled_actions[1].action_type, ActionType::Archive);
        assert_eq!(app.scheduled_actions[2].message_id, 3);
        assert_eq!(app.scheduled_actions[2].action_type, ActionType::MoveToSpam);
    }

    // -- Stress / invariant checking ------------------------------------------

    #[test]
    fn random_action_sequences_preserve_invariants() {
        let action_types = [
            ActionType::Delete,
            ActionType::Archive,
            ActionType::MoveToSpam,
            ActionType::MoveToInboxRead,
            ActionType::MoveToInboxUnread,
        ];
        let message_ids: Vec<MessageId> = (1..=5).collect();
        let messages: Vec<Message> = message_ids
            .iter()
            .map(|&id| make_message(id, MessageStatus::Read))
            .collect();
        let mut app = test_app(messages);

        // Apply 100 random-ish actions using a simple deterministic sequence
        for i in 0..100 {
            let msg_id = message_ids[i % message_ids.len()];
            let action_type = action_types[(i * 7 + i / 3) % action_types.len()];
            app.schedule_action(action_type, msg_id, MessageStatus::Read);

            // Invariant: at most one action per message
            let mut seen_ids: Vec<MessageId> =
                app.scheduled_actions.iter().map(|a| a.message_id).collect();
            seen_ids.sort();
            seen_ids.dedup();
            assert_eq!(
                seen_ids.len(),
                app.scheduled_actions.len(),
                "duplicate message_id in scheduled_actions at iteration {i}"
            );
        }
    }

    #[test]
    fn repeated_same_action_stays_scheduled() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        app.schedule_action(ActionType::Delete, 1, MessageStatus::Read);
        assert_eq!(app.scheduled_actions.len(), 1);
        // Pressing delete 50 more times keeps it scheduled
        for _ in 0..50 {
            let outcome = app.schedule_action(ActionType::Delete, 1, MessageStatus::Deleted);
            assert_eq!(outcome, ScheduleOutcome::AlreadyScheduled);
            assert_eq!(app.scheduled_actions.len(), 1);
        }
    }

    // -- Concurrent staging while committing ----------------------------------

    #[test]
    fn staging_while_committing_does_not_remove_new_staged_messages() {
        let backend = Arc::new(DeferredBackend::new());
        let messages: Vec<Message> = (1..=1000)
            .map(|id| make_message(id, MessageStatus::Read))
            .collect();
        let mut app = test_app_with_backend(messages, Arc::clone(&backend) as Arc<dyn MailBackend>);

        assert_eq!(app.mailbox.messages.len(), 1000);

        // Stage 100 deletions (messages 1..=100).
        for id in 1..=100u64 {
            app.schedule_action(ActionType::Delete, id, MessageStatus::Read);
            // Mark the message status locally as the real schedule_delete would.
            if let Some(msg) = app.mailbox.messages.iter_mut().find(|m| m.id == id) {
                msg.status = MessageStatus::Deleted;
            }
        }
        assert_eq!(app.scheduled_actions.len(), 100);

        // Commit ($) — sends the 100 deletions to the backend.
        app.commit_actions().unwrap();
        assert!(
            app.scheduled_actions.is_empty(),
            "scheduled_actions should be drained after commit"
        );
        assert_eq!(app.commit_batches.len(), 1, "one batch in flight");

        // Messages are optimistically removed at commit time.
        assert_eq!(
            app.mailbox.messages.len(),
            900,
            "deleted messages removed immediately on commit"
        );

        // WHILE the delete batch is in flight, stage 100 archival actions
        // (messages 101..=200).
        for id in 101..=200u64 {
            app.schedule_action(ActionType::Archive, id, MessageStatus::Read);
            if let Some(msg) = app.mailbox.messages.iter_mut().find(|m| m.id == id) {
                msg.status = MessageStatus::Archived;
            }
        }
        assert_eq!(
            app.scheduled_actions.len(),
            100,
            "100 archive actions staged"
        );

        // Now complete the delete batch in the backend.
        backend.complete_all();

        // Poll so the app sees the completions and finalizes.
        app.poll_commit_updates();

        // Still 900 — no further removal needed since messages were already gone.
        assert_eq!(
            app.mailbox.messages.len(),
            900,
            "900 messages remain after deletes finalized"
        );

        // The 100 archive actions should still be staged (not committed).
        assert_eq!(
            app.scheduled_actions.len(),
            100,
            "archive actions still in staging"
        );

        // None of the archived messages should have been removed.
        let archived_count = app
            .mailbox
            .messages
            .iter()
            .filter(|m| m.status == MessageStatus::Archived)
            .count();
        assert_eq!(
            archived_count, 100,
            "all 100 archived messages still present"
        );

        // Verify the deleted messages are gone.
        let deleted_present = app
            .mailbox
            .messages
            .iter()
            .any(|m| m.id >= 1 && m.id <= 100);
        assert!(
            !deleted_present,
            "deleted messages should have been removed"
        );
    }

    #[test]
    fn failed_actions_reinsert_messages_with_original_status() {
        let backend = Arc::new(DeferredBackend::new());
        let messages: Vec<Message> = (1..=10)
            .map(|id| make_message(id, MessageStatus::Read))
            .collect();
        let mut app = test_app_with_backend(messages, Arc::clone(&backend) as Arc<dyn MailBackend>);

        assert_eq!(app.mailbox.messages.len(), 10);

        // Stage deletions for messages 1..=5.
        for id in 1..=5u64 {
            app.schedule_action(ActionType::Delete, id, MessageStatus::Read);
            if let Some(msg) = app.mailbox.messages.iter_mut().find(|m| m.id == id) {
                msg.status = MessageStatus::Deleted;
            }
        }
        assert_eq!(app.scheduled_actions.len(), 5);

        // Commit — messages should be optimistically removed.
        app.commit_actions().unwrap();
        assert_eq!(
            app.mailbox.messages.len(),
            5,
            "5 messages removed on commit"
        );
        assert!(app.scheduled_actions.is_empty());

        // Complete the batch with failures for messages 2 and 4.
        let fail_ids: std::collections::HashSet<MessageId> = [2, 4].iter().copied().collect();
        backend.complete_with_failures(&fail_ids);

        // Poll and finalize.
        app.poll_commit_updates();

        // Messages 2 and 4 should be re-inserted with original status (Read).
        assert_eq!(
            app.mailbox.messages.len(),
            7,
            "5 remaining + 2 re-inserted = 7"
        );

        for id in [2, 4] {
            let msg = app
                .mailbox
                .messages
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("message {id} should have been re-inserted"));
            assert_eq!(
                msg.status,
                MessageStatus::Read,
                "message {id} should have original status restored"
            );
        }

        // Successfully deleted messages (1, 3, 5) should still be gone.
        for id in [1, 3, 5] {
            assert!(
                !app.mailbox.messages.iter().any(|m| m.id == id),
                "message {id} should remain removed (succeeded)"
            );
        }

        // Failed actions should be re-queued in scheduled_actions.
        assert_eq!(app.scheduled_actions.len(), 2, "2 failed actions re-queued");
        let requeued_ids: std::collections::HashSet<MessageId> =
            app.scheduled_actions.iter().map(|a| a.message_id).collect();
        assert!(requeued_ids.contains(&2));
        assert!(requeued_ids.contains(&4));
    }

    // -- Attachment paths -----------------------------------------------------

    #[test]
    fn expand_user_path_unescapes_before_expanding_tilde() {
        let Some(home) = env::var_os("HOME") else {
            return;
        };
        // Escapes have to be stripped first, or the file name keeps the
        // backslash and the read fails.
        assert_eq!(
            expand_user_path("~/My\\ File.pdf"),
            PathBuf::from(home).join("My File.pdf")
        );
    }

    #[test]
    fn expand_user_path_decodes_file_urls() {
        assert_eq!(
            expand_user_path("file:///tmp/My%20File.pdf"),
            PathBuf::from("/tmp/My File.pdf")
        );
        assert_eq!(
            expand_user_path("file://localhost/tmp/report.pdf"),
            PathBuf::from("/tmp/report.pdf")
        );
    }

    #[test]
    fn expand_user_path_leaves_plain_paths_percent_encoded() {
        // Only `file://` URLs are percent-encoded; a real file may be named
        // "100%20done".
        assert_eq!(
            expand_user_path("/tmp/100%20done.txt"),
            PathBuf::from("/tmp/100%20done.txt")
        );
    }

    #[test]
    fn expand_user_path_strips_quotes() {
        assert_eq!(
            expand_user_path("\"/tmp/my file.txt\""),
            PathBuf::from("/tmp/my file.txt")
        );
        assert_eq!(
            expand_user_path("'/tmp/my file.txt'"),
            PathBuf::from("/tmp/my file.txt")
        );
    }

    #[test]
    fn parse_paste_paths_keeps_single_path_raw() {
        // A single token stays escaped so expand_user_path is the only place
        // that unescapes -- otherwise the value is unescaped twice.
        assert_eq!(
            parse_paste_paths("/tmp/a\\ b.txt"),
            vec!["/tmp/a\\ b.txt".to_string()]
        );
        assert_eq!(
            expand_user_path(&parse_paste_paths("/tmp/a\\ b.txt")[0]),
            PathBuf::from("/tmp/a b.txt")
        );
    }

    #[test]
    fn unescaped_spaces_in_a_pasted_path_are_not_a_drop() {
        // Terminals escape or quote spaces when they deliver a drop, so an
        // unescaped space means this is text: it splits into tokens that are
        // not all files, and therefore gets typed instead of attached.
        // (A path typed into the attach prompt never comes through here -- the
        // prompt hands its raw value straight to attach_file_from_path.)
        assert!(parse_paste_paths("/tmp/my file.txt").len() > 1);
        assert_eq!(dropped_file_paths("/tmp/my file.txt"), None);
    }

    #[test]
    fn parse_paste_paths_splits_multi_file_drops() {
        assert_eq!(
            parse_paste_paths("/tmp/a.txt\n/tmp/b.txt"),
            vec!["/tmp/a.txt".to_string(), "/tmp/b.txt".to_string()]
        );
        assert_eq!(
            parse_paste_paths("'/tmp/a b.txt' '/tmp/c.txt'"),
            vec!["/tmp/a b.txt".to_string(), "/tmp/c.txt".to_string()]
        );
    }

    #[test]
    fn dropped_file_paths_accepts_existing_absolute_paths() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let path = file.path().to_string_lossy().into_owned();
        assert_eq!(dropped_file_paths(&path), Some(vec![path.clone()]));
    }

    #[test]
    fn dropped_file_paths_rejects_ordinary_text() {
        assert_eq!(dropped_file_paths("Hello there, please review"), None);
        assert_eq!(dropped_file_paths(""), None);
        // Relative paths are rejected even when they exist, so pasting a word
        // that happens to name a file in the cwd still types as text.
        assert_eq!(dropped_file_paths("Cargo.toml"), None);
    }

    #[test]
    fn dropped_file_paths_requires_every_candidate_to_exist() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let existing = file.path().to_string_lossy().into_owned();
        let text = format!("{existing}\n/tmp/definitely-not-here-9d3f1a.txt");
        assert_eq!(dropped_file_paths(&text), None);
    }

    #[test]
    fn fallback_attachment_name_uses_mime_extension() {
        assert_eq!(fallback_attachment_name(0, "image/png"), "attachment-1.png");
        assert_eq!(
            fallback_attachment_name(2, "application/pdf"),
            "attachment-3.pdf"
        );
        assert_eq!(
            fallback_attachment_name(0, "image/jpeg; name=x"),
            "attachment-1.jpg"
        );
        assert_eq!(
            fallback_attachment_name(0, "application/octet-stream"),
            "attachment-1.bin"
        );
        assert_eq!(
            fallback_attachment_name(0, "text/plain"),
            "attachment-1.txt"
        );
    }

    #[test]
    fn guess_mime_type_covers_common_extensions() {
        let guess = |name: &str| guess_mime_type(std::path::Path::new(name));

        assert_eq!(guess("/tmp/report.pdf"), "application/pdf");
        assert_eq!(guess("/tmp/photo.HEIC"), "image/heic");
        assert_eq!(guess("/tmp/clip.mkv"), "video/x-matroska");
        assert_eq!(guess("/tmp/archive.7z"), "application/x-7z-compressed");
        assert_eq!(guess("/tmp/song.flac"), "audio/flac");
        assert_eq!(guess("/tmp/no-extension"), "application/octet-stream");
        assert_eq!(guess("/tmp/unknown.qqq"), "application/octet-stream");
    }

    #[test]
    fn guess_mime_type_marks_text_as_utf8() {
        let guess = |name: &str| guess_mime_type(std::path::Path::new(name));

        // Without the charset a recipient may decode these as Latin-1.
        assert_eq!(guess("/tmp/notes.txt"), "text/plain; charset=utf-8");
        assert_eq!(guess("/tmp/main.rs"), "text/x-rust; charset=utf-8");
        assert_eq!(guess("/tmp/config.toml"), "text/x-toml; charset=utf-8");
        assert_eq!(guess("/tmp/data.csv"), "text/csv; charset=utf-8");
        // Binary types are left alone.
        assert_eq!(guess("/tmp/logo.png"), "image/png");
    }

    #[test]
    fn extension_for_mime_type_prefers_the_explicit_table() {
        // Ambiguous types come from the table.
        assert_eq!(extension_for_mime_type("text/plain"), "txt");
        assert_eq!(extension_for_mime_type("text/plain; charset=utf-8"), "txt");
        assert_eq!(extension_for_mime_type("image/jpeg"), "jpg");
        // Prefixed subtypes have no usable subtype, so they are listed too.
        assert_eq!(extension_for_mime_type("video/x-matroska"), "mkv");
        assert_eq!(extension_for_mime_type("application/x-7z-compressed"), "7z");
        // Subtypes that already read like an extension need no entry.
        assert_eq!(extension_for_mime_type("image/heic"), "heic");
        assert_eq!(extension_for_mime_type("audio/flac"), "flac");
        // Unknown types stay generic rather than guessing.
        assert_eq!(extension_for_mime_type("application/x-made-up"), "bin");
        assert_eq!(extension_for_mime_type("application/octet-stream"), "bin");
    }

    #[test]
    fn sanitize_filename_cannot_escape_the_target_folder() {
        // A MIME filename is attacker-controlled, so every separator has to
        // become part of the name instead of a directory step.
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_filename("/etc/passwd"), "etc_passwd");
        assert_eq!(
            sanitize_filename("nested/dir/report.pdf"),
            "nested_dir_report.pdf"
        );
        assert_eq!(sanitize_filename("C:\\Windows\\hosts"), "C:_Windows_hosts");
        assert_eq!(sanitize_filename("evil\0.txt"), "evil_.txt");
    }

    #[test]
    fn sanitize_filename_rejects_names_that_carry_no_content() {
        // The caller falls back to `attachment-N.ext` on an empty result, so
        // "." and ".." must not survive as usable names.
        assert_eq!(sanitize_filename(".."), "");
        assert_eq!(sanitize_filename("."), "");
        assert_eq!(sanitize_filename("   "), "");
        assert_eq!(sanitize_filename("/"), "");
        assert_eq!(sanitize_filename(""), "");
        // Leading dots are only a problem on their own.
        assert_eq!(sanitize_filename("..invoice.pdf"), "..invoice.pdf");
        assert_eq!(sanitize_filename("  report.pdf  "), "report.pdf");
    }

    #[test]
    fn unique_path_in_never_overwrites_an_existing_file() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let folder = dir.path();

        // Nothing there yet: the name is used as is.
        assert_eq!(
            unique_path_in(folder, "report.pdf"),
            folder.join("report.pdf")
        );

        // The counter goes before the extension, not after it.
        fs::write(folder.join("report.pdf"), b"first").expect("write");
        assert_eq!(
            unique_path_in(folder, "report.pdf"),
            folder.join("report (1).pdf")
        );

        // And it keeps counting rather than reusing " (1)".
        fs::write(folder.join("report (1).pdf"), b"second").expect("write");
        assert_eq!(
            unique_path_in(folder, "report.pdf"),
            folder.join("report (2).pdf")
        );
    }

    #[test]
    fn unique_path_in_handles_names_without_an_extension() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let folder = dir.path();

        fs::write(folder.join("notes"), b"first").expect("write");
        assert_eq!(unique_path_in(folder, "notes"), folder.join("notes (1)"));

        // A dotfile has no stem, so the leading dot is not an extension either.
        fs::write(folder.join(".gitignore"), b"first").expect("write");
        assert_eq!(
            unique_path_in(folder, ".gitignore"),
            folder.join(".gitignore (1)")
        );
    }

    // -- Compose focus order --------------------------------------------------

    fn outgoing_attachment(filename: &str) -> OutgoingAttachment {
        OutgoingAttachment {
            filename: filename.to_string(),
            mime_type: "application/pdf".to_string(),
            data: b"%PDF-1.7".to_vec(),
        }
    }

    #[test]
    fn focus_sequence_skips_the_attachment_list_when_there_is_none() {
        let mut compose = ComposeState::new();
        assert_eq!(
            compose.focus_sequence(),
            vec![
                ComposeFocus::Field(ComposeField::To),
                ComposeFocus::Field(ComposeField::Cc),
                ComposeFocus::Field(ComposeField::Bcc),
                ComposeFocus::Field(ComposeField::Subject),
                ComposeFocus::Body,
                ComposeFocus::Button(ComposeButton::Attach),
                ComposeFocus::Button(ComposeButton::Cancel),
                ComposeFocus::Button(ComposeButton::Edit),
                ComposeFocus::Button(ComposeButton::Draft),
                ComposeFocus::Button(ComposeButton::Send),
            ]
        );

        // Tab from the last header lands on the body, not on an empty list.
        compose.set_focus(ComposeFocus::Field(ComposeField::Subject));
        compose.focus_next();
        assert_eq!(compose.focus(), ComposeFocus::Body);

        // And Shift+Tab from the first field wraps to the last button.
        compose.set_focus(ComposeFocus::Field(ComposeField::To));
        compose.focus_prev();
        assert_eq!(compose.focus(), ComposeFocus::Button(ComposeButton::Send));
    }

    #[test]
    fn focus_sequence_includes_the_attachment_list_once_a_file_is_attached() {
        let mut compose = ComposeState::new();
        compose.add_attachment(outgoing_attachment("invoice.pdf"));

        assert_eq!(
            compose.focus_sequence(),
            vec![
                ComposeFocus::Field(ComposeField::To),
                ComposeFocus::Field(ComposeField::Cc),
                ComposeFocus::Field(ComposeField::Bcc),
                ComposeFocus::Field(ComposeField::Subject),
                ComposeFocus::Attachments,
                ComposeFocus::Body,
                ComposeFocus::Button(ComposeButton::Attach),
                ComposeFocus::Button(ComposeButton::Cancel),
                ComposeFocus::Button(ComposeButton::Edit),
                ComposeFocus::Button(ComposeButton::Draft),
                ComposeFocus::Button(ComposeButton::Send),
            ]
        );

        // The list sits between the headers and the body in both directions.
        compose.set_focus(ComposeFocus::Field(ComposeField::Subject));
        compose.focus_next();
        assert_eq!(compose.focus(), ComposeFocus::Attachments);
        compose.focus_next();
        assert_eq!(compose.focus(), ComposeFocus::Body);
        compose.focus_prev();
        assert_eq!(compose.focus(), ComposeFocus::Attachments);
    }

    // -- Attachment indicator -------------------------------------------------

    #[test]
    fn the_message_list_marks_messages_that_carry_attachments() {
        let mut message = make_message(1, MessageStatus::Read);
        let app = test_app(vec![message.clone()]);

        assert_eq!(
            app.formatted_message_row(&message, OffsetDateTime::UNIX_EPOCH)
                .flags,
            "    ",
            "no attachments, no marker"
        );

        message.has_attachments = true;
        assert_eq!(
            app.formatted_message_row(&message, OffsetDateTime::UNIX_EPOCH)
                .flags,
            "   @"
        );

        // The marker has its own column, so it survives the other flags.
        message.status = MessageStatus::New;
        message.starred = true;
        message.answered = true;
        assert_eq!(
            app.formatted_message_row(&message, OffsetDateTime::UNIX_EPOCH)
                .flags,
            "N*↩@"
        );
    }

    #[test]
    fn opening_a_message_corrects_the_attachment_indicator() {
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app(vec![message]);

        // The list said "no attachments" -- the parsed message says otherwise.
        app.show_loaded_message(
            1,
            MessageContent {
                attachments: vec![attachment(
                    Some("invoice.pdf"),
                    "application/pdf",
                    Some(b"%PDF-1.7"),
                    None,
                )],
                ..Default::default()
            },
        );

        assert!(app.mailbox.messages[0].has_attachments);
        assert!(
            app.message_view
                .as_ref()
                .is_some_and(|view| view.message.has_attachments),
            "the open viewer has to agree with the list"
        );

        // And the correction works the other way round, too.
        app.mailbox.messages[0].has_attachments = true;
        app.show_loaded_message(1, MessageContent::default());
        assert!(!app.mailbox.messages[0].has_attachments);
    }

    #[test]
    fn forwarding_a_message_corrects_the_attachment_indicator_too() {
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app(vec![message.clone()]);

        // No viewer is involved here: the body was fetched to seed compose.
        // The list still has to learn what the fetch found.
        app.deliver_loaded_message(
            message,
            MessageLoadPurpose::Forward,
            None,
            LoadedMessage {
                content: MessageContent {
                    attachments: vec![attachment(
                        Some("invoice.pdf"),
                        "application/pdf",
                        Some(b"%PDF-1.7"),
                        None,
                    )],
                    ..Default::default()
                },
                attachments: Vec::new(),
                unavailable: 0,
            },
        );

        assert!(app.compose.is_some(), "forwarding opens compose");
        assert!(app.mailbox.messages[0].has_attachments);
    }

    /// Draw the whole UI into a test terminal and return it line by line.
    fn rendered_screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| crate::ui::render(frame, app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_attachment_marker_reaches_the_screen() {
        let mut with = make_message(1, MessageStatus::Read);
        with.has_attachments = true;
        with.subject = "Has an attachment".to_string();
        let mut without = make_message(2, MessageStatus::Read);
        without.subject = "Plain text only".to_string();

        let mut app = test_app(vec![with, without]);
        let screen = rendered_screen(&mut app, 100, 12);

        let marked = screen
            .iter()
            .find(|line| line.contains("Has an attachment"))
            .expect("the message list shows the first message");
        assert!(
            marked.contains('@'),
            "the attachment marker has to be drawn, got {marked:?}"
        );

        let unmarked = screen
            .iter()
            .find(|line| line.contains("Plain text only"))
            .expect("the message list shows the second message");
        assert!(
            !unmarked.contains('@'),
            "a message without attachments stays unmarked, got {unmarked:?}"
        );
    }

    #[test]
    fn text_field_insert_str_flattens_pasted_text() {
        let mut field = TextFieldState::default();
        // Enclosing line breaks (the terminal's) go away; the inner one becomes
        // a separator rather than gluing the addresses together.
        assert!(field.insert_str("first@example.com\nsecond@example.com\n"));
        assert_eq!(field.value, "first@example.com second@example.com");
        assert_eq!(field.cursor, text_len(&field.value));

        // Inserts at the cursor rather than appending, and keeps a deliberate
        // trailing space.
        field.move_home();
        field.insert_str("zero@example.com, ");
        assert_eq!(
            field.value,
            "zero@example.com, first@example.com second@example.com"
        );
        assert_eq!(field.cursor, text_len("zero@example.com, "));
    }

    #[test]
    fn draft_status_line_reports_lost_attachments() {
        assert_eq!(draft_status_line("Editing draft.", 0), "Editing draft.");
        assert!(draft_status_line("Editing draft.", 1).contains("1 attachment"));
        assert!(draft_status_line("Editing draft.", 3).contains("3 attachments"));
    }

    // -- Paste routing --------------------------------------------------------

    #[test]
    fn pasted_text_reaches_the_focused_compose_field() {
        let mut app = test_app(vec![]);
        app.open_compose();

        app.handle_paste_text("someone@example.com").expect("paste");

        let compose = app.compose.as_ref().expect("compose open");
        assert_eq!(
            compose.field_data(ComposeField::To).0,
            "someone@example.com",
            "bracketed paste is the only route into the field; dropping it loses the paste"
        );
        assert!(compose.attachments().is_empty());
    }

    #[test]
    fn pasted_text_reaches_the_search_input() {
        let mut app = test_app(vec![make_message(1, MessageStatus::Read)]);
        app.open_search();

        app.handle_paste_text("invoice").expect("paste");

        assert_eq!(
            app.search.as_ref().expect("search open").input.value,
            "invoice"
        );
    }

    #[test]
    fn pasted_text_reaches_the_save_attachment_folder_field() {
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app(vec![message.clone()]);
        app.save_attachment = Some(SaveAttachmentDialog::new(String::new()));
        if let Some(dialog) = app.save_attachment.as_mut() {
            dialog.focus = SaveAttachmentFocus::Folder;
        }

        app.handle_paste_text("/tmp/target-dir").expect("paste");

        let dialog = app.save_attachment.as_ref().expect("dialog open");
        assert_eq!(dialog.folder_data().0, "/tmp/target-dir");
    }

    #[test]
    fn dropping_a_file_while_composing_attaches_it() {
        use std::io::Write as _;

        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(b"payload").expect("write");
        let path = file.path().to_string_lossy().into_owned();

        let mut app = test_app(vec![]);
        app.open_compose();

        app.handle_paste_text(&path).expect("paste");

        let compose = app.compose.as_ref().expect("compose open");
        assert_eq!(compose.attachments().len(), 1);
        assert_eq!(compose.attachments()[0].data, b"payload");
        assert_eq!(
            compose.field_data(ComposeField::To).0,
            "",
            "a drop must not also type the path into the focused field"
        );
    }

    // -- Asking before attaching something big --------------------------------

    /// A file over the threshold, made sparse: the guard reads the length from
    /// the filesystem, so there is no reason to write ten megabytes to test it.
    fn oversized_file() -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        file.as_file()
            .set_len(LARGE_ATTACHMENT_BYTES + 1)
            .expect("grow file");
        file
    }

    fn drop_file(app: &mut App, file: &tempfile::NamedTempFile) {
        let path = file.path().to_string_lossy().into_owned();
        app.handle_paste_text(&path).expect("drop");
    }

    #[test]
    fn a_large_file_is_not_read_until_the_user_approves_it() {
        let file = oversized_file();
        let mut app = test_app(vec![]);
        app.open_compose();

        drop_file(&mut app, &file);

        let compose = app.compose.as_ref().expect("compose open");
        assert!(
            compose.attachments().is_empty(),
            "the file must not be in the message before the question is answered"
        );
        let (name, size, projected) = compose
            .large_attachment_question()
            .expect("the user has to be asked");
        assert_eq!(name, file.path().file_name().unwrap().to_string_lossy());
        assert_eq!(size as u64, LARGE_ATTACHMENT_BYTES + 1);
        assert!(
            projected > size,
            "the projected message counts the base64 encoding, not the bytes on disk"
        );
    }

    #[test]
    fn approving_a_large_file_attaches_it() {
        let file = oversized_file();
        let mut app = test_app(vec![]);
        app.open_compose();
        drop_file(&mut app, &file);

        app.handle_key(KeyEvent::from(KeyCode::Char('y')))
            .expect("approve");

        let compose = app.compose.as_ref().expect("compose open");
        assert_eq!(compose.attachments().len(), 1);
        assert_eq!(
            compose.attachments()[0].size() as u64,
            LARGE_ATTACHMENT_BYTES + 1
        );
        assert!(compose.large_attachment_question().is_none());
    }

    #[test]
    fn declining_a_large_file_leaves_the_message_alone() {
        let file = oversized_file();
        let mut app = test_app(vec![]);
        app.open_compose();
        drop_file(&mut app, &file);

        app.handle_key(KeyEvent::from(KeyCode::Esc)).expect("skip");

        let compose = app
            .compose
            .as_ref()
            .expect("Esc answers the question, it does not cancel compose");
        assert!(compose.attachments().is_empty());
        assert!(compose.large_attachment_question().is_none());
        assert!(
            compose
                .status()
                .is_some_and(|line| line.starts_with("Skipped")),
            "a file left out has to say so: {:?}",
            compose.status()
        );
    }

    /// The question interrupts a drop part-way through, so the files behind it
    /// have to survive the wait and still be attached afterwards.
    #[test]
    fn a_drop_carries_on_after_the_question_is_answered() {
        use std::io::Write as _;

        let mut small = tempfile::NamedTempFile::new().expect("temp file");
        small.write_all(b"first").expect("write");
        let large = oversized_file();
        let mut last = tempfile::NamedTempFile::new().expect("temp file");
        last.write_all(b"third").expect("write");

        let mut app = test_app(vec![]);
        app.open_compose();
        app.handle_paste_text(&format!(
            "{}\n{}\n{}",
            small.path().display(),
            large.path().display(),
            last.path().display()
        ))
        .expect("drop");

        {
            let compose = app.compose.as_ref().expect("compose open");
            assert_eq!(
                compose.attachments().len(),
                1,
                "the drop stops at the file it has to ask about"
            );
            assert!(compose.large_attachment_question().is_some());
        }

        app.handle_key(KeyEvent::from(KeyCode::Char('n')))
            .expect("skip the big one");

        let compose = app.compose.as_ref().expect("compose open");
        let names: Vec<&str> = compose
            .attachments()
            .iter()
            .map(|attachment| attachment.filename.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                small.path().file_name().unwrap().to_str().unwrap(),
                last.path().file_name().unwrap().to_str().unwrap()
            ],
            "the file behind the question still gets attached"
        );
        assert_eq!(
            compose.status(),
            Some("Attached 2 files (10B). Skipped 1."),
            "the summary covers the whole drop, not just the part after the answer"
        );
    }

    #[test]
    fn the_message_size_counts_the_body_and_the_encoded_attachments() {
        use std::io::Write as _;

        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(&[0u8; 3000]).expect("write");

        let mut app = test_app(vec![]);
        app.open_compose();
        let body_only = app.compose.as_ref().expect("compose").message_size();

        drop_file(&mut app, &file);

        let compose = app.compose.as_ref().expect("compose open");
        assert_eq!(compose.message_size(), body_only + base64_size(3000));
        assert!(
            compose.message_size() > body_only + 3000,
            "base64 costs a third on top of the bytes on disk"
        );
    }

    #[test]
    fn base64_size_counts_the_padding_and_the_line_breaks() {
        assert_eq!(base64_size(0), 0);
        // Three bytes are one full quantum: four characters, one line, one CRLF.
        assert_eq!(base64_size(3), 4 + 2);
        // One byte still costs a padded quantum.
        assert_eq!(base64_size(1), 4 + 2);
        // 57 bytes is exactly one 76-character line.
        assert_eq!(base64_size(57), 76 + 2);
        assert_eq!(base64_size(58), 80 + 4);
    }

    // -- Handing the terminal to the editor and getting it back ---------------

    // These are Unix-only because crossterm may drive a Windows console through
    // the API instead of writing escape sequences, leaving the sink they assert
    // on empty.  The behaviour under test is the same on both.

    /// What the editor is given: a cooked terminal on the main screen with a
    /// visible cursor.  Raw mode is a termios call, so it leaves no bytes here.
    #[cfg(unix)]
    const RELEASED: &str = concat!(
        "\u{1b}[?2004l", // DisableBracketedPaste
        "\u{1b}[?1049l", // LeaveAlternateScreen
        "\u{1b}[?25h",   // Show cursor
    );

    /// What the app takes back once the editor exits.
    #[cfg(unix)]
    const RESTORED: &str = concat!(
        "\u{1b}[?1049h", // EnterAlternateScreen
        "\u{1b}[?2004h", // EnableBracketedPaste
    );

    /// Drive the editor helper the way [`App::edit_compose_body`] does.
    ///
    /// Raw mode is process-wide and `cargo test` runs with the developer's
    /// terminal attached, so this puts it back the way it was found before any
    /// assertion can panic -- leaving it raw would wreck the shell that ran the
    /// tests.
    #[cfg(unix)]
    fn run_editor_in_test(
        app: &mut App,
        sink: &mut Vec<u8>,
        program: &str,
    ) -> std::io::Result<ExitStatus> {
        let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        let status = app.run_editor_command(sink, &mut Command::new(program));
        if !was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        status
    }

    /// The drop-to-attach path above works only while bracketed paste is on,
    /// and every editor worth the name turns it off on its way out.
    #[cfg(unix)]
    #[test]
    fn the_editor_gets_the_terminal_and_gives_it_back() {
        let mut app = test_app(vec![]);
        let mut terminal = Vec::new();
        let status = run_editor_in_test(&mut app, &mut terminal, "true").expect("run");

        assert!(status.success());
        assert_eq!(
            String::from_utf8(terminal).expect("utf-8"),
            format!("{RELEASED}{RESTORED}"),
            "the modes are released for the editor and taken back once it exits"
        );
        assert!(
            app.take_full_redraw(),
            "re-entering the alternate screen clears it, so the next frame cannot be a diff"
        );
        assert!(!app.take_full_redraw(), "the request is taken, not latched");
    }

    /// An editor the user quit without saving exits non-zero -- but it still had
    /// the terminal, so the restore is just as necessary.
    #[cfg(unix)]
    #[test]
    fn the_terminal_comes_back_from_an_editor_that_failed() {
        let mut app = test_app(vec![]);
        let mut terminal = Vec::new();
        let status = run_editor_in_test(&mut app, &mut terminal, "false").expect("run");

        assert!(!status.success());
        assert_eq!(
            String::from_utf8(terminal).expect("utf-8"),
            format!("{RELEASED}{RESTORED}")
        );
        assert!(app.take_full_redraw());
    }

    #[cfg(unix)]
    #[test]
    fn the_terminal_comes_back_from_an_editor_that_never_launches() {
        let mut app = test_app(vec![]);
        let mut terminal = Vec::new();
        let result = run_editor_in_test(&mut app, &mut terminal, "elma-no-such-editor");

        assert!(result.is_err(), "a missing editor has to be reported");
        assert_eq!(
            String::from_utf8(terminal).expect("utf-8"),
            format!("{RELEASED}{RESTORED}"),
            "the early return out of edit_compose_body still leaves a usable terminal"
        );
        assert!(app.take_full_redraw());
    }

    // -- Carrying attachments into compose ------------------------------------

    /// Backend that serves blob downloads, like JMAP does.
    struct BlobBackend;

    impl MailBackend for BlobBackend {
        fn load_mailbox(
            &self,
            _mailbox: MailboxKind,
        ) -> anyhow::Result<(MailboxSnapshot, mpsc::Receiver<BackendEvent>)> {
            let (_tx, rx) = mpsc::channel();
            Ok((
                MailboxSnapshot {
                    total: 0,
                    messages: vec![],
                },
                rx,
            ))
        }

        fn load_message(&self, _message_id: MessageId) -> anyhow::Result<MessageContent> {
            Ok(MessageContent::default())
        }

        fn apply_actions(
            &self,
            _actions: Vec<Action>,
        ) -> anyhow::Result<mpsc::Receiver<ActionStatus>> {
            let (_tx, rx) = mpsc::channel();
            Ok(rx)
        }

        fn send_message(&self, _message: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }

        fn save_draft(&self, _message: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }

        fn fetch_attachment_blob(&self, blob_id: &str) -> anyhow::Result<Vec<u8>> {
            if blob_id == "missing" {
                anyhow::bail!("no such blob");
            }
            Ok(format!("blob:{blob_id}").into_bytes())
        }
    }

    fn attachment(
        filename: Option<&str>,
        mime_type: &str,
        data: Option<&[u8]>,
        blob_id: Option<&str>,
    ) -> MessageAttachment {
        MessageAttachment {
            filename: filename.map(str::to_string),
            mime_type: mime_type.to_string(),
            size: data.map_or(0, |d| d.len()),
            data: data.map(<[u8]>::to_vec),
            blob_id: blob_id.map(str::to_string),
            inline: false,
        }
    }

    /// The same, but a part the body references as `cid:…`.
    fn inline_attachment(filename: &str, data: &[u8]) -> MessageAttachment {
        MessageAttachment {
            inline: true,
            ..attachment(Some(filename), "image/png", Some(data), None)
        }
    }

    #[test]
    fn restore_compose_attachments_keeps_inline_bytes() {
        let app = test_app(vec![]);
        let content = MessageContent {
            mailer: String::new(),
            parts: vec![],
            attachments: vec![attachment(
                Some("report.pdf"),
                "application/pdf",
                Some(b"%PDF-1.7"),
                None,
            )],
        };

        let (restored, unavailable) = restore_compose_attachments(app.backend.as_ref(), &content);
        assert_eq!(unavailable, 0);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].filename, "report.pdf");
        assert_eq!(restored[0].mime_type, "application/pdf");
        assert_eq!(restored[0].data, b"%PDF-1.7");
    }

    #[test]
    fn restore_compose_attachments_downloads_blobs() {
        let app = test_app_with_backend(vec![], Arc::new(BlobBackend));
        let content = MessageContent {
            mailer: String::new(),
            parts: vec![],
            attachments: vec![
                attachment(Some("sheet.xlsx"), "application/pdf", None, Some("b1")),
                // No filename: the name has to come from the content type.
                attachment(None, "image/png", None, Some("b2")),
            ],
        };

        let (restored, unavailable) = restore_compose_attachments(app.backend.as_ref(), &content);
        assert_eq!(unavailable, 0);
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].data, b"blob:b1");
        assert_eq!(restored[1].filename, "attachment-2.png");
        assert_eq!(restored[1].data, b"blob:b2");
    }

    #[test]
    fn restore_compose_attachments_counts_unavailable() {
        let app = test_app_with_backend(vec![], Arc::new(BlobBackend));
        let content = MessageContent {
            mailer: String::new(),
            parts: vec![],
            attachments: vec![
                attachment(Some("ok.txt"), "text/plain", Some(b"hi"), None),
                // Download fails.
                attachment(Some("bad.txt"), "text/plain", None, Some("missing")),
                // Neither bytes nor a blob to fetch.
                attachment(Some("gone.txt"), "text/plain", None, None),
            ],
        };

        let (restored, unavailable) = restore_compose_attachments(app.backend.as_ref(), &content);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].filename, "ok.txt");
        assert_eq!(
            unavailable, 2,
            "attachments that cannot be recovered must be counted, not dropped silently"
        );
    }

    /// An embedded image is a file, but it is not something the sender
    /// attached: a forward quotes the body as text, so carrying the logo over
    /// would attach it to a message that no longer references it.
    #[test]
    fn restore_compose_attachments_leaves_inline_parts_behind() {
        let app = test_app(vec![]);
        let content = MessageContent {
            mailer: String::new(),
            parts: vec![],
            attachments: vec![
                inline_attachment("signature-logo.png", b"PNG"),
                attachment(Some("report.pdf"), "application/pdf", Some(b"%PDF"), None),
            ],
        };

        let (restored, unavailable) = restore_compose_attachments(app.backend.as_ref(), &content);
        assert_eq!(unavailable, 0, "a part left out on purpose is not a loss");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].filename, "report.pdf");
    }

    /// The save dialog is the one place inline parts are offered, so the guard
    /// that refuses to open it has to count them.
    #[test]
    fn a_message_with_only_an_inline_image_can_still_be_saved_from() {
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app(vec![message]);
        app.show_loaded_message(
            1,
            MessageContent {
                mailer: String::new(),
                parts: vec![],
                attachments: vec![inline_attachment("signature-logo.png", b"PNG")],
            },
        );

        app.handle_key(KeyEvent::from(KeyCode::Char('S')))
            .expect("open the save dialog");

        assert!(
            app.save_attachment.is_some(),
            "an embedded image is still a file the reader can keep"
        );
        assert_eq!(app.save_attachment_attachments().len(), 1);
    }

    /// The whole point of the flag: listed for saving, ignored by the marker.
    #[test]
    fn opening_a_message_with_an_inline_image_leaves_the_marker_alone() {
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app(vec![message.clone()]);
        assert!(!app.mailbox.messages[0].has_attachments);

        app.deliver_loaded_message(
            message,
            MessageLoadPurpose::View,
            None,
            LoadedMessage {
                content: MessageContent {
                    mailer: String::new(),
                    parts: vec![],
                    attachments: vec![inline_attachment("signature-logo.png", b"PNG")],
                },
                attachments: Vec::new(),
                unavailable: 0,
            },
        );

        assert!(
            !app.mailbox.messages[0].has_attachments,
            "an `@` must not appear on a newsletter just because it was opened"
        );
    }

    #[test]
    fn forwarding_carries_the_original_attachments() {
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app_with_backend(vec![message.clone()], Arc::new(BlobBackend));

        app.message_view = Some(MessageViewState {
            message_id: message.id,
            message_index: 0,
            message: message.clone(),
            content: MessageContent {
                mailer: String::new(),
                parts: vec![MessageContentPart {
                    content_type: "text/plain".to_string(),
                    content: b"original body".to_vec(),
                }],
                attachments: vec![attachment(
                    Some("invoice.pdf"),
                    "application/pdf",
                    Some(b"%PDF-1.7"),
                    None,
                )],
            },
            document: None,
            raw_html: None,
            scroll: 0,
            unformatted: false,
            info_line: None,
            read_at: None,
        });

        app.open_forward().expect("forward should start loading");
        pump_until(&mut app, "compose to open", |app| app.compose.is_some());

        let compose = app.compose.as_ref().expect("compose should be open");
        assert_eq!(
            compose.attachments().len(),
            1,
            "forwarding must not drop the original attachment"
        );
        assert_eq!(compose.attachments()[0].filename, "invoice.pdf");
        assert_eq!(compose.attachments()[0].data, b"%PDF-1.7");
    }

    // -- Keeping the UI thread free -------------------------------------------

    /// Drive the event loop until `predicate` holds, then return.
    ///
    /// Panics rather than hanging when the background work never lands.
    fn pump_until(app: &mut App, what: &str, mut predicate: impl FnMut(&App) -> bool) {
        for _ in 0..400 {
            app.poll_backend_events();
            if predicate(app) {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    /// Backend whose calls block until the test releases them.
    ///
    /// Every method the UI must not wait on parks on `gate`, so a test can
    /// observe the state the UI is left in *while* the backend is busy.
    struct GatedBackend {
        gate: Mutex<mpsc::Receiver<()>>,
        loads: Mutex<usize>,
        sent: Mutex<Vec<OutgoingMessage>>,
        drafts: Mutex<Vec<OutgoingMessage>>,
        fail: bool,
    }

    impl GatedBackend {
        /// Returns the backend plus the handle that unblocks it.
        fn new(fail: bool) -> (Arc<Self>, mpsc::Sender<()>) {
            let (tx, rx) = mpsc::channel();
            let backend = Arc::new(Self {
                gate: Mutex::new(rx),
                loads: Mutex::new(0),
                sent: Mutex::new(Vec::new()),
                drafts: Mutex::new(Vec::new()),
                fail,
            });
            (backend, tx)
        }

        fn wait(&self) {
            let _ = self.gate.lock().unwrap().recv();
        }

        fn result(&self) -> anyhow::Result<()> {
            if self.fail {
                anyhow::bail!("backend refused");
            }
            Ok(())
        }
    }

    impl MailBackend for GatedBackend {
        fn load_mailbox(
            &self,
            _mailbox: MailboxKind,
        ) -> anyhow::Result<(MailboxSnapshot, mpsc::Receiver<BackendEvent>)> {
            let (_tx, rx) = mpsc::channel();
            Ok((
                MailboxSnapshot {
                    total: 0,
                    messages: vec![],
                },
                rx,
            ))
        }

        fn load_message(&self, _message_id: MessageId) -> anyhow::Result<MessageContent> {
            *self.loads.lock().unwrap() += 1;
            self.wait();
            self.result()?;
            Ok(MessageContent {
                mailer: String::new(),
                parts: vec![MessageContentPart {
                    content_type: "text/plain".to_string(),
                    content: b"loaded body".to_vec(),
                }],
                attachments: vec![],
            })
        }

        fn apply_actions(
            &self,
            actions: Vec<Action>,
        ) -> anyhow::Result<mpsc::Receiver<ActionStatus>> {
            let (tx, rx) = mpsc::channel();
            for action in actions {
                let _ = tx.send(ActionStatus {
                    action,
                    result: Ok(()),
                });
            }
            Ok(rx)
        }

        fn send_message(&self, message: OutgoingMessage) -> anyhow::Result<()> {
            self.wait();
            self.result()?;
            self.sent.lock().unwrap().push(message);
            Ok(())
        }

        fn save_draft(&self, message: OutgoingMessage) -> anyhow::Result<()> {
            self.wait();
            self.result()?;
            self.drafts.lock().unwrap().push(message);
            Ok(())
        }
    }

    fn compose_with_recipient(app: &mut App) {
        let mut compose = ComposeState::new();
        compose.set_field_text(ComposeField::To, "someone@example.com");
        compose.set_field_text(ComposeField::Subject, "Hello");
        app.compose = Some(compose);
    }

    #[test]
    fn sending_does_not_block_the_ui_thread() {
        let (backend, release) = GatedBackend::new(false);
        let mut app = test_app_with_backend(vec![], backend.clone());
        compose_with_recipient(&mut app);

        // Returns while the backend is still parked inside send_message.
        app.submit_outgoing(OutgoingKind::Send).expect("submit");

        assert!(
            app.pending_outgoing().is_some(),
            "the send must be reported as in flight"
        );
        assert!(
            app.compose.is_some(),
            "compose stays open until the backend confirms"
        );
        assert!(
            app.compose_action_bar().contains("Sending message"),
            "the header has to say what the wait is for, got {:?}",
            app.compose_action_bar()
        );

        release.send(()).expect("release the backend");
        pump_until(&mut app, "the send to finish", |app| app.compose.is_none());

        assert_eq!(backend.sent.lock().unwrap().len(), 1);
        assert_eq!(app.mailbox.status_line.as_deref(), Some("Message sent."));
        assert!(app.pending_outgoing().is_none());
    }

    #[test]
    fn keys_are_ignored_while_a_send_is_in_flight() {
        let (backend, _release) = GatedBackend::new(false);
        let mut app = test_app_with_backend(vec![], backend);
        compose_with_recipient(&mut app);
        app.submit_outgoing(OutgoingKind::Send).expect("submit");

        app.handle_key(KeyEvent::from(KeyCode::Esc)).expect("esc");
        app.handle_key(KeyEvent::from(KeyCode::Char('x')))
            .expect("typing");
        app.handle_paste_text("pasted").expect("paste");

        let compose = app
            .compose
            .as_ref()
            .expect("Esc must not discard a message the backend already has");
        assert_eq!(
            compose.field_data(ComposeField::Subject).0,
            "Hello",
            "the message must not change while it is being sent"
        );
    }

    #[test]
    fn a_failed_send_keeps_the_message_in_compose() {
        let (backend, release) = GatedBackend::new(true);
        let mut app = test_app_with_backend(vec![], backend);
        compose_with_recipient(&mut app);
        app.submit_outgoing(OutgoingKind::Send).expect("submit");
        release.send(()).expect("release the backend");

        pump_until(&mut app, "the failure to surface", |app| {
            app.compose_status_line().is_some()
        });

        let compose = app.compose.as_ref().expect("compose must stay open");
        assert_eq!(
            compose.field_data(ComposeField::To).0,
            "someone@example.com",
            "a failed send must not lose the message"
        );
        assert!(
            app.compose_status_line()
                .is_some_and(|status| status.starts_with("Failed to send:")),
            "got {:?}",
            app.compose_status_line()
        );
        assert!(app.pending_outgoing().is_none(), "the operation is over");
    }

    #[test]
    fn saving_a_draft_runs_in_the_background() {
        let (backend, release) = GatedBackend::new(false);
        let mut app = test_app_with_backend(vec![], backend.clone());
        compose_with_recipient(&mut app);

        app.submit_outgoing(OutgoingKind::Draft).expect("submit");
        assert!(app.pending_outgoing().is_some());

        release.send(()).expect("release the backend");
        pump_until(&mut app, "the draft to be stored", |app| {
            app.compose.is_none()
        });

        assert_eq!(backend.drafts.lock().unwrap().len(), 1);
        assert_eq!(app.mailbox.status_line.as_deref(), Some("Draft saved."));
    }

    #[test]
    fn opening_a_message_loads_it_in_the_background() {
        let (backend, release) = GatedBackend::new(false);
        let message = make_message(1, MessageStatus::New);
        let mut app = test_app_with_backend(vec![message], backend);

        app.handle_key(KeyEvent::from(KeyCode::Enter))
            .expect("enter");

        assert!(
            app.message_view.is_none(),
            "the viewer must not open before the body arrives"
        );
        let (label, _) = app
            .pending_message_load()
            .expect("the load must be reported as in flight");
        assert!(label.contains("Subject 1"), "got {label:?}");

        release.send(()).expect("release the backend");
        pump_until(&mut app, "the body to arrive", |app| {
            app.message_view.is_some()
        });

        let view = app.message_view.as_ref().expect("viewer open");
        assert_eq!(view.message_id, 1);
        assert!(app.pending_message_load().is_none());
    }

    #[test]
    fn repeating_the_open_key_does_not_start_a_second_load() {
        let (backend, release) = GatedBackend::new(false);
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app_with_backend(vec![message], backend.clone());

        app.handle_key(KeyEvent::from(KeyCode::Enter))
            .expect("enter");
        // Wait for the worker to reach the backend, so a second fetch would show up.
        for _ in 0..400 {
            if *backend.loads.lock().unwrap() > 0 {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }

        app.handle_key(KeyEvent::from(KeyCode::Enter))
            .expect("enter again");
        thread::sleep(std::time::Duration::from_millis(50));

        assert_eq!(
            *backend.loads.lock().unwrap(),
            1,
            "a second keypress must not fetch the same body twice"
        );

        release.send(()).expect("release the backend");
        pump_until(&mut app, "the body to arrive", |app| {
            app.message_view.is_some()
        });
    }

    #[test]
    fn a_failed_load_reports_instead_of_aborting() {
        let (backend, release) = GatedBackend::new(true);
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app_with_backend(vec![message], backend);

        app.handle_key(KeyEvent::from(KeyCode::Enter))
            .expect("enter");
        release.send(()).expect("release the backend");

        pump_until(&mut app, "the failure to surface", |app| {
            app.mailbox.status_line.is_some()
        });

        assert!(
            app.mailbox
                .status_line
                .as_deref()
                .is_some_and(|status| status.starts_with("Failed to load message:")),
            "got {:?}",
            app.mailbox.status_line
        );
        assert!(app.message_view.is_none());
    }

    #[test]
    fn forwarding_downloads_blobs_off_the_ui_thread() {
        let (backend, release) = GatedBackend::new(false);
        let message = make_message(1, MessageStatus::Read);
        let mut app = test_app_with_backend(vec![message], backend);

        app.open_forward().expect("forward should start loading");

        assert!(
            app.compose.is_none(),
            "compose must wait for the attachments instead of freezing the UI"
        );
        assert!(app.pending_message_load().is_some());

        release.send(()).expect("release the backend");
        pump_until(&mut app, "compose to open", |app| app.compose.is_some());
    }

    /// Backend whose mailbox load parks until the test releases it.
    ///
    /// Stands in for the real cost of connecting and authenticating, which is
    /// what startup used to wait for one account at a time.
    struct SlowLoadBackend {
        gate: Mutex<mpsc::Receiver<()>>,
        started: Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
    }

    impl SlowLoadBackend {
        fn new(fail: bool) -> (Arc<Self>, mpsc::Sender<()>) {
            let (tx, rx) = mpsc::channel();
            let backend = Arc::new(Self {
                gate: Mutex::new(rx),
                started: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail,
            });
            (backend, tx)
        }

        /// How many loads have entered the backend, released or not.
        fn started(&self) -> usize {
            self.started.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl MailBackend for SlowLoadBackend {
        fn load_mailbox(
            &self,
            _mailbox: MailboxKind,
        ) -> anyhow::Result<(MailboxSnapshot, mpsc::Receiver<BackendEvent>)> {
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = self.gate.lock().unwrap().recv();
            if self.fail {
                anyhow::bail!("could not reach the server");
            }
            let (_tx, rx) = mpsc::channel();
            Ok((
                MailboxSnapshot {
                    total: 1,
                    messages: vec![make_message(1, MessageStatus::New)],
                },
                rx,
            ))
        }

        fn load_message(&self, _message_id: MessageId) -> anyhow::Result<MessageContent> {
            unimplemented!()
        }

        fn apply_actions(
            &self,
            _actions: Vec<Action>,
        ) -> anyhow::Result<mpsc::Receiver<ActionStatus>> {
            let (_tx, rx) = mpsc::channel();
            Ok(rx)
        }

        fn send_message(&self, _message: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }

        fn save_draft(&self, _message: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Spin until `predicate` holds, without pumping the app.
    fn spin_until(what: &str, mut predicate: impl FnMut() -> bool) {
        for _ in 0..400 {
            if predicate() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn startup_does_not_wait_for_the_accounts_to_connect() {
        let (backend, release) = SlowLoadBackend::new(false);
        // Returns while the backend is still parked inside load_mailbox: the
        // terminal can be set up and a first frame drawn before any account is
        // reachable.
        let app = App::new(vec![AccountDescriptor::new("Work", backend.clone())]).expect("app");

        let (name, mailbox, state) = app
            .loading_overlay()
            .expect("an account that has not connected yet must explain itself");
        assert_eq!(name, "Work");
        assert_eq!(mailbox, MailboxKind::Inbox);
        assert_eq!(state.phase, LoadPhase::Connecting);

        release.send(()).expect("release the backend");
    }

    #[test]
    fn every_account_connects_at_once() {
        let (first, release_first) = SlowLoadBackend::new(false);
        let (second, release_second) = SlowLoadBackend::new(false);

        let app = App::new(vec![
            AccountDescriptor::new("First", first.clone()),
            AccountDescriptor::new("Second", second.clone()),
        ])
        .expect("app");

        // Both are inside load_mailbox while neither has been released, so the
        // second account did not have to wait out the first.
        spin_until("both accounts to start loading", || {
            first.started() == 1 && second.started() == 1
        });
        assert!(app.loading_overlay().is_some());

        release_first.send(()).expect("release first");
        release_second.send(()).expect("release second");
    }

    #[test]
    fn the_overlay_gives_way_to_the_first_messages() {
        let (backend, release) = SlowLoadBackend::new(false);
        let mut app = App::new(vec![AccountDescriptor::new("Work", backend.clone())]).expect("app");

        release.send(()).expect("release the backend");
        pump_until(&mut app, "the inbox to arrive", |app| {
            app.loading_overlay().is_none()
        });

        // The overlay lifts on the first batch of messages rather than on the
        // end of the load, so the list is readable while the rest arrives.
        assert!(
            app.mailbox.messages.iter().any(|msg| !msg.is_placeholder()),
            "the overlay may only go once there is something to read"
        );

        pump_until(&mut app, "the load to finish", |app| app.loaded);
        assert!(
            app.loading.is_none(),
            "a finished load leaves nothing behind"
        );
    }

    #[test]
    fn a_background_account_finishes_without_being_looked_at() {
        let (visible, release_visible) = SlowLoadBackend::new(false);
        let (background, release_background) = SlowLoadBackend::new(false);

        let mut app = App::new(vec![
            AccountDescriptor::new("Visible", visible.clone()),
            AccountDescriptor::new("Background", background.clone()),
        ])
        .expect("app");

        release_visible.send(()).expect("release visible");
        release_background.send(()).expect("release background");

        // The second account is never selected, so only a poll that covers every
        // account will ever complete it.
        pump_until(&mut app, "the background account to load", |app| {
            app.accounts[1].loaded
        });
        assert!(
            app.accounts[1]
                .mailbox
                .messages
                .iter()
                .any(|msg| !msg.is_placeholder()),
            "switching to it later must land on a ready mailbox"
        );
    }

    #[test]
    fn switching_to_an_account_that_is_still_loading_explains_the_wait() {
        let (ready, release_ready) = SlowLoadBackend::new(false);
        let (slow, release_slow) = SlowLoadBackend::new(false);

        let mut app = App::new(vec![
            AccountDescriptor::new("Ready", ready.clone()),
            AccountDescriptor::new("Slow", slow.clone()),
        ])
        .expect("app");

        release_ready.send(()).expect("release ready");
        pump_until(&mut app, "the first account to load", |app| {
            app.loading_overlay().is_none()
        });

        app.switch_account(1).expect("switch");

        let (name, _, state) = app
            .loading_overlay()
            .expect("landing on an account that is still connecting must say so");
        assert_eq!(name, "Slow");
        assert_eq!(state.phase, LoadPhase::Connecting);

        release_slow.send(()).expect("release slow");
        pump_until(&mut app, "the second account to load", |app| {
            app.loading_overlay().is_none()
        });
    }

    #[test]
    fn switching_mailbox_explains_the_wait_until_messages_arrive() {
        let (backend, release) = SlowLoadBackend::new(false);
        let mut app = App::new(vec![AccountDescriptor::new("Work", backend.clone())]).expect("app");

        release.send(()).expect("release the inbox load");
        pump_until(&mut app, "the inbox to arrive", |app| {
            app.loading_overlay().is_none()
        });

        // The backend gates every load, so this one parks the same way.
        app.switch_mailbox(MailboxKind::Archive).expect("switch");

        let (_, mailbox, state) = app
            .loading_overlay()
            .expect("an empty list mid-switch must explain itself");
        assert_eq!(mailbox, MailboxKind::Archive);
        // Not `Connecting`: the session from the inbox load is still up, and
        // claiming otherwise reads as though every folder switch reconnected.
        assert_eq!(state.phase, LoadPhase::Opening);

        release.send(()).expect("release the archive load");
        pump_until(&mut app, "the archive to arrive", |app| {
            app.loading_overlay().is_none()
        });
    }

    #[test]
    fn a_failed_load_says_why_where_there_is_room_to_read_it() {
        let (backend, release) = SlowLoadBackend::new(true);
        let mut app = App::new(vec![AccountDescriptor::new("Work", backend.clone())]).expect("app");

        release.send(()).expect("release the backend");
        pump_until(&mut app, "the load to fail", |app| {
            matches!(
                app.loading_overlay().map(|(_, _, state)| &state.phase),
                Some(LoadPhase::Failed(_))
            )
        });

        let (_, _, state) = app
            .loading_overlay()
            .expect("the reason must stay on screen");
        let LoadPhase::Failed(reason) = &state.phase else {
            panic!("expected a failure phase");
        };
        assert!(
            reason.contains("could not reach the server"),
            "the overlay has to carry the backend's reason, got {reason:?}"
        );
        assert!(!app.loaded, "a failed load must not count as loaded");
    }
}
