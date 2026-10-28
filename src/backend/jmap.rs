//! JMAP backend built on the `jmap-client` crate.
//!
//! This implementation mirrors the asynchronous behaviour of the Gmail backend
//! while talking to FastMail (or any JMAP-compliant provider).  Mailboxes and
//! messages are synchronised over the JMAP HTTP API and background updates are
//! processed on an internal Tokio runtime so the UI thread remains responsive.

use crate::{
    backend::{
        ActionStatus, BackendEvent, FAILURES_BEFORE_REPORT, LeafPart, MailBackend, MailboxSnapshot,
        OutgoingMessage, PartRole, build_compose_body, compose_address, mailer_header,
        oauth::OAuthCredential, retry_backoff,
    },
    model::{
        Action, ActionType, MailboxKind, Message, MessageAttachment, MessageContent,
        MessageContentPart, MessageId, MessageStatus,
    },
};
use anyhow::{Context, Result, anyhow};
use jmap_client::{
    Method, Set as JmapSet, URI,
    client::{Client, Credentials},
    core::{
        error::MethodErrorType,
        response::{EmailSetResponse, EmailSubmissionSetResponse, TaggedMethodResponse},
        session::Session,
        set::SetObject,
    },
    email::{self, Email as JmapEmail, EmailBodyPart, Property as EmailProperty},
    identity::Property as IdentityProperty,
    mailbox::{Mailbox as JmapMailbox, Property as MailboxProperty, Role as MailboxRole},
};
use lettre::{Message as LettreEmail, message::Mailbox as LettreMailbox};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        mpsc,
    },
    time::Duration,
};
use time::OffsetDateTime;
use tokio::{runtime::Runtime, sync::Mutex as AsyncMutex, task::JoinHandle};

const INITIAL_FETCH_LIMIT: usize = 128;
const BACKFILL_BATCH_SIZE: usize = 128;
const MAX_BODY_VALUE_BYTES: usize = 512 * 1024;
const EVENT_IDLE_POLL: Duration = Duration::from_secs(45);
/// How long one JMAP request may take.
///
/// `jmap-client` builds a fresh HTTP client per call and applies a single
/// timeout to all of them, defaulting to ten seconds.  That is generous for a
/// method call and much too tight for a `Blob/download`: the same limit covers
/// reading the body, so a large attachment on a slow link never finished.  A
/// minute is long enough for the attachment and short enough that a request
/// which will never answer cannot hold the background poll for a stretch the
/// user would notice.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// What the copy in Sent carries once the message is on its way: read, and no
/// longer a draft.  The compose path imports the message with `$draft` set, and
/// delivery replaces the whole set with this one.
const SENT_KEYWORDS: [&str; 1] = ["$seen"];
/// What archiving takes a message out of: the inbox, and Trash or Spam for a
/// message that is being rescued from either.
const ARCHIVE_FROM: [MailboxKind; 3] = [MailboxKind::Inbox, MailboxKind::Trash, MailboxKind::Spam];
/// What moving a message back to the inbox takes it out of.
const RESTORE_FROM: [MailboxKind; 3] =
    [MailboxKind::Trash, MailboxKind::Spam, MailboxKind::Archive];
/// How many times a move reads a message's mailboxes and tries to write them
/// back before giving up on a server that keeps reporting changes in between.
const MOVE_ATTEMPTS: u32 = 3;

/// Debug builds trace the JMAP conversation into `jmap-log-<stamp>-<pid>.log`,
/// the counterpart of the IMAP trace the Gmail backend writes.
///
/// jmap-client owns the HTTP layer and offers no hook to observe it, so the
/// trace is written around the calls rather than underneath them: a `C->S` line
/// with the JMAP method and its arguments, and an `S->C` line with what came
/// back.  The credentials ride in the `Authorization` header, which never
/// reaches the trace -- headers are not logged at all.  The two ways one could
/// still slip through are covered explicitly: URLs are stripped of any userinfo
/// they carry, and the account's secret is registered with the log, so a
/// payload that contains it comes out redacted whatever produced it.
#[cfg(debug_assertions)]
mod debug_logging {
    use super::JmapError;
    use crate::backend::debug_log::{DebugLog, LogKind, REDACTED};
    use std::{fmt::Display, sync::Arc};

    /// Scope for lines that belong to the account rather than to one request.
    const SESSION_SCOPE: &str = "req#000";

    fn logger() -> Option<Arc<DebugLog>> {
        DebugLog::global(LogKind::Jmap).ok()
    }

    /// Register the account's password or token so it can never reach the
    /// trace, whichever payload it might turn up in.
    pub fn register_secret(secret: &str) {
        if let Some(logger) = logger() {
            logger.register_secret(secret);
        }
    }

    /// Note something that is not tied to a single request.
    pub fn log_session(payload: impl Display) {
        if let Some(logger) = logger() {
            logger.log_event(SESSION_SCOPE, "INFO", &payload.to_string());
        }
    }

    /// Start tracing one JMAP method call.
    pub fn request(method: &str, arguments: impl Display) -> JmapRequest {
        let logger = logger();
        let scope = match &logger {
            Some(logger) => format!("req#{:03}", logger.allocate_connection_id()),
            None => SESSION_SCOPE.to_owned(),
        };
        if let Some(logger) = &logger {
            logger.log_event(&scope, "C->S", &format!("{method} {arguments}"));
        }
        JmapRequest {
            logger,
            scope,
            method: method.to_owned(),
        }
    }

    /// A JMAP method call in flight.
    pub struct JmapRequest {
        logger: Option<Arc<DebugLog>>,
        scope: String,
        method: String,
    }

    impl JmapRequest {
        /// Record what the server made of the request, handing `result` back
        /// for the call site's usual error handling -- with the error wrapped
        /// on the way past, so the cause `jmap-client` drops on the floor
        /// survives the trip.  See [`JmapError`].
        pub fn response<T>(
            &self,
            result: Result<T, jmap_client::Error>,
            summary: impl FnOnce(&T) -> String,
        ) -> Result<T, JmapError> {
            let result = result.map_err(JmapError::from);
            if let Some(logger) = &self.logger {
                let payload = match &result {
                    Ok(value) => format!("{} -> {}", self.method, summary(value)),
                    // Flattened: the log writes a line at a time, and the cause
                    // is the half that says what actually went wrong.
                    Err(err) => format!("{} -> ERROR: {}", self.method, err.one_line()),
                };
                logger.log_event(&self.scope, "S->C", &payload);
            }
            result
        }
    }

    /// Strip the credentials a URL may carry in its userinfo
    /// (`https://user:password@host/...`) before it reaches the trace.
    pub fn redact_url(url: &str) -> String {
        let Some((scheme, rest)) = url.split_once("://") else {
            return url.to_owned();
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => rest.split_at(index),
            None => (rest, ""),
        };
        match authority.rsplit_once('@') {
            Some((_, host)) => format!("{scheme}://{REDACTED}@{host}{path}"),
            None => url.to_owned(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn redact_url_drops_userinfo() {
            assert_eq!(
                redact_url("https://user:s3cret@api.example.com/jmap/session"),
                "https://***@api.example.com/jmap/session"
            );
        }

        #[test]
        fn redact_url_keeps_urls_without_credentials() {
            let url = "https://api.fastmail.com/jmap/session";
            assert_eq!(redact_url(url), url);
        }

        #[test]
        fn redact_url_handles_userinfo_without_a_path() {
            assert_eq!(
                redact_url("https://user:s3cret@api.example.com"),
                "https://***@api.example.com"
            );
        }

        #[test]
        fn redact_url_leaves_non_urls_alone() {
            assert_eq!(redact_url("api.example.com"), "api.example.com");
        }
    }
}

#[cfg(not(debug_assertions))]
mod debug_logging {
    use super::JmapError;
    use std::fmt::Display;

    pub struct JmapRequest;

    pub fn register_secret(_secret: &str) {}

    pub fn log_session(_payload: impl Display) {}

    pub fn request(_method: &str, _arguments: impl Display) -> JmapRequest {
        JmapRequest
    }

    impl JmapRequest {
        pub fn response<T>(
            &self,
            result: Result<T, jmap_client::Error>,
            _summary: impl FnOnce(&T) -> String,
        ) -> Result<T, JmapError> {
            result.map_err(JmapError::from)
        }
    }

    pub fn redact_url(url: &str) -> String {
        url.to_owned()
    }
}

/// Authentication parameters for connecting to a JMAP server.
#[derive(Clone)]
pub enum JmapAuth {
    Basic {
        username: String,
        password: String,
    },
    Bearer {
        token: String,
    },
    /// A sign-in held outside the configuration file, which hands out access
    /// tokens and renews them as they run out.
    OAuth(Arc<OAuthCredential>),
}

impl JmapAuth {
    /// The secret to send right now.
    ///
    /// This is where an OAuth account notices its access token has aged out and
    /// trades the refresh token for a new one, so every path that reaches the
    /// server goes through here rather than around it.  `refused` says the
    /// server turned the last one down, which renews an OAuth token whatever
    /// its expiry claims; a password or API token is what it is either way.
    async fn secret_now(&self, refused: bool) -> Result<String> {
        Ok(match self {
            JmapAuth::Basic { password, .. } => password.clone(),
            JmapAuth::Bearer { token } => token.clone(),
            JmapAuth::OAuth(credential) if refused => credential.renewed_access_token().await?,
            JmapAuth::OAuth(credential) => credential.access_token().await?,
        })
    }

    /// The header for `secret`, which must have come from [`Self::secret_now`].
    fn credentials_with(&self, secret: &str) -> Credentials {
        match self {
            JmapAuth::Basic { username, .. } => Credentials::basic(username, secret),
            JmapAuth::Bearer { .. } | JmapAuth::OAuth(_) => Credentials::bearer(secret),
        }
    }

    /// What was sent, for an error message the user can act on.
    fn credential_description(&self) -> String {
        match self {
            JmapAuth::Basic { username, .. } => format!("password for {username}"),
            JmapAuth::Bearer { .. } => "API token".to_string(),
            JmapAuth::OAuth(credential) => {
                format!("access token signed in for {}", credential.username())
            }
        }
    }

    /// The other credential, in case the server wanted that one.
    fn rejection_hint(&self) -> &'static str {
        match self {
            JmapAuth::Basic { .. } => {
                "if the server expects an API token instead, configure it as `token = \"...\"`"
            }
            JmapAuth::Bearer { .. } => {
                "if the server expects a password instead, configure it as `password = \"...\"`"
            }
            JmapAuth::OAuth(_) => {
                "the sign-in may have been revoked or had its access withdrawn -- \
                 sign in again with `elma --login`"
            }
        }
    }
}

/// Written out by hand rather than derived: the derived version would put the
/// password or token into whatever is being formatted.
impl std::fmt::Debug for JmapAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JmapAuth::Basic { username, .. } => write!(f, "Basic {{ username: {username:?} }}"),
            JmapAuth::Bearer { .. } => write!(f, "Bearer"),
            JmapAuth::OAuth(credential) => {
                write!(f, "OAuth {{ username: {:?} }}", credential.username())
            }
        }
    }
}

impl std::fmt::Display for JmapAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JmapAuth::Basic { username, .. } => write!(f, "Basic ({username})"),
            JmapAuth::Bearer { .. } => write!(f, "Bearer"),
            JmapAuth::OAuth(credential) => write!(f, "OAuth ({})", credential.username()),
        }
    }
}

/// A `jmap-client` error that keeps hold of the cause it hides.
///
/// `jmap_client::Error` implements `std::error::Error` with an empty body, so
/// its `source()` is always `None` and the reqwest error behind
/// `Error::Transport` gets no further than the one line reqwest prints for
/// itself: "error sending request for url (...)".  That line reads exactly the
/// same whether the request timed out, was refused, never resolved or lost its
/// TLS handshake, which left every transport failure looking identical in the
/// log and in front of the user.  Restating the source puts the rest of the
/// chain back, so `{err:?}` says which of them it was.
#[derive(Debug)]
pub struct JmapError(jmap_client::Error);

impl JmapError {
    /// The message and everything behind it on one line, for the debug log --
    /// which writes a line at a time and has no room for anyhow's layout.
    fn one_line(&self) -> String {
        let mut text = self.to_string();
        let mut cause = std::error::Error::source(self);
        while let Some(err) = cause {
            text.push_str(&format!(": {err}"));
            cause = std::error::Error::source(err);
        }
        text
    }
}

impl std::fmt::Display for JmapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for JmapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            // One link further down than one might expect: `jmap_client::Error`
            // quotes reqwest's message inside its own, so handing back the
            // reqwest error itself would only say the same thing twice.  What
            // it has to add starts at its cause.
            jmap_client::Error::Transport(err) => std::error::Error::source(err),
            _ => None,
        }
    }
}

impl From<jmap_client::Error> for JmapError {
    fn from(err: jmap_client::Error) -> Self {
        Self(err)
    }
}

/// Say why the session could not be built, in one line.
///
/// A credential the server turns down arrives as a bare `401 Unauthorized` that
/// names neither the account nor what was sent, and the mailbox loader shows an
/// error's message rather than walking its causes -- so which credential went
/// out, and which one to reach for instead, are folded into the message itself.
fn connect_error(err: JmapError, auth: &JmapAuth, base_url: &str) -> anyhow::Error {
    let url = debug_logging::redact_url(base_url);
    match http_status(&err) {
        Some(status @ (401 | 403)) => anyhow::Error::new(CredentialRefused(format!(
            "JMAP server at {url} rejected the {credential} (HTTP {status}); {hint}",
            credential = auth.credential_description(),
            hint = auth.rejection_hint(),
        ))),
        // Flattened rather than left for anyhow to lay out: the mailbox loader
        // shows an error's message and not its causes, so a reason that stayed
        // one link down would never be seen.
        _ => anyhow!("connecting to JMAP server at {url}: {}", err.one_line()),
    }
}

/// The account the session names as primary for mail.
///
/// A session names a primary account per capability, and they need not all be
/// the same: FastMail's lists a second account next to the mail one, and the
/// mail methods turn its ID down with `accountNotSupportedByMethod`.
fn mail_account(session: &Session) -> Option<&str> {
    session
        .primary_accounts()
        .find(|(capability, _)| capability.as_str() == URI::Mail.as_ref())
        .map(|(_, account)| account.as_str())
}

/// Point the client's requests at the mail account.
///
/// Left to itself, `jmap-client` uses whichever primary account its hash map
/// yields first -- and the map is seeded afresh in every process, so with more
/// than one account on offer, where the mail methods went changed from one
/// start to the next.  A session without a mail account keeps that default,
/// for want of anything better.
fn use_mail_account(client: &mut Client) {
    let session = client.session();
    match mail_account(&session) {
        Some(account) => {
            client.set_default_account_id(account);
        }
        None => debug_logging::log_session(format_args!(
            "the session names no primary account for mail; using {}",
            client.default_account_id()
        )),
    }
}

/// A session fetch the server refused, already worded for the user by
/// [`connect_error`].
///
/// A type of its own, with nothing behind it, so the refusal can still be told
/// apart from other failures once it has been folded into one line -- the
/// mailbox loader prints an error's causes, and the `jmap-client` error would
/// only repeat the status code there.
#[derive(Debug)]
struct CredentialRefused(String);

impl std::fmt::Display for CredentialRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CredentialRefused {}

/// Whether `err` is the server refusing the credential rather than anything
/// else that can go wrong on the way to it.
fn credential_was_refused(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.is::<CredentialRefused>()
            || cause
                .downcast_ref::<JmapError>()
                .is_some_and(|err| matches!(http_status(err), Some(401 | 403)))
    })
}

/// The HTTP status behind a `jmap-client` error, for the errors that carry one.
fn http_status(err: &JmapError) -> Option<u16> {
    match &err.0 {
        jmap_client::Error::Transport(err) => err.status().map(|status| status.as_u16()),
        jmap_client::Error::Problem(problem) => {
            problem.status().and_then(|status| status.try_into().ok())
        }
        // A refusal that is not `application/problem+json` -- which is what a
        // reverse proxy in front of the JMAP server tends to produce -- reaches
        // us as the status line on its own, `401 Unauthorized`.
        jmap_client::Error::Server(message) => message
            .split_whitespace()
            .next()
            .and_then(|code| code.parse().ok()),
        _ => None,
    }
}

