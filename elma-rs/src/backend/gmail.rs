use crate::{
    backend::{BackendEvent, MailBackend},
    model::{
        Action, ActionType, Message, MessageContent, MessageContentPart, MessageId, MessageStatus,
    },
};
use anyhow::{Context, Result, anyhow};
use async_imap::{
    Session,
    extensions::idle::IdleResponse,
    types::{Fetch, Flag},
};
use async_native_tls::{TlsStream, connect as tls_connect};
use futures::TryStreamExt;
use imap_proto::types::{AttributeValue, MailboxDatum, NameAttribute, Response};
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

type AsyncSession = Session<TlsStream<TcpStream>>;

const GMAIL_HOST: &str = "imap.gmail.com";
const GMAIL_PORT: u16 = 993;
const DEFAULT_ARCHIVE_LABEL: &str = "[Gmail]/All Mail";
const DEFAULT_TRASH_LABEL: &str = "[Gmail]/Trash";
const MAX_PART_DEPTH: usize = 5;

pub struct GmailBackend {
    inner: Arc<GmailInner>,
}

struct GmailInner {
    email: String,
    password: String,
    runtime: Arc<Runtime>,
    session: AsyncMutex<Option<AsyncSession>>,
    state: AsyncMutex<SharedState>,
    labels: AsyncMutex<SpecialMailboxes>,
    events: Mutex<Option<mpsc::Sender<BackendEvent>>>,
    idle_stop: AsyncMutex<Option<oneshot::Sender<()>>>,
    idle_handle: AsyncMutex<Option<JoinHandle<()>>>,
}

#[derive(Default)]
struct SharedState {
    messages: HashMap<MessageId, StoredMessage>,
    seq_to_id: BTreeMap<u32, MessageId>,
    uid_to_id: HashMap<u32, MessageId>,
}

struct StoredMessage {
    message: Message,
    seq: u32,
    uid: u32,
}

#[derive(Clone)]
struct SpecialMailboxes {
    archive: String,
    trash: String,
}

impl Default for SpecialMailboxes {
    fn default() -> Self {
        Self {
            archive: DEFAULT_ARCHIVE_LABEL.to_string(),
            trash: DEFAULT_TRASH_LABEL.to_string(),
        }
    }
}

impl GmailBackend {
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
                events: Mutex::new(None),
                idle_stop: AsyncMutex::new(None),
                idle_handle: AsyncMutex::new(None),
            }),
        })
    }
}

impl MailBackend for GmailBackend {
    fn load_inbox(&self) -> Result<(Vec<Message>, mpsc::Receiver<BackendEvent>)> {
        let (sender, receiver) = mpsc::channel();

        {
            let mut guard = self.inner.events.lock().unwrap();
            *guard = Some(sender.clone());
        }

        let messages = self.inner.runtime.block_on(async {
            self.inner.ensure_connected().await?;
            self.inner.select_inbox().await?;

            let mut session_guard = self.inner.session.lock().await;
            let session = session_guard
                .as_mut()
                .ok_or_else(|| anyhow!("IMAP session is not available"))?;

            let query = "(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE UID)";
            let mut fetch_stream = session.fetch("1:*", query).await?;

            let mut messages = Vec::new();
            let mut new_state = SharedState::default();

            while let Some(fetch) = fetch_stream.try_next().await? {
                if let Some(stored) = build_message_from_fetch(&fetch)? {
                    let message = stored.message.clone();
                    new_state.insert(stored);
                    messages.push(message);
                }
            }

            messages.sort_by_key(|msg| msg.sent);

            {
                let mut state_guard = self.inner.state.lock().await;
                *state_guard = new_state;
            }

            self.inner.start_idle_loop().await?;

            Ok::<_, anyhow::Error>(messages)
        })?;

        Ok((messages, receiver))
    }

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

            let mut fetches = session
                .uid_fetch(uid.to_string(), "(RFC822)")
                .await
                .context("fetching full message")?;

