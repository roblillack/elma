//! Gmail backend built on the IMAP protocol.
//!
//! The implementation shares the same public contract as the mock backend but runs
//! all network I/O on an internal Tokio runtime.  Actions are processed asynchronously
//! so the terminal UI never blocks while Gmail applies flag and mailbox updates.

use crate::{
    backend::{ActionStatus, BackendEvent, MailBackend, OutgoingMessage},
    model::{
        Action, ActionType, MailboxKind, Message, MessageContent, MessageContentPart, MessageId,
        MessageStatus,
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
use imap_proto::types::{AttributeValue, MailboxDatum, NameAttribute, Response};
use lettre::{
    Message as LettreEmail, SmtpTransport, Transport, message::Mailbox as LettreMailbox,
    transport::smtp::authentication::Credentials,
};
use mailparse::{self, MailHeaderMap, ParsedMail};
use std::{
    collections::{BTreeMap, HashMap},
    str,
    sync::{Arc, Mutex, mpsc},
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
}

#[cfg(not(debug_assertions))]
mod debug_logging {
    use async_native_tls::TlsStream;
    use tokio::net::TcpStream;

    pub type LoggedTlsStream = TlsStream<TcpStream>;

    pub fn wrap_stream(stream: TlsStream<TcpStream>) -> anyhow::Result<LoggedTlsStream> {
        Ok(stream)
    }
}

use debug_logging::{LoggedTlsStream, wrap_stream};

type AsyncSession = Session<LoggedTlsStream>;

const GMAIL_HOST: &str = "imap.gmail.com";
const GMAIL_PORT: u16 = 993;
const DEFAULT_STARRED_LABEL: &str = "[Gmail]/Starred";
const DEFAULT_ARCHIVE_LABEL: &str = "[Gmail]/All Mail";
const DEFAULT_SPAM_LABEL: &str = "[Gmail]/Spam";
const DEFAULT_SENT_LABEL: &str = "[Gmail]/Sent Mail";
const DEFAULT_DRAFTS_LABEL: &str = "[Gmail]/Drafts";
const DEFAULT_TRASH_LABEL: &str = "[Gmail]/Trash";
const MAX_PART_DEPTH: usize = 5;

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
}

/// Cached view of the Gmail mailbox.
#[derive(Default)]
struct SharedState {
    messages: HashMap<MessageId, StoredMessage>,
    seq_to_id: BTreeMap<u32, MessageId>,
    uid_to_id: HashMap<u32, MessageId>,
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
            }),
        })
    }
}

impl MailBackend for GmailBackend {
    /// Fetch `mailbox` and subscribe to Gmail's IDLE notifications for it.
    fn load_mailbox(
        &self,
        mailbox: MailboxKind,
    ) -> Result<(Vec<Message>, mpsc::Receiver<BackendEvent>)> {
        let (sender, receiver) = mpsc::channel();

        {
            let mut guard = self.inner.events.lock().unwrap();
            *guard = Some(sender.clone());
        }

        let messages = self.inner.runtime.block_on(async {
            self.inner.ensure_connected().await?;
            self.inner.pause_idle().await?;
            self.inner.select_mailbox(mailbox).await?;
            let messages = self.inner.refresh_selected_mailbox().await?;
            {
                let mut current = self.inner.current_mailbox.lock().await;
                *current = mailbox;
            }
            self.inner.start_idle_loop().await?;

            Ok::<_, anyhow::Error>(messages)
        })?;

        Ok((messages, receiver))
    }

