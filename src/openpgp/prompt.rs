//! Asking the user for a passphrase from a worker thread.
//!
//! Decrypting and signing happen where the backends do their work -- off the
//! UI thread, in calls that block until they are done.  When a key turns out
//! to be locked, the worker cannot draw a prompt itself: it posts a
//! [`PendingPassphrase`] to a [`PassphraseQueue`] and waits, and the UI picks
//! it up on its next tick, asks, and sends the answer back.

use std::sync::mpsc;
use std::time::Duration;
use zeroize::Zeroizing;

/// What the prompt should say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassphraseRequest {
    pub description: String,
    /// The previous answer to the same question was wrong.
    pub retry: bool,
}

/// Something that can come up with a passphrase.
pub trait PassphraseSource {
    /// The passphrase for `request`, or `None` when the user declines.
    fn passphrase(&mut self, request: &PassphraseRequest) -> Option<Zeroizing<String>>;
}

/// How long a worker waits for an answer before giving up.  Long enough to go
/// and find the passphrase; short enough that a prompt nobody sees does not
/// hold a backend forever.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A question waiting for the UI.
pub struct PendingPassphrase {
    pub request: PassphraseRequest,
    reply: mpsc::Sender<Option<Zeroizing<String>>>,
}

impl PendingPassphrase {
    pub fn answer(self, passphrase: Option<Zeroizing<String>>) {
        // The worker may have timed out meanwhile; nothing to do about that.
        let _ = self.reply.send(passphrase);
    }
}

/// A connected pair: workers ask through the first, the UI answers from the
/// second.
pub fn channel() -> (AskTheUser, PassphraseQueue) {
    let (sender, receiver) = mpsc::channel();
    (AskTheUser { sender }, PassphraseQueue { receiver })
}

/// The UI's end: questions waiting to be asked.
pub struct PassphraseQueue {
    receiver: mpsc::Receiver<PendingPassphrase>,
}

impl PassphraseQueue {
    /// The next question, if a worker is waiting on one.
    pub fn next_pending(&self) -> Option<PendingPassphrase> {
        self.receiver.try_recv().ok()
    }
}

/// The workers' end: asks the user through the UI.
pub struct AskTheUser {
    sender: mpsc::Sender<PendingPassphrase>,
}

impl PassphraseSource for AskTheUser {
    fn passphrase(&mut self, request: &PassphraseRequest) -> Option<Zeroizing<String>> {
        let (reply, answer) = mpsc::channel();
        self.sender
            .send(PendingPassphrase {
                request: request.clone(),
                reply,
            })
            .ok()?;
        answer.recv_timeout(ANSWER_TIMEOUT).ok().flatten()
    }
}
