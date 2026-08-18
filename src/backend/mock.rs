//! Mock backend used for demo mode and manual testing.
//!
//! This implementation keeps all data in-memory and simulates asynchronous behaviour
//! by introducing small random delays when applying actions.  The goal is to exercise
//! the UI logic without relying on an external mail provider.

use crate::{
    backend::{ActionStatus, BackendEvent, MailBackend, MailboxSnapshot, OutgoingMessage},
    model::{
        Action, ActionType, MailboxKind, Message, MessageAttachment, MessageContent,
        MessageContentPart, MessageId, MessageStatus,
    },
};
use anyhow::{Result, anyhow};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, SystemTime},
};
use time::{Duration as TimeDuration, OffsetDateTime};

const INITIAL_MESSAGE_COUNT: usize = 250;
/// How long the worker sits on an action before applying it, mimicking the
/// round trips a real backend incurs.  Tests build a backend without it.
const ACTION_DELAY_MS: (u64, u64) = (50, 500);
const MAILER_NAME: &str = "MockMailer/tdoc-demo";
const DEFAULT_SENDER: &str = "user@mock.example";
/// The one part that is a file without being an attachment.
const INLINE_TEMPLATE: (&str, &str) = ("signature-logo.png", "image/png");

const ATTACHMENT_TEMPLATES: &[(&str, &str)] = &[
    ("proposal.pdf", "application/pdf"),
    ("diagram.png", "image/png"),
    (
        "report.xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("notes.txt", "text/plain"),
    (
        "presentation.pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("archive.zip", "application/zip"),
];

/// An action paired with the channel it should report completion on.
struct WorkItem {
    action: Action,
    result_tx: Sender<ActionStatus>,
}

/// Simple in-memory backend for demos and integration tests.
///
/// Messages are generated deterministically at startup so the UI can exercise the
/// same flows as the Gmail backend without network access.
pub struct MockBackend {
    mailboxes: Arc<Mutex<HashMap<MailboxKind, Vec<MockMessage>>>>,
    contents: Arc<Mutex<HashMap<MessageId, MessageContent>>>,
    event_sender: Arc<Mutex<Option<Sender<BackendEvent>>>>,
    id_counter: Arc<AtomicU64>,
    work_queue: Arc<(Mutex<VecDeque<WorkItem>>, Condvar)>,
}

#[derive(Clone)]
struct MockMessage {
    message: Message,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::demo()
    }
}

impl MockBackend {
    /// Create a mock backend configured for the CLI demo mode.
    ///
    /// The builder loads a stock set of messages, spins up a background thread that
    /// periodically injects new mail, and returns immediately so the UI stays
    /// responsive.
    pub fn demo() -> Self {
        let mailboxes = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut guard = mailboxes.lock().expect("mailboxes mutex poisoned");
            for kind in MailboxKind::ALL {
                guard.insert(kind, Vec::new());
            }
        }
        let contents = Arc::new(Mutex::new(HashMap::new()));
        let event_sender = Arc::new(Mutex::new(None));
        let id_counter = Arc::new(AtomicU64::new(0));
        let work_queue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));

        let backend = Self {
            mailboxes: Arc::clone(&mailboxes),
            contents: Arc::clone(&contents),
            event_sender: Arc::clone(&event_sender),
            id_counter: Arc::clone(&id_counter),
            work_queue: Arc::clone(&work_queue),
        };

        backend.populate_initial_mailboxes(INITIAL_MESSAGE_COUNT);
        backend.spawn_incoming_mail_generator(mailboxes, contents, event_sender, id_counter);
        backend.spawn_action_worker(work_queue, Some(ACTION_DELAY_MS));
        backend
    }

    #[cfg(test)]
    /// A backend holding no mail and making no noise: no randomised inbox, no
    /// generator thread injecting new messages, and no artificial delay before
    /// an action is applied.
    ///
    /// Everything else is what demo mode runs -- the same work queue, the same
    /// worker thread, the same action semantics -- so a test driving this is
    /// driving the real thing, just deterministically and without the wait.
    pub(crate) fn empty() -> Self {
        let mailboxes = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut guard = mailboxes.lock().expect("mailboxes mutex poisoned");
            for kind in MailboxKind::ALL {
                guard.insert(kind, Vec::new());
            }
        }
        let work_queue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));

        let backend = Self {
            mailboxes,
            contents: Arc::new(Mutex::new(HashMap::new())),
            event_sender: Arc::new(Mutex::new(None)),
            id_counter: Arc::new(AtomicU64::new(0)),
            work_queue: Arc::clone(&work_queue),
        };

        backend.spawn_action_worker(work_queue, None);
        backend
    }

    #[cfg(test)]
    /// Put a message into a mailbox, for tests that need something to act on.
    pub(crate) fn insert(&self, mailbox: MailboxKind, message: Message) {
        let mut mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
        let list = mailboxes.entry(mailbox).or_default();
        list.push(MockMessage { message });
        list.sort_by_key(|mock| mock.message.sent);
    }

    #[cfg(test)]
    /// The messages a mailbox holds, in the order the UI would list them.
    pub(crate) fn messages_in(&self, mailbox: MailboxKind) -> Vec<Message> {
        self.mailboxes
            .lock()
            .expect("mailboxes mutex poisoned")
            .get(&mailbox)
            .map(|list| list.iter().map(|mock| mock.message.clone()).collect())
            .unwrap_or_default()
    }

    fn populate_initial_mailboxes(&self, inbox_count: usize) {
        let mut rng = SimpleRng::new(random_seed());
        let mut mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
        let mut contents = self.contents.lock().expect("contents mutex poisoned");

        for _ in 0..inbox_count {
            let id = self.next_id();
            let (message, content) = old_random_message(id, &mut rng);
            contents.insert(id, content);
            mailboxes
                .entry(MailboxKind::Inbox)
                .or_default()
                .push(MockMessage { message });
        }

        let templates = [
            (MailboxKind::Important, 30usize, MessageStatus::Read),
            (MailboxKind::Starred, 40usize, MessageStatus::Read),
            (MailboxKind::Sent, 25usize, MessageStatus::Read),
            (MailboxKind::Drafts, 12usize, MessageStatus::New),
            (MailboxKind::Archive, 35usize, MessageStatus::Read),
            (MailboxKind::Spam, 22usize, MessageStatus::Read),
            (MailboxKind::Trash, 18usize, MessageStatus::Read),
        ];

        for (kind, count, status) in templates {
            let list = mailboxes.entry(kind).or_default();
            for _ in 0..count {
                let id = self.next_id();
                let sent = OffsetDateTime::now_utc()
                    - TimeDuration::hours(rng.gen_range_usize(0..720) as i64)
                    - TimeDuration::minutes(rng.gen_range_usize(0..60) as i64);
                let (mut message, mut content) = new_random_message(id, sent, &mut rng);
                message.status = status;
                match kind {
                    MailboxKind::Starred => {
                        message.starred = true;
                        message.labels = vec!["Starred".to_string()];
                    }
                    MailboxKind::Important => {
                        message.important = true;
                        message.labels = vec!["Important".to_string()];
                    }
                    MailboxKind::Sent => {
                        message.labels = vec!["Sent".to_string()];
                        message.starred = false;
                    }
                    MailboxKind::Drafts => {
                        message.labels = vec!["Draft".to_string()];
                        message.starred = false;
                    }
                    MailboxKind::Archive => {
                        message.labels = vec!["Archive".to_string()];
                        message.starred = rng.one_in(5);
                        if rng.one_in(6) {
                            message.status = MessageStatus::New;
                        }
                    }
                    MailboxKind::Spam => {
                        message.labels = vec!["Spam".to_string()];
                        message.starred = false;
                        if rng.one_in(4) {
                            message.status = MessageStatus::New;
                        }
                    }
                    MailboxKind::Trash => {
                        message.labels = vec!["Trash".to_string()];
                        message.starred = false;
                        if rng.one_in(6) {
                            message.status = MessageStatus::New;
                        }
                    }
                    MailboxKind::Inbox => {}
                }
                if message.important
                    && !message
                        .labels
                        .iter()
                        .any(|label| label.eq_ignore_ascii_case("Important"))
                {
                    message.labels.push("Important".to_string());
                }
                update_mailer(&mut content, message.status);
                contents.insert(id, content);
                list.push(MockMessage { message });
            }
        }

        for list in mailboxes.values_mut() {
            list.sort_by_key(|mock| mock.message.sent);
        }
    }

    fn spawn_incoming_mail_generator(
        &self,
        mailboxes: Arc<Mutex<HashMap<MailboxKind, Vec<MockMessage>>>>,
        contents: Arc<Mutex<HashMap<MessageId, MessageContent>>>,
        event_sender: Arc<Mutex<Option<Sender<BackendEvent>>>>,
        id_counter: Arc<AtomicU64>,
    ) {
        thread::spawn(move || {
            let mut rng = SimpleRng::new(random_seed() ^ 0x9e3779b97f4a7c15);
            loop {
                let sleep_ms = rng.gen_range_usize(350..1200) as u64;
                thread::sleep(Duration::from_millis(sleep_ms));

                let id = id_counter.fetch_add(1, Ordering::SeqCst) + 1;
                let sent = OffsetDateTime::now_utc();
                let (message, content) = new_random_message(id, sent, &mut rng);

                {
                    let mut message_lock = mailboxes.lock().expect("mailboxes mutex poisoned");
                    let mut content_lock = contents.lock().expect("contents mutex poisoned");

                    content_lock.insert(id, content);
                    message_lock
                        .entry(MailboxKind::Inbox)
                        .or_default()
                        .push(MockMessage {
                            message: message.clone(),
                        });
                }

                let sender = {
                    let guard = event_sender.lock().expect("event sender mutex poisoned");
                    guard.clone()
                };

                if let Some(sender) = sender
                    && sender.send(BackendEvent::NewMessage(message)).is_err()
                {
                    let mut guard = event_sender.lock().expect("event sender mutex poisoned");
                    *guard = None;
                }
            }
        });
    }

    fn spawn_action_worker(
        &self,
        work_queue: Arc<(Mutex<VecDeque<WorkItem>>, Condvar)>,
        delay_ms: Option<(u64, u64)>,
    ) {
        let mailboxes = Arc::clone(&self.mailboxes);
        let contents = Arc::clone(&self.contents);

        thread::spawn(move || {
            let mut delay_rng = SimpleRng::new(random_seed() ^ 0xa511f93acb5d7a77);
            loop {
                let item = {
                    let (lock, cvar) = &*work_queue;
                    let mut queue = lock.lock().expect("work queue mutex poisoned");
                    while queue.is_empty() {
                        queue = cvar.wait(queue).expect("work queue condvar poisoned");
                    }
                    queue.pop_front().expect("queue was non-empty")
                };

                if let Some((low, high)) = delay_ms {
                    let delay =
                        delay_rng.gen_range_usize_inclusive(low as usize, high as usize) as u64;
                    thread::sleep(Duration::from_millis(delay));
                }

                let result = MockBackend::apply_action_now(&mailboxes, &contents, &item.action)
                    .map_err(|err| err.to_string());
                // Receiver may have been dropped — ignore send errors.
                let _ = item.result_tx.send(ActionStatus {
                    action: item.action,
                    result,
                });
            }
        });
    }

    fn next_id(&self) -> MessageId {
        self.id_counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Update the mock data structures to reflect a single action.
    ///
    /// This helper runs synchronously inside the worker thread spawned by
    /// [`MailBackend::apply_actions`].  It mutates the shared message and content
    /// collections and mirrors the behaviour of the real Gmail backend closely
    /// enough for UI testing.
    fn apply_action_now(
        mailboxes: &Arc<Mutex<HashMap<MailboxKind, Vec<MockMessage>>>>,
        contents: &Arc<Mutex<HashMap<MessageId, MessageContent>>>,
        action: &Action,
    ) -> Result<()> {
        let mut mailboxes = mailboxes.lock().expect("mailboxes mutex poisoned");
        let mut contents = contents.lock().expect("contents mutex poisoned");

        let mut removed = None;
        for kind in MailboxKind::ALL {
            if let Some(list) = mailboxes.get_mut(&kind)
                && let Some(index) = list
                    .iter()
                    .position(|mock| mock.message.id == action.message_id)
            {
                let mock = list.remove(index);
                removed = Some((kind, mock));
                break;
            }
        }

        let Some((source_kind, mut mock)) = removed else {
            return Err(anyhow!("message {} not found", action.message_id));
        };

        let mut target_kind = source_kind;
        let was_new = matches!(mock.message.status, MessageStatus::New);
        match action.action_type {
            ActionType::Archive => {
                mock.message
                    .labels
                    .retain(|label| !label.eq_ignore_ascii_case("Trash"));
                if !mock
                    .message
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case("Archive"))
                {
                    mock.message.labels.push("Archive".to_string());
                }
                mock.message.status = if was_new {
                    MessageStatus::New
                } else {
                    MessageStatus::Read
                };
                target_kind = MailboxKind::Archive;
            }
            ActionType::Delete => {
                mock.message
                    .labels
                    .retain(|label| !label.eq_ignore_ascii_case("Archive"));
                if !mock
                    .message
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case("Trash"))
                {
                    mock.message.labels.push("Trash".to_string());
                }
                mock.message.status = if was_new {
                    MessageStatus::New
                } else {
                    MessageStatus::Read
                };
                target_kind = MailboxKind::Trash;
            }
            ActionType::MoveToSpam => {
                mock.message.labels.retain(|label| {
                    !label.eq_ignore_ascii_case("Archive")
                        && !label.eq_ignore_ascii_case("Trash")
                        && !label.eq_ignore_ascii_case("Spam")
                });
                mock.message.labels.push("Spam".to_string());
                mock.message.status = if was_new {
                    MessageStatus::New
                } else {
                    MessageStatus::Read
                };
                target_kind = MailboxKind::Spam;
            }
            ActionType::MoveToInboxUnread => {
                mock.message.status = MessageStatus::New;
                mock.message.labels.retain(|label| {
                    !label.eq_ignore_ascii_case("Archive")
                        && !label.eq_ignore_ascii_case("Trash")
                        && !label.eq_ignore_ascii_case("Spam")
                });
                target_kind = MailboxKind::Inbox;
            }
            ActionType::MoveToInboxRead => {
                mock.message.status = MessageStatus::Read;
                mock.message.labels.retain(|label| {
                    !label.eq_ignore_ascii_case("Archive")
                        && !label.eq_ignore_ascii_case("Trash")
                        && !label.eq_ignore_ascii_case("Spam")
                });
                target_kind = MailboxKind::Inbox;
            }
            ActionType::MarkAsRead => {
                mock.message.status = MessageStatus::Read;
            }
            ActionType::MarkAsStarred => {
                mock.message.starred = true;
                if !mock
                    .message
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case("Starred"))
                {
                    mock.message.labels.push("Starred".to_string());
                }
            }
            ActionType::MarkAsUnstarred => {
                mock.message.starred = false;
                mock.message
                    .labels
                    .retain(|label| !label.eq_ignore_ascii_case("Starred"));
            }
            ActionType::MarkAsImportant => {
                mock.message.important = true;
                if !mock
                    .message
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case("Important"))
                {
                    mock.message.labels.push("Important".to_string());
                }
            }
            ActionType::MarkAsUnimportant => {
                mock.message.important = false;
                mock.message
                    .labels
                    .retain(|label| !label.eq_ignore_ascii_case("Important"));
            }
        }

        if let Some(content) = contents.get_mut(&action.message_id) {
            if mock.message.status == MessageStatus::New {
                content.mailer = format!("{MAILER_NAME} (unread)");
            } else {
                content.mailer = MAILER_NAME.to_string();
            }
        }

        if let Some(list) = mailboxes.get_mut(&target_kind) {
            list.push(mock);
            list.sort_by_key(|entry| entry.message.sent);
        } else {
            return Err(anyhow!("mailbox {target_kind:?} not found"));
        }

        Ok(())
    }
}

