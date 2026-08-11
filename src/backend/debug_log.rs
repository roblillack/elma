//! Shared plumbing for the protocol traces debug builds write.
//!
//! Both network backends keep a log of their conversation with the server in
//! the working directory -- `gmail-log-<stamp>-<pid>.log` for IMAP,
//! `jmap-log-<stamp>-<pid>.log` for JMAP -- using one common line format:
//!
//! ```text
//! 2026-08-11T08:06:36.456960Z conn#01 C->S: A0001 LOGIN "user@example.com" "***" <CRLF>
//! ```
//!
//! Those files end up attached to bug reports, so no credential may ever reach
//! them.  Redaction happens in two layers: each backend redacts the parts of
//! its own protocol that carry credentials, and on top of that every payload is
//! scrubbed of the secrets the backend registered through
//! [`DebugLog::register_secret`], which catches a credential regardless of how
//! the protocol framed it.

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use std::{
    borrow::Cow,
    fs::OpenOptions,
    io::{BufWriter, Write},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};

/// What redacted material is replaced with.
pub const REDACTED: &str = "***";

/// Secrets shorter than this are not searched for in payloads: a short string
/// shows up inside ordinary protocol data often enough that scrubbing it would
/// riddle the whole trace with `***`.  The per-protocol redaction in the
/// backends covers those; this layer is only the safety net underneath it.
const MIN_SCRUBBED_SECRET_LEN: usize = 6;

/// Which backend a trace belongs to.
#[derive(Clone, Copy, Debug)]
pub enum LogKind {
    GmailImap,
    Jmap,
}

impl LogKind {
    /// File name prefix for this backend's trace.
    fn slug(self) -> &'static str {
        match self {
            Self::GmailImap => "gmail",
            Self::Jmap => "jmap",
        }
    }

    /// How the backend is named in the log header.
    fn title(self) -> &'static str {
        match self {
            Self::GmailImap => "Gmail IMAP",
            Self::Jmap => "JMAP",
        }
    }

    /// The process-wide log for this backend.
    fn slot(self) -> &'static Mutex<Option<Arc<DebugLog>>> {
        static GMAIL: Mutex<Option<Arc<DebugLog>>> = Mutex::new(None);
        static JMAP: Mutex<Option<Arc<DebugLog>>> = Mutex::new(None);
        match self {
            Self::GmailImap => &GMAIL,
            Self::Jmap => &JMAP,
        }
    }
}

/// A protocol trace file plus the set of secrets that must stay out of it.
#[derive(Debug)]
pub struct DebugLog {
    file: Mutex<BufWriter<std::fs::File>>,
    next_id: AtomicUsize,
    secrets: RwLock<Vec<String>>,
}

impl DebugLog {
    fn init(kind: LogKind) -> Result<Self> {
        let now = Local::now();
        let stamp = now.format("%Y-%m-%d-%H%M").to_string();
        let pid = std::process::id();
        let filename = format!("{}-log-{stamp}-{pid}.log", kind.slug());
        let path = std::env::current_dir()
            .context("determining current directory for debug log file")?
            .join(filename);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening debug log file at {}", path.display()))?;

        let mut writer = BufWriter::new(file);
        let header_time = now.format("%Y-%m-%d %H:%M:%S");
        writeln!(
            writer,
            "# {} debug log started {header_time} local, pid {pid}",
            kind.title()
        )
        .ok();

        Ok(Self {
            file: Mutex::new(writer),
            next_id: AtomicUsize::new(1),
            secrets: RwLock::new(Vec::new()),
        })
    }

    /// The log for `kind`, created on first use.
    ///
    /// The lock is held across the file creation so two threads racing to be
    /// first end up with one log -- and one header line -- rather than two.
    pub fn global(kind: LogKind) -> Result<Arc<Self>> {
        let mut slot = kind.slot().lock().unwrap_or_else(|err| err.into_inner());
        if let Some(existing) = slot.as_ref() {
            return Ok(Arc::clone(existing));
        }

        let logger = Arc::new(Self::init(kind)?);
        *slot = Some(Arc::clone(&logger));
        Ok(logger)
    }

    /// Hand out the next connection (or request) number for this log.
    pub fn allocate_connection_id(&self) -> usize {
        self.next_id.fetch_add(1, Ordering::AcqRel)
    }

    /// Record a credential that must never show up in the trace.
    ///
    /// Backends call this before they connect, so the scrubbing is in place
    /// for the very first byte that gets logged.
    pub fn register_secret(&self, secret: &str) {
        if secret.len() < MIN_SCRUBBED_SECRET_LEN {
            return;
        }
        let mut secrets = self.secrets.write().unwrap_or_else(|err| err.into_inner());
        if !secrets.iter().any(|known| known == secret) {
            secrets.push(secret.to_owned());
        }
    }

    /// Replace every registered secret in `text` with [`REDACTED`].
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let secrets = self.secrets.read().unwrap_or_else(|err| err.into_inner());
        let mut result = Cow::Borrowed(text);
        for secret in secrets.iter() {
            if result.contains(secret.as_str()) {
                result = Cow::Owned(result.replace(secret.as_str(), REDACTED));
            }
        }
        result
    }

    /// Append one line to the trace.
    ///
    /// `scope` identifies the connection or request the line belongs to
    /// (`conn#01`, `req#004`), `label` its direction (`C->S`, `S->C`, `INFO`).
    pub fn log_event(&self, scope: &str, label: &str, payload: &str) {
        let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ");
        let payload = self.scrub(payload);
        if let Ok(mut writer) = self.file.lock() {
            let _ = writeln!(writer, "{timestamp} {scope} {label}: {payload}");
            let _ = writer.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> DebugLog {
        DebugLog {
            file: Mutex::new(BufWriter::new(tempfile::tempfile().unwrap())),
            next_id: AtomicUsize::new(1),
            secrets: RwLock::new(Vec::new()),
        }
    }

    #[test]
    fn scrubs_registered_secret_anywhere_in_the_payload() {
        let log = log();
        log.register_secret("hunter2hunter2");

        let scrubbed = log.scrub(r#"A001 LOGIN "user" "hunter2hunter2""#);

        assert_eq!(scrubbed, r#"A001 LOGIN "user" "***""#);
    }

    #[test]
    fn scrubs_every_occurrence_of_every_secret() {
        let log = log();
        log.register_secret("app-password");
        log.register_secret("bearer-token-value");

        let scrubbed = log.scrub("app-password bearer-token-value app-password");

        assert_eq!(scrubbed, "*** *** ***");
    }

    #[test]
    fn leaves_payloads_without_secrets_untouched() {
        let log = log();
        log.register_secret("app-password");

        assert!(matches!(
            log.scrub("* OK Gimap ready for requests"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn ignores_secrets_too_short_to_search_for() {
        // A three-character secret would match inside ordinary protocol data
        // and turn the trace into noise; protocol-level redaction covers it.
        let log = log();
        log.register_secret("abc");

        assert_eq!(log.scrub("FETCH abcdef"), "FETCH abcdef");
    }
}
