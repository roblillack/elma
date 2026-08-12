//! SVG snapshot tests covering the major views of the client.
//!
//! Each test drives the real [`App`](crate::app::App) through synthetic key
//! events against the fixture mailbox and snapshots the rendered terminal as
//! SVG (see [`crate::test_harness`]) -- twice: once as a dark terminal draws
//! it and once as a light one does, so a colour that only works against one of
//! the two backgrounds shows up as such.  Review changed snapshots with
//! `cargo insta review`; the `.snap.svg` files under `src/snapshots/` open
//! directly in a browser.

use std::io::Write as _;

use crossterm::event::{KeyCode, KeyModifiers};

use super::{FixtureBackend, TerminalTheme, TestApp, account};
use crate::app::LoadPhase;
use crate::model::MailboxKind;
use crate::ui;

/// Terminal geometry every snapshot is taken at: wide enough for the message
/// list's five columns, tall enough for the compose dialog to sit inside the
/// frame rather than filling it.
const WIDTH: u16 = 100;
const HEIGHT: u16 = 32;

/// Snapshot the current frame once per terminal theme, as
/// `<name>-dark.svg` and `<name>-light.svg`.
///
/// The two differ only in what the terminal paints where the client asked for
/// no colour of its own, which is exactly the question a snapshot can answer
/// and a running client cannot: whether the view still reads on a background
/// the developer is not looking at.
fn assert_svg(name: &str, app: &mut TestApp) {
    for theme in TerminalTheme::ALL {
        let svg = app.svg(theme);
        write_svg(name, theme, svg);
    }
}

/// Snapshot the current frame as the client draws it for a user who has asked
/// for no colour, once per terminal theme.
///
/// Both terminals again, and for the same reason twice over: the monochrome
/// theme's whole claim is that bold, faint and reverse video say what colour
/// says, on a background it never has to know.
fn assert_mono_svg(name: &str, app: &mut TestApp) {
    for theme in TerminalTheme::ALL {
        let svg = app.svg_in_palette(ui::Theme::Mono, theme);
        write_svg(name, theme, svg);
    }
}

fn write_svg(name: &str, theme: TerminalTheme, svg: String) {
    let mut settings = insta::Settings::clone_current();
    settings.set_prepend_module_to_snapshot(false);
    settings.set_omit_expression(true);
    settings.bind(|| {
        insta::assert_binary_snapshot!(
            format!("{name}-{}.svg", theme.name()).as_str(),
            svg.into_bytes()
        );
    });
}

/// The standard app: one account on the fixture inbox, fully loaded.
fn inbox() -> TestApp {
    TestApp::inbox(WIDTH, HEIGHT)
}

/// The standard app with the newest message open in the viewer.
fn message_view() -> TestApp {
    let mut app = inbox();
    app.key(KeyCode::Enter);
    app
}

/// Assert that some row of the frame contains `needle`.
fn assert_on_screen(app: &TestApp, needle: &str) {
    assert!(
        app.buffer_lines().iter().any(|line| line.contains(needle)),
        "{needle:?} is not on screen:\n{}",
        app.buffer_lines().join("\n")
    );
}

// -- Message list ----------------------------------------------------------

/// The list itself: flags, dates, senders, sizes, labels, and one row per
/// message state -- unread red, archived teal italics, deleted struck through,
/// spam magenta -- with the newest message selected.
#[test]
fn message_list() {
    let mut app = inbox();
    assert_on_screen(&app, "Invoice 2026-0342 for February");
    assert_svg("message_list", &mut app);
}

/// A list taller than the screen scrolls with the selection: it opens on the
/// newest message at the bottom, and Home brings the oldest into view -- which
/// is also where the date column drops the time for a year.
#[test]
fn message_list_scrolls_with_the_selection() {
    // Short enough that the eleven fixture messages do not fit at once.
    let mut app = TestApp::inbox(WIDTH, 12);
    assert_svg("message_list_scrolled_to_the_newest", &mut app);

    app.key(KeyCode::Home);
    assert_on_screen(&app, "2025]");
    assert_svg("message_list_scrolled_to_the_oldest", &mut app);
}

