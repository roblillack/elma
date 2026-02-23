//! JMAP backend built on the `jmap-client` crate.
//!
//! This implementation mirrors the asynchronous behaviour of the Gmail backend
//! while talking to FastMail (or any JMAP-compliant provider).  Mailboxes and
//! messages are synchronised over the JMAP HTTP API and background updates are
//! processed on an internal Tokio runtime so the UI thread remains responsive.

use crate::{
    backend::{ActionStatus, BackendEvent, MailBackend, MailboxSnapshot, OutgoingMessage},
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
use lettre::{
    Message as LettreEmail,
    message::{Mailbox as LettreMailbox, MultiPart, SinglePart},
};
use std::{
    cmp::Ordering,
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

/// Authentication parameters for connecting to a JMAP server.
#[derive(Clone, Debug)]
pub enum JmapAuth {
    Basic { username: String, password: String },
    Bearer { token: String },
}

impl JmapAuth {
    fn credentials(&self) -> Credentials {
        match self {
            JmapAuth::Basic { username, password } => Credentials::basic(username, password),
            JmapAuth::Bearer { token } => Credentials::bearer(token),
        }
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
    inner: Arc<JmapInner>,
}

impl JmapBackend {
    /// Create a new backend instance bound to `config`.
    pub fn new(config: JmapConfig) -> Result<Self> {
        let runtime =
            Arc::new(Runtime::new().context("failed to create Tokio runtime for JMAP backend")?);

        let inner = runtime.block_on(JmapInner::initialize(runtime.clone(), config))?;

        Ok(Self { inner })
    }
}

impl MailBackend for JmapBackend {
    fn load_mailbox(
        &self,
        mailbox: MailboxKind,
    ) -> Result<(MailboxSnapshot, mpsc::Receiver<BackendEvent>)> {
        let (sender, receiver) = mpsc::channel();

        {
            let mut guard = self.inner.events.lock().unwrap();
            *guard = Some(sender);
        }

        let snapshot = self.inner.runtime.block_on(async {
            self.inner.set_current_mailbox(mailbox).await;
            self.inner.stop_backfill().await;
            let sync = self
                .inner
                .sync_mailbox(mailbox)
                .await
                .context("loading mailbox contents")?;
            self.inner
                .start_event_loop()
                .await
                .context("starting JMAP event loop")?;
            self.inner
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
        self.inner
            .runtime
            .block_on(self.inner.load_message(message_id))
    }

    fn apply_actions(&self, actions: Vec<Action>) -> Result<mpsc::Receiver<ActionStatus>> {
        let (tx, rx) = mpsc::channel();
        let runtime = Arc::clone(&self.inner.runtime);
        let inner = Arc::clone(&self.inner);

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
                eprintln!("JMAP refresh error after actions: {err:?}");
            }
        });

        Ok(rx)
    }

    fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        let runtime = Arc::clone(&self.inner.runtime);
        let inner = Arc::clone(&self.inner);
        runtime.block_on(async move { inner.send_message(message).await })
    }

    fn save_draft(&self, message: OutgoingMessage) -> Result<()> {
        let runtime = Arc::clone(&self.inner.runtime);
        let inner = Arc::clone(&self.inner);
        runtime.block_on(async move { inner.save_draft(message).await })
    }
}

struct JmapInner {
    runtime: Arc<Runtime>,
    client: Arc<Client>,
    mailboxes: AsyncMutex<MailboxCache>,
    state: AsyncMutex<JmapState>,
    identity: IdentityInfo,
    events: Mutex<Option<mpsc::Sender<BackendEvent>>>,
    current_mailbox: AsyncMutex<MailboxKind>,
    event_handle: AsyncMutex<Option<JoinHandle<()>>>,
    event_cancel: AsyncMutex<Option<Arc<AtomicBool>>>,
    backfill_handle: AsyncMutex<Option<JoinHandle<()>>>,
    backfill_cancel: AsyncMutex<Option<Arc<AtomicBool>>>,
}

impl JmapInner {
    async fn initialize(runtime: Arc<Runtime>, config: JmapConfig) -> Result<Arc<Self>> {
        let JmapConfig {
            base_url,
            auth,
            trusted_hosts,
        } = config;

        let mut builder = Client::new().credentials(auth.credentials());
        if !trusted_hosts.is_empty() {
            builder = builder.follow_redirects(trusted_hosts);
        }

        let client = builder
            .connect(&base_url)
            .await
            .with_context(|| format!("connecting to JMAP server at {}", base_url))?;

        let client = Arc::new(client);

        let mailboxes = Self::load_mailboxes(&client).await?;
        let identity = Self::load_identity(&client).await?;

        Ok(Arc::new(Self {
            runtime,
            client,
            mailboxes: AsyncMutex::new(mailboxes),
            state: AsyncMutex::new(JmapState::default()),
            identity,
            events: Mutex::new(None),
            current_mailbox: AsyncMutex::new(MailboxKind::Inbox),
            event_handle: AsyncMutex::new(None),
            event_cancel: AsyncMutex::new(None),
            backfill_handle: AsyncMutex::new(None),
            backfill_cancel: AsyncMutex::new(None),
        }))
    }

    async fn set_current_mailbox(&self, mailbox: MailboxKind) {
        let mut guard = self.current_mailbox.lock().await;
        *guard = mailbox;
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
            let message = build_message(message_id, uid, index as u32 + 1, &data, &mailbox_cache)?;

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
        let mailbox = *self.current_mailbox.lock().await;
        self.stop_backfill().await;
        let sync = self.sync_mailbox(mailbox).await?;
        self.emit_diff(sync);
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

        let mut response = request
            .send_get_email()
            .await
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
            let created = self
                .client
                .email_import(raw.clone(), [drafts_id.clone()], Some(["$draft"]), None)
                .await
                .context("importing draft for submission")?;
            created
                .id()
                .map(|id| id.to_string())
                .ok_or_else(|| anyhow!("email import did not return an identifier"))?
        } else {
            let created = self
                .client
                .email_import(
                    raw.clone(),
                    std::iter::empty::<String>(),
                    None::<Vec<&str>>,
                    None,
                )
                .await
                .context("importing message for submission")?;
            created
                .id()
                .map(|id| id.to_string())
                .ok_or_else(|| anyhow!("email import did not return an identifier"))?
        };

        self.client
            .email_submission_create(draft_id.clone(), self.identity.id.clone())
            .await
            .context("creating JMAP email submission")?;

        if let Some(sent_id) = sent_id {
            let _ = self
                .client
                .email_set_mailboxes(&draft_id, [sent_id])
                .await
                .context("moving submitted message to Sent mailbox")?;
        }

        let _ = self
            .client
            .email_set_keyword(&draft_id, "$draft", false)
            .await;

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

        self.client
            .email_import(raw, [drafts_id], Some(["$draft"]), None)
            .await
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

    fn emit_diff(&self, sync: MailboxSync) {
        for message in sync.added {
            self.emit_event(BackendEvent::NewMessage(message));
        }
        for message in sync.updated {
            self.emit_event(BackendEvent::MessageFlagsChanged(message));
        }
        for id in sync.removed {
            self.emit_event(BackendEvent::MessageDeleted(id));
        }
    }

    fn emit_event(&self, event: BackendEvent) {
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

        self.client
            .email_set_mailboxes(&jmap_id, [target_id])
            .await
            .context("updating mailbox assignment")?;

        Ok(())
    }

    async fn set_keyword(&self, message_id: MessageId, keyword: &str, value: bool) -> Result<()> {
        let jmap_id = self.lookup_message(message_id).await?;
        self.client
            .email_set_keyword(&jmap_id, keyword, value)
            .await
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
                let mut next_seq = state.current_sequence.len() as u32;
                let mut entries = Vec::new();
                for data in emails {
                    let (message_id, uid, _) = state.ensure_ids(&data.jmap_id);
                    if state.current_sequence.contains(&message_id) {
                        continue;
                    }
                    next_seq += 1;
                    match build_message(message_id, uid, next_seq, &data, &mailbox_cache) {
                        Ok(message) => entries.push((
                            message_id,
                            StoredMessage {
                                message,
                                jmap_id: data.jmap_id,
                            },
                        )),
                        Err(err) => {
                            eprintln!("JMAP backfill build error: {err:?}");
                            next_seq -= 1;
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
                self.emit_event(BackendEvent::NewMessage(message));
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
        let mut query_response = request
            .send_query_email()
            .await
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
        let mut response = request
            .send_get_email()
            .await
            .context("fetching mailbox messages")?;

        let emails = response
            .take_list()
            .into_iter()
            .filter_map(FetchedEmail::from_email)
            .collect::<Vec<_>>();

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
        let mut response = request
            .send_get_mailbox()
            .await
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
        let mut response = request
            .send_get_identity()
            .await
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

        let multipart = MultiPart::alternative()
            .singlepart(SinglePart::plain(text_body))
            .singlepart(SinglePart::html(html_body));

        builder
            .multipart(multipart)
            .context("building MIME message")
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
        self.resequence();

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
        self.resequence();

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

    fn resequence(&mut self) {
        self.current_sequence.sort_by(|a, b| {
            let msg_a = self.messages.get(a).map(|stored| &stored.message);
            let msg_b = self.messages.get(b).map(|stored| &stored.message);
            match (msg_a, msg_b) {
                (Some(ma), Some(mb)) => match ma.sent.cmp(&mb.sent) {
                    Ordering::Equal => ma.id.cmp(&mb.id),
                    other => other,
                },
                (None, None) => Ordering::Equal,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
            }
        });

        for (idx, id) in self.current_sequence.iter().enumerate() {
            if let Some(stored) = self.messages.get_mut(id) {
                stored.message.seq = idx as u32 + 1;
            }
        }
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

    let mut labels = Vec::new();
    for mailbox_id in &data.mailbox_ids {
        if let Some(name) = cache.name_for_id(mailbox_id) {
            labels.push(name.to_string());
        }
    }
    if starred
        && !labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case("Starred"))
    {
        labels.push("Starred".to_string());
    }
    if important
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

    let attachments = email
        .attachments()
        .unwrap_or_default()
        .iter()
        .map(|part| MessageAttachment {
            filename: part.name().map(|name| name.to_string()),
            mime_type: part
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_string(),
            size: part.size(),
        })
        .collect();

    Ok(MessageContent {
        mailer,
        parts,
        attachments,
    })
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
