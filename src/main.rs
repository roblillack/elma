mod app;
mod backend;
mod clock;
mod model;
#[cfg(test)]
mod test_harness;
mod ui;
mod viewer;

use crate::app::{AccountDescriptor, App};
use crate::backend::{
    MailBackend,
    gmail::GmailBackend,
    jmap::{JmapAuth, JmapBackend, JmapConfig},
    mock::MockBackend,
};
use anyhow::{Context, Result, anyhow};
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, prelude::CrosstermBackend};
use serde::Deserialize;
use std::io::{self, Stdout};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

const TICK_RATE: Duration = Duration::from_millis(100);

/// Where a JMAP account looks for its session object when the configuration
/// does not say.  FastMail is the provider this backend was written against.
const DEFAULT_JMAP_SESSION_URL: &str = "https://api.fastmail.com/jmap/session";

fn main() -> Result<()> {
    let args = std::env::args().skip(1);
    let mut demo_mode = false;

    for arg in args {
        match arg.as_str() {
            "-D" | "--demo" => demo_mode = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            _ => {
                eprintln!("Unknown argument: {arg}");
                print_usage();
                return Ok(());
            }
        }
    }

    let accounts = load_accounts(demo_mode)?;

    let mut app = App::new(accounts).context("failed to initialize application state")?;
    run(&mut app).context("failed while running application loop")
}

fn run(app: &mut App) -> Result<()> {
    let mut terminal = init_terminal().context("failed to set up terminal")?;
    let result = loop {
        app.poll_backend_events();

        // Something -- the editor -- had the screen and wiped what the renderer
        // drew, so the next frame cannot be a diff against it.
        if app.take_full_redraw() {
            terminal
                .clear()
                .context("failed to repaint after returning from a child process")?;
        }

        terminal
            .draw(|frame| ui::render(frame, app))
            .context("failed to render frame")?;

        if app.should_quit() {
            break Ok(());
        }

        if event::poll(TICK_RATE).context("failed to poll for events")? {
            match event::read().context("failed to read event")? {
                Event::Key(key) => app.handle_key(key).context("failed to handle key event")?,
                Event::Resize(_, _) => app.on_resize(),
                Event::Mouse(_) => {}
                Event::Paste(text) => app
                    .handle_paste_text(&text)
                    .context("failed to handle paste event")?,
                Event::FocusGained | Event::FocusLost => {}
            }
        }
    };

    restore_terminal(terminal)?;
    result
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("failed to create terminal instance")
}

fn restore_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
    .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")
}

fn print_usage() {
    println!("elma-rs - Ratatui-based mail client demo");
    println!();
    println!("USAGE:");
    println!("    elma-rs [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    -D, --demo    Run with the built-in mock backend (default)");
    println!("    -h, --help    Show this help message");
}

fn load_accounts(demo_mode: bool) -> Result<Vec<AccountDescriptor>> {
    if demo_mode {
        return Ok(vec![AccountDescriptor::new(
            "Demo",
            Arc::new(MockBackend::demo()),
        )]);
    }

    match load_accounts_from_config()? {
        Some(accounts) if !accounts.is_empty() => Ok(accounts),
        Some(_) => {
            eprintln!(
                "No accounts configured; falling back to demo backend (use --demo to hide this message)."
            );
            Ok(vec![AccountDescriptor::new(
                "Demo",
                Arc::new(MockBackend::demo()),
            )])
        }
        None => {
            eprintln!(
                "No configuration file found; using demo backend (use --demo to hide this message)."
            );
            Ok(vec![AccountDescriptor::new(
                "Demo",
                Arc::new(MockBackend::demo()),
            )])
        }
    }
}

