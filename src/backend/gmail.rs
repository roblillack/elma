//! Gmail backend built on the IMAP protocol.
//!
//! The implementation shares the same public contract as the mock backend but runs
//! all network I/O on an internal Tokio runtime.  Actions are processed asynchronously
//! so the terminal UI never blocks while Gmail applies flag and mailbox updates.

use crate::{
    backend::{ActionStatus, BackendEvent, MailBackend, MailboxSnapshot, OutgoingMessage},
    model::{
        Action, ActionType, MailboxKind, Message, MessageAttachment, MessageContent,
        MessageContentPart, MessageId, MessageStatus,
    },
};
use anyhow::{Context, Result, anyhow};
use async_imap::{
    Session,
    extensions::idle::IdleResponse,
    types::{Fetch, Flag},
};
use async_native_tls::connect as tls_connect;
use futures::TryStreamExt;
use imap_proto::types::{
    AttributeValue, BodyContentCommon, BodyParams, BodyStructure, MailboxDatum, NameAttribute,
    Response,
};
use lettre::{
    Message as LettreEmail, SmtpTransport, Transport,
    message::{Mailbox as LettreMailbox, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
};
use mailparse::{self, DispositionType, MailHeaderMap, ParsedMail};
use std::{
    collections::{BTreeMap, HashMap},
    str,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use time::OffsetDateTime;
use tokio::time as tokio_time;
use tokio::{
    net::TcpStream,
    runtime::Runtime,
    sync::{Mutex as AsyncMutex, oneshot},
    task::JoinHandle,
};

#[cfg(debug_assertions)]
mod debug_logging {
    use std::{
        fs::OpenOptions,
        io::{self, BufWriter, IoSlice, Write},
        pin::Pin,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

    use anyhow::{Context as AnyhowContext, Result};
    use async_native_tls::TlsStream;
    use chrono::{Local, Utc};
    use tokio::io::ReadBuf;
    use tokio::net::TcpStream;

    #[derive(Debug)]
    pub struct GmailImapLogger {
        file: Mutex<BufWriter<std::fs::File>>,
        next_id: AtomicUsize,
    }

    impl GmailImapLogger {
        fn init() -> Result<Self> {
            let now = Local::now();
            let stamp = now.format("%Y-%m-%d-%H%M").to_string();
            let pid = std::process::id();
            let filename = format!("gmail-log-{stamp}-{pid}.log");
            let path = std::env::current_dir()
                .context("determining current directory for Gmail log file")?
                .join(filename);

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening Gmail debug log file at {}", path.display()))?;

            let mut writer = BufWriter::new(file);
            let header_time = now.format("%Y-%m-%d %H:%M:%S");
            writeln!(
                writer,
                "# Gmail IMAP debug log started {header_time} local, pid {pid}"
            )
            .ok();

            Ok(Self {
                file: Mutex::new(writer),
                next_id: AtomicUsize::new(1),
            })
        }

        pub fn global() -> Result<Arc<Self>> {
            static LOGGER: OnceLock<Arc<GmailImapLogger>> = OnceLock::new();
            if let Some(logger) = LOGGER.get() {
                return Ok(Arc::clone(logger));
            }

            let logger = Arc::new(Self::init()?);
            match LOGGER.set(Arc::clone(&logger)) {
                Ok(()) => Ok(logger),
                Err(_) => Ok(Arc::clone(
                    LOGGER
                        .get()
                        .expect("Gmail IMAP logger should be present after set"),
                )),
            }
        }

        fn allocate_connection_id(&self) -> usize {
            self.next_id.fetch_add(1, Ordering::AcqRel)
        }

        fn log_event(&self, connection_id: usize, label: &str, payload: &str) {
            let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ");
            if let Ok(mut writer) = self.file.lock() {
                let _ = writeln!(
                    writer,
                    "{timestamp} conn#{connection_id:02} {label}: {payload}"
                );
                let _ = writer.flush();
            }
        }

        fn log_data(&self, connection_id: usize, direction: &str, data: &[u8]) {
            // Convert to UTF-8-friendly string, replacing invalid bytes.
            let text = String::from_utf8_lossy(data);
            for segment in text.split_inclusive('\n') {
                let trimmed = segment.trim_end_matches(['\r', '\n']);
                if segment.ends_with('\n') {
                    let mut owned = trimmed.to_owned();
                    owned.push_str(" <CRLF>");
                    self.log_event(connection_id, direction, &owned);
                } else {
                    self.log_event(connection_id, direction, trimmed);
                }
            }
        }
    }

    #[derive(Debug)]
    pub struct LoggedTlsStream {
        inner: TlsStream<TcpStream>,
        logger: Arc<GmailImapLogger>,
        connection_id: usize,
    }

    impl LoggedTlsStream {
        pub fn new(inner: TlsStream<TcpStream>) -> Result<Self> {
            let logger = GmailImapLogger::global()?;
            let connection_id = logger.allocate_connection_id();
            logger.log_event(connection_id, "INFO", "IMAP connection established");

            Ok(Self {
                inner,
                logger,
                connection_id,
            })
        }

        fn log_outgoing(&self, data: &[u8]) {
            self.logger.log_data(self.connection_id, "C->S", data);
        }

        fn log_incoming(&self, data: &[u8]) {
            self.logger.log_data(self.connection_id, "S->C", data);
        }
    }

    impl Unpin for LoggedTlsStream {}

    impl Drop for LoggedTlsStream {
        fn drop(&mut self) {
            self.logger
                .log_event(self.connection_id, "INFO", "IMAP connection closed");
        }
    }

    impl tokio::io::AsyncRead for LoggedTlsStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let before = buf.filled().len();
            match Pin::new(&mut this.inner).poll_read(cx, buf) {
                Poll::Ready(Ok(())) => {
                    let after = buf.filled().len();
                    if after > before {
                        this.log_incoming(&buf.filled()[before..after]);
                    }
                    Poll::Ready(Ok(()))
                }
                other => other,
            }
        }
    }

    impl tokio::io::AsyncWrite for LoggedTlsStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            match Pin::new(&mut this.inner).poll_write(cx, buf) {
                Poll::Ready(Ok(written)) => {
                    if written > 0 {
                        this.log_outgoing(&buf[..written]);
                    }
                    Poll::Ready(Ok(written))
                }
                other => other,
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            match Pin::new(&mut this.inner).poll_write_vectored(cx, bufs) {
                Poll::Ready(Ok(written)) => {
                    if written > 0 {
                        let mut remaining = written;
                        let mut aggregated = Vec::with_capacity(written);
                        for slice in bufs {
                            if remaining == 0 {
                                break;
                            }
                            let take = remaining.min(slice.len());
                            aggregated.extend_from_slice(&slice[..take]);
                            remaining -= take;
                        }
                        this.log_outgoing(&aggregated);
                    }
                    Poll::Ready(Ok(written))
                }
                other => other,
            }
        }

        fn is_write_vectored(&self) -> bool {
            self.inner.is_write_vectored()
        }
    }

    pub fn wrap_stream(stream: TlsStream<TcpStream>) -> Result<LoggedTlsStream> {
        LoggedTlsStream::new(stream)
    }

    pub fn log_backend_event(label: &str, payload: &str) {
        if let Ok(logger) = GmailImapLogger::global() {
            logger.log_event(0, label, payload);
        }
    }
}

#[cfg(not(debug_assertions))]
mod debug_logging {
    use async_native_tls::TlsStream;
    use tokio::net::TcpStream;