/// Scheduled actions do not leave for the server until `$`: until then the
/// rows show what would happen (`D`/`A` flags, struck-through text) and the
/// action bar offers the commit.
#[test]
fn message_list_with_scheduled_actions() {
    let mut app = inbox();
    app.key(KeyCode::Home);
    app.char('d'); // delete the oldest message; selection moves to the next
    app.char('y'); // archive that one
    app.char('!'); // and mark the one after it as spam
    assert_on_screen(&app, "3 scheduled actions");
    assert_svg("message_list_with_scheduled_actions", &mut app);
}

/// A mailbox whose headers are still arriving: the rows that have not landed
/// are placeholders, and the read-progress bar sits at the right of the action
/// bar.
#[test]
fn message_list_while_backfilling() {
    let mut app = TestApp::new(
        WIDTH,
        HEIGHT,
        vec![account(
            "Personal",
            FixtureBackend::backfilling(MailboxKind::Inbox, 18),
        )],
    );
    assert_on_screen(&app, "Loading message...");
    assert_svg("message_list_while_backfilling", &mut app);
}

/// Nothing to show, and nothing wrong -- distinct from a failed load.
#[test]
fn message_list_empty_mailbox() {
    let mut app = TestApp::new(
        WIDTH,
        HEIGHT,
        vec![account("Personal", FixtureBackend::empty())],
    );
    assert_svg("message_list_empty_mailbox", &mut app);
}

/// The Sent folder shows who a message went to where the inbox shows who it
/// came from.
#[test]
fn message_list_sent_shows_recipients() {
    let mut app = inbox();
    app.char('g');
    app.char('t');
    assert_on_screen(&app, "Sent");
    assert_svg("message_list_sent_shows_recipients", &mut app);
}

// -- Message view ----------------------------------------------------------

/// An HTML message rendered through the FTML viewer, under its metadata block
/// and attachment list.
#[test]
fn message_view_formatted() {
    let mut app = message_view();
    assert_on_screen(&app, "Invoice 2026-0342");
    assert_svg("message_view_formatted", &mut app);
}

/// `.` toggles to the HTML the server actually sent.
#[test]
fn message_view_raw_html() {
    let mut app = message_view();
    app.char('.');
    assert_on_screen(&app, "Showing raw HTML");
    assert_svg("message_view_raw_html", &mut app);
}

/// Scrolling past the metadata block onto the body and the part listing.
#[test]
fn message_view_scrolled() {
    let mut app = message_view();
    app.key(KeyCode::PageDown);
    app.key(KeyCode::PageDown);
    assert_svg("message_view_scrolled", &mut app);
}

/// The body has been asked for but has not arrived: the list stays up and the
/// info bar counts the wait next to a throbber.
#[test]
fn message_view_body_loading() {
    let mut app = inbox();
    app.key_without_settling(KeyCode::Enter);
    assert_on_screen(&app, "Loading 'Invoice 2026-0342");
    assert_svg("message_view_body_loading", &mut app);
    app.settle();
}

/// A message without attachments loses the `S:SaveAttachment` entry from the
/// action bar and the attachment list from the body.
#[test]
fn message_view_without_attachments() {
    let mut app = inbox();
    app.key(KeyCode::Up);
    app.key(KeyCode::Enter);
    assert_on_screen(&app, "Release notes for 0.3");
    assert_svg("message_view_without_attachments", &mut app);
}

/// `k` walks up the list from inside the viewer, opening the previous message
/// -- here one whose body the fixture backend has no content for, which is how
/// a failed body load reads.
#[test]
fn message_view_body_load_failed() {
    let mut app = message_view();
    app.char('k');
    app.char('k');
    assert_on_screen(&app, "Failed to load message");
    assert_svg("message_view_body_load_failed", &mut app);
}

/// The dialog that asks where an attachment should be written, listing every
/// file the message carries -- the inline logo included, which the list's `@`
/// marker deliberately ignores.
///
/// The folder it opens on is the one the machine running the tests would save
/// into, so the test types one of its own over it before drawing.
#[test]
fn save_attachment_dialog() {
    let mut app = message_view();
    app.key_with(KeyCode::Char('S'), KeyModifiers::SHIFT);
    assert_on_screen(&app, "Save attachment");

    app.key(KeyCode::Tab); // onto the folder field
    app.clear_field();
    app.type_text("/home/rob/Downloads");
    assert_svg("save_attachment_dialog_folder_focused", &mut app);

    app.key(KeyCode::Tab); // back onto the list, where the dialog opens
    assert_svg("save_attachment_dialog", &mut app);
}

