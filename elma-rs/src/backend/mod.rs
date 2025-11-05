//! Backend integration layer.
//!
//! The backend module defines the abstraction that the TUI uses to communicate with
//! different mail providers.  Backends expose a push-based event stream for mailbox
//! updates and a pull-based channel for action completion status.  The contract is
//! intentionally asynchronous so the UI thread never blocks on network or disk I/O.
//! See [`MailBackend`] for details about the required behaviour.

use crate::model::{Action, MailboxKind, Message, MessageContent, MessageId};
use anyhow::Result;
use std::sync::mpsc::Receiver;

pub mod gmail;
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
    pub content: String,
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
/// * `load_message` stays synchronous because the UI only invokes it when opening a
///   single message; implementors are free to run async code internally.
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
    fn load_message(&self, message_id: MessageId) -> Result<MessageContent>;
    /// Begin processing a batch of actions and return a channel with completion updates.
    fn apply_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>>;
    /// Send a fully composed message.
    fn send_message(&self, message: OutgoingMessage) -> Result<()>;
    /// Store a draft message.
    fn save_draft(&self, message: OutgoingMessage) -> Result<()>;
}
