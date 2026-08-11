//! Backend integration layer.
//!
//! The backend module defines the abstraction that the TUI uses to communicate with
//! different mail providers.  Backends expose a push-based event stream for mailbox
//! updates and a pull-based channel for action completion status.  The contract is
//! intentionally asynchronous so the UI thread never blocks on network or disk I/O.
//! See [`MailBackend`] for details about the required behaviour.

use crate::model::{Action, MailboxKind, Message, MessageContent, MessageId};
use anyhow::Result;
use lettre::message::{
    Attachment as LettreAttachment, MultiPart, SinglePart, header::ContentType as LettreContentType,
};
use std::sync::mpsc::Receiver;

pub mod gmail;
pub mod jmap;
pub mod mock;

/// Notifications that backends emit when something about the mailbox changes.
///
/// The UI subscribes to the channel returned by [`MailBackend::load_mailbox`] and keeps
/// its local state in sync with the events that arrive.
#[derive(Clone, Debug)]
pub enum BackendEvent {
    NewMessage(Message),
    MessageFlagsChanged(Message),
    MessageDeleted(MessageId),
}

/// Result information for a committed action.
///
/// Backends must send one result per action passed to [`MailBackend::apply_actions`].
/// Successful operations report `Ok(())`; failures should capture the backend-specific
/// error text so the UI can surface it to the user.
#[derive(Clone, Debug)]
pub struct ActionStatus {
    pub action: Action,
    pub result: std::result::Result<(), String>,
}

/// Snapshot of a mailbox returned by [`MailBackend::load_mailbox`].
#[derive(Clone, Debug)]
pub struct MailboxSnapshot {
    /// Total number of messages currently present in the mailbox.
    pub total: usize,
    /// Messages that have been populated so far. Implementations may return a
    /// partial slice so the UI can start rendering immediately.
    pub messages: Vec<Message>,
}

/// Data associated with an outgoing message created from the compose view.
#[derive(Clone, Debug, Default)]
pub struct OutgoingMessage {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub text_body: String,
    pub html_body: String,
    pub attachments: Vec<OutgoingAttachment>,
}

/// A file attached to an outgoing message.
#[derive(Clone, Debug)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

impl OutgoingAttachment {
    pub fn size(&self) -> usize {
        self.data.len()
    }
}

/// Assemble the MIME body of an outgoing message.
///
/// The two bodies always go into a `multipart/alternative`; attachments wrap
/// that in a `multipart/mixed`.  Every SMTP-based backend builds the same
/// structure, so the code lives here rather than being copied per backend.
pub(crate) fn build_compose_body(
    text_body: String,
    html_body: String,
    attachments: Vec<OutgoingAttachment>,
) -> Result<MultiPart> {
    let alternative = MultiPart::alternative()
        .singlepart(SinglePart::plain(text_body))
        .singlepart(SinglePart::html(html_body));

    if attachments.is_empty() {
        return Ok(alternative);
    }

    let mut mixed = MultiPart::mixed().multipart(alternative);
    for attachment in attachments {
        let content_type: LettreContentType = attachment
            .mime_type
            .parse()
            .unwrap_or_else(|_| LettreContentType::parse("application/octet-stream").unwrap());
        let part = LettreAttachment::new(attachment.filename).body(attachment.data, content_type);
        mixed = mixed.singlepart(part);
    }
    Ok(mixed)
}

/// The MIME headers of a leaf body part that decide how to treat it.
///
/// Filled in from whatever the backend has at hand: an IMAP `BODYSTRUCTURE`,
/// a parsed MIME tree, or a JMAP `EmailBodyPart`.
pub(crate) struct LeafPart<'a> {
    /// Major type only — `text`, `image`, `application`, …
    pub(crate) major_type: &'a str,
    /// Whether the part names a file, in the disposition or the content type.
    pub(crate) has_filename: bool,
    /// `Content-Disposition`, without its parameters.
    pub(crate) disposition: Option<&'a str>,
    /// Whether the part carries a `Content-ID` the body could point at.
    pub(crate) has_content_id: bool,
}

/// What a leaf part is to the reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PartRole {
    /// Body text or markup: the viewer renders it, and there is nothing here to
    /// save that the reader is not already looking at.
    Body,
    /// Content the body points at with `cid:…`, an embedded image being the
    /// usual case. Part of how the message reads rather than something the
    /// sender attached — but still a file, and still worth being able to keep.
    Inline,
    /// A file the sender attached.
    Attachment,
}