// -- Composer --------------------------------------------------------------

/// A blank message: header fields, an empty body, the button row.
#[test]
fn compose_blank() {
    let mut app = inbox();
    app.char('c');
    assert_svg("compose_blank", &mut app);
}

/// A reply quotes the message and fills in recipient and subject.
#[test]
fn compose_reply() {
    let mut app = inbox();
    app.char('r');
    assert_on_screen(&app, "billing@vendor.example");
    assert_svg("compose_reply", &mut app);
}

/// Reply-to-all keeps everyone the message went to, in the Cc.
#[test]
fn compose_reply_all() {
    let mut app = inbox();
    app.char('a');
    assert_on_screen(&app, "Cc: rob@example.com, accounts@example.com");
    assert_svg("compose_reply_all", &mut app);
}

/// A forward carries the attachments over and leaves the recipient to fill in.
#[test]
fn compose_forward() {
    let mut app = inbox();
    app.char('f');
    assert_on_screen(&app, "invoice-2026-0342.pdf");
    assert_svg("compose_forward", &mut app);
}

/// Enter on a message in Drafts reopens it in the composer rather than in the
/// viewer, recipient, subject and body as they were saved.
#[test]
fn compose_editing_a_draft() {
    let mut app = inbox();
    app.char('g');
    app.char('d');
    app.key(KeyCode::Enter);
    assert_on_screen(&app, "Editing draft.");
    assert_svg("compose_editing_a_draft", &mut app);
}

/// Typing into the header fields, with the caret in the subject line.
#[test]
fn compose_filled_in() {
    let mut app = inbox();
    app.char('c');
    app.type_text("anna@example.com");
    app.key(KeyCode::Tab); // Cc
    app.key(KeyCode::Tab); // Bcc
    app.key(KeyCode::Tab); // Subject
    app.type_text("Thursday works");
    assert_svg("compose_filled_in", &mut app);
}

/// Tab from the To field to the Attach button.  Stopping anywhere short of it
/// matters: Enter on the body hands the message to `$EDITOR`.
fn focus_attach_button(app: &mut TestApp) {
    for _ in 0..5 {
        app.key(KeyCode::Tab); // Cc, Bcc, Subject, body, Attach
    }
}

/// The prompt that asks for a path to attach.
#[test]
fn compose_attachment_prompt() {
    let mut app = inbox();
    app.char('c');
    focus_attach_button(&mut app);
    app.key(KeyCode::Enter);
    assert_on_screen(&app, "Path:");
    assert_svg("compose_attachment_prompt", &mut app);
}

/// While the backend has the message, the composer is read-only: no lit
/// button, no caret, and the status line counts the wait.
#[test]
fn compose_sending() {
    let mut app = inbox();
    app.char('c');
    app.type_text("anna@example.com");
    for _ in 0..9 {
        app.key(KeyCode::Tab); // Cc, Bcc, Subject, body, then five buttons
    }
    app.key_without_settling(KeyCode::Enter);
    assert_on_screen(&app, "please wait");
    assert_svg("compose_sending", &mut app);
    app.settle();
}

/// A file that was attached shows up in its own pane above the body, with the
/// size the message will grow by.
#[test]
fn compose_with_attachment() {
    // A real file, because attaching reads one: written under a temporary
    // directory whose name never reaches the screen, only the file's does.
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("meeting-notes.txt");
    let mut file = std::fs::File::create(&path).expect("create the attachment");
    file.write_all(b"Agenda:\n- release 0.3\n- snapshot tests\n")
        .expect("write the attachment");

    let mut app = inbox();
    app.char('c');
    focus_attach_button(&mut app);
    app.key(KeyCode::Enter);
    app.type_text(path.to_str().expect("utf-8 path"));
    app.key(KeyCode::Enter);

    assert_on_screen(&app, "meeting-notes.txt");
    assert_svg("compose_with_attachment", &mut app);
}

// -- Search ----------------------------------------------------------------

/// `/` opens the search line at the bottom, with the caret in it and the list
/// unfiltered until something is typed.
#[test]
fn search_input_opened() {
    let mut app = inbox();
    app.char('/');
    assert_on_screen(&app, "Find:");
    assert_svg("search_input_opened", &mut app);
}