impl MockBackend {
    fn store_composed_message(
        &self,
        outgoing: OutgoingMessage,
        mailbox: MailboxKind,
        status: MessageStatus,
        label: &'static str,
    ) -> Result<()> {
        let id = self.next_id();
        let sent = OffsetDateTime::now_utc();

        let OutgoingMessage {
            to,
            cc,
            bcc,
            subject,
            text_body,
            html_body,
            attachments,
        } = outgoing;

        let mut recipients = Vec::new();
        recipients.extend(to);
        recipients.extend(cc);
        recipients.extend(bcc);

        let attachments_total: usize = attachments.iter().map(|att| att.size()).sum();
        let size = text_body.len() + html_body.len() + subject.len() + attachments_total;
        let has_attachments = !attachments.is_empty();

        let mut message = Message {
            id,
            sent,
            sender: DEFAULT_SENDER.to_string(),
            recipients,
            subject,
            size,
            starred: false,
            important: false,
            answered: false,
            forwarded: false,
            status,
            labels: Vec::new(),
            uid: id as u32,
            seq: 0,
            has_attachments,
        };

        if !label.is_empty() {
            message.labels.push(label.to_string());
        }

        let mut content_state = MessageContent {
            mailer: format!("{MAILER_NAME} compose"),
            ..Default::default()
        };
        content_state.parts.push(MessageContentPart {
            content_type: "text/plain".to_string(),
            content: text_body.into_bytes(),
        });
        content_state.parts.push(MessageContentPart {
            content_type: "text/html".to_string(),
            content: html_body.into_bytes(),
        });
        for attachment in &attachments {
            content_state.attachments.push(MessageAttachment {
                filename: Some(attachment.filename.clone()),
                mime_type: attachment.mime_type.clone(),
                size: attachment.size(),
                data: Some(attachment.data.clone()),
                blob_id: None,
                // Everything compose attaches is attached outright; nothing
                // here is referenced by the body.
                inline: false,
            });
            content_state.parts.push(MessageContentPart {
                content_type: attachment.mime_type.clone(),
                content: attachment.data.clone(),
            });
        }

        let mut mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
        let mut contents = self.contents.lock().expect("contents mutex poisoned");
        contents.insert(id, content_state);

        let entry = mailboxes.entry(mailbox).or_default();
        entry.push(MockMessage { message });
        entry.sort_by_key(|mock| mock.message.sent);

        Ok(())
    }
}