/// Wait for `delay`, giving up early if the loop has been cancelled.  Answers
/// whether the caller should carry on.
///
/// Sliced rather than slept in one go so a cancellation is picked up within the
/// second: the backoff climbs into the minutes, and a task that only looked at
/// its flag on waking would keep the folder it was polling pinned for that long
/// after the user had left it.
async fn sleep_unless_cancelled(cancel: &AtomicBool, delay: Duration) -> bool {
    const SLICE: Duration = Duration::from_secs(1);

    let mut remaining = delay;
    while !remaining.is_zero() {
        if cancel.load(AtomicOrdering::SeqCst) {
            return false;
        }
        let slice = remaining.min(SLICE);
        tokio::time::sleep(slice).await;
        remaining -= slice;
    }
    !cancel.load(AtomicOrdering::SeqCst)
}

/// Configuration required to bootstrap the JMAP backend.
#[derive(Clone, Debug)]
pub struct JmapConfig {
    pub base_url: String,
    pub auth: JmapAuth,
    pub trusted_hosts: Vec<String>,
    /// The address the account is configured with, which decides the
    /// identity mail goes out as.  See [`choose_identity`].
    pub username: String,
}

/// Production backend that communicates with FastMail (or any other JMAP
/// provider) over HTTPS.
pub struct JmapBackend {
    runtime: Arc<Runtime>,
    config: JmapConfig,
    /// Connected session, built on first use rather than at construction.
    inner: Mutex<Option<Arc<JmapInner>>>,
    /// Set when the server refused the credential on the first connection, so
    /// the next attempt renews an OAuth token instead of presenting the same
    /// one again.  Once connected, [`JmapInner`] keeps track of this itself.
    refused_before_connecting: AtomicBool,
}

impl JmapBackend {
    /// Create a new backend instance bound to `config`.
    ///
    /// Nothing is sent over the network here: fetching the JMAP session object,
    /// the mailbox list and the identity all wait until the first call that
    /// needs them.  Constructing an account is what the startup path does before
    /// it can draw anything, so it has to stay off the network -- see
    /// [`Self::inner`].
    pub fn new(config: JmapConfig) -> Result<Self> {
        let runtime =
            Arc::new(Runtime::new().context("failed to create Tokio runtime for JMAP backend")?);

        Ok(Self {
            runtime,
            config,
            inner: Mutex::new(None),
            refused_before_connecting: AtomicBool::new(false),
        })
    }

    /// The connected session, connecting on the first call.
    ///
    /// Every trait method goes through here, and all of them are called from
    /// worker threads, so blocking on the connection is expected.  The lock is
    /// held across the connect so that two threads racing to be first produce
    /// one session rather than two; the loser waits and gets the same one.
    ///
    /// A session that is already up is checked for a credential that has moved
    /// on -- an OAuth access token about to run out -- and reconnected in place
    /// if so.  See [`JmapInner::ensure_authorized`].
    fn inner(&self) -> Result<Arc<JmapInner>> {
        let mut guard = self.inner.lock().unwrap();

        if let Some(inner) = guard.as_ref() {
            let inner = Arc::clone(inner);
            drop(guard);
            inner.noting_refusal(self.runtime.block_on(inner.ensure_authorized()))?;
            return Ok(inner);
        }

        let refused = self.refused_before_connecting.load(AtomicOrdering::SeqCst);
        let secret = self
            .runtime
            .block_on(self.config.auth.secret_now(refused))?;
        debug_logging::register_secret(&secret);

        // No context is added here: everything [`JmapInner::initialize`] can
        // fail at already names the server, and a wrapper would only hide the
        // rejected-credential message the status line is meant to show.
        let result = self.runtime.block_on(JmapInner::initialize(
            Arc::clone(&self.runtime),
            self.config.clone(),
            secret,
        ));
        self.refused_before_connecting.store(
            result.as_ref().is_err_and(credential_was_refused),
            AtomicOrdering::SeqCst,
        );
        let inner = result?;
        *guard = Some(Arc::clone(&inner));
        Ok(inner)
    }
}

impl MailBackend for JmapBackend {
    fn load_mailbox(
        &self,
        mailbox: MailboxKind,
    ) -> Result<(MailboxSnapshot, mpsc::Receiver<BackendEvent>)> {
        let (sender, receiver) = mpsc::channel();
        let inner = self.inner()?;

        let snapshot = inner.runtime.block_on(async {
            inner.set_current_mailbox(mailbox);
            inner.stop_backfill().await;

            // The new channel only goes live once the previous mailbox's
            // backfill is gone.  Installing it earlier hands that backfill's
            // in-flight batch -- fetched before it was cancelled, and carrying
            // sequence numbers from the *old* mailbox -- to the folder we are
            // switching to.
            {
                let mut guard = inner.events.lock().unwrap();
                *guard = Some(sender);
            }

            let sync = inner
                .sync_mailbox(mailbox)
                .await
                .context("loading mailbox contents")?;
            inner
                .start_event_loop()
                .await
                .context("starting JMAP event loop")?;
            inner
                .start_backfill_if_needed(mailbox)
                .await
                .context("starting JMAP backfill")?;

            Ok::<_, anyhow::Error>(MailboxSnapshot {
                total: sync.total,
                messages: sync.messages,
            })
        });
        let snapshot = inner.noting_refusal(snapshot)?;

        Ok((snapshot, receiver))
    }

    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        let inner = self.inner()?;
        inner.noting_refusal(inner.runtime.block_on(inner.load_message(message_id)))
    }

    fn apply_actions(&self, actions: Vec<Action>) -> Result<mpsc::Receiver<ActionStatus>> {
        let (tx, rx) = mpsc::channel();
        let inner = self.inner()?;
        let runtime = Arc::clone(&inner.runtime);

        runtime.spawn(async move {
            let mut refresh_needed = false;
            for action in actions {
                let result = inner.noting_refusal(inner.process_action(action.clone()).await);
                if result.is_ok() {
                    refresh_needed = true;
                }
                if tx
                    .send(ActionStatus {
                        action,
                        result: result.map_err(|err| err.to_string()),
                    })
                    .is_err()
                {
                    break;
                }
            }

            // Logged and no more: every action reported its own outcome
            // through `ActionStatus` already, and the poll behind this one
            // picks up whatever the refresh missed.
            if refresh_needed
                && let Err(err) = inner.noting_refusal(inner.refresh_current_mailbox().await)
            {
                debug_logging::log_session(format_args!("refresh after actions failed: {err:?}"));
            }
        });

        Ok(rx)
    }

    fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        let inner = self.inner()?;
        let result = inner
            .runtime
            .block_on(Arc::clone(&inner).send_message(message));
        inner.noting_refusal(result)
    }

    fn save_draft(&self, message: OutgoingMessage) -> Result<()> {
        let inner = self.inner()?;
        let result = inner
            .runtime
            .block_on(Arc::clone(&inner).save_draft(message));
        inner.noting_refusal(result)
    }

    fn fetch_attachment_blob(&self, blob_id: &str) -> Result<Vec<u8>> {
        let inner = self.inner()?;
        let client = inner.client();
        let result = inner.runtime.block_on(async {
            let trace = debug_logging::request("Blob/download", format_args!("blobId={blob_id}"));
            trace
                .response(client.download(blob_id).await, |blob| {
                    format!("bytes={}", blob.len())
                })
                .with_context(|| format!("downloading JMAP blob {blob_id}"))
        });
        inner.noting_refusal(result)
    }
}

struct JmapInner {
    runtime: Arc<Runtime>,
    /// What the session was set up with, kept for connecting again.
    config: JmapConfig,
    /// The connected client.  Swapped for a new one when the credential
    /// changes, which is why it is reached through [`Self::client`]; calls
    /// already in flight finish on the one they started with.
    client: Mutex<Arc<Client>>,
    /// The secret the current client's `Authorization` header was built from.
    /// An access token that has since been refreshed no longer matches, which
    /// is how [`Self::ensure_authorized`] knows to connect again.  Held across
    /// the check and the reconnect, so one caller renews and the rest wait for
    /// what it got -- a refresh token that rotates can only be spent once.
    authorized_with: AsyncMutex<String>,
    /// Set when the server turned the current client's credential down, so the
    /// next call connects again instead of retrying with something it has
    /// stopped accepting.
    rejected: AtomicBool,
    mailboxes: AsyncMutex<MailboxCache>,
    state: AsyncMutex<JmapState>,
    identity: IdentityInfo,
    events: Mutex<Option<mpsc::Sender<BackendEvent>>>,
    // Read by `emit_event` on every emission, so this is a plain mutex: the
    // async one would force `emit_event` and its callers to become async.
    current_mailbox: Mutex<MailboxKind>,
    event_handle: AsyncMutex<Option<JoinHandle<()>>>,
    event_cancel: AsyncMutex<Option<Arc<AtomicBool>>>,
    backfill_handle: AsyncMutex<Option<JoinHandle<()>>>,
    backfill_cancel: AsyncMutex<Option<Arc<AtomicBool>>>,
}

impl JmapInner {
    async fn initialize(
        runtime: Arc<Runtime>,
        config: JmapConfig,
        secret: String,
    ) -> Result<Arc<Self>> {
        let client = Arc::new(Self::connect(&config, &secret).await?);

        let mailboxes = Self::load_mailboxes(&client).await?;
        let identity = Self::load_identity(&client, &config.username).await?;

        Ok(Arc::new(Self {
            runtime,
            config,
            client: Mutex::new(client),
            authorized_with: AsyncMutex::new(secret),
            rejected: AtomicBool::new(false),
            mailboxes: AsyncMutex::new(mailboxes),
            state: AsyncMutex::new(JmapState::default()),
            identity,
            events: Mutex::new(None),
            current_mailbox: Mutex::new(MailboxKind::Inbox),
            event_handle: AsyncMutex::new(None),
            event_cancel: AsyncMutex::new(None),
            backfill_handle: AsyncMutex::new(None),
            backfill_cancel: AsyncMutex::new(None),
        }))
    }

    /// Fetch the session object with `secret` in the `Authorization` header.
    async fn connect(config: &JmapConfig, secret: &str) -> Result<Client> {
        let JmapConfig {
            base_url,
            auth,
            trusted_hosts,
            ..
        } = config;

        let mut builder = Client::new()
            .credentials(auth.credentials_with(secret))
            .timeout(REQUEST_TIMEOUT);
        if !trusted_hosts.is_empty() {
            builder = builder.follow_redirects(trusted_hosts.iter().cloned());
        }

        let trace = debug_logging::request(
            "Session/get",
            format_args!("url={} auth={auth}", debug_logging::redact_url(base_url)),
        );
        let connected = builder.connect(base_url).await.map(|mut client| {
            use_mail_account(&mut client);
            client
        });
        trace
            .response(connected, |client| {
                format!(
                    "account={} api={}",
                    client.default_account_id(),
                    debug_logging::redact_url(client.session().api_url())
                )
            })
            .map_err(|err| connect_error(err, auth, base_url))
    }

    /// The client to send the next request with.
    fn client(&self) -> Arc<Client> {
        Arc::clone(&self.client.lock().unwrap())
    }

    /// Make sure the client carries a credential the server will take.
    ///
    /// For an OAuth account this is where an access token about to run out is
    /// traded for a new one.  `jmap-client` fixes the `Authorization` header
    /// when it connects, so a new token means connecting again -- but only the
    /// client is replaced.  The message ids the UI holds, the mailbox cache, the
    /// event channel and the tasks feeding it all belong to the account rather
    /// than to the header, and carry on as they were: throwing them away with
    /// the old client would leave the UI holding ids nothing recognises, and
    /// nothing polling for new mail until the next mailbox switch.
    ///
    /// Called before every request the UI makes and before every background
    /// poll, so a token is renewed whichever of the two comes first.
    async fn ensure_authorized(&self) -> Result<()> {
        let mut authorized_with = self.authorized_with.lock().await;

        let rejected = self.rejected.load(AtomicOrdering::SeqCst);
        let secret = self.config.auth.secret_now(rejected).await?;
        debug_logging::register_secret(&secret);
        if *authorized_with == secret && !rejected {
            return Ok(());
        }

        debug_logging::log_session("credential changed, reconnecting the JMAP session");
        let client = Self::connect(&self.config, &secret).await?;
        *self.client.lock().unwrap() = Arc::new(client);
        *authorized_with = secret;
        self.rejected.store(false, AtomicOrdering::SeqCst);
        Ok(())
    }

