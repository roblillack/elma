//! Headless test harness: drives the real [`App`] -- real key handling, real
//! rendering, real backend threads -- against ratatui's `TestBackend`, and
//! renders the resulting cell buffer to deterministic SVG for `insta` snapshot
//! tests.
//!
//! The SVG renderer maps the terminal cell grid 1:1 onto a fixed-size pixel
//! grid of background `<rect>`s and `<text>` runs, so a snapshot captures the
//! styling the terminal would show -- colours, bold/italic, the reverse-video
//! selection bar, the strike-through of a message scheduled for deletion --
//! without rasterising any fonts: the output is identical on every machine,
//! diffs as text, and opens in any browser.
//!
//! Two things would otherwise differ between two runs of the same test, and
//! both are held still here: the clock (see [`crate::clock`]) and the mail,
//! which comes from [`FixtureBackend`] rather than the mock backend's
//! randomised inbox.
//!
//! Snapshots live in `src/snapshots/*.snap.svg`.  Review changes with
//! `cargo insta review`, or set `INSTA_UPDATE=always` to rewrite them.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Terminal,
    backend::{Backend as _, TestBackend},
    buffer::{Buffer, Cell},
    layout::Position,
    style::{Color, Modifier},
};
use time::{OffsetDateTime, macros::datetime};

use crate::app::{AccountDescriptor, App};
use crate::backend::{ActionStatus, BackendEvent, MailBackend, MailboxSnapshot, OutgoingMessage};
use crate::clock;
use crate::model::{
    Action, MailboxKind, Message, MessageAttachment, MessageContent, MessageContentPart, MessageId,
    MessageStatus,
};
use crate::ui;

/// The moment every snapshot is taken at.  Messages sent in 2026 therefore
/// render as `[Mar 12 08:15]` and older ones carry their year.
pub(crate) const NOW: OffsetDateTime = datetime!(2026-03-14 09:41:00 UTC);

/// The age every operation in flight reports.  Picked so the throbber sits on
/// a frame in the middle of its cycle and the counter reads `0.4s`.
pub(crate) const OPERATION_AGE: Duration = Duration::from_millis(400);

/// How long a test waits for the backend threads before giving up.  Nothing in
/// the fixtures blocks except by request, so reaching this means a deadlock.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Stands in for "this frame shows no caret", which the test backend cannot
/// say by itself.  Off-screen, so a frame that really does place a caret can
/// never be mistaken for it.
const NO_CURSOR: Position = Position {
    x: u16::MAX,
    y: u16::MAX,
};

const FONT_SIZE: usize = 16;
const DEFAULT_FG: &str = "#d8d8d8";
const DEFAULT_BG: &str = "#101010";

/// Pixel geometry of one terminal cell in the rendered SVG.
#[derive(Clone, Copy)]
pub(crate) struct CellMetrics {
    /// Cell width in px.
    pub(crate) width: usize,
    /// Cell height in px -- the line height.
    pub(crate) height: usize,
    /// Text baseline offset from the top of a cell row.
    pub(crate) baseline: usize,
}

impl Default for CellMetrics {
    /// The snapshot geometry: 20px rows leave breathing room around the 16px
    /// font, so snapshots stay legible with whatever monospace font the
    /// viewer's browser resolves.
    fn default() -> Self {
        Self {
            width: 10,
            height: 20,
            baseline: 15,
        }
    }
}

/// Elma running against an in-memory terminal.
pub(crate) struct TestApp {
    app: App,
    terminal: Terminal<TestBackend>,
}

impl TestApp {
    /// An app on a `width`x`height` terminal showing `accounts`, with the
    /// startup load already finished and the first frame drawn.
    pub(crate) fn new(width: u16, height: u16, accounts: Vec<AccountDescriptor>) -> Self {
        let mut app = Self::starting(width, height, accounts);
        app.settle();
        app.draw();
        app
    }

    /// A single-account app on the standard inbox fixture.
    pub(crate) fn inbox(width: u16, height: u16) -> Self {
        Self::new(
            width,
            height,
            vec![account("Personal", FixtureBackend::new())],
        )
    }

