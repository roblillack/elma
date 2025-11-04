//! Mock backend used for demo mode and manual testing.
//!
//! This implementation keeps all data in-memory and simulates asynchronous behaviour
//! by introducing small random delays when applying actions.  The goal is to exercise
//! the UI logic without relying on an external mail provider.

use crate::{
    backend::{ActionStatus, BackendEvent, MailBackend},
    model::{
        Action, ActionType, MailboxKind, Message, MessageContent, MessageContentPart, MessageId,
        MessageStatus,
    },
};
use anyhow::{Result, anyhow};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, SystemTime},
};
use time::{Duration as TimeDuration, OffsetDateTime};

const INITIAL_MESSAGE_COUNT: usize = 250;
const MAILER_NAME: &str = "MockMailer/tdoc-demo";

/// Simple in-memory backend for demos and integration tests.
///
/// Messages are generated deterministically at startup so the UI can exercise the
/// same flows as the Gmail backend without network access.
pub struct MockBackend {
    mailboxes: Arc<Mutex<HashMap<MailboxKind, Vec<MockMessage>>>>,
    contents: Arc<Mutex<HashMap<MessageId, MessageContent>>>,
    event_sender: Arc<Mutex<Option<Sender<BackendEvent>>>>,
    id_counter: Arc<AtomicU64>,
}

#[derive(Clone)]
struct MockMessage {
    message: Message,
}

impl MockBackend {
    /// Build the default mock backend, pre-populating the inbox and starting the
    /// background generator that drip-feeds new messages.
    pub fn default() -> Self {
        Self::demo()
    }

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

        let backend = Self {
            mailboxes: Arc::clone(&mailboxes),
            contents: Arc::clone(&contents),
            event_sender: Arc::clone(&event_sender),
            id_counter: Arc::clone(&id_counter),
        };

        backend.populate_initial_mailboxes(INITIAL_MESSAGE_COUNT);
        backend.spawn_incoming_mail_generator(mailboxes, contents, event_sender, id_counter);
        backend
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
                .or_insert_with(Vec::new)
                .push(MockMessage { message });
        }

        let templates = [
            (MailboxKind::Starred, 40usize, MessageStatus::Read),
            (MailboxKind::Sent, 25usize, MessageStatus::Read),
            (MailboxKind::Drafts, 12usize, MessageStatus::New),
            (MailboxKind::Archive, 35usize, MessageStatus::Read),
            (MailboxKind::Trash, 18usize, MessageStatus::Read),
        ];

        for (kind, count, status) in templates {
            let list = mailboxes.entry(kind).or_insert_with(Vec::new);
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
                    MailboxKind::Trash => {
                        message.labels = vec!["Trash".to_string()];
                        message.starred = false;
                        if rng.one_in(6) {
                            message.status = MessageStatus::New;
                        }
                    }
                    MailboxKind::Inbox => {}
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
                        .or_insert_with(Vec::new)
                        .push(MockMessage {
                            message: message.clone(),
                        });
                }

                let sender = {
                    let guard = event_sender.lock().expect("event sender mutex poisoned");
                    guard.clone()
                };

                if let Some(sender) = sender {
                    if sender.send(BackendEvent::NewMessage(message)).is_err() {
                        let mut guard = event_sender.lock().expect("event sender mutex poisoned");
                        *guard = None;
                    }
                }
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
            if let Some(list) = mailboxes.get_mut(&kind) {
                if let Some(index) = list
                    .iter()
                    .position(|mock| mock.message.id == action.message_id)
                {
                    let mock = list.remove(index);
                    removed = Some((kind, mock));
                    break;
                }
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
            ActionType::MoveToInboxUnread => {
                mock.message.status = MessageStatus::New;
                mock.message.labels.retain(|label| {
                    !label.eq_ignore_ascii_case("Archive") && !label.eq_ignore_ascii_case("Trash")
                });
                target_kind = MailboxKind::Inbox;
            }
            ActionType::MoveToInboxRead => {
                mock.message.status = MessageStatus::Read;
                mock.message.labels.retain(|label| {
                    !label.eq_ignore_ascii_case("Archive") && !label.eq_ignore_ascii_case("Trash")
                });
                target_kind = MailboxKind::Inbox;
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

impl MailBackend for MockBackend {
    /// Return the current mailbox snapshot and subscribe to future events.
    fn load_mailbox(&self, mailbox: MailboxKind) -> Result<(Vec<Message>, Receiver<BackendEvent>)> {
        let mut messages = {
            let mailboxes = self.mailboxes.lock().expect("mailboxes mutex poisoned");
            let list = mailboxes
                .get(&mailbox)
                .ok_or_else(|| anyhow!("mailbox {mailbox:?} not found"))?
                .iter()
                .map(|mock| mock.message.clone())
                .collect::<Vec<_>>();
            list
        };

        messages.sort_by_key(|msg| msg.sent);

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

        Ok((messages, receiver))
    }

    /// Fetch the MIME content for an individual message.
    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        let contents = self.contents.lock().expect("contents mutex poisoned");
        contents
            .get(&message_id)
            .cloned()
            .ok_or_else(|| anyhow!("message {message_id} not found"))
    }

    /// Spawn a worker that applies each action with a short random delay.
    ///
    /// The jitter (50–500 ms) mimics the round trips that the Gmail backend incurs,
    /// giving the UI a realistic opportunity to render progress.
    fn apply_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>> {
        let (tx, rx) = mpsc::channel();
        let mailboxes = Arc::clone(&self.mailboxes);
        let contents = Arc::clone(&self.contents);

        thread::spawn(move || {
            let mut delay_rng = SimpleRng::new(random_seed() ^ 0xa511f93acb5d7a77);
            for action in actions {
                let delay_ms = delay_rng.gen_range_usize_inclusive(50, 500) as u64;
                thread::sleep(Duration::from_millis(delay_ms));

                let result = MockBackend::apply_action_now(&mailboxes, &contents, &action)
                    .map_err(|err| err.to_string());
                if tx.send(ActionStatus { action, result }).is_err() {
                    break;
                }
            }
        });

        Ok(rx)
    }
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

    let size = rng.gen_range_usize(0..7_203_680) + 200;

    let message = Message {
        id,
        sent,
        sender,
        recipients,
        subject,
        size,
        starred: false,
        answered: false,
        forwarded: false,
        status: MessageStatus::New,
        labels: Vec::new(),
        uid: id as u32,
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