impl MailBackend for MockBackend {
    /// Return the current mailbox snapshot and subscribe to future events.
    fn load_mailbox(
        &self,
        mailbox: MailboxKind,
    ) -> Result<(MailboxSnapshot, Receiver<BackendEvent>)> {
        let mut messages = {
            let mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
            if mailbox == MailboxKind::Important {
                let mut seen = HashSet::new();
                let mut collected = Vec::new();
                for list in mailboxes.values() {
                    for mock in list {
                        if !mock.message.important {
                            continue;
                        }
                        if seen.insert(mock.message.id) {
                            collected.push(mock.message.clone());
                        }
                    }
                }
                collected
            } else {
                mailboxes
                    .get(&mailbox)
                    .ok_or_else(|| anyhow!("mailbox {mailbox:?} not found"))?
                    .iter()
                    .map(|mock| mock.message.clone())
                    .collect::<Vec<_>>()
            }
        };

        messages.sort_by_key(|msg| msg.sent);
        for (index, message) in messages.iter_mut().enumerate() {
            message.seq = index as u32 + 1;
        }

        let receiver = if mailbox == MailboxKind::Inbox {
            let (sender, receiver) = mpsc::channel();
            {
                let mut guard = self
                    .event_sender
                    .lock()
                    .expect("event sender mutex poisoned");
                *guard = Some(sender);
            }
            receiver
        } else {
            let (_sender, receiver) = mpsc::channel();
            receiver
        };

        let total = messages.len();
        Ok((MailboxSnapshot { total, messages }, receiver))
    }

