//! The scripted Elma demo, recorded as the `demo.gif` the README embeds:
//!
//! ```sh
//! cargo run --release --example demo --features recorder
//! ```
//!
//! Every step below is a real key press through the client's own event
//! handling, drawn by its own renderer, so the recording cannot drift away
//! from what Elma actually does -- if a view changes, the next recording shows
//! the change.  Set `ELMA_DEMO_FRAMES=<dir>` to dump each frame as a PNG while
//! working on the script.
//!
//! The story: a morning's mail is triaged -- two messages archived, two
//! deleted, one reported as spam -- and the scheduled actions are committed in
//! one go.  The invoice that is left gets opened and its PDF saved to disk.
//! Then the colleague who asked for a document gets her answer, with the
//! document attached; sending it drops back to the list, where her message is
//! archived and that action committed too.

// Elma is a binary crate, so an example cannot import its modules: it compiles
// them in instead, which is also why most of them end up unused here.
#![allow(dead_code)]

#[path = "../../src/app.rs"]
mod app;
#[path = "../../src/backend/mod.rs"]
mod backend;
#[path = "../../src/clock.rs"]
mod clock;
#[path = "../../src/model.rs"]
mod model;
#[path = "../../src/test_harness.rs"]
mod test_harness;
#[path = "../../src/ui/mod.rs"]
mod ui;
#[path = "../../src/viewer.rs"]
mod viewer;

mod mailbox;
mod recorder;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};

use crate::app::AccountDescriptor;
use crate::mailbox::DemoBackend;
use crate::recorder::Recorder;
use crate::test_harness::TestApp;

/// The recorded terminal.  Wide enough for the message list's five columns and
/// for the compose dialog to sit inside a margin rather than filling the
/// screen, and tall enough that the triaged mail all fits without scrolling.
const COLUMNS: u16 = 100;
const ROWS: u16 = 30;

/// How long the result of an interaction stays on screen, by the kind of
/// interaction that produced it, in 10ms steps (the GIF delay unit).
mod pace {
    /// Between typed characters.
    pub const TYPE: u32 = 4;
    /// Moving the selection through the message list.
    pub const MOVE: u32 = 26;
    /// Letting a scheduled action's flag register.
    pub const ACTION: u32 = 55;
    /// Stepping through the compose dialog's fields and buttons.
    pub const STEP: u32 = 35;
    /// A moment of reading before moving on.
    pub const READ: u32 = 90;
    /// Something worth taking in properly: an opened message, a sent reply.
    pub const STUDY: u32 = 150;
    /// The opening frame.
    pub const FIRST: u32 = 110;
    /// The closing frame, before the GIF loops.
    pub const LAST: u32 = 300;
}

/// The demo's vocabulary: the keys a user would press, with human pacing.
struct Demo {
    recorder: Recorder,
}

impl Demo {
    fn start() -> Self {
        let account = AccountDescriptor::new("Personal", Arc::new(DemoBackend::new()));
        let app = TestApp::new(COLUMNS, ROWS, vec![account]);
        let mut recorder = Recorder::start(app);
        recorder.hold(pace::FIRST);
        Self { recorder }
    }

    fn press(&mut self, code: KeyCode, delay: u32) {
        self.recorder.press(code, KeyModifiers::NONE, delay);
    }

    /// Move the selection up `count` messages.
    fn up(&mut self, count: usize) {
        for _ in 0..count {
            self.press(KeyCode::Up, pace::MOVE);
        }
    }

    /// Press an action key -- `y`, `d`, `!`, `$`, `r` -- and let its effect
    /// register.
    fn action(&mut self, key: char) {
        self.press(KeyCode::Char(key), pace::ACTION);
    }

    fn tab(&mut self, count: usize) {
        for _ in 0..count {
            self.press(KeyCode::Tab, pace::STEP);
        }
    }

    fn enter(&mut self, delay: u32) {
        self.press(KeyCode::Enter, delay);
    }

    /// Type text character by character.
    fn write(&mut self, text: &str) {
        for ch in text.chars() {
            self.press(KeyCode::Char(ch), pace::TYPE);
        }
    }

    /// Leave the current frame on screen a while longer.
    fn pause(&mut self, cs: u32) {
        self.recorder.hold(cs);
    }

