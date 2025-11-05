use std::fmt;
use time::{Month, OffsetDateTime};

pub type MessageId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageStatus {
    Read,
    New,
    Deleted,
    Archived,
    PendingInbox,
    Spam,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MailboxKind {
    Inbox,
    Starred,
    Important,
    Sent,
    Drafts,
    Archive,
    Spam,
    Trash,
}

impl MailboxKind {
    pub const ALL: [MailboxKind; 8] = [
        MailboxKind::Inbox,
        MailboxKind::Starred,
        MailboxKind::Important,
        MailboxKind::Sent,
        MailboxKind::Drafts,
        MailboxKind::Archive,
        MailboxKind::Spam,
        MailboxKind::Trash,
    ];

    /// User-visible name for the mailbox.
    pub fn title(self) -> &'static str {
        match self {
            MailboxKind::Inbox => "Inbox",
            MailboxKind::Starred => "Starred",
            MailboxKind::Important => "Important",
            MailboxKind::Sent => "Sent",
            MailboxKind::Drafts => "Drafts",
            MailboxKind::Archive => "Archive",
            MailboxKind::Spam => "Spam",
            MailboxKind::Trash => "Trash",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: MessageId,
    pub sent: OffsetDateTime,
    pub sender: String,
    pub recipients: Vec<String>,
    pub subject: String,
    pub size: usize,
    pub starred: bool,
    pub important: bool,
    pub answered: bool,
    pub forwarded: bool,
    pub status: MessageStatus,
    pub labels: Vec<String>,
    pub uid: u32,
    pub seq: u32,
    pub has_attachments: bool,
}

impl Message {
    pub fn flag_string(&self) -> String {
        if self.is_placeholder() {
            return "    ".to_string();
        }

        let mut flags = [' ', ' ', ' ', ' '];

        flags[0] = match self.status {
            MessageStatus::Archived => 'A',
            MessageStatus::Deleted => 'D',
            MessageStatus::New => 'N',
            MessageStatus::Read => ' ',
            MessageStatus::PendingInbox => 'I',
            MessageStatus::Spam => '!',
        };

        flags[1] = match (self.important, self.starred) {
            (false, false) => ' ',
            (false, true) => '*',
            (true, false) => '○',
            (true, true) => '⊛',
        };

        flags[2] = match (self.forwarded, self.answered) {
            (true, true) => '⇄',
            (true, false) => '→',
            (false, true) => '↩',
            (false, false) => ' ',
        };

        flags[3] = if self.has_attachments { '@' } else { ' ' };

        flags.into_iter().collect()
    }

    pub fn formatted_received(&self, now: OffsetDateTime) -> String {
        let month = short_month(self.sent.month());
        let day = self.sent.day();
        if self.sent.year() == now.year() {
            let hour = self.sent.hour();
            let minute = self.sent.minute();
            format!("[{month} {day:02} {hour:02}:{minute:02}]")
        } else {
            let year = self.sent.year();
            format!("[{month} {day:02}, {year}]")
        }
    }

    pub fn recipients_display(&self) -> String {
        if self.recipients.is_empty() {
            "Unknown recipient".to_string()
        } else {
            self.recipients.join(", ")
        }
    }

    pub fn is_placeholder(&self) -> bool {
        self.uid == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct MessageContent {
    pub mailer: String,
    pub parts: Vec<MessageContentPart>,
    pub attachments: Vec<MessageAttachment>,
}

impl MessageContent {
    pub fn part(&self, content_type: &str) -> Option<&MessageContentPart> {
        self.parts
            .iter()
            .find(|part| part.content_type.eq_ignore_ascii_case(content_type))
    }
}

#[derive(Clone, Debug)]
pub struct MessageAttachment {
    pub filename: Option<String>,
    pub mime_type: String,
    pub size: usize,
}

#[derive(Clone, Debug)]
pub struct MessageContentPart {
    pub content_type: String,
    pub content: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionType {
    Delete,
    Archive,
    MoveToInboxUnread,
    MoveToInboxRead,
    MarkAsStarred,
    MarkAsUnstarred,
    MarkAsImportant,
    MarkAsUnimportant,
    MoveToSpam,
}

#[derive(Clone, Debug)]
pub struct Action {
    pub action_type: ActionType,
    pub message_id: MessageId,
}

impl Action {
    pub fn new(action_type: ActionType, message_id: MessageId) -> Self {
        Self {
            action_type,
            message_id,
        }
    }
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1000 {
        return format!("{:>3}B", bytes);
    }

    let mut value = bytes;
    for unit in ['K', 'M', 'G', 'T'] {
        if value < 10 * 1000 {
            let as_float = value as f64 / 1000.0;
            return format!("{:.1}{unit}", as_float);
        }
        value /= 1000;
        if value < 1000 {
            return format!("{:>3}{unit}", value);
        }
    }

    "????".to_string()
}

pub fn padded_sender(sender: &str) -> String {
    let mut truncated = sender.chars().take(20).collect::<String>();
    let len = truncated.chars().count();
    for _ in len..20 {
        truncated.push(' ');
    }
    truncated
}

impl fmt::Display for MessageStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            MessageStatus::Read => "Read",
            MessageStatus::New => "New",
            MessageStatus::Deleted => "Deleted",
            MessageStatus::Archived => "Archived",
            MessageStatus::PendingInbox => "Pending inbox",
            MessageStatus::Spam => "Spam",
        };
        f.write_str(text)
    }
}

impl fmt::Display for MailboxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.title())
    }
}

fn short_month(month: Month) -> &'static str {
    match month {
        Month::January => "Jan",
        Month::February => "Feb",
        Month::March => "Mar",
        Month::April => "Apr",
        Month::May => "May",
        Month::June => "Jun",
        Month::July => "Jul",
        Month::August => "Aug",
        Month::September => "Sep",
        Month::October => "Oct",
        Month::November => "Nov",
        Month::December => "Dec",
    }
}