/// Typing filters the list as it goes, still with the input focused.
#[test]
fn search_filters_while_typing() {
    let mut app = inbox();
    app.char('/');
    app.type_text("example.org");
    assert_svg("search_filters_while_typing", &mut app);
}

/// Enter hands the list back the keyboard and the query moves into a panel in
/// the corner, which is what says the list is still filtered.
#[test]
fn search_results_confirmed() {
    let mut app = inbox();
    app.char('/');
    app.type_text("invoice");
    app.key(KeyCode::Enter);
    assert_on_screen(&app, "Showing results for:");
    assert_svg("search_results_confirmed", &mut app);
}

/// A query nothing matches empties the list rather than falling back to all of
/// it.
#[test]
fn search_without_matches() {
    let mut app = inbox();
    app.char('/');
    app.type_text("nothing matches this");
    app.key(KeyCode::Enter);
    assert_svg("search_without_matches", &mut app);
}

// -- Loading overlay -------------------------------------------------------

/// The cold start: the mailbox is empty because the backend has not answered
/// yet, and the overlay says so rather than showing an empty inbox.
#[test]
fn loading_overlay_connecting() {
    let (backend, gate) = FixtureBackend::blocking();
    let mut app = TestApp::starting(WIDTH, HEIGHT, vec![account("Personal", backend)]);
    assert_on_screen(&app, "Connecting to the mail server");
    assert_svg("loading_overlay_connecting", &mut app);

    // Let the load through so the worker thread is not left parked on the gate.
    gate.open();
    app.settle();
}

/// Headers arriving, with the count the overlay reports.
#[test]
fn loading_overlay_receiving() {
    let (backend, gate) = FixtureBackend::blocking();
    let mut app = TestApp::starting(WIDTH, HEIGHT, vec![account("Personal", backend)]);
    app.app_mut().set_load_phase(LoadPhase::Receiving {
        loaded: 40,
        total: 250,
    });
    app.draw();
    assert_on_screen(&app, "40 of 250 headers");
    assert_svg("loading_overlay_receiving", &mut app);

    gate.open();
    app.settle();
}

/// A load that failed keeps the overlay and puts the reason in it -- with an
/// empty list, it is the only place the whole sentence fits.
#[test]
fn loading_overlay_failed() {
    let mut app = TestApp::starting(
        WIDTH,
        HEIGHT,
        vec![account(
            "Personal",
            FixtureBackend::failing("login failed: the server rejected the app-specific password"),
        )],
    );
    app.settle();
    app.draw();
    assert_on_screen(&app, "Could not open this mailbox");
    assert_svg("loading_overlay_failed", &mut app);
}

// -- Choosers --------------------------------------------------------------

/// `g` lists the mailboxes and the key each is behind.
#[test]
fn mailbox_chooser() {
    let mut app = inbox();
    app.char('g');
    assert_on_screen(&app, "Go to");
    assert_svg("mailbox_chooser", &mut app);
}

/// `G` lists the configured accounts, marking the one being looked at.
#[test]
fn account_chooser() {
    let mut app = TestApp::new(
        WIDTH,
        HEIGHT,
        vec![
            account("Personal", FixtureBackend::new()),
            account("Work", FixtureBackend::new()),
            account("rob@fastmail.example", FixtureBackend::empty()),
        ],
    );
    app.char('G');
    assert_on_screen(&app, "(current)");
    assert_svg("account_chooser", &mut app);
}

/// Picking another account switches the list to its mailbox.
#[test]
fn account_switched() {
    let mut app = TestApp::new(
        WIDTH,
        HEIGHT,
        vec![
            account("Personal", FixtureBackend::new()),
            account("Work", FixtureBackend::empty()),
        ],
    );
    app.char('G');
    app.char('2');
    assert_on_screen(&app, "Work");
    assert_svg("account_switched", &mut app);
}

// -- Without colour --------------------------------------------------------

/// The list as `--color=never` would have it: the bars in reverse video, the
/// unread messages bold, the archived one in italics, the deleted one struck
/// through, and the labels told apart by which of them is a chip.
#[test]
fn mono_message_list() {
    let mut app = inbox();
    assert_mono_svg("mono_message_list", &mut app);
}