    /// Hand `result` back, marking the client refused if the server turned its
    /// credential down.
    ///
    /// A refused credential will not start working on a retry with the same
    /// client, so the next call -- the user's or the poll loop's -- renews it
    /// and connects again first.  See [`Self::ensure_authorized`].
    fn noting_refusal<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(err) = &result
            && credential_was_refused(err)
        {
            self.rejected.store(true, AtomicOrdering::SeqCst);
            debug_logging::log_session(
                "server refused the credential; renewing it on the next attempt",
            );
        }
        result
    }

    fn set_current_mailbox(&self, mailbox: MailboxKind) {
        let mut guard = self.current_mailbox.lock().unwrap();
        *guard = mailbox;
    }

    fn current_mailbox(&self) -> MailboxKind {
        *self.current_mailbox.lock().unwrap()
    }

    async fn sync_mailbox(&self, mailbox: MailboxKind) -> Result<MailboxSync> {
        let FetchedMailbox {
            total,
            emails,
            has_more,
            start_position,
        } = self.fetch_mailbox(mailbox).await?;

        let mut state = self.state.lock().await;
        let switching = state.current_mailbox != Some(mailbox);
        if switching {
            state.current_sequence.clear();
            state.highest_received_index = 0;
            state.current_mailbox = Some(mailbox);
        }
        let mailbox_cache = self.mailboxes.lock().await.clone();
        let mut added_ids = Vec::new();
        let mut updated_ids = Vec::new();

        // New to this listing, not merely new to the session: a message seen
        // in another folder earlier still has to be announced when it turns up
        // here, or it never reaches the list the user is looking at.
        let listed: HashSet<MessageId> = state.current_sequence.iter().copied().collect();
        let coverage = PageCoverage::of(&emails, start_position, has_more);
        let page_end = start_position + emails.len();
        let mut page = Vec::with_capacity(emails.len());
        let mut page_ids = Vec::with_capacity(emails.len());

        for (index, data) in emails.into_iter().enumerate() {
            let (message_id, uid) = state.ensure_ids(&data.jmap_id);
            let seq = total.saturating_sub(start_position + index).max(1) as u32;
            let message = build_message(message_id, uid, seq, &data, &mailbox_cache, mailbox)?;

            if !listed.contains(&message_id) {
                added_ids.push(message_id);
            } else if let Some(previous) = state.messages.get(&message_id)
                && flags_changed(&previous.message, &message)
            {
                updated_ids.push(message_id);
            }

            page_ids.push(message_id);
            page.push((
                message_id,
                StoredMessage {
                    message,
                    jmap_id: data.jmap_id,
                    received_at: data.received_at,
                },
            ));
        }

        let removed = state.apply_page(page, &coverage, page_end);
        let more_available = has_more && state.highest_received_index < total;
        state.set_more_available(more_available);

        let mut new_messages = Vec::with_capacity(page_ids.len());
        for id in &page_ids {
            if let Some(stored) = state.messages.get(id) {
                new_messages.push(stored.message.clone());
            }
        }
        let mut added = Vec::with_capacity(added_ids.len());
        for id in added_ids {
            if let Some(stored) = state.messages.get(&id) {
                added.push(stored.message.clone());
            }
        }
        let mut updated = Vec::with_capacity(updated_ids.len());
        for id in updated_ids {
            if let Some(stored) = state.messages.get(&id) {
                updated.push(stored.message.clone());
            }
        }

        new_messages.sort_by_key(|msg| msg.seq);
        added.sort_by_key(|msg| msg.seq);
        updated.sort_by_key(|msg| msg.seq);

        drop(state);

        self.update_mailbox_total(mailbox, total).await;

        Ok(MailboxSync {
            total,
            messages: new_messages,
            added,
            updated,
            removed,
        })
    }

    async fn refresh_current_mailbox(self: &Arc<Self>) -> Result<()> {
        let mailbox = self.current_mailbox();
        self.stop_backfill().await;
        let sync = self.sync_mailbox(mailbox).await?;
        self.emit_diff(mailbox, sync);
        self.start_backfill_if_needed(mailbox).await?;
        Ok(())
    }

    async fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        let jmap_id = {
            let state = self.state.lock().await;
            state
                .jmap_to_id
                .iter()
                .find_map(|(jid, id)| (*id == message_id).then_some(jid.clone()))
                .ok_or_else(|| anyhow!("message {message_id} not found in cache"))?
        };

        let client = self.client();
        let mut request = client.build();
        let get = request.get_email();
        get.ids([jmap_id.as_str()]);
        get.properties([
            EmailProperty::Id,
            EmailProperty::BodyStructure,
            EmailProperty::BodyValues,
            EmailProperty::TextBody,
            EmailProperty::HtmlBody,
            EmailProperty::Attachments,
            EmailProperty::HasAttachment,
            EmailProperty::To,
            EmailProperty::Cc,
            EmailProperty::Bcc,
            EmailProperty::Header(email::Header::as_text("X-Mailer", false)),
        ]);
        {
            let args = get.arguments();
            args.fetch_all_body_values(true)
                .max_body_value_bytes(MAX_BODY_VALUE_BYTES);
        }

        let trace = debug_logging::request(
            "Email/get",
            format_args!("id={jmap_id} properties=body maxBodyValueBytes={MAX_BODY_VALUE_BYTES}"),
        );
        let mut response = trace
            .response(request.send_get_email().await, |response| {
                format!("emails={}", response.list().len())
            })
            .context("fetching message body")?;

        let email = response
            .take_list()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("message body not returned by server"))?;

        build_message_content(&email)
    }

    async fn process_action(self: &Arc<Self>, action: Action) -> Result<()> {
        match action.action_type {
            ActionType::Delete => {
                self.move_exclusively(action.message_id, MailboxKind::Trash)
                    .await
            }
            ActionType::Archive => {
                self.move_to_mailbox(action.message_id, &ARCHIVE_FROM, MailboxKind::Archive)
                    .await
            }
            ActionType::MoveToInboxUnread => {
                self.move_to_mailbox(action.message_id, &RESTORE_FROM, MailboxKind::Inbox)
                    .await?;
                self.set_keyword(action.message_id, "$seen", false).await
            }
            ActionType::MoveToInboxRead => {
                self.move_to_mailbox(action.message_id, &RESTORE_FROM, MailboxKind::Inbox)
                    .await?;
                self.set_keyword(action.message_id, "$seen", true).await
            }
            ActionType::MarkAsRead => self.set_keyword(action.message_id, "$seen", true).await,
            ActionType::MarkAsUnread => self.set_keyword(action.message_id, "$seen", false).await,
            ActionType::MarkAsStarred => {
                self.set_keyword(action.message_id, "$flagged", true).await
            }
            ActionType::MarkAsUnstarred => {
                self.set_keyword(action.message_id, "$flagged", false).await
            }
            ActionType::MarkAsImportant => {
                self.set_keyword(action.message_id, "$important", true)
                    .await
            }
            ActionType::MarkAsUnimportant => {
                self.set_keyword(action.message_id, "$important", false)
                    .await
            }
            ActionType::MoveToSpam => {
                self.move_exclusively(action.message_id, MailboxKind::Spam)
                    .await
            }
        }
    }

    async fn send_message(self: Arc<Self>, outgoing: OutgoingMessage) -> Result<()> {
        let email = self
            .build_compose_email(outgoing, false)
            .context("building outgoing message")?;
        let raw = email.formatted();

        let drafts_id = self
            .mailboxes
            .lock()
            .await
            .id_for_kind(MailboxKind::Drafts)
            .cloned();
        let sent_id = self
            .mailboxes
            .lock()
            .await
            .id_for_kind(MailboxKind::Sent)
            .cloned();

        let draft_id = if let Some(drafts_id) = drafts_id {
            let trace = debug_logging::request(
                "Email/import",
                format_args!(
                    "bytes={} mailboxIds=[{drafts_id}] keywords=[$draft]",
                    raw.len()
                ),
            );
            let created = trace
                .response(
                    self.client()
                        .email_import(raw.clone(), [drafts_id.clone()], Some(["$draft"]), None)
                        .await,
                    |email| format!("id={:?}", email.id()),
                )
                .context("importing draft for submission")?;
            created
                .id()
                .map(|id| id.to_string())
                .ok_or_else(|| anyhow!("email import did not return an identifier"))?
        } else {
            let trace = debug_logging::request("Email/import", format_args!("bytes={}", raw.len()));
            let created = trace
                .response(
                    self.client()
                        .email_import(
                            raw.clone(),
                            std::iter::empty::<String>(),
                            None::<Vec<&str>>,
                            None,
                        )
                        .await,
                    |email| format!("id={:?}", email.id()),
                )
                .context("importing message for submission")?;
            created
                .id()
                .map(|id| id.to_string())
                .ok_or_else(|| anyhow!("email import did not return an identifier"))?
        };

        // The submission carries the follow-up edit to the message itself:
        // `onSuccessUpdateEmail` files the copy under Sent and drops the
        // `$draft` keyword in the same request that hands the message over for
        // delivery.  Doing that in two further round trips left a window in
        // which a delivered message still looked like a draft -- and when the
        // second one failed, it stayed that way for good.
        //
        // The update replaces `keywords` wholesale rather than patching
        // `keywords/$draft` away.  RFC 8621 allows only `true` as a keyword
        // value, so dropping one means patching it to `null` -- which
        // jmap-client cannot express, its patch map being an
        // `AHashMap<String, bool>` that can only ever send `false`.  FastMail
        // rejects that with `invalidProperties`.  Replacing the set outright
        // sidesteps the patch and is what marks the sent copy read.
        let client = self.client();
        let mut request = client.build();
        let create_id = {
            let submission = request.set_email_submission();
            let create_id = submission
                .create()
                .email_id(draft_id.clone())
                .identity_id(self.identity.id.clone())
                .create_id()
                .ok_or_else(|| anyhow!("email submission was built without a creation id"))?;

            describe_sent_copy(
                submission.arguments().on_success_update_email(&create_id),
                sent_id.as_deref(),
            );

            create_id
        };

        let trace = debug_logging::request(
            "EmailSubmission/set",
            format_args!(
                "create emailId={draft_id} identityId={} onSuccessUpdateEmail mailboxIds=[{}] keywords=[{}]",
                self.identity.id,
                sent_id.as_deref().unwrap_or("unchanged"),
                SENT_KEYWORDS.join(" "),
            ),
        );

        // `onSuccessUpdateEmail` makes the server answer with a second,
        // implicit `Email/set` response.  That rules out jmap-client's
        // `send_single`, which pops the *last* method response: it would hand
        // back the implicit update, and since one `SetResponse` deserialises
        // happily from another's JSON that arrives as a submission which was
        // simply never created -- a delivered message reported as a failure.
        let responses = trace
            .response(request.send().await, |response| {
                format!("responses={}", response.method_responses().len())
            })
            .context("creating JMAP email submission")?
            .unwrap_method_responses();

        let (submission, update) = split_submission_responses(responses)?;

        submission
            .ok_or_else(|| anyhow!("server did not answer the email submission"))?
            .created(&create_id)
            .context("submitting message for delivery")?;

        // Past this point the message is on its way, so a rejected update is
        // worth recording but not worth failing the send over: it leaves the
        // copy in Sent looking like a draft, and nothing more.
        if let Some(mut update) = update {
            match update.updated(&draft_id) {
                Ok(_) => debug_logging::log_session(format_args!(
                    "Email/set (implicit) id={draft_id} -> updated"
                )),
                Err(err) => debug_logging::log_session(format_args!(
                    "Email/set (implicit) id={draft_id} -> ERROR: {err}"
                )),
            }
        }

        Ok(())
    }

    async fn save_draft(self: Arc<Self>, outgoing: OutgoingMessage) -> Result<()> {
        let drafts_id = self
            .mailboxes
            .lock()
            .await
            .id_for_kind(MailboxKind::Drafts)
            .cloned()
            .ok_or_else(|| anyhow!("Drafts mailbox not available on account"))?;

        let email = self
            .build_compose_email(outgoing, true)
            .context("building draft message")?;
        let raw = email.formatted();

        let trace = debug_logging::request(
            "Email/import",
            format_args!(
                "bytes={} mailboxIds=[{drafts_id}] keywords=[$draft]",
                raw.len()
            ),
        );
        trace
            .response(
                self.client()
                    .email_import(raw, [drafts_id], Some(["$draft"]), None)
                    .await,
                |email| format!("id={:?}", email.id()),
            )
            .context("importing draft message")?;

        Ok(())
    }

    async fn start_event_loop(self: &Arc<Self>) -> Result<()> {
        let mut handle_guard = self.event_handle.lock().await;
        if handle_guard.is_some() {
            return Ok(());
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));
        {
            let mut cancel_guard = self.event_cancel.lock().await;
            *cancel_guard = Some(cancel_flag.clone());
        }

        let this = Arc::clone(self);
        let handle = self.runtime.spawn(async move {
            this.event_loop(cancel_flag).await;
        });

        *handle_guard = Some(handle);
        Ok(())
    }

    /// Poll the current mailbox for changes until the loop is cancelled.
    ///
    /// A poll that fails is not news on its own.  The machine wakes from sleep,
    /// the network changes, a request times out -- and the attempt behind it
    /// succeeds.  So a failure costs a backoff (see [`retry_backoff`]) and
    /// nothing else, and only once [`FAILURES_BEFORE_REPORT`] of them have gone
    /// by in a row does the UI hear about it, once for the whole spell rather
    /// than once per attempt.
    ///
    /// None of it is printed.  The terminal belongs to the renderer, which
    /// draws by diffing against what it last put on screen: a line written
    /// behind its back lands on top of the frame, in raw mode so every newline
    /// staircases across the width, and stays there until a resize forces a
    /// repaint.  What there is to say goes to the debug log and to the status
    /// line.
    async fn event_loop(self: Arc<Self>, cancel: Arc<AtomicBool>) {
        let mut failures: u32 = 0;
        let mut reported = false;

        loop {
            let delay = if failures == 0 {
                EVENT_IDLE_POLL
            } else {
                retry_backoff(failures)
            };
            if !sleep_unless_cancelled(&cancel, delay).await {
                return;
            }

            let poll = async {
                self.ensure_authorized().await?;
                self.refresh_current_mailbox().await
            };
            // A refusal still counts as a failure: if renewing the credential
            // does not help either, the status line has to say so.
            match self.noting_refusal(poll.await) {
                Ok(()) => {
                    if failures > 0 {
                        debug_logging::log_session(format_args!(
                            "background refresh recovered after {failures} failed attempt(s)"
                        ));
                    }
                    if reported {
                        self.emit_status(BackendEvent::SyncRecovered);
                    }
                    failures = 0;
                    reported = false;
                }
                Err(err) => {
                    failures += 1;
                    debug_logging::log_session(format_args!(
                        "background refresh failed ({failures} in a row): {err:?}"
                    ));
                    if failures >= FAILURES_BEFORE_REPORT && !reported {
                        reported = true;
                        self.emit_status(BackendEvent::SyncTrouble(format!(
                            "Not syncing: {}. Retrying...",
                            err.root_cause()
                        )));
                    }
                }
            }
        }
    }

    /// Tell the UI what a refresh found.
    ///
    /// Removals go first.  A new message carries the sequence number it has
    /// once they are gone, and the UI files it under that number: announced
    /// while a message that has left is still listed, it would land one row
    /// off and overwrite the message it found there.
    fn emit_diff(&self, mailbox: MailboxKind, sync: MailboxSync) {
        for id in sync.removed {
            self.emit_event(mailbox, BackendEvent::MessageDeleted(id));
        }
        for message in sync.added {
            self.emit_event(mailbox, BackendEvent::NewMessage(message));
        }
        for message in sync.updated {
            self.emit_event(mailbox, BackendEvent::MessageFlagsChanged(message));
        }
    }

    /// Hand `event` to the UI, but only while `mailbox` is still the one on
    /// screen.
    ///
    /// Every event carries the mailbox it was produced for.  Tasks outlive the
    /// folder that started them -- a backfill parked in a fetch, or the refresh
    /// `apply_actions` spawns without any cancellation at all -- and the sender
    /// is a single slot shared by all of them.  Without this check such a task
    /// delivers messages, and sequence numbers derived from the old mailbox's
    /// total, into whichever folder the user has since opened.
    fn emit_event(&self, mailbox: MailboxKind, event: BackendEvent) {
        if self.current_mailbox() != mailbox {
            return;
        }
        if let Some(sender) = self.events.lock().unwrap().as_ref() {
            let _ = sender.send(event);
        }
    }

    /// Hand `event` to the UI whichever mailbox is on screen.
    ///
    /// The check [`Self::emit_event`] makes is there to keep one folder's
    /// messages out of another's list.  How the connection is faring belongs to
    /// no folder in particular, so it goes out unfiltered -- and it has to,
    /// since a report that the account cannot be reached is worth just as much
    /// after the user has moved to another folder.
    fn emit_status(&self, event: BackendEvent) {
        if let Some(sender) = self.events.lock().unwrap().as_ref() {
            let _ = sender.send(event);
        }
    }

    /// File a message under `target` and nothing else: what deleting it and
    /// reporting it as spam do.
    ///
    /// RFC 8621 (section 2) counts a message that is in the trash and in
    /// another mailbox as being in both, and has clients show it in both.
    /// Moving a message to Trash or Spam while leaving its other mailboxes
    /// alone would keep it on display in every one of them, so these two moves
    /// replace the whole set rather than only taking it out of the folder it
    /// was in.
    async fn move_exclusively(&self, message_id: MessageId, target: MailboxKind) -> Result<()> {
        let target_id = self.mailbox_id(target).await?;
        let jmap_id = self.lookup_message(message_id).await?;

        let trace = debug_logging::request(
            "Email/set",
            format_args!("update id={jmap_id} mailboxIds=[{target_id}] ({target})"),
        );
        trace
            .response(
                self.client()
                    .email_set_mailboxes(&jmap_id, [target_id])
                    .await,
                |_| "updated".to_owned(),
            )
            .context("updating mailbox assignment")?;

        Ok(())
    }

    /// Move a message out of the mailboxes in `from` and into `target`,
    /// leaving every other mailbox it is in -- a label, a project folder --
    /// as it was.
    ///
    /// Taking one mailbox away is a patch to `mailboxIds/<id>: null`, which
    /// jmap-client cannot send: its patch map holds booleans, and `false` is
    /// not a value RFC 8621 allows there.  So the message's mailboxes are
    /// read, adjusted and written back whole.  The write carries the state the
    /// read saw (`ifInState`), and a server that has seen any change since
    /// turns it down rather than have it undo that change; the move then reads
    /// again and retries, a few times at most.
    async fn move_to_mailbox(
        &self,
        message_id: MessageId,
        from: &[MailboxKind],
        target: MailboxKind,
    ) -> Result<()> {
        let target_id = self.mailbox_id(target).await?;
        let source_ids = self.mailboxes.lock().await.ids_for_kinds(from);
        let jmap_id = self.lookup_message(message_id).await?;

        let mut attempt = 1;
        loop {
            let (state, current) = self.mailbox_ids_of(&jmap_id).await?;
            let moved = moved_membership(&current, &source_ids, &target_id);
            if moved.iter().collect::<HashSet<_>>() == current.iter().collect::<HashSet<_>>() {
                return Ok(());
            }

            let trace = debug_logging::request(
                "Email/set",
                format_args!(
                    "update id={jmap_id} mailboxIds=[{}] ({target}) ifInState={state}",
                    moved.join(" ")
                ),
            );
            let client = self.client();
            let mut request = client.build();
            let set = request.set_email();
            set.if_in_state(state);
            set.update(jmap_id.as_str())
                .mailbox_ids(moved.iter().map(String::as_str));
            let result = trace.response(
                match request.send_single::<EmailSetResponse>().await {
                    Ok(mut response) => response.updated(&jmap_id),
                    Err(err) => Err(err),
                },
                |_| "updated".to_owned(),
            );

            match result {
                Ok(_) => return Ok(()),
                Err(err) if is_state_mismatch(&err) && attempt < MOVE_ATTEMPTS => attempt += 1,
                Err(err) => return Err(err).context("updating mailbox assignment"),
            }
        }
    }

    /// The mailboxes a message is in, with the `Email` state they were read at.
    async fn mailbox_ids_of(&self, jmap_id: &str) -> Result<(String, Vec<String>)> {
        let client = self.client();
        let mut request = client.build();
        request
            .get_email()
            .ids([jmap_id])
            .properties([EmailProperty::MailboxIds]);

        let trace = debug_logging::request(
            "Email/get",
            format_args!("id={jmap_id} properties=mailboxIds"),
        );
        let mut response = trace
            .response(request.send_get_email().await, |response| {
                format!(
                    "state={} emails={}",
                    response.state(),
                    response.list().len()
                )
            })
            .with_context(|| format!("reading mailboxes of {jmap_id}"))?;

        let state = response.state().to_owned();
        let email = response
            .take_list()
            .pop()
            .ok_or_else(|| anyhow!("message {jmap_id} not found on server"))?;
        let mailbox_ids = email.mailbox_ids().into_iter().map(str::to_owned).collect();
        Ok((state, mailbox_ids))
    }

    async fn mailbox_id(&self, kind: MailboxKind) -> Result<String> {
        self.mailboxes
            .lock()
            .await
            .id_for_kind(kind)
            .cloned()
            .ok_or_else(|| anyhow!("mailbox {kind} is not available"))
    }

    /// Turn one keyword on or off for a message.
    ///
    /// Setting is a plain patch.  Clearing is not: RFC 8621 requires every
    /// value in `keywords` to be `true`, so a keyword is dropped by patching it
    /// to `null` -- and jmap-client models patches as `AHashMap<String, bool>`,
    /// which can only ever send `false`.  FastMail rejects that with
    /// `invalidProperties`, which is why un-starring, un-flagging and moving a
    /// message back to the inbox unread all failed against it.
    ///
    /// Clearing therefore reads the current keywords back and writes the whole
    /// set minus the one being dropped.  A keyword changed by someone else
    /// between the read and the write is lost, which is the lesser evil against
    /// the operation not working at all.
    async fn set_keyword(&self, message_id: MessageId, keyword: &str, value: bool) -> Result<()> {
        let jmap_id = self.lookup_message(message_id).await?;

        if value {
            let trace = debug_logging::request(
                "Email/set",
                format_args!("update id={jmap_id} keywords/{keyword}=true"),
            );
            trace
                .response(
                    self.client()
                        .email_set_keyword(&jmap_id, keyword, true)
                        .await,
                    |_| "updated".to_owned(),
                )
                .with_context(|| format!("setting keyword {keyword}"))?;
            return Ok(());
        }

        let remaining = self.keywords_without(&jmap_id, keyword).await?;
        let trace = debug_logging::request(
            "Email/set",
            format_args!("update id={jmap_id} keywords=[{}]", remaining.join(" ")),
        );
        trace
            .response(
                self.client()
                    .email_set_keywords(&jmap_id, remaining.iter().map(String::as_str))
                    .await,
                |_| "updated".to_owned(),
            )
            .with_context(|| format!("clearing keyword {keyword}"))?;
        Ok(())
    }

    /// The keywords the message currently carries, without `keyword`.
    async fn keywords_without(&self, jmap_id: &str, keyword: &str) -> Result<Vec<String>> {
        let trace = debug_logging::request(
            "Email/get",
            format_args!("id={jmap_id} properties=keywords"),
        );
        let email = trace
            .response(
                self.client()
                    .email_get(jmap_id, Some([EmailProperty::Keywords]))
                    .await,
                |email| match email {
                    Some(email) => format!("keywords=[{}]", email.keywords().join(" ")),
                    None => "not found".to_owned(),
                },
            )
            .with_context(|| format!("reading keywords of {jmap_id}"))?
            .ok_or_else(|| anyhow!("message {jmap_id} not found on server"))?;

        Ok(email
            .keywords()
            .into_iter()
            .filter(|existing| *existing != keyword)
            .map(str::to_owned)
            .collect())
    }

    async fn lookup_message(&self, message_id: MessageId) -> Result<String> {
        let state = self.state.lock().await;
        state
            .messages
            .get(&message_id)
            .map(|stored| stored.jmap_id.clone())
            .ok_or_else(|| anyhow!("message {message_id} not found in cache"))
    }

    async fn update_mailbox_total(&self, mailbox: MailboxKind, total: usize) {
        let mut cache = self.mailboxes.lock().await;
        cache.set_total(mailbox, total);
    }

    async fn stop_backfill(&self) {
        if let Some(cancel) = self.backfill_cancel.lock().await.take() {
            cancel.store(true, AtomicOrdering::SeqCst);
        }
        if let Some(handle) = self.backfill_handle.lock().await.take() {
            let _ = handle.await;
        }
    }

    async fn start_backfill_if_needed(self: &Arc<Self>, mailbox: MailboxKind) -> Result<()> {
        // `refresh_current_mailbox` picks its mailbox before a network round-trip
        // and cannot be cancelled, so it can arrive here after the user has moved
        // on.  Returning early keeps it from stopping the backfill the new folder
        // just started -- there is one handle slot, so that backfill would be
        // forgotten rather than paused -- and from downloading the folder we left.
        if self.current_mailbox() != mailbox {
            return Ok(());
        }

        let more_available = {
            let mut state = self.state.lock().await;
            if state.highest_received_index < state.current_sequence.len() {
                state.highest_received_index = state.current_sequence.len();
            }
            state.more_available()
        };

        if !more_available {
            self.stop_backfill().await;
            return Ok(());
        }

        self.stop_backfill().await;

        let cancel_flag = Arc::new(AtomicBool::new(false));
        {
            let mut cancel_guard = self.backfill_cancel.lock().await;
            *cancel_guard = Some(cancel_flag.clone());
        }

        let this = Arc::clone(self);
        let handle = self.runtime.spawn(async move {
            this.backfill_loop(mailbox, cancel_flag).await;
        });

        let mut handle_guard = self.backfill_handle.lock().await;
        *handle_guard = Some(handle);

        Ok(())
    }

    async fn backfill_loop(self: Arc<Self>, mailbox: MailboxKind, cancel: Arc<AtomicBool>) {
        loop {
            if cancel.load(AtomicOrdering::SeqCst) {
                break;
            }

            let cursor = {
                let state = self.state.lock().await;
                state.next_cursor_index()
            };

            let page = match self
                .fetch_mailbox_page(mailbox, cursor, BACKFILL_BATCH_SIZE)
                .await
            {
                Ok(page) => page,
                Err(err) => {
                    // Backfill is best-effort: the run stops here, and the next
                    // refresh starts a new one from where this got to.
                    debug_logging::log_session(format_args!("backfill fetch failed: {err:?}"));
                    break;
                }
            };

            let FetchedMailbox {
                total,
                emails,
                has_more,
                start_position,
            } = page;

            // The fetch above is a network round-trip, and the cancel flag is
            // otherwise only read at the top of the loop.  Bail out here rather
            // than fold a batch belonging to the mailbox we are leaving into the
            // shared state.
            if cancel.load(AtomicOrdering::SeqCst) {
                break;
            }

            if emails.is_empty() {
                self.state.lock().await.set_more_available(false);
                break;
            }

            self.update_mailbox_total(mailbox, total).await;

            let mailbox_cache = {
                let cache = self.mailboxes.lock().await;
                cache.clone()
            };

            let batch_len = emails.len();

            let prepared_entries = {
                let mut state = self.state.lock().await;
                let mut entries = Vec::new();
                for (offset, data) in emails.into_iter().enumerate() {
                    let (message_id, uid) = state.ensure_ids(&data.jmap_id);
                    if state.current_sequence.contains(&message_id) {
                        continue;
                    }
                    let seq = total.saturating_sub(start_position + offset).max(1) as u32;
                    match build_message(message_id, uid, seq, &data, &mailbox_cache, mailbox) {
                        Ok(message) => entries.push((
                            message_id,
                            StoredMessage {
                                message,
                                jmap_id: data.jmap_id,
                                received_at: data.received_at,
                            },
                        )),
                        Err(err) => {
                            debug_logging::log_session(format_args!(
                                "backfill could not build a message: {err:?}"
                            ));
                        }
                    }
                }
                entries
            };

            if prepared_entries.is_empty() {
                let mut state = self.state.lock().await;
                let new_highest = start_position + batch_len;
                if new_highest > state.highest_received_index {
                    state.highest_received_index = new_highest;
                }
                let more_pending = has_more && state.highest_received_index < total;
                state.set_more_available(more_pending);
                if !more_pending {
                    break;
                }
                // No new entries added but server claims more data; avoid tight loop.
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }

            let (emitted, more_pending) = {
                let mut state = self.state.lock().await;
                let new_highest = start_position + batch_len;
                if new_highest > state.highest_received_index {
                    state.highest_received_index = new_highest;
                }
                let more_pending = has_more && state.highest_received_index < total;
                state.set_more_available(more_pending);
                let emitted = state.append_backfill(prepared_entries);
                (emitted, more_pending)
            };

            if emitted.is_empty() {
                if !more_pending {
                    break;
                }
                continue;
            }

            for message in emitted {
                self.emit_event(mailbox, BackendEvent::NewMessage(message));
            }

            if !more_pending {
                break;
            }
        }
    }

    async fn fetch_mailbox(&self, mailbox: MailboxKind) -> Result<FetchedMailbox> {
        self.fetch_mailbox_page(mailbox, 0, INITIAL_FETCH_LIMIT)
            .await
    }

    async fn fetch_mailbox_page(
        &self,
        mailbox: MailboxKind,
        position: usize,
        limit: usize,
    ) -> Result<FetchedMailbox> {
        let cache = self.mailboxes.lock().await;
        let filter = cache.filter_for(mailbox)?;
        drop(cache);

        let client = self.client();
        let mut request = client.build();
        let query = request.query_email();
        if let Some(filter) = filter {
            query.filter(filter);
        }
        query
            .sort([email::query::Comparator::received_at().descending()])
            .limit(limit)
            .calculate_total(true);
        if position > 0 {
            query.position(position as i32);
        }
        let trace = debug_logging::request(
            "Email/query",
            format_args!("mailbox={mailbox} position={position} limit={limit}"),
        );
        let mut query_response = trace
            .response(request.send_query_email().await, |response| {
                format!("total={:?} ids={}", response.total(), response.ids().len())
            })
            .context("querying mailbox messages")?;
        let total = query_response
            .total()
            .unwrap_or(position + query_response.ids().len());
        let ids = query_response.take_ids();

        if ids.is_empty() {
            return Ok(FetchedMailbox {
                total,
                emails: Vec::new(),
                has_more: false,
                start_position: position,
            });
        }

        let client = self.client();
        let mut request = client.build();
        let get = request.get_email();
        get.ids(ids.iter().map(|id| id.as_str()));
        get.properties([
            EmailProperty::Id,
            EmailProperty::From,
            EmailProperty::To,
            EmailProperty::Subject,
            EmailProperty::SentAt,
            EmailProperty::ReceivedAt,
            EmailProperty::Size,
            EmailProperty::MailboxIds,
            EmailProperty::Keywords,
            EmailProperty::HasAttachment,
        ]);
        let trace = debug_logging::request(
            "Email/get",
            format_args!("mailbox={mailbox} ids={} properties=envelope", ids.len()),
        );
        let mut response = trace
            .response(request.send_get_email().await, |response| {
                format!("emails={}", response.list().len())
            })
            .context("fetching mailbox messages")?;

        let mut email_map: HashMap<String, FetchedEmail> = response
            .take_list()
            .into_iter()
            .filter_map(FetchedEmail::from_email)
            .map(|e| (e.jmap_id.clone(), e))
            .collect();
        let emails: Vec<FetchedEmail> = ids
            .iter()
            .filter_map(|id| email_map.remove(id.as_str()))
            .collect();

        let has_more = if let Some(total) = query_response.total() {
            position + ids.len() < total
        } else {
            ids.len() == limit
        };

        Ok(FetchedMailbox {
            total,
            emails,
            has_more,
            start_position: position,
        })
    }

    async fn load_mailboxes(client: &Client) -> Result<MailboxCache> {
        let mut request = client.build();
        let get = request.get_mailbox();
        get.properties([
            MailboxProperty::Id,
            MailboxProperty::Name,
            MailboxProperty::Role,
            MailboxProperty::TotalEmails,
        ]);
        let trace = debug_logging::request(
            "Mailbox/get",
            format_args!("properties=[id,name,role,totalEmails]"),
        );
        let mut response = trace
            .response(request.send_get_mailbox().await, |response| {
                format!("mailboxes={}", response.list().len())
            })
            .context("fetching available mailboxes")?;
        let mailboxes = response.take_list();
        Ok(MailboxCache::from_mailboxes(mailboxes))
    }

    async fn load_identity(client: &Client, username: &str) -> Result<IdentityInfo> {
        let mut request = client.build();
        request.add_capability(URI::Submission);
        let get = request.get_identity();
        get.properties([
            IdentityProperty::Id,
            IdentityProperty::Email,
            IdentityProperty::Name,
        ]);
        let trace =
            debug_logging::request("Identity/get", format_args!("properties=[id,email,name]"));
        let mut response = trace
            .response(request.send_get_identity().await, |response| {
                format!("identities={}", response.list().len())
            })
            .context("fetching account identity")?;
        // One that came back without the properties asked for cannot be sent
        // as, and is no reason to turn the account away when others can.
        let identities = response
            .take_list()
            .into_iter()
            .filter_map(|identity| {
                Some(IdentityInfo {
                    id: identity.id()?.to_string(),
                    email: identity.email()?.to_string(),
                    name: identity.name().map(|name| name.to_string()),
                })
            })
            .collect();

        let session = client.session();
        match choose_identity(identities, username, session.username()) {
            Some(identity) => {
                debug_logging::log_session(format_args!(
                    "sending as {} (identity {})",
                    identity.email, identity.id
                ));
                Ok(identity)
            }
            None => {
                let email = session.username().to_string();
                Ok(IdentityInfo {
                    id: email.clone(),
                    email,
                    name: None,
                })
            }
        }
    }

    /// Build the message to submit or, with `keep_bcc`, the draft to store.
    ///
    /// lettre leaves the `Bcc` header out by default; a draft has to keep it,
    /// being the only record of who it was going to be blind-copied to.
    fn build_compose_email(
        &self,
        outgoing: OutgoingMessage,
        keep_bcc: bool,
    ) -> Result<LettreEmail> {
        let OutgoingMessage {
            to,
            cc,
            bcc,
            subject,
            text_body,
            html_body,
            attachments,
        } = outgoing;

        if to.is_empty() && cc.is_empty() && bcc.is_empty() {
            return Err(anyhow!("message must have at least one recipient"));
        }

        let from_addr = LettreMailbox::new(
            self.identity.name.clone(),
            self.identity
                .email
                .parse()
                .with_context(|| format!("invalid identity email: {}", self.identity.email))?,
        );

        let mut builder = LettreEmail::builder()
            .from(from_addr)
            .raw_header(mailer_header());

        for addr in to {
            let mailbox: LettreMailbox = addr
                .parse()
                .with_context(|| format!("invalid To address: {addr}"))?;
            builder = builder.to(mailbox);
        }

        for addr in cc {
            let mailbox: LettreMailbox = addr
                .parse()
                .with_context(|| format!("invalid Cc address: {addr}"))?;
            builder = builder.cc(mailbox);
        }

        for addr in bcc {
            let mailbox: LettreMailbox = addr
                .parse()
                .with_context(|| format!("invalid Bcc address: {addr}"))?;
            builder = builder.bcc(mailbox);
        }

        if keep_bcc {
            builder = builder.keep_bcc();
        }

        builder = builder.subject(subject);

        let body = build_compose_body(text_body, html_body, attachments)?;

        builder.multipart(body).context("building MIME message")
    }
}