    /// Fetch the MIME content for an individual message.
    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        let contents = self.contents.lock().expect("contents mutex poisoned");
        contents
            .get(&message_id)
            .cloned()
            .ok_or_else(|| anyhow!("message {message_id} not found"))
    }

    /// Queue actions for the persistent worker thread.
    ///
    /// Actions are appended to the back of the work queue so any previously
    /// submitted or immediate work runs first.  The worker adds jitter (50–500 ms) mimics the round trips that the Gmail backend incurs,
    /// giving the UI a realistic opportunity to render progress.
    fn apply_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>> {
        let (tx, rx) = mpsc::channel();
        let (lock, cvar) = &*self.work_queue;
        let mut queue = lock.lock().expect("work queue mutex poisoned");
        for action in actions {
            queue.push_back(WorkItem {
                action,
                result_tx: tx.clone(),
            });
        }
        cvar.notify_one();
        Ok(rx)
    }

    /// Queue actions at the front so they execute before pending scheduled work.
    fn apply_immediate_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>> {
        let (tx, rx) = mpsc::channel();
        let (lock, cvar) = &*self.work_queue;
        let mut queue = lock.lock().expect("work queue mutex poisoned");
        for (i, action) in actions.into_iter().enumerate() {
            queue.insert(
                i,
                WorkItem {
                    action,
                    result_tx: tx.clone(),
                },
            );
        }
        cvar.notify_one();
        Ok(rx)
    }

    fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        self.store_composed_message(message, MailboxKind::Sent, MessageStatus::Read, "Sent")
    }

    fn save_draft(&self, message: OutgoingMessage) -> Result<()> {
        self.store_composed_message(message, MailboxKind::Drafts, MessageStatus::New, "Draft")
    }
}

