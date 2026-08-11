//! JMAP backend built on the `jmap-client` crate.
//!
//! This implementation mirrors the asynchronous behaviour of the Gmail backend
//! while talking to FastMail (or any JMAP-compliant provider).  Mailboxes and
//! messages are synchronised over the JMAP HTTP API and background updates are
//! processed on an internal Tokio runtime so the UI thread remains responsive.

use crate::{
    backend::{
        ActionStatus, BackendEvent, LeafPart, MailBackend, MailboxSnapshot, OutgoingMessage,
        PartRole, build_compose_body, oauth::OAuthCredential,
    },
    model::{
        Action, ActionType, MailboxKind, Message, MessageAttachment, MessageContent,
        MessageContentPart, MessageId, MessageStatus,
    },
};
use anyhow::{Context, Result, anyhow};
use jmap_client::{
    URI,
    client::{Client, Credentials},
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
use tokio::{runtime::Runtime, sync::Mutex as AsyncMutex, task::JoinHandle, time::interval};

const INITIAL_FETCH_LIMIT: usize = 128;
const BACKFILL_BATCH_SIZE: usize = 128;
const MAX_BODY_VALUE_BYTES: usize = 512 * 1024;
const EVENT_IDLE_POLL: Duration = Duration::from_secs(45);
const EVENT_RETRY_DELAY: Duration = Duration::from_secs(10);

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
        /// untouched so call sites keep their usual error handling.
        pub fn response<T, E: Display>(
            &self,
            result: Result<T, E>,
            summary: impl FnOnce(&T) -> String,
        ) -> Result<T, E> {
            if let Some(logger) = &self.logger {
                let payload = match &result {
                    Ok(value) => format!("{} -> {}", self.method, summary(value)),
                    Err(err) => format!("{} -> ERROR: {err}", self.method),
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
    use std::fmt::Display;

    pub struct JmapRequest;

    pub fn register_secret(_secret: &str) {}

    pub fn log_session(_payload: impl Display) {}

    pub fn request(_method: &str, _arguments: impl Display) -> JmapRequest {
        JmapRequest
    }

    impl JmapRequest {
        pub fn response<T, E: Display>(
            &self,
            result: Result<T, E>,
            _summary: impl FnOnce(&T) -> String,
        ) -> Result<T, E> {
            result
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
    /// server goes through here rather than around it.
    async fn secret_now(&self) -> Result<String> {
        Ok(match self {
            JmapAuth::Basic { password, .. } => password.clone(),
            JmapAuth::Bearer { token } => token.clone(),
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

/// Say why the session could not be built, in one line.
///
/// A credential the server turns down arrives as a bare `401 Unauthorized` that
/// names neither the account nor what was sent, and the mailbox loader shows an
/// error's message rather than walking its causes -- so which credential went
/// out, and which one to reach for instead, are folded into the message itself.
fn connect_error(err: jmap_client::Error, auth: &JmapAuth, base_url: &str) -> anyhow::Error {
    let url = debug_logging::redact_url(base_url);
    match http_status(&err) {
        Some(status @ (401 | 403)) => anyhow!(
            "JMAP server at {url} rejected the {credential} (HTTP {status}); {hint}",
            credential = auth.credential_description(),
            hint = auth.rejection_hint(),
        ),
        _ => anyhow!("connecting to JMAP server at {url}: {err}"),
    }
}

/// Whether `err` is the server refusing the credential rather than anything
/// else that can go wrong on the way to it.
fn credential_was_refused(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<jmap_client::Error>())
        .any(|err| matches!(http_status(err), Some(401 | 403)))
}

/// The HTTP status behind a `jmap-client` error, for the errors that carry one.
fn http_status(err: &jmap_client::Error) -> Option<u16> {
    match err {
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

/// Configuration required to bootstrap the JMAP backend.
#[derive(Clone, Debug)]
pub struct JmapConfig {
    pub base_url: String,
    pub auth: JmapAuth,
    pub trusted_hosts: Vec<String>,
}

/// Production backend that communicates with FastMail (or any other JMAP
/// provider) over HTTPS.
pub struct JmapBackend {
    runtime: Arc<Runtime>,
    config: JmapConfig,
    /// Connected session, built on first use rather than at construction.
    inner: Mutex<Option<Arc<JmapInner>>>,
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
        })
    }

    /// The connected session, connecting on the first call.
    ///
    /// Every trait method goes through here, and all of them are called from
    /// worker threads, so blocking on the connection is expected.  The lock is
    /// held across the connect so that two threads racing to be first produce
    /// one session rather than two; the loser waits and gets the same one.
    fn inner(&self) -> Result<Arc<JmapInner>> {
        let mut guard = self.inner.lock().unwrap();

        // Asked for before the cache is consulted, because for an OAuth account
        // this is what renews an access token that has run out -- and a renewed
        // token is precisely the case where the cached session has to go.
        let secret = self.runtime.block_on(self.config.auth.secret_now())?;
        debug_logging::register_secret(&secret);

        if let Some(inner) = guard.as_ref() {
            if inner.authorized_with == secret && !inner.session_rejected() {
                return Ok(Arc::clone(inner));
            }
            // Either the credential has moved on or the server stopped accepting
            // this one.  The session cannot be re-authorized in place -- the
            // library fixes the header when it connects -- so it is torn down,
            // background tasks and all, and built again below.
            debug_logging::log_session("credential changed, rebuilding the JMAP session");
            self.runtime.block_on(inner.shutdown());
            *guard = None;
        }

        // No context is added here: everything [`JmapInner::initialize`] can
        // fail at already names the server, and a wrapper would only hide the
        // rejected-credential message the status line is meant to show.
        let inner = self.runtime.block_on(JmapInner::initialize(
            Arc::clone(&self.runtime),
            self.config.clone(),
            secret,
        ))?;
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
        })?;

        Ok((snapshot, receiver))
    }

    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        let inner = self.inner()?;
        inner.runtime.block_on(inner.load_message(message_id))
    }

    fn apply_actions(&self, actions: Vec<Action>) -> Result<mpsc::Receiver<ActionStatus>> {
        let (tx, rx) = mpsc::channel();
        let inner = self.inner()?;
        let runtime = Arc::clone(&inner.runtime);

        runtime.spawn(async move {
            let mut refresh_needed = false;
            for action in actions {
                let result = inner.process_action(action.clone()).await;
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

            if refresh_needed && let Err(err) = inner.refresh_current_mailbox().await {
                debug_logging::log_session(format_args!("refresh after actions failed: {err:?}"));
                eprintln!("JMAP refresh error after actions: {err:?}");
            }
        });

        Ok(rx)
    }

    fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        let inner = self.inner()?;
        let runtime = Arc::clone(&inner.runtime);
        runtime.block_on(async move { inner.send_message(message).await })
    }

    fn save_draft(&self, message: OutgoingMessage) -> Result<()> {
        let inner = self.inner()?;
        let runtime = Arc::clone(&inner.runtime);
        runtime.block_on(async move { inner.save_draft(message).await })
    }

    fn fetch_attachment_blob(&self, blob_id: &str) -> Result<Vec<u8>> {
        let inner = self.inner()?;
        let runtime = Arc::clone(&inner.runtime);
        let client = Arc::clone(&inner.client);
        let blob_id = blob_id.to_string();
        runtime.block_on(async move {
            let trace = debug_logging::request("Blob/download", format_args!("blobId={blob_id}"));
            trace
                .response(client.download(&blob_id).await, |blob| {
                    format!("bytes={}", blob.len())
                })
                .with_context(|| format!("downloading JMAP blob {blob_id}"))
        })
    }
}

struct JmapInner {
    runtime: Arc<Runtime>,
    client: Arc<Client>,
    /// The secret this session's `Authorization` header was built from.  An
    /// access token that has since been refreshed no longer matches, which is
    /// how [`JmapBackend::inner`] knows to build a new session.
    authorized_with: String,
    /// Set when the server turned this session's credential down, so the next
    /// call reconnects instead of retrying with something it has stopped
    /// accepting.
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
        let JmapConfig {
            base_url,
            auth,
            trusted_hosts,
        } = config;

        let mut builder = Client::new().credentials(auth.credentials_with(&secret));
        if !trusted_hosts.is_empty() {
            builder = builder.follow_redirects(trusted_hosts);
        }

        let trace = debug_logging::request(
            "Session/get",
            format_args!("url={} auth={auth}", debug_logging::redact_url(&base_url)),
        );
        let client = trace
            .response(builder.connect(&base_url).await, |client| {
                format!(
                    "account={} api={}",
                    client.default_account_id(),
                    debug_logging::redact_url(client.session().api_url())
                )
            })
            .map_err(|err| connect_error(err, &auth, &base_url))?;

        let client = Arc::new(client);

        let mailboxes = Self::load_mailboxes(&client).await?;
        let identity = Self::load_identity(&client).await?;

        Ok(Arc::new(Self {
            runtime,
            client,
            authorized_with: secret,
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

    /// Whether the server has refused this session's credential.
    fn session_rejected(&self) -> bool {
        self.rejected.load(AtomicOrdering::SeqCst)
    }

    /// Stop everything running on behalf of this session.
    ///
    /// The background tasks hold their own handle on the session, so dropping
    /// the last outside reference would not end them -- they have to be told.
    async fn shutdown(&self) {
        self.stop_backfill().await;
        self.stop_event_loop().await;
    }

    async fn stop_event_loop(&self) {
        if let Some(cancel) = self.event_cancel.lock().await.take() {
            cancel.store(true, AtomicOrdering::SeqCst);
        }
        if let Some(handle) = self.event_handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
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
        let previous_highest = state.highest_received_index;
        let mut remaining_ids = state.current_sequence.clone();
        let mailbox_cache = self.mailboxes.lock().await.clone();
        let mut added_ids = Vec::new();
        let mut updated_ids = Vec::new();

        let mut new_stored = HashMap::new();

        let page_len = emails.len();
        let capacity = remaining_ids.len().max(page_len);
        let mut new_sequence = Vec::with_capacity(capacity);
        let mut page_ids = Vec::with_capacity(page_len);

        for (index, data) in emails.into_iter().enumerate() {
            let (message_id, uid, is_new) = state.ensure_ids(&data.jmap_id);
            let seq = total.saturating_sub(start_position + index).max(1) as u32;
            let message = build_message(message_id, uid, seq, &data, &mailbox_cache, mailbox)?;

            if is_new {
                added_ids.push(message_id);
            } else if let Some(previous) = state.messages.get(&message_id)
                && flags_changed(&previous.message, &message)
            {
                updated_ids.push(message_id);
            }

            if let Some(pos) = remaining_ids.iter().position(|id| *id == message_id) {
                remaining_ids.remove(pos);
            }

            new_sequence.push(message_id);
            page_ids.push(message_id);
            new_stored.insert(
                message_id,
                StoredMessage {
                    message,
                    jmap_id: data.jmap_id,
                },
            );
        }

        new_sequence.extend(remaining_ids);

        let removed = state.update_current(mailbox, new_sequence, new_stored);
        let new_highest = start_position + page_len;
        state.highest_received_index = previous_highest.max(new_highest);
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

        let mut request = self.client.build();
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
                self.move_to_mailbox(action.message_id, MailboxKind::Trash)
                    .await
            }
            ActionType::Archive => {
                self.move_to_mailbox(action.message_id, MailboxKind::Archive)
                    .await
            }
            ActionType::MoveToInboxUnread => {
                self.move_to_mailbox(action.message_id, MailboxKind::Inbox)
                    .await?;
                self.set_keyword(action.message_id, "$seen", false).await
            }
            ActionType::MoveToInboxRead => {
                self.move_to_mailbox(action.message_id, MailboxKind::Inbox)
                    .await?;
                self.set_keyword(action.message_id, "$seen", true).await
            }
            ActionType::MarkAsRead => self.set_keyword(action.message_id, "$seen", true).await,
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
                self.move_to_mailbox(action.message_id, MailboxKind::Spam)
                    .await
            }
        }
    }

    async fn send_message(self: Arc<Self>, outgoing: OutgoingMessage) -> Result<()> {
        let email = self
            .build_compose_email(outgoing)
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
                    self.client
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
                    self.client
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

        let trace = debug_logging::request(
            "EmailSubmission/set",
            format_args!("create emailId={draft_id} identityId={}", self.identity.id),
        );
        trace
            .response(
                self.client
                    .email_submission_create(draft_id.clone(), self.identity.id.clone())
                    .await,
                |_| "submitted".to_owned(),
            )
            .context("creating JMAP email submission")?;

        if let Some(sent_id) = sent_id {
            let trace = debug_logging::request(
                "Email/set",
                format_args!("update id={draft_id} mailboxIds=[{sent_id}] (Sent)"),
            );
            let _ = trace
                .response(
                    self.client.email_set_mailboxes(&draft_id, [sent_id]).await,
                    |_| "updated".to_owned(),
                )
                .context("moving submitted message to Sent mailbox")?;
        }

        let trace = debug_logging::request(
            "Email/set",
            format_args!("update id={draft_id} keywords/$draft=false"),
        );
        let _ = trace.response(
            self.client
                .email_set_keyword(&draft_id, "$draft", false)
                .await,
            |_| "updated".to_owned(),
        );

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
            .build_compose_email(outgoing)
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
                self.client
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

    async fn event_loop(self: Arc<Self>, cancel: Arc<AtomicBool>) {
        loop {
            if cancel.load(AtomicOrdering::SeqCst) {
                break;
            }

            let mut poll_interval = interval(EVENT_IDLE_POLL);
            loop {
                if cancel.load(AtomicOrdering::SeqCst) {
                    return;
                }
                let _ = poll_interval.tick().await;
                if let Err(err) = self.refresh_current_mailbox().await {
                    debug_logging::log_session(format_args!("background refresh failed: {err:?}"));
                    // A credential the server has stopped accepting -- most
                    // often an access token that ran out while the mailbox sat
                    // idle -- will not start working on a retry.  The session is
                    // marked instead, so the next thing the user does rebuilds
                    // it with a fresh credential rather than polling in vain.
                    if credential_was_refused(&err) {
                        self.rejected.store(true, AtomicOrdering::SeqCst);
                        debug_logging::log_session(
                            "server refused the credential; the session will be rebuilt on the next call",
                        );
                        return;
                    }
                    eprintln!("JMAP background refresh error: {err:?}");
                    break;
                }
            }

            if cancel.load(AtomicOrdering::SeqCst) {
                break;
            }
            tokio::time::sleep(EVENT_RETRY_DELAY).await;
        }
    }

    fn emit_diff(&self, mailbox: MailboxKind, sync: MailboxSync) {
        for message in sync.added {
            self.emit_event(mailbox, BackendEvent::NewMessage(message));
        }
        for message in sync.updated {
            self.emit_event(mailbox, BackendEvent::MessageFlagsChanged(message));
        }
        for id in sync.removed {
            self.emit_event(mailbox, BackendEvent::MessageDeleted(id));
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

    async fn move_to_mailbox(&self, message_id: MessageId, target: MailboxKind) -> Result<()> {
        let target_id = self
            .mailboxes
            .lock()
            .await
            .id_for_kind(target)
            .cloned()
            .ok_or_else(|| anyhow!("mailbox {target} is not available"))?;

        let jmap_id = self.lookup_message(message_id).await?;

        let trace = debug_logging::request(
            "Email/set",
            format_args!("update id={jmap_id} mailboxIds=[{target_id}] ({target})"),
        );
        trace
            .response(
                self.client.email_set_mailboxes(&jmap_id, [target_id]).await,
                |_| "updated".to_owned(),
            )
            .context("updating mailbox assignment")?;

        Ok(())
    }

    async fn set_keyword(&self, message_id: MessageId, keyword: &str, value: bool) -> Result<()> {
        let jmap_id = self.lookup_message(message_id).await?;
        let trace = debug_logging::request(
            "Email/set",
            format_args!("update id={jmap_id} keywords/{keyword}={value}"),
        );
        trace
            .response(
                self.client
                    .email_set_keyword(&jmap_id, keyword, value)
                    .await,
                |_| "updated".to_owned(),
            )
            .with_context(|| format!("updating keyword {keyword}"))?;
        Ok(())
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
                    eprintln!("JMAP backfill fetch error: {err:?}");
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
                    let (message_id, uid, _) = state.ensure_ids(&data.jmap_id);
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
                            },
                        )),
                        Err(err) => {
                            eprintln!("JMAP backfill build error: {err:?}");
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

        let mut request = self.client.build();
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

        let mut request = self.client.build();
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

    async fn load_identity(client: &Client) -> Result<IdentityInfo> {
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
        if let Some(identity) = response.take_list().into_iter().next() {
            let id = identity
                .id()
                .map(|id| id.to_string())
                .ok_or_else(|| anyhow!("identity has no id"))?;
            let email = identity
                .email()
                .map(|addr| addr.to_string())
                .ok_or_else(|| anyhow!("identity has no email address"))?;
            let name = identity.name().map(|name| name.to_string());
            Ok(IdentityInfo { id, email, name })
        } else {
            let session = client.session();
            let email = session.username().to_string();
            Ok(IdentityInfo {
                id: email.clone(),
                email,
                name: None,
            })
        }
    }

    fn build_compose_email(&self, outgoing: OutgoingMessage) -> Result<LettreEmail> {
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

        let mut builder = LettreEmail::builder().from(from_addr);

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
    fn ensure_ids(&mut self, jmap_id: &str) -> (MessageId, u32, bool) {
        if let Some(id) = self.jmap_to_id.get(jmap_id).copied() {
            let uid = self
                .messages
                .get(&id)
                .map(|stored| stored.message.uid)
                .unwrap_or_else(|| {
                    self.next_uid += 1;
                    self.next_uid - 1
                });
            (id, uid, false)
        } else {
            self.next_message_id = self.next_message_id.saturating_add(1).max(1);
            let id = self.next_message_id;
            self.next_uid = self.next_uid.saturating_add(1).max(1);
            let uid = self.next_uid;
            self.jmap_to_id.insert(jmap_id.to_string(), id);
            (id, uid, true)
        }
    }

    fn update_current(
        &mut self,
        mailbox: MailboxKind,
        new_sequence: Vec<MessageId>,
        new_messages: HashMap<MessageId, StoredMessage>,
    ) -> Vec<MessageId> {
        let mut removed = Vec::new();
        let new_set: HashSet<_> = new_sequence.iter().copied().collect();
        for id in &self.current_sequence {
            if !new_set.contains(id) {
                removed.push(*id);
            }
        }

        for id in &removed {
            if let Some(stored) = self.messages.get_mut(id) {
                stored.message.status = match mailbox {
                    MailboxKind::Trash => MessageStatus::Deleted,
                    MailboxKind::Spam => MessageStatus::Spam,
                    _ => stored.message.status,
                };
            }
        }

        for (id, stored) in new_messages {
            self.messages.insert(id, stored);
        }

        self.current_sequence = new_sequence;

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
        let mailbox_ids = email
            .mailbox_ids()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
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

fn flags_changed(before: &Message, after: &Message) -> bool {
    before.status != after.status
        || before.starred != after.starred
        || before.important != after.important
        || before.answered != after.answered
        || before.forwarded != after.forwarded
        || before.has_attachments != after.has_attachments
        || before.labels != after.labels
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
    })
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

    #[test]
    fn status_is_read_off_a_bare_status_line() {
        let err = jmap_client::Error::Server("401 Unauthorized".to_string());
        assert_eq!(http_status(&err), Some(401));
    }

    #[test]
    fn a_server_message_without_a_status_reads_as_none() {
        let err = jmap_client::Error::Server("connection reset".to_string());
        assert_eq!(http_status(&err), None);
    }

    #[test]
    fn a_rejected_password_names_the_account_and_offers_the_token() {
        let message = connect_error(
            jmap_client::Error::Server("401 Unauthorized".to_string()),
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
            jmap_client::Error::Server("403 Forbidden".to_string()),
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
            jmap_client::Error::Server("503 Service Unavailable".to_string()),
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
            jmap_client::Error::Server("401 Unauthorized".to_string()),
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
}
