//! Gmail backend built on the IMAP protocol.
//!
//! The implementation shares the same public contract as the mock backend but runs
//! all network I/O on an internal Tokio runtime.  Actions are processed asynchronously
//! so the terminal UI never blocks while Gmail applies flag and mailbox updates.

use crate::{
    backend::{
        ActionStatus, BackendEvent, LeafPart, MailBackend, MailboxSnapshot, OutgoingMessage,
        PartRole, build_compose_body,
    },
    model::{
        Action, ActionType, MailboxKind, Message, MessageAttachment, MessageContent,
        MessageContentPart, MessageId, MessageStatus,
    },
};
use anyhow::{Context, Result, anyhow};
use async_imap::{
    Session,
    extensions::idle::{Handle as IdleHandle, IdleResponse},
    types::{Flag, UnsolicitedResponse},
};
use futures::TryStreamExt;
use imap_proto::types::{
    AttributeValue, BodyContentCommon, BodyContentSinglePart, BodyParams, BodyStructure,
    MailboxDatum, NameAttribute, Response, Status,
};
use lettre::{
    Message as LettreEmail, SmtpTransport, Transport, message::Mailbox as LettreMailbox,
    transport::smtp::authentication::Credentials,
};
use mailparse::{self, DispositionType, MailHeaderMap, ParsedMail};
use rustls_platform_verifier::ConfigVerifierExt;
use std::{
    collections::{BTreeMap, HashMap},
    str,
    sync::{
        Arc, Mutex, OnceLock,
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
use tokio_rustls::TlsConnector;

#[cfg(debug_assertions)]
mod debug_logging {
    use crate::backend::debug_log::{DebugLog, LogKind, REDACTED};
    use std::{
        borrow::Cow,
        io::{self, IoSlice},
        pin::Pin,
        sync::Arc,
        task::{Context, Poll},
    };

    use anyhow::Result;
    use tokio::io::ReadBuf;
    use tokio::net::TcpStream;
    use tokio_rustls::client::TlsStream;

    /// How many bytes one direction may buffer before an unterminated line is
    /// written out anyway.  Only a server that never sends a newline gets near
    /// this; the cap is here to keep the buffer bounded.
    const MAX_PENDING_BYTES: usize = 64 * 1024;

    fn logger() -> Result<Arc<DebugLog>> {
        DebugLog::global(LogKind::GmailImap)
    }

    /// Register the account password so it can never reach the trace, no matter
    /// which shape the protocol gives it.
    pub fn register_secret(secret: &str) {
        if let Ok(logger) = logger() {
            logger.register_secret(secret);
        }
    }

    pub fn log_backend_event(label: &str, payload: &str) {
        if let Ok(logger) = logger() {
            logger.log_event("conn#00", label, payload);
        }
    }

    fn log_line(
        logger: &DebugLog,
        connection_id: usize,
        direction: &str,
        line: &str,
        terminated: bool,
    ) {
        let scope = format!("conn#{connection_id:02}");
        if terminated {
            logger.log_event(&scope, direction, &format!("{line} <CRLF>"));
        } else {
            logger.log_event(&scope, direction, line);
        }
    }

    /// Reassembles the byte chunks of one direction into protocol lines.
    ///
    /// A single write is not a protocol line: async-imap sends a command as
    /// several writes -- the tag, a space, the command, the terminating CRLF --
    /// and incoming TLS records break wherever they like.  Redaction that
    /// inspects one chunk in isolation therefore never sees a complete `LOGIN`
    /// command, which is exactly how passwords used to reach the trace in
    /// clear text.  Buffering until the newline arrives hands the redactor
    /// whole lines, and makes the log easier to read as a bonus.
    #[derive(Debug, Default)]
    struct LineAssembler {
        pending: Vec<u8>,
    }

    impl LineAssembler {
        /// Feed `data`, invoking `sink` once per line that is now complete.
        fn push(&mut self, data: &[u8], mut sink: impl FnMut(&str, bool)) {
            self.pending.extend_from_slice(data);
            while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=index).collect();
                let text = String::from_utf8_lossy(&line);
                sink(text.trim_end_matches(['\r', '\n']), true);
            }
            if self.pending.len() >= MAX_PENDING_BYTES {
                self.flush(sink);
            }
        }

        /// Write out whatever is buffered, e.g. when the connection is closing.
        fn flush(&mut self, mut sink: impl FnMut(&str, bool)) {
            if self.pending.is_empty() {
                return;
            }
            let line = std::mem::take(&mut self.pending);
            sink(&String::from_utf8_lossy(&line), false);
        }
    }

    /// Tracks which parts of the IMAP conversation carry credentials.
    ///
    /// Mostly that is a per-line decision, but both ways of authenticating
    /// spill onto the lines that follow: a `LOGIN` argument may be sent as a
    /// literal, so its value arrives on the next line, and `AUTHENTICATE`
    /// starts a SASL exchange in which the client's responses are bare base64
    /// lines.
    #[derive(Debug, Default)]
    struct ImapRedactor {
        /// Client lines still to be swallowed whole because a `LOGIN` literal
        /// announced them.
        pending_literal_lines: usize,
        /// A SASL exchange is in progress: everything the client sends until
        /// the server's tagged response is credential material.
        sasl_active: bool,
    }

    impl ImapRedactor {
        /// Redact one line the client sent.
        fn client_line<'a>(&mut self, line: &'a str) -> Cow<'a, str> {
            if self.pending_literal_lines > 0 {
                self.pending_literal_lines -= 1;
                // The continuation may end in another literal: `LOGIN {5+}`
                // puts the user name on the next line, and the password can
                // follow it as a second literal.
                if ends_with_literal(line) {
                    self.pending_literal_lines += 1;
                }
                return Cow::Borrowed(REDACTED);
            }

            if self.sasl_active {
                // "*" aborts the exchange; anything else is a SASL response.
                return if line.trim() == "*" {
                    Cow::Borrowed(line)
                } else {
                    Cow::Borrowed(REDACTED)
                };
            }

            match split_command(line) {
                Some((command, args_start)) if command.eq_ignore_ascii_case("LOGIN") => {
                    Cow::Owned(self.redact_login(line, args_start))
                }
                Some((command, args_start)) if command.eq_ignore_ascii_case("AUTHENTICATE") => {
                    self.sasl_active = true;
                    Cow::Owned(redact_authenticate(line, args_start))
                }
                _ => Cow::Borrowed(line),
            }
        }

        /// Follow the server side; only the end of a SASL exchange matters.
        fn server_line(&mut self, line: &str) {
            // A continuation ("+ ...") keeps the exchange going, every other
            // response ends it.
            if self.sasl_active && !line.starts_with('+') {
                self.sasl_active = false;
            }
        }

        /// Redact the arguments of `<tag> LOGIN <user> <password>`.
        fn redact_login(&mut self, line: &str, args_start: usize) -> String {
            let args = &line[args_start..];
            // A literal moves the value onto the following line, so hide the
            // rest of this one and swallow that line whole.
            if ends_with_literal(args) {
                self.pending_literal_lines = 1;
                return format!("{}{REDACTED}", &line[..args_start]);
            }
            match skip_imap_token(args) {
                // Everything after the user name is the password.
                Some(password_start) => {
                    format!("{}\"{REDACTED}\"", &line[..args_start + password_start])
                }
                // Unparseable arguments: drop all of them rather than gamble on
                // where the password starts.
                None => format!("{}{REDACTED}", &line[..args_start]),
            }
        }
    }

    /// Split `<tag> <command> <arguments>` into the command and the offset its
    /// arguments start at.
    fn split_command(line: &str) -> Option<(&str, usize)> {
        let tag_end = line.find(' ')?;
        let rest = &line[tag_end + 1..];
        let command_end = rest.find(' ')?;
        Some((&rest[..command_end], tag_end + 1 + command_end + 1))
    }

    /// `<tag> AUTHENTICATE <mechanism> [<initial response>]`: keep the
    /// mechanism, drop the credentials that may ride along with it.
    fn redact_authenticate(line: &str, args_start: usize) -> String {
        let args = &line[args_start..];
        match args.find(' ') {
            Some(mechanism_end) => {
                format!("{}{REDACTED}", &line[..args_start + mechanism_end + 1])
            }
            None => line.to_owned(),
        }
    }

    /// Does the line end in a literal announcement (`{42}` or `{42+}`), meaning
    /// the value itself arrives on the next line?
    fn ends_with_literal(line: &str) -> bool {
        let trimmed = line.trim_end();
        let Some(inner) = trimmed.strip_suffix('}') else {
            return false;
        };
        let Some(brace) = inner.rfind('{') else {
            return false;
        };
        let digits = inner[brace + 1..].trim_end_matches('+');
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }

    /// Skip one IMAP token (quoted string or atom) and trailing whitespace.
    /// Returns the byte offset where the next token begins.
    fn skip_imap_token(s: &str) -> Option<usize> {
        let trimmed = s.trim_start();
        let leading = s.len() - trimmed.len();
        if let Some(inner) = trimmed.strip_prefix('"') {
            // Quoted string: find the closing quote (skipping escaped chars).
            let mut chars = inner.char_indices();
            loop {
                match chars.next() {
                    Some((_, '\\')) => {
                        chars.next();
                    }
                    Some((i, '"')) => {
                        let token_end = 1 + i + 1;
                        let trailing =
                            trimmed[token_end..].len() - trimmed[token_end..].trim_start().len();
                        return Some(leading + token_end + trailing);
                    }
                    Some(_) => {}
                    None => return None,
                }
            }
        } else {
            // Atom: run until whitespace.
            let end = trimmed.find(' ')?;
            let trailing = trimmed[end..].len() - trimmed[end..].trim_start().len();
            Some(leading + end + trailing)
        }
    }

    #[derive(Debug)]
    pub struct LoggedTlsStream {
        inner: TlsStream<TcpStream>,
        logger: Arc<DebugLog>,
        connection_id: usize,
        outgoing: LineAssembler,
        incoming: LineAssembler,
        redactor: ImapRedactor,
    }

    impl LoggedTlsStream {
        pub fn new(inner: TlsStream<TcpStream>) -> Result<Self> {
            let logger = logger()?;
            let connection_id = logger.allocate_connection_id();
            logger.log_event(
                &format!("conn#{connection_id:02}"),
                "INFO",
                "IMAP connection established",
            );

            Ok(Self {
                inner,
                logger,
                connection_id,
                outgoing: LineAssembler::default(),
                incoming: LineAssembler::default(),
                redactor: ImapRedactor::default(),
            })
        }

        fn log_outgoing(&mut self, data: &[u8]) {
            let Self {
                logger,
                connection_id,
                outgoing,
                redactor,
                ..
            } = self;
            outgoing.push(data, |line, terminated| {
                let line = redactor.client_line(line);
                log_line(logger, *connection_id, "C->S", &line, terminated);
            });
        }

        fn log_incoming(&mut self, data: &[u8]) {
            let Self {
                logger,
                connection_id,
                incoming,
                redactor,
                ..
            } = self;
            incoming.push(data, |line, terminated| {
                redactor.server_line(line);
                log_line(logger, *connection_id, "S->C", line, terminated);
            });
        }
    }

    impl Unpin for LoggedTlsStream {}

    impl Drop for LoggedTlsStream {
        fn drop(&mut self) {
            let Self {
                logger,
                connection_id,
                outgoing,
                incoming,
                redactor,
                ..
            } = self;
            outgoing.flush(|line, terminated| {
                let line = redactor.client_line(line);
                log_line(logger, *connection_id, "C->S", &line, terminated);
            });
            incoming.flush(|line, terminated| {
                log_line(logger, *connection_id, "S->C", line, terminated);
            });
            logger.log_event(
                &format!("conn#{connection_id:02}"),
                "INFO",
                "IMAP connection closed",
            );
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

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Push `writes` through the same assemble-then-redact pipeline
        /// `log_outgoing` uses, and collect what would reach the log.
        fn trace_client(writes: &[&str]) -> Vec<String> {
            let mut assembler = LineAssembler::default();
            let mut redactor = ImapRedactor::default();
            let mut logged = Vec::new();
            for write in writes {
                assembler.push(write.as_bytes(), |line, _| {
                    logged.push(redactor.client_line(line).into_owned());
                });
            }
            assembler.flush(|line, _| logged.push(redactor.client_line(line).into_owned()));
            logged
        }

        /// Split `data` into lines the way the logger does.
        fn assemble(chunks: &[&str]) -> Vec<(String, bool)> {
            let mut assembler = LineAssembler::default();
            let mut lines = Vec::new();
            for chunk in chunks {
                assembler.push(chunk.as_bytes(), |line, terminated| {
                    lines.push((line.to_owned(), terminated));
                });
            }
            assembler.flush(|line, terminated| lines.push((line.to_owned(), terminated)));
            lines
        }

        #[test]
        fn redact_login_quoted() {
            let logged = trace_client(&["A001 LOGIN \"user@example.com\" \"s3cret\"\r\n"]);
            assert_eq!(logged[0], r#"A001 LOGIN "user@example.com" "***""#);
        }

        #[test]
        fn redact_login_unquoted() {
            let logged = trace_client(&["A001 LOGIN user@example.com mypassword\r\n"]);
            assert_eq!(logged[0], r#"A001 LOGIN user@example.com "***""#);
            assert!(!logged[0].contains("mypassword"));
        }

        #[test]
        fn redact_login_case_insensitive() {
            let logged = trace_client(&["tag login \"USER\" \"PASS\"\r\n"]);
            assert_eq!(logged[0], r#"tag login "USER" "***""#);
        }

        #[test]
        fn redact_ignores_non_login() {
            assert_eq!(
                trace_client(&["A002 SELECT INBOX\r\n"])[0],
                "A002 SELECT INBOX"
            );
        }

        #[test]
        fn redact_login_with_escaped_quotes_in_username() {
            let logged = trace_client(&["A001 LOGIN \"user\\\"name\" \"pass\"\r\n"]);
            assert!(logged[0].contains(r#""***""#));
            assert!(!logged[0].contains(r#""pass""#));
        }

        #[test]
        fn redact_login_split_across_writes() {
            // This is how the password used to reach the log: async-imap writes
            // the tag, the separator, the command and the trailing CRLF
            // separately, and no single write looks like a LOGIN command.
            let logged = trace_client(&["A0001", " ", r#"LOGIN "user" "s3cret""#, " \r\n"]);

            assert_eq!(logged.len(), 1);
            assert_eq!(logged[0], r#"A0001 LOGIN "user" "***""#);
            assert!(!logged[0].contains("s3cret"));
        }

        #[test]
        fn redact_login_sent_as_literal() {
            // `LOGIN {4+}` announces the arguments on the next line.
            let logged = trace_client(&[
                "A001 LOGIN {4+}\r\n",
                "user \"s3cret\"\r\n",
                "A002 NOOP\r\n",
            ]);
            assert_eq!(logged, vec!["A001 LOGIN ***", "***", "A002 NOOP"]);
        }

        #[test]
        fn redact_login_with_password_as_second_literal() {
            let logged = trace_client(&[
                "A001 LOGIN {4+}\r\n",
                "user {6+}\r\n",
                "s3cret\r\n",
                "A002 NOOP\r\n",
            ]);
            assert!(logged[..3].iter().all(|line| !line.contains("s3cret")));
            assert_eq!(logged[3], "A002 NOOP");
        }

        #[test]
        fn redact_authenticate_initial_response() {
            let logged = trace_client(&["A001 AUTHENTICATE PLAIN AHVzZXIAcGFzc3dvcmQ=\r\n"]);
            assert_eq!(logged[0], "A001 AUTHENTICATE PLAIN ***");
        }

        #[test]
        fn redact_sasl_continuation_lines() {
            let mut redactor = ImapRedactor::default();
            assert_eq!(
                redactor.client_line("A001 AUTHENTICATE XOAUTH2"),
                "A001 AUTHENTICATE XOAUTH2"
            );
            // Everything the client sends now is credential material, until the
            // server answers with something other than a continuation.
            redactor.server_line("+");
            assert_eq!(redactor.client_line("dXNlcj1yb2JAZXhhbXBsZS5jb20B"), "***");
            redactor.server_line("A001 OK user authenticated");
            assert_eq!(
                redactor.client_line("A002 SELECT INBOX"),
                "A002 SELECT INBOX"
            );
        }

        #[test]
        fn assembler_keeps_lines_whole_and_marks_terminators() {
            let lines = assemble(&["* OK Gimap ready\r\n* CAPABILITY IMAP", "4rev1\r\npartial"]);
            assert_eq!(
                lines,
                vec![
                    ("* OK Gimap ready".to_owned(), true),
                    ("* CAPABILITY IMAP4rev1".to_owned(), true),
                    ("partial".to_owned(), false),
                ]
            );
        }
    }
}

#[cfg(not(debug_assertions))]
mod debug_logging {
    use tokio::net::TcpStream;
    use tokio_rustls::client::TlsStream;

    pub type LoggedTlsStream = TlsStream<TcpStream>;

    pub fn wrap_stream(stream: TlsStream<TcpStream>) -> anyhow::Result<LoggedTlsStream> {
        Ok(stream)
    }

    pub fn log_backend_event(_label: &str, _payload: &str) {}

    pub fn register_secret(_secret: &str) {}
}

use debug_logging::{LoggedTlsStream, log_backend_event, register_secret, wrap_stream};

type AsyncSession = Session<LoggedTlsStream>;
type AsyncIdleHandle = IdleHandle<LoggedTlsStream>;

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
/// Format of the IMAP `INTERNALDATE` attribute (RFC 3501 section 2.3.3).
const INTERNAL_DATE_FORMAT: &str = "%d-%b-%Y %H:%M:%S %z";
/// How many times [`GmailInner::drain_exists`] re-enters
/// [`GmailInner::handle_exists`] before leaving the rest to the IDLE loop.
const MAX_EXISTS_DRAIN_ROUNDS: usize = 4;
/// How many times a backfill run redials *without a batch succeeding in
/// between* before leaving the rest of the mailbox to the next attempt.
const MAX_BACKFILL_RECONNECTS: u32 = 3;
/// How long a backfill run may go without asking the server whether new mail
/// has arrived.  Bounds how late a message shows up while backfill has IDLE
/// switched off.
const NEW_MAIL_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// TLS settings shared by every connection to Gmail.
///
/// Certificates are checked against the operating system's trust store, so a CA
/// the machine has been told to trust — a corporate TLS-inspecting proxy, an
/// internal CA — works here, and one the machine has revoked stops working.
///
/// Built once. The trust store does not change between connections, and a
/// dropped IDLE session reconnects often enough that loading it each time is
/// pure waste. A failure is not cached: only a successful config is stored, so a
/// platform that was briefly unable to answer gets asked again.
fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

    if let Some(config) = CONFIG.get() {
        return Ok(Arc::clone(config));
    }

    let config = Arc::new(
        rustls::ClientConfig::with_platform_verifier()
            .context("loading the platform certificate store")?,
    );
    Ok(Arc::clone(CONFIG.get_or_init(|| config)))
}

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
    /// Set while a backfill run owns the IMAP session.
    ///
    /// Backfill holds IDLE down for its whole run rather than cycling it per
    /// batch, so anything that would ordinarily restart IDLE afterwards -- an
    /// action, say -- has to leave it alone until the run is done.  Without
    /// this the next batch would find the session taken by a fresh IDLE task.
    backfill_active: AtomicBool,
    /// Serialises action processing so that only one [`process_action`] call
    /// runs at a time.  Without this, concurrent batches (e.g. an immediate
    /// flag-change batch and a regular move batch) can race: one task's
    /// [`start_idle_loop`] may re-take the IMAP session before the other
    /// task's [`apply_action_internal`] can use it.
    action_lock: AsyncMutex<()>,
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
                backfill_active: AtomicBool::new(false),
                action_lock: AsyncMutex::new(()),
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
            // Backfill first: when it has work it claims the session, and
            // `start_idle_loop` then declines rather than starting an IDLE the
            // backfill would immediately have to stop.
            self.inner.start_backfill_if_needed().await?;
            self.inner.start_idle_loop().await?;

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

            let label_exists = self
                .inner
                .fetch_gmail_labels_command(session, &format!("UID FETCH {uid} (UID X-GM-LABELS)"))
                .await?;
            self.inner.drain_exists(session, label_exists).await?;

            drop(session_guard);
            self.inner.start_backfill_if_needed().await?;
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

        // Registered before the connection exists, so the debug log can scrub
        // the password out of everything it writes -- whichever way the
        // protocol, or a future version of async-imap, happens to frame it.
        register_secret(&self.password);

        let tcp = TcpStream::connect((GMAIL_HOST, GMAIL_PORT))
            .await
            .context("connecting to Gmail IMAP server")?;
        let connector = TlsConnector::from(tls_config()?);
        let server_name = rustls::pki_types::ServerName::try_from(GMAIL_HOST)
            .context("invalid TLS server name")?
            .to_owned();
        let tls_stream = connector
            .connect(server_name, tcp)
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

        // Re-select the current mailbox so the session is ready for UID
        // commands.  Without this, a reconnected session sits in the
        // "authenticated" state and Gmail rejects UID MOVE / UID STORE with
        // "BAD … not allowed now".
        let mailbox = { *self.current_mailbox.lock().await };
        self.select_mailbox(mailbox).await?;

        Ok(())
    }

    async fn stop_backfill_task(&self) {
        if let Some(cancel) = self.backfill_cancel.lock().await.take() {
            cancel.store(true, Ordering::SeqCst);
        }
        // Cleared here too, not just where the run ends: the task may never
        // have been spawned, and a stale flag would keep IDLE off for good.
        self.backfill_active.store(false, Ordering::SeqCst);
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

        // Set before spawning, so a `start_idle_loop` racing the new task
        // cannot slip a fresh IDLE in ahead of it.
        self.backfill_active.store(true, Ordering::SeqCst);

        let this = Arc::clone(self);
        let handle = self.runtime.spawn(async move {
            this.backfill_loop(cancel_flag).await;
        });

        let mut handle_guard = self.backfill_handle.lock().await;
        *handle_guard = Some(handle);

        Ok(())
    }

    /// Work backwards through the mailbox, a batch at a time.
    ///
    /// IDLE is stopped once for the whole run instead of around every batch.
    /// Cycling it per batch cost an IDLE/DONE round trip and roughly 340ms of
    /// dead time per 100 messages -- on a 16k mailbox that was about half of
    /// all commands sent, and a fair bet for what provoked Gmail's `* BYE
    /// System Error`.
    ///
    /// Staying out of IDLE means new mail has to be asked for: Gmail does not
    /// volunteer EXISTS during a FETCH.  See [`Self::poll_for_new_mail`].
    async fn backfill_loop(self: Arc<Self>, cancel: Arc<AtomicBool>) {
        if let Err(err) = self.pause_idle().await {
            eprintln!("Gmail backfill could not pause IDLE: {err:?}");
        }

        self.run_backfill_batches(&cancel).await;

        // Before restarting IDLE, or `start_idle_loop` will decline.
        self.backfill_active.store(false, Ordering::SeqCst);
        if let Err(err) = self.start_idle_loop().await {
            eprintln!("Gmail idle restart error: {err:?}");
        }
    }

    /// Ask the server whether anything has arrived.
    ///
    /// Gmail does not volunteer EXISTS during a FETCH.  Across a full backfill
    /// it stayed silent for 76 seconds and some 320 commands with a message
    /// already waiting, then reported it 264ms after the client entered IDLE
    /// -- entering IDLE is what flushes the announcement, not time spent
    /// listening.  So while backfill keeps IDLE off, new mail has to be asked
    /// for.  NOOP is what RFC 3501 offers (section 6.4.1): "a periodic poll
    /// for new messages or message status updates during a period of
    /// inactivity".
    async fn poll_for_new_mail(&self) -> Result<()> {
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP session is not available"))?;

        session
            .noop()
            .await
            .context("polling for new mail with NOOP")?;

        // `noop` answers through async-imap's own command path, which parks the
        // untagged EXISTS in the unsolicited-response channel rather than
        // handing it back; `drain_exists` reads it out of there.
        self.drain_exists(session, None).await
    }

    async fn run_backfill_batches(self: &Arc<Self>, cancel: &Arc<AtomicBool>) {
        // Counted consecutively and reset by every batch that lands, so the cap
        // catches a connection that will not stay up rather than a long run
        // that got unlucky a few times.
        let mut failed_reconnects = 0;
        let mut last_poll = tokio_time::Instant::now();

        loop {
            if cancel.load(Ordering::SeqCst) {
                return;
            }

            if last_poll.elapsed() >= NEW_MAIL_POLL_INTERVAL {
                last_poll = tokio_time::Instant::now();
                if let Err(err) = self.poll_for_new_mail().await {
                    // Left for the next batch to trip over and reconnect on.
                    eprintln!("Gmail backfill new-mail poll error: {err:?}");
                }
            }

            let range = {
                let state = self.state.lock().await;
                state.next_backfill_range(BACKFILL_BATCH_SIZE as usize)
            };

            let Some((start, end)) = range else {
                return;
            };

            let err = match self.load_backfill_batch(start, end, cancel).await {
                Ok(()) => {
                    failed_reconnects = 0;
                    continue;
                }
                Err(err) => err,
            };

            eprintln!("Gmail backfill error: {err:?}");

            // Gmail hangs up on long backfills often enough that giving up on
            // the first one would leave most of the mailbox unread.  Dial
            // again and retry the same range -- `ensure_connected` re-selects
            // the mailbox, so the sequence numbers still line up.
            if !is_connection_lost(&err) || failed_reconnects >= MAX_BACKFILL_RECONNECTS {
                return;
            }

            failed_reconnects += 1;
            eprintln!(
                "Gmail backfill: reconnecting ({failed_reconnects}/{MAX_BACKFILL_RECONNECTS})"
            );
            if let Err(err) = self.ensure_connected().await {
                eprintln!("Gmail backfill reconnect error: {err:?}");
                return;
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

        let mut session_guard = self.session.lock().await;
        let session = session_guard
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP session is not available"))?;

        // Catch up on anything announced while the session was elsewhere before
        // reaching further back.  Backfill runs continuously, so this is the
        // last line of defence for every path that still loses an EXISTS to
        // async-imap's unsolicited-response channel.
        if let Err(err) = self.drain_exists(session, None).await {
            eprintln!("Gmail backfill EXISTS handling error: {err:?}");
        }

        let (stored_messages, fetch_exists) = match self
            .fetch_message_range(session, start, end)
            .await
            .with_context(|| format!("fetching backfill range {start}:{end}"))
        {
            Ok(result) => result,
            Err(err) => {
                drop(session_guard);
                if is_connection_lost(&err) {
                    self.discard_session().await;
                }
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

        let mut label_exists = None;
        if to_emit_ids.is_empty() {
            cancel.store(true, Ordering::SeqCst);
        } else if !cancel.load(Ordering::SeqCst) {
            let command = if start == end {
                format!("FETCH {start} (UID X-GM-LABELS)")
            } else {
                format!("FETCH {start}:{end} (UID X-GM-LABELS)")
            };
            match self.fetch_gmail_labels_command(session, &command).await {
                Ok(exists) => label_exists = exists,
                Err(err) => {
                    eprintln!("Gmail backfill label fetch error: {err:?}");
                    // Labels are optional; a dead connection is not.  Carrying
                    // on here is what sent an IDLE into a closed socket.
                    if is_connection_lost(&err) {
                        drop(session_guard);
                        self.discard_session().await;
                        return Err(err);
                    }
                }
            }
        }

        // Process any EXISTS notifications observed during the FETCH commands.
        if let Err(err) = self
            .drain_exists(session, max_exists(fetch_exists, label_exists))
            .await
        {
            eprintln!("Gmail backfill EXISTS handling error: {err:?}");
            if is_connection_lost(&err) {
                drop(session_guard);
                self.discard_session().await;
                return Err(err);
            }
        }

        drop(session_guard);

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

        Ok(())
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

            let (stored_messages, fetch_exists) = self
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

            let mut label_exists = None;
            if !message_ids.is_empty() {
                let command = if start == end {
                    format!("FETCH {start} (UID X-GM-LABELS)")
                } else {
                    format!("FETCH {start}:{end} (UID X-GM-LABELS)")
                };
                label_exists = self.fetch_gmail_labels_command(session, &command).await?;
            }

            // Mail that landed while we were reading the initial slice sits
            // above `exists` and would otherwise stay invisible until the next
            // reconnect.
            self.drain_exists(session, max_exists(fetch_exists, label_exists))
                .await?;
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
    ) -> Result<(Vec<StoredMessage>, Option<u32>)> {
        if start == 0 || end == 0 || start > end {
            return Ok((Vec::new(), None));
        }

        let range = if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        };

        let command = format!("FETCH {range} {FETCH_MESSAGE_QUERY}");
        let tag = session
            .run_command(&command)
            .await
            .with_context(|| format!("issuing FETCH command for range {range}"))?;

        let mut collected = Vec::new();
        let mut highest_exists: Option<u32> = None;

        loop {
            let Some(response) = session
                .read_response()
                .await
                .context("reading FETCH response")?
            else {
                return Err(anyhow!("IMAP connection closed during FETCH"));
            };

            let parsed = response.parsed();
            match parsed {
                Response::Fetch(seq, attrs) => match build_message_from_attrs(*seq, attrs)? {
                    Some(stored) => collected.push(stored),
                    None => {
                        let _ = self.handle_fetch_update(*seq, attrs).await?;
                    }
                },
                Response::Expunge(seq) => {
                    self.handle_expunge(*seq).await;
                }
                Response::MailboxData(MailboxDatum::Exists(count)) => {
                    highest_exists =
                        Some(highest_exists.map_or(*count, |prev: u32| prev.max(*count)));
                }
                Response::Done { tag: done_tag, .. } if done_tag == &tag => break,
                Response::Data {
                    status: Status::Bye,
                    ..
                } => return Err(ConnectionLost.into()),
                _ => {}
            }
        }

        Ok((collected, highest_exists))
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
            if has_important_attribute(attrs) {
                important = Some(entry_name.clone());
            } else if important.is_none() {
                // Fallback for servers that omit the attribute: match the
                // English name.  Only correct for English-locale accounts, so
                // the attribute above always wins.
                let lower = entry_name.to_ascii_lowercase();
                if lower == "important" || lower.ends_with("/important") {
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
    ) -> Result<Option<u32>> {
        let command_text = command.to_string();
        let tag = session
            .run_command(&command_text)
            .await
            .with_context(|| format!("issuing command `{command_text}`"))?;

        let mut highest_exists: Option<u32> = None;

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
                Response::MailboxData(MailboxDatum::Exists(count)) => {
                    highest_exists =
                        Some(highest_exists.map_or(*count, |prev: u32| prev.max(*count)));
                }
                Response::Data {
                    status: Status::Bye,
                    ..
                } => return Err(ConnectionLost.into()),
                _ => {}
            }
        }

        Ok(highest_exists)
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
            attachments,
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

        let body = build_compose_body(text_body, html_body, attachments)?;

        builder
            .multipart(body)
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
        // A backfill run owns the session until it finishes and restarts IDLE
        // itself.  Starting one here would take the session out from under the
        // next batch.
        if self.backfill_active.load(Ordering::SeqCst) {
            return Ok(());
        }

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
    ///
    /// Locks `idle_handle` first, then `idle_stop` — matching the order used by
    /// [`start_idle_loop`] — so that a concurrent start cannot insert a new
    /// (stop_tx, handle) pair between our two reads.
    async fn pause_idle(&self) -> Result<()> {
        let mut handle_guard = self.idle_handle.lock().await;
        let stop = {
            let mut guard = self.idle_stop.lock().await;
            guard.take()
        };
        if let Some(stop_tx) = stop {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = handle_guard.take() {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Pause the IDLE worker, apply `action`, then restart listening for updates.
    async fn process_action(self: Arc<Self>, action: Action) -> Result<()> {
        let _guard = self.action_lock.lock().await;
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

                    // A notification may already have arrived: the server can
                    // send EXISTS between `+ idling` and the stop signal, and
                    // during backfill `pause_idle` fires on every batch, so the
                    // two race constantly.  `select!` picks a ready branch at
                    // random, so roughly half of those races land here.
                    //
                    // Whatever is pending has to be collected *now*.
                    // `Handle::done` reads through to the tagged OK and hands
                    // everything it passes to async-imap's unsolicited-response
                    // channel, which nothing drains -- so an EXISTS left for
                    // `done` to find is gone, and the mail it announced stays
                    // invisible until the next reconnect.
                    let mut exists = Vec::new();
                    let mut label_refresh = Vec::new();
                    self.collect_pending_idle(&mut idle_handle, &mut exists, &mut label_refresh)
                        .await;

                    if let Ok(mut sess) = idle_handle.done().await {
                        self.apply_idle_updates(&mut sess, exists, label_refresh).await;
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
                        // NOOP is how the server reports anything it has been
                        // holding back, but async-imap answers it through
                        // `run_command_and_check_ok`, which parks untagged
                        // responses out of reach.  Pick them up.
                        if let Err(err) = self.drain_exists(&mut sess, None).await {
                            eprintln!("Gmail idle EXISTS handling error: {err:?}");
                        }
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

                    self.collect_pending_idle(&mut idle_handle, &mut exists, &mut label_refresh)
                        .await;

                    if exists.is_empty() && label_refresh.is_empty() {
                        continue;
                    }

                    match idle_handle.done().await {
                        Ok(mut sess) => {
                            self.apply_idle_updates(&mut sess, exists, label_refresh)
                                .await;
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

    /// Pick up IDLE notifications that have already arrived, without waiting
    /// for new ones.
    async fn collect_pending_idle(
        &self,
        idle_handle: &mut AsyncIdleHandle,
        exists: &mut Vec<u32>,
        label_refresh: &mut Vec<u32>,
    ) {
        loop {
            let (next_wait, next_stopper) = idle_handle.wait_with_timeout(Duration::from_millis(0));
            let outcome = next_wait.await;
            drop(next_stopper);

            match outcome {
                Ok(IdleResponse::NewData(resp)) => {
                    if let Err(err) = self
                        .process_idle_response(resp.parsed(), exists, label_refresh)
                        .await
                    {
                        eprintln!("Gmail idle processing error: {err:?}");
                    }
                }
                Ok(IdleResponse::Timeout) | Ok(IdleResponse::ManualInterrupt) => break,
                Err(err) => {
                    eprintln!("Gmail idle additional wait error: {err:?}");
                    break;
                }
            }
        }
    }

    /// Act on the notifications gathered while IDLE was running, now that the
    /// session is usable again.
    async fn apply_idle_updates(
        &self,
        session: &mut AsyncSession,
        exists: Vec<u32>,
        mut label_refresh: Vec<u32>,
    ) {
        for count in exists {
            if let Err(err) = self.drain_exists(session, Some(count)).await {
                eprintln!("Gmail idle EXISTS handling error: {err:?}");
            }
        }

        if label_refresh.is_empty() {
            return;
        }

        label_refresh.sort_unstable();
        label_refresh.dedup();
        let uid_set = label_refresh
            .iter()
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let command = format!("UID FETCH {uid_set} (UID X-GM-LABELS)");
        match self.fetch_gmail_labels_command(session, &command).await {
            Ok(label_exists) => {
                if let Err(err) = self.drain_exists(session, label_exists).await {
                    eprintln!("Gmail idle EXISTS handling error: {err:?}");
                }
            }
            Err(err) => eprintln!("Gmail label refresh error: {err:?}"),
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

    /// Fetch the messages announced by `initial`, then keep going for as long
    /// as doing so turns up further announcements.
    ///
    /// Every FETCH issued along the way can itself carry an untagged EXISTS, so
    /// one pass is not enough on a busy mailbox.  The round cap stops a server
    /// that announces another message on every pass from pinning us here; what
    /// is left over is picked up by the next IDLE cycle.
    async fn drain_exists(&self, session: &mut AsyncSession, initial: Option<u32>) -> Result<()> {
        let mut pending = max_exists(initial, take_unsolicited_exists(session));
        for _ in 0..MAX_EXISTS_DRAIN_ROUNDS {
            let Some(count) = pending else {
                break;
            };
            pending = self.handle_exists(session, count).await?;
        }
        Ok(())
    }

    /// Returns the highest EXISTS observed while fetching, if any.
    async fn handle_exists(
        &self,
        session: &mut AsyncSession,
        remote_count: u32,
    ) -> Result<Option<u32>> {
        let (range, fetch_exists) = self.collect_new_messages(session, remote_count).await?;
        let Some(range) = range else {
            return Ok(fetch_exists);
        };

        let command = format!("FETCH {range} (UID X-GM-LABELS)");
        let label_exists = self.fetch_gmail_labels_command(session, &command).await?;
        Ok(max_exists(fetch_exists, label_exists))
    }

    /// Returns the range that was fetched, plus the highest EXISTS seen while
    /// fetching it.
    async fn collect_new_messages(
        &self,
        session: &mut AsyncSession,
        remote_count: u32,
    ) -> Result<(Option<String>, Option<u32>)> {
        let start = {
            let mut state = self.state.lock().await;
            let expected = state.expected_exists();
            if remote_count <= expected {
                state.set_expected_exists(remote_count);
                return Ok((None, None));
            }
            let start = expected + 1;
            state.set_expected_exists(remote_count);
            start
        };

        if start > remote_count {
            return Ok((None, None));
        }

        let range = if start == remote_count {
            start.to_string()
        } else {
            format!("{start}:{remote_count}")
        };

        let (stored_messages, fetch_exists) = self
            .fetch_message_range(session, start, remote_count)
            .await
            .with_context(|| format!("fetching new message range {range}"))?;

        if stored_messages.is_empty() {
            return Ok((None, fetch_exists));
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

        Ok((Some(range), fetch_exists))
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
            state.expunge(seq);
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
            let label_exists = self.fetch_gmail_labels_command(session, &command).await?;
            self.drain_exists(session, label_exists).await?;
        }

        Ok(())
    }

    /// Retire a session the server has hung up on.
    ///
    /// Leaving it in place is worse than having none: `ensure_connected` only
    /// dials when the slot is empty, so the next task to pick the session up
    /// sends its command into a closed socket, fails, and only reconnects
    /// after that -- five seconds and two confusing errors later.
    async fn discard_session(&self) {
        let mut guard = self.session.lock().await;
        *guard = None;
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

/// Does this LIST entry carry Gmail's `\Important` special-use attribute?
///
/// `\Important` (RFC 8457) is not one of the RFC 6154 attributes the IMAP
/// parser knows, so it arrives as `NameAttribute::Extension("\\Important")`.
/// Detecting it is the only locale-independent way to find the mailbox: Gmail
/// localises the name itself (`[Gmail]/Important`, `[Gmail]/Wichtig`, …).
fn has_important_attribute(attrs: &[NameAttribute<'_>]) -> bool {
    attrs.iter().any(|attr| match attr {
        NameAttribute::Extension(name) => {
            let trimmed = name.trim_start_matches('\\');
            trimmed.eq_ignore_ascii_case("Important")
        }
        _ => false,
    })
}

fn build_message_from_attrs(
    seq: u32,
    attrs: &[AttributeValue<'_>],
) -> Result<Option<StoredMessage>> {
    let mut uid = None;
    let mut envelope = None;
    let mut flags_raw: Option<&Vec<std::borrow::Cow<'_, str>>> = None;
    let mut internal_date_str: Option<&str> = None;
    let mut size = 0u32;
    let mut body_structure: Option<&BodyStructure<'_>> = None;

    for attr in attrs {
        match attr {
            AttributeValue::Uid(u) => uid = Some(*u),
            AttributeValue::Envelope(env) => envelope = Some(env.as_ref()),
            AttributeValue::Flags(f) => flags_raw = Some(f),
            AttributeValue::InternalDate(d) => internal_date_str = Some(d.as_ref()),
            AttributeValue::Rfc822Size(s) => size = *s,
            AttributeValue::BodyStructure(bs) => body_structure = Some(bs),
            _ => {}
        }
    }

    // An unsolicited FETCH announcing a flag change carries neither ENVELOPE
    // nor UID (`* 5 FETCH (FLAGS (\Seen))`).  Report it as "not a message" so
    // the caller can route it to `handle_fetch_update` instead of failing the
    // whole command.
    let (Some(uid), Some(envelope)) = (uid, envelope) else {
        return Ok(None);
    };

    let sent = parse_envelope_date(envelope)
        .or_else(|| internal_date_str.and_then(parse_internal_date))
        .unwrap_or_else(OffsetDateTime::now_utc);

    let sender = extract_sender(envelope);
    let recipients = extract_recipients(envelope);
    let subject = decode_header(envelope.subject.as_ref(), "Subject");

    let (status, starred, answered, forwarded, important) = match flags_raw {
        Some(flags) => summarize_flags_from_names(flags.iter().map(|s| s.as_ref())),
        None => (MessageStatus::New, false, false, false, false),
    };

    let has_attachments = body_structure
        .map(body_contains_attachment)
        .unwrap_or(false);

    let message = Message {
        id: uid as u64,
        sent,
        sender,
        recipients,
        subject,
        size: size as usize,
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

/// Whether the message described by this BODYSTRUCTURE carries an attachment.
///
/// This is what the message list marker is built from, long before the body
/// itself is fetched; [`collect_parts`] has to reach the same verdict from the
/// parsed message, which is why both go through [`LeafPart::is_attachment`].
fn body_contains_attachment(structure: &BodyStructure<'_>) -> bool {
    match structure {
        BodyStructure::Multipart { bodies, .. } => bodies.iter().any(body_contains_attachment),
        BodyStructure::Message {
            common,
            other,
            body,
            ..
        } => body_part_is_attachment(common, other) || body_contains_attachment(body),
        BodyStructure::Text { common, other, .. } | BodyStructure::Basic { common, other, .. } => {
            body_part_is_attachment(common, other)
        }
    }
}

fn body_part_is_attachment(
    common: &BodyContentCommon<'_>,
    other: &BodyContentSinglePart<'_>,
) -> bool {
    let has_filename = common.disposition.as_ref().is_some_and(|disposition| {
        body_params_contains(&disposition.params, "filename")
            || body_params_contains(&disposition.params, "name")
    }) || body_params_contains(&common.ty.params, "name");

    LeafPart {
        major_type: common.ty.ty.as_ref(),
        has_filename,
        disposition: common
            .disposition
            .as_ref()
            .map(|disposition| disposition.ty.as_ref()),
        // `other.id` is the part's Content-ID.
        has_content_id: other.id.as_ref().is_some_and(|id| !id.trim().is_empty()),
    }
    .is_attachment()
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
        let disposition_name = match disposition.disposition {
            DispositionType::Attachment => Some("attachment"),
            DispositionType::Inline => Some("inline"),
            _ => None,
        };
        let content_id = mail.headers.get_first_value("Content-ID");
        let role = LeafPart {
            major_type: mail.ctype.mimetype.split('/').next().unwrap_or_default(),
            has_filename: filename.is_some(),
            disposition: disposition_name,
            has_content_id: content_id.is_some_and(|id| !id.trim().is_empty()),
        }
        .role();
        // Inline parts are listed too: they earn no marker in the message list,
        // but an embedded photo is still a file the reader may want to keep.
        if role != PartRole::Body {
            attachments.push(MessageAttachment {
                filename,
                mime_type: content_type.clone(),
                size: data.len(),
                data: Some(data.clone()),
                blob_id: None,
                inline: role == PartRole::Inline,
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

/// The server hung up on us -- it sent `* BYE`, or the socket closed.
///
/// Carried as its own error type so callers can tell "this session is dead"
/// apart from "this command failed", and retire the session rather than hand
/// the corpse to the next task.
#[derive(Debug)]
struct ConnectionLost;

impl std::fmt::Display for ConnectionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the IMAP server closed the connection")
    }
}

impl std::error::Error for ConnectionLost {}

/// Does this error mean the session is gone rather than the command failed?
fn is_connection_lost(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.is::<ConnectionLost>() || cause.downcast_ref::<std::io::Error>().is_some()
    })
}

/// Take any EXISTS async-imap parked in its unsolicited-response channel.
///
/// Several async-imap entry points -- `Handle::done`, `noop`, the `fetch` and
/// `list` streams -- do not surface untagged responses they were not asked
/// for.  They push them into `Session::unsolicited_responses` instead, a
/// bounded channel nothing in this backend reads, so an EXISTS that lands
/// there is dropped and the mail it announces stays invisible until the next
/// reconnect.  This is the backstop for the paths that still go through those
/// entry points.
///
/// Only EXISTS is replayed.  An EXPUNGE arriving this way has lost its
/// position relative to the FETCH responses it came in with, and applying it
/// out of order would renumber the wrong messages -- worse than missing it.
fn take_unsolicited_exists(session: &AsyncSession) -> Option<u32> {
    let mut highest = None;
    while let Ok(response) = session.unsolicited_responses.try_recv() {
        if let UnsolicitedResponse::Exists(count) = response {
            highest = max_exists(highest, Some(count));
        }
    }
    highest
}

/// The higher of two observed EXISTS counts, if either is present.
fn max_exists(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Parse an IMAP `INTERNALDATE` such as `25-Feb-2026 06:52:06 +0100`.
///
/// This is *not* an RFC 2822 date — the day and month are joined by dashes and
/// there is no weekday — so `mailparse::dateparse`, which handles the envelope
/// `Date:` header in [`parse_envelope_date`], rejects it outright.  Use the
/// same format string async-imap applies in `Fetch::internal_date`.
fn parse_internal_date(raw: &str) -> Option<OffsetDateTime> {
    let parsed = chrono::DateTime::parse_from_str(raw, INTERNAL_DATE_FORMAT).ok()?;
    OffsetDateTime::from_unix_timestamp(parsed.timestamp()).ok()
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
    use imap_proto::types::{ContentDisposition, ContentEncoding, ContentType};
    use std::borrow::Cow;

    /// Nothing else in the suite opens a TLS connection, so a dependency that
    /// quietly switches on a second rustls crypto provider would otherwise get
    /// past CI and panic on the first handshake instead.
    ///
    /// The `Err` is ignored on purpose. A machine with no CA certificates
    /// installed says nothing about this code, and the bug being guarded against
    /// is a panic rather than a returned error.
    #[test]
    fn the_tls_settings_name_exactly_one_crypto_provider() {
        let _ = tls_config();
    }

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

    fn make_message_with_uid(id: u64, seq: u32, uid: u32) -> StoredMessage {
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
            uid,
            seq,
            has_attachments: false,
        };

        StoredMessage { message, seq, uid }
    }

    // ---------------------------------------------------------------
    //  Attachment detection
    // ---------------------------------------------------------------

    fn body_params(pairs: &[(&str, &str)]) -> BodyParams<'static> {
        if pairs.is_empty() {
            return None;
        }
        Some(
            pairs
                .iter()
                .map(|(key, value)| (Cow::from(key.to_string()), Cow::from(value.to_string())))
                .collect(),
        )
    }

    /// A leaf part as the server describes it in a BODYSTRUCTURE.
    fn body_part(
        mime_type: &str,
        ctype_params: &[(&str, &str)],
        disposition: Option<(&str, &[(&str, &str)])>,
        content_id: Option<&str>,
    ) -> BodyStructure<'static> {
        let (ty, subtype) = mime_type.split_once('/').expect("major/minor type");
        let common = BodyContentCommon {
            ty: ContentType {
                ty: Cow::from(ty.to_string()),
                subtype: Cow::from(subtype.to_string()),
                params: body_params(ctype_params),
            },
            disposition: disposition.map(|(ty, params)| ContentDisposition {
                ty: Cow::from(ty.to_string()),
                params: body_params(params),
            }),
            language: None,
            location: None,
        };
        let other = BodyContentSinglePart {
            id: content_id.map(|id| Cow::from(id.to_string())),
            md5: None,
            description: None,
            transfer_encoding: ContentEncoding::SevenBit,
            octets: 42,
        };

        if ty.eq_ignore_ascii_case("text") {
            BodyStructure::Text {
                common,
                other,
                lines: 3,
                extension: None,
            }
        } else {
            BodyStructure::Basic {
                common,
                other,
                extension: None,
            }
        }
    }

    fn multipart(subtype: &str, bodies: Vec<BodyStructure<'static>>) -> BodyStructure<'static> {
        BodyStructure::Multipart {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: Cow::from("multipart".to_string()),
                    subtype: Cow::from(subtype.to_string()),
                    params: None,
                },
                disposition: None,
                language: None,
                location: None,
            },
            bodies,
            extension: None,
        }
    }

    #[test]
    fn the_list_marker_follows_the_body_structure() {
        // A plain message, and the usual text+html pair: nothing to offer.
        assert!(!body_contains_attachment(&body_part(
            "text/plain",
            &[],
            None,
            None
        )));
        assert!(!body_contains_attachment(&multipart(
            "alternative",
            vec![
                body_part("text/plain", &[], None, None),
                body_part("text/html", &[], None, None),
            ]
        )));

        // An attached PDF lights up the marker.
        assert!(body_contains_attachment(&multipart(
            "mixed",
            vec![
                body_part("text/plain", &[], None, None),
                body_part(
                    "application/pdf",
                    &[],
                    Some(("attachment", &[("filename", "invoice.pdf")])),
                    None
                ),
            ]
        )));

        // An image with no Content-ID cannot be shown in the body, so it is
        // offered for download however the sender labelled it.
        assert!(body_contains_attachment(&multipart(
            "related",
            vec![
                body_part("text/html", &[], None, None),
                body_part(
                    "image/png",
                    &[("name", "logo.png")],
                    Some(("inline", &[])),
                    None
                ),
            ]
        )));
    }

    #[test]
    fn a_logo_the_html_body_references_is_savable_but_earns_no_marker() {
        // The signature logo of an HTML mail: `multipart/related` with a
        // Content-ID the body points at as `cid:…`.  Marking it as an
        // attachment would put an `@` on half the newsletters in the mailbox.
        let structure = multipart(
            "related",
            vec![
                body_part("text/html", &[], None, None),
                body_part(
                    "image/png",
                    &[("name", "logo.png")],
                    Some(("inline", &[("filename", "logo.png")])),
                    Some("<logo@example.com>"),
                ),
            ],
        );
        assert!(!body_contains_attachment(&structure));

        let raw = concat!(
            "Content-Type: multipart/related; boundary=\"b\"\r\n",
            "\r\n",
            "--b\r\n",
            "Content-Type: text/html\r\n",
            "\r\n",
            "<p>hi <img src=\"cid:logo@example.com\"></p>\r\n",
            "--b\r\n",
            "Content-Type: image/png; name=\"logo.png\"\r\n",
            "Content-Disposition: inline; filename=\"logo.png\"\r\n",
            "Content-ID: <logo@example.com>\r\n",
            "\r\n",
            "PNG\r\n",
            "--b--\r\n",
        );
        let parsed = mailparse::parse_mail(raw.as_bytes()).expect("parsing the fixture");
        let content = build_message_content(&parsed).expect("building content");

        // Listed, so the save dialog can offer it -- but flagged, so nothing
        // reads it as the message carrying an attachment.
        assert_eq!(content.attachments.len(), 1, "{:?}", content.attachments);
        let logo = &content.attachments[0];
        assert_eq!(logo.filename.as_deref(), Some("logo.png"));
        assert!(
            logo.inline,
            "the opened message has to agree with the list about the marker"
        );
        assert_eq!(
            logo.data.as_deref(),
            Some(&b"PNG"[..]),
            "the bytes have to come with it, or there is nothing to save"
        );
        assert!(content.part("image/png").is_some());
    }

    #[test]
    fn a_named_text_part_is_an_attachment_before_and_after_opening() {
        // Older mailers attach a CSV as `text/csv; name=...` with no
        // Content-Disposition at all.  The list used to read that as "no
        // attachment" while the parsed message listed one, so the marker
        // appeared out of nowhere the moment the message was opened.
        assert!(body_contains_attachment(&multipart(
            "mixed",
            vec![
                body_part("text/plain", &[], None, None),
                body_part("text/csv", &[("name", "report.csv")], None, None),
            ]
        )));

        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=\"b\"\r\n",
            "\r\n",
            "--b\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "hello\r\n",
            "--b\r\n",
            "Content-Type: text/csv; name=\"report.csv\"\r\n",
            "\r\n",
            "a,b\r\n",
            "--b--\r\n",
        );
        let parsed = mailparse::parse_mail(raw.as_bytes()).expect("parsing the fixture");
        let content = build_message_content(&parsed).expect("building content");

        assert_eq!(content.attachments.len(), 1);
        assert_eq!(
            content.attachments[0].filename.as_deref(),
            Some("report.csv")
        );
    }

    fn populate_state(count: u32) -> SharedState {
        let mut state = SharedState::default();
        for seq in 1..=count {
            state.insert(make_message(seq as u64, seq));
        }
        state.set_expected_exists(count);
        state
    }

    // ---------------------------------------------------------------
    //  remove_by_seq
    // ---------------------------------------------------------------

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

    #[test]
    fn remove_lowest_seq_shifts_everything_down() {
        let mut state = populate_state(4);

        state.remove_by_seq(1).expect("remove seq 1");
        assert_eq!(state.messages.len(), 3);

        // Original seqs 2,3,4 should now be 1,2,3.
        for (expected_seq, expected_id) in [(1, 2), (2, 3), (3, 4)] {
            let stored = state.messages.get(&expected_id).unwrap();
            assert_eq!(stored.seq, expected_seq);
            assert_eq!(*state.seq_to_id.get(&expected_seq).unwrap(), expected_id);
        }
    }

    #[test]
    fn remove_highest_seq_leaves_others_unchanged() {
        let mut state = populate_state(4);

        state.remove_by_seq(4).expect("remove seq 4");
        assert_eq!(state.messages.len(), 3);

        for seq in 1..=3 {
            let stored = state.messages.get(&(seq as u64)).unwrap();
            assert_eq!(stored.seq, seq);
        }
    }

    #[test]
    fn remove_nonexistent_seq_returns_none() {
        let mut state = populate_state(3);
        assert!(state.remove_by_seq(99).is_none());
        assert_eq!(state.messages.len(), 3);
    }

    // ---------------------------------------------------------------
    //  Batch archive: simulate sequential move_message calls
    // ---------------------------------------------------------------

    /// Simulate the state mutations a batch of `move_message` calls performs:
    /// each one reads (uid, seq) from state, then calls remove_by_seq.
    /// After every removal the remaining seq numbers shift down.
    #[test]
    fn batch_archive_sequential_removes() {
        let mut state = populate_state(10);

        // Archive messages 3, 5, 7 (by id).  Their initial seqs are 3, 5, 7.
        // After removing seq 3, message 5 shifts to seq 4, message 7 to seq 6.
        // After removing seq 4 (was msg 5), message 7 shifts to seq 5.
        // After removing seq 5 (was msg 7), done.
        let ids_to_archive: Vec<u64> = vec![3, 5, 7];
        for id in &ids_to_archive {
            let seq = state.messages.get(id).expect("message exists").seq;
            state.remove_by_seq(seq).expect("removal succeeds");
        }

        assert_eq!(state.messages.len(), 7);
        for id in &ids_to_archive {
            assert!(!state.messages.contains_key(id));
        }

        // All remaining messages should have contiguous seqs 1..=7.
        let mut seqs: Vec<u32> = state.seq_to_id.keys().copied().collect();
        seqs.sort();
        assert_eq!(seqs, (1..=7).collect::<Vec<_>>());
    }

    /// A large batch archive that removes every other message, simulating a
    /// user selecting many messages and committing all at once.
    #[test]
    fn batch_archive_every_other_message() {
        let mut state = populate_state(20);
        let ids_to_archive: Vec<u64> = (1..=20).filter(|id| id % 2 == 0).collect();

        for id in &ids_to_archive {
            let seq = state.messages.get(id).expect("message exists").seq;
            state.remove_by_seq(seq).expect("removal succeeds");
        }

        assert_eq!(state.messages.len(), 10);
        let mut seqs: Vec<u32> = state.seq_to_id.keys().copied().collect();
        seqs.sort();
        assert_eq!(seqs, (1..=10).collect::<Vec<_>>());
    }

    // ---------------------------------------------------------------
    //  Flag updates interleaved with moves (the bug scenario)
    // ---------------------------------------------------------------

    /// Simulate the state-level effect of interleaved flag changes and
    /// moves: an immediate batch marks messages as read, while a regular
    /// batch archives other messages.  After both complete, only the
    /// flag-changed messages should remain.
    #[test]
    fn interleaved_flag_change_and_move() {
        let mut state = populate_state(5);

        // Immediate batch: mark message 2 as read (flag change, stays in state).
        let uid = state.messages.get(&2).unwrap().uid;
        state.apply_flag_values(uid, MessageStatus::Read, false, false, false, false);
        let msg2 = &state.messages.get(&2).unwrap().message;
        assert_eq!(msg2.status, MessageStatus::Read);

        // Regular batch: archive messages 1, 3, 4, 5.
        for id in [1, 3, 4, 5] {
            let seq = state.messages.get(&id).expect("exists").seq;
            state.remove_by_seq(seq).expect("removal succeeds");
        }

        // Only message 2 should remain, at seq 1.
        assert_eq!(state.messages.len(), 1);
        let stored = state.messages.get(&2).unwrap();
        assert_eq!(stored.seq, 1);
        assert_eq!(stored.message.status, MessageStatus::Read);
    }

    /// Flag change followed by move of the SAME message (e.g. mark-as-read
    /// then archive): the flag change is a no-op on state structure, then
    /// the move removes it.
    #[test]
    fn flag_then_move_same_message() {
        let mut state = populate_state(3);

        // Flag change on message 2.
        let uid = state.messages.get(&2).unwrap().uid;
        state.apply_flag_values(uid, MessageStatus::Read, true, false, false, false);
        assert!(state.messages.contains_key(&2));

        // Now move (archive) message 2.
        let seq = state.messages.get(&2).unwrap().seq;
        state.remove_by_seq(seq).expect("remove msg 2");

        assert_eq!(state.messages.len(), 2);
        assert!(!state.messages.contains_key(&2));

        // Messages 1 and 3 remain with contiguous seqs.
        let mut seqs: Vec<u32> = state.seq_to_id.keys().copied().collect();
        seqs.sort();
        assert_eq!(seqs, vec![1, 2]);
    }

    // ---------------------------------------------------------------
    //  Lookup after removal (the "message not found" error path)
    // ---------------------------------------------------------------

    #[test]
    fn lookup_removed_message_returns_none() {
        let mut state = populate_state(3);

        state.remove_by_seq(2).expect("remove msg 2");

        // By message id.
        assert!(!state.messages.contains_key(&2));
        // By uid.
        assert!(!state.uid_to_id.contains_key(&2));
    }

    // ---------------------------------------------------------------
    //  expunge
    // ---------------------------------------------------------------

    #[test]
    fn expunge_decrements_expected_exists() {
        let mut state = populate_state(5);
        assert_eq!(state.expected_exists(), 5);

        state.expunge(3);
        assert_eq!(state.expected_exists(), 4);
        assert_eq!(state.messages.len(), 4);

        state.expunge(1);
        assert_eq!(state.expected_exists(), 3);
        assert_eq!(state.messages.len(), 3);
    }

    #[test]
    fn expunge_unknown_seq_decrements_exists_but_returns_none() {
        let mut state = populate_state(5);
        let result = state.expunge(99);
        assert!(result.is_none());
        assert_eq!(state.expected_exists(), 4);
        assert_eq!(state.messages.len(), 5);
    }

    // ---------------------------------------------------------------
    //  insert
    // ---------------------------------------------------------------

    #[test]
    fn insert_updates_all_indices() {
        let mut state = SharedState::default();
        state.insert(make_message_with_uid(42, 10, 500));

        assert!(state.messages.contains_key(&42));
        assert_eq!(*state.seq_to_id.get(&10).unwrap(), 42);
        assert_eq!(*state.uid_to_id.get(&500).unwrap(), 42);
    }

    #[test]
    fn insert_overwrites_existing_message_id() {
        let mut state = SharedState::default();
        state.insert(make_message(1, 1));

        // Re-insert the same id with a different seq.
        let mut replacement = make_message(1, 5);
        replacement.message.subject = "updated".to_string();
        state.insert(replacement);

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages.get(&1).unwrap().seq, 5);
        assert_eq!(state.messages.get(&1).unwrap().message.subject, "updated");
    }

    // ---------------------------------------------------------------
    //  apply_flag_values
    // ---------------------------------------------------------------

    #[test]
    fn apply_flag_values_returns_none_when_unchanged() {
        let mut state = populate_state(1);
        // Message defaults: status=New, starred=false, answered=false, forwarded=false
        let result = state.apply_flag_values(1, MessageStatus::New, false, false, false, false);
        assert!(result.is_none());
    }

    #[test]
    fn apply_flag_values_updates_and_returns_message() {
        let mut state = populate_state(1);
        let result = state.apply_flag_values(1, MessageStatus::Read, true, false, false, false);
        let updated = result.expect("should return updated message");
        assert_eq!(updated.status, MessageStatus::Read);
        assert!(updated.starred);

        // State should reflect the change.
        let stored = &state.messages.get(&1).unwrap().message;
        assert_eq!(stored.status, MessageStatus::Read);
        assert!(stored.starred);
    }

    #[test]
    fn apply_flag_values_unknown_uid_returns_none() {
        let mut state = populate_state(1);
        let result = state.apply_flag_values(999, MessageStatus::Read, false, false, false, false);
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------
    //  update_labels
    // ---------------------------------------------------------------

    #[test]
    fn update_labels_sets_important_flag() {
        let mut state = populate_state(1);

        let result = state.update_labels(1, vec!["\\Important".to_string()]);
        let updated = result.expect("labels changed");
        assert!(updated.important);
        assert_eq!(updated.labels, vec!["\\Important"]);
    }

    #[test]
    fn update_labels_handles_escaped_important() {
        let mut state = populate_state(1);

        // Gmail sometimes sends \\Important (double backslash from IMAP parser).
        let result = state.update_labels(1, vec!["\\\\Important".to_string()]);
        let updated = result.expect("labels changed");
        assert!(updated.important);
    }

    // ---------------------------------------------------------------
    //  has_important_attribute
    // ---------------------------------------------------------------

    #[test]
    fn important_attribute_detected_regardless_of_locale() {
        // Both an English and a German account list the same `\Important`
        // attribute, only the mailbox name differs.
        let attrs = vec![
            NameAttribute::NoInferiors,
            NameAttribute::Extension("\\Important".into()),
        ];
        assert!(has_important_attribute(&attrs));
    }

    #[test]
    fn important_attribute_absent_on_other_mailboxes() {
        let attrs = vec![NameAttribute::NoInferiors, NameAttribute::Flagged];
        assert!(!has_important_attribute(&attrs));

        let attrs = vec![NameAttribute::Extension("\\Foobar".into())];
        assert!(!has_important_attribute(&attrs));
    }

    #[test]
    fn update_labels_returns_none_when_unchanged() {
        let mut state = populate_state(1);
        state.update_labels(1, vec!["Foo".to_string()]);

        let result = state.update_labels(1, vec!["Foo".to_string()]);
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------
    //  Seq renumbering consistency under various removal patterns
    // ---------------------------------------------------------------

    /// Verify the three index maps stay consistent after a complex
    /// sequence of inserts and removals.
    #[test]
    fn indices_stay_consistent_after_mixed_operations() {
        let mut state = populate_state(8);

        // Remove from the middle, then both ends.
        state.remove_by_seq(4);
        state.remove_by_seq(1);
        state.remove_by_seq(6); // shifted seq of original msg 8

        assert_eq!(state.messages.len(), 5);
        assert_eq!(state.seq_to_id.len(), 5);
        assert_eq!(state.uid_to_id.len(), 5);

        // Every seq_to_id entry must point to a message whose stored seq
        // matches the key, and whose uid is in uid_to_id.
        for (&seq, &id) in &state.seq_to_id {
            let stored = state.messages.get(&id).unwrap();
            assert_eq!(stored.seq, seq, "seq mismatch for message {id}");
            assert_eq!(
                *state.uid_to_id.get(&stored.uid).unwrap(),
                id,
                "uid_to_id mismatch for message {id}"
            );
        }
    }

    /// Removing all messages one-by-one from the front should leave
    /// an empty, consistent state.
    #[test]
    fn remove_all_from_front() {
        let mut state = populate_state(5);
        for _ in 0..5 {
            state.remove_by_seq(1).expect("still messages left");
        }
        assert!(state.messages.is_empty());
        assert!(state.seq_to_id.is_empty());
        assert!(state.uid_to_id.is_empty());
    }

    /// Removing all messages one-by-one from the back should leave
    /// an empty, consistent state.
    #[test]
    fn remove_all_from_back() {
        let mut state = populate_state(5);
        for seq in (1..=5).rev() {
            state.remove_by_seq(seq).expect("still messages left");
        }
        assert!(state.messages.is_empty());
        assert!(state.seq_to_id.is_empty());
        assert!(state.uid_to_id.is_empty());
    }

    // ---------------------------------------------------------------
    //  Backfill range
    // ---------------------------------------------------------------

    #[test]
    fn next_backfill_range_returns_none_when_at_seq_1() {
        let mut state = SharedState::default();
        state.insert(make_message(1, 1));
        assert!(state.next_backfill_range(100).is_none());
    }

    #[test]
    fn next_backfill_range_returns_chunk_below_lowest() {
        let mut state = SharedState::default();
        state.insert(make_message(100, 50));
        let (start, end) = state.next_backfill_range(10).unwrap();
        assert_eq!(end, 49);
        assert_eq!(start, 40);
    }

    // ---------------------------------------------------------------
    //  EXISTS bookkeeping
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    //  Connection loss
    // ---------------------------------------------------------------

    #[test]
    fn a_bye_is_recognised_through_the_context_chain() {
        // The read loops return `ConnectionLost` bare, but every caller adds
        // context on the way up, so the check has to walk the chain.
        let err = anyhow::Error::from(ConnectionLost)
            .context("reading Gmail label response")
            .context("fetching backfill range 100:200");

        assert!(is_connection_lost(&err));
    }

    #[test]
    fn a_socket_error_counts_as_connection_loss() {
        // What a mid-command hangup actually looks like: rustls reports the
        // missing close_notify as an io::Error.
        let io = std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify",
        );
        let err = anyhow::Error::from(io).context("reading FETCH response");

        assert!(is_connection_lost(&err));
    }

    #[test]
    fn an_ordinary_failure_is_not_connection_loss() {
        // A command the server rejected must not cost us the session.
        let err = anyhow!("missing UID in fetch response").context("fetching backfill range 1:100");

        assert!(!is_connection_lost(&err));
    }

    #[test]
    fn max_exists_keeps_the_higher_announcement() {
        assert_eq!(max_exists(None, None), None);
        assert_eq!(max_exists(Some(7), None), Some(7));
        assert_eq!(max_exists(None, Some(7)), Some(7));
        assert_eq!(max_exists(Some(4), Some(9)), Some(9));
        assert_eq!(max_exists(Some(9), Some(4)), Some(9));
    }

    // ---------------------------------------------------------------
    //  build_message_from_attrs
    // ---------------------------------------------------------------

    /// Parse one untagged FETCH line the way the session loop sees it.
    fn fetch_line(line: &[u8]) -> (u32, Vec<AttributeValue<'_>>) {
        let (rest, response) =
            imap_proto::parser::parse_response(line).expect("the fixture has to parse");
        assert!(rest.is_empty(), "the whole line has to be consumed");
        match response {
            Response::Fetch(seq, attrs) => (seq, attrs),
            other => panic!("expected a FETCH response, got {other:?}"),
        }
    }

    /// A full FETCH response to [`FETCH_MESSAGE_QUERY`].  `date` goes into the
    /// ENVELOPE's date field verbatim, so a test can pass `NIL` to drop it.
    fn message_fetch_line(date: &str) -> Vec<u8> {
        format!(
            concat!(
                r#"* 3 FETCH (FLAGS (\Seen) INTERNALDATE "25-Feb-2026 06:52:06 +0100" "#,
                "RFC822.SIZE 4242 ENVELOPE ({date} \"Hello there\" ",
                "((\"Ada\" NIL \"ada\" \"example.com\")) NIL NIL ",
                "((\"Bob\" NIL \"bob\" \"example.org\")) NIL NIL NIL ",
                "\"<id@example.com>\") UID 77 ",
                "BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 12 1))",
                "\r\n"
            ),
            date = date
        )
        .into_bytes()
    }

    #[test]
    fn a_full_fetch_response_becomes_a_message() {
        let line = message_fetch_line("\"Wed, 25 Feb 2026 07:00:00 +0000\"");
        let (seq, attrs) = fetch_line(&line);

        let stored = build_message_from_attrs(seq, &attrs)
            .expect("building the message")
            .expect("a full FETCH response is a message");

        assert_eq!(stored.uid, 77);
        assert_eq!(stored.seq, 3);
        assert_eq!(stored.message.id, 77);
        assert_eq!(stored.message.subject, "Hello there");
        assert_eq!(stored.message.sender, "Ada");
        assert_eq!(stored.message.recipients, vec!["Bob"]);
        assert_eq!(stored.message.size, 4242);
        assert_eq!(stored.message.status, MessageStatus::Read);
        assert!(!stored.message.starred);
        assert!(!stored.message.has_attachments);
        // The envelope's own Date header wins over INTERNALDATE.
        assert_eq!(stored.message.sent.unix_timestamp(), 1_772_002_800);
    }

    #[test]
    fn a_missing_envelope_date_falls_back_to_internaldate() {
        // INTERNALDATE is `25-Feb-2026 06:52:06 +0100`, which is not an RFC 2822
        // date: dashes join the day and month and there is no weekday.  Parsing
        // it with mailparse fails, which silently backdated every such message
        // to "now" instead.
        let line = message_fetch_line("NIL");
        let (seq, attrs) = fetch_line(&line);

        let stored = build_message_from_attrs(seq, &attrs)
            .expect("building the message")
            .expect("a full FETCH response is a message");

        assert_eq!(stored.message.sent.unix_timestamp(), 1_771_998_726);
    }

    #[test]
    fn parse_internal_date_reads_the_imap_format() {
        assert_eq!(
            parse_internal_date("25-Feb-2026 06:52:06 +0100").map(|d| d.unix_timestamp()),
            Some(1_771_998_726)
        );
        assert_eq!(parse_internal_date("Wed, 25 Feb 2026 06:52:06 +0100"), None);
        assert_eq!(parse_internal_date("nonsense"), None);
    }

    #[test]
    fn an_unsolicited_flag_update_is_not_a_message() {
        // Gmail may announce a flag change at any point during a FETCH.  Such a
        // response carries neither UID nor ENVELOPE, and treating the missing
        // UID as an error used to abort the whole backfill batch.
        let line = b"* 5 FETCH (FLAGS (\\Seen))\r\n";
        let (seq, attrs) = fetch_line(line);

        assert!(
            build_message_from_attrs(seq, &attrs)
                .expect("a flag update is not an error")
                .is_none()
        );
    }

    #[test]
    fn a_fetch_response_without_an_envelope_is_not_a_message() {
        let line = b"* 5 FETCH (UID 77 FLAGS (\\Seen))\r\n";
        let (seq, attrs) = fetch_line(line);

        assert!(
            build_message_from_attrs(seq, &attrs)
                .expect("a flag update is not an error")
                .is_none()
        );
    }

    #[test]
    fn an_attachment_in_the_bodystructure_reaches_the_message() {
        let line = concat!(
            r#"* 3 FETCH (FLAGS () INTERNALDATE "25-Feb-2026 06:52:06 +0100" "#,
            "RFC822.SIZE 4242 ENVELOPE (NIL \"Hello there\" ",
            "((\"Ada\" NIL \"ada\" \"example.com\")) NIL NIL ",
            "((\"Bob\" NIL \"bob\" \"example.org\")) NIL NIL NIL ",
            "\"<id@example.com>\") UID 77 BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" ",
            "(\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 12 1)(\"APPLICATION\" \"PDF\" ",
            "(\"NAME\" \"report.pdf\") NIL NIL \"BASE64\" 5000) \"MIXED\"))",
            "\r\n"
        )
        .as_bytes();
        let (seq, attrs) = fetch_line(line);

        let stored = build_message_from_attrs(seq, &attrs)
            .expect("building the message")
            .expect("a full FETCH response is a message");

        assert!(stored.message.has_attachments);
        assert_eq!(stored.message.status, MessageStatus::New);
    }
}