fn load_accounts_from_config() -> Result<Option<Vec<AccountDescriptor>>> {
    let path = match config_path() {
        Some(path) => path,
        None => return Ok(None),
    };

    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("unable to read configuration file {}", path.display()))?;
    let config: Config = toml::from_str(&raw)
        .with_context(|| format!("unable to parse configuration file {}", path.display()))?;

    if let Some(entries) = config.accounts {
        let mut accounts = Vec::new();
        for (idx, entry) in entries.into_iter().enumerate() {
            accounts.push(build_account_from_config(entry, idx)?);
        }
        return Ok(Some(accounts));
    }

    Ok(Some(Vec::new()))
}

fn build_account_from_config(mut config: AccountConfig, index: usize) -> Result<AccountDescriptor> {
    let backend_name = config.backend.to_ascii_lowercase();
    match backend_name.as_str() {
        "gmail" => {
            let username = config
                .email
                .or(config.username)
                .ok_or_else(|| anyhow!("accounts[{index}].email missing for Gmail backend"))?;
            let password = config
                .password
                .ok_or_else(|| anyhow!("accounts[{index}].password missing for Gmail backend"))?;
            let backend = GmailBackend::new(&username, password)
                .with_context(|| format!("failed to initialize Gmail backend for {username}"))?;
            let name = config.name.unwrap_or(username);
            Ok(AccountDescriptor::new(name, Arc::new(backend)))
        }
        "demo" => {
            let name = config.name.unwrap_or("Demo".to_string());
            let backend: Arc<dyn MailBackend> = Arc::new(MockBackend::demo());
            Ok(AccountDescriptor::new(name, backend))
        }
        "jmap" => {
            let JmapSetup {
                username,
                config: jmap,
            } = jmap_setup_from_config(&mut config, index)?;
            let backend = JmapBackend::new(jmap)
                .with_context(|| format!("failed to initialize JMAP backend for {username}"))?;
            let name = config.name.unwrap_or(username);
            Ok(AccountDescriptor::new(name, Arc::new(backend)))
        }
        other => Err(anyhow!("accounts[{index}]: unsupported backend '{other}'")),
    }
}

/// A JMAP account worked out from one `[[accounts]]` entry.
///
/// Safe to format: the credential it carries is inside [`JmapConfig`], whose
/// `Debug` redacts it.
#[derive(Debug)]
struct JmapSetup {
    /// Who we are logging in as, also the account's name unless one is given.
    username: String,
    config: JmapConfig,
}

/// Translate an `[[accounts]]` entry into the parameters the JMAP backend takes.
///
/// Kept apart from [`build_account_from_config`] so the mapping -- which
/// endpoint, which credentials, which hosts a redirect may go to -- is testable
/// without building a backend or touching the network.
///
/// Either credential works: `token` is sent as a bearer token, `password` as
/// HTTP Basic.  A token wins if both are configured, since a server that issues
/// tokens is the one that wants them.
fn jmap_setup_from_config(config: &mut AccountConfig, index: usize) -> Result<JmapSetup> {
    let username = config
        .username
        .take()
        .or_else(|| config.email.clone())
        .ok_or_else(|| anyhow!("accounts[{index}].username missing for JMAP backend"))?;

    let mut base_url = config
        .url
        .take()
        .unwrap_or_else(|| DEFAULT_JMAP_SESSION_URL.to_string());
    normalize_fastmail_url(&mut base_url);
    let host_is_fastmail = url_host(&base_url).is_some_and(is_fastmail_host);

    let auth = if let Some(token) = config.token.take() {
        JmapAuth::Bearer { token }
    } else if let Some(password) = config.password.take() {
        // FastMail's app-specific passwords cover IMAP, POP, SMTP, CalDAV and
        // CardDAV, but not JMAP: every JMAP endpoint answers a Basic header with
        // `401 Invalid Authorization header, not bearer` before it ever looks at
        // the credentials, and advertises bearer as the only method it accepts
        // (`.well-known/oauth-protected-resource/jmap/session`).  Letting the
        // account through would trade this explanation for that bare 401 on the
        // first folder load, so say it here instead.  Should FastMail start
        // accepting Basic, this is the check to drop.
        if host_is_fastmail {
            return Err(anyhow!(
                "accounts[{index}]: {base_url} is FastMail's JMAP API, which accepts API tokens \
                 only, not passwords. App-specific passwords work for IMAP, POP, SMTP, CalDAV \
                 and CardDAV, but not for JMAP. Create a token with the \"Mail\" scope under \
                 Settings -> Privacy & Security -> Manage API tokens and configure it as \
                 `token = \"...\"` in place of `password = \"...\"`."
            ));
        }
        JmapAuth::Basic {
            username: username.clone(),
            password,
        }
    } else {
        return Err(anyhow!(
            "accounts[{index}].password or token missing for JMAP backend"
        ));
    };

    let mut trusted_hosts = config.redirect_hosts.take().unwrap_or_default();
    if let Some(host) = url_host(&base_url) {
        if !trusted_hosts
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(host))
        {
            trusted_hosts.push(host.to_string());
        }
        if host_is_fastmail {
            extend_fastmail_hosts(&mut trusted_hosts);
        }
    }

    Ok(JmapSetup {
        username,
        config: JmapConfig {
            base_url,
            auth,
            trusted_hosts,
        },
    })
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".elmarc"))
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(alias = "accounts")]
    accounts: Option<Vec<AccountConfig>>,
}

