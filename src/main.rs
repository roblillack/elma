mod app;
mod backend;
mod model;
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
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, prelude::CrosstermBackend};
use serde::Deserialize;
use std::io::{self, Stdout};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

const TICK_RATE: Duration = Duration::from_millis(100);

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
                Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        }
    };

    restore_terminal(terminal)?;
    result
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("failed to create terminal instance")
}

fn restore_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
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
    let backend_name = config.r#type.to_ascii_lowercase();
    match backend_name.as_str() {
        "gmail" => {
            let username = config
                .email
                .ok_or_else(|| anyhow!("accounts[{index}].username missing for Gmail backend"))?;
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
            let username = config
                .username
                .take()
                .or_else(|| config.email.clone())
                .ok_or_else(|| anyhow!("accounts[{index}].username missing for JMAP backend"))?;
            let auth = if let Some(token) = config.token.take() {
                JmapAuth::Bearer { token }
            } else {
                let password = config.password.take().ok_or_else(|| {
                    anyhow!("accounts[{index}].password or token missing for JMAP backend")
                })?;
                JmapAuth::Basic {
                    username: username.clone(),
                    password,
                }
            };
            let mut base_url = config
                .url
                .take()
                .unwrap_or_else(|| "https://api.fastmail.com/jmap/session".to_string());
            normalize_fastmail_url(&mut base_url);
            let mut trusted_hosts = config.redirect_hosts.take().unwrap_or_default();
            if let Some(host) = url_host(&base_url) {
                if !trusted_hosts
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(host))
                {
                    trusted_hosts.push(host.to_string());
                }
                extend_fastmail_hosts(host, &mut trusted_hosts);
            }
            let backend = JmapBackend::new(JmapConfig {
                base_url,
                auth,
                trusted_hosts,
            })
            .with_context(|| format!("failed to initialize JMAP backend for {username}"))?;
            let name = config.name.unwrap_or(username);
            Ok(AccountDescriptor::new(name, Arc::new(backend)))
        }
        other => Err(anyhow!("accounts[{index}]: unsupported backend '{other}'")),
    }
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".elmarc"))
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(alias = "accounts")]
    accounts: Option<Vec<AccountConfig>>,
}

#[derive(Debug, Deserialize)]
struct AccountConfig {
    name: Option<String>,
    r#type: String,
    email: Option<String>,
    password: Option<String>,
    username: Option<String>,
    token: Option<String>,
    url: Option<String>,
    #[serde(alias = "redirect_hosts")]
    redirect_hosts: Option<Vec<String>>,
}

fn url_host(url: &str) -> Option<&str> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = without_scheme.split('/').next()?.trim();
    if host.is_empty() { None } else { Some(host) }
}

fn extend_fastmail_hosts(host: &str, list: &mut Vec<String>) {
    if host.ends_with("fastmail.com") {
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
}

fn normalize_fastmail_url(url: &mut String) {
    if url.eq_ignore_ascii_case("https://jmap.fastmail.com")
        || url.eq_ignore_ascii_case("https://jmap.fastmail.com/")
        || url.eq_ignore_ascii_case("https://jmap.fastmail.com/.well-known/jmap")
    {
        *url = "https://api.fastmail.com/jmap/session".to_string();
        return;
    }

    if url.eq_ignore_ascii_case("https://api.fastmail.com")
        || url.eq_ignore_ascii_case("https://api.fastmail.com/")
    {
        *url = "https://api.fastmail.com/jmap/session".to_string();
        return;
    }

    if url.ends_with('/') && !url.contains("/jmap/session") {
        while url.ends_with('/') {
            url.pop();
        }
    }

    if url.eq_ignore_ascii_case("api.fastmail.com") {
        *url = "https://api.fastmail.com/jmap/session".to_string();
    }
}