    /// Stop unless the client is where the script thinks it is.
    ///
    /// A demo is a user who cannot look at the screen: one key press landing
    /// somewhere unintended -- a Tab too many, and Enter opens `$EDITOR`
    /// instead of the attach prompt -- otherwise records a minute of the
    /// client doing something else entirely, and the recording still comes out
    /// looking plausible.  These checks are the looking.
    fn expect(&self, needle: &str) {
        assert!(
            self.recorder
                .screen()
                .iter()
                .any(|line| line.contains(needle)),
            "the demo went off script: expected {needle:?} on screen, got\n{}",
            self.recorder.screen().join("\n"),
        );
    }

    fn finish(self, path: &Path) -> Result<()> {
        self.recorder.finish(path)
    }
}

fn main() -> Result<()> {
    // Resolved before the working directory moves, below.
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("demo.gif");

    // The demo writes a file to disk and reads another one back, and both
    // paths end up on screen. Working from a scratch directory keeps them
    // short -- a plain file name and a plain `Downloads`, rather than a
    // temporary path nobody wants to read in a GIF -- and keeps the recording
    // from touching anything of the user's.
    let scratch = tempfile::tempdir().context("create the scratch directory")?;
    std::fs::write(
        scratch.path().join(mailbox::REQUESTED_DOCUMENT),
        vec![b'%'; 24_000],
    )
    .context("write the document the demo attaches")?;
    std::env::set_current_dir(scratch.path()).context("enter the scratch directory")?;
    // SAFETY: single-threaded still -- the client's worker threads start with
    // the app, below.
    unsafe { std::env::set_var("XDG_DOWNLOAD_DIR", "Downloads") };

    let mut demo = Demo::start();
    demo.expect("Invoice 2026-0342 for February");
    demo.pause(pace::READ);

    // The client opens on the newest message, at the bottom. Jump back up a
    // page, to the oldest thing that still needs dealing with: from here every
    // action moves the selection on by itself, so the triage runs downwards
    // without a single cursor key.
    demo.press(KeyCode::PageUp, pace::MOVE);
    demo.pause(pace::READ);

    demo.action('d'); // the newsletter has been read
    demo.action('!'); // the shop's "last chance" is spam
    demo.action('y'); // the sprint invitation is worth keeping, elsewhere
    demo.action('y'); // and so is the hotel booking
    demo.action('d'); // the support ticket is closed
    demo.expect("5 scheduled actions");
    demo.pause(pace::STUDY);

    // Nothing has left the client yet: one keystroke commits the lot, and the
    // five messages disappear from the inbox at once.
    demo.action('$');
    demo.expect("0 scheduled actions");
    demo.pause(pace::STUDY);

    // The triage came to rest on the invoice, which is what is left to deal
    // with. Open it and read it.
    demo.enter(pace::STUDY);
    demo.expect("Invoice 2026-0342");
    demo.press(KeyCode::PageDown, pace::READ);
    demo.pause(pace::READ);

    // Keep the PDF: `S` lists what the message carries, Enter writes the
    // selected one into the download folder.
    demo.action('S');
    demo.expect("Save attachment");
    demo.pause(pace::READ);
    demo.enter(pace::STUDY);
    demo.expect("Saved to");
    demo.pause(pace::READ);
    demo.press(KeyCode::Esc, pace::STEP);
    demo.press(KeyCode::Char('q'), pace::MOVE);

    // A colleague is waiting for a document. Open what she asked for.
    demo.press(KeyCode::Up, pace::MOVE);
    demo.enter(pace::STUDY);
    demo.expect("countersigned vendor agreement");
    demo.pause(pace::READ);

    // Answer her: the reply opens with the recipient, the subject and the
    // quoted original in place, and the focus on the body.
    demo.action('r');
    demo.expect("Re: Do you have the signed vendor agreement?");
    demo.pause(pace::READ);

    // One Tab on from the body is the Attach button; the document she is
    // asking for goes with the reply.
    demo.tab(1);
    demo.enter(pace::STEP);
    demo.expect("Path:");
    demo.write(mailbox::REQUESTED_DOCUMENT);
    demo.enter(pace::STUDY);
    demo.expect(&format!("Attached '{}'", mailbox::REQUESTED_DOCUMENT));

    // Tab on past Cancel, Edit message and Draft to Send, and off it goes.
    demo.tab(4);
    demo.enter(pace::STUDY);
    demo.expect("Message sent.");
    demo.pause(pace::READ);

    // Sending drops back to the list, on the message that was answered: it is
    // dealt with now, so archive it and commit that too.
    demo.action('y');
    demo.expect("1 scheduled actions");
    demo.pause(pace::READ);
    demo.action('$');
    demo.expect("0 scheduled actions");
    demo.pause(pace::LAST);

    demo.finish(&output)
}