    pub type LoggedTlsStream = TlsStream<TcpStream>;

    pub fn wrap_stream(stream: TlsStream<TcpStream>) -> anyhow::Result<LoggedTlsStream> {
        Ok(stream)
    }

    pub fn log_backend_event(_label: &str, _payload: &str) {}
}

use debug_logging::{LoggedTlsStream, log_backend_event, wrap_stream};

type AsyncSession = Session<LoggedTlsStream>;

const GMAIL_HOST: &str = "imap.gmail.com";
const GMAIL_PORT: u16 = 993;
const DEFAULT_STARRED_LABEL: &str = "[Gmail]/Starred";
const DEFAULT_IMPORTANT_LABEL: &str = "[Gmail]/Important";
const DEFAULT_ARCHIVE_LABEL: &str = "[Gmail]/All Mail";
const DEFAULT_SPAM_LABEL: &str = "[Gmail]/Spam";
const DEFAULT_SENT_LABEL: &str = "[Gmail]/Sent Mail";
const DEFAULT_DRAFTS_LABEL: &str = "[Gmail]/Drafts";
const DEFAULT_TRASH_LABEL: &str = "[Gmail]/Trash";
const MAX_PART_DEPTH: usize = 5;
const INITIAL_FETCH_LIMIT: u32 = 100;
const BACKFILL_BATCH_SIZE: u32 = 100;
const FETCH_MESSAGE_QUERY: &str = "(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE UID BODYSTRUCTURE)";

/// Production backend that communicates with Gmail over IMAP.
///
/// A small Tokio runtime is embedded inside the struct so the rest of the
/// application can stay synchronous.  All long-running work (IDLE loop, action
/// commits, message downloads) is scheduled onto that runtime.
pub struct GmailBackend {
    inner: Arc<GmailInner>,
}

/// Shared state for the Gmail backend.
///
/// `GmailInner` owns the runtime, caches, and mutable data structures so tasks can
/// coordinate through `Arc` and async locks.
struct GmailInner {
    email: String,
    password: String,
    runtime: Arc<Runtime>,
    session: AsyncMutex<Option<AsyncSession>>,
    state: AsyncMutex<SharedState>,
    labels: AsyncMutex<SpecialMailboxes>,
    current_mailbox: AsyncMutex<MailboxKind>,
    events: Mutex<Option<mpsc::Sender<BackendEvent>>>,
    idle_stop: AsyncMutex<Option<oneshot::Sender<()>>>,
    idle_handle: AsyncMutex<Option<JoinHandle<()>>>,
    backfill_handle: AsyncMutex<Option<JoinHandle<()>>>,
    backfill_cancel: AsyncMutex<Option<Arc<AtomicBool>>>,
}

/// Cached view of the Gmail mailbox.
#[derive(Default)]
struct SharedState {
    messages: HashMap<MessageId, StoredMessage>,
    seq_to_id: BTreeMap<u32, MessageId>,
    uid_to_id: HashMap<u32, MessageId>,
    expected_exists: u32,
}

/// Metadata we retain for every message Gmail reports.
struct StoredMessage {
    message: Message,
    seq: u32,
    uid: u32,
}

/// User-specific Gmail labels that correspond to archive/trash.
#[derive(Clone)]
struct SpecialMailboxes {
    starred: String,
    important: String,
    archive: String,
    spam: String,
    sent: String,
    drafts: String,
    trash: String,
}

impl Default for SpecialMailboxes {
    fn default() -> Self {
        Self {
            starred: DEFAULT_STARRED_LABEL.to_string(),
            important: DEFAULT_IMPORTANT_LABEL.to_string(),
            archive: DEFAULT_ARCHIVE_LABEL.to_string(),
            spam: DEFAULT_SPAM_LABEL.to_string(),
            sent: DEFAULT_SENT_LABEL.to_string(),
            drafts: DEFAULT_DRAFTS_LABEL.to_string(),
            trash: DEFAULT_TRASH_LABEL.to_string(),
        }
    }
}

impl GmailBackend {
    /// Create a Gmail backend bound to the given account.
    ///
    /// A dedicated Tokio runtime is created up front so subsequent operations can
    /// spawn tasks without blocking the caller.
    pub fn new<E, P>(email: E, password: P) -> Result<Self>
    where
        E: Into<String>,
        P: Into<String>,
    {
        let runtime =
            Arc::new(Runtime::new().context("failed to create Tokio runtime for Gmail backend")?);

        Ok(Self {
            inner: Arc::new(GmailInner {
                email: email.into(),
                password: password.into(),
                runtime,
                session: AsyncMutex::new(None),
                state: AsyncMutex::new(SharedState::default()),
                labels: AsyncMutex::new(SpecialMailboxes::default()),
                current_mailbox: AsyncMutex::new(MailboxKind::Inbox),
                events: Mutex::new(None),
                idle_stop: AsyncMutex::new(None),
                idle_handle: AsyncMutex::new(None),
                backfill_handle: AsyncMutex::new(None),
                backfill_cancel: AsyncMutex::new(None),
            }),
        })
    }
}

impl MailBackend for GmailBackend {
    /// Fetch `mailbox` and subscribe to Gmail's IDLE notifications for it.
    fn load_mailbox(
        &self,
        mailbox: MailboxKind,
    ) -> Result<(MailboxSnapshot, mpsc::Receiver<BackendEvent>)> {
        let (sender, receiver) = mpsc::channel();

        let snapshot = self.inner.runtime.block_on(async {
            self.inner.ensure_connected().await?;
            self.inner.stop_backfill_task().await;
            self.inner.pause_idle().await?;

            // Install the new event sender only after the old backfill and idle
            // tasks have fully stopped, so their final events go to the old
            // (now-discarded) channel instead of this new one.
            {
                let mut guard = self.inner.events.lock().unwrap();
                *guard = Some(sender);
            }
            let exists = self.inner.select_mailbox(mailbox).await?;
            let messages = self.inner.refresh_selected_mailbox(exists).await?;
            {
                let mut current = self.inner.current_mailbox.lock().await;
                *current = mailbox;
            }
            self.inner.start_idle_loop().await?;
            self.inner.start_backfill_if_needed().await?;

            Ok::<_, anyhow::Error>(MailboxSnapshot {
                total: exists as usize,
                messages,
            })
        })?;

        Ok((snapshot, receiver))
    }