/// The bars with something on them: a mailbox still arriving puts the
/// read-progress indicator in the corner of a bar that is itself reverse video.
#[test]
fn mono_message_list_while_backfilling() {
    let mut app = TestApp::new(
        WIDTH,
        HEIGHT,
        vec![account(
            "Personal",
            FixtureBackend::backfilling(MailboxKind::Inbox, 18),
        )],
    );
    assert_on_screen(&app, "Loading message...");
    assert_mono_svg("mono_message_list_while_backfilling", &mut app);
}

/// Two popups, one on top of the other, without a colour between them: the
/// prompt taking keys is framed in heavy lines and the dialog underneath it
/// goes faint, name, fields, buttons and all.
#[test]
fn mono_compose_under_a_prompt() {
    let mut app = inbox();
    app.char('c');
    focus_attach_button(&mut app);
    app.key(KeyCode::Enter);
    assert_on_screen(&app, "Path:");
    assert_mono_svg("mono_compose_under_a_prompt", &mut app);
}

/// A search still in force, reported by a popup that has no keys: faint, in a
/// light frame, over a list that has them.
#[test]
fn mono_search_results() {
    let mut app = inbox();
    app.char('/');
    app.type_text("invoice");
    app.key(KeyCode::Enter);
    assert_on_screen(&app, "Showing results for:");
    assert_mono_svg("mono_search_results", &mut app);
}

/// A dialog with everything in it: a name, a field with the caret in it, a
/// list with a selection, and the keys along the bottom of the frame.
#[test]
fn mono_save_attachment_dialog() {
    let mut app = message_view();
    app.key_with(KeyCode::Char('S'), KeyModifiers::SHIFT);
    app.key(KeyCode::Tab); // onto the folder field
    app.clear_field();
    app.type_text("/home/rob/Downloads");
    assert_mono_svg("mono_save_attachment_dialog_folder_focused", &mut app);

    app.key(KeyCode::Tab); // back onto the list
    assert_mono_svg("mono_save_attachment_dialog", &mut app);
}

/// Not one colour anywhere in the interface, in any state of it.
///
/// The palette module holds itself to this, but it can only speak for the
/// styles it hands out: a call site that named a colour of its own would slip
/// past it, and would then be drawn against a terminal whose own two colours
/// the client has deliberately not asked about.
#[test]
fn nothing_in_the_monochrome_theme_names_a_colour() {
    let mut app = inbox();
    assert_colourless(&mut app, "the message list");

    app.char('g');
    assert_colourless(&mut app, "the mailbox chooser");
    app.key(KeyCode::Esc);

    app.char('/');
    app.type_text("invoice");
    assert_colourless(&mut app, "the search prompt");
    app.key(KeyCode::Enter);
    assert_colourless(&mut app, "a filtered list");
    app.key(KeyCode::Esc);

    app.char('c');
    focus_attach_button(&mut app);
    app.key(KeyCode::Enter);
    assert_colourless(&mut app, "the composer under a prompt");
    app.key(KeyCode::Esc);
    app.key(KeyCode::Esc);

    let mut app = message_view();
    assert_colourless(&mut app, "the message viewer");
    app.key_with(KeyCode::Char('S'), KeyModifiers::SHIFT);
    assert_colourless(&mut app, "the save dialog");

    // A load that failed and one still running: between them they cover the
    // error colour and the progress indicator.
    let mut app = TestApp::new(
        WIDTH,
        HEIGHT,
        vec![account(
            "Personal",
            FixtureBackend::failing("connection refused"),
        )],
    );
    assert_colourless(&mut app, "a failed load");

    let mut app = TestApp::new(
        WIDTH,
        HEIGHT,
        vec![account(
            "Personal",
            FixtureBackend::backfilling(MailboxKind::Inbox, 18),
        )],
    );
    assert_colourless(&mut app, "a mailbox still arriving");
}

/// Draw the current state in the monochrome palette and check that every cell
/// leaves its colours to the terminal.
fn assert_colourless(app: &mut TestApp, what: &str) {
    use ratatui::style::Color;

    app.draw_in(ui::Theme::Mono);
    for cell in app.buffer().content() {
        for (role, colour) in [("foreground", cell.fg), ("background", cell.bg)] {
            assert_eq!(
                colour,
                Color::Reset,
                "{what}: {:?} asks for a {role} of {colour:?}",
                cell.symbol()
            );
        }
    }
}
