//! LazyForge binary: terminal setup, the real event loop, and teardown.
//!
//! Rendering lives in [`polyforge_tui::ui`], state transitions in
//! [`polyforge_tui::app`]. This file only wires crossterm input to the app
//! and guarantees terminal restoration on every exit path, including errors
//! mid-loop.

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use polyforge_tui::app::App;
use polyforge_tui::ui;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::ExecutableCommand;
use ratatui::Terminal;

/// How long `event::poll` waits before handing control back for a tick.
const POLL_TIMEOUT: Duration = Duration::from_millis(200);

fn main() -> ExitCode {
    let flag_path = parse_ledger_flag(std::env::args().skip(1));
    match run(flag_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lazyforge: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Simple two-arg walk: `--ledger PATH` selects the ledger; unknown flags are
/// ignored for now.
fn parse_ledger_flag(args: impl Iterator<Item = String>) -> Option<PathBuf> {
    let args: Vec<String> = args.collect();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--ledger" {
            return args.get(index + 1).map(PathBuf::from);
        }
        index += 1;
    }
    None
}

fn run(flag_path: Option<PathBuf>) -> io::Result<()> {
    let ledger_path = flag_path.unwrap_or_else(App::ledger_path_default);
    let mut app = App::load(ledger_path);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // The loop body runs to completion or returns an error; either way the
    // terminal is restored before the result propagates.
    let outcome = event_loop(&mut terminal, &mut app);
    restore(&mut terminal);
    outcome
}

/// Drive one frame per poll cycle: key events mutate state, poll timeouts
/// advance the toast clock, every iteration repaints.
fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(POLL_TIMEOUT)? {
            if let Event::Key(key) = event::read()? {
                // Release events would double-handle keys on platforms that
                // emit them; only presses drive transitions.
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        } else {
            // Poll timeout: advance the virtual toast clock, drop expired.
            app.toasts.retain_mut(|toast| toast.tick());
        }
    }
    Ok(())
}

/// Restore the terminal to cooked mode on ALL exit paths.
fn restore(terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}