#[derive(Clone, Debug)]
struct IdentityInfo {
    id: String,
    email: String,
    name: Option<String>,
}

/// The identity mail goes out as: the one for the account's own address.
///
/// FastMail lists every address an account may send as -- aliases, other
/// domains -- in no promised order, so the first is not necessarily the
/// account's own.  The configured `username` is the address the user named;
/// the session's username is the server's idea of the same, for an account
/// configured with something other than an address.  Failing both, the first
/// identity is as good a guess as any.
fn choose_identity(
    mut identities: Vec<IdentityInfo>,
    username: &str,
    session_username: &str,
) -> Option<IdentityInfo> {
    let matching = [username, session_username].into_iter().find_map(|wanted| {
        identities
            .iter()
            .position(|identity| identity.email.eq_ignore_ascii_case(wanted))
    });
    match matching {
        Some(position) => Some(identities.swap_remove(position)),
        None => identities.into_iter().next(),
    }
}

#[derive(Clone)]
struct MailboxCache {
    by_id: HashMap<String, MailboxInfo>,
    by_kind: HashMap<MailboxKind, String>,
    totals_override: HashMap<MailboxKind, usize>,
}

impl MailboxCache {
    fn from_mailboxes(list: Vec<JmapMailbox>) -> Self {
        let mut by_id = HashMap::new();
        let mut by_kind = HashMap::new();

        for mailbox in list {
            if let Some(id) = mailbox.id() {
                let id = id.to_string();
                let role = mailbox.role();
                let name = mailbox.name().unwrap_or(id.as_str()).to_string();
                let total = mailbox.total_emails();
                by_id.insert(
                    id.clone(),
                    MailboxInfo {
                        name,
                        total_emails: total,
                    },
                );
                if let Some(kind) = kind_from_role(&role) {
                    by_kind.insert(kind, id);
                }
            }
        }

        Self {
            by_id,
            by_kind,
            totals_override: HashMap::new(),
        }
    }