    /// Download the full MIME body for `message_id`.
    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        self.inner.runtime.block_on(async {
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
                    .uid_fetch(uid.to_string(), "(RFC822)")
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

    /// Resolve the IMAP mailbox name for `mailbox`.
    async fn mailbox_name(&self, mailbox: MailboxKind) -> Result<String> {
        let labels = self.labels.lock().await;
        let name = match mailbox {
            MailboxKind::Inbox => "INBOX".to_string(),
            MailboxKind::Starred => labels.starred.clone(),
            MailboxKind::Sent => labels.sent.clone(),
            MailboxKind::Drafts => labels.drafts.clone(),
            MailboxKind::Archive => labels.archive.clone(),
            MailboxKind::Spam => labels.spam.clone(),
            MailboxKind::Trash => labels.trash.clone(),
        };
        Ok(name)
    }

    /// Select the given mailbox so subsequent fetches operate on the right context.
    async fn select_mailbox(&self, mailbox: MailboxKind) -> Result<()> {
        let name = self.mailbox_name(mailbox).await?;
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP session is not available"))?;
        session
            .select(&name)
            .await
            .with_context(|| format!("selecting mailbox {name}"))?;
        Ok(())
    }

    /// Refresh the cached state for the currently selected mailbox.
    async fn refresh_selected_mailbox(&self) -> Result<Vec<Message>> {
        let query = "(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE UID)";
        let mut messages = Vec::new();
        let mut new_state = SharedState::default();

        {
            let mut session_guard = self.session.lock().await;
            let session = session_guard
                .as_mut()
                .ok_or_else(|| anyhow!("IMAP session is not available"))?;

            let mut fetch_stream = session.fetch("1:*", query).await?;
            while let Some(fetch) = fetch_stream.try_next().await? {
                if let Some(stored) = build_message_from_fetch(&fetch)? {
                    messages.push(stored.message.clone());
                    new_state.insert(stored);
                }
            }
        }

        messages.sort_by_key(|msg| msg.sent);

        {
            let mut state_guard = self.state.lock().await;
            *state_guard = new_state;
        }

        if !messages.is_empty() {
            let mut session_guard = self.session.lock().await;
            let session = session_guard
                .as_mut()
                .ok_or_else(|| anyhow!("IMAP session is not available"))?;
            self.fetch_gmail_labels_command(session, "FETCH 1:* (UID X-GM-LABELS)")
                .await?;
        }

        Ok(messages)
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
        let mut sent = None;
        let mut drafts = None;

        while let Some(name) = list_stream.try_next().await? {
            let attrs = name.attributes();
            if attrs.iter().any(|attr| matches!(attr, NameAttribute::All)) {
                archive = Some(name.name().to_string());
            }
            if attrs
                .iter()
                .any(|attr| matches!(attr, NameAttribute::Trash))
            {
                trash = Some(name.name().to_string());
            }
            if attrs.iter().any(|attr| matches!(attr, NameAttribute::Junk)) {
                spam = Some(name.name().to_string());
            }
            if attrs
                .iter()
                .any(|attr| matches!(attr, NameAttribute::Flagged))
            {
                starred = Some(name.name().to_string());
            }
            if attrs.iter().any(|attr| matches!(attr, NameAttribute::Sent)) {
                sent = Some(name.name().to_string());
            }
            if attrs
                .iter()
                .any(|attr| matches!(attr, NameAttribute::Drafts))
            {
                drafts = Some(name.name().to_string());
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
                    self.handle_fetch_update(*seq, attrs).await?;
                }
                Response::Done { tag: done_tag, .. } if done_tag == &tag => break,
                Response::Expunge(seq) => {
                    self.handle_expunge(*seq).await;
                }
                Response::MailboxData(MailboxDatum::Exists(count)) => {
                    let _ = self.collect_new_messages(session, *count).await?;
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
            content,
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

        builder
            .body(content)
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
        if let Err(err) = restart {
            return Err(err);
        }
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
                Ok(IdleResponse::NewData(resp)) => match idle_handle.done().await {
                    Ok(mut sess) => {
                        if let Err(err) =
                            self.handle_parsed_response(&mut sess, resp.parsed()).await
                        {
                            eprintln!("Gmail idle processing error: {err:?}");
                        }
                        self.reinsert_session(sess).await;
                    }
                    Err(err) => {
                        eprintln!("Gmail idle completion error: {err:?}");
                    }
                },
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

    async fn handle_parsed_response(
        &self,
        session: &mut AsyncSession,
        response: &Response<'_>,
    ) -> Result<()> {
        match response {
            Response::Expunge(seq) => {
                self.handle_expunge(*seq).await;
            }
            Response::MailboxData(MailboxDatum::Exists(count)) => {
                self.handle_exists(session, *count).await?;
            }
            Response::Fetch(seq, attrs) => {
                self.handle_fetch_update(*seq, attrs).await?;
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
        let current = {
            let state = self.state.lock().await;
            state.len() as u32
        };

        if remote_count <= current {
            return Ok(None);
        }

        let start = current + 1;
        let range = format!("{start}:{remote_count}");
        let query = "(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE UID)";
        let mut has_new_messages = false;
        {
            let mut fetches = session.fetch(&range, query).await?;

            while let Some(fetch) = fetches.try_next().await? {
                if let Some(stored) = build_message_from_fetch(&fetch)? {
                    let message = stored.message.clone();
                    {
                        let mut state = self.state.lock().await;
                        state.insert(stored);
                    }
                    self.emit_event(BackendEvent::NewMessage(message));
                    has_new_messages = true;
                }
            }
        }

        if has_new_messages {
            Ok(Some(range))
        } else {
            Ok(None)
        }
    }

    async fn handle_fetch_update(&self, _seq: u32, attrs: &[AttributeValue<'_>]) -> Result<()> {
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
            None => return Ok(()),
        };

        let mut updated_message = None;
        if flags.is_some() || labels.is_some() {
            let mut state = self.state.lock().await;
            let mut changed = false;

            if let Some(flag_list) = flags {
                let (status, starred, answered, forwarded) =
                    summarize_flags_from_names(flag_list.iter().map(|s| s.as_str()));
                if state
                    .apply_flag_values(uid, status, starred, answered, forwarded)
                    .is_some()
                {
                    changed = true;
                }
            }

            if let Some(label_list) = labels {
                if state.update_labels(uid, label_list).is_some() {
                    changed = true;
                }
            }

            if changed {
                if let Some(id) = state.uid_to_id.get(&uid).copied() {
                    if let Some(stored) = state.messages.get(&id) {
                        updated_message = Some(stored.message.clone());
                    }
                }
            }
        }

        if let Some(message) = updated_message {
            self.emit_event(BackendEvent::MessageFlagsChanged(message));
        }

        Ok(())
    }

    async fn handle_expunge(&self, seq: u32) {
        let removed = {
            let mut state = self.state.lock().await;
            state.remove_by_seq(seq)
        };

        if let Some(msg) = removed {
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
            ActionType::MarkAsStarred => {
                self.update_flags(action.message_id, "+FLAGS.SILENT (\\Flagged)")
                    .await
            }
            ActionType::MarkAsUnstarred => {
                self.update_flags(action.message_id, "-FLAGS.SILENT (\\Flagged)")
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
                let (status, starred, answered, forwarded) =
                    summarize_flags_from_flag_iter(fetch.flags());
                let mut state = self.state.lock().await;
                if let Some(message) = state.apply_flag_values(
                    fetch.uid.unwrap_or(uid),
                    status,
                    starred,
                    answered,
                    forwarded,
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
    fn len(&self) -> usize {
        self.messages.len()
    }

    fn insert(&mut self, stored: StoredMessage) {
        self.seq_to_id.insert(stored.seq, stored.message.id);
        self.uid_to_id.insert(stored.uid, stored.message.id);
        self.messages.insert(stored.message.id, stored);
    }

    fn remove_by_seq(&mut self, seq: u32) -> Option<Message> {
        let id = self.seq_to_id.remove(&seq)?;
        let stored = self.messages.remove(&id)?;
        self.uid_to_id.remove(&stored.uid);

        let updates: Vec<(u32, MessageId)> = self
            .seq_to_id
            .range((seq + 1)..)
            .map(|(old_seq, msg_id)| (*old_seq, *msg_id))
            .collect();

        for (old_seq, msg_id) in updates {
            if let Some(entry) = self.messages.get_mut(&msg_id) {
                self.seq_to_id.remove(&old_seq);
                self.seq_to_id.insert(old_seq - 1, msg_id);
                entry.seq -= 1;
            }
        }

        Some(stored.message)
    }

    fn apply_flag_values(
        &mut self,
        uid: u32,
        status: MessageStatus,
        starred: bool,
        answered: bool,
        forwarded: bool,
    ) -> Option<Message> {
        let id = *self.uid_to_id.get(&uid)?;
        let stored = self.messages.get_mut(&id)?;
        let mut updated = stored.message.clone();

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
        if stored.message.labels == labels {
            return None;
        }
        stored.message.labels = labels;
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

    let message = Message {
        id: uid as u64,
        sent,
        sender,
        recipients,
        subject,
        size,
        starred,
        answered,
        forwarded,
        status,
        labels: Vec::new(),
        uid,
    };

    Ok(Some(StoredMessage { message, seq, uid }))
}

fn build_message_content(mail: &ParsedMail<'_>) -> Result<MessageContent> {
    let mailer = mail.headers.get_first_value("X-Mailer").unwrap_or_default();

    let mut parts = Vec::new();
    collect_parts(mail, 0, &mut parts)?;

    Ok(MessageContent { mailer, parts })
}

fn collect_parts(
    mail: &ParsedMail<'_>,
    depth: usize,
    parts: &mut Vec<MessageContentPart>,
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
        parts.push(MessageContentPart {
            content_type,
            content: data,
        });
    } else {
        for sub in &mail.subparts {
            collect_parts(sub, depth + 1, parts)?;
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

fn summarize_flags_from_names<'a, I>(flags: I) -> (MessageStatus, bool, bool, bool)
where
    I: IntoIterator<Item = &'a str>,
{
    let mut seen = false;
    let mut starred = false;
    let mut answered = false;
    let mut forwarded = false;

    for flag in flags {
        match flag {
            "\\Seen" => seen = true,
            "\\Flagged" => starred = true,
            "\\Answered" => answered = true,
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

    (status, starred, answered, forwarded)
}

fn summarize_flags_from_flag_iter<'a, I>(flags: I) -> (MessageStatus, bool, bool, bool)
where
    I: IntoIterator<Item = Flag<'a>>,
{
    let mut seen = false;
    let mut starred = false;
    let mut answered = false;
    let mut forwarded = false;

    for flag in flags {
        match flag {
            Flag::Seen => seen = true,
            Flag::Flagged => starred = true,
            Flag::Answered => answered = true,
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

    (status, starred, answered, forwarded)
}
