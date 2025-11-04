use crate::{
    backend::{BackendEvent, MailBackend},
    model::{
        Action, ActionType, Message, MessageContent, MessageContentPart, MessageId, MessageStatus,
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

pub struct MockBackend {
    messages: Arc<Mutex<Vec<MockMessage>>>,
    contents: Arc<Mutex<HashMap<MessageId, MessageContent>>>,
    receiver: Mutex<Option<Receiver<BackendEvent>>>,
    id_counter: Arc<AtomicU64>,
}

#[derive(Clone)]
struct MockMessage {
    message: Message,
}

impl MockBackend {
    pub fn default() -> Self {
        Self::demo()
    }

    pub fn demo() -> Self {
        let (sender, receiver) = mpsc::channel();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let contents = Arc::new(Mutex::new(HashMap::new()));
        let id_counter = Arc::new(AtomicU64::new(0));

        let backend = Self {
            messages: Arc::clone(&messages),
            contents: Arc::clone(&contents),
            receiver: Mutex::new(Some(receiver)),
            id_counter: Arc::clone(&id_counter),
        };

        backend.populate_initial_messages(INITIAL_MESSAGE_COUNT);
        backend.spawn_incoming_mail_generator(messages, contents, sender, id_counter);
        backend
    }

    fn populate_initial_messages(&self, count: usize) {
        let mut rng = SimpleRng::new(random_seed());
        let mut messages = self.messages.lock().expect("messages mutex poisoned");
        let mut contents = self.contents.lock().expect("contents mutex poisoned");

        for _ in 0..count {
            let id = self.next_id();
            let (message, content) = old_random_message(id, &mut rng);
            contents.insert(id, content);
            messages.push(MockMessage { message });
        }
    }

    fn spawn_incoming_mail_generator(
        &self,
        messages: Arc<Mutex<Vec<MockMessage>>>,
        contents: Arc<Mutex<HashMap<MessageId, MessageContent>>>,
        sender: Sender<BackendEvent>,
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
                    let mut message_lock = messages.lock().expect("messages mutex poisoned");
                    let mut content_lock = contents.lock().expect("contents mutex poisoned");

                    content_lock.insert(id, content);
                    message_lock.push(MockMessage {
                        message: message.clone(),
                    });
                }

                let _ = sender.send(BackendEvent::NewMessage(message));
            }
        });
    }

    fn next_id(&self) -> MessageId {
        self.id_counter.fetch_add(1, Ordering::SeqCst) + 1
    }
}

impl MailBackend for MockBackend {
    fn load_inbox(&self) -> Result<(Vec<Message>, Receiver<BackendEvent>)> {
        let mut receiver_guard = self.receiver.lock().expect("receiver mutex poisoned");
        let receiver = receiver_guard
            .take()
            .ok_or_else(|| anyhow!("Inbox already loaded"))?;

        let mut messages = self.messages.lock().expect("messages mutex poisoned");
        messages.sort_by_key(|msg| msg.message.sent);
        let list = messages.iter().map(|msg| msg.message.clone()).collect();

        Ok((list, receiver))
    }

    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        let contents = self.contents.lock().expect("contents mutex poisoned");
        contents
            .get(&message_id)
            .cloned()
            .ok_or_else(|| anyhow!("message {message_id} not found"))
    }

    fn apply_action(&self, action: &Action) -> Result<()> {
        let mut messages = self.messages.lock().expect("messages mutex poisoned");
        let mut contents = self.contents.lock().expect("contents mutex poisoned");

        if let Some(msg) = messages
            .iter_mut()
            .find(|mock| mock.message.id == action.message_id)
        {
            match action.action_type {
                ActionType::Archive => msg.message.status = MessageStatus::Archived,
                ActionType::Delete => msg.message.status = MessageStatus::Deleted,
                ActionType::MoveToInboxUnread => msg.message.status = MessageStatus::New,
                ActionType::MoveToInboxRead => msg.message.status = MessageStatus::Read,
                ActionType::MarkAsStarred => msg.message.starred = true,
                ActionType::MarkAsUnstarred => msg.message.starred = false,
            }

            if let Some(content) = contents.get_mut(&action.message_id) {
                if msg.message.status == MessageStatus::New {
                    content.mailer = format!("{MAILER_NAME} (unread)");
                } else {
                    content.mailer = MAILER_NAME.to_string();
                }
            }

            Ok(())
        } else {
            Err(anyhow!("message {} not found", action.message_id))
        }
    }
}

fn new_random_message(
    id: MessageId,
    sent: OffsetDateTime,
    rng: &mut SimpleRng,
) -> (Message, MessageContent) {
    let subject = generate_subject(rng);
    let sender = generate_sender(rng);
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

struct SimpleRng(u64);

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 32) as u32
    }

    fn gen_range_usize(&mut self, range: Range<usize>) -> usize {
        if range.start >= range.end {
            range.start
        } else {
            let span = range.end - range.start;
            range.start + (self.next_u32() as usize % span)
        }
    }

    fn gen_range_usize_inclusive(&mut self, start: usize, end: usize) -> usize {
        if start >= end {
            start
        } else {
            start + (self.next_u32() as usize % (end - start + 1))
        }
    }

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