fn generate_mock_attachments(rng: &mut SimpleRng) -> Vec<MessageAttachment> {
    let mut attachments = Vec::new();

    if rng.one_in(3) {
        let count = rng.gen_range_usize_inclusive(1, 3);
        for _ in 0..count {
            let (filename, mime_type) =
                ATTACHMENT_TEMPLATES[rng.gen_range_usize(0..ATTACHMENT_TEMPLATES.len())];
            let data = mock_attachment_payload(filename, mime_type, mock_attachment_size(rng));
            attachments.push(MessageAttachment {
                filename: Some(filename.to_string()),
                mime_type: mime_type.to_string(),
                // Always the real byte count: a listed size the save dialog
                // then contradicts by writing a 90-byte file is worse demo
                // data than a small attachment.
                size: data.len(),
                data: Some(data),
                blob_id: None,
                inline: false,
            });
        }
    }

    // The signature logo an HTML mail references as `cid:…`.  It earns the
    // message no `@` in the list, but `S` still offers it -- which is the whole
    // distinction, and worth having in the demo mailbox to look at.
    if rng.one_in(3) {
        let (filename, mime_type) = INLINE_TEMPLATE;
        let data = mock_attachment_payload(filename, mime_type, mock_attachment_size(rng));
        attachments.push(MessageAttachment {
            filename: Some(filename.to_string()),
            mime_type: mime_type.to_string(),
            size: data.len(),
            data: Some(data),
            blob_id: None,
            inline: true,
        });
    }

    attachments
}

/// Sizes are deliberately modest -- every generated message is held in memory,
/// so the multi-megabyte figures the templates suggest would cost hundreds of
/// megabytes for a demo mailbox.
fn mock_attachment_size(rng: &mut SimpleRng) -> usize {
    rng.gen_range_usize(2_000..120_000)
}

/// Filler bytes of exactly `size` length, so what the UI lists is what a save
/// actually writes.
fn mock_attachment_payload(filename: &str, mime_type: &str, size: usize) -> Vec<u8> {
    const FILLER: &[u8] = b"elma mock attachment payload -- not a real file. ";

    let mut data = format!(
        "This is a placeholder payload for the mock attachment '{filename}' ({mime_type}, {size} bytes).\n"
    )
    .into_bytes();
    while data.len() < size {
        let take = (size - data.len()).min(FILLER.len());
        data.extend_from_slice(&FILLER[..take]);
    }
    // ASCII throughout, so truncating a long header cannot split a character.
    data.truncate(size);
    data
}

