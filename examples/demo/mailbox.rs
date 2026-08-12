//! The mail the demo goes through.
//!
//! Its own inbox rather than the snapshot fixture's: the fixture carries a row
//! in every state the list can draw, including messages that already read as
//! deleted or archived, which would muddle a recording whose whole point is the
//! user putting them into those states.  Everything here arrives plain.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};

use anyhow::{Result, anyhow};
use time::{OffsetDateTime, macros::datetime};

use crate::backend::{ActionStatus, BackendEvent, MailBackend, MailboxSnapshot, OutgoingMessage};
use crate::model::{
    Action, MailboxKind, Message, MessageAttachment, MessageContent, MessageContentPart, MessageId,
    MessageStatus,
};

/// The message the demo opens and saves the attachment of.
pub const INVOICE: MessageId = 9;

/// The colleague's message, which asks for a document and gets answered with
/// it attached.
pub const COLLEAGUE: MessageId = 3;

/// The file the colleague is asking for.  Written to disk before the demo
/// starts, because attaching it reads it.
pub const REQUESTED_DOCUMENT: &str = "vendor-agreement-2026.pdf";

/// An in-memory account holding [`inbox`], answering every request at once.
pub struct DemoBackend {
    messages: Vec<Message>,
    contents: HashMap<MessageId, MessageContent>,
}

impl DemoBackend {
    pub fn new() -> Self {
        Self {
            messages: inbox(),
            contents: contents(),
        }
    }
}

impl MailBackend for DemoBackend {
    fn load_mailbox(
        &self,
        mailbox: MailboxKind,
    ) -> Result<(MailboxSnapshot, Receiver<BackendEvent>)> {
        let messages = match mailbox {
            MailboxKind::Inbox => self.messages.clone(),
            _ => Vec::new(),
        };
        // Nothing is ever sent on it: the demo's mail does not change under it.
        let (_sender, events) = mpsc::channel();
        Ok((
            MailboxSnapshot {
                total: messages.len(),
                messages,
            },
            events,
        ))
    }

    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        self.contents
            .get(&message_id)
            .cloned()
            .ok_or_else(|| anyhow!("no such message: {message_id}"))
    }

    fn apply_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>> {
        let (sender, receiver) = mpsc::channel();
        for action in actions {
            let _ = sender.send(ActionStatus {
                action,
                result: Ok(()),
            });
        }
        Ok(receiver)
    }

    fn send_message(&self, _message: OutgoingMessage) -> Result<()> {
        Ok(())
    }

    fn save_draft(&self, _message: OutgoingMessage) -> Result<()> {
        Ok(())
    }
}

/// One message, written out in full.
struct Entry {
    id: MessageId,
    sent: OffsetDateTime,
    sender: &'static str,
    subject: &'static str,
    size: usize,
    status: MessageStatus,
    starred: bool,
    important: bool,
    answered: bool,
    has_attachments: bool,
    labels: &'static [&'static str],
}

impl Entry {
    const fn new(
        id: MessageId,
        sent: OffsetDateTime,
        sender: &'static str,
        subject: &'static str,
        size: usize,
        status: MessageStatus,
    ) -> Self {
        Self {
            id,
            sent,
            sender,
            subject,
            size,
            status,
            starred: false,
            important: false,
            answered: false,
            has_attachments: false,
            labels: &[],
        }
    }

    fn build(&self, seq: u32) -> Message {
        Message {
            id: self.id,
            sent: self.sent,
            sender: self.sender.to_string(),
            recipients: vec!["rob@example.com".to_string()],
            subject: self.subject.to_string(),
            size: self.size,
            starred: self.starred,
            important: self.important,
            answered: self.answered,
            forwarded: false,
            status: self.status,
            labels: self.labels.iter().map(|label| label.to_string()).collect(),
            uid: 4000 + self.id as u32,
            seq,
            has_attachments: self.has_attachments,
        }
    }
}