#[derive(Deserialize)]
struct AccountConfig {
    name: Option<String>,
    /// Which backend serves the account.  `type` is accepted as well: both
    /// spellings have been documented, and a configuration that names the older
    /// one would otherwise fail to parse at all.
    #[serde(alias = "type")]
    backend: String,
    email: Option<String>,
    password: Option<String>,
    username: Option<String>,
    token: Option<String>,
    url: Option<String>,
    #[serde(alias = "redirect_hosts")]
    redirect_hosts: Option<Vec<String>>,
}

/// Written out by hand rather than derived, so that formatting an account --
/// in an error context, a panic message, a debug print -- cannot spill the
/// password or token it carries.
impl std::fmt::Debug for AccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountConfig")
            .field("name", &self.name)
            .field("backend", &self.backend)
            .field("email", &self.email)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("username", &self.username)
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("url", &self.url)
            .field("redirect_hosts", &self.redirect_hosts)
            .finish()
    }
}

fn url_host(url: &str) -> Option<&str> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = without_scheme.split('/').next()?.trim();
    if host.is_empty() { None } else { Some(host) }
}

/// Whether `host` belongs to FastMail, rather than merely ending in something
/// that reads like it (`notfastmail.com`).
fn is_fastmail_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("fastmail.com")
        || host.to_ascii_lowercase().ends_with(".fastmail.com")
}

/// Add the hosts FastMail moves a session between, so the redirect policy lets
/// them through.
fn extend_fastmail_hosts(list: &mut Vec<String>) {
    for candidate in [
        "fastmail.com",
        "www.fastmail.com",
        "api.fastmail.com",
        "jmap.fastmail.com",
    ] {
        if !list
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(candidate))
        {
            list.push(candidate.to_string());
        }
    }
}