    /// Download the full MIME body for `message_id`.
    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        self.inner.runtime.block_on(async {
            self.inner.stop_backfill_task().await;
            self.inner.pause_idle().await?;
            let uid = {
                let state = self.inner.state.lock().await;
                state
                    .messages
                    .get(&message_id)
                    .map(|stored| stored.uid)
                    .ok_or_else(|| anyhow!("message {message_id} not found in cache"))?
            };

            let mut session_guard = self.inner.session.lock().await;
            let session = session_guard
                .as_mut()
                .ok_or_else(|| anyhow!("IMAP session is not available"))?;

            let mut content = None;
            {
                let mut fetches = session
                    .uid_fetch(uid.to_string(), "(BODY.PEEK[])")
                    .await
                    .context("fetching full message")?;

                while let Some(fetch) = fetches.try_next().await? {
                    if let Some(body) = fetch.body() {
                        let parsed = mailparse::parse_mail(body)
                            .context("parsing message MIME structure")?;
                        content = Some(build_message_content(&parsed)?);
                    }
                }
            }

            self.inner
                .fetch_gmail_labels_command(session, &format!("UID FETCH {uid} (UID X-GM-LABELS)"))
                .await?;

            self.inner.start_idle_loop().await?;
            self.inner.start_backfill_if_needed().await?;

            content.ok_or_else(|| anyhow!("message body not returned by server"))
        })
    }

    /// Schedule a batch of Gmail actions on the internal runtime.
    ///
    /// Each action is processed sequentially so we can interleave the IMAP commands
    /// with pauses in the IDLE loop.  The returned channel yields an [`ActionStatus`]
    /// per action, mirroring the contract defined in [`MailBackend`].
    fn apply_actions(&self, actions: Vec<Action>) -> Result<mpsc::Receiver<ActionStatus>> {
        let (tx, rx) = mpsc::channel();
        let runtime = Arc::clone(&self.inner.runtime);
        let inner = Arc::clone(&self.inner);

        runtime.spawn(async move {
            for action in actions {
                let result = Arc::clone(&inner)
                    .process_action(action.clone())
                    .await
                    .map_err(|err| err.to_string());
                if tx.send(ActionStatus { action, result }).is_err() {
                    break;
                }
            }
        });

        Ok(rx)
    }

    fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        let runtime = Arc::clone(&self.inner.runtime);
        let inner = Arc::clone(&self.inner);
        runtime.block_on(async move { inner.send_via_smtp(message).await })
    }

    fn save_draft(&self, message: OutgoingMessage) -> Result<()> {
        let runtime = Arc::clone(&self.inner.runtime);
        let inner = Arc::clone(&self.inner);
        runtime.block_on(async move { inner.save_draft(message).await })
    }
}

impl GmailInner {
    /// Ensure we have an authenticated IMAP session ready to use.
    async fn ensure_connected(&self) -> Result<()> {
        let mut guard = self.session.lock().await;
        if guard.is_some() {
            return Ok(());
        }

        let tcp = TcpStream::connect((GMAIL_HOST, GMAIL_PORT))
            .await
            .context("connecting to Gmail IMAP server")?;
        let tls_stream = tls_connect(GMAIL_HOST, tcp)
            .await
            .context("starting TLS handshake with Gmail")?;
        let tls_stream = wrap_stream(tls_stream).context("enabling Gmail IMAP debug logging")?;
        let mut client = async_imap::Client::new(tls_stream);
        client
            .read_response()
            .await
            .context("reading IMAP greeting")?
            .ok_or_else(|| anyhow!("connection closed before IMAP greeting"))?;
        let session = client
            .login(&self.email, &self.password)
            .await
            .map_err(|(err, _)| err)
            .context("logging in to Gmail")?;

        *guard = Some(session);

        drop(guard);
        self.determine_special_mailboxes().await?;

        Ok(())
    }

    async fn stop_backfill_task(&self) {
        if let Some(cancel) = self.backfill_cancel.lock().await.take() {
            cancel.store(true, Ordering::SeqCst);
        }
        let handle = {
            let mut guard = self.backfill_handle.lock().await;
            guard.take()
        };
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    async fn start_backfill_if_needed(self: &Arc<Self>) -> Result<()> {
        let has_work = {
            let state = self.state.lock().await;
            state
                .next_backfill_range(BACKFILL_BATCH_SIZE as usize)
                .is_some()
        };

        if !has_work {
            self.stop_backfill_task().await;
            return Ok(());
        }

        self.stop_backfill_task().await;

        let cancel_flag = Arc::new(AtomicBool::new(false));
        {
            let mut cancel_guard = self.backfill_cancel.lock().await;
            *cancel_guard = Some(Arc::clone(&cancel_flag));
        }

        let this = Arc::clone(self);
        let handle = self.runtime.spawn(async move {
            this.backfill_loop(cancel_flag).await;
        });

        let mut handle_guard = self.backfill_handle.lock().await;
        *handle_guard = Some(handle);

        Ok(())
    }

    async fn backfill_loop(self: Arc<Self>, cancel: Arc<AtomicBool>) {
        loop {
            if cancel.load(Ordering::SeqCst) {
                break;
            }

            let range = {
                let state = self.state.lock().await;
                state.next_backfill_range(BACKFILL_BATCH_SIZE as usize)
            };

            let Some((start, end)) = range else {
                break;
            };

            if let Err(err) = self.load_backfill_batch(start, end, &cancel).await {
                eprintln!("Gmail backfill error: {err:?}");
                break;
            }
        }
    }

    async fn load_backfill_batch(
        self: &Arc<Self>,
        start: u32,
        end: u32,
        cancel: &Arc<AtomicBool>,
    ) -> Result<()> {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.pause_idle().await?;
        let mut session_guard = self.session.lock().await;
        let session = session_guard
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP session is not available"))?;

        let stored_messages = match self
            .fetch_message_range(session, start, end)
            .await
            .with_context(|| format!("fetching backfill range {start}:{end}"))
        {
            Ok(messages) => messages,
            Err(err) => {
                drop(session_guard);
                let _ = self.start_idle_loop().await;
                return Err(err);
            }
        };

        let mut to_emit_ids = Vec::new();
        {
            let mut state = self.state.lock().await;
            for stored in stored_messages {
                to_emit_ids.push(stored.message.id);
                state.insert(stored);
            }
        }

        if to_emit_ids.is_empty() {
            cancel.store(true, Ordering::SeqCst);
        } else if !cancel.load(Ordering::SeqCst) {
            let command = if start == end {
                format!("FETCH {start} (UID X-GM-LABELS)")
            } else {
                format!("FETCH {start}:{end} (UID X-GM-LABELS)")
            };
            if let Err(err) = self.fetch_gmail_labels_command(session, &command).await {
                eprintln!("Gmail backfill label fetch error: {err:?}");
            }
        }

        drop(session_guard);
        let restart_result = self.start_idle_loop().await;

        // Re-read messages from state after labels have been applied,
        // so NewMessage events carry up-to-date importance and label data.
        {
            let state = self.state.lock().await;
            for id in to_emit_ids {
                if let Some(stored) = state.messages.get(&id) {
                    self.emit_event(BackendEvent::NewMessage(stored.message.clone()));
                }
            }
        }

        restart_result
    }

    /// Resolve the IMAP mailbox name for `mailbox`.
    async fn mailbox_name(&self, mailbox: MailboxKind) -> Result<String> {
        let labels = self.labels.lock().await;
        let name = match mailbox {
            MailboxKind::Inbox => "INBOX".to_string(),
            MailboxKind::Starred => labels.starred.clone(),
            MailboxKind::Important => labels.important.clone(),
            MailboxKind::Sent => labels.sent.clone(),
            MailboxKind::Drafts => labels.drafts.clone(),
            MailboxKind::Archive => labels.archive.clone(),
            MailboxKind::Spam => labels.spam.clone(),
            MailboxKind::Trash => labels.trash.clone(),
        };
        Ok(name)
    }

    /// Select the given mailbox so subsequent fetches operate on the right context.
    async fn select_mailbox(&self, mailbox: MailboxKind) -> Result<u32> {
        let name = self.mailbox_name(mailbox).await?;
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP session is not available"))?;
        let mailbox_info = session
            .select(&name)
            .await
            .with_context(|| format!("selecting mailbox {name}"))?;
        Ok(mailbox_info.exists)
    }

    /// Refresh the cached state for the currently selected mailbox.
    async fn refresh_selected_mailbox(&self, exists: u32) -> Result<Vec<Message>> {
        let mut messages = Vec::new();
        let mut new_state = SharedState::default();
        new_state.set_expected_exists(exists);

        if exists == 0 {
            let mut state_guard = self.state.lock().await;
            *state_guard = new_state;
            return Ok(messages);
        }

        let end = exists;
        let fetch_count = INITIAL_FETCH_LIMIT.min(exists);
        let start = end - fetch_count + 1;

        let mut message_ids = Vec::new();
        {
            let mut session_guard = self.session.lock().await;
            let session = session_guard
                .as_mut()
                .ok_or_else(|| anyhow!("IMAP session is not available"))?;

            let stored_messages = self
                .fetch_message_range(session, start, end)
                .await
                .context("fetching initial mailbox slice")?;
            for stored in stored_messages {
                message_ids.push(stored.message.id);
                new_state.insert(stored);
            }

            // Install new_state before the label fetch so handle_fetch_update
            // finds the messages in the correct state.
            {
                let mut state_guard = self.state.lock().await;
                *state_guard = new_state;
            }

            if !message_ids.is_empty() {
                let command = if start == end {
                    format!("FETCH {start} (UID X-GM-LABELS)")
                } else {
                    format!("FETCH {start}:{end} (UID X-GM-LABELS)")
                };
                self.fetch_gmail_labels_command(session, &command).await?;
            }
        }

        // Re-read messages from state after labels have been applied.
        {
            let state = self.state.lock().await;
            for id in message_ids {
                if let Some(stored) = state.messages.get(&id) {
                    messages.push(stored.message.clone());
                }
            }
        }

        messages.sort_by_key(|msg| msg.seq);
        Ok(messages)
    }

    async fn fetch_message_range(
        &self,
        session: &mut AsyncSession,
        start: u32,
        end: u32,
    ) -> Result<Vec<StoredMessage>> {
        if start == 0 || end == 0 || start > end {
            return Ok(Vec::new());
        }

        let range = if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        };

        let mut fetch_stream = session.fetch(&range, FETCH_MESSAGE_QUERY).await?;
        let mut collected = Vec::new();

        while let Some(fetch) = fetch_stream.try_next().await? {
            if let Some(stored) = build_message_from_fetch(&fetch)? {
                collected.push(stored);
            }
        }

        Ok(collected)
    }