    /// Like [`TestApp::new`], but caught before the backend has answered: the
    /// mailbox is still empty and the loading overlay is up.
    ///
    /// Nothing is waited for, so the backend has to be one that cannot finish
    /// on its own -- [`FixtureBackend::blocking`] -- or the frame is a race.
    pub(crate) fn starting(width: u16, height: u16, accounts: Vec<AccountDescriptor>) -> Self {
        clock::freeze(NOW, OPERATION_AGE);
        let app = App::new(accounts).expect("build the application");
        let terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        let mut test_app = Self { app, terminal };
        test_app.draw();
        test_app
    }

    pub(crate) fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Render one frame.  Every method that feeds the app an event redraws
    /// afterwards, the way the main loop does.
    pub(crate) fn draw(&mut self) {
        // ratatui hands the backend a cursor position only for a frame that
        // asked for one, so a sentinel written beforehand survives exactly
        // when the view places no caret -- which is what tells the two apart
        // afterwards, the backend having no notion of a hidden cursor.
        self.terminal
            .backend_mut()
            .set_cursor_position(NO_CURSOR)
            .expect("reset the test cursor");
        let app = &mut self.app;
        self.terminal
            .draw(|frame| ui::render(frame, app))
            .expect("draw frame");
    }

    /// Where the frame placed the caret, if it placed one at all.
    fn cursor(&mut self) -> Option<Position> {
        match self.terminal.get_cursor_position() {
            Ok(position) if position != NO_CURSOR => Some(position),
            _ => None,
        }
    }

    /// Run the backend work started so far to completion, the way the main
    /// loop's polling would.
    ///
    /// Mailbox loads, message loads and committed actions all run on their own
    /// threads, so a test that just pressed the key that starts one has to wait
    /// for it before the frame it snapshots means anything.
    pub(crate) fn settle(&mut self) {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        loop {
            self.app.poll_backend_events();
            if !self.app.has_work_in_flight() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "backend work did not finish within {SETTLE_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Feed one key through the real event handling, let whatever it started
    /// finish, and redraw.
    pub(crate) fn key(&mut self, code: KeyCode) {
        self.key_with(code, KeyModifiers::NONE);
    }

    pub(crate) fn key_with(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app
            .handle_key(KeyEvent::new(code, modifiers))
            .expect("handle key");
        self.settle();
        self.draw();
    }

    /// Press a key without waiting for the work it starts, leaving the frame
    /// showing the operation in flight.
    pub(crate) fn key_without_settling(&mut self, code: KeyCode) {
        self.app
            .handle_key(KeyEvent::new(code, KeyModifiers::NONE))
            .expect("handle key");
        self.draw();
    }

    pub(crate) fn char(&mut self, ch: char) {
        self.key(KeyCode::Char(ch));
    }

    pub(crate) fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.char(ch);
        }
    }

    /// Empty the focused text field by holding backspace, which is what a user
    /// would do: no field in the client has a clear-the-line key.
    ///
    /// Used to replace a value that came from the machine the tests run on --
    /// the save dialog offers the user's own download folder -- with one a
    /// snapshot can be taken of.  The count is a ceiling: presses past the
    /// start of the field do nothing.
    pub(crate) fn clear_field(&mut self) {
        for _ in 0..256 {
            self.key(KeyCode::Backspace);
        }
    }