fn new_random_message(
    id: MessageId,
    sent: OffsetDateTime,
    rng: &mut SimpleRng,
) -> (Message, MessageContent) {
    let subject = generate_subject(rng);
    let sender = generate_sender(rng);
    let recipients = generate_recipients(rng);
    let body = random_body(&sender, &subject, rng);
    let html = format!("<html><body><h1>{subject}</h1>{body}</body></html>");
    let plain = html2text::from_read(html.as_bytes(), 80);

    let mut content = MessageContent::default();
    content.parts.push(MessageContentPart {
        content_type: "text/html".to_string(),
        content: html.as_bytes().to_vec(),
    });
    content.parts.push(MessageContentPart {
        content_type: "text/plain".to_string(),
        content: plain.into_bytes(),
    });

    let attachments = generate_mock_attachments(rng);
    let attachments_bytes = attachments.iter().map(|att| att.size).sum::<usize>();
    // An embedded image is part of how the message reads, so it does not earn
    // the message a marker in the list -- see LeafPart::role.
    let has_attachments = attachments.iter().any(|attachment| !attachment.inline);
    content.attachments = attachments;

    let size = rng.gen_range_usize(0..7_203_680) + 200 + attachments_bytes;

    let important = rng.one_in(6);
    let mut labels = Vec::new();
    if important {
        labels.push("Important".to_string());
    }

    let message = Message {
        id,
        sent,
        sender,
        recipients,
        subject,
        size,
        starred: false,
        important,
        answered: false,
        forwarded: false,
        status: MessageStatus::New,
        labels,
        uid: id as u32,
        seq: 0,
        has_attachments,
    };

    update_mailer(&mut content, message.status);
    (message, content)
}

fn old_random_message(id: MessageId, rng: &mut SimpleRng) -> (Message, MessageContent) {
    let sent = OffsetDateTime::now_utc()
        - TimeDuration::hours(rng.gen_range_usize(0..1000) as i64)
        - TimeDuration::minutes(rng.gen_range_usize(0..60) as i64);

    let (mut message, mut content) = new_random_message(id, sent, rng);
    message.starred = rng.one_in(10);
    message.answered = rng.one_in(7);
    message.forwarded = rng.one_in(25);
    message.status = MessageStatus::Read;
    if rng.one_in(20) {
        message.status = MessageStatus::New;
        message.starred = false;
        message.answered = false;
        message.forwarded = false;
    }
    update_mailer(&mut content, message.status);
    (message, content)
}

fn update_mailer(content: &mut MessageContent, status: MessageStatus) {
    content.mailer = if status == MessageStatus::New {
        format!("{MAILER_NAME} (unread)")
    } else {
        MAILER_NAME.to_string()
    };
}

fn generate_sender(rng: &mut SimpleRng) -> String {
    let first = rng.choose_str(FIRST_NAMES);
    let mut parts = Vec::with_capacity(3);
    parts.push(first.to_string());
    if rng.one_in(20) {
        let middle = rng.choose_str(FIRST_NAMES).chars().next().unwrap_or('A');
        parts.push(format!("{middle}."));
    }
    parts.push(rng.choose_str(LAST_NAMES).to_string());
    parts.join(" ")
}

fn generate_recipients(rng: &mut SimpleRng) -> Vec<String> {
    let count = rng.gen_range_usize_inclusive(1, 3);
    let mut recipients = Vec::with_capacity(count);
    for _ in 0..count {
        recipients.push(generate_sender(rng));
    }
    recipients
}

fn generate_subject(rng: &mut SimpleRng) -> String {
    let mut subject = rng.choose_str(SUBJECTS).to_string();
    if rng.one_in(5) {
        subject = format!("Re: {subject}");
        if rng.one_in(2) {
            subject = format!("Re: {subject}");
        }
    }
    subject
}

fn random_body(sender: &str, subject: &str, rng: &mut SimpleRng) -> String {
    let greeting = rng.choose_str(GREETINGS);
    let closing = rng.choose_str(CLOSINGS);
    let paragraph_count = rng.gen_range_usize_inclusive(2, 4);
    let mut paragraphs = Vec::with_capacity(paragraph_count);

    for _ in 0..paragraph_count {
        paragraphs.push(rng.choose_str(PARAGRAPHS));
    }

    let summary = format!("<p><em>Summary:</em> {}</p>", rng.choose_str(SUMMARIES));

    let mut body = format!("<p>{greeting}</p>");
    for paragraph in paragraphs {
        body.push_str(&format!("<p>{paragraph}</p>"));
    }
    body.push_str(&summary);
    body.push_str(&format!(
        "<p>Subject reference: <strong>{subject}</strong></p>"
    ));
    body.push_str(&format!("<p>{closing}<br/>{sender}</p>"));
    body
}

const FIRST_NAMES: &[&str] = &[
    "Alex", "Casey", "Jordan", "Morgan", "Taylor", "Jamie", "Riley", "Sam", "Drew", "Skyler",
];