/// Point the ways one might write down "FastMail" at the endpoint FastMail
/// actually serves.  `jmap.fastmail.com` was retired -- it answers with a
/// redirect to the marketing site -- so a configuration naming it is sent to
/// `api.fastmail.com` rather than left to fail.
fn normalize_fastmail_url(url: &mut String) {
    if url.eq_ignore_ascii_case("https://jmap.fastmail.com")
        || url.eq_ignore_ascii_case("https://jmap.fastmail.com/")
        || url.eq_ignore_ascii_case("https://jmap.fastmail.com/.well-known/jmap")
    {
        *url = DEFAULT_JMAP_SESSION_URL.to_string();
        return;
    }

    if url.eq_ignore_ascii_case("https://api.fastmail.com")
        || url.eq_ignore_ascii_case("https://api.fastmail.com/")
    {
        *url = DEFAULT_JMAP_SESSION_URL.to_string();
        return;
    }

    if url.ends_with('/') && !url.contains("/jmap/session") {
        while url.ends_with('/') {
            url.pop();
        }
    }

    if url.eq_ignore_ascii_case("api.fastmail.com") {
        *url = DEFAULT_JMAP_SESSION_URL.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(toml_source: &str) -> AccountConfig {
        let config: Config = toml::from_str(toml_source).expect("configuration should parse");
        config
            .accounts
            .expect("configuration should carry accounts")
            .pop()
            .expect("configuration should carry one account")
    }

    fn jmap_setup(toml_source: &str) -> Result<JmapSetup> {
        jmap_setup_from_config(&mut account(toml_source), 0)
    }

    #[test]
    fn jmap_password_authenticates_with_basic() {
        let setup = jmap_setup(
            r#"
            [[accounts]]
            backend = "jmap"
            username = "rob@example.com"
            password = "s3cret"
            url = "https://mail.example.com"
            "#,
        )
        .expect("a password should be enough to set up a JMAP account");

        assert_eq!(setup.username, "rob@example.com");
        assert_eq!(setup.config.base_url, "https://mail.example.com");
        assert_eq!(
            format!("{:?}", setup.config.auth),
            r#"Basic { username: "rob@example.com" }"#
        );
    }

    /// The username the Basic header carries is the account's, not whatever the
    /// server might infer from the endpoint.
    #[test]
    fn jmap_password_falls_back_to_the_email_address() {
        let setup = jmap_setup(
            r#"
            [[accounts]]
            backend = "jmap"
            email = "rob@example.com"
            password = "s3cret"
            url = "https://mail.example.com"
            "#,
        )
        .expect("an email address should stand in for a username");

        assert_eq!(setup.username, "rob@example.com");
        assert_eq!(
            format!("{:?}", setup.config.auth),
            r#"Basic { username: "rob@example.com" }"#
        );
    }

    #[test]
    fn jmap_token_authenticates_with_bearer() {
        let setup = jmap_setup(
            r#"
            [[accounts]]
            backend = "jmap"
            username = "rob@fastmail.com"
            token = "an-api-token"
            "#,
        )
        .expect("a token should be enough to set up a JMAP account");

        assert_eq!(format!("{:?}", setup.config.auth), "Bearer");
        assert_eq!(setup.config.base_url, DEFAULT_JMAP_SESSION_URL);
    }

    #[test]
    fn jmap_prefers_the_token_when_both_credentials_are_given() {
        let setup = jmap_setup(
            r#"
            [[accounts]]
            backend = "jmap"
            username = "rob@fastmail.com"
            password = "s3cret"
            token = "an-api-token"
            "#,
        )
        .expect("a token should be usable alongside a password");

        assert_eq!(format!("{:?}", setup.config.auth), "Bearer");
    }

    #[test]
    fn jmap_without_a_credential_is_rejected() {
        let error = jmap_setup(
            r#"
            [[accounts]]
            backend = "jmap"
            username = "rob@example.com"
            "#,
        )
        .expect_err("an account without a credential cannot connect");

        assert!(
            error.to_string().contains("password or token missing"),
            "unexpected error: {error}"
        );
    }

    /// FastMail's JMAP API only accepts bearer tokens, so an account configured
    /// with a password is turned away at startup -- with the token to create
    /// spelled out -- rather than on the first folder load.
    #[test]
    fn jmap_password_against_fastmail_explains_the_token() {
        for url in [
            None,
            Some("https://api.fastmail.com/jmap/session"),
            Some("https://jmap.fastmail.com/.well-known/jmap"),
            Some("https://phl.api.fastmail.com/jmap/session"),
        ] {
            let mut source = String::from(
                "[[accounts]]\nbackend = \"jmap\"\nusername = \"rob@fastmail.com\"\npassword = \"s3cret\"\n",
            );
            if let Some(url) = url {
                source.push_str(&format!("url = \"{url}\"\n"));
            }

            let error = jmap_setup(&source)
                .expect_err("FastMail cannot be reached over JMAP with a password");
            let message = error.to_string();
            assert!(
                message.contains("API token") && message.contains("token = "),
                "unexpected error for {url:?}: {message}"
            );
        }
    }

    /// A server that is not FastMail is left to judge the password itself.
    #[test]
    fn jmap_password_against_a_lookalike_host_is_allowed() {
        let setup = jmap_setup(
            r#"
            [[accounts]]
            backend = "jmap"
            username = "rob@notfastmail.com"
            password = "s3cret"
            url = "https://jmap.notfastmail.com"
            "#,
        )
        .expect("only FastMail's own hosts refuse Basic authentication");

        assert!(format!("{:?}", setup.config.auth).starts_with("Basic"));
    }

    #[test]
    fn jmap_trusts_the_endpoint_host_for_redirects() {
        let setup = jmap_setup(
            r#"
            [[accounts]]
            backend = "jmap"
            username = "rob@example.com"
            password = "s3cret"
            url = "https://mail.example.com/.well-known/jmap"
            "#,
        )
        .expect("setup should succeed");

        assert_eq!(setup.config.trusted_hosts, vec!["mail.example.com"]);
    }

    #[test]
    fn jmap_trusts_the_hosts_fastmail_redirects_between() {
        let setup = jmap_setup(
            r#"
            [[accounts]]
            backend = "jmap"
            username = "rob@fastmail.com"
            token = "an-api-token"
            "#,
        )
        .expect("setup should succeed");

        for host in ["api.fastmail.com", "www.fastmail.com", "jmap.fastmail.com"] {
            assert!(
                setup.config.trusted_hosts.iter().any(|h| h == host),
                "{host} should be trusted: {:?}",
                setup.config.trusted_hosts
            );
        }
    }

    /// `backend` names the backend; `type`, which earlier configurations use,
    /// is still understood.
    #[test]
    fn account_backend_is_also_spelled_type() {
        for key in ["backend", "type"] {
            assert_eq!(
                account(&format!(
                    "[[accounts]]\n{key} = \"jmap\"\nusername = \"rob@example.com\"\npassword = \"s3cret\"\n"
                ))
                .backend,
                "jmap",
                "`{key}` should select the backend"
            );
        }
    }

    #[test]
    fn retired_fastmail_urls_are_pointed_at_the_current_endpoint() {
        for candidate in [
            "https://jmap.fastmail.com",
            "https://jmap.fastmail.com/",
            "https://jmap.fastmail.com/.well-known/jmap",
            "https://api.fastmail.com",
            "https://api.fastmail.com/",
            "api.fastmail.com",
        ] {
            let mut url = candidate.to_string();
            normalize_fastmail_url(&mut url);
            assert_eq!(url, DEFAULT_JMAP_SESSION_URL, "for {candidate}");
        }
    }

    #[test]
    fn other_urls_only_lose_their_trailing_slash() {
        let mut url = "https://mail.example.com/jmap/".to_string();
        normalize_fastmail_url(&mut url);
        assert_eq!(url, "https://mail.example.com/jmap");
    }

    #[test]
    fn fastmail_hosts_are_told_apart_from_lookalikes() {
        for host in ["fastmail.com", "api.fastmail.com", "PHL.API.FASTMAIL.COM"] {
            assert!(is_fastmail_host(host), "{host} is FastMail");
        }
        for host in ["notfastmail.com", "fastmail.com.example.net", "example.com"] {
            assert!(!is_fastmail_host(host), "{host} is not FastMail");
        }
    }

    #[test]
    fn url_host_ignores_scheme_and_path() {
        assert_eq!(
            url_host("https://mail.example.com/jmap/session"),
            Some("mail.example.com")
        );
        assert_eq!(url_host("mail.example.com"), Some("mail.example.com"));
        assert_eq!(url_host("https://"), None);
    }
}
