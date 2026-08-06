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

/// Abstraction over a mail provider implementation.
///
/// The trait is purposely synchronous from the caller's perspective while the
/// implementation is free to spawn its own async tasks.  `apply_actions` returns an
/// `mpsc::Receiver` for streaming status updates; the UI polls that receiver every
/// frame so the terminal stays responsive during long-running commits.
///
/// # Design Notes
///
/// * `load_inbox` produces both the initial message list and a channel that streams
///   [`BackendEvent`] updates.  This ensures there is a single source of truth for
///   mailbox mutations.
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
/// #     fn load_inbox(&self) -> anyhow::Result<(MailboxSnapshot, std::sync::mpsc::Receiver<_>)> {
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

    /// Load the default inbox and return a channel that streams [`BackendEvent`] updates.
    fn load_inbox(&self) -> Result<(MailboxSnapshot, Receiver<BackendEvent>)> {
        self.load_mailbox(MailboxKind::Inbox)
    }
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