const LAST_NAMES: &[&str] = &[
    "Anderson", "Bennett", "Chen", "Diaz", "Edwards", "Fischer", "Garcia", "Hughes", "Iqbal",
    "Jensen", "Klein", "Lopez", "Miller", "Nguyen", "Ortiz",
];

const SUBJECTS: &[&str] = &[
    "Project update: timeline adjustments",
    "Reminder: submit the sprint report",
    "Lunch & learn invitation",
    "Draft agenda for tomorrow's sync",
    "Customer feedback summary",
    "Action required: security checklist",
    "Planning notes for the offsite",
    "Quick question about the release",
    "Design doc review request",
    "Thanks for the presentation yesterday",
];

const GREETINGS: &[&str] = &[
    "Hello team,",
    "Hi folks,",
    "Good afternoon,",
    "Hi there,",
    "Hello everyone,",
];

const PARAGRAPHS: &[&str] = &[
    "I wanted to share a short update on the latest mock email generated by the backend. \
     The content is designed to demonstrate how the FTML pager renders HTML documents.",
    "Please take a moment to skim the details below. The mock system rotates through a set \
     of templates so the inbox feels active while we test the UI interactions.",
    "If you spot anything that looks off in the rendered output, feel free to flag it. \
     The goal is to mirror the Go client experience as closely as possible, including colors and keybindings.",
    "This paragraph exists to stretch the pager a little further, just to make sure scrolling \
     continues to feel natural. We also want to see how long messages behave in the mock inbox.",
];

const SUMMARIES: &[&str] = &[
    "Schedules remain on track and the next handshake is queued for Friday.",
    "No additional changes are necessary; existing settings should be sufficient.",
    "The demo data set continues to evolve so we get a realistic preview.",
    "Pending review items: onboarding copy updates and the latest Ratatui tweaks.",
];

const CLOSINGS: &[&str] = &["Best regards,", "Cheers,", "Thanks!", "See you soon,"];

/// Tiny deterministic RNG used to keep mock timings reproducible.
struct SimpleRng(u64);

impl SimpleRng {
    /// Construct a new generator from a seed.
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Produce the next pseudo-random 32-bit value.
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 32) as u32
    }

    /// Return a value uniformly sampled from the half-open `range`.
    fn gen_range_usize(&mut self, range: Range<usize>) -> usize {
        if range.start >= range.end {
            range.start
        } else {
            let span = range.end - range.start;
            range.start + (self.next_u32() as usize % span)
        }
    }

    /// Return a value uniformly sampled from the inclusive range `[start, end]`.
    fn gen_range_usize_inclusive(&mut self, start: usize, end: usize) -> usize {
        if start >= end {
            start
        } else {
            start + (self.next_u32() as usize % (end - start + 1))
        }
    }

    /// Pick an item from `slice`, cycling uniformly.
    fn choose_str<'a>(&mut self, slice: &'a [&'a str]) -> &'a str {
        let idx = self.gen_range_usize(0..slice.len());
        slice[idx]
    }

    fn one_in(&mut self, n: usize) -> bool {
        if n == 0 {
            return true;
        }
        self.gen_range_usize(0..n) == 0
    }
}

fn random_seed() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678_9ABC_DEF0)
        ^ 0xA5A5_A5A5_F0F0_F0F0
}