            let mut content = None;
            while let Some(fetch) = fetches.try_next().await? {
                if let Some(body) = fetch.body() {
                    let parsed =
                        mailparse::parse_mail(body).context("parsing message MIME structure")?;
                    content = Some(build_message_content(&parsed)?);
                }
            }

            self.inner.start_idle_loop().await?;

            content.ok_or_else(|| anyhow!("message body not returned by server"))
        })
    }

    fn apply_action(&self, action: &Action) -> Result<()> {
        self.inner.runtime.block_on(async {
            self.inner.pause_idle().await?;
            self.inner
                .apply_action_internal(action)
                .await
                .with_context(|| format!("applying action {:?}", action.action_type))?;
            self.inner.start_idle_loop().await?;
            Ok(())
        })
    }
}

impl GmailInner {
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

    async fn select_inbox(&self) -> Result<()> {
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP session is not available"))?;
        session.select("INBOX").await.context("selecting INBOX")?;
        Ok(())
    }

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
        }

        {
            let mut labels = self.labels.lock().await;
            if let Some(value) = archive {
                labels.archive = value;
            }
            if let Some(value) = trash {
                labels.trash = value;
            }
        }

        Ok(())
    }

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
        let current = {
            let state = self.state.lock().await;
            state.len() as u32
        };

        if remote_count <= current {
            return Ok(());
        }

        let start = current + 1;
        let range = format!("{start}:{remote_count}");
        let query = "(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE UID)";
        let mut fetches = session.fetch(&range, query).await?;

        while let Some(fetch) = fetches.try_next().await? {
            if let Some(stored) = build_message_from_fetch(&fetch)? {
                let message = stored.message.clone();
                {
                    let mut state = self.state.lock().await;
                    state.insert(stored);
                }
                self.emit_event(BackendEvent::NewMessage(message));
            }
        }

        Ok(())
    }

    async fn handle_fetch_update(&self, _seq: u32, attrs: &[AttributeValue<'_>]) -> Result<()> {
        let mut flags: Option<Vec<String>> = None;
        let mut uid = None;

        for attr in attrs {
            match attr {
                AttributeValue::Flags(list) => {
                    let snapshot = list.iter().map(|flag| flag.as_ref().to_string()).collect();
                    flags = Some(snapshot);
                }
                AttributeValue::Uid(value) => uid = Some(*value),
                _ => {}
            }
        }

        let uid = match uid {
            Some(uid) => uid,
            None => return Ok(()),
        };

        if let Some(flag_list) = flags {
            let (status, starred, answered, forwarded) =
                summarize_flags_from_names(flag_list.iter().map(|s| s.as_str()));
            let mut state = self.state.lock().await;
            if let Some(message) =
                state.apply_flag_values(uid, status, starred, answered, forwarded)
            {
                self.emit_event(BackendEvent::MessageFlagsChanged(message));
            }
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
    let subject = decode_header(envelope.subject.as_ref());
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
        let data = mail
            .get_body_raw()
            .context("reading message body segment")?;
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

fn decode_header(value: Option<&std::borrow::Cow<'_, [u8]>>) -> String {
    value
        .map(|raw| String::from_utf8_lossy(raw.as_ref()).into_owned())
        .unwrap_or_default()
}

fn extract_sender(envelope: &imap_proto::types::Envelope<'_>) -> String {
    if let Some(addresses) = &envelope.from {
        if let Some(address) = addresses.first() {
            if let Some(name) = &address.name {
                if let Ok(text) = str::from_utf8(name.as_ref()) {
                    return text.to_string();
                }
                return String::from_utf8_lossy(name.as_ref()).into_owned();
            }

            let mailbox = address
                .mailbox
                .as_ref()
                .and_then(|m| str::from_utf8(m.as_ref()).ok());
            let host = address
                .host
                .as_ref()
                .and_then(|h| str::from_utf8(h.as_ref()).ok());
            if let (Some(mailbox), Some(host)) = (mailbox, host) {
                return format!("{mailbox}@{host}");
            }
        }
    }
    "Unknown sender".to_string()
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
