//! Terminal setup and teardown, and working out which of its two backgrounds
//! the terminal is showing.
//!
//! Elma paints popups differently on a dark terminal than on a light one (see
//! [`crate::ui::theme`]), which means it has to know which it is on.  Nothing in
//! the terminal API says so, so this asks in the only way there is: the OSC 11
//! query, `ESC ] 11 ; ? ST`, which a terminal answers with its background
//! colour.  Terminals that do not know the query stay silent, so the query is
//! sent with a primary device attributes request behind it -- every terminal
//! answers that one, which is what makes it safe to stop reading.
//!
//! Two fallbacks stand behind the query, and the configuration stands in front
//! of it: `theme = "dark"` or `"light"` in `~/.elmarc` skips the handshake
//! entirely.

use std::io::{self, IsTerminal, Read, Stdout, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, prelude::CrosstermBackend};

use crate::ui::Theme;

/// How long the terminal has to answer the background query.  Long enough for
/// a round trip over a slow ssh link, short enough not to read as a hang.
const QUERY_TIMEOUT: Duration = Duration::from_millis(150);

/// How much longer a colour has to arrive once the terminal has answered the
/// query behind it.  Only spent on terminals that do not support the question.
const FENCE_GRACE: Duration = Duration::from_millis(20);

/// What the client should use when nothing says otherwise.  Dark terminals are
/// both the more common and the more forgiving of the two: the light popup
/// surface a dark theme puts on top still reads on a light terminal, while the
/// black one a light theme uses does not read on a dark one.
const FALLBACK: Theme = Theme::Dark;

/// What the configuration asks for, which may be "work it out".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemePreference {
    /// Ask the terminal, and fall back if it will not say.
    #[default]
    Auto,
    Fixed(Theme),
}

impl ThemePreference {
    /// Read a `theme = ...` value from the configuration file.
    ///
    /// Anything else is an error rather than a silent fallback: a typo in the
    /// key would otherwise show up only as the wrong colours.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "dark" => Ok(Self::Fixed(Theme::Dark)),
            "light" => Ok(Self::Fixed(Theme::Light)),
            other => Err(anyhow::anyhow!(
                "theme: expected \"dark\", \"light\" or \"auto\", found \"{other}\""
            )),
        }
    }
}

/// Enter raw mode and the alternate screen, and report the theme to draw in.
///
/// The theme is settled here because the query has to go out after raw mode is
/// on -- otherwise the terminal's answer is echoed and line-buffered -- and
/// before the alternate screen is up, so a terminal that answers with something
/// unparseable leaves its debris on the shell's screen rather than in the middle
/// of the mailbox.
pub fn init(preference: ThemePreference) -> Result<(Terminal<CrosstermBackend<Stdout>>, Theme)> {
    enable_raw_mode().context("failed to enable raw mode")?;

    let theme = match preference {
        ThemePreference::Fixed(theme) => theme,
        ThemePreference::Auto => detect_theme(),
    };

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend).context("failed to create terminal instance")?;
    Ok((terminal, theme))
}

pub fn restore(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
    .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")
}

/// Work out the terminal's background: ask it, then believe `COLORFGBG`, then
/// give up and assume a dark one.
///
/// Only called with raw mode already on.
fn detect_theme() -> Theme {
    if io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && let Some(background) = query_background(QUERY_TIMEOUT)
    {
        return theme_for(background);
    }

    std::env::var("COLORFGBG")
        .ok()
        .and_then(|value| theme_from_colorfgbg(&value))
        .unwrap_or(FALLBACK)
}

/// Whether a background colour is light enough to be a light theme, by
/// perceived brightness (ITU-R BT.709).
fn theme_for((r, g, b): (u8, u8, u8)) -> Theme {
    let luminance = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    if luminance >= 128.0 {
        Theme::Light
    } else {
        Theme::Dark
    }
}

