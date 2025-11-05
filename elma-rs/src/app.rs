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
use std::{
    cmp::{max, min},
    collections::VecDeque,
    io::Cursor,
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError},
    },
    thread,
};
use tdoc::{Document, html};
use time::OffsetDateTime;

const PAGE_JUMP: isize = 5;
const PROGRESS_SEGMENTS: usize = 5;
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

fn next_loaded_index(messages: &[Message], start: usize) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(idx, msg)| (!msg.is_placeholder()).then_some(idx))
}

fn previous_loaded_index(messages: &[Message], start: usize) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .take(start)
        .rev()
        .find_map(|(idx, msg)| (!msg.is_placeholder()).then_some(idx))
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
}

/// Which screen the UI is currently rendering.
pub enum ActiveView {
    Mailbox,
    Message,
    Compose,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeField {
    To,
    Cc,
    Bcc,
    Subject,
    Content,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeButton {
    Cancel,
    Draft,
    Send,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeFocus {
    Field(ComposeField),
    Button(ComposeButton),
}

#[derive(Default)]
struct TextFieldState {
    value: String,
    cursor: usize,
}

#[derive(Default)]
struct TextAreaState {
    value: String,
    cursor: usize,
}

pub(crate) struct ComposeState {
    to: TextFieldState,
    cc: TextFieldState,
    bcc: TextFieldState,
    subject: TextFieldState,
    content: TextAreaState,
    draft_id: Option<MessageId>,
    focus: ComposeFocus,
    status: Option<String>,
}

impl Default for ComposeState {
    fn default() -> Self {
        Self {
            to: TextFieldState::default(),
            cc: TextFieldState::default(),
            bcc: TextFieldState::default(),
            subject: TextFieldState::default(),
            content: TextAreaState::default(),
            draft_id: None,
            focus: ComposeFocus::Field(ComposeField::To),
            status: None,
        }
    }
}

const COMPOSE_FIELD_SEQUENCE: [ComposeField; 5] = [
    ComposeField::To,
    ComposeField::Cc,
    ComposeField::Bcc,
    ComposeField::Subject,
    ComposeField::Content,
];

const COMPOSE_BUTTON_SEQUENCE: [ComposeButton; 3] = [
    ComposeButton::Cancel,
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
        body: String,
    ) -> Self {
        let mut state = Self::default();
        state.draft_id = Some(draft_id);
        state.to.value = to;
        state.to.cursor = text_len(&state.to.value);
        state.cc.value = cc;
        state.cc.cursor = text_len(&state.cc.value);
        state.bcc.value = bcc;
        state.bcc.cursor = text_len(&state.bcc.value);
        state.subject.value = subject;
        state.subject.cursor = text_len(&state.subject.value);
        state.content.value = body;
        state.content.cursor = text_len(&state.content.value);
        state.focus = ComposeFocus::Field(ComposeField::Content);
        state
    }

    pub(crate) fn focus(&self) -> ComposeFocus {
        self.focus
    }

    fn set_focus(&mut self, focus: ComposeFocus) {
        self.focus = focus;
    }

    fn focus_next(&mut self) {
        self.focus = match self.focus {
            ComposeFocus::Field(current) => {
                let mut iter = COMPOSE_FIELD_SEQUENCE.iter();
                while let Some(field) = iter.next() {
                    if *field == current {
                        break;
                    }
                }
                if let Some(next) = iter.next() {
                    ComposeFocus::Field(*next)
                } else {
                    ComposeFocus::Button(COMPOSE_BUTTON_SEQUENCE[0])
                }
            }
            ComposeFocus::Button(current) => {
                let mut iter = COMPOSE_BUTTON_SEQUENCE.iter();
                while let Some(button) = iter.next() {
                    if *button == current {
                        break;
                    }
                }
                if let Some(next) = iter.next() {
                    ComposeFocus::Button(*next)
                } else {
                    ComposeFocus::Field(COMPOSE_FIELD_SEQUENCE[0])
                }
            }
        };
    }

    fn focus_prev(&mut self) {
        self.focus = match self.focus {
            ComposeFocus::Field(current) => {
                if let Some(pos) = COMPOSE_FIELD_SEQUENCE
                    .iter()
                    .position(|field| *field == current)
                {
                    if pos == 0 {
                        ComposeFocus::Button(*COMPOSE_BUTTON_SEQUENCE.last().unwrap())
                    } else {
                        ComposeFocus::Field(COMPOSE_FIELD_SEQUENCE[pos - 1])
                    }
                } else {
                    ComposeFocus::Field(COMPOSE_FIELD_SEQUENCE[0])
                }
            }
            ComposeFocus::Button(current) => {
                if let Some(pos) = COMPOSE_BUTTON_SEQUENCE
                    .iter()
                    .position(|button| *button == current)
                {
                    if pos == 0 {
                        ComposeFocus::Field(*COMPOSE_FIELD_SEQUENCE.last().unwrap())
                    } else {
                        ComposeFocus::Button(COMPOSE_BUTTON_SEQUENCE[pos - 1])
                    }
                } else {
                    ComposeFocus::Button(COMPOSE_BUTTON_SEQUENCE[0])
                }
            }
        };
    }

    fn focus_button_next(&mut self) {
        let next = match self.focus {
            ComposeFocus::Button(current) => {
                let mut iter = COMPOSE_BUTTON_SEQUENCE.iter();
                while let Some(button) = iter.next() {
                    if *button == current {
                        break;
                    }
                }
                iter.next().copied().unwrap_or(COMPOSE_BUTTON_SEQUENCE[0])
            }
            _ => COMPOSE_BUTTON_SEQUENCE[0],
        };
        self.focus = ComposeFocus::Button(next);
    }

    fn focus_button_prev(&mut self) {
        let prev = match self.focus {
            ComposeFocus::Button(current) => {
                let mut last_seen = *COMPOSE_BUTTON_SEQUENCE.last().unwrap();
                for button in COMPOSE_BUTTON_SEQUENCE {
                    if button == current {
                        break;
                    }
                    last_seen = button;
                }
                last_seen
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
            ComposeField::Content => (&self.content.value[..], self.content.cursor),
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

    fn field_state_mut(&mut self, field: ComposeField) -> FieldStateMut<'_> {
        match field {
            ComposeField::To => FieldStateMut::Text(&mut self.to),
            ComposeField::Cc => FieldStateMut::Text(&mut self.cc),
            ComposeField::Bcc => FieldStateMut::Text(&mut self.bcc),
            ComposeField::Subject => FieldStateMut::Text(&mut self.subject),
            ComposeField::Content => FieldStateMut::Area(&mut self.content),
        }
    }

    pub(crate) fn to_outgoing(&self) -> OutgoingMessage {
        OutgoingMessage {
            to: split_addresses(&self.to.value),
            cc: split_addresses(&self.cc.value),
            bcc: split_addresses(&self.bcc.value),
            subject: self.subject.value.clone(),
            content: self.content.value.clone(),
        }
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

impl TextAreaState {
    fn insert(&mut self, ch: char) -> bool {
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

enum FieldStateMut<'a> {
    Text(&'a mut TextFieldState),
    Area(&'a mut TextAreaState),
}

fn compose_body_from_content(content: &MessageContent) -> String {
    if let Some(plain) = content
        .parts
        .iter()
        .find(|part| mime_type_matches(part, "text/plain"))
    {
        return String::from_utf8_lossy(&plain.content).into_owned();
    }

    if let Some(html) = content
        .parts
        .iter()
        .find(|part| mime_type_matches(part, "text/html"))
    {
        return String::from_utf8_lossy(&html.content).into_owned();
    }

    String::new()
}

fn mime_type_matches(part: &MessageContentPart, expected: &str) -> bool {
    part.content_type
        .split(';')
        .next()
        .map(|value| value.trim())
        .map_or(false, |value| value.eq_ignore_ascii_case(expected))
}

fn split_addresses(input: &str) -> Vec<String> {
    input
        .split(|ch| ch == ',' || ch == ';')
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

    let mut count = 0usize;
    for (idx, _) in text.char_indices() {
        if count == char_index {
            return idx;
        }
        count += 1;
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
                mailbox_load_progress: None,
                scheduled_actions: Vec::new(),
                current_mailbox: MailboxKind::Inbox,
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
            KeyCode::Char(ch) if matches!(ch, 'y' | 'Y') => {
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
            KeyCode::Char(ch) if matches!(ch, 'n' | 'N') => {
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

        self.normalize_scroll();
        true
    }

    /// Drain backend event channels and merge them into local state.
    pub fn poll_backend_events(&mut self) {
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
                        *existing = message;
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
                    .get_or_insert_with(|| CommitProgress { total, completed });
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

                if let Some(progress) = self.mailbox_load_progress.as_ref() {
                    if progress.total > 0 {
                        self.mailbox.status_line = Some(format!(
                            "Loading {current}: {}/{} messages",
                            completed, progress.total
                        ));
                    }
                }
            }
            MailboxLoadUpdate::Finished { events, status } => {
                self.mailbox.events = events;
                self.mailbox_loader = None;
                self.mailbox_load_progress = None;
                let loaded = loaded_message_count(&self.mailbox.messages);
                let total = self.mailbox.messages.len();
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

            if delta_completed > 0 {
                if let Some(progress) = self.commit_progress.as_mut() {
                    progress.completed = progress
                        .completed
                        .saturating_add(delta_completed)
                        .min(progress.total);
                }
            }
        }

        self.finalize_commit_batches();
    }

    /// Integrate any finished batches back into the inbox state.
    fn finalize_commit_batches(&mut self) {
        loop {
            let ready = match self.commit_batches.front() {
                Some(batch) if batch.completed >= batch.len() || batch.finished => true,
                _ => false,
            };

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

            if batch.failed.is_empty() {
                self.mailbox.messages.retain(|msg| {
                    msg.status != MessageStatus::Archived
                        && msg.status != MessageStatus::Deleted
                        && msg.status != MessageStatus::PendingInbox
                        && msg.status != MessageStatus::Spam
                });
                self.mailbox.status_line = Some("Actions committed.".to_string());
            } else {
                let summary = format!("Failed to apply {} actions.", batch.failed.len());
                self.mailbox.status_line = Some(summary);
                self.scheduled_actions
                    .extend(batch.failed.into_iter().map(|(action, _error)| action));
            }

            self.sync_message_view_state();
            if let Some(idx) = self.mailbox.selected {
                if idx >= self.mailbox.messages.len() && !self.mailbox.messages.is_empty() {
                    self.mailbox.selected = Some(self.mailbox.messages.len() - 1);
                }
            }

            if self.mailbox.messages.is_empty() {
                self.mailbox.selected = None;
                self.message_view = None;
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

    pub(crate) fn inbox_selected(&self) -> Option<usize> {
        self.mailbox.selected
    }

    fn selected_loaded_message(&self) -> Option<&Message> {
        self.mailbox
            .selected
            .and_then(|idx| self.mailbox.messages.get(idx))
            .filter(|msg| !msg.is_placeholder())
    }

    fn selected_loaded_message_mut(&mut self) -> Option<&mut Message> {
        let idx = self.mailbox.selected?;
        self.mailbox
            .messages
            .get_mut(idx)
            .filter(|msg| !msg.is_placeholder())
    }

    pub(crate) fn inbox_action_bar(&self) -> String {
        let mut text = String::from("^Q:Quit g:GoToMailbox G:Accounts c:Compose");

        if let Some(idx) = self.mailbox.selected {
            if let Some(msg) = self.mailbox.messages.get(idx) {
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
                        text.push_str(" r:Reply");
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
        }

        if !self.scheduled_actions.is_empty() {
            text.push_str(" $:Commit");
        }

        text
    }

    /// Renderable text indicator reflecting aggregate commit progress.
    pub(crate) fn commit_indicator(&self) -> Option<String> {
        if let Some(progress) = self
            .mailbox_load_progress
            .as_ref()
            .and_then(Self::format_progress)
        {
            return Some(progress);
        }

        self.commit_progress
            .as_ref()
            .and_then(Self::format_progress)
    }

    fn format_progress(progress: &CommitProgress) -> Option<String> {
        if progress.total == 0 {
            return None;
        }

        let capped_completed = progress.completed.min(progress.total);
        let filled = (capped_completed * PROGRESS_SEGMENTS + progress.total - 1) / progress.total;

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
        let selected = self
            .mailbox
            .selected
            .map(|idx| format!("{}", idx + 1))
            .unwrap_or_else(|| "-".to_string());
        format!(
            "{} • {} — message {selected}/{total}, {} scheduled actions, got {} events",
            self.name,
            self.current_mailbox,
            self.scheduled_actions.len(),
            self.mailbox.event_count
        )
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
            KeyCode::Char('!') => {
                if let Some(idx) = self.mailbox.selected {
                    if let Some(msg) = self.mailbox.messages.get(idx) {
                        if self.current_mailbox == MailboxKind::Spam
                            || msg.status == MessageStatus::Spam
                        {
                            self.schedule_move_to_inbox();
                        } else {
                            self.schedule_move_to_spam();
                        }
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
            _ => {}
        }

        Ok(())
    }

    fn handle_message_key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            self.open_compose();
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

                if let Some(starred) = starred {
                    if let Some(view) = self.message_view.as_mut() {
                        if view.message_id == id {
                            view.message.starred = starred;
                        }
                    }
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

                if let Some(important) = important {
                    if let Some(view) = self.message_view.as_mut() {
                        if view.message_id == id {
                            view.message.important = important;
                        }
                    }
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

                if let Some(important) = important {
                    if let Some(view) = self.message_view.as_mut() {
                        if view.message_id == id {
                            view.message.important = important;
                        }
                    }
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
                    if field == ComposeField::Content {
                        let inserted = {
                            let state = compose.field_state_mut(field);
                            match state {
                                FieldStateMut::Text(_) => false,
                                FieldStateMut::Area(area) => area.insert('\n'),
                            }
                        };
                        if inserted {
                            compose.clear_status();
                        }
                    } else {
                        compose.focus_next();
                    }
                    return Ok(());
                }
                KeyCode::Left => {
                    let state = compose.field_state_mut(field);
                    match state {
                        FieldStateMut::Text(state) => {
                            let _ = state.move_left();
                        }
                        FieldStateMut::Area(state) => {
                            let _ = state.move_left();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Right => {
                    let state = compose.field_state_mut(field);
                    match state {
                        FieldStateMut::Text(state) => {
                            let _ = state.move_right();
                        }
                        FieldStateMut::Area(state) => {
                            let _ = state.move_right();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Home => {
                    let state = compose.field_state_mut(field);
                    match state {
                        FieldStateMut::Text(state) => {
                            let _ = state.move_home();
                        }
                        FieldStateMut::Area(state) => {
                            let _ = state.move_home();
                        }
                    }
                    return Ok(());
                }
                KeyCode::End => {
                    let state = compose.field_state_mut(field);
                    match state {
                        FieldStateMut::Text(state) => {
                            let _ = state.move_end();
                        }
                        FieldStateMut::Area(state) => {
                            let _ = state.move_end();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Backspace => {
                    let modified = {
                        let state = compose.field_state_mut(field);
                        match state {
                            FieldStateMut::Text(state) => state.backspace(),
                            FieldStateMut::Area(state) => state.backspace(),
                        }
                    };
                    if modified {
                        compose.clear_status();
                    }
                    return Ok(());
                }
                KeyCode::Delete => {
                    let modified = {
                        let state = compose.field_state_mut(field);
                        match state {
                            FieldStateMut::Text(state) => state.delete(),
                            FieldStateMut::Area(state) => state.delete(),
                        }
                    };
                    if modified {
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

                    let modified = {
                        let state = compose.field_state_mut(field);
                        match state {
                            FieldStateMut::Text(state) => state.insert(ch),
                            FieldStateMut::Area(state) => state.insert(ch),
                        }
                    };
                    if modified {
                        compose.clear_status();
                    }
                    return Ok(());
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
                    compose.set_focus(ComposeFocus::Field(ComposeField::Content));
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
            ComposeButton::Draft => self.save_current_draft(),
            ComposeButton::Send => self.send_current_compose(),
        }
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
            Some(compose) => (compose.draft_id(), compose.to_outgoing()),
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
            Some(compose) => (compose.draft_id(), compose.to_outgoing()),
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

        let body = compose_body_from_content(&content);

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
        let len = self.mailbox.messages.len();
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
        if self.mailbox.messages.is_empty() {
            self.mailbox.selected = None;
        } else {
            self.mailbox.selected = Some(0);
        }
    }

    fn select_last(&mut self) {
        if self.mailbox.messages.is_empty() {
            self.mailbox.selected = None;
        } else {
            self.mailbox.selected = Some(self.mailbox.messages.len() - 1);
        }
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

        self.scheduled_actions
            .push(Action::new(action_type, message_id));
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

        self.scheduled_actions
            .push(Action::new(action_type, message_id));
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

        msg.status = MessageStatus::Archived;
        let message_id = msg.id;

        self.scheduled_actions
            .push(Action::new(ActionType::Archive, message_id));
        self.advance_selection_after_action(idx);
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

        msg.status = MessageStatus::Deleted;
        let message_id = msg.id;

        self.scheduled_actions
            .push(Action::new(ActionType::Delete, message_id));
        self.advance_selection_after_action(idx);
        self.sync_message_view_state();
    }

    fn schedule_move_to_spam(&mut self) {
        let Some(idx) = self.mailbox.selected else {
            return;
        };

        let Some(msg) = self.selected_loaded_message_mut() else {
            self.mailbox
                .status_line
                .get_or_insert_with(|| "Message is still loading.".to_string());
            return;
        };

        msg.status = MessageStatus::Spam;
        let message_id = msg.id;

        self.scheduled_actions
            .push(Action::new(ActionType::MoveToSpam, message_id));
        self.advance_selection_after_action(idx);
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
        msg.status = MessageStatus::PendingInbox;
        let action_type = if restore_unread {
            ActionType::MoveToInboxUnread
        } else {
            ActionType::MoveToInboxRead
        };
        let message_id = msg.id;

        self.scheduled_actions
            .push(Action::new(action_type, message_id));
        self.advance_selection_after_action(idx);
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

        let action_type = match msg.status {
            MessageStatus::New | MessageStatus::Read => {
                msg.status = MessageStatus::New;
                ActionType::MoveToInboxUnread
            }
            MessageStatus::Deleted | MessageStatus::Archived | MessageStatus::Spam => {
                msg.status = MessageStatus::Read;
                ActionType::MoveToInboxRead
            }
            MessageStatus::PendingInbox => {
                self.mailbox.status_line =
                    Some("Message already scheduled to move to inbox.".to_string());
                return;
            }
        };
        let message_id = msg.id;

        self.scheduled_actions
            .push(Action::new(action_type, message_id));
        self.mailbox.status_line = None;
        self.sync_message_view_state();
    }

    fn advance_selection_after_action(&mut self, current_idx: usize) {
        if self.mailbox.messages.is_empty() {
            self.mailbox.selected = None;
            return;
        }
        let mut next_idx = current_idx + 1;
        if next_idx >= self.mailbox.messages.len() {
            next_idx = self.mailbox.messages.len().saturating_sub(1);
        }

        let candidate = next_loaded_index(&self.mailbox.messages, next_idx)
            .or_else(|| previous_loaded_index(&self.mailbox.messages, current_idx));

        self.mailbox.selected = candidate.or_else(|| last_loaded_index(&self.mailbox.messages));
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

        Ok(())
    }

    fn open_selected_message(&mut self) -> Result<()> {
        let idx = match self.mailbox.selected {
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

        self.message_view = Some(MessageViewState {
            message_id: message.id,
            message_index: idx,
            message,
            content,
            document,
            raw_html,
            scroll: 0,
            unformatted: false,
            info_line: None,
        });

        Ok(())
    }

    fn open_adjacent_message(&mut self, offset: isize) -> Result<()> {
        let Some(current) = self.message_view.as_ref() else {
            return Ok(());
        };
        let len = self.mailbox.messages.len() as isize;
        if len == 0 {
            return Ok(());
        }
        let next_index = current.message_index as isize + offset;
        if next_index < 0 || next_index >= len {
            return Ok(());
        }
        self.mailbox.selected = Some(next_index as usize);
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
                important: false,
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
            important: message.important,
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
    pub(crate) important: bool,
}

impl App {
    fn normalize_scroll(&mut self) {
        let len = self.mailbox.messages.len();
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