impl LeafPart<'_> {
    /// What this part is, by the one rule every backend follows.
    ///
    /// Each backend asks twice: once for the message list, from what the server
    /// says about a message it has not fetched yet, and once for the opened
    /// message, from the parts it actually got. If the two disagree the `@`
    /// marker in the list flips as soon as the user opens the message, so the
    /// rule lives here rather than being written out per backend.
    pub(crate) fn role(&self) -> PartRole {
        // An explicit `attachment` disposition settles it, even for parts that
        // also carry a Content-ID.
        if self
            .disposition
            .is_some_and(|value| value.eq_ignore_ascii_case("attachment"))
        {
            return PartRole::Attachment;
        }

        // A Content-ID on a part the sender did not mark as an attachment is
        // there for the body to reference as `cid:…` — the logo in an HTML
        // mail. Whether the body *does* reference it cannot be checked from the
        // message list, where there is no body yet, so anything that could be
        // referenced counts as inline and the marker stays put on open.
        if self.has_content_id
            && self
                .disposition
                .is_none_or(|value| value.eq_ignore_ascii_case("inline"))
        {
            return PartRole::Inline;
        }

        if self.has_filename {
            return PartRole::Attachment;
        }

        // What is left is either body text or something the reader cannot see
        // any other way.
        if self.major_type.eq_ignore_ascii_case("multipart")
            || self.major_type.eq_ignore_ascii_case("text")
        {
            PartRole::Body
        } else {
            PartRole::Attachment
        }
    }

    /// Whether this part earns the message an attachment marker in the list.
    ///
    /// Deliberately narrower than "is a file": a signature logo would otherwise
    /// mark every newsletter as carrying an attachment.
    pub(crate) fn is_attachment(&self) -> bool {
        matches!(self.role(), PartRole::Attachment)
    }
}

/// Abstraction over a mail provider implementation.
///
/// The trait is purposely synchronous from the caller's perspective while the
/// implementation is free to spawn its own async tasks.  `apply_actions` returns an
/// `mpsc::Receiver` for streaming status updates; the UI polls that receiver every
/// frame so the terminal stays responsive during long-running commits.
///
/// # Design Notes
///
/// * `load_mailbox` produces both the initial message list and a channel that streams
///   [`BackendEvent`] updates.  This ensures there is a single source of truth for
///   mailbox mutations.  It is called from a worker thread and may block for as
///   long as connecting and authenticating take.
/// * `load_message`, `send_message`, `save_draft` and `fetch_attachment_blob` are
///   blocking calls that the UI always makes from a worker thread, never from the
///   event loop.  They may take as long as the network does, and two of them (or
///   one of them plus an `apply_actions` batch) can be in flight at the same time,
///   so implementations have to serialise their own connection state.
/// * `apply_actions` accepts the full action batch and must never block the caller.
///   Implementations should spawn work (e.g. on a thread or async runtime) and send
///   [`ActionStatus`] entries as each action completes.
///
/// # Examples
///
/// ```
/// use elma_rs::backend::{ActionStatus, MailBackend, MailboxSnapshot};
/// use elma_rs::model::{Action, ActionType, MessageId};
/// # struct DemoBackend;
/// # impl MailBackend for DemoBackend {
/// #     fn load_mailbox(&self, _kind: elma_rs::model::MailboxKind) -> anyhow::Result<(MailboxSnapshot, std::sync::mpsc::Receiver<_>)> {
/// #         unimplemented!()
/// #     }
/// #     fn load_message(&self, _id: MessageId) -> anyhow::Result<_> { unimplemented!() }
/// #     fn apply_actions(&self, actions: Vec<Action>) -> anyhow::Result<std::sync::mpsc::Receiver<ActionStatus>> {
/// #         // start background work here
/// #         unimplemented!()
/// #     }
/// # }
/// let backend = DemoBackend;
/// let statuses = backend.apply_actions(vec![Action::new(ActionType::Archive, MessageId(42))])?;
/// // UI polls `statuses` until every ActionStatus has been received.
/// # Ok::<_, anyhow::Error>(())
/// ```
pub trait MailBackend: Send + Sync {
    /// Load `mailbox` and return a channel that streams [`BackendEvent`] updates.
    fn load_mailbox(
        &self,
        mailbox: MailboxKind,
    ) -> Result<(MailboxSnapshot, Receiver<BackendEvent>)>;