    fn id_for_kind(&self, kind: MailboxKind) -> Option<&String> {
        self.by_kind.get(&kind)
    }

    /// The ids of those of `kinds` the account has a mailbox for.
    fn ids_for_kinds(&self, kinds: &[MailboxKind]) -> Vec<String> {
        kinds
            .iter()
            .filter_map(|kind| self.id_for_kind(*kind).cloned())
            .collect()
    }

    fn name_for_id(&self, id: &str) -> Option<&str> {
        self.by_id.get(id).map(|info| info.name.as_str())
    }

    fn set_total(&mut self, kind: MailboxKind, total: usize) {
        if let Some(id) = self.by_kind.get(&kind)
            && let Some(info) = self.by_id.get_mut(id)
        {
            info.total_emails = total;
            return;
        }
        self.totals_override.insert(kind, total);
    }

    fn filter_for(&self, kind: MailboxKind) -> Result<Option<email::query::Filter>> {
        Ok(match kind {
            MailboxKind::Starred => Some(email::query::Filter::has_keyword("$flagged")),
            MailboxKind::Important => {
                if let Some(id) = self.id_for_kind(MailboxKind::Important) {
                    Some(email::query::Filter::in_mailbox(id.clone()))
                } else {
                    Some(email::query::Filter::has_keyword("$important"))
                }
            }
            other => {
                let id = self
                    .id_for_kind(other)
                    .ok_or_else(|| anyhow!("mailbox {other} is not available"))?;
                Some(email::query::Filter::in_mailbox(id.clone()))
            }
        })
    }
}

#[derive(Clone)]
struct MailboxInfo {
    name: String,
    total_emails: usize,
}

#[derive(Default)]
struct JmapState {
    messages: HashMap<MessageId, StoredMessage>,
    jmap_to_id: HashMap<String, MessageId>,
    current_sequence: Vec<MessageId>,
    current_mailbox: Option<MailboxKind>,
    next_message_id: MessageId,
    next_uid: u32,
    more_available: bool,
    highest_received_index: usize,
}

impl JmapState {
    fn ensure_ids(&mut self, jmap_id: &str) -> (MessageId, u32) {
        if let Some(id) = self.jmap_to_id.get(jmap_id).copied() {
            let uid = self
                .messages
                .get(&id)
                .map(|stored| stored.message.uid)
                .unwrap_or_else(|| {
                    self.next_uid += 1;
                    self.next_uid - 1
                });
            (id, uid)
        } else {
            self.next_message_id = self.next_message_id.saturating_add(1).max(1);
            let id = self.next_message_id;
            self.next_uid = self.next_uid.saturating_add(1).max(1);
            let uid = self.next_uid;
            self.jmap_to_id.insert(jmap_id.to_string(), id);
            (id, uid)
        }
    }

    /// Fold a fresh look at the top of the open mailbox into its listing, and
    /// return the messages that look shows to have left.
    ///
    /// The page becomes the head of the listing.  A message listed before and
    /// missing from the page has only gone if the page would have had to show
    /// it (see [`PageCoverage`]); one further down may simply have slid past
    /// the end of the page, and stays.  Those that went leave the listing but
    /// not `messages`, so an action already queued against one still finds the
    /// email it was meant for.
    ///
    /// `page_end` is where the page stops in the mailbox.  The backfill cursor
    /// moves back a place for every message that left: each sat before it, so
    /// everything the backfill has yet to read is now that many places nearer
    /// the top, and a cursor left where it was would step over as many
    /// messages as had gone.
    fn apply_page(
        &mut self,
        page: Vec<(MessageId, StoredMessage)>,
        coverage: &PageCoverage,
        page_end: usize,
    ) -> Vec<MessageId> {
        let on_page: HashSet<MessageId> = page.iter().map(|(id, _)| *id).collect();
        let mut sequence: Vec<MessageId> = page.iter().map(|(id, _)| *id).collect();
        let mut removed = Vec::new();

        for id in std::mem::take(&mut self.current_sequence) {
            if on_page.contains(&id) {
                continue;
            }
            let received_at = self.messages.get(&id).and_then(|stored| stored.received_at);
            if coverage.covers(received_at) {
                removed.push(id);
            } else {
                sequence.push(id);
            }
        }

        for (id, stored) in page {
            self.messages.insert(id, stored);
        }
        self.current_sequence = sequence;
        self.highest_received_index = self
            .highest_received_index
            .saturating_sub(removed.len())
            .max(page_end);

        removed
    }

    fn append_backfill(&mut self, entries: Vec<(MessageId, StoredMessage)>) -> Vec<Message> {
        let mut new_ids = Vec::new();
        for (id, stored) in entries {
            let already_present = self.current_sequence.contains(&id);
            self.messages.insert(id, stored);
            if !already_present {
                self.current_sequence.push(id);
                new_ids.push(id);
            }
        }
        let mut emitted = Vec::with_capacity(new_ids.len());
        for id in new_ids {
            if let Some(stored) = self.messages.get(&id) {
                emitted.push(stored.message.clone());
            }
        }
        emitted.sort_by_key(|msg| msg.seq);
        emitted
    }

    fn set_more_available(&mut self, more: bool) {
        self.more_available = more;
    }

    fn more_available(&self) -> bool {
        self.more_available
    }

    fn next_cursor_index(&self) -> usize {
        self.highest_received_index
    }
}

struct StoredMessage {
    message: Message,
    jmap_id: String,
    /// The `receivedAt` the mailbox query sorts by, for telling whether a
    /// later page would have had to show this message.
    received_at: Option<i64>,
}

/// How much of the open mailbox one page of `Email/query` results vouches for.
///
/// The query sorts newest first by `receivedAt`, so a page that starts at the
/// top holds every message that arrived after the oldest one on it.  A message
/// listed earlier and missing from such a page has left the mailbox if it
/// arrived after that oldest one -- or at any time at all, when the page runs
/// to the end of the mailbox.  One that arrived at the very moment the oldest
/// did may have been sorted just past the end of the page, so it gets the
/// benefit of the doubt, as does one whose arrival time is not known.
struct PageCoverage {
    /// The page holds the whole mailbox.
    complete: bool,
    /// When the oldest message on the page arrived, for a page that starts at
    /// the top of the mailbox.
    oldest_received_at: Option<i64>,
}

impl PageCoverage {
    fn of(emails: &[FetchedEmail], start_position: usize, has_more: bool) -> Self {
        // A page from further down says nothing about what lies above it.
        if start_position > 0 {
            return Self {
                complete: false,
                oldest_received_at: None,
            };
        }
        Self {
            complete: !has_more,
            oldest_received_at: emails.iter().filter_map(|email| email.received_at).min(),
        }
    }

    /// Whether a message that arrived at `received_at` would be on the page if
    /// it were still in the mailbox.
    fn covers(&self, received_at: Option<i64>) -> bool {
        if self.complete {
            return true;
        }
        matches!(
            (self.oldest_received_at, received_at),
            (Some(oldest), Some(received)) if received > oldest
        )
    }
}

struct MailboxSync {
    total: usize,
    messages: Vec<Message>,
    added: Vec<Message>,
    updated: Vec<Message>,
    removed: Vec<MessageId>,
}

struct FetchedMailbox {
    total: usize,
    emails: Vec<FetchedEmail>,
    has_more: bool,
    start_position: usize,
}

struct FetchedEmail {
    jmap_id: String,
    from: Vec<String>,
    to: Vec<String>,
    subject: String,
    received_at: Option<i64>,
    sent_at: Option<i64>,
    size: usize,
    mailbox_ids: Vec<String>,
    keywords: Vec<String>,
    has_attachments: bool,
}

impl FetchedEmail {
    fn from_email(email: JmapEmail) -> Option<Self> {
        let jmap_id = email.id()?.to_string();
        let from = email
            .from()
            .unwrap_or_default()
            .iter()
            .map(format_address)
            .collect();
        let to = email
            .to()
            .unwrap_or_default()
            .iter()
            .map(format_address)
            .collect();
        let subject = email.subject().unwrap_or("").to_string();
        let received_at = email.received_at();
        let sent_at = email.sent_at();
        let size = email.size();
        // `jmap-client` keeps `mailboxIds` in an `AHashMap`, and hands them
        // over in that map's iteration order -- which is seeded afresh for
        // every response, so the same email comes back with its mailboxes in a
        // different order on every poll.  The order reaches the labels built
        // from these ids, and `flags_changed` compares those as a sequence, so
        // leaving it alone reports a flag change on every poll for any message
        // filed in more than one other mailbox -- and reshuffles the labels
        // under the reader while it is at it.  Sorting here is what makes both
        // stable.
        let mut mailbox_ids: Vec<String> = email
            .mailbox_ids()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        mailbox_ids.sort();
        let keywords = email
            .keywords()
            .into_iter()
            .map(|k| k.to_string())
            .collect();
        let has_attachments = email.has_attachment();
        Some(Self {
            jmap_id,
            from,
            to,
            subject,
            received_at,
            sent_at,
            size,
            mailbox_ids,
            keywords,
            has_attachments,
        })
    }
}

fn build_message(
    id: MessageId,
    uid: u32,
    seq: u32,
    data: &FetchedEmail,
    cache: &MailboxCache,
    mailbox: MailboxKind,
) -> Result<Message> {
    let sent = data
        .sent_at
        .or(data.received_at)
        .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok())
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);

    let sender = data
        .from
        .first()
        .cloned()
        .unwrap_or_else(|| "Unknown sender".to_string());

    let status = determine_status(data, cache);
    let starred = data
        .keywords
        .iter()
        .any(|kw| kw.eq_ignore_ascii_case("$flagged"));
    let important = data
        .keywords
        .iter()
        .any(|kw| kw.eq_ignore_ascii_case("$important"));
    let answered = data
        .keywords
        .iter()
        .any(|kw| kw.eq_ignore_ascii_case("$answered"));
    let forwarded = data
        .keywords
        .iter()
        .any(|kw| kw.eq_ignore_ascii_case("$forwarded"));

    // Every message listed in a mailbox is by definition part of it, so the
    // mailbox that is currently open never earns a label of its own — only the
    // *other* mailboxes a message belongs to carry information.  Matching by id
    // rather than by name also keeps this working for servers that hand out
    // localized mailbox names.
    let current_mailbox_id = cache.id_for_kind(mailbox);
    let mut labels = Vec::new();
    for mailbox_id in &data.mailbox_ids {
        if Some(mailbox_id) == current_mailbox_id {
            continue;
        }
        if let Some(name) = cache.name_for_id(mailbox_id) {
            labels.push(name.to_string());
        }
    }
    if starred
        && mailbox != MailboxKind::Starred
        && !labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case("Starred"))
    {
        labels.push("Starred".to_string());
    }
    if important
        && mailbox != MailboxKind::Important
        && !labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case("Important"))
    {
        labels.push("Important".to_string());
    }

    Ok(Message {
        id,
        sent,
        sender,
        recipients: data.to.clone(),
        subject: data.subject.clone(),
        size: data.size,
        starred,
        important,
        answered,
        forwarded,
        status,
        labels,
        uid,
        seq,
        has_attachments: data.has_attachments,
    })
}

fn determine_status(data: &FetchedEmail, cache: &MailboxCache) -> MessageStatus {
    let contains = |kind: MailboxKind| {
        if let Some(id) = cache.id_for_kind(kind) {
            data.mailbox_ids.iter().any(|entry| entry == id)
        } else {
            false
        }
    };

    if contains(MailboxKind::Trash) {
        MessageStatus::Deleted
    } else if contains(MailboxKind::Spam) {
        MessageStatus::Spam
    } else if !contains(MailboxKind::Inbox) && contains(MailboxKind::Archive) {
        MessageStatus::Archived
    } else if data
        .keywords
        .iter()
        .any(|kw| kw.eq_ignore_ascii_case("$seen"))
    {
        MessageStatus::Read
    } else {
        MessageStatus::New
    }
}

fn format_address(addr: &email::EmailAddress) -> String {
    match (addr.name(), addr.email()) {
        (Some(name), email) if !name.is_empty() => format!("{name} <{email}>"),
        (_, email) => email.to_string(),
    }
}

/// The mailboxes a message is in once it has moved to `target`: out of every
/// mailbox in `sources`, into `target`, and still in everything else.
fn moved_membership(current: &[String], sources: &[String], target: &str) -> Vec<String> {
    let mut moved: Vec<String> = current
        .iter()
        .filter(|id| !sources.contains(id))
        .cloned()
        .collect();
    if !moved.iter().any(|id| id == target) {
        moved.push(target.to_owned());
    }
    moved
}

/// Whether the server turned a write down because what it was based on has
/// changed since (`ifInState`).
fn is_state_mismatch(err: &JmapError) -> bool {
    matches!(
        &err.0,
        jmap_client::Error::Method(method) if *method.error() == MethodErrorType::StateMismatch
    )
}

fn flags_changed(before: &Message, after: &Message) -> bool {
    before.status != after.status
        || before.starred != after.starred
        || before.important != after.important
        || before.answered != after.answered
        || before.forwarded != after.forwarded
        || before.has_attachments != after.has_attachments
        || before.labels != after.labels
}