    /// Plain-text contents of the terminal buffer, one string per row.  Useful
    /// for asserting on what a frame says without pinning how it looks.
    pub(crate) fn buffer_lines(&self) -> Vec<String> {
        let buffer = self.terminal.backend().buffer();
        let area = buffer.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Render the current frame to SVG.
    pub(crate) fn svg(&mut self) -> String {
        let cursor = self.cursor();
        buffer_to_svg(
            self.terminal.backend().buffer(),
            cursor,
            CellMetrics::default(),
        )
    }
}

/// An account for [`TestApp::new`].
pub(crate) fn account(name: &str, backend: FixtureBackend) -> AccountDescriptor {
    AccountDescriptor::new(name, Arc::new(backend))
}

// -- SVG rendering ---------------------------------------------------------

/// The resolved visual style of a cell, with `REVERSED` already applied.
#[derive(Clone, PartialEq)]
struct CellStyle {
    fg: String,
    bg: String,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

fn cell_style(cell: &Cell) -> CellStyle {
    let mut fg = css_color(cell.fg, DEFAULT_FG);
    let mut bg = css_color(cell.bg, DEFAULT_BG);
    let modifier = cell.modifier;
    if modifier.contains(Modifier::REVERSED) {
        std::mem::swap(&mut fg, &mut bg);
    }
    CellStyle {
        fg,
        bg,
        bold: modifier.contains(Modifier::BOLD),
        dim: modifier.contains(Modifier::DIM),
        italic: modifier.contains(Modifier::ITALIC),
        underline: modifier.contains(Modifier::UNDERLINED),
        strike: modifier.contains(Modifier::CROSSED_OUT),
    }
}

/// The 16 base ANSI colours (VS Code's terminal palette).
const ANSI16: [&str; 16] = [
    "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5",
    "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#ffffff",
];

fn css_color(color: Color, default: &str) -> String {
    match color {
        Color::Reset => default.to_string(),
        Color::Black => ANSI16[0].to_string(),
        Color::Red => ANSI16[1].to_string(),
        Color::Green => ANSI16[2].to_string(),
        Color::Yellow => ANSI16[3].to_string(),
        Color::Blue => ANSI16[4].to_string(),
        Color::Magenta => ANSI16[5].to_string(),
        Color::Cyan => ANSI16[6].to_string(),
        Color::Gray => ANSI16[7].to_string(),
        Color::DarkGray => ANSI16[8].to_string(),
        Color::LightRed => ANSI16[9].to_string(),
        Color::LightGreen => ANSI16[10].to_string(),
        Color::LightYellow => ANSI16[11].to_string(),
        Color::LightBlue => ANSI16[12].to_string(),
        Color::LightMagenta => ANSI16[13].to_string(),
        Color::LightCyan => ANSI16[14].to_string(),
        Color::White => ANSI16[15].to_string(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => indexed_color(i),
    }
}

/// Standard xterm-256 palette.
fn indexed_color(index: u8) -> String {
    match index {
        0..=15 => ANSI16[index as usize].to_string(),
        16..=231 => {
            let i = index - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            let r = steps[(i / 36) as usize];
            let g = steps[((i % 36) / 6) as usize];
            let b = steps[(i % 6) as usize];
            format!("#{r:02x}{g:02x}{b:02x}")
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232);
            format!("#{v:02x}{v:02x}{v:02x}")
        }
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a ratatui cell buffer (plus optional cursor position) to SVG.
///
/// Cells of one row are coalesced into runs of equal style; each run emits a
/// background `<rect>` (when not the default background) and a `<text>` with an
/// exact `textLength`, so glyphs stay on the cell grid regardless of the font
/// the viewer resolves `monospace` to.
pub(crate) fn buffer_to_svg(
    buffer: &Buffer,
    cursor: Option<Position>,
    metrics: CellMetrics,
) -> String {
    let CellMetrics {
        width: cell_w,
        height: cell_h,
        baseline,
    } = metrics;
    let area = buffer.area;
    let width_px = area.width as usize * cell_w;
    let height_px = area.height as usize * cell_h;

    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width_px}" height="{height_px}" viewBox="0 0 {width_px} {height_px}" font-family="'DejaVu Sans Mono', Menlo, Consolas, monospace" font-size="{FONT_SIZE}px">"#
    );
    let _ = writeln!(
        svg,
        r#"<rect width="100%" height="100%" fill="{DEFAULT_BG}"/>"#
    );

    for y in 0..area.height {
        // Coalesce the row into runs of (start cell, cell count, text, style).
        let mut runs: Vec<(usize, usize, String, CellStyle)> = Vec::new();
        for x in 0..area.width {
            let cell = buffer.cell(Position::new(x, y)).expect("cell in area");
            let style = cell_style(cell);
            match runs.last_mut() {
                Some((_, cells, text, last_style)) if *last_style == style => {
                    *cells += 1;
                    text.push_str(cell.symbol());
                }
                _ => runs.push((x as usize, 1, cell.symbol().to_string(), style)),
            }
        }

        let row_px = y as usize * cell_h;
        for (start, cells, text, style) in &runs {
            if style.bg != DEFAULT_BG {
                let _ = writeln!(
                    svg,
                    r#"<rect x="{}" y="{row_px}" width="{}" height="{cell_h}" fill="{}"/>"#,
                    start * cell_w,
                    cells * cell_w,
                    style.bg,
                );
            }
            if text.trim().is_empty() && !style.underline && !style.strike {
                continue;
            }
            let mut attrs = String::new();
            if style.bold {
                attrs.push_str(r#" font-weight="bold""#);
            }
            if style.italic {
                attrs.push_str(r#" font-style="italic""#);
            }
            match (style.underline, style.strike) {
                (true, true) => attrs.push_str(r#" text-decoration="underline line-through""#),
                (true, false) => attrs.push_str(r#" text-decoration="underline""#),
                (false, true) => attrs.push_str(r#" text-decoration="line-through""#),
                (false, false) => {}
            }
            if style.dim {
                attrs.push_str(r#" opacity="0.6""#);
            }
            let _ = writeln!(
                svg,
                r#"<text x="{}" y="{}" fill="{}" textLength="{}" lengthAdjust="spacingAndGlyphs" xml:space="preserve"{attrs}>{}</text>"#,
                start * cell_w,
                row_px + baseline,
                style.fg,
                cells * cell_w,
                xml_escape(text),
            );
        }
    }

    if let Some(position) = cursor {
        let _ = writeln!(
            svg,
            r##"<rect x="{}" y="{}" width="{cell_w}" height="{cell_h}" fill="#ffffff" fill-opacity="0.4"/>"##,
            position.x as usize * cell_w,
            position.y as usize * cell_h,
        );
    }

    svg.push_str("</svg>\n");
    svg
}

// -- The fixture mailbox ---------------------------------------------------

/// A backend serving a fixed mailbox, written down message by message.
///
/// The mock backend generates its inbox at random from the current time, which
/// is what makes it a good demo and a useless snapshot fixture.  This one hands
/// out the same mail on every run, and can be told to stall or fail so the
/// states around a load -- the overlay, the error, a mailbox still filling in
/// behind the list -- are reachable without a race.
pub(crate) struct FixtureBackend {
    mailboxes: HashMap<MailboxKind, Vec<Message>>,
    contents: HashMap<MessageId, MessageContent>,
    /// Reported as the mailbox size regardless of how many messages are handed
    /// over, so a partly-loaded mailbox can be snapshotted.
    totals: HashMap<MailboxKind, usize>,
    /// Held closed until released, to catch the app while it is still loading.
    gate: Option<Arc<Gate>>,
    /// What every mailbox load fails with, if the account is meant to fail.
    failure: Option<String>,
}

/// A latch a load waits on, so a test decides when the mailbox arrives.
#[derive(Default)]
pub(crate) struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut open = self.open.lock().expect("gate mutex");
        while !*open {
            open = self.changed.wait(open).expect("gate mutex");
        }
    }

    /// Let every waiting load through.
    pub(crate) fn open(&self) {
        *self.open.lock().expect("gate mutex") = true;
        self.changed.notify_all();
    }
}

impl FixtureBackend {
    /// The standard fixture: an inbox of eleven messages plus a few of the
    /// other mailboxes, enough to show every flag, colour and label the list
    /// can draw.
    pub(crate) fn new() -> Self {
        Self {
            mailboxes: fixture_mailboxes(),
            contents: fixture_contents(),
            totals: HashMap::new(),
            gate: None,
            failure: None,
        }
    }

    /// A fixture that stalls on every mailbox load until [`release`] is called.
    ///
    /// [`release`]: FixtureBackend::release
    pub(crate) fn blocking() -> (Self, Arc<Gate>) {
        let gate = Arc::new(Gate::default());
        let backend = Self {
            gate: Some(Arc::clone(&gate)),
            ..Self::new()
        };
        (backend, gate)
    }

    /// A fixture whose mailbox loads all fail with `reason`.
    pub(crate) fn failing(reason: &str) -> Self {
        Self {
            failure: Some(reason.to_string()),
            ..Self::new()
        }
    }

    /// A fixture that announces `total` messages in `mailbox` but only hands
    /// over the ones it has, leaving the rest as placeholders the way a backend
    /// still fetching headers does.
    pub(crate) fn backfilling(mailbox: MailboxKind, total: usize) -> Self {
        let mut backend = Self::new();
        backend.totals.insert(mailbox, total);
        backend
    }

    /// An empty account -- every mailbox loads, and there is nothing in any of
    /// them.
    pub(crate) fn empty() -> Self {
        Self {
            mailboxes: HashMap::new(),
            contents: HashMap::new(),
            totals: HashMap::new(),
            gate: None,
            failure: None,
        }
    }
}

impl MailBackend for FixtureBackend {
    fn load_mailbox(
        &self,
        mailbox: MailboxKind,
    ) -> Result<(MailboxSnapshot, Receiver<BackendEvent>)> {
        if let Some(gate) = &self.gate {
            gate.wait();
        }
        if let Some(reason) = &self.failure {
            return Err(anyhow!("{reason}"));
        }

        let messages = self.mailboxes.get(&mailbox).cloned().unwrap_or_default();
        let total = self
            .totals
            .get(&mailbox)
            .copied()
            .unwrap_or(messages.len())
            .max(messages.len());
        // Never connected to anything, so nothing will ever arrive on it; the
        // app treats the closed channel as a quiet mailbox.
        let (_sender, events) = mpsc::channel();
        Ok((MailboxSnapshot { total, messages }, events))
    }

    fn load_message(&self, message_id: MessageId) -> Result<MessageContent> {
        self.contents
            .get(&message_id)
            .cloned()
            .ok_or_else(|| anyhow!("no such message: {message_id}"))
    }

    fn apply_actions(&self, actions: Vec<Action>) -> Result<Receiver<ActionStatus>> {
        let (sender, receiver) = mpsc::channel();
        for action in actions {
            let _ = sender.send(ActionStatus {
                action,
                result: Ok(()),
            });
        }
        Ok(receiver)
    }

    fn send_message(&self, _message: OutgoingMessage) -> Result<()> {
        Ok(())
    }

    fn save_draft(&self, _message: OutgoingMessage) -> Result<()> {
        Ok(())
    }

    fn fetch_attachment_blob(&self, _blob_id: &str) -> Result<Vec<u8>> {
        Ok(b"fixture attachment payload".to_vec())
    }
}

/// One message of the fixture inbox, written out in full so a snapshot diff
/// points at the field that changed.
struct Fixture {
    id: MessageId,
    sent: OffsetDateTime,
    sender: &'static str,
    recipients: &'static [&'static str],
    subject: &'static str,
    size: usize,
    status: MessageStatus,
    starred: bool,
    important: bool,
    answered: bool,
    forwarded: bool,
    has_attachments: bool,
    labels: &'static [&'static str],
}

impl Fixture {
    const fn new(
        id: MessageId,
        sent: OffsetDateTime,
        sender: &'static str,
        subject: &'static str,
        size: usize,
        status: MessageStatus,
    ) -> Self {
        Self {
            id,
            sent,
            sender,
            recipients: &["rob@example.com"],
            subject,
            size,
            status,
            starred: false,
            important: false,
            answered: false,
            forwarded: false,
            has_attachments: false,
            labels: &[],
        }
    }

    fn build(&self, seq: u32) -> Message {
        Message {
            id: self.id,
            sent: self.sent,
            sender: self.sender.to_string(),
            recipients: self.recipients.iter().map(|to| to.to_string()).collect(),
            subject: self.subject.to_string(),
            size: self.size,
            starred: self.starred,
            important: self.important,
            answered: self.answered,
            forwarded: self.forwarded,
            status: self.status,
            labels: self.labels.iter().map(|l| l.to_string()).collect(),
            uid: 1000 + self.id as u32,
            seq,
            has_attachments: self.has_attachments,
        }
    }
}

/// The inbox every list, viewer and composer snapshot starts from.
///
/// Between them the entries cover each state a row can be in: unread, read,
/// starred, important, answered, forwarded, carrying an attachment, labelled,
/// and -- the last one -- old enough that its date carries a year.
fn inbox_fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            starred: true,
            important: true,
            labels: &["Work", "Invoices"],
            has_attachments: true,
            // Two of them, so a reply-to-all has someone to put in the Cc.
            recipients: &["rob@example.com", "accounts@example.com"],
            ..Fixture::new(
                1,
                datetime!(2026-03-14 08:15:00 UTC),
                "billing@vendor.example",
                "Invoice 2026-0342 for February",
                18_400,
                MessageStatus::New,
            )
        },
        Fixture {
            labels: &["Work"],
            ..Fixture::new(
                2,
                datetime!(2026-03-14 07:52:00 UTC),
                "mira.tomassi@example.org",
                "Re: Release notes for 0.3",
                7_320,
                MessageStatus::New,
            )
        },
        Fixture {
            answered: true,
            ..Fixture::new(
                3,
                datetime!(2026-03-13 19:04:00 UTC),
                "dev-list@ratatui.example",
                "[dev] Widget layout changes landing next week",
                42_100,
                MessageStatus::Read,
            )
        },
        Fixture {
            starred: true,
            ..Fixture::new(
                4,
                datetime!(2026-03-13 15:30:00 UTC),
                "anna@example.com",
                "Lunch on Thursday?",
                2_100,
                MessageStatus::Read,
            )
        },
        Fixture {
            forwarded: true,
            has_attachments: true,
            labels: &["Travel"],
            ..Fixture::new(
                5,
                datetime!(2026-03-12 11:20:00 UTC),
                "reservations@hotel.example",
                "Your booking confirmation",
                96_500,
                MessageStatus::Read,
            )
        },
        Fixture {
            important: true,
            ..Fixture::new(
                6,
                datetime!(2026-03-11 09:00:00 UTC),
                "security@bank.example",
                "Unusual sign-in attempt blocked",
                5_600,
                MessageStatus::Read,
            )
        },
        Fixture::new(
            7,
            datetime!(2026-03-10 16:45:00 UTC),
            "newsletter@rustweekly.example",
            "This Week in Rust #612",
            134_000,
            MessageStatus::Read,
        ),
        Fixture::new(
            8,
            datetime!(2026-03-09 08:05:00 UTC),
            "no-reply@calendar.example",
            "Invitation: Sprint review @ Fri Mar 20",
            8_900,
            MessageStatus::Archived,
        ),
        Fixture::new(
            9,
            datetime!(2026-03-08 22:18:00 UTC),
            "deals@shop.example",
            "LAST CHANCE: 80% off everything!!!",
            61_000,
            MessageStatus::Spam,
        ),
        Fixture::new(
            10,
            datetime!(2026-03-07 13:37:00 UTC),
            "tickets@support.example",
            "Ticket #4711 has been closed",
            3_400,
            MessageStatus::Deleted,
        ),
        Fixture {
            labels: &["Work", "Archive"],
            ..Fixture::new(
                11,
                datetime!(2025-12-24 10:02:00 UTC),
                "hr@example.com",
                "Holiday schedule for next year",
                12_800,
                MessageStatus::Read,
            )
        },
    ]
}

/// Turn fixtures into a mailbox the way a server hands one over: oldest
/// message first, numbered from one, so the app selects the newest.
fn mailbox(fixtures: &[Fixture]) -> Vec<Message> {
    let mut sorted: Vec<&Fixture> = fixtures.iter().collect();
    sorted.sort_by_key(|fixture| fixture.sent);
    sorted
        .into_iter()
        .enumerate()
        .map(|(idx, fixture)| fixture.build(idx as u32 + 1))
        .collect()
}

fn fixture_mailboxes() -> HashMap<MailboxKind, Vec<Message>> {
    let mut mailboxes = HashMap::new();

    mailboxes.insert(MailboxKind::Inbox, mailbox(&inbox_fixtures()));

    let sent = mailbox(&[
        Fixture {
            recipients: &["mira.tomassi@example.org", "anna@example.com"],
            ..Fixture::new(
                21,
                datetime!(2026-03-13 18:40:00 UTC),
                "rob@example.com",
                "Re: Release notes for 0.3",
                4_200,
                MessageStatus::Read,
            )
        },
        Fixture {
            recipients: &["billing@vendor.example"],
            ..Fixture::new(
                22,
                datetime!(2026-03-11 12:12:00 UTC),
                "rob@example.com",
                "Payment scheduled",
                1_900,
                MessageStatus::Read,
            )
        },
    ]);
    mailboxes.insert(MailboxKind::Sent, sent);

    let drafts = mailbox(&[Fixture {
        recipients: &["anna@example.com"],
        labels: &["Draft"],
        ..Fixture::new(
            31,
            datetime!(2026-03-14 09:12:00 UTC),
            "rob@example.com",
            "Thursday works for me",
            1_400,
            MessageStatus::New,
        )
    }]);
    mailboxes.insert(MailboxKind::Drafts, drafts);

    let starred: Vec<Fixture> = inbox_fixtures()
        .into_iter()
        .filter(|fixture| fixture.starred)
        .collect();
    mailboxes.insert(MailboxKind::Starred, mailbox(&starred));

    mailboxes
}

/// Bodies for the messages a test opens, keyed by message id.
///
/// Only the ones a snapshot actually opens are written out; everything else
/// falls back to the "no such message" error, which is itself a state worth
/// being able to show.
fn fixture_contents() -> HashMap<MessageId, MessageContent> {
    let mut contents = HashMap::new();

    contents.insert(
        1,
        MessageContent {
            mailer: "VendorBilling/2.1".to_string(),
            parts: vec![
                MessageContentPart {
                    content_type: "text/plain".to_string(),
                    content: b"Invoice 2026-0342 is attached. Payment is due within 14 days."
                        .to_vec(),
                },
                MessageContentPart {
                    content_type: "text/html".to_string(),
                    content: br#"<html><body>
<h1>Invoice 2026-0342</h1>
<p>Dear customer,</p>
<p>your invoice for <b>February 2026</b> is attached as a PDF. The total is
<b>1,248.00 EUR</b>, payable within 14 days.</p>
<ul>
<li>Hosting, February 2026 &mdash; 899.00 EUR</li>
<li>Support plan &mdash; 349.00 EUR</li>
</ul>
<p>Kind regards,<br>Vendor Billing</p>
</body></html>"#
                        .to_vec(),
                },
            ],
            attachments: vec![
                MessageAttachment {
                    filename: Some("invoice-2026-0342.pdf".to_string()),
                    mime_type: "application/pdf".to_string(),
                    size: 14_320,
                    data: Some(b"%PDF-1.4 fixture".to_vec()),
                    blob_id: None,
                    inline: false,
                },
                MessageAttachment {
                    filename: Some("terms.txt".to_string()),
                    mime_type: "text/plain".to_string(),
                    size: 1_100,
                    data: Some(b"Payment terms: 14 days net.".to_vec()),
                    blob_id: None,
                    inline: false,
                },
                MessageAttachment {
                    filename: Some("logo.png".to_string()),
                    mime_type: "image/png".to_string(),
                    size: 2_400,
                    data: Some(b"\x89PNG fixture".to_vec()),
                    blob_id: None,
                    inline: true,
                },
            ],
        },
    );

    contents.insert(
        2,
        MessageContent {
            mailer: "Thunderbird/128.0".to_string(),
            parts: vec![
                MessageContentPart {
                    content_type: "text/plain".to_string(),
                    content: b"Looks good to me. One question about the changelog.".to_vec(),
                },
                MessageContentPart {
                    content_type: "text/html".to_string(),
                    content: br#"<html><body>
<p>Hi Rob,</p>
<p>the release notes look good to me. One question though: should the
<i>attachment indicator</i> be listed under features or under fixes?</p>
<blockquote><p>&gt; The list now marks messages that carry an attachment.</p></blockquote>
<p>Either way, ship it.</p>
<p>&mdash; Mira</p>
</body></html>"#
                        .to_vec(),
                },
            ],
            attachments: Vec::new(),
        },
    );

    contents.insert(
        31,
        MessageContent {
            mailer: "Elma".to_string(),
            parts: vec![
                MessageContentPart {
                    content_type: "text/plain".to_string(),
                    content: b"Thursday works for me. Shall we say half past twelve?".to_vec(),
                },
                MessageContentPart {
                    content_type: "text/html".to_string(),
                    content: br#"<html><body><p>Thursday works for me. Shall we say half past
twelve?</p></body></html>"#
                        .to_vec(),
                },
            ],
            attachments: Vec::new(),
        },
    );

    contents.insert(
        4,
        MessageContent {
            mailer: "Apple Mail (16.0)".to_string(),
            parts: vec![MessageContentPart {
                content_type: "text/plain".to_string(),
                content: b"Are you free for lunch on Thursday? There is a new place by the river."
                    .to_vec(),
            }],
            attachments: Vec::new(),
        },
    );

    contents
}

#[path = "snapshot_tests.rs"]
mod snapshot_tests;
