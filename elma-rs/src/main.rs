mod app;
mod backend;
mod model;
mod ui;
mod viewer;

use crate::app::App;
use crate::backend::mock::MockBackend;
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, prelude::CrosstermBackend};
use std::io::{self, Stdout};
use std::time::Duration;

const TICK_RATE: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut demo_mode = false;

    while let Some(arg) = args.next() {
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

    let backend = if demo_mode {
        MockBackend::demo()
    } else {
        MockBackend::default()
    };

    let mut app = App::new(Box::new(backend)).context("failed to initialize application state")?;
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