/// Describe what the copy of a sent message should look like once delivery has
/// been accepted: filed under Sent, read, and no longer a draft.
///
/// Both properties are written whole rather than patched.  A JMAP `PatchObject`
/// removes a member by setting it to `null`, and jmap-client's patch map is an
/// `AHashMap<String, bool>` that can only ever send `false` -- which RFC 8621
/// forbids as a keyword value and FastMail rejects with `invalidProperties`.
fn describe_sent_copy(update: &mut JmapEmail<JmapSet>, sent_id: Option<&str>) {
    if let Some(sent_id) = sent_id {
        update.mailbox_ids([sent_id]);
    }
    update.keywords(SENT_KEYWORDS);
}

/// Pick the `EmailSubmission/set` and the implicit `Email/set` out of what the
/// server answered, by type rather than by position.
///
/// `onSuccessUpdateEmail` earns a second method response, which is what rules
/// out jmap-client's `send_single`: it pops the *last* one, and a `SetResponse`
/// for one type parses happily from another's JSON, so the implicit update
/// arrives disguised as a submission that was never created.
fn split_submission_responses(
    responses: Vec<TaggedMethodResponse>,
) -> Result<(Option<EmailSubmissionSetResponse>, Option<EmailSetResponse>)> {
    let mut submission = None;
    let mut update = None;

    for response in responses {
        if response.is_type(Method::SetEmailSubmission) {
            submission = Some(
                response
                    .unwrap_set_email_submission()
                    .context("reading the email submission response")?,
            );
        } else if response.is_type(Method::SetEmail) {
            update = Some(
                response
                    .unwrap_set_email()
                    .context("reading the response to the message update")?,
            );
        }
    }

    Ok((submission, update))
}

fn build_message_content(email: &JmapEmail) -> Result<MessageContent> {
    let mailer = email
        .header(&email::Header::as_text("X-Mailer", false))
        .and_then(|value| match value {
            email::HeaderValue::AsText(text) => Some(text.clone()),
            email::HeaderValue::AsTextAll(list) => list.first().cloned(),
            _ => None,
        })
        .unwrap_or_default();

    let mut parts = Vec::new();

    if let Some(text_parts) = email.text_body() {
        collect_parts(email, text_parts, &mut parts);
    }
    if let Some(html_parts) = email.html_body() {
        collect_parts(email, html_parts, &mut parts);
    }

    // `attachments` is everything outside the text and HTML bodies, so it also
    // holds the images an HTML mail references as `cid:…`.  The server leaves
    // those out of `hasAttachment`, which is what the message list marker is
    // built from, so they are kept apart by their `inline` flag rather than
    // dropped: the marker ignores them, the save dialog does not.
    let attachments = email
        .attachments()
        .unwrap_or_default()
        .iter()
        .filter_map(|part| {
            let role = jmap_part_role(part);
            if role == PartRole::Body {
                return None;
            }
            Some(MessageAttachment {
                filename: part.name().map(|name| name.to_string()),
                mime_type: part
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                size: part.size(),
                data: None,
                blob_id: part.blob_id().map(|id| id.to_string()),
                inline: role == PartRole::Inline,
            })
        })
        .collect();

    Ok(MessageContent {
        mailer,
        parts,
        attachments,
        to: compose_addresses(email.to()),
        cc: compose_addresses(email.cc()),
        bcc: compose_addresses(email.bcc()),
    })
}

/// Recipients as the compose fields take them, quoted where the name needs it
/// -- unlike [`format_address`], which writes them for display.
fn compose_addresses(addresses: Option<&[email::EmailAddress]>) -> Vec<String> {
    addresses
        .unwrap_or_default()
        .iter()
        .map(|addr| compose_address(addr.name(), addr.email()))
        .collect()
}

/// What a JMAP body part is, by the rule every backend shares.
///
/// JMAP hands the decision over ready-made: `type`, `disposition` and `cid` are
/// separate properties, so there is nothing to parse out of a header.
fn jmap_part_role(part: &EmailBodyPart) -> PartRole {
    let content_type = part.content_type().unwrap_or("application/octet-stream");

    LeafPart {
        major_type: content_type.split('/').next().unwrap_or_default(),
        has_filename: part.name().is_some_and(|name| !name.trim().is_empty()),
        disposition: part.content_disposition(),
        has_content_id: part.content_id().is_some_and(|cid| !cid.trim().is_empty()),
    }
    .role()
}

fn collect_parts(
    email: &JmapEmail,
    segments: &[EmailBodyPart],
    parts: &mut Vec<MessageContentPart>,
) {
    for segment in segments {
        if let Some(part_id) = segment.part_id()
            && let Some(body) = email.body_value(part_id)
        {
            let content_type = segment.content_type().unwrap_or("text/plain").to_string();
            parts.push(MessageContentPart {
                content_type,
                content: body.value().as_bytes().to_vec(),
            });
        }
    }
}

