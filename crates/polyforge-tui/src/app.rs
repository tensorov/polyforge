//! Pure app-state machine for LazyForge.
//!
//! Decoupled from rendering: no ratatui `Frame` here, only key events
//! (`KeyCode` via ratatui's crossterm re-export) drive transitions. Every
//! fallible ledger read lands in `error_screen`; nothing panics.

use std::collections::BTreeMap;
use std::path::PathBuf;

use polyforge_core::gate::latest_state_per_task;
use polyforge_core::ledger::{Ledger, LedgerError};
use polyforge_core::{EvidenceState, GateError};
use ratatui::crossterm::event::KeyCode;

use crate::toast::Toast;

/// Toast lifetime in ticks; the render loop drives [`Toast::tick`].
const TOAST_TTL_TICKS: u32 = 30;

/// Mirrors the CLI resolution: `PF_LEDGER` when set and non-empty,
/// `.pf/ledger.jsonl` otherwise.
const DEFAULT_LEDGER: &str = ".pf/ledger.jsonl";

/// Which pane owns keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    List,
    Detail,
}

/// Application state: the task map, the selection, and the detail cache.
///
/// `Debug` is implemented manually: [`Toast`] carries no `Debug` impl and the
/// toast list renders as its visible count.
pub struct App {
    pub ledger_path: PathBuf,
    pub tasks: BTreeMap<String, EvidenceState>,
    /// (kind, state) per entry of the selected task, sequence order.
    pub entries_of_selected: Vec<(String, String)>,
    pub selected: usize,
    pub pane: Pane,
    pub toasts: Vec<Toast>,
    pub error_screen: Option<String>,
    pub should_quit: bool,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("ledger_path", &self.ledger_path)
            .field("tasks", &self.tasks)
            .field("entries_of_selected", &self.entries_of_selected)
            .field("selected", &self.selected)
            .field("pane", &self.pane)
            .field(
                "toasts_visible",
                &self.toasts.iter().filter(|t| t.visible()).count(),
            )
            .field("error_screen", &self.error_screen)
            .field("should_quit", &self.should_quit)
            .finish()
    }
}

impl App {
    /// Default ledger path, mirroring the CLI resolution exactly.
    pub fn ledger_path_default() -> PathBuf {
        match std::env::var("PF_LEDGER") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => PathBuf::from(DEFAULT_LEDGER),
        }
    }

    /// Open the ledger at `path` and compute the latest state per task.
    ///
    /// A corrupt or unreadable ledger never panics: the error text lands on
    /// the error screen and `tasks` stays empty (fail closed).
    pub fn load(path: impl Into<PathBuf>) -> App {
        let ledger_path = path.into();
        let ledger = Ledger::new(&ledger_path);
        let base = App {
            ledger_path,
            tasks: BTreeMap::new(),
            entries_of_selected: Vec::new(),
            selected: 0,
            pane: Pane::List,
            toasts: Vec::new(),
            error_screen: None,
            should_quit: false,
        };
        match latest_state_per_task(&ledger) {
            Ok(tasks) => App { tasks, ..base },
            Err(err) => App {
                error_screen: Some(format!("{err}")),
                ..base
            },
        }
    }

    /// Re-read the selected task's entries into `entries_of_selected`.
    ///
    /// Collects `(kind, state)` string pairs in sequence order. Any read or
    /// integrity failure moves the app onto the error screen instead of
    /// surfacing partial data.
    pub fn refresh_detail(&mut self) {
        if self.error_screen.is_some() {
            return;
        }
        let Some(task_id) = self.selected_task_id().map(str::to_string) else {
            return;
        };
        let ledger = Ledger::new(&self.ledger_path);
        let read: Result<Vec<polyforge_core::EvidenceEntry>, LedgerError> = ledger.iter_entries();
        let entries = match read {
            Ok(entries) => entries,
            Err(err) => {
                self.error_screen = Some(format!("{}", GateError::from(err)));
                return;
            }
        };
        self.entries_of_selected = entries
            .into_iter()
            .filter(|entry| payload_task(entry) == Some(task_id.as_str()))
            .map(|entry| {
                (
                    entry.kind.clone(),
                    payload_state(&entry).unwrap_or_default().to_string(),
                )
            })
            .collect();
    }

    /// Handle a key press. Movement is bounds-clamped; `Enter` toggles the
    /// pane; `q` quits; anything else is a no-op.
    pub fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                if self.pane == Pane::Detail {
                    self.refresh_detail();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.tasks.len().saturating_sub(1));
                if self.pane == Pane::Detail {
                    self.refresh_detail();
                }
            }
            KeyCode::Enter => {
                self.pane = match self.pane {
                    Pane::List => Pane::Detail,
                    Pane::Detail => Pane::List,
                };
                if self.pane == Pane::Detail {
                    self.refresh_detail();
                }
            }
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    /// Push a toast with the standard 30-tick lifetime.
    pub fn push_toast(&mut self, msg: impl Into<String>) {
        self.toasts.push(Toast::new(msg, TOAST_TTL_TICKS));
    }

    /// The task id under the current selection (`None` when empty).
    ///
    /// Public so the validation keybindings (T8b) can hand the selected task
    /// to [`crate::validate::validate_single`] / `validate_bulk`.
    pub fn selected_task_id(&self) -> Option<&str> {
        self.tasks.keys().nth(self.selected).map(String::as_str)
    }
}

