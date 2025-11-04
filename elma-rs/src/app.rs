use crate::backend::{BackendEvent, MailBackend};
use crate::model::{
    Action, ActionType, Message, MessageContent, MessageId, MessageStatus, format_size,
    padded_sender,
};
use anyhow::{Context, Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{
    cmp::{max, min},
    io::Cursor,
    sync::mpsc::{Receiver, TryRecvError},
};
use tdoc::{Document, html};
use time::OffsetDateTime;

const PAGE_JUMP: isize = 5;

pub enum ActiveView {
    Inbox,
    Message,
}

pub struct App {
    backend: Box<dyn MailBackend>,
    inbox: InboxState,
    message_view: Option<MessageViewState>,
    should_quit: bool,
}

struct InboxState {
    messages: Vec<Message>,
    selected: Option<usize>,
    scheduled: Vec<Action>,
    events: Receiver<BackendEvent>,
    event_count: usize,
    status_line: Option<String>,
    scroll_top: usize,
}

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

impl App {
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

        let inbox = InboxState {
            messages: sorted,
            selected,
            scheduled: Vec::new(),
            events,
            event_count: 0,
            status_line: None,
            scroll_top: 0,
        };

        Ok(Self {
            backend,
            inbox,
            message_view: None,
            should_quit: false,
        })
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn active_view(&self) -> ActiveView {
        if self.message_view.is_some() {
            ActiveView::Message
        } else {
            ActiveView::Inbox
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        self.poll_backend_events();

        match self.active_view() {
            ActiveView::Inbox => self.handle_inbox_key(key),
            ActiveView::Message => self.handle_message_key(key),
        }
    }

    pub fn poll_backend_events(&mut self) {
        let mut resort = false;
        let mut refresh = false;
        let current_id = self.message_view.as_ref().map(|view| view.message_id);

        loop {
            match self.inbox.events.try_recv() {
                Ok(BackendEvent::NewMessage(message)) => {
                    self.inbox.event_count += 1;
                    self.inbox.messages.push(message);
                    resort = true;
                    refresh = true;
                }
                Ok(BackendEvent::MessageFlagsChanged(message)) => {
                    if let Some(existing) = self
                        .inbox
                        .messages
                        .iter_mut()
                        .find(|msg| msg.id == message.id)
                    {
                        *existing = message;
                        self.inbox.event_count += 1;
                        refresh = true;
                    }
                }
                Ok(BackendEvent::MessageDeleted(id)) => {
                    if let Some(position) = self.inbox.messages.iter().position(|msg| msg.id == id)
                    {
                        self.inbox.messages.remove(position);
                        self.inbox.event_count += 1;

                        if let Some(selected) = self.inbox.selected {
                            if self.inbox.messages.is_empty() {
                                self.inbox.selected = None;
                            } else if selected >= self.inbox.messages.len() {
                                self.inbox.selected = Some(self.inbox.messages.len() - 1);
                            } else if position <= selected && selected > 0 {
                                self.inbox.selected = Some(selected.saturating_sub(1));
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
            self.inbox.messages.sort_by_key(|msg| msg.sent);
        }

        if refresh {
            self.update_selection_after_refresh(current_id);
        }
    }

    pub fn on_resize(&mut self) {
        if let Some(view) = &mut self.message_view {
            view.scroll = 0;
        }
    }

    pub(crate) fn inbox_messages(&self) -> &[Message] {
        &self.inbox.messages
    }

    pub(crate) fn inbox_selected(&self) -> Option<usize> {
        self.inbox.selected
    }

    pub(crate) fn inbox_action_bar(&self) -> String {
        let mut text = String::from("^Q:Quit");

        if let Some(idx) = self.inbox.selected {
            if let Some(msg) = self.inbox.messages.get(idx) {
                text.push_str(" Enter:Open");
                if msg.starred {
                    text.push_str(" s:Unstar");
                } else {
                    text.push_str(" s:Star");
                }

                match msg.status {
                    MessageStatus::New | MessageStatus::Read => {
                        text.push_str(" r:Reply y:Archive d:Delete");
                    }
                    MessageStatus::Deleted => text.push_str(" r:Reply y:Archive u:Undelete"),
                    MessageStatus::Archived => text.push_str(" r:Reply u:Unarchive d:Delete"),
                }
            }
        }

        if !self.inbox.scheduled.is_empty() {
            text.push_str(" $:Commit");
        }

        text
    }

    pub(crate) fn inbox_info_bar(&self) -> String {
        let total = self.inbox.messages.len();
        let selected = self
            .inbox
            .selected
            .map(|idx| format!("{}", idx + 1))
            .unwrap_or_else(|| "-".to_string());
        format!(
            "Message {selected}/{total}, {} scheduled actions, got {} events",
            self.inbox.scheduled.len(),
            self.inbox.event_count
        )
    }

    pub(crate) fn inbox_status_line(&self) -> Option<&str> {
        self.inbox.status_line.as_deref()
    }

    pub(crate) fn inbox_scroll_top(&self) -> usize {
        self.inbox.scroll_top
    }

    pub(crate) fn set_inbox_scroll_top(&mut self, value: usize) {
        self.inbox.scroll_top = value;
    }

    pub(crate) fn message_view(&self) -> Option<&MessageViewState> {
        self.message_view.as_ref()
    }

    fn handle_inbox_key(&mut self, key: KeyEvent) -> Result<()> {
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
                        if let Some(msg) =
                            self.inbox.messages.iter().find(|message| message.id == id)
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
        let len = self.inbox.messages.len();
        if len == 0 {
            self.inbox.selected = None;
            return;
        }
        let current = self.inbox.selected.unwrap_or(len.saturating_sub(1)) as isize;
        let max_index = len as isize - 1;
        let next = min(max(0, current + delta), max_index) as usize;
        self.inbox.selected = Some(next);
    }

    fn select_first(&mut self) {
        if self.inbox.messages.is_empty() {
            self.inbox.selected = None;
        } else {
            self.inbox.selected = Some(0);
        }
    }

    fn select_last(&mut self) {
        if self.inbox.messages.is_empty() {
            self.inbox.selected = None;
        } else {
            self.inbox.selected = Some(self.inbox.messages.len() - 1);
        }
    }

    fn toggle_star(&mut self) {
        if let Some(idx) = self.inbox.selected {
            if let Some(msg) = self.inbox.messages.get_mut(idx) {
                msg.starred = !msg.starred;
                let ty = if msg.starred {
                    ActionType::MarkAsStarred
                } else {
                    ActionType::MarkAsUnstarred
                };
                self.inbox.scheduled.push(Action::new(ty, msg.id));
                self.inbox.status_line = None;
                self.sync_message_view_state();
            }
        }
    }

    fn schedule_archive(&mut self) {
        if let Some(idx) = self.inbox.selected {
            if let Some(msg) = self.inbox.messages.get_mut(idx) {
                msg.status = MessageStatus::Archived;
                self.inbox
                    .scheduled
                    .push(Action::new(ActionType::Archive, msg.id));
                self.advance_selection_after_action(idx);
                self.sync_message_view_state();
            }
        }
    }

    fn schedule_delete(&mut self) {
        if let Some(idx) = self.inbox.selected {
            if let Some(msg) = self.inbox.messages.get_mut(idx) {
                msg.status = MessageStatus::Deleted;
                self.inbox
                    .scheduled
                    .push(Action::new(ActionType::Delete, msg.id));
                self.advance_selection_after_action(idx);
                self.sync_message_view_state();
            }
        }
    }

    fn toggle_unread(&mut self) {
        if let Some(idx) = self.inbox.selected {
            if let Some(msg) = self.inbox.messages.get_mut(idx) {
                let action_type = match msg.status {
                    MessageStatus::New | MessageStatus::Read => {
                        msg.status = MessageStatus::New;
                        ActionType::MoveToInboxUnread
                    }
                    MessageStatus::Deleted | MessageStatus::Archived => {
                        msg.status = MessageStatus::Read;
                        ActionType::MoveToInboxRead
                    }
                };
                self.inbox.scheduled.push(Action::new(action_type, msg.id));
                self.inbox.status_line = None;
                self.sync_message_view_state();
            }
        }
    }

    fn advance_selection_after_action(&mut self, current_idx: usize) {
        if self.inbox.messages.is_empty() {
            self.inbox.selected = None;
            return;
        }
        let next = min(current_idx + 1, self.inbox.messages.len().saturating_sub(1));
        self.inbox.selected = Some(next);
    }

    fn commit_actions(&mut self) -> Result<()> {
        if self.inbox.scheduled.is_empty() {
            return Ok(());
        }

        let actions = std::mem::take(&mut self.inbox.scheduled);
        let mut failed = Vec::new();
        for action in &actions {
            if let Err(err) = self.backend.apply_action(action) {
                failed.push((action.message_id, err));
            }
        }

        if failed.is_empty() {
            self.inbox.messages.retain(|msg| {
                msg.status != MessageStatus::Archived && msg.status != MessageStatus::Deleted
            });
            self.inbox.status_line = Some("Actions committed.".to_string());
        } else {
            let summary = format!("Failed to apply {} actions.", failed.len());
            self.inbox.status_line = Some(summary);
            self.inbox.scheduled.extend(
                actions
                    .into_iter()
                    .filter(|action| failed.iter().any(|(id, _)| *id == action.message_id)),
            );
        }

        self.sync_message_view_state();
        if let Some(idx) = self.inbox.selected {
            if idx >= self.inbox.messages.len() && !self.inbox.messages.is_empty() {
                self.inbox.selected = Some(self.inbox.messages.len() - 1);
            }
        }

        if self.inbox.messages.is_empty() {
            self.inbox.selected = None;
            self.message_view = None;
        }

        self.normalize_scroll();

        Ok(())
    }

    fn open_selected_message(&mut self) -> Result<()> {
        let idx = match self.inbox.selected {
            Some(idx) => idx,
            None => return Ok(()),
        };

        let message = self
            .inbox
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
        let len = self.inbox.messages.len() as isize;
        if len == 0 {
            return Ok(());
        }
        let next_index = current.message_index as isize + offset;
        if next_index < 0 || next_index >= len {
            return Ok(());
        }
        self.inbox.selected = Some(next_index as usize);
        self.open_selected_message()
    }

    fn sync_message_view_state(&mut self) {
        if let Some(view) = self.message_view.as_mut() {
            if let Some((idx, message)) = self
                .inbox
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
        if self.inbox.messages.is_empty() {
            self.inbox.selected = None;
            self.message_view = None;
            self.inbox.scroll_top = 0;
            return;
        }

        if let Some(id) = current_id {
            if let Some((idx, _)) = self
                .inbox
                .messages
                .iter()
                .enumerate()
                .find(|(_, msg)| msg.id == id)
            {
                self.inbox.selected = Some(idx);
            }
        } else if self.inbox.selected.is_none() {
            self.inbox.selected = Some(self.inbox.messages.len() - 1);
        }

        self.sync_message_view_state();
        self.normalize_scroll();
    }

    pub(crate) fn formatted_message_row(
        &self,
        message: &Message,
        now: OffsetDateTime,
    ) -> MessageRow {
        MessageRow {
            flags: message.flag_string(),
            date: message.formatted_received(now),
            sender: padded_sender(&message.sender),
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
        let len = self.inbox.messages.len();
        if len == 0 {
            self.inbox.scroll_top = 0;
        } else {
            let max_top = len.saturating_sub(1);
            if self.inbox.scroll_top > max_top {
                self.inbox.scroll_top = max_top;
            }
        }
    }
}