mod html2text {
    pub fn from_read(bytes: &[u8], width: usize) -> String {
        let mut out = String::new();
        let mut col = 0usize;
        let data = String::from_utf8_lossy(bytes);
        for word in data.split_whitespace() {
            let len = word.chars().count();
            if col + len + 1 > width {
                out.push('\n');
                col = 0;
            }
            if col != 0 {
                out.push(' ');
                col += 1;
            }
            out.push_str(word);
            col += len;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_attachments_advertise_the_bytes_they_carry() {
        let mut rng = SimpleRng::new(7);
        let mut seen = 0;
        for _ in 0..200 {
            for attachment in generate_mock_attachments(&mut rng) {
                let data = attachment.data.as_ref().expect("mock data is inline");
                assert_eq!(attachment.size, data.len());
                seen += 1;
            }
        }
        assert!(seen > 0, "expected the generator to produce attachments");
    }

    #[test]
    fn mock_attachment_payload_matches_the_requested_size() {
        // Longer than the header, so it pads.
        let padded = mock_attachment_payload("report.pdf", "application/pdf", 4_096);
        assert_eq!(padded.len(), 4_096);
        // Shorter than the header, so it truncates.
        let clipped = mock_attachment_payload("report.pdf", "application/pdf", 12);
        assert_eq!(clipped.len(), 12);
        assert_eq!(&clipped, b"This is a pl");
    }

    /// Drive one action through the queue and the worker, the way the app
    /// does, and wait for the worker to report back.
    fn apply(backend: &MockBackend, action_type: ActionType, message_id: MessageId) {
        let results = backend
            .apply_actions(vec![Action::new(action_type, message_id)])
            .expect("queueing the action");
        let status = results
            .recv_timeout(Duration::from_secs(5))
            .expect("the worker reports back");
        status.result.expect("the action succeeds");
    }

    fn outgoing(subject: &str) -> OutgoingMessage {
        OutgoingMessage {
            to: vec!["someone@example.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.to_string(),
            text_body: "Body.".to_string(),
            html_body: "<p>Body.</p>".to_string(),
            attachments: Vec::new(),
        }
    }

    fn message(id: MessageId) -> Message {
        Message {
            id,
            sent: OffsetDateTime::now_utc(),
            sender: "someone@example.com".to_string(),
            recipients: vec![DEFAULT_SENDER.to_string()],
            subject: "A message".to_string(),
            size: 128,
            starred: false,
            important: false,
            answered: false,
            forwarded: false,
            status: MessageStatus::Read,
            labels: Vec::new(),
            uid: id as u32,
            seq: 0,
            has_attachments: false,
        }
    }

    fn has_label(message: &Message, label: &str) -> bool {
        message
            .labels
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(label))
    }

    /// The end state the JMAP backend spells out as `mailboxIds=[Sent]` and
    /// `keywords=[$seen]`: filed under Sent, already read, and no longer
    /// carrying the draft label it was composed under.
    #[test]
    fn sending_files_the_copy_under_sent_as_read_and_not_a_draft() {
        let backend = MockBackend::empty();

        backend
            .send_message(outgoing("Two attachments"))
            .expect("sending succeeds");

        let sent = backend.messages_in(MailboxKind::Sent);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].subject, "Two attachments");
        assert_eq!(sent[0].status, MessageStatus::Read);
        assert!(has_label(&sent[0], "Sent"));
        assert!(
            !has_label(&sent[0], "Draft"),
            "a sent message must not still look like a draft: {:?}",
            sent[0].labels
        );
        assert!(backend.messages_in(MailboxKind::Drafts).is_empty());
    }

    #[test]
    fn saving_a_draft_files_it_under_drafts_and_leaves_sent_alone() {
        let backend = MockBackend::empty();

        backend
            .save_draft(outgoing("Still writing"))
            .expect("saving succeeds");

        let drafts = backend.messages_in(MailboxKind::Drafts);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].status, MessageStatus::New);
        assert!(has_label(&drafts[0], "Draft"));
        assert!(backend.messages_in(MailboxKind::Sent).is_empty());
    }

    /// Un-starring is the action that stayed broken against FastMail for as
    /// long as the JMAP backend patched `keywords/$flagged` to `false`.
    #[test]
    fn unstarring_clears_the_star_and_drops_its_label() {
        let backend = MockBackend::empty();
        let mut starred = message(1);
        starred.starred = true;
        starred.labels = vec!["Starred".to_string()];
        backend.insert(MailboxKind::Starred, starred);

        apply(&backend, ActionType::MarkAsUnstarred, 1);

        let messages = backend.messages_in(MailboxKind::Starred);
        assert_eq!(messages.len(), 1);
        assert!(!messages[0].starred);
        assert!(
            !has_label(&messages[0], "Starred"),
            "the label outlived the star: {:?}",
            messages[0].labels
        );
    }

    #[test]
    fn unmarking_important_clears_the_flag_and_drops_its_label() {
        let backend = MockBackend::empty();
        let mut important = message(2);
        important.important = true;
        important.labels = vec!["Important".to_string()];
        backend.insert(MailboxKind::Important, important);

        apply(&backend, ActionType::MarkAsUnimportant, 2);

        let messages = backend.messages_in(MailboxKind::Important);
        assert_eq!(messages.len(), 1);
        assert!(!messages[0].important);
        assert!(!has_label(&messages[0], "Important"));
    }

    /// The third caller of the keyword-clearing path: back to the inbox, and
    /// unread again.
    #[test]
    fn moving_back_to_the_inbox_unread_restores_the_new_status() {
        let backend = MockBackend::empty();
        let mut archived = message(3);
        archived.labels = vec!["Archive".to_string()];
        backend.insert(MailboxKind::Archive, archived);

        apply(&backend, ActionType::MoveToInboxUnread, 3);

        assert!(backend.messages_in(MailboxKind::Archive).is_empty());
        let inbox = backend.messages_in(MailboxKind::Inbox);
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].status, MessageStatus::New);
        assert!(!has_label(&inbox[0], "Archive"));
    }

    #[test]
    fn marking_as_read_leaves_the_message_where_it_is() {
        let backend = MockBackend::empty();
        let mut unread = message(4);
        unread.status = MessageStatus::New;
        backend.insert(MailboxKind::Inbox, unread);

        apply(&backend, ActionType::MarkAsRead, 4);

        let inbox = backend.messages_in(MailboxKind::Inbox);
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].status, MessageStatus::Read);
    }
}