/// A morning's mail, oldest first and numbered from one, the way a server
/// hands a mailbox over -- so the client opens on the newest message at the
/// bottom.
///
/// Arranged for the way the demo works through it: three messages worth
/// keeping at the top, then the five it deals with, then the invoice it
/// answers.  Every action moves the selection on by one, so the triage runs
/// downwards from a single `PageUp` -- five messages up, which is exactly
/// [`PAGE_JUMP`](../../src/app.rs) -- and comes to rest on the invoice.
fn inbox() -> Vec<Message> {
    [
        Entry {
            starred: true,
            ..Entry::new(
                1,
                datetime!(2026-03-11 15:30:00 UTC),
                "anna@example.com",
                "Lunch on Thursday?",
                2_100,
                MessageStatus::Read,
            )
        },
        Entry {
            answered: true,
            ..Entry::new(
                2,
                datetime!(2026-03-12 09:12:00 UTC),
                "dev-list@ratatui.example",
                "[dev] Widget layout changes landing next week",
                42_100,
                MessageStatus::Read,
            )
        },
        Entry {
            labels: &["Work"],
            ..Entry::new(
                COLLEAGUE,
                datetime!(2026-03-12 17:40:00 UTC),
                "mira.tomassi@example.org",
                "Do you have the signed vendor agreement?",
                7_320,
                MessageStatus::New,
            )
        },
        Entry::new(
            4,
            datetime!(2026-03-13 06:15:00 UTC),
            "newsletter@rustweekly.example",
            "This Week in Rust #612",
            134_000,
            MessageStatus::Read,
        ),
        Entry::new(
            5,
            datetime!(2026-03-13 08:02:00 UTC),
            "deals@shop.example",
            "LAST CHANCE: 80% off everything!!!",
            61_000,
            MessageStatus::Read,
        ),
        Entry::new(
            6,
            datetime!(2026-03-13 11:20:00 UTC),
            "no-reply@calendar.example",
            "Invitation: Sprint review @ Fri Mar 20",
            8_900,
            MessageStatus::Read,
        ),
        Entry {
            has_attachments: true,
            labels: &["Travel"],
            ..Entry::new(
                7,
                datetime!(2026-03-13 16:45:00 UTC),
                "reservations@hotel.example",
                "Your booking confirmation",
                96_500,
                MessageStatus::Read,
            )
        },
        Entry::new(
            8,
            datetime!(2026-03-14 07:12:00 UTC),
            "tickets@support.example",
            "Ticket #4711 has been closed",
            3_400,
            MessageStatus::Read,
        ),
        Entry {
            important: true,
            has_attachments: true,
            labels: &["Work", "Invoices"],
            ..Entry::new(
                INVOICE,
                datetime!(2026-03-14 08:15:00 UTC),
                "billing@vendor.example",
                "Invoice 2026-0342 for February",
                18_400,
                MessageStatus::New,
            )
        },
    ]
    .iter()
    .enumerate()
    .map(|(index, entry)| entry.build(index as u32 + 1))
    .collect()
}

/// Bodies for the messages the demo opens.
fn contents() -> HashMap<MessageId, MessageContent> {
    let mut contents = HashMap::new();

    contents.insert(
        COLLEAGUE,
        MessageContent {
            mailer: "Thunderbird/128.0".to_string(),
            parts: vec![
                MessageContentPart {
                    content_type: "text/plain".to_string(),
                    content: b"The auditors are asking for the countersigned vendor agreement."
                        .to_vec(),
                },
                MessageContentPart {
                    content_type: "text/html".to_string(),
                    content: br#"<html><body>
<p>Hi Rob,</p>
<p>the auditors are asking for the <b>countersigned vendor agreement</b> that
goes with this month's invoice, and I cannot find it anywhere in the shared
folder.</p>
<p>Do you still have your copy? Sending it straight back to me is fine.</p>
<p>Thanks!<br>Mira</p>
</body></html>"#
                        .to_vec(),
                },
            ],
            attachments: Vec::new(),
        },
    );

    contents.insert(
        INVOICE,
        MessageContent {
            mailer: "VendorBilling/2.1".to_string(),
            parts: vec![
                MessageContentPart {
                    content_type: "text/plain".to_string(),
                    content: b"Invoice 2026-0342 is attached. Payment is due within 14 days."
                        .to_vec(),
                },
                MessageContentPart {
                    content_type: "text/html".to_string(),
                    content: br#"<html><body>
<h1>Invoice 2026-0342</h1>
<p>Dear customer,</p>
<p>your invoice for <b>February 2026</b> is attached as a PDF. The total is
<b>1,248.00 EUR</b>, payable within 14 days.</p>
<ul>
<li>Hosting, February 2026 &mdash; 899.00 EUR</li>
<li>Support plan &mdash; 349.00 EUR</li>
</ul>
<p>A copy is attached for your records. The countersigned agreement it is
billed against is the one your auditors will ask for.</p>
<p>Kind regards,<br>Vendor Billing</p>
</body></html>"#
                        .to_vec(),
                },
            ],
            attachments: vec![MessageAttachment {
                filename: Some("invoice-2026-0342.pdf".to_string()),
                mime_type: "application/pdf".to_string(),
                size: 14_320,
                data: Some(b"%PDF-1.4 demo".to_vec()),
                blob_id: None,
                inline: false,
            }],
        },
    );

    contents
}
