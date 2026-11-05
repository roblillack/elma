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
    /// Recipients as addresses, each written so it parses back as a single
    /// mailbox: reopening a draft puts them straight into the compose fields.
    ///
    /// [`Message::recipients`] is for display and may hold nothing but a name;
    /// these are what the message is actually addressed to.
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    /// What OpenPGP had to say about the message, if it was signed or
    /// encrypted.  `None` for ordinary mail.
    pub security: Option<SecurityInfo>,
}

/// The outcome of decrypting and verifying a message, for the message view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecurityInfo {
    pub encryption: Option<EncryptionState>,
    pub signatures: Vec<SignatureState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncryptionState {
    /// Encrypted, and decrypted for display.
    Decrypted,
    /// Decrypted, but encrypted the way OpenPGP did before integrity
    /// protection: it could have been changed on the way without that
    /// showing.
    Unprotected,
    /// Encrypted, and could not be decrypted.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureState {
    pub verdict: SignatureVerdict,
    /// The signer's user ID, when the key is known.
    pub signer: Option<String>,
    /// Fingerprint of the signing key, or its key ID when that is all the
    /// signature names.
    pub key: String,
    /// Whether the signing certificate carries the address the message claims
    /// to be from.  A good signature from somebody else's key is worth little.
    pub matches_sender: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureVerdict {
    Good,
    /// The signature does not match the content: tampered with, or broken in
    /// transit.
    Bad(String),
    /// Nobody in the keyring made this signature.
    UnknownKey,
    /// The key is known but can no longer vouch for anything.
    Unusable(String),
}

/// How a [`SecurityInfo`] should be presented, from reassuring to alarming.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecurityLevel {
    Good,
    Uncertain,
    Bad,
}

impl SecurityInfo {
    pub fn is_empty(&self) -> bool {
        self.encryption.is_none() && self.signatures.is_empty()
    }

    pub fn was_encrypted(&self) -> bool {
        self.encryption.is_some()
    }

    pub fn level(&self) -> SecurityLevel {
        let encryption = match &self.encryption {
            Some(EncryptionState::Failed(_)) => SecurityLevel::Bad,
            Some(EncryptionState::Unprotected) => SecurityLevel::Uncertain,
            _ => SecurityLevel::Good,
        };
        self.signatures
            .iter()
            .map(SignatureState::level)
            .chain(std::iter::once(encryption))
            .max()
            .unwrap_or(SecurityLevel::Good)
    }

    /// One line for the message header.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match &self.encryption {
            Some(EncryptionState::Decrypted) => parts.push("Encrypted".to_string()),
            Some(EncryptionState::Unprotected) => {
                parts.push("Encrypted in the old format without integrity protection".to_string())
            }
            Some(EncryptionState::Failed(reason)) => {
                parts.push(format!("Encrypted, cannot decrypt: {reason}"))
            }
            None => {}
        }
        parts.extend(self.signatures.iter().map(SignatureState::summary));
        if self.encryption.is_some() && self.signatures.is_empty() {
            parts.push("not signed".to_string());
        }
        parts.join("; ")
    }
}

impl SignatureState {
    pub fn level(&self) -> SecurityLevel {
        match self.verdict {
            SignatureVerdict::Good if self.matches_sender => SecurityLevel::Good,
            SignatureVerdict::Good | SignatureVerdict::UnknownKey => SecurityLevel::Uncertain,
            SignatureVerdict::Bad(_) | SignatureVerdict::Unusable(_) => SecurityLevel::Bad,
        }
    }

    pub fn summary(&self) -> String {
        let who = self.signer.as_deref().unwrap_or("unknown signer");
        let key = short_key(&self.key);
        match &self.verdict {
            SignatureVerdict::Good if self.matches_sender => {
                format!("Good signature from {who} ({key})")
            }
            SignatureVerdict::Good => {
                format!("Good signature from {who} ({key}), who is not the sender")
            }
            SignatureVerdict::Bad(reason) => format!("BAD signature by {who} ({key}): {reason}"),
            SignatureVerdict::UnknownKey => format!("Signed by unknown key {key}"),
            SignatureVerdict::Unusable(reason) => {
                format!("Signature by {who} ({key}) cannot be trusted: {reason}")
            }
        }
    }
}

/// The last sixteen hex digits of a fingerprint: the long key ID, which is
/// what people recognise and what fits in a header line.
fn short_key(key: &str) -> &str {
    let start = key.len().saturating_sub(16);
    key.get(start..).unwrap_or(key)
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
    /// Raw bytes of the attachment, if already fetched.
    ///
    /// Backends that deliver the full MIME payload up-front populate this.
    /// JMAP leaves it empty and exposes `blob_id` instead so callers can
    /// download the body on demand.
    pub data: Option<Vec<u8>>,
    /// Opaque backend identifier for fetching the attachment body later.
    pub blob_id: Option<String>,
    /// Whether the body displays this part itself, referencing it as `cid:…`.
    ///
    /// An embedded image is part of how the message reads, so it earns no
    /// attachment marker in the list and is not carried into a forward — but it
    /// is still a file, and the save dialog offers it like any other.
    pub inline: bool,
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
    MarkAsRead,
    MarkAsUnread,
    MarkAsUnimportant,
    MoveToSpam,
}

#[derive(Clone, Debug)]
pub struct Action {
    pub action_type: ActionType,
    pub message_id: MessageId,
    pub original_status: Option<MessageStatus>,
}

impl Action {
    pub fn new(action_type: ActionType, message_id: MessageId) -> Self {
        Self {
            action_type,
            message_id,
            original_status: None,
        }
    }

    pub fn with_original_status(
        action_type: ActionType,
        message_id: MessageId,
        original_status: MessageStatus,
    ) -> Self {
        Self {
            action_type,
            message_id,
            original_status: Some(original_status),
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
