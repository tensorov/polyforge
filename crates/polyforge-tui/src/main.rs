//! LazyForge binary: terminal setup, the real event loop, and teardown.
//!
//! Rendering lives in [`polyforge_tui::ui`], state transitions in
//! [`polyforge_tui::app`]. This file only wires crossterm input to the app
//! and guarantees terminal restoration on every exit path, including errors
//! mid-loop and panics anywhere in the process.

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use polyforge_tui::app::{App, MouseDirection};
use polyforge_tui::ui;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, MouseEventKind,
};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::ExecutableCommand;
use ratatui::Terminal;

/// How long `event::poll` waits before handing control back for a tick.
const POLL_TIMEOUT: Duration = Duration::from_millis(200);

fn main() -> ExitCode {
    // Installed before any terminal setup so a panic mid-setup or mid-loop
    // still hands the terminal back instead of leaving the shell raw.
    std::panic::set_hook(Box::new(|info| {
        restore_terminal();
        eprintln!("{info}");
    }));

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
    stdout.execute(EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // The loop body runs to completion or returns an error; either way the
    // terminal is restored before the result propagates. The panic hook
    // covers the remaining exit path.
    let outcome = event_loop(&mut terminal, &mut app);
    restore_terminal();
    outcome
}

/// Drive one frame per poll cycle: key and mouse events mutate state, poll
/// timeouts advance the toast clock, every iteration repaints.
fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(POLL_TIMEOUT)? {
            match event::read()? {
                Event::Key(key) => app.handle_key_event(key),
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown => {
                        app.handle_mouse_scroll(MouseDirection::Down, app.pane);
                    }
                    MouseEventKind::ScrollUp => {
                        app.handle_mouse_scroll(MouseDirection::Up, app.pane);
                    }
                    _ => {}
                },
                // Resize repaints on the next draw; focus flips and
                // bracketed paste carry no state this console tracks.
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        } else {
            // Poll timeout: advance the virtual toast clock, drop expired.
            app.toasts.retain_mut(|toast| toast.tick());
        }
    }
    Ok(())
}

/// Restore the terminal to cooked mode on ALL exit paths.
///
/// Idempotent: duplicate calls (clean exit plus the panic hook) degrade to
/// ignored errors instead of failing fatally.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(DisableMouseCapture);
    let _ = io::stdout().execute(LeaveAlternateScreen);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> impl Iterator<Item = String> {
        items
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parse_ledger_flag_extracts_the_path_after_the_flag() {
        assert_eq!(
            parse_ledger_flag(args(&["--ledger", "/tmp/x.jsonl"])),
            Some(PathBuf::from("/tmp/x.jsonl"))
        );
    }

    #[test]
    fn parse_ledger_flag_returns_none_without_the_flag() {
        assert_eq!(parse_ledger_flag(args(&[])), None);
    }

    #[test]
    fn parse_ledger_flag_skips_unknown_flags_before_the_match() {
        assert_eq!(
            parse_ledger_flag(args(&["--verbose", "--ledger", "/tmp/y.jsonl"])),
            Some(PathBuf::from("/tmp/y.jsonl"))
        );
    }

    #[test]
    fn parse_ledger_flag_with_no_value_yields_none() {
        assert_eq!(parse_ledger_flag(args(&["--ledger"])), None);
    }
}