    /// Discover the Gmail archive and trash mailboxes so we can move messages later.
    async fn determine_special_mailboxes(&self) -> Result<()> {
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP session is not available"))?;

        let mut list_stream = session
            .list(Some(""), Some("*"))
            .await
            .context("listing mailboxes")?;

        let mut archive = None;
        let mut trash = None;
        let mut spam = None;
        let mut starred = None;
        let mut important = None;
        let mut sent = None;
        let mut drafts = None;

        while let Some(name) = list_stream.try_next().await? {
            let attrs = name.attributes();
            let entry_name = name.name().to_string();
            if attrs.iter().any(|attr| matches!(attr, NameAttribute::All)) {
                archive = Some(entry_name.clone());
            }
            if attrs
                .iter()
                .any(|attr| matches!(attr, NameAttribute::Trash))
            {
                trash = Some(entry_name.clone());
            }
            if attrs.iter().any(|attr| matches!(attr, NameAttribute::Junk)) {
                spam = Some(entry_name.clone());
            }
            if attrs
                .iter()
                .any(|attr| matches!(attr, NameAttribute::Flagged))
            {
                starred = Some(entry_name.clone());
            }
            if important.is_none() {
                let lower = entry_name.to_ascii_lowercase();
                if lower == "\\important" || lower == "important" || lower.ends_with("/important") {
                    important = Some(entry_name.clone());
                }
            }
            if attrs.iter().any(|attr| matches!(attr, NameAttribute::Sent)) {
                sent = Some(entry_name.clone());
            }
            if attrs
                .iter()
                .any(|attr| matches!(attr, NameAttribute::Drafts))
            {
                drafts = Some(entry_name);
            }
        }

        {
            let mut labels = self.labels.lock().await;
            if let Some(value) = archive {
                labels.archive = value;
            }
            if let Some(value) = trash {
                labels.trash = value;
            }
            if let Some(value) = spam {
                labels.spam = value;
            }
            if let Some(value) = starred {
                labels.starred = value;
            }
            if let Some(value) = important {
                labels.important = value;
            }
            if let Some(value) = sent {
                labels.sent = value;
            }
            if let Some(value) = drafts {
                labels.drafts = value;
            }
        }

        Ok(())
    }