/// Ask the terminal for its background colour and wait for the answer.
///
/// Reads from stdin directly rather than through crossterm's event loop, which
/// has no notion of an OSC reply.  That is safe only here, before the main loop
/// starts reading keys.
///
/// Reading goes on until the device attributes answer lands even once the
/// colour is in hand, because a terminal answers queries in the order it was
/// asked: whatever is left in the buffer when this returns is read as input by
/// the main loop, and `ESC [ ? 6 2 ; 1 c` arriving as keystrokes would delete a
/// message.
#[cfg(unix)]
fn query_background(timeout: Duration) -> Option<(u8, u8, u8)> {
    let mut stdout = io::stdout();
    // OSC 11 asks the question; DA1 behind it is the fence.  A terminal that
    // does not know OSC 11 ignores it silently but still answers DA1, and that
    // answer is what says "no colour is coming" without waiting out the timeout.
    stdout.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    stdout.flush().ok()?;

    let mut deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 128];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !readable(remaining) {
            break;
        }
        match io::stdin().read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => reply.extend_from_slice(&chunk[..read]),
        }
        if ends_device_attributes(&reply) {
            if parse_background(&reply).is_some() {
                break;
            }
            // The fence came back without a colour in front of it.  Usually
            // that means the terminal has no answer to give, but a colour
            // arriving late would be read as keystrokes, so wait out a moment
            // rather than the rest of the timeout.
            deadline = deadline.min(Instant::now() + FENCE_GRACE);
        }
    }

    parse_background(&reply)
}

/// No stdin poll off Unix, so nothing is asked and the fallbacks decide.
#[cfg(not(unix))]
fn query_background(_timeout: Duration) -> Option<(u8, u8, u8)> {
    None
}

/// Wait until stdin has something to read, or `timeout` runs out.
#[cfg(unix)]
fn readable(timeout: Duration) -> bool {
    let mut poll_fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one initialised pollfd is described to poll, which writes back
    // only into `revents`.
    unsafe { libc::poll(&mut poll_fd, 1, millis) > 0 }
}

