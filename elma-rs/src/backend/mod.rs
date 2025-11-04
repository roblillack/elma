use crate::model::{Action, Message, MessageContent, MessageId};
use anyhow::Result;
use std::sync::mpsc::Receiver;

pub mod mock;

#[derive(Clone, Debug)]
pub enum BackendEvent {
    NewMessage(Message),
}

pub trait MailBackend: Send + Sync {
    fn load_inbox(&self) -> Result<(Vec<Message>, Receiver<BackendEvent>)>;
    fn load_message(&self, message_id: MessageId) -> Result<MessageContent>;
    fn apply_action(&self, action: &Action) -> Result<()>;
}