    async fn fetch_gmail_labels_command(
        &self,
        session: &mut AsyncSession,
        command: &str,
    ) -> Result<()> {
        let command_text = command.to_string();
        let tag = session
            .run_command(&command_text)
            .await
            .with_context(|| format!("issuing command `{command_text}`"))?;

        loop {
            let Some(response) = session
                .read_response()
                .await
                .context("reading Gmail label response")?
            else {
                return Err(anyhow!(
                    "IMAP connection closed while fetching Gmail labels"
                ));
            };

            let parsed = response.parsed();
            match parsed {
                Response::Fetch(seq, attrs) => {
                    let _ = self.handle_fetch_update(*seq, attrs).await?;
                }
                Response::Done { tag: done_tag, .. } if done_tag == &tag => break,
                Response::Expunge(seq) => {
                    self.handle_expunge(*seq).await;
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn send_via_smtp(self: Arc<Self>, outgoing: OutgoingMessage) -> Result<()> {
        let email = self
            .build_compose_email(outgoing)
            .context("building SMTP message")?;
        let account = self.email.clone();
        let password = self.password.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let creds = Credentials::new(account, password);
            let transport = SmtpTransport::relay("smtp.gmail.com")
                .context("configuring Gmail SMTP relay")?
                .credentials(creds)
                .build();
            transport
                .send(&email)
                .map_err(|err| anyhow!("SMTP send failed: {err}"))?;
            Ok(())
        })
        .await
        .context("joining SMTP sender task")??;

        Ok(())
    }

    async fn save_draft(self: Arc<Self>, outgoing: OutgoingMessage) -> Result<()> {
        let email = self
            .build_compose_email(outgoing)
            .context("building draft message")?;
        let raw = email.formatted();

        self.pause_idle().await?;
        let append_result = self
            .append_raw_message(MailboxKind::Drafts, &raw, &["\\Draft"])
            .await;
        let restart_result = self.start_idle_loop().await;
        append_result?;
        restart_result?;
        Ok(())
    }

    fn build_compose_email(&self, outgoing: OutgoingMessage) -> Result<LettreEmail> {
        let OutgoingMessage {
            to,
            cc,
            bcc,
            subject,
            text_body,
            html_body,
        } = outgoing;

        if to.is_empty() && cc.is_empty() && bcc.is_empty() {
            return Err(anyhow!("message must have at least one recipient"));
        }

        let from_mailbox: LettreMailbox = self
            .email
            .parse()
            .with_context(|| format!("invalid Gmail address: {}", self.email))?;

        let mut builder = LettreEmail::builder().from(from_mailbox);

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

        let multipart = MultiPart::alternative()
            .singlepart(SinglePart::plain(text_body))
            .singlepart(SinglePart::html(html_body));

        builder
            .multipart(multipart)
            .context("serialising compose body for SMTP")
    }

    async fn append_raw_message(
        &self,
        mailbox: MailboxKind,
        raw: &[u8],
        flags: &[&str],
    ) -> Result<()> {
        self.ensure_connected().await?;
        let name = self.mailbox_name(mailbox).await?;
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP session is not available"))?;
        let flags_literal = if flags.is_empty() {
            None
        } else {
            Some(format!("({})", flags.join(" ")))
        };
        session
            .append(&name, flags_literal.as_deref(), None, raw)
            .await
            .context("appending message to mailbox")?;
        Ok(())
    }

    /// Spawn the IDLE task that listens for new Gmail events.
    async fn start_idle_loop(self: &Arc<Self>) -> Result<()> {
        let mut handle_guard = self.idle_handle.lock().await;
        if handle_guard.is_some() {
            return Ok(());
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut stop_guard = self.idle_stop.lock().await;
            *stop_guard = Some(tx);
        }

        let this = Arc::clone(self);
        let join = self.runtime.spawn(async move {
            this.idle_task(rx).await;
        });

        *handle_guard = Some(join);
        Ok(())
    }

    /// Tear down the IDLE loop so another task can operate on the IMAP session.
    async fn pause_idle(&self) -> Result<()> {
        let stop = {
            let mut guard = self.idle_stop.lock().await;
            guard.take()
        };
        if let Some(stop_tx) = stop {
            let _ = stop_tx.send(());
        }

        if let Some(handle) = self.idle_handle.lock().await.take() {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Pause the IDLE worker, apply `action`, then restart listening for updates.
    async fn process_action(self: Arc<Self>, action: Action) -> Result<()> {
        self.pause_idle().await?;
        let result = self
            .apply_action_internal(&action)
            .await
            .with_context(|| format!("applying action {:?}", action.action_type));
        let restart = self.start_idle_loop().await;
        restart?;
        result
    }

    /// Long-lived task that keeps Gmail notifications flowing via the IDLE extension.
    async fn idle_task(self: Arc<Self>, mut stop_rx: oneshot::Receiver<()>) {
        loop {
            if let Err(err) = self.ensure_connected().await {
                eprintln!("Gmail idle reconnect error: {err:?}");
                tokio_time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            let session = {
                let mut guard = self.session.lock().await;
                guard.take()
            };

            let session = match session {
                Some(session) => session,
                None => continue,
            };

            let mut idle_handle = session.idle();
            if let Err(err) = idle_handle.init().await {
                eprintln!("Gmail idle init error: {err:?}");
                tokio_time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            let (wait_fut, stopper) = idle_handle.wait_with_timeout(Duration::from_secs(300));
            let result = tokio::select! {
                _ = &mut stop_rx => {
                    drop(stopper);
                    if let Ok(sess) = idle_handle.done().await {
                        self.reinsert_session(sess).await;
                    }
                    break;
                }
                wait_result = wait_fut => wait_result,
            };

            match result {
                Ok(IdleResponse::Timeout) | Ok(IdleResponse::ManualInterrupt) => {
                    if let Ok(mut sess) = idle_handle.done().await {
                        let _ = sess.noop().await;
                        self.reinsert_session(sess).await;
                    }
                }
                Ok(IdleResponse::NewData(resp)) => {
                    drop(stopper);

                    let mut exists = Vec::new();
                    let mut label_refresh = Vec::new();
                    if let Err(err) = self
                        .process_idle_response(resp.parsed(), &mut exists, &mut label_refresh)
                        .await
                    {
                        eprintln!("Gmail idle processing error: {err:?}");
                    }

                    loop {
                        let (next_wait, next_stopper) =
                            idle_handle.wait_with_timeout(Duration::from_millis(0));
                        match next_wait.await {
                            Ok(IdleResponse::NewData(resp)) => {
                                if let Err(err) = self
                                    .process_idle_response(
                                        resp.parsed(),
                                        &mut exists,
                                        &mut label_refresh,
                                    )
                                    .await
                                {
                                    eprintln!("Gmail idle processing error: {err:?}");
                                }
                                drop(next_stopper);
                            }
                            Ok(IdleResponse::Timeout) | Ok(IdleResponse::ManualInterrupt) => {
                                drop(next_stopper);
                                break;
                            }
                            Err(err) => {
                                drop(next_stopper);
                                eprintln!("Gmail idle additional wait error: {err:?}");
                                break;
                            }
                        }
                    }

                    if exists.is_empty() && label_refresh.is_empty() {
                        continue;
                    }

                    match idle_handle.done().await {
                        Ok(mut sess) => {
                            for count in exists {
                                if let Err(err) = self.handle_exists(&mut sess, count).await {
                                    eprintln!("Gmail idle EXISTS handling error: {err:?}");
                                }
                            }
                            if !label_refresh.is_empty() {
                                label_refresh.sort_unstable();
                                label_refresh.dedup();
                                for uid in label_refresh {
                                    let command = format!("UID FETCH {uid} (UID X-GM-LABELS)");
                                    if let Err(err) =
                                        self.fetch_gmail_labels_command(&mut sess, &command).await
                                    {
                                        eprintln!("Gmail label refresh error: {err:?}");
                                    }
                                }
                            }
                            self.reinsert_session(sess).await;
                        }
                        Err(err) => {
                            eprintln!("Gmail idle completion error: {err:?}");
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Gmail idle wait error: {err:?}");
                    if let Ok(sess) = idle_handle.done().await {
                        self.reinsert_session(sess).await;
                    }
                    tokio_time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn process_idle_response(
        &self,
        response: &Response<'_>,
        pending_exists: &mut Vec<u32>,
        pending_labels: &mut Vec<u32>,
    ) -> Result<()> {
        match response {
            Response::Expunge(seq) => {
                log_backend_event("BACKEND", &format!("processing EXPUNGE for seq {seq}"));
                self.handle_expunge(*seq).await;
            }
            Response::MailboxData(MailboxDatum::Exists(count)) => {
                log_backend_event("BACKEND", &format!("queueing EXISTS with count {count}"));
                pending_exists.push(*count);
            }
            Response::Fetch(seq, attrs) => {
                log_backend_event("BACKEND", &format!("processing FETCH update for seq {seq}"));
                if let Some(uid) = self.handle_fetch_update(*seq, attrs).await? {
                    log_backend_event("BACKEND", &format!("queueing label refresh for uid {uid}"));
                    pending_labels.push(uid);
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn handle_exists(&self, session: &mut AsyncSession, remote_count: u32) -> Result<()> {
        if let Some(range) = self.collect_new_messages(session, remote_count).await? {
            let command = format!("FETCH {range} (UID X-GM-LABELS)");
            self.fetch_gmail_labels_command(session, &command).await?;
        }
        Ok(())
    }

    async fn collect_new_messages(
        &self,
        session: &mut AsyncSession,
        remote_count: u32,
    ) -> Result<Option<String>> {
        let start = {
            let mut state = self.state.lock().await;
            let expected = state.expected_exists();
            if remote_count <= expected {
                state.set_expected_exists(remote_count);
                return Ok(None);
            }
            let start = expected + 1;
            state.set_expected_exists(remote_count);
            start
        };

        if start > remote_count {
            return Ok(None);
        }

        let range = if start == remote_count {
            start.to_string()
        } else {
            format!("{start}:{remote_count}")
        };

        let stored_messages = self
            .fetch_message_range(session, start, remote_count)
            .await
            .with_context(|| format!("fetching new message range {range}"))?;

        if stored_messages.is_empty() {
            return Ok(None);
        }

        let mut to_emit = Vec::with_capacity(stored_messages.len());
        {
            let mut state = self.state.lock().await;
            for stored in stored_messages {
                let message = stored.message.clone();
                state.insert(stored);
                to_emit.push(message);
            }
        }

        for message in to_emit {
            self.emit_event(BackendEvent::NewMessage(message));
        }

        Ok(Some(range))
    }

    async fn handle_fetch_update(
        &self,
        _seq: u32,
        attrs: &[AttributeValue<'_>],
    ) -> Result<Option<u32>> {
        let mut flags: Option<Vec<String>> = None;
        let mut labels: Option<Vec<String>> = None;
        let mut uid = None;

        for attr in attrs {
            match attr {
                AttributeValue::Flags(list) => {
                    let snapshot = list.iter().map(|flag| flag.as_ref().to_string()).collect();
                    flags = Some(snapshot);
                }
                AttributeValue::Uid(value) => uid = Some(*value),
                AttributeValue::GmailLabels(values) => {
                    let snapshot = values
                        .iter()
                        .map(|value| value.as_ref().to_string())
                        .collect();
                    labels = Some(snapshot);
                }
                _ => {}
            }
        }

        let uid = match uid {
            Some(uid) => uid,
            None => return Ok(None),
        };

        let mut updated_message = None;
        if flags.is_some() || labels.is_some() {
            let mut state = self.state.lock().await;
            let mut changed = false;

            if let Some(flag_list) = flags {
                let (status, starred, answered, forwarded, important) =
                    summarize_flags_from_names(flag_list.iter().map(|s| s.as_str()));
                if state
                    .apply_flag_values(uid, status, starred, answered, forwarded, important)
                    .is_some()
                {
                    changed = true;
                }
            }

            if let Some(label_list) = labels
                && state.update_labels(uid, label_list).is_some()
            {
                changed = true;
            }

            if changed
                && let Some(id) = state.uid_to_id.get(&uid).copied()
                && let Some(stored) = state.messages.get(&id)
            {
                updated_message = Some(stored.message.clone());
            }
        }

        if let Some(message) = updated_message {
            self.emit_event(BackendEvent::MessageFlagsChanged(message));
        }

        Ok(Some(uid))
    }

    async fn handle_expunge(&self, seq: u32) {
        let removed = {
            let mut state = self.state.lock().await;
            state.expunge(seq)
        };

        if let Some(msg) = removed {
            log_backend_event(
                "BACKEND",
                &format!("emitting MessageDeleted for id {} (seq {seq})", msg.id),
            );
            self.emit_event(BackendEvent::MessageDeleted(msg.id));
        }
    }

    async fn apply_action_internal(&self, action: &Action) -> Result<()> {
        match action.action_type {
            ActionType::Archive => {
                let mailbox = {
                    let labels = self.labels.lock().await;
                    labels.archive.clone()
                };
                self.move_message(action.message_id, &mailbox).await
            }
            ActionType::Delete => {
                let mailbox = {
                    let labels = self.labels.lock().await;
                    labels.trash.clone()
                };
                self.move_message(action.message_id, &mailbox).await
            }
            ActionType::MoveToSpam => {
                let mailbox = {
                    let labels = self.labels.lock().await;
                    labels.spam.clone()
                };
                self.move_message(action.message_id, &mailbox).await
            }
            ActionType::MoveToInboxUnread => {
                self.update_flags(action.message_id, "-FLAGS.SILENT (\\Seen)")
                    .await
            }
            ActionType::MoveToInboxRead => {
                self.update_flags(action.message_id, "+FLAGS.SILENT (\\Seen)")
                    .await
            }
            ActionType::MarkAsRead => {
                self.update_flags(action.message_id, "+FLAGS.SILENT (\\Seen)")
                    .await
            }
            ActionType::MarkAsStarred => {
                self.update_flags(action.message_id, "+FLAGS.SILENT (\\Flagged)")
                    .await
            }
            ActionType::MarkAsUnstarred => {
                self.update_flags(action.message_id, "-FLAGS.SILENT (\\Flagged)")
                    .await
            }
            ActionType::MarkAsImportant => {
                self.update_gmail_labels(action.message_id, "+X-GM-LABELS (\\Important)")
                    .await
            }
            ActionType::MarkAsUnimportant => {
                self.update_gmail_labels(action.message_id, "-X-GM-LABELS (\\Important)")
                    .await
            }
        }
    }

    async fn move_message(&self, message_id: MessageId, target: &str) -> Result<()> {
        let (uid, seq) = {
            let state = self.state.lock().await;
            let stored = state
                .messages
                .get(&message_id)
                .ok_or_else(|| anyhow!("message {message_id} not found"))?;
            (stored.uid, stored.seq)
        };

        let uid_arg = uid.to_string();
        {
            let mut guard = self.session.lock().await;
            let session = guard
                .as_mut()
                .ok_or_else(|| anyhow!("IMAP session is not available"))?;
            session
                .uid_mv(&uid_arg, target)
                .await
                .with_context(|| format!("moving message {message_id} to {target}"))?;
        }

        {
            let mut state = self.state.lock().await;
            state.remove_by_seq(seq);
        }

        self.emit_event(BackendEvent::MessageDeleted(message_id));
        Ok(())
    }

    async fn update_flags(&self, message_id: MessageId, query: &str) -> Result<()> {
        let uid = {
            let state = self.state.lock().await;
            state
                .messages
                .get(&message_id)
                .map(|stored| stored.uid)
                .ok_or_else(|| anyhow!("message {message_id} not found"))?
        };

        let uid_arg = uid.to_string();
        let mut updated = None;

        {
            let mut guard = self.session.lock().await;
            let session = guard
                .as_mut()
                .ok_or_else(|| anyhow!("IMAP session is not available"))?;
            let mut stream = session.uid_store(&uid_arg, query).await?;

            while let Some(fetch) = stream.try_next().await? {
                let (status, starred, answered, forwarded, important) =
                    summarize_flags_from_flag_iter(fetch.flags());
                let mut state = self.state.lock().await;
                if let Some(message) = state.apply_flag_values(
                    fetch.uid.unwrap_or(uid),
                    status,
                    starred,
                    answered,
                    forwarded,
                    important,
                ) {
                    updated = Some(message);
                }
            }
        }

        if let Some(message) = updated {
            self.emit_event(BackendEvent::MessageFlagsChanged(message));
        }

        Ok(())
    }

    /// Update Gmail labels via the X-GM-LABELS extension.
    ///
    /// Unlike `update_flags` (which operates on standard IMAP FLAGS), this
    /// sends a raw `UID STORE` command with an X-GM-LABELS query and
    /// processes the label response through `handle_fetch_update`, which
    /// updates both `message.labels` and `message.important`.
    async fn update_gmail_labels(&self, message_id: MessageId, query: &str) -> Result<()> {
        let uid = {
            let state = self.state.lock().await;
            state
                .messages
                .get(&message_id)
                .map(|stored| stored.uid)
                .ok_or_else(|| anyhow!("message {message_id} not found"))?
        };

        let command = format!("UID STORE {uid} {query}");
        {
            let mut guard = self.session.lock().await;
            let session = guard
                .as_mut()
                .ok_or_else(|| anyhow!("IMAP session is not available"))?;
            self.fetch_gmail_labels_command(session, &command).await?;
        }

        Ok(())
    }

    async fn reinsert_session(&self, session: AsyncSession) {
        let mut guard = self.session.lock().await;
        *guard = Some(session);
    }

    fn emit_event(&self, event: BackendEvent) {
        if let Some(sender) = self.events.lock().unwrap().as_ref() {
            let _ = sender.send(event);
        }
    }
}

impl SharedState {
    fn set_expected_exists(&mut self, count: u32) {
        self.expected_exists = count;
    }

    fn expected_exists(&self) -> u32 {
        self.expected_exists
    }

    fn lowest_loaded_seq(&self) -> Option<u32> {
        self.seq_to_id.keys().next().copied()
    }

    fn next_backfill_range(&self, batch_size: usize) -> Option<(u32, u32)> {
        let lowest = self.lowest_loaded_seq()?;
        if lowest <= 1 {
            return None;
        }
        let end = lowest - 1;
        let chunk = batch_size as u32;
        let start = if end + 1 > chunk { end + 1 - chunk } else { 1 };
        Some((start, end))
    }

    fn expunge(&mut self, seq: u32) -> Option<Message> {
        if self.expected_exists > 0 {
            self.expected_exists -= 1;
        }
        self.remove_by_seq(seq)
    }

    fn insert(&mut self, mut stored: StoredMessage) {
        stored.message.seq = stored.seq;
        self.seq_to_id.insert(stored.seq, stored.message.id);
        self.uid_to_id.insert(stored.uid, stored.message.id);
        self.messages.insert(stored.message.id, stored);
    }

    fn remove_by_seq(&mut self, seq: u32) -> Option<Message> {
        let removed = self.seq_to_id.remove(&seq).and_then(|id| {
            let stored = self.messages.remove(&id)?;
            self.uid_to_id.remove(&stored.uid);
            Some(stored.message)
        });

        let updates: Vec<(u32, MessageId)> = self
            .seq_to_id
            .range((seq + 1)..)
            .map(|(old_seq, msg_id)| (*old_seq, *msg_id))
            .collect();

        for (old_seq, msg_id) in updates {
            if let Some(entry) = self.messages.get_mut(&msg_id) {
                self.seq_to_id.remove(&old_seq);
                let new_seq = old_seq.saturating_sub(1);
                self.seq_to_id.insert(new_seq, msg_id);
                entry.seq = new_seq;
                entry.message.seq = new_seq;
            }
        }

        removed
    }

    fn apply_flag_values(
        &mut self,
        uid: u32,
        status: MessageStatus,
        starred: bool,
        answered: bool,
        forwarded: bool,
        _important: bool,
    ) -> Option<Message> {
        let id = *self.uid_to_id.get(&uid)?;
        let stored = self.messages.get_mut(&id)?;
        let mut updated = stored.message.clone();

        // Note: `important` is intentionally NOT updated here.  Gmail does not
        // include `\Important` in standard IMAP FLAGS — it only appears in
        // X-GM-LABELS, which is handled by `update_labels`.
        let changed = status != updated.status
            || starred != updated.starred
            || answered != updated.answered
            || forwarded != updated.forwarded;

        updated.status = status;
        updated.starred = starred;
        updated.answered = answered;
        updated.forwarded = forwarded;

        if changed {
            stored.message = updated.clone();
            Some(updated)
        } else {
            None
        }
    }

    fn update_labels(&mut self, uid: u32, labels: Vec<String>) -> Option<Message> {
        let id = *self.uid_to_id.get(&uid)?;
        let stored = self.messages.get_mut(&id)?;
        // Gmail may send \Important as either a flag atom (`\Important`,
        // one backslash) or a quoted string (`"\\Important"`, which the IMAP
        // parser keeps as two backslashes since nom's `escaped` does not
        // unescape).  Match both representations.
        let important = labels.iter().any(|l| {
            l.eq_ignore_ascii_case("\\Important") || l.eq_ignore_ascii_case("\\\\Important")
        });
        if stored.message.labels == labels && stored.message.important == important {
            return None;
        }
        stored.message.labels = labels;
        stored.message.important = important;
        Some(stored.message.clone())
    }
}

fn build_message_from_fetch(fetch: &Fetch) -> Result<Option<StoredMessage>> {
    let uid = fetch
        .uid
        .ok_or_else(|| anyhow!("missing UID in fetch response"))?;
    let seq = fetch.message;

    let envelope = match fetch.envelope() {
        Some(env) => env,
        None => return Ok(None),
    };

    let sent = parse_envelope_date(envelope)
        .or_else(|| fetch.internal_date().and_then(convert_internal_date))
        .unwrap_or_else(OffsetDateTime::now_utc);

    let sender = extract_sender(envelope);
    let recipients = extract_recipients(envelope);
    let subject = decode_header(envelope.subject.as_ref(), "Subject");
    let size = fetch.size.unwrap_or_default() as usize;
    let flags: Vec<_> = fetch.flags().collect();

    let status = if flags.iter().any(|flag| matches!(flag, Flag::Seen)) {
        MessageStatus::Read
    } else {
        MessageStatus::New
    };
    let starred = flags.iter().any(|flag| matches!(flag, Flag::Flagged));
    let answered = flags.iter().any(|flag| matches!(flag, Flag::Answered));
    let forwarded = flags.iter().any(|flag| matches!(flag, Flag::Custom(name) if name.eq_ignore_ascii_case("\\Forwarded") || name.eq_ignore_ascii_case("$Forwarded")));
    let important = flags
        .iter()
        .any(|flag| matches!(flag, Flag::Custom(name) if name.eq_ignore_ascii_case("\\Important")));
    let has_attachments = fetch
        .bodystructure()
        .map(body_contains_attachment)
        .unwrap_or(false);

    let message = Message {
        id: uid as u64,
        sent,
        sender,
        recipients,
        subject,
        size,
        starred,
        important,
        answered,
        forwarded,
        status,
        labels: Vec::new(),
        uid,
        seq,
        has_attachments,
    };

    Ok(Some(StoredMessage { message, seq, uid }))
}

fn body_contains_attachment(structure: &BodyStructure<'_>) -> bool {
    match structure {
        BodyStructure::Multipart { bodies, .. } => bodies.iter().any(body_contains_attachment),
        BodyStructure::Message { common, body, .. } => {
            body_part_is_attachment(common) || body_contains_attachment(body)
        }
        BodyStructure::Text { common, .. } | BodyStructure::Basic { common, .. } => {
            body_part_is_attachment(common)
        }
    }
}

fn body_part_is_attachment(common: &BodyContentCommon<'_>) -> bool {
    if let Some(disposition) = &common.disposition {
        if disposition.ty.as_ref().eq_ignore_ascii_case("attachment") {
            return true;
        }
        if body_params_contains(&disposition.params, "filename") {
            return true;
        }
    }

    if body_params_contains(&common.ty.params, "name")
        && !common.ty.ty.as_ref().eq_ignore_ascii_case("text")
    {
        return true;
    }

    let ty = common.ty.ty.as_ref();
    if ty.eq_ignore_ascii_case("multipart") {
        return false;
    }
    if ty.eq_ignore_ascii_case("text") {
        return false;
    }

    true
}

fn body_params_contains(params: &BodyParams<'_>, name: &str) -> bool {
    params
        .as_ref()
        .and_then(|pairs| {
            pairs
                .iter()
                .find(|(key, _)| key.as_ref().eq_ignore_ascii_case(name))
        })
        .map(|(_, value)| !value.as_ref().is_empty())
        .unwrap_or(false)
}

fn build_message_content(mail: &ParsedMail<'_>) -> Result<MessageContent> {
    let mailer = mail.headers.get_first_value("X-Mailer").unwrap_or_default();

    let mut parts = Vec::new();
    let mut attachments = Vec::new();
    collect_parts(mail, 0, &mut parts, &mut attachments)?;

    Ok(MessageContent {
        mailer,
        parts,
        attachments,
    })
}

fn collect_parts(
    mail: &ParsedMail<'_>,
    depth: usize,
    parts: &mut Vec<MessageContentPart>,
    attachments: &mut Vec<MessageAttachment>,
) -> Result<()> {
    if depth >= MAX_PART_DEPTH {
        return Ok(());
    }

    if mail.subparts.is_empty() {
        let content_type = mail.ctype.mimetype.clone();
        let data = if mail.ctype.mimetype.starts_with("text/") {
            match mail.get_body() {
                Ok(text) => text.into_bytes(),
                Err(_) => mail
                    .get_body_raw()
                    .context("reading message body segment")?,
            }
        } else {
            mail.get_body_raw()
                .context("reading message body segment")?
        };
        let disposition = mail.get_content_disposition();
        let filename = disposition
            .params
            .get("filename")
            .cloned()
            .or_else(|| disposition.params.get("name").cloned())
            .or_else(|| mail.ctype.params.get("name").cloned())
            .filter(|name| !name.trim().is_empty());
        let is_text = mail
            .ctype
            .mimetype
            .split('/')
            .next()
            .map(|major| major.eq_ignore_ascii_case("text"))
            .unwrap_or(false);
        let is_attachment = matches!(disposition.disposition, DispositionType::Attachment)
            || filename.is_some()
            || !is_text;
        if is_attachment {
            attachments.push(MessageAttachment {
                filename,
                mime_type: content_type.clone(),
                size: data.len(),
            });
        }
        parts.push(MessageContentPart {
            content_type,
            content: data,
        });
    } else {
        for sub in &mail.subparts {
            collect_parts(sub, depth + 1, parts, attachments)?;
        }
    }

    Ok(())
}

fn parse_envelope_date(envelope: &imap_proto::types::Envelope<'_>) -> Option<OffsetDateTime> {
    envelope.date.as_ref().and_then(|raw| {
        let text = str::from_utf8(raw.as_ref()).ok()?;
        let ts = mailparse::dateparse(text).ok()?;
        OffsetDateTime::from_unix_timestamp(ts).ok()
    })
}

fn convert_internal_date(dt: chrono::DateTime<chrono::FixedOffset>) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(dt.timestamp()).ok()
}

fn decode_header(value: Option<&std::borrow::Cow<'_, [u8]>>, field_name: &str) -> String {
    let Some(raw) = value else {
        return String::new();
    };

    if raw.is_empty() {
        return String::new();
    }

    // Use mailparse's header parser so RFC 2047 encoded words are decoded
    // consistently with full message parsing.
    let mut header_bytes = Vec::with_capacity(field_name.len() + 4 + raw.len());
    header_bytes.extend_from_slice(field_name.as_bytes());
    header_bytes.extend_from_slice(b": ");
    header_bytes.extend_from_slice(raw.as_ref());
    header_bytes.extend_from_slice(b"\r\n");

    match mailparse::parse_header(&header_bytes) {
        Ok((parsed, _)) => parsed.get_value(),
        Err(_) => String::from_utf8_lossy(raw.as_ref()).into_owned(),
    }
}

fn extract_sender(envelope: &imap_proto::types::Envelope<'_>) -> String {
    envelope
        .from
        .as_ref()
        .and_then(|addresses| addresses.first())
        .and_then(|address| decode_envelope_address(address, "From"))
        .unwrap_or_else(|| "Unknown sender".to_string())
}

fn extract_recipients(envelope: &imap_proto::types::Envelope<'_>) -> Vec<String> {
    envelope
        .to
        .as_ref()
        .map(|addresses| {
            addresses
                .iter()
                .filter_map(|address| decode_envelope_address(address, "To"))
                .collect()
        })
        .unwrap_or_default()
}

fn decode_envelope_address(
    address: &imap_proto::types::Address<'_>,
    field_name: &str,
) -> Option<String> {
    if let Some(name) = &address.name {
        let decoded = decode_header(Some(name), field_name);
        if !decoded.is_empty() {
            return Some(decoded);
        }
    }

    let mailbox = address
        .mailbox
        .as_ref()
        .and_then(|m| str::from_utf8(m.as_ref()).ok());
    let host = address
        .host
        .as_ref()
        .and_then(|h| str::from_utf8(h.as_ref()).ok());

    match (mailbox, host) {
        (Some(mailbox), Some(host)) => Some(format!("{mailbox}@{host}")),
        (Some(mailbox), None) => Some(mailbox.to_string()),
        _ => None,
    }
}

fn summarize_flags_from_names<'a, I>(flags: I) -> (MessageStatus, bool, bool, bool, bool)
where
    I: IntoIterator<Item = &'a str>,
{
    let mut seen = false;
    let mut starred = false;
    let mut answered = false;
    let mut forwarded = false;
    let mut important = false;

    for flag in flags {
        match flag {
            "\\Seen" => seen = true,
            "\\Flagged" => starred = true,
            "\\Answered" => answered = true,
            "\\Important" => important = true,
            value
                if value.eq_ignore_ascii_case("\\Forwarded")
                    || value.eq_ignore_ascii_case("$Forwarded") =>
            {
                forwarded = true
            }
            _ => {}
        }
    }

    let status = if seen {
        MessageStatus::Read
    } else {
        MessageStatus::New
    };

    (status, starred, answered, forwarded, important)
}

fn summarize_flags_from_flag_iter<'a, I>(flags: I) -> (MessageStatus, bool, bool, bool, bool)
where
    I: IntoIterator<Item = Flag<'a>>,
{
    let mut seen = false;
    let mut starred = false;
    let mut answered = false;
    let mut forwarded = false;
    let mut important = false;

    for flag in flags {
        match flag {
            Flag::Seen => seen = true,
            Flag::Flagged => starred = true,
            Flag::Answered => answered = true,
            Flag::Custom(name) if name.eq_ignore_ascii_case("\\Important") => important = true,
            Flag::Custom(name)
                if name.eq_ignore_ascii_case("\\Forwarded")
                    || name.eq_ignore_ascii_case("$Forwarded") =>
            {
                forwarded = true
            }
            _ => {}
        }
    }

    let status = if seen {
        MessageStatus::Read
    } else {
        MessageStatus::New
    };

    (status, starred, answered, forwarded, important)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(id: u64, seq: u32) -> StoredMessage {
        let message = Message {
            id,
            sent: OffsetDateTime::UNIX_EPOCH,
            sender: format!("sender{id}"),
            recipients: Vec::new(),
            subject: format!("subject{id}"),
            size: 0,
            starred: false,
            important: false,
            answered: false,
            forwarded: false,
            status: MessageStatus::New,
            labels: Vec::new(),
            uid: id as u32,
            seq,
            has_attachments: false,
        };

        StoredMessage {
            message,
            seq,
            uid: id as u32,
        }
    }

    #[test]
    fn remove_by_seq_handles_repeated_sequence_numbers() {
        let mut state = SharedState::default();
        for seq in 1..=5 {
            let stored = make_message(seq as u64, seq);
            state.insert(stored);
        }

        assert_eq!(state.messages.len(), 5);

        let first = state.remove_by_seq(3).expect("first removal");
        assert_eq!(first.id, 3);
        assert_eq!(first.seq, 3);
        let second = state.remove_by_seq(3).expect("second removal");
        assert_eq!(second.id, 4);
        assert_eq!(second.seq, 3);
        let third = state.remove_by_seq(3).expect("third removal");
        assert_eq!(third.id, 5);
        assert_eq!(third.seq, 3);
        assert!(state.remove_by_seq(3).is_none());

        assert_eq!(state.messages.len(), 2);
        assert!(state.seq_to_id.contains_key(&1));
        assert!(state.seq_to_id.contains_key(&2));
    }
}
