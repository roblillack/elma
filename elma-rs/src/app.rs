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
    sync::mpsc::{Receiver, TryRecvError},
};
use tdoc::{Document, html};
use time::OffsetDateTime;

const PAGE_JUMP: isize = 5;
const PROGRESS_SEGMENTS: usize = 5;

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

/// Which screen the UI is currently rendering.
pub enum ActiveView {
    Mailbox,
    Message,
    Compose,
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
    backend: Box<dyn MailBackend>,
    mailbox: MailboxState,
    message_view: Option<MessageViewState>,
    compose: Option<ComposeState>,
    should_quit: bool,
    commit_batches: VecDeque<CommitBatchState>,
    commit_progress: Option<CommitProgress>,
    scheduled_actions: Vec<Action>,
    current_mailbox: MailboxKind,
    pending_shortcut: Option<ShortcutMenuState>,
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

#[derive(Clone, Copy)]
struct ShortcutItem {
    key: char,
    description: &'static str,
    action: ShortcutAction,
}

#[derive(Clone, Copy)]
enum ShortcutAction {
    SwitchMailbox(MailboxKind),
}

#[derive(Clone, Copy)]
pub(crate) struct ShortcutEntry {
    pub(crate) key: char,
    pub(crate) description: &'static str,
}

impl ShortcutMenuState {
    fn go_to_menu() -> Self {
        let items = vec![
            ShortcutItem::new('i', "Inbox", MailboxKind::Inbox),
            ShortcutItem::new('s', "Starred", MailboxKind::Starred),
            ShortcutItem::new('t', "Sent", MailboxKind::Sent),
            ShortcutItem::new('d', "Drafts", MailboxKind::Drafts),
            ShortcutItem::new('a', "Archive", MailboxKind::Archive),
            ShortcutItem::new('S', "Spam", MailboxKind::Spam),
            ShortcutItem::new('T', "Trash", MailboxKind::Trash),
        ];

        Self {
            menu: ShortcutMenu {
                title: "Go to",
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

    pub(crate) fn entries(&self) -> impl Iterator<Item = ShortcutEntry> + '_ {
        self.items.iter().map(|item| ShortcutEntry {
            key: item.key(),
            description: item.description(),
        })
    }
}

impl ShortcutItem {
    const fn new(key: char, description: &'static str, mailbox: MailboxKind) -> Self {
        Self {
            key,
            description,
            action: ShortcutAction::SwitchMailbox(mailbox),
        }
    }

    fn matches(&self, key: char) -> bool {
        self.key == key
    }

    fn key(&self) -> char {
        self.key
    }

    fn description(&self) -> &'static str {
        self.description
    }
}
impl App {
    /// Build the application state around the provided backend.
    pub fn new(backend: Box<dyn MailBackend>) -> Result<Self> {
        let (messages, events) = backend
            .load_inbox()
            .context("failed to load inbox from backend")?;

        let mut sorted = messages;
        sorted.sort_by_key(|msg| msg.sent);
        let selected = if sorted.is_empty() {
            None
        } else {
            Some(sorted.len().saturating_sub(1))
        };

        let mailbox = MailboxState {
            messages: sorted,
            selected,
            events,
            event_count: 0,
            status_line: None,
            scroll_top: 0,
        };

        Ok(Self {
            backend,
            mailbox,
            message_view: None,
            compose: None,
            should_quit: false,
            commit_batches: VecDeque::new(),
            commit_progress: None,
            scheduled_actions: Vec::new(),
            current_mailbox: MailboxKind::Inbox,
            pending_shortcut: None,
        })
    }

