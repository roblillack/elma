//! Core application state and controller logic.
//!
//! [`App`] encapsulates all user interface state (selected message, scheduled
//! actions, progress indicators) and synchronises with the configured backend.
//! The type is intentionally synchronous from the TUI's perspective yet internally
//! manages asynchronous commit results via channels so the UI thread never blocks.

use crate::backend::{ActionStatus, BackendEvent, MailBackend, OutgoingMessage};
use crate::model::{
    Action, ActionType, MailboxKind, Message, MessageContent, MessageContentPart, MessageId,
    MessageStatus, format_size, padded_sender,
};
use anyhow::{Context, Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use shell_words::split as shell_split;
use std::{
    cmp::{max, min},
    collections::VecDeque,
    env, fs,
    io::{Cursor, Write},
    ops::{Deref, DerefMut},
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
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
    Cancel,
    Edit,
    Draft,
    Send,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeFocus {
    Field(ComposeField),
    Body,
    Button(ComposeButton),
}

#[derive(Default)]
struct TextFieldState {
    value: String,
    cursor: usize,
}

pub(crate) struct ComposeState {
    to: TextFieldState,
    cc: TextFieldState,
    bcc: TextFieldState,
    subject: TextFieldState,
    body: Document,
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
            body: Document::new(),
            draft_id: None,
            focus: ComposeFocus::Field(ComposeField::To),
            status: None,
            body_scroll: 0,
            body_view_height: 0,
        }
    }
}

const COMPOSE_BUTTON_SEQUENCE: [ComposeButton; 4] = [
    ComposeButton::Cancel,
    ComposeButton::Edit,
    ComposeButton::Draft,
    ComposeButton::Send,
];