    /// Load the full content for a single message.
    ///
    /// Called from a worker thread, so blocking here is expected.
    fn load_message(&self, message_id: MessageId) -> Result<MessageContent>;
    /// Begin processing a batch of actions and return a channel with completion updates.
    fn apply_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>>;
    /// Submit actions that should be executed ahead of any pending queued work.
    ///
    /// Backends with an internal work queue should insert these at the front.
    /// The default implementation simply delegates to [`apply_actions`](MailBackend::apply_actions).
    fn apply_immediate_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>> {
        self.apply_actions(actions)
    }
    /// Send a fully composed message.
    ///
    /// Called from a worker thread; uploading megabytes of attachments here is
    /// expected and does not stall the UI.
    fn send_message(&self, message: OutgoingMessage) -> Result<()>;
    /// Store a draft message.
    ///
    /// Called from a worker thread, like [`send_message`](MailBackend::send_message).
    fn save_draft(&self, message: OutgoingMessage) -> Result<()>;
    /// Download the raw bytes for an attachment identified by its backend blob id.
    ///
    /// Called from a worker thread, once per attachment that needs it.
    /// Backends that deliver the full message payload up-front populate
    /// [`MessageAttachment::data`](crate::model::MessageAttachment::data) directly
    /// and can leave this method unimplemented.  Backends that only return a
    /// pointer (for example JMAP) must override this to issue the blob download.
    fn fetch_attachment_blob(&self, _blob_id: &str) -> Result<Vec<u8>> {
        Err(anyhow::anyhow!(
            "this backend does not support on-demand attachment download"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{LeafPart, PartRole};

    fn part(major_type: &str, has_filename: bool) -> LeafPart<'_> {
        LeafPart {
            major_type,
            has_filename,
            disposition: None,
            has_content_id: false,
        }
    }

    #[test]
    fn body_text_is_not_an_attachment() {
        assert!(!part("text", false).is_attachment());
        assert!(!part("multipart", false).is_attachment());
    }

    #[test]
    fn a_named_part_is_an_attachment_whatever_its_type() {
        // Older mailers send `text/csv; name="report.csv"` with no
        // Content-Disposition at all.
        assert!(part("text", true).is_attachment());
        assert!(part("application", true).is_attachment());
    }

    #[test]
    fn a_part_the_reader_cannot_see_otherwise_is_an_attachment() {
        // No name, no disposition — but a PDF is not something the body pane
        // can render, so it has to be offered for download.
        assert!(part("application", false).is_attachment());
        assert!(part("image", false).is_attachment());
    }

    /// The three roles are distinct: body text has nothing to save, an embedded
    /// image has, and only the third earns a marker.
    #[test]
    fn a_part_the_body_references_is_a_file_without_being_an_attachment() {
        let referenced = LeafPart {
            major_type: "image",
            has_filename: true,
            disposition: Some("inline"),
            has_content_id: true,
        };

        assert_eq!(referenced.role(), PartRole::Inline);
        assert_eq!(part("text", false).role(), PartRole::Body);
        assert_eq!(part("application", true).role(), PartRole::Attachment);
    }

    #[test]
    fn an_image_the_body_can_reference_is_not_an_attachment() {
        // The logo in an HTML mail: the body points at it as `cid:…`, so it is
        // part of the message, not something to save.  Senders write this both
        // with an explicit `inline` disposition and with none at all.
        let referenced = LeafPart {
            major_type: "image",
            has_filename: true,
            disposition: Some("inline"),
            has_content_id: true,
        };
        assert!(!referenced.is_attachment());
        assert!(
            !LeafPart {
                disposition: None,
                ..referenced
            }
            .is_attachment()
        );

        // Without a Content-ID nothing can reference it, so it stays an
        // attachment however the sender labelled it.
        assert!(
            LeafPart {
                has_content_id: false,
                ..referenced
            }
            .is_attachment()
        );

        // And a sender who says `attachment` outright is taken at their word,
        // Content-ID or not.
        assert!(
            LeafPart {
                disposition: Some("attachment"),
                ..referenced
            }
            .is_attachment()
        );
    }
}
