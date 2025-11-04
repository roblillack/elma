use crate::model::{Action, Message, MessageContent, MessageId};
use anyhow::Result;
use std::sync::mpsc::Receiver;

pub mod gmail;
pub mod mock;

#[derive(Clone, Debug)]
pub enum BackendEvent {
    NewMessage(Message),
    MessageFlagsChanged(Message),
    MessageDeleted(MessageId),
}

#[derive(Clone, Debug)]
pub struct ActionStatus {
    pub action: Action,
    pub result: std::result::Result<(), String>,
}

pub trait MailBackend: Send + Sync {
    fn load_inbox(&self) -> Result<(Vec<Message>, Receiver<BackendEvent>)>;
    fn load_message(&self, message_id: MessageId) -> Result<MessageContent>;
    fn apply_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>>;
}