const COMPOSE_FOCUS_SEQUENCE: [ComposeFocus; 9] = [
    ComposeFocus::Field(ComposeField::To),
    ComposeFocus::Field(ComposeField::Cc),
    ComposeFocus::Field(ComposeField::Bcc),
    ComposeFocus::Field(ComposeField::Subject),
    ComposeFocus::Body,
    ComposeFocus::Button(ComposeButton::Cancel),
    ComposeFocus::Button(ComposeButton::Edit),
    ComposeFocus::Button(ComposeButton::Draft),
    ComposeFocus::Button(ComposeButton::Send),
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
        state.body = body;
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

    fn focus_next(&mut self) {
        let current_idx = COMPOSE_FOCUS_SEQUENCE
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = (current_idx + 1) % COMPOSE_FOCUS_SEQUENCE.len();
        self.focus = COMPOSE_FOCUS_SEQUENCE[next];
    }

    fn focus_prev(&mut self) {
        let current_idx = COMPOSE_FOCUS_SEQUENCE
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let prev = if current_idx == 0 {
            COMPOSE_FOCUS_SEQUENCE.len() - 1
        } else {
            current_idx - 1
        };
        self.focus = COMPOSE_FOCUS_SEQUENCE[prev];
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

    pub(crate) fn field_parts(&self, field: ComposeField) -> (&str, &str) {
        let (value, cursor) = self.field_data(field);
        let idx = byte_index_for(value, cursor);
        value.split_at(idx)
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
        })
    }

    pub(crate) fn body(&self) -> &Document {
        &self.body
    }

    pub(crate) fn set_body(&mut self, document: Document) {
        self.body = document;
        self.body_scroll = 0;
        self.body_view_height = 0;
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
        self.body = document;
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
        paragraph.content_mut().push(Span::new_text(text.into_owned()));
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

fn byte_index_for(text: &str, char_index: usize) -> usize {
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
    pub fn new(descriptors: Vec<AccountDescriptor>) -> Result<Self> {
        if descriptors.is_empty() {
            return Err(anyhow!("no accounts configured"));
        }

        let mut accounts = Vec::with_capacity(descriptors.len());

        for descriptor in descriptors {
            let backend = descriptor.backend;
            let account_name = descriptor.name;
            let (mut snapshot, events) = backend
                .load_inbox()
                .with_context(|| format!("failed to load inbox for account {}", account_name))?;
            snapshot.messages.sort_by_key(|msg| msg.seq);

            let mut messages = Vec::new();
            if snapshot.total > 0 {
                ensure_placeholder_capacity(&mut messages, snapshot.total);
            }
            for message in snapshot.messages {
                if message.seq == 0 {
                    messages.push(message);
                    continue;
                }
                let index = message.seq.saturating_sub(1) as usize;
                ensure_placeholder_capacity(&mut messages, index + 1);
                messages[index] = message;
            }

            let selected = last_loaded_index(&messages).or_else(|| {
                if messages.is_empty() {
                    None
                } else {
                    Some(messages.len() - 1)
                }
            });

            let total = messages.len();
            let loaded = loaded_message_count(&messages);
            let initial_load_progress = if loaded < total {
                Some(CommitProgress {
                    total,
                    completed: loaded,
                })
            } else {
                None
            };

            accounts.push(AccountState {
                name: account_name,
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
                mailbox_load_progress: initial_load_progress,
                scheduled_actions: Vec::new(),
                current_mailbox: MailboxKind::Inbox,
                search: None,
            });
        }

        Ok(Self {
            accounts,
            active_account: 0,
            compose: None,
            should_quit: false,
            pending_shortcut: None,
            pending_navigation: None,
        })
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

    fn begin_mailbox_load(&mut self, target: MailboxKind, status: Option<String>) -> Result<()> {
        let backend = Arc::clone(&self.current_account().backend);
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
                let _ = sender.send(MailboxLoadUpdate::Failed(err.to_string()));
            }
        });

        {
            let account = self.current_account_mut();
            account.mailbox_loader = Some(MailboxLoaderState { receiver });
            account.mailbox_load_progress = Some(CommitProgress {
                total: PROGRESS_SEGMENTS,
                completed: 0,
            });
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
        self.poll_commit_updates();

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

    fn poll_mailbox_loader(&mut self) {
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
        format!("{label} - Tab:Next Shift+Tab:Prev Esc:Cancel Enter:Activate")
    }

    pub(crate) fn compose_status_line(&self) -> Option<&str> {
        self.compose.as_ref().and_then(|state| state.status())
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
            KeyCode::Enter => {
                if !self.try_open_selected_draft()? {
                    self.open_selected_message()?;
                }
            }
            KeyCode::Right => self.open_selected_message()?,
            KeyCode::Char('d') | KeyCode::Char('D') => self.schedule_delete(),
            KeyCode::Char('#') => self.schedule_delete(),
            KeyCode::Backspace | KeyCode::Delete => self.schedule_delete(),
            KeyCode::Char('/') => self.open_search(),
            KeyCode::Esc => {
                if self.search.is_some() {
                    self.close_search();
                }
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

        if matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) {
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
        self.compose = Some(ComposeState::new());
        self.message_view = None;
        self.mailbox.status_line = Some("Compose mode active.".to_string());
    }

    fn document_for_message(&mut self, message: &Message) -> Result<Document> {
        if let Some(view) = self.message_view.as_ref()
            && view.message_id == message.id
        {
            if let Some(document) = &view.document {
                return Ok(document.clone());
            }
            return Ok(document_from_message_content(&view.content));
        }

        let content = self
            .backend
            .load_message(message.id)
            .with_context(|| format!("failed to load message {}", message.id))?;
        Ok(document_from_message_content(&content))
    }

    fn open_reply(&mut self, reply_all: bool) -> Result<()> {
        let message = match self.selected_loaded_message() {
            Some(message) => message.clone(),
            None => {
                self.mailbox
                    .status_line
                    .get_or_insert_with(|| "Message is still loading.".to_string());
                return Ok(());
            }
        };

        let document = match self.document_for_message(&message) {
            Ok(doc) => doc,
            Err(err) => {
                self.mailbox.status_line = Some(format!("Failed to load message body: {err}"));
                return Ok(());
            }
        };

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
        Ok(())
    }

    fn open_forward(&mut self) -> Result<()> {
        let message = match self.selected_loaded_message() {
            Some(message) => message.clone(),
            None => {
                self.mailbox
                    .status_line
                    .get_or_insert_with(|| "Message is still loading.".to_string());
                return Ok(());
            }
        };

        let document = match self.document_for_message(&message) {
            Ok(doc) => doc,
            Err(err) => {
                self.mailbox.status_line = Some(format!("Failed to load message body: {err}"));
                return Ok(());
            }
        };

        let mut compose = ComposeState::new();
        let subject = prefix_subject(&message.subject, "Fwd:");
        compose.set_field_text(ComposeField::Subject, subject);
        compose.set_body(build_forward_document(&document, &message));
        compose.set_focus(ComposeFocus::Body);

        self.compose = Some(compose);
        self.message_view = None;
        self.mailbox.status_line = Some(format!("Forwarding '{}'.", message.subject));
        Ok(())
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return Ok(());
        }

        let Some(compose) = self.compose.as_mut() else {
            return Ok(());
        };

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
            ComposeButton::Cancel => {
                self.cancel_compose();
                Ok(())
            }
            ComposeButton::Edit => self.edit_compose_body(),
            ComposeButton::Draft => self.save_current_draft(),
            ComposeButton::Send => self.send_current_compose(),
        }
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

        let editor_status = if let Some((program, args)) = argv.split_first() {
            let mut command = Command::new(program);
            if !args.is_empty() {
                command.args(args);
            }
            command.arg(&temp_path);
            command.status()
        } else {
            Command::new("vi").arg(&temp_path).status()
        };

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

    fn send_current_compose(&mut self) -> Result<()> {
        let (draft_id, message) = match self.compose.as_ref() {
            Some(compose) => {
                let draft_id = compose.draft_id();
                match compose.to_outgoing() {
                    Ok(message) => (draft_id, message),
                    Err(err) => {
                        if let Some(compose) = self.compose.as_mut() {
                            compose.set_status(format!("Failed to prepare message: {err}"));
                        }
                        return Ok(());
                    }
                }
            }
            None => return Ok(()),
        };

        if message.to.is_empty() && message.cc.is_empty() && message.bcc.is_empty() {
            if let Some(compose) = self.compose.as_mut() {
                compose.set_status("Add at least one recipient.");
            }
            return Ok(());
        }

        match self.backend.send_message(message) {
            Ok(()) => {
                let mut status = "Message sent.".to_string();
                if let Some(id) = draft_id {
                    self.remove_message_from_mailbox(id);
                    if let Err(err) = self.submit_actions(vec![Action::new(ActionType::Delete, id)])
                    {
                        status = format!("Message sent but failed to remove draft: {err}");
                    }
                }
                self.compose = None;
                self.mailbox.status_line = Some(status);
            }
            Err(err) => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.set_status(format!("Failed to send: {err}"));
                }
            }
        }
        Ok(())
    }

    fn save_current_draft(&mut self) -> Result<()> {
        let (draft_id, message) = match self.compose.as_ref() {
            Some(compose) => {
                let draft_id = compose.draft_id();
                match compose.to_outgoing() {
                    Ok(message) => (draft_id, message),
                    Err(err) => {
                        if let Some(compose) = self.compose.as_mut() {
                            compose.set_status(format!("Failed to prepare draft: {err}"));
                        }
                        return Ok(());
                    }
                }
            }
            None => return Ok(()),
        };

        match self.backend.save_draft(message) {
            Ok(()) => {
                let mut status = if draft_id.is_some() {
                    "Draft updated.".to_string()
                } else {
                    "Draft saved.".to_string()
                };

                if let Some(id) = draft_id {
                    self.remove_message_from_mailbox(id);
                    if let Err(err) = self.submit_actions(vec![Action::new(ActionType::Delete, id)])
                    {
                        status = format!("Draft saved but failed to remove previous copy: {err}");
                    }
                }

                self.compose = None;
                self.mailbox.status_line = Some(status);
            }
            Err(err) => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.set_status(format!("Failed to save draft: {err}"));
                }
            }
        }
        Ok(())
    }

    fn try_open_selected_draft(&mut self) -> Result<bool> {
        let idx = match self.mailbox.selected {
            Some(idx) => idx,
            None => return Ok(false),
        };

        let Some(message) = self.mailbox.messages.get(idx).cloned() else {
            return Ok(false);
        };

        if !self.is_draft_message(&message) {
            return Ok(false);
        }

        let content = self
            .backend
            .load_message(message.id)
            .with_context(|| format!("failed to load draft {}", message.id))?;

        let body = document_from_message_content(&content);

        let compose = ComposeState::from_draft(
            message.id,
            message.recipients.join(", "),
            String::new(),
            String::new(),
            message.subject.clone(),
            body,
        );

        self.compose = Some(compose);
        self.message_view = None;
        self.mailbox.status_line = Some("Editing draft.".to_string());
        Ok(true)
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

        let real_idx = match self.real_selected_index() {
            Some(idx) => idx,
            None => return Ok(()),
        };

        let mut message = match self.selected_loaded_message() {
            Some(msg) => msg.clone(),
            None => {
                self.mailbox
                    .status_line
                    .get_or_insert_with(|| "Message is still loading.".to_string());
                return Ok(());
            }
        };

        let content = self
            .backend
            .load_message(message.id)
            .with_context(|| format!("failed to load message {}", message.id))?;

        let has_attachments = !content.attachments.is_empty();
        if message.has_attachments != has_attachments {
            message.has_attachments = has_attachments;
            if let Some(slot) = self.selected_loaded_message_mut() {
                slot.has_attachments = has_attachments;
            }
        }

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

        Ok(())
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
            let seq = message.seq;
            return MessageRow {
                sequence: format!("{:>5}", seq),
                flags: "    ".to_string(),
                date: "Loading".to_string(),
                sender: padded_sender("Loading"),
                size: String::new(),
                uid: String::from("..."),
                subject: format!("Loading message #{seq}..."),
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
            sequence: format!("{:>5}", message.seq),
            flags: message.flag_string(),
            date: message.formatted_received(now),
            sender: padded_sender(&display_name),
            size: format_size(message.size),
            uid: message.uid.to_string(),
            subject: message.subject.clone(),
            labels: message.labels.clone(),
            status: message.status,
            starred: message.starred,
        }
    }
}

pub(crate) struct MessageRow {
    pub(crate) sequence: String,
    pub(crate) flags: String,
    pub(crate) date: String,
    pub(crate) sender: String,
    pub(crate) size: String,
    pub(crate) uid: String,
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
}