/// Pull the background colour out of an `OSC 11 ; rgb:RRRR/GGGG/BBBB ST` reply.
///
/// The components are hex of any width -- 4 digits is what most terminals send,
/// but 2 and 1 are both legal -- and are scaled to 8 bits by their width, so
/// `ffff`, `ff` and `f` all mean full brightness.
fn parse_background(reply: &[u8]) -> Option<(u8, u8, u8)> {
    let text = String::from_utf8_lossy(reply);
    let body = text.split("\x1b]11;").nth(1)?;
    // The reply ends at BEL or at ST; until one of them lands it is still
    // arriving, and a component read now could be half a number.
    let end = body.find(['\x07', '\x1b'])?;
    let components = body[..end].strip_prefix("rgb:")?;

    let mut parts = components.split('/');
    let r = scale_component(parts.next()?)?;
    let g = scale_component(parts.next()?)?;
    let b = scale_component(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

/// One `RRRR`-style component of an OSC colour, as 8 bits.
fn scale_component(digits: &str) -> Option<u8> {
    let digits = digits.trim();
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    Some((value * 255 / max) as u8)
}

/// Whether the reply carries a primary device attributes answer,
/// `ESC [ ? ... c`, which is the terminal saying it has answered everything it
/// is going to.
fn ends_device_attributes(reply: &[u8]) -> bool {
    reply
        .windows(3)
        .position(|window| window == b"\x1b[?")
        .is_some_and(|start| reply[start + 3..].contains(&b'c'))
}

/// Read a theme out of `COLORFGBG`, the convention rxvt started and konsole and
/// others kept: foreground and background as palette indices, sometimes with a
/// third field between them.
///
/// Only the 16 base colours are understood.  A terminal that reports something
/// else is left to the fallback rather than guessed at.
fn theme_from_colorfgbg(value: &str) -> Option<Theme> {
    let background: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    match background {
        0..=6 | 8 => Some(Theme::Dark),
        7 | 9..=15 => Some(Theme::Light),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_theme_is_taken_literally() {
        assert_eq!(
            ThemePreference::parse("dark").unwrap(),
            ThemePreference::Fixed(Theme::Dark)
        );
        assert_eq!(
            ThemePreference::parse(" Light ").unwrap(),
            ThemePreference::Fixed(Theme::Light)
        );
        assert_eq!(
            ThemePreference::parse("auto").unwrap(),
            ThemePreference::Auto
        );
    }

    /// A misspelt theme is worth an error: silently drawing in the other one
    /// looks like a bug in the client rather than a typo in the file.
    #[test]
    fn an_unknown_theme_is_rejected() {
        let error = ThemePreference::parse("solarized").expect_err("unknown theme");
        assert!(error.to_string().contains("solarized"), "{error}");
    }

    #[test]
    fn a_background_query_answer_is_understood() {
        // What xterm, kitty, iTerm2 and friends send back.
        assert_eq!(
            parse_background(b"\x1b]11;rgb:2e2e/3434/3636\x1b\\"),
            Some((46, 52, 54))
        );
        // BEL-terminated, and two digits per component.
        assert_eq!(
            parse_background(b"\x1b]11;rgb:ee/ee/ec\x07"),
            Some((238, 238, 236))
        );
        // Padded to full brightness whatever the width.
        assert_eq!(
            parse_background(b"\x1b]11;rgb:f/f/f\x07"),
            Some((255, 255, 255))
        );
        // A DA1 answer arriving first does not get in the way.
        assert_eq!(
            parse_background(b"\x1b[?62;c\x1b]11;rgb:0000/0000/0000\x07"),
            Some((0, 0, 0))
        );
    }

    #[test]
    fn a_half_arrived_or_nonsense_answer_is_not_guessed_at() {
        // No terminator yet: the rest may still be on its way.
        assert_eq!(parse_background(b"\x1b]11;rgb:2e2e/3434/36"), None);
        // Answers to something else entirely.
        assert_eq!(parse_background(b"\x1b[?62;1;c"), None);
        assert_eq!(parse_background(b""), None);
        // A colour space the client cannot read.
        assert_eq!(parse_background(b"\x1b]11;cmy:0.1/0.2/0.3\x07"), None);
        // Too many components to be a colour.
        assert_eq!(parse_background(b"\x1b]11;rgb:11/22/33/44\x07"), None);
    }

    #[test]
    fn brightness_decides_which_theme_a_background_is() {
        // Tango's two: #2e3436 and #eeeeec.
        assert_eq!(theme_for((46, 52, 54)), Theme::Dark);
        assert_eq!(theme_for((238, 238, 236)), Theme::Light);
        // Green weighs the most, blue the least: a saturated blue is still dark.
        assert_eq!(theme_for((0, 0, 255)), Theme::Dark);
        assert_eq!(theme_for((0, 200, 0)), Theme::Light);
    }

    #[test]
    fn the_da1_fence_is_recognised() {
        assert!(ends_device_attributes(b"\x1b[?62;1;6c"));
        assert!(ends_device_attributes(b"\x1b]11;rgb:00/00/00\x07\x1b[?6c"));
        // Still arriving: the final `c` has not landed.
        assert!(!ends_device_attributes(b"\x1b[?62;1;6"));
        assert!(!ends_device_attributes(b"\x1b]11;rgb:00/00/00\x07"));
    }

    #[test]
    fn colorfgbg_says_which_background_it_has() {
        assert_eq!(theme_from_colorfgbg("15;0"), Some(Theme::Dark));
        assert_eq!(theme_from_colorfgbg("0;15"), Some(Theme::Light));
        // konsole writes three fields, the background last.
        assert_eq!(theme_from_colorfgbg("15;default;0"), Some(Theme::Dark));
        assert_eq!(theme_from_colorfgbg("0;default;7"), Some(Theme::Light));
        // Nothing usable: the caller falls back rather than guessing.
        assert_eq!(theme_from_colorfgbg("15;default"), None);
        assert_eq!(theme_from_colorfgbg("12;250"), None);
        assert_eq!(theme_from_colorfgbg(""), None);
    }
}
