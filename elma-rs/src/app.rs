//! Core application state and controller logic.
//!
//! [`App`] encapsulates all user interface state (selected message, scheduled
//! actions, progress indicators) and synchronises with the configured backend.
//! The type is intentionally synchronous from the TUI's perspective yet internally
//! manages asynchronous commit results via channels so the UI thread never blocks.

use crate::backend::{ActionStatus, BackendEvent, MailBackend};
use crate::model::{
    Action, ActionType, MailboxKind, Message, MessageContent, MessageId, MessageStatus,
    format_size, padded_sender,
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
        if self.message_view.is_some() {
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

        if self.process_pending_shortcut(key)? {
            return Ok(());
        }

        if self.pending_shortcut.is_none()
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
        {
            self.open_go_to_menu();
            return Ok(());
        }

        match self.active_view() {
            ActiveView::Mailbox => self.handle_mailbox_key(key),
            ActiveView::Message => self.handle_message_key(key),
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
                    if let Some(position) =
                        self.mailbox.messages.iter().position(|msg| msg.id == id)
                    {
                        self.mailbox.messages.remove(position);
                        self.mailbox.event_count += 1;

                        if let Some(selected) = self.mailbox.selected {
                            if self.mailbox.messages.is_empty() {
                                self.mailbox.selected = None;
                            } else if selected >= self.mailbox.messages.len() {
                                self.mailbox.selected = Some(self.mailbox.messages.len() - 1);
                            } else if position <= selected && selected > 0 {
                                self.mailbox.selected = Some(selected.saturating_sub(1));
                            }
                        }

                        refresh = true;
                    }

                    if let Some(view) = &self.message_view {
                        if view.message_id == id {
                            self.message_view = None;
                        }
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
        let mut text = String::from("^Q:Quit g:GoTo");

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
            KeyCode::Char('$') => self.commit_actions()?,
            KeyCode::Enter | KeyCode::Right => self.open_selected_message()?,
            KeyCode::Char('d') | KeyCode::Char('D') => self.schedule_delete(),
            KeyCode::Char('#') => self.schedule_delete(),
            KeyCode::Backspace | KeyCode::Delete => self.schedule_delete(),
            _ => {}
        }

        Ok(())
    }

    fn handle_message_key(&mut self, key: KeyEvent) -> Result<()> {
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
                self.scheduled_actions.push(Action::new(action_type, msg.id));
                self.advance_selection_after_action(idx);
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
                    MessageStatus::Deleted | MessageStatus::Archived => {
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
    fn commit_actions(&mut self) -> Result<()> {
        if self.scheduled_actions.is_empty() {
            return Ok(());
        }

        let actions = std::mem::take(&mut self.scheduled_actions);
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
                self.scheduled_actions.extend(actions);

                if let Some(progress) = self.commit_progress.as_mut() {
                    progress.total = progress.total.saturating_sub(action_count);
                    progress.completed = progress.completed.min(progress.total);
                    if progress.total == 0 {
                        self.commit_progress = None;
                    }
                }

                return Err(err.context("failed to queue actions with backend"));
            }
        };

        self.commit_batches
            .push_back(CommitBatchState::new(actions, receiver));

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