/// Extract the task id from a ledger entry's payload, if present.
fn payload_task(entry: &polyforge_core::EvidenceEntry) -> Option<&str> {
    entry.payload.get("task_id")?.as_str()
}

/// Extract the tri-state verdict string from a ledger entry's payload.
fn payload_state(entry: &polyforge_core::EvidenceEntry) -> Option<&str> {
    entry.payload.get("state")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use polyforge_core::evidence::{promote, EvidenceEntry as TriStateEvidence};
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_ledger_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pf-tui-app-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{name}-{}-{n}.jsonl", std::process::id()))
    }

    fn claim(task: &str) -> TriStateEvidence {
        TriStateEvidence::new_claim(task, "abc123", "diff-1", "ts-1")
    }

    fn attestation(task: &str) -> TriStateEvidence {
        TriStateEvidence::tool_attestation(
            task,
            "abc123",
            "diff-1",
            "cargo-1.95.0",
            "env-x",
            "cargo test",
            0,
            "h1",
            "ts-2",
        )
    }

    fn validation(task: &str) -> TriStateEvidence {
        TriStateEvidence::validation(task, "abc123", "diff-1", "oracle", "all green", "ts-3")
    }

    /// Task A reaches Validated (claim + tool attestation + validation);
    /// task B stays ModelClaimed (bare claim).
    fn write_two_task_ledger(path: &Path) {
        let mut ledger = Ledger::new(path);
        let c = claim("task-a");
        let v = promote(&c, &attestation("task-a")).unwrap();
        let d = promote(&v, &validation("task-a")).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(v.to_ledger_entry()).unwrap();
        ledger.append(d.to_ledger_entry()).unwrap();
        ledger.append(claim("task-b").to_ledger_entry()).unwrap();
    }

    #[test]
    fn load_empty_ledger_yields_empty_tasks_without_error() {
        let path = tmp_ledger_path("empty");
        let app = App::load(&path);
        assert!(app.tasks.is_empty());
        assert!(app.error_screen.is_none());
        assert_eq!(app.selected, 0);
        assert_eq!(app.pane, Pane::List);
        assert!(!app.should_quit);
    }

    #[test]
    fn load_maps_tasks_and_movement_is_bounded_with_aliases() {
        let path = tmp_ledger_path("two-tasks");
        write_two_task_ledger(&path);

        let mut app = App::load(&path);
        assert!(app.error_screen.is_none());
        assert_eq!(app.tasks.len(), 2);
        assert_eq!(app.tasks.get("task-a"), Some(&EvidenceState::Validated));
        assert_eq!(app.tasks.get("task-b"), Some(&EvidenceState::ModelClaimed));

        // BTreeMap ordering: task-a first.
        assert_eq!(app.selected_task_id(), Some("task-a"));

        // Down / j move forward within bounds.
        app.handle_key(KeyCode::Down);
        assert_eq!(app.selected_task_id(), Some("task-b"));
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.selected_task_id(), Some("task-b"), "clamped at last");

        // Up / k move back; Up at 0 stays at 0.
        app.handle_key(KeyCode::Up);
        assert_eq!(app.selected_task_id(), Some("task-a"));
        app.handle_key(KeyCode::Char('k'));
        app.handle_key(KeyCode::Up);
        assert_eq!(app.selected_task_id(), Some("task-a"), "clamped at first");
    }

    #[test]
    fn enter_toggles_pane_both_ways() {
        let path = tmp_ledger_path("pane-toggle");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        assert_eq!(app.pane, Pane::List);
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.pane, Pane::Detail);
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.pane, Pane::List);
    }

    #[test]
    fn q_sets_should_quit_and_other_keys_are_noops() {
        let path = tmp_ledger_path("quit");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('x'));
        assert!(!app.should_quit);
        app.handle_key(KeyCode::Char('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn corrupt_ledger_fails_closed_onto_error_screen() {
        let path = tmp_ledger_path("corrupt");
        write_two_task_ledger(&path);
        std::fs::write(&path, b"not json at all\n").unwrap();

        let app = App::load(&path);
        let screen = app
            .error_screen
            .expect("corrupt ledger must set error screen");
        assert!(
            screen.contains("integrity") || !screen.is_empty(),
            "error text must surface, got: {screen}"
        );
        assert!(app.tasks.is_empty(), "no partial data may leak");
    }

    #[test]
    fn refresh_detail_collects_entries_for_the_selected_task() {
        let path = tmp_ledger_path("detail");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        // Selected = task-a: claim -> attestation -> validation, in order.
        app.refresh_detail();
        assert_eq!(
            app.entries_of_selected,
            vec![
                ("ModelClaim".to_string(), "ModelClaimed".to_string()),
                ("ToolAttestation".to_string(), "Verified".to_string()),
                ("Validation".to_string(), "Validated".to_string()),
            ]
        );

        // Move to task-b: one bare claim only.
        app.handle_key(KeyCode::Down);
        app.refresh_detail();
        assert_eq!(
            app.entries_of_selected,
            vec![("ModelClaim".to_string(), "ModelClaimed".to_string())]
        );
    }
}