fn kind_from_role(role: &MailboxRole) -> Option<MailboxKind> {
    match role {
        MailboxRole::Inbox => Some(MailboxKind::Inbox),
        MailboxRole::Archive => Some(MailboxKind::Archive),
        MailboxRole::Sent => Some(MailboxKind::Sent),
        MailboxRole::Drafts => Some(MailboxKind::Drafts),
        MailboxRole::Junk => Some(MailboxKind::Spam),
        MailboxRole::Trash => Some(MailboxKind::Trash),
        MailboxRole::Important => Some(MailboxKind::Important),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_auth() -> JmapAuth {
        JmapAuth::Basic {
            username: "rob@example.com".to_string(),
            password: "s3cret".to_string(),
        }
    }

    fn bearer_auth() -> JmapAuth {
        JmapAuth::Bearer {
            token: "an-api-token".to_string(),
        }
    }

    /// A configured password reaches the server as an HTTP Basic header, and a
    /// server that turns it down produces the message the status line shows.
    ///
    /// Driven through a socket on loopback rather than a stubbed HTTP layer:
    /// `jmap-client` assembles the `Authorization` header itself, so only the
    /// wire can show that the password goes out, and goes out intact.  Note that
    /// a debug build leaves a `jmap-log-*.log` behind for this, the same trace
    /// file a real session writes.
    #[test]
    fn a_password_is_sent_as_basic_authentication() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port to listen on");
        let port = listener
            .local_addr()
            .expect("the listener to know its address")
            .port();

        // One connection is all the session fetch makes; the 401 ends it there.
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("the client to connect");
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            (&stream)
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .ok();
            request
        });

        let backend = JmapBackend::new(JmapConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            auth: basic_auth(),
            trusted_hosts: Vec::new(),
            username: "rob@example.com".to_string(),
        })
        .expect("the backend to be constructed");
        let error = backend
            .load_mailbox(MailboxKind::Inbox)
            .expect_err("a server that answers 401 to have no mailboxes to hand over");
        let request = server.join().expect("the server thread to finish");

        let authorization = request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_string())
            .unwrap_or_else(|| panic!("the request should carry credentials:\n{request}"));
        assert_eq!(
            authorization, "Basic cm9iQGV4YW1wbGUuY29tOnMzY3JldA==",
            "base64 of rob@example.com:s3cret"
        );
        assert!(
            request.starts_with("GET /.well-known/jmap "),
            "the session object is what gets fetched: {request}"
        );

        let message = format!("{error:#}");
        assert!(
            message.contains("rejected the password for rob@example.com"),
            "{message}"
        );
    }

    /// The session object of a server at `base` whose mail lives in account
    /// `u1` -- and which, like FastMail, names other primary accounts besides,
    /// for capabilities that are not about mail.
    fn fake_session(base: &str) -> serde_json::Value {
        serde_json::json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": {
                    "maxSizeUpload": 50_000_000,
                    "maxConcurrentUpload": 4,
                    "maxSizeRequest": 10_000_000,
                    "maxConcurrentRequests": 4,
                    "maxCallsInRequest": 16,
                    "maxObjectsInGet": 500,
                    "maxObjectsInSet": 500,
                    "collationAlgorithms": []
                },
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {},
                "urn:ietf:params:jmap:contacts": {},
                "urn:ietf:params:jmap:calendars": {},
                "urn:ietf:params:jmap:principals": {}
            },
            "accounts": {
                "u1": {
                    "name": "rob@example.com",
                    "isPersonal": true,
                    "isReadOnly": false,
                    "accountCapabilities": {}
                },
                "c1": {
                    "name": "rob@example.com",
                    "isPersonal": true,
                    "isReadOnly": false,
                    "accountCapabilities": {}
                }
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:contacts": "c1",
                "urn:ietf:params:jmap:calendars": "c1",
                "urn:ietf:params:jmap:principals": "c1",
                "urn:ietf:params:jmap:mail": "u1",
                "urn:ietf:params:jmap:submission": "u1"
            },
            "username": "rob@example.com",
            "apiUrl": format!("{base}/api"),
            "downloadUrl": format!("{base}/download/{{accountId}}/{{blobId}}/{{name}}?type={{type}}"),
            "uploadUrl": format!("{base}/upload/{{accountId}}/"),
            "eventSourceUrl": format!("{base}/events?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
            "state": "session-1"
        })
    }

    /// Requests go to the mail account, not to whichever primary account the
    /// session happens to list first.
    #[test]
    fn the_mail_account_is_the_one_requests_go_to() {
        let session: Session = serde_json::from_value(fake_session("http://127.0.0.1"))
            .expect("the session object to parse");
        assert_eq!(mail_account(&session), Some("u1"));

        let without_mail: Session = serde_json::from_value({
            let mut session = fake_session("http://127.0.0.1");
            session["primaryAccounts"]
                .as_object_mut()
                .expect("primary accounts")
                .remove("urn:ietf:params:jmap:mail");
            session
        })
        .expect("the session object to parse");
        assert_eq!(mail_account(&without_mail), None);
    }

    fn identity(id: &str, email: &str) -> IdentityInfo {
        IdentityInfo {
            id: id.to_string(),
            email: email.to_string(),
            name: None,
        }
    }

    /// FastMail lists aliases and other domains next to the account's own
    /// address, and the own address need not come first.
    #[test]
    fn mail_goes_out_as_the_configured_address() {
        let identities = vec![
            identity("i1", "postmaster@lillack.net"),
            identity("i2", "Rob@Lillack.net"),
            identity("i3", "rob@example.com"),
        ];
        let chosen = choose_identity(identities, "rob@lillack.net", "rob@lillack.net")
            .expect("an identity to be chosen");
        assert_eq!(
            chosen.id, "i2",
            "the address the account is configured with, in any case"
        );
    }

    #[test]
    fn the_session_username_stands_in_for_a_username_that_is_no_address() {
        let identities = vec![
            identity("i1", "postmaster@lillack.net"),
            identity("i2", "rob@lillack.net"),
        ];
        let chosen = choose_identity(identities, "rob", "rob@lillack.net")
            .expect("an identity to be chosen");
        assert_eq!(chosen.id, "i2");
    }

    #[test]
    fn without_a_matching_identity_the_first_is_used() {
        let identities = vec![
            identity("i1", "postmaster@lillack.net"),
            identity("i2", "info@lillack.net"),
        ];
        let chosen = choose_identity(identities, "rob@lillack.net", "rob@lillack.net")
            .expect("an identity to be chosen");
        assert_eq!(chosen.id, "i1");

        assert!(choose_identity(Vec::new(), "rob@lillack.net", "rob@lillack.net").is_none());
    }

    /// One request as [`fake_jmap_server`] saw it.
    #[derive(Debug)]
    struct SeenRequest {
        /// `GET /.well-known/jmap`, `POST /api`, ...
        line: String,
        authorization: Option<String>,
        body: String,
    }

    /// A JMAP server on loopback that knows just enough to be connected to: the
    /// session object, an empty mailbox list and no identities.  It doubles as
    /// an OAuth token endpoint, handing out `token-1`, `token-2`, ... with a
    /// lifetime short enough that every one of them is due for renewal at once.
    ///
    /// Answers `requests` requests and hands back what they were.  The first
    /// `refused_sessions` fetches of the session object are answered `401`, the
    /// way a server answers a token it has withdrawn.
    ///
    /// A request that does not come within ten seconds ends the serving early,
    /// so a client that never makes it fails its test rather than hanging it.
    fn fake_jmap_server(
        requests: usize,
        refused_sessions: usize,
    ) -> (u16, std::thread::JoinHandle<Vec<SeenRequest>>) {
        use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
        use std::time::Instant;

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port to listen on");
        listener
            .set_nonblocking(true)
            .expect("a listener that can give up");
        let port = listener.local_addr().expect("an address").port();
        let base = format!("http://127.0.0.1:{port}");

        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut tokens_issued = 0;
            let mut sessions_refused = 0;
            for _ in 0..requests {
                let deadline = Instant::now() + Duration::from_secs(10);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break Some(stream),
                        Err(err)
                            if err.kind() == ErrorKind::WouldBlock && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break None,
                    }
                };
                let Some(stream) = stream else {
                    break;
                };
                // Windows hands the listener's non-blocking mode on to what it
                // accepts.
                stream
                    .set_nonblocking(false)
                    .expect("a blocking connection");
                let mut reader = BufReader::new(&stream);

                let mut line = String::new();
                reader.read_line(&mut line).expect("a request line");
                let mut authorization = None;
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap_or((&header, ""));
                    match name.to_ascii_lowercase().as_str() {
                        "authorization" => authorization = Some(value.trim().to_string()),
                        "content-length" => length = value.trim().parse().unwrap_or(0),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("the request body");
                let body = String::from_utf8_lossy(&body).to_string();

                let refused = line.starts_with("GET /.well-known/jmap ")
                    && sessions_refused < refused_sessions;
                let payload = if refused {
                    sessions_refused += 1;
                    serde_json::Value::Null
                } else if line.starts_with("GET /.well-known/jmap ") {
                    fake_session(&base)
                } else if line.starts_with("POST /token ") {
                    tokens_issued += 1;
                    serde_json::json!({
                        "access_token": format!("token-{tokens_issued}"),
                        "refresh_token": format!("refresh-{tokens_issued}"),
                        "expires_in": 1
                    })
                } else {
                    let method = ["Mailbox/get", "Identity/get"]
                        .into_iter()
                        .find(|method| body.contains(method))
                        .unwrap_or_else(|| panic!("an unexpected request: {line} {body}"));
                    // Only the mail account holds mail, as on FastMail.
                    let answer = if body.contains("\"accountId\":\"u1\"") {
                        serde_json::json!([method, {
                            "accountId": "u1",
                            "state": "1",
                            "list": [],
                            "notFound": []
                        }, "s0"])
                    } else {
                        serde_json::json!(["error", { "type": "accountNotSupportedByMethod" }, "s0"])
                    };
                    serde_json::json!({
                        "sessionState": "session-1",
                        "methodResponses": [answer]
                    })
                }
                .to_string();

                let response = if refused {
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                };
                (&stream)
                    .write_all(response.as_bytes())
                    .expect("the response to go out");
                seen.push(SeenRequest {
                    line: line.trim_end().to_string(),
                    authorization,
                    body,
                });
            }
            seen
        });

        (port, server)
    }

    /// A credential the server turned down is replaced by connecting again --
    /// and only the connection is replaced.  The message ids the UI holds and
    /// the event channel belong to the account, and outlive the header.
    #[test]
    fn a_refused_credential_reconnects_without_losing_the_session() {
        // Session, mailboxes and identity to connect; the session again after.
        let (port, server) = fake_jmap_server(4, 0);
        let backend = JmapBackend::new(JmapConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            auth: bearer_auth(),
            trusted_hosts: Vec::new(),
            username: "rob@example.com".to_string(),
        })
        .expect("the backend to be constructed");

        let first = backend.inner().expect("the first connection to succeed");
        let first_client = first.client();
        first.runtime.block_on(async {
            first.state.lock().await.next_message_id = 42;
        });
        let (sender, _receiver) = mpsc::channel();
        *first.events.lock().unwrap() = Some(sender);

        // What the event loop does on a 401.
        first.rejected.store(true, AtomicOrdering::SeqCst);
        let second = backend.inner().expect("connecting again to succeed");
        let seen = server.join().expect("the server thread to finish");

        assert!(
            Arc::ptr_eq(&first, &second),
            "the session itself carries on"
        );
        assert!(
            !Arc::ptr_eq(&first_client, &second.client()),
            "the refused client is replaced"
        );
        assert!(!second.rejected.load(AtomicOrdering::SeqCst));
        assert_eq!(
            second
                .runtime
                .block_on(async { second.state.lock().await.next_message_id }),
            42,
            "the ids handed to the UI still mean what they meant"
        );
        assert!(
            second.events.lock().unwrap().is_some(),
            "the UI still hears about new mail"
        );

        let lines: Vec<_> = seen.iter().map(|request| request.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "GET /.well-known/jmap HTTP/1.1",
                "POST /api HTTP/1.1",
                "POST /api HTTP/1.1",
                "GET /.well-known/jmap HTTP/1.1",
            ],
            "connecting again fetches the session object and nothing more"
        );
    }

    /// An OAuth account signed in to [`fake_jmap_server`] on `port`, holding
    /// `token-0` until `expires_at` and `refresh-0` to renew it with.
    fn oauth_backend(port: u16, directory: &std::path::Path, expires_at: i64) -> JmapBackend {
        use crate::backend::oauth::{StoredCredential, TokenStore};

        let base_url = format!("http://127.0.0.1:{port}");
        let store = TokenStore::new(directory.join("oauth.toml"));
        store
            .save(&StoredCredential {
                endpoint: base_url.clone(),
                username: "rob@example.com".to_string(),
                issuer: base_url.clone(),
                client_id: "a-client".to_string(),
                token_endpoint: format!("{base_url}/token"),
                resource: Some(format!("{base_url}/jmap/session")),
                access_token: "token-0".to_string(),
                refresh_token: Some("refresh-0".to_string()),
                expires_at: Some(expires_at),
                scopes: Vec::new(),
            })
            .expect("the sign-in to be stored");

        JmapBackend::new(JmapConfig {
            base_url: base_url.clone(),
            auth: JmapAuth::OAuth(Arc::new(OAuthCredential::new(
                store,
                &base_url,
                "rob@example.com",
                "Work",
            ))),
            trusted_hosts: Vec::new(),
            username: "rob@example.com".to_string(),
        })
        .expect("the backend to be constructed")
    }

    /// An OAuth access token that runs out is renewed on the next call, and the
    /// renewed one goes out on a fresh connection under the same session.
    #[test]
    fn a_renewed_access_token_reconnects_without_losing_the_session() {
        // Renewal, session, mailboxes and identity to connect; renewal and
        // session again on the next call.
        let (port, server) = fake_jmap_server(6, 0);
        let directory = tempfile::tempdir().expect("a temporary directory");
        let backend = oauth_backend(port, directory.path(), 0);

        let first = backend.inner().expect("the first connection to succeed");
        first.runtime.block_on(async {
            first.state.lock().await.next_message_id = 42;
        });
        let second = backend.inner().expect("the renewal to succeed");
        let seen = server.join().expect("the server thread to finish");

        assert!(
            Arc::ptr_eq(&first, &second),
            "the session itself carries on"
        );
        assert_eq!(
            second
                .runtime
                .block_on(async { second.state.lock().await.next_message_id }),
            42,
            "the ids handed to the UI still mean what they meant"
        );

        let lines: Vec<_> = seen.iter().map(|request| request.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "POST /token HTTP/1.1",
                "GET /.well-known/jmap HTTP/1.1",
                "POST /api HTTP/1.1",
                "POST /api HTTP/1.1",
                "POST /token HTTP/1.1",
                "GET /.well-known/jmap HTTP/1.1",
            ]
        );
        assert_eq!(seen[1].authorization.as_deref(), Some("Bearer token-1"));
        assert_eq!(
            seen[5].authorization.as_deref(),
            Some("Bearer token-2"),
            "the renewed token is the one that goes out"
        );
        assert!(
            seen[4].body.contains("refresh_token=refresh-1"),
            "the rotated refresh token is the one spent next: {}",
            seen[4].body
        );
        assert!(
            seen[4].body.contains("resource="),
            "the renewal names the endpoint it is for: {}",
            seen[4].body
        );
    }

    /// A token the server refuses mid-session is renewed on the next call, even
    /// though its expiry has not come yet -- connecting again with the same one
    /// would only meet the same refusal.
    #[test]
    fn a_token_refused_before_its_expiry_is_renewed() {
        // Session, mailboxes and identity to connect; renewal and session after
        // the refusal.
        let (port, server) = fake_jmap_server(5, 0);
        let directory = tempfile::tempdir().expect("a temporary directory");
        let backend = oauth_backend(port, directory.path(), i64::MAX);

        let inner = backend.inner().expect("the first connection to succeed");
        // What a request that met a 401 leaves behind.
        let refusal = inner.noting_refusal::<()>(Err(anyhow::Error::new(JmapError::from(
            jmap_client::Error::Server("401 Unauthorized".to_string()),
        ))));
        assert!(refusal.is_err());
        backend.inner().expect("the renewal to succeed");
        let seen = server.join().expect("the server thread to finish");

        let lines: Vec<_> = seen.iter().map(|request| request.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "GET /.well-known/jmap HTTP/1.1",
                "POST /api HTTP/1.1",
                "POST /api HTTP/1.1",
                "POST /token HTTP/1.1",
                "GET /.well-known/jmap HTTP/1.1",
            ],
            "the refused token is renewed rather than presented again"
        );
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer token-0"));
        assert_eq!(seen[4].authorization.as_deref(), Some("Bearer token-1"));
    }

    /// The same holds for the very first connection: a stored token the server
    /// has withdrawn is renewed on the next attempt, not sent again.
    #[test]
    fn a_token_refused_on_the_first_connection_is_renewed() {
        // A refused session fetch; then renewal, session, mailboxes, identity.
        let (port, server) = fake_jmap_server(5, 1);
        let directory = tempfile::tempdir().expect("a temporary directory");
        let backend = oauth_backend(port, directory.path(), i64::MAX);

        let error = backend
            .inner()
            .err()
            .expect("the withdrawn token to be refused");
        assert!(
            error.to_string().contains("rejected the access token"),
            "{error}"
        );
        backend.inner().expect("the renewed token to be accepted");
        let seen = server.join().expect("the server thread to finish");

        let lines: Vec<_> = seen.iter().map(|request| request.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "GET /.well-known/jmap HTTP/1.1",
                "POST /token HTTP/1.1",
                "GET /.well-known/jmap HTTP/1.1",
                "POST /api HTTP/1.1",
                "POST /api HTTP/1.1",
            ]
        );
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer token-0"));
        assert_eq!(seen[2].authorization.as_deref(), Some("Bearer token-1"));
    }

    /// What reaches the event loop is a `JmapError` under whatever context the
    /// call site added; the refusal has to be recognised through that.
    #[test]
    fn a_refusal_is_recognised_under_added_context() {
        let refused = anyhow::Error::new(JmapError::from(jmap_client::Error::Server(
            "401 Unauthorized".to_string(),
        )))
        .context("refreshing the mailbox");
        assert!(credential_was_refused(&refused));

        let unavailable = anyhow::Error::new(JmapError::from(jmap_client::Error::Server(
            "503 Service Unavailable".to_string(),
        )))
        .context("refreshing the mailbox");
        assert!(!credential_was_refused(&unavailable));

        // A refused session fetch, already folded into one line for the user.
        let at_connect = connect_error(
            JmapError::from(jmap_client::Error::Server("401 Unauthorized".to_string())),
            &bearer_auth(),
            "https://api.example.com/jmap/session",
        );
        assert!(credential_was_refused(&at_connect));
        assert_eq!(
            format!("{at_connect:#}"),
            at_connect.to_string(),
            "nothing behind it for the mailbox loader to repeat"
        );
    }

    #[test]
    fn status_is_read_off_a_bare_status_line() {
        let err = JmapError::from(jmap_client::Error::Server("401 Unauthorized".to_string()));
        assert_eq!(http_status(&err), Some(401));
    }

    /// The reason a transport failure carries is the half worth having, and it
    /// sits one link below the line `jmap-client` prints for itself.
    #[test]
    fn a_transport_failure_keeps_the_reason_behind_it() {
        // A port nothing is listening on: bound only to be told a free one,
        // then dropped, so the connection is refused rather than left hanging.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            listener.local_addr().expect("local address").port()
        };

        let runtime = Runtime::new().expect("runtime");
        let err = JmapError::from(runtime.block_on(async move {
            Client::new()
                .credentials(Credentials::bearer("token"))
                .connect(&format!("http://127.0.0.1:{port}"))
                .await
                .err()
                .expect("nothing is listening on that port")
        }));

        // On its own the error names the URL and nothing one could act on ...
        assert_eq!(
            err.to_string(),
            format!(
                "Transport error: error sending request for url (http://127.0.0.1:{port}/.well-known/jmap)"
            )
        );
        // ... while the chain behind it says what actually happened.
        let detail = err.one_line().to_lowercase();
        assert!(detail.contains("refused"), "{detail}");
    }

    #[test]
    fn a_server_message_without_a_status_reads_as_none() {
        let err = JmapError::from(jmap_client::Error::Server("connection reset".to_string()));
        assert_eq!(http_status(&err), None);
    }

    #[test]
    fn a_rejected_password_names_the_account_and_offers_the_token() {
        let message = connect_error(
            JmapError::from(jmap_client::Error::Server("401 Unauthorized".to_string())),
            &basic_auth(),
            "https://mail.example.com/.well-known/jmap",
        )
        .to_string();

        assert!(
            message.contains("rejected the password for rob@example.com"),
            "{message}"
        );
        assert!(message.contains("HTTP 401"), "{message}");
        assert!(message.contains("token = "), "{message}");
        assert!(!message.contains("s3cret"), "{message}");
    }

    #[test]
    fn a_rejected_token_offers_the_password() {
        let message = connect_error(
            JmapError::from(jmap_client::Error::Server("403 Forbidden".to_string())),
            &bearer_auth(),
            "https://api.fastmail.com/jmap/session",
        )
        .to_string();

        assert!(message.contains("rejected the API token"), "{message}");
        assert!(message.contains("password = "), "{message}");
        assert!(!message.contains("an-api-token"), "{message}");
    }

    /// A failure that is not about the credentials keeps the server's own words.
    #[test]
    fn other_failures_are_reported_as_they_come() {
        let message = connect_error(
            JmapError::from(jmap_client::Error::Server(
                "503 Service Unavailable".to_string(),
            )),
            &basic_auth(),
            "https://mail.example.com",
        )
        .to_string();

        assert!(message.contains("connecting to JMAP server"), "{message}");
        assert!(message.contains("503"), "{message}");
    }

    /// Credentials a URL carries are stripped before they reach an error the UI
    /// will print.
    #[test]
    fn a_url_with_userinfo_is_redacted_in_errors() {
        let message = connect_error(
            JmapError::from(jmap_client::Error::Server("401 Unauthorized".to_string())),
            &basic_auth(),
            "https://rob:s3cret@mail.example.com/.well-known/jmap",
        )
        .to_string();

        assert!(!message.contains("s3cret"), "{message}");
    }

    /// Mailbox cache with the given `(id, name, kind)` entries; user mailboxes
    /// pass `None` as their kind.
    fn cache(entries: &[(&str, &str, Option<MailboxKind>)]) -> MailboxCache {
        let mut by_id = HashMap::new();
        let mut by_kind = HashMap::new();
        for (id, name, kind) in entries {
            by_id.insert(
                (*id).to_string(),
                MailboxInfo {
                    name: (*name).to_string(),
                    total_emails: 0,
                },
            );
            if let Some(kind) = kind {
                by_kind.insert(*kind, (*id).to_string());
            }
        }

        MailboxCache {
            by_id,
            by_kind,
            totals_override: HashMap::new(),
        }
    }

    fn email(mailbox_ids: &[&str], keywords: &[&str]) -> FetchedEmail {
        FetchedEmail {
            jmap_id: "M1".to_string(),
            from: vec!["someone@example.com".to_string()],
            to: vec!["me@example.com".to_string()],
            subject: "Hello".to_string(),
            received_at: Some(0),
            sent_at: Some(0),
            size: 42,
            mailbox_ids: mailbox_ids.iter().map(|id| (*id).to_string()).collect(),
            keywords: keywords.iter().map(|kw| (*kw).to_string()).collect(),
            has_attachments: false,
        }
    }

    fn labels_in(mailbox: MailboxKind, data: &FetchedEmail, cache: &MailboxCache) -> Vec<String> {
        build_message(1, 1, 1, data, cache, mailbox)
            .expect("message builds")
            .labels
    }

    fn standard_cache() -> MailboxCache {
        cache(&[
            ("mb-inbox", "Inbox", Some(MailboxKind::Inbox)),
            ("mb-sent", "Sent", Some(MailboxKind::Sent)),
            ("mb-archive", "Archive", Some(MailboxKind::Archive)),
            ("mb-news", "Newsletters", None),
        ])
    }

    #[test]
    fn open_mailbox_is_not_repeated_as_a_label() {
        let cache = standard_cache();

        assert!(labels_in(MailboxKind::Inbox, &email(&["mb-inbox"], &[]), &cache).is_empty());
        assert!(labels_in(MailboxKind::Sent, &email(&["mb-sent"], &[]), &cache).is_empty());
    }

    #[test]
    fn other_mailboxes_stay_visible_as_labels() {
        let cache = standard_cache();
        let data = email(&["mb-inbox", "mb-news"], &[]);

        assert_eq!(
            labels_in(MailboxKind::Inbox, &data, &cache),
            vec!["Newsletters".to_string()]
        );
        // Seen from the newsletter side the inbox is the interesting part.
        assert_eq!(
            labels_in(MailboxKind::Archive, &data, &cache),
            vec!["Inbox".to_string(), "Newsletters".to_string()]
        );
    }

    /// A canned `Email/get` payload, the way a poll receives one.  Parsed
    /// afresh on every call, so each returns an independently-seeded
    /// `AHashMap` for `mailboxIds` -- which is the whole point.
    fn parse_email(mailbox_ids: &[&str]) -> FetchedEmail {
        let ids: serde_json::Map<String, serde_json::Value> = mailbox_ids
            .iter()
            .map(|id| ((*id).to_string(), serde_json::json!(true)))
            .collect();
        let email: JmapEmail = serde_json::from_value(serde_json::json!({
            "id": "M1",
            "mailboxIds": ids,
            "keywords": {"$seen": true},
            "subject": "Hello",
            "size": 42,
            "receivedAt": "2026-08-20T06:00:00Z",
            "hasAttachment": false,
        }))
        .expect("parsing the canned email");

        FetchedEmail::from_email(email).expect("an email with an id builds")
    }

    /// `jmap-client` holds `mailboxIds` in an `AHashMap` and hands them over in
    /// its iteration order, which is seeded per map -- so the same email arrives
    /// with its mailboxes shuffled from one poll to the next.  Sorting them on
    /// the way in is what stops that reaching the labels.
    #[test]
    fn mailbox_ids_arrive_in_a_stable_order() {
        let wire = ["mb-news", "mb-inbox", "mb-work", "mb-archive"];
        let sorted = vec![
            "mb-archive".to_string(),
            "mb-inbox".to_string(),
            "mb-news".to_string(),
            "mb-work".to_string(),
        ];

        // Four ids permute 24 ways, so an unsorted read is overwhelmingly
        // unlikely to survive this many independently-seeded maps.
        for poll in 0..64 {
            assert_eq!(parse_email(&wire).mailbox_ids, sorted, "poll {poll}");
        }
    }

    /// The leak this was found through: a message filed in more than one other
    /// mailbox reported a flag change on every background poll, because
    /// `flags_changed` compares labels as a sequence and the sequence came out
    /// of a differently-seeded hash map each time.  Nothing about the message
    /// changed, so nothing may be reported.
    #[test]
    fn an_unchanged_email_reports_no_flag_change_between_polls() {
        let cache = standard_cache();
        let wire = ["mb-inbox", "mb-news", "mb-archive", "mb-sent"];

        let first = build_message(1, 1, 1, &parse_email(&wire), &cache, MailboxKind::Inbox)
            .expect("message builds");

        for poll in 0..64 {
            let again = build_message(1, 1, 1, &parse_email(&wire), &cache, MailboxKind::Inbox)
                .expect("message builds");
            assert!(
                !flags_changed(&first, &again),
                "poll {poll} reported a change: {:?} -> {:?}",
                first.labels,
                again.labels
            );
        }
    }

    /// A reopened draft is addressed from these, so a name with a comma in it
    /// has to come out quoted: unquoted, it would split into two recipients.
    #[test]
    fn a_loaded_message_carries_its_recipients_as_addresses() {
        let email: JmapEmail = serde_json::from_value(serde_json::json!({
            "id": "M1",
            "to": [
                {"name": "Doe, John", "email": "john@example.com"},
                {"name": null, "email": "ann@example.com"},
            ],
            "cc": [{"name": "Bob", "email": "bob@example.com"}],
            "bcc": [{"name": "", "email": "boss@example.com"}],
        }))
        .expect("parsing the canned email");

        let content = build_message_content(&email).expect("content builds");
        assert_eq!(
            content.to,
            ["\"Doe, John\" <john@example.com>", "ann@example.com"]
        );
        assert_eq!(content.cc, ["Bob <bob@example.com>"]);
        assert_eq!(content.bcc, ["boss@example.com"]);
    }

    #[test]
    fn localized_mailbox_names_are_matched_by_id() {
        let cache = cache(&[("mb-sent", "Gesendet", Some(MailboxKind::Sent))]);

        assert!(labels_in(MailboxKind::Sent, &email(&["mb-sent"], &[]), &cache).is_empty());
    }

    #[test]
    fn starred_and_important_are_dropped_in_their_own_mailbox() {
        let cache = standard_cache();
        let data = email(&["mb-inbox"], &["$flagged", "$important"]);

        assert_eq!(
            labels_in(MailboxKind::Starred, &data, &cache),
            vec!["Inbox".to_string(), "Important".to_string()]
        );
        assert_eq!(
            labels_in(MailboxKind::Important, &data, &cache),
            vec!["Inbox".to_string(), "Starred".to_string()]
        );
    }
    /// The shape FastMail rejected: jmap-client can only put `false` in a
    /// patch, and RFC 8621 says a keyword is removed with `null` and never
    /// carries any value but `true`.  Locked down here so the send path is never
    /// "simplified" back onto `email_set_keyword(.., false)`.
    #[test]
    fn patching_a_keyword_off_produces_the_form_the_server_refuses() {
        let mut email = <JmapEmail<JmapSet> as SetObject>::new(None);
        email.keyword("$draft", false);

        let json = serde_json::to_value(&email).expect("serialising the update");

        assert_eq!(json["keywords/$draft"], serde_json::json!(false));
    }

    /// What the sent copy is described as instead: whole properties, every
    /// value `true`, and not a patch pointer in sight.
    #[test]
    fn the_sent_copy_is_described_without_patching_anything() {
        let mut email = <JmapEmail<JmapSet> as SetObject>::new(None);
        describe_sent_copy(&mut email, Some("P6F"));

        let json = serde_json::to_value(&email).expect("serialising the update");

        assert_eq!(json["mailboxIds"], serde_json::json!({"P6F": true}));
        assert_eq!(json["keywords"], serde_json::json!({"$seen": true}));
        let patched: Vec<&String> = json
            .as_object()
            .expect("the update is an object")
            .keys()
            .filter(|key| key.contains('/'))
            .collect();
        assert!(
            patched.is_empty(),
            "left a patch pointer behind: {patched:?}"
        );
    }

    /// Without a Sent mailbox there is nowhere to file the copy, but it still
    /// has to stop being a draft.
    #[test]
    fn the_sent_copy_still_drops_the_draft_keyword_without_a_sent_mailbox() {
        let mut email = <JmapEmail<JmapSet> as SetObject>::new(None);
        describe_sent_copy(&mut email, None);

        let json = serde_json::to_value(&email).expect("serialising the update");

        assert!(json.get("mailboxIds").is_none());
        assert_eq!(json["keywords"], serde_json::json!({"$seen": true}));
    }

    fn responses(body: serde_json::Value) -> Vec<TaggedMethodResponse> {
        serde_json::from_value::<jmap_client::core::response::Response<TaggedMethodResponse>>(body)
            .expect("parsing the server response")
            .unwrap_method_responses()
    }

    /// `onSuccessUpdateEmail` earns a second method response, and the
    /// submission is the *first* of the two.  Reading the last one -- which is
    /// what jmap-client's `send_single` does -- found no creation and reported
    /// a delivered message as a failed send.
    #[test]
    fn the_submission_is_found_behind_the_implicit_update() {
        let parsed = responses(serde_json::json!({
            "sessionState": "abc",
            "methodResponses": [
                ["EmailSubmission/set", {
                    "accountId": "u1",
                    "created": {"c0": {"id": "S1"}}
                }, "s0"],
                ["Email/set", {
                    "accountId": "u1",
                    "updated": {"StnBpNCKXfQ7": null}
                }, "s0"]
            ]
        }));
        assert_eq!(parsed.len(), 2);

        let (submission, update) =
            split_submission_responses(parsed).expect("both responses are readable");

        submission
            .expect("the submission response is there")
            .created("c0")
            .expect("the submission was created");
        update.expect("the implicit update response is there");
    }

    /// A server that answers the submission alone is still understood; there is
    /// simply no update to report on.
    #[test]
    fn a_lone_submission_response_is_read_on_its_own() {
        let parsed = responses(serde_json::json!({
            "sessionState": "abc",
            "methodResponses": [
                ["EmailSubmission/set", {
                    "accountId": "u1",
                    "created": {"c0": {"id": "S1"}}
                }, "s0"]
            ]
        }));

        let (submission, update) =
            split_submission_responses(parsed).expect("the response is readable");

        assert!(submission.is_some());
        assert!(update.is_none());
    }

    /// A refused submission has to stay refused: this is the one case where
    /// failing the send is the honest answer.
    #[test]
    fn a_refused_submission_is_reported_as_an_error() {
        let parsed = responses(serde_json::json!({
            "sessionState": "abc",
            "methodResponses": [
                ["EmailSubmission/set", {
                    "accountId": "u1",
                    "notCreated": {"c0": {"type": "forbiddenFrom"}}
                }, "s0"]
            ]
        }));

        let (submission, _) = split_submission_responses(parsed).expect("the response is readable");

        assert!(
            submission
                .expect("the submission response is there")
                .created("c0")
                .is_err()
        );
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_string()).collect()
    }

    /// Where a message filed under `current` ends up when it is moved out of
    /// `from` and into `target`, in an account with every role.
    fn moved(current: &[&str], from: &[MailboxKind], target: &str) -> Vec<String> {
        let cache = cache(&[
            ("mb-inbox", "Inbox", Some(MailboxKind::Inbox)),
            ("mb-archive", "Archive", Some(MailboxKind::Archive)),
            ("mb-trash", "Trash", Some(MailboxKind::Trash)),
            ("mb-spam", "Spam", Some(MailboxKind::Spam)),
            ("mb-sent", "Sent", Some(MailboxKind::Sent)),
            ("mb-news", "Newsletters", None),
        ]);
        moved_membership(&ids(current), &cache.ids_for_kinds(from), target)
    }

    /// Archiving used to replace every mailbox a message was in with Archive,
    /// so a label or project folder went with the inbox.
    #[test]
    fn archiving_keeps_the_other_mailboxes_a_message_is_in() {
        assert_eq!(
            moved(&["mb-inbox", "mb-news"], &ARCHIVE_FROM, "mb-archive"),
            ids(&["mb-news", "mb-archive"])
        );
    }

    /// RFC 8621 shows a message that is in the trash and somewhere else in
    /// both places, so archiving one from Trash or Spam has to take it out.
    #[test]
    fn archiving_rescues_a_message_from_trash_or_spam() {
        assert_eq!(
            moved(&["mb-trash"], &ARCHIVE_FROM, "mb-archive"),
            ids(&["mb-archive"])
        );
        assert_eq!(
            moved(&["mb-spam", "mb-news"], &ARCHIVE_FROM, "mb-archive"),
            ids(&["mb-news", "mb-archive"])
        );
    }

    #[test]
    fn moving_back_to_the_inbox_leaves_trash_spam_and_archive() {
        assert_eq!(
            moved(&["mb-trash", "mb-news"], &RESTORE_FROM, "mb-inbox"),
            ids(&["mb-news", "mb-inbox"])
        );
        assert_eq!(
            moved(&["mb-archive", "mb-spam"], &RESTORE_FROM, "mb-inbox"),
            ids(&["mb-inbox"])
        );
    }

    /// The read/unread toggle goes through the same move, so a message filed
    /// somewhere the move does not come from -- Sent, here -- has to stay
    /// there rather than be carried off to the inbox.
    #[test]
    fn a_move_from_elsewhere_only_adds_the_target() {
        assert_eq!(
            moved(&["mb-sent"], &RESTORE_FROM, "mb-inbox"),
            ids(&["mb-sent", "mb-inbox"])
        );
    }

    #[test]
    fn a_message_already_in_the_target_is_not_filed_there_twice() {
        assert_eq!(
            moved(&["mb-inbox", "mb-news"], &RESTORE_FROM, "mb-inbox"),
            ids(&["mb-inbox", "mb-news"])
        );
    }

    /// A move only retries when the server says its read went stale; any
    /// other refusal is final.
    #[test]
    fn only_a_stale_read_counts_as_a_state_mismatch() {
        let method_error = |kind: &str| {
            let error: jmap_client::core::error::MethodError =
                serde_json::from_value(serde_json::json!({ "type": kind }))
                    .expect("parsing the method error");
            JmapError::from(jmap_client::Error::Method(error))
        };

        assert!(is_state_mismatch(&method_error("stateMismatch")));
        assert!(!is_state_mismatch(&method_error("invalidArguments")));
        assert!(!is_state_mismatch(&JmapError::from(
            jmap_client::Error::Server("503 Service Unavailable".to_string())
        )));
    }

    /// Message `id` as the mailbox query returns it, arriving at `received_at`.
    fn fetched_at(id: MessageId, received_at: i64) -> FetchedEmail {
        let mut data = email(&["mb-inbox"], &[]);
        data.jmap_id = format!("M{id}");
        data.received_at = Some(received_at);
        data
    }

    /// Message `id`, arriving an hour before message `id - 1`: the lower the
    /// id, the nearer the top of the mailbox.
    fn fetched(id: MessageId) -> FetchedEmail {
        fetched_at(id, 1_000_000 - id as i64 * 3600)
    }

    fn stored(id: MessageId, data: FetchedEmail) -> StoredMessage {
        let message = build_message(
            id,
            id as u32,
            1,
            &data,
            &standard_cache(),
            MailboxKind::Inbox,
        )
        .expect("message builds");
        StoredMessage {
            message,
            jmap_id: data.jmap_id,
            received_at: data.received_at,
        }
    }

    /// The Inbox listing `ids`, with the backfill having read `read` places.
    fn listing(ids: &[MessageId], read: usize) -> JmapState {
        let mut state = JmapState {
            current_mailbox: Some(MailboxKind::Inbox),
            highest_received_index: read,
            ..JmapState::default()
        };
        for &id in ids {
            state.messages.insert(id, stored(id, fetched(id)));
            state.current_sequence.push(id);
        }
        state
    }

    /// The top of the mailbox as a refresh fetches it.
    fn first_page(
        emails: Vec<(MessageId, FetchedEmail)>,
        has_more: bool,
    ) -> (Vec<(MessageId, StoredMessage)>, PageCoverage) {
        let (ids, emails): (Vec<MessageId>, Vec<FetchedEmail>) = emails.into_iter().unzip();
        let coverage = PageCoverage::of(&emails, 0, has_more);
        let page = ids
            .into_iter()
            .zip(emails)
            .map(|(id, data)| (id, stored(id, data)))
            .collect();
        (page, coverage)
    }

    fn page_of(
        ids: &[MessageId],
        has_more: bool,
    ) -> (Vec<(MessageId, StoredMessage)>, PageCoverage) {
        first_page(ids.iter().map(|&id| (id, fetched(id))).collect(), has_more)
    }

    /// Before this, a refresh kept every message it had ever listed, so a
    /// message moved or deleted elsewhere stayed on screen for good.
    #[test]
    fn a_message_missing_from_the_top_of_the_mailbox_has_left() {
        let mut state = listing(&[1, 2, 3, 4, 5, 6], 6);
        let (page, coverage) = page_of(&[1, 3, 4], true);

        let removed = state.apply_page(page, &coverage, 3);

        assert_eq!(removed, vec![2]);
        assert_eq!(state.current_sequence, vec![1, 3, 4, 5, 6]);
        // Gone from the listing, but an action queued against it before the
        // refresh still has an email to aim at.
        assert_eq!(
            state.messages.get(&2).map(|stored| stored.jmap_id.as_str()),
            Some("M2")
        );
    }

    /// A message that is simply further down than the page reaches has not
    /// gone anywhere -- including one a new arrival has just pushed off it.
    #[test]
    fn a_message_past_the_end_of_the_page_stays() {
        let mut state = listing(&[1, 2, 3, 4, 5, 6], 6);
        let newest = fetched_at(7, 2_000_000);
        let (page, coverage) =
            first_page(vec![(7, newest), (1, fetched(1)), (2, fetched(2))], true);

        let removed = state.apply_page(page, &coverage, 3);

        assert!(removed.is_empty(), "{removed:?}");
        assert_eq!(state.current_sequence, vec![7, 1, 2, 3, 4, 5, 6]);
        assert_eq!(state.highest_received_index, 6);
    }

    /// A page that runs to the end of the mailbox leaves nowhere else for a
    /// missing message to be, however long ago it arrived.
    #[test]
    fn a_page_that_reaches_the_end_vouches_for_the_whole_mailbox() {
        let mut state = listing(&[1, 2, 3], 3);
        let (page, coverage) = page_of(&[1, 2], false);

        let removed = state.apply_page(page, &coverage, 2);

        assert_eq!(removed, vec![3]);
        assert_eq!(state.current_sequence, vec![1, 2]);
    }

    /// Messages that arrived in the same second sort in whatever order the
    /// server picks, so one missing from the page may sit just past its end.
    #[test]
    fn a_message_tied_with_the_oldest_on_the_page_stays() {
        let mut state = listing(&[1, 2, 3], 3);
        let tied = fetched(2).received_at;
        if let Some(stored) = state.messages.get_mut(&3) {
            stored.received_at = tied;
        }
        let (page, coverage) = page_of(&[1, 2], true);

        let removed = state.apply_page(page, &coverage, 2);

        assert!(removed.is_empty(), "{removed:?}");
        assert_eq!(state.current_sequence, vec![1, 2, 3]);
    }

    /// The backfill reads the mailbox by position.  Every message that leaves
    /// from above its cursor moves the rest up a place, and a cursor that
    /// stayed put would skip one message for each -- leaving it on screen as a
    /// row that never finishes loading.
    #[test]
    fn the_backfill_resumes_where_the_unread_messages_now_start() {
        // Ten messages; the first page and one backfill batch have read six.
        let mut state = listing(&[1, 2, 3, 4, 5, 6], 6);
        // Message 2 is archived, so 7 is now the sixth message in the mailbox.
        let mailbox: Vec<MessageId> = (1..=10).filter(|id| *id != 2).collect();
        let (page, coverage) = page_of(&[1, 3, 4], true);

        state.apply_page(page, &coverage, 3);

        assert_eq!(mailbox[state.next_cursor_index()], 7);
    }

    /// Only a page from the top of the mailbox says anything about what is
    /// missing from it.
    #[test]
    fn a_page_from_further_down_vouches_for_nothing() {
        let coverage = PageCoverage::of(&[fetched(5), fetched(6)], 4, false);

        assert!(!coverage.covers(fetched(1).received_at));
        assert!(!coverage.covers(fetched(9).received_at));
    }
}