    /// Whether the main loop should terminate.
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Determine which view should be rendered by the UI.
    pub fn active_view(&self) -> ActiveView {
        if self.compose.is_some() {
            ActiveView::Compose
        } else if self.message_view.is_some() {
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
            && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
        {
            self.open_go_to_menu();
            return Ok(());
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
                self.mailbox.status_line = Some("Shortcut cancelled.".to_string());
                Ok(true)
            }
            KeyCode::Char(ch) => {
                let action = state.action_for(ch);
                self.pending_shortcut = None;
                match action {
                    Some(ShortcutAction::SwitchMailbox(target)) => {
                        self.switch_mailbox(target)?;
                    }
                    None => {
                        self.mailbox.status_line = Some(format!("Unknown go to target: {ch}"));
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

    fn open_go_to_menu(&mut self) {
        self.pending_shortcut = Some(ShortcutMenuState::go_to_menu());
        self.mailbox.status_line = Some("Go to: press the highlighted key.".to_string());
    }

    fn switch_mailbox(&mut self, target: MailboxKind) -> Result<()> {
        if target == self.current_mailbox {
            self.mailbox.status_line = Some(format!("Already viewing {target}."));
            return Ok(());
        }

        let (mut messages, events) = self
            .backend
            .load_mailbox(target)
            .with_context(|| format!("failed to load {target} mailbox"))?;
        messages.sort_by_key(|msg| msg.sent);
        let selected = if messages.is_empty() {
            None
        } else {
            Some(messages.len().saturating_sub(1))
        };

        self.mailbox = MailboxState {
            messages,
            selected,
            events,
            event_count: 0,
            status_line: Some(format!("Opened {target}.")),
            scroll_top: 0,
        };

        self.current_mailbox = target;
        self.message_view = None;
        self.normalize_scroll();
        Ok(())
    }

    pub(crate) fn shortcut_menu(&self) -> Option<&ShortcutMenu> {
        self.pending_shortcut.as_ref().map(|state| state.menu())
    }

    fn remove_message_from_mailbox(&mut self, id: MessageId) -> bool {
        let position = match self.mailbox.messages.iter().position(|msg| msg.id == id) {
            Some(pos) => pos,
            None => return false,
        };

        self.mailbox.messages.remove(position);

        if let Some(selected) = self.mailbox.selected {
            if self.mailbox.messages.is_empty() {
                self.mailbox.selected = None;
            } else if selected >= self.mailbox.messages.len() {
                self.mailbox.selected = Some(self.mailbox.messages.len() - 1);
            } else if position <= selected && selected > 0 {
                self.mailbox.selected = Some(selected.saturating_sub(1));
            }
        }

        self.normalize_scroll();

        let should_close = self
            .message_view
            .as_ref()
            .map(|view| view.message_id == id)
            .unwrap_or(false);
        if should_close {
            self.message_view = None;
        }

        true
    }

    /// Drain backend event channels and merge them into local state.
    pub fn poll_backend_events(&mut self) {
        self.poll_commit_updates();

        let mut resort = false;
        let mut refresh = false;
        let current_id = self.message_view.as_ref().map(|view| view.message_id);

        loop {
            match self.mailbox.events.try_recv() {
                Ok(BackendEvent::NewMessage(message)) => {
                    self.mailbox.event_count += 1;
                    self.mailbox.messages.push(message);
                    resort = true;
                    refresh = true;
                }
                Ok(BackendEvent::MessageFlagsChanged(message)) => {
                    if let Some(existing) = self
                        .mailbox
                        .messages
                        .iter_mut()
                        .find(|msg| msg.id == message.id)
                    {
                        *existing = message;
                        self.mailbox.event_count += 1;
                        refresh = true;
                    }
                }
                Ok(BackendEvent::MessageDeleted(id)) => {
                    if self.remove_message_from_mailbox(id) {
                        self.mailbox.event_count += 1;
                        refresh = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if resort {
            self.mailbox.messages.sort_by_key(|msg| msg.sent);
        }

        if refresh {
            self.update_selection_after_refresh(current_id);
        }
    }

    /// Poll every active commit batch for recently completed actions.
    fn poll_commit_updates(&mut self) {
        if self.commit_batches.is_empty() {
            return;
        }

        for batch in &mut self.commit_batches {
            loop {
                match batch.receiver.try_recv() {
                    Ok(status) => {
                        if let Some(progress) = self.commit_progress.as_mut() {
                            progress.completed =
                                progress.completed.saturating_add(1).min(progress.total);
                        }

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

    pub(crate) fn inbox_action_bar(&self) -> String {
        let mut text = String::from("^Q:Quit g:GoTo c:Compose");

        if let Some(idx) = self.mailbox.selected {
            if let Some(msg) = self.mailbox.messages.get(idx) {
                text.push_str(" Enter:Open");
                if msg.starred {
                    text.push_str(" s:Unstar");
                } else {
                    text.push_str(" s:Star");
                }

                let in_archive = self.current_mailbox == MailboxKind::Archive;
                let in_trash = self.current_mailbox == MailboxKind::Trash;
                let in_spam = self.current_mailbox == MailboxKind::Spam;
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
        let progress = self.commit_progress.as_ref()?;
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
            "{} — message {selected}/{total}, {} scheduled actions, got {} events",
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
                if let Some(view) = self.message_view.as_mut() {
                    if view.message_id == id {
                        if let Some(msg) = self
                            .mailbox
                            .messages
                            .iter()
                            .find(|message| message.id == id)
                        {
                            view.message.starred = msg.starred;
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
            KeyCode::Char('j') | KeyCode::Down => {
                self.open_adjacent_message(1)?;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.open_adjacent_message(-1)?;
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
        if let Some(idx) = self.mailbox.selected {
            if let Some(msg) = self.mailbox.messages.get_mut(idx) {
                msg.starred = !msg.starred;
                let ty = if msg.starred {
                    ActionType::MarkAsStarred
                } else {
                    ActionType::MarkAsUnstarred
                };
                self.scheduled_actions.push(Action::new(ty, msg.id));
                self.mailbox.status_line = None;
                self.sync_message_view_state();
            }
        }
    }

    fn schedule_archive(&mut self) {
        if self.current_mailbox == MailboxKind::Archive {
            self.schedule_move_to_inbox();
            return;
        }

        if let Some(idx) = self.mailbox.selected {
            if let Some(msg) = self.mailbox.messages.get_mut(idx) {
                msg.status = MessageStatus::Archived;
                self.scheduled_actions
                    .push(Action::new(ActionType::Archive, msg.id));
                self.advance_selection_after_action(idx);
                self.sync_message_view_state();
            }
        }
    }

    fn schedule_delete(&mut self) {
        if self.current_mailbox == MailboxKind::Trash {
            self.schedule_move_to_inbox();
            return;
        }

        if let Some(idx) = self.mailbox.selected {
            if let Some(msg) = self.mailbox.messages.get_mut(idx) {
                msg.status = MessageStatus::Deleted;
                self.scheduled_actions
                    .push(Action::new(ActionType::Delete, msg.id));
                self.advance_selection_after_action(idx);
                self.sync_message_view_state();
            }
        }
    }

    fn schedule_move_to_spam(&mut self) {
        if let Some(idx) = self.mailbox.selected {
            if let Some(msg) = self.mailbox.messages.get_mut(idx) {
                msg.status = MessageStatus::Spam;
                self.scheduled_actions
                    .push(Action::new(ActionType::MoveToSpam, msg.id));
                self.advance_selection_after_action(idx);
                self.mailbox.status_line = None;
                self.sync_message_view_state();
            }
        }
    }

    fn schedule_move_to_inbox(&mut self) {
        if let Some(idx) = self.mailbox.selected {
            if let Some(msg) = self.mailbox.messages.get_mut(idx) {
                let restore_unread = matches!(msg.status, MessageStatus::New);
                msg.status = MessageStatus::PendingInbox;
                let action_type = if restore_unread {
                    ActionType::MoveToInboxUnread
                } else {
                    ActionType::MoveToInboxRead
                };
                self.scheduled_actions
                    .push(Action::new(action_type, msg.id));
                self.advance_selection_after_action(idx);
                self.mailbox.status_line = None;
                self.sync_message_view_state();
            }
        }
    }

    fn toggle_unread(&mut self) {
        if let Some(idx) = self.mailbox.selected {
            if let Some(msg) = self.mailbox.messages.get_mut(idx) {
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
                self.scheduled_actions
                    .push(Action::new(action_type, msg.id));
                self.mailbox.status_line = None;
                self.sync_message_view_state();
            }
        }
    }

    fn advance_selection_after_action(&mut self, current_idx: usize) {
        if self.mailbox.messages.is_empty() {
            self.mailbox.selected = None;
            return;
        }
        let next = min(
            current_idx + 1,
            self.mailbox.messages.len().saturating_sub(1),
        );
        self.mailbox.selected = Some(next);
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

        let message = self
            .mailbox
            .messages
            .get(idx)
            .cloned()
            .ok_or_else(|| anyhow!("no message selected"))?;

        let content = self
            .backend
            .load_message(message.id)
            .with_context(|| format!("failed to load message {}", message.id))?;

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
        if let Some(view) = self.message_view.as_mut() {
            if let Some((idx, message)) = self
                .mailbox
                .messages
                .iter()
                .enumerate()
                .find(|(_, msg)| msg.id == view.message_id)
            {
                view.message_index = idx;
                view.message = message.clone();
            } else {
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
            self.mailbox.selected = Some(self.mailbox.messages.len() - 1);
        }

        self.sync_message_view_state();
        self.normalize_scroll();
    }

    pub(crate) fn formatted_message_row(
        &self,
        message: &Message,
        now: OffsetDateTime,
    ) -> MessageRow {
        let display_name = if self.current_mailbox == MailboxKind::Sent {
            message.recipients_display()
        } else {
            message.sender.clone()
        };

        MessageRow {
            flags: message.flag_string(),
            date: message.formatted_received(now),
            sender: padded_sender(&display_name),
            size: format_size(message.size),
            subject: format_subject(message),
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
    pub(crate) status: MessageStatus,
    pub(crate) starred: bool,
}

fn format_subject(message: &Message) -> String {
    let labels = if message.labels.is_empty() {
        String::new()
    } else {
        format!("{} ", message.labels.join("+"))
    };
    format!("{} {}{}", message.uid, labels, message.subject)
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
