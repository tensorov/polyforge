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
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::toast::Toast;
use crate::validate::{validate_bulk, validate_single};

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

/// Wheel direction for [`App::handle_mouse_scroll`]. A local enum keeps the
/// app state machine free of crossterm types beyond the key router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseDirection {
    Up,
    Down,
}

/// A validation action awaiting operator confirmation.
///
/// While a modal is open it owns keyboard focus: only `Enter` (execute) and
/// `Esc` (cancel) reach it; navigation keys are swallowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingConfirm {
    /// Validate the one task under the selection (`v`).
    Single { task_id: String },
    /// Validate every currently `Verified` task at once (`A`).
    Bulk { tasks: Vec<String> },
}

/// What free-form input mode is capturing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputPurpose {
    /// Rationale capture for a pending validation (`r`).
    #[default]
    Rationale,
    /// Task-list substring filter (`/`).
    Filter,
}

/// Free-form text capture: printable keystrokes land in
/// [`InputMode::buffer`] until `Enter` commits or `Esc` discards them.
/// The [`InputPurpose`] decides what a committed buffer means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputMode {
    pub buffer: String,
    pub purpose: InputPurpose,
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
    /// Confirmation modal state; while set it owns keyboard focus.
    pub pending_confirm: Option<PendingConfirm>,
    /// Rationale input mode; while set, printable keys edit its buffer.
    pub input_mode: Option<InputMode>,
    /// Rationale committed via `Enter` in input mode; consumed by
    /// single-task validation.
    pub pending_rationale: Option<String>,
    /// Case-insensitive substring filter over task ids (`None` shows all).
    pub filter: Option<String>,
    /// Full-screen keymap overlay (`?` toggles).
    pub show_help: bool,
    /// Chain tail hash from the last ledger read (empty when unreadable).
    pub tail_hash: String,
    /// Scroll position of the detail pane content; list scrolling moves
    /// `selected` instead.
    pub detail_offset: usize,
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
            .field("pending_confirm", &self.pending_confirm)
            .field("input_mode", &self.input_mode)
            .field("pending_rationale", &self.pending_rationale)
            .field("filter", &self.filter)
            .field("show_help", &self.show_help)
            .field("tail_hash", &self.tail_hash)
            .field("detail_offset", &self.detail_offset)
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
            pending_confirm: None,
            input_mode: None,
            pending_rationale: None,
            filter: None,
            show_help: false,
            tail_hash: read_tail_hash(&ledger),
            detail_offset: 0,
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

    /// Re-read the ledger's latest state per task into `tasks`.
    ///
    /// Mirrors [`App::load`]'s read path without rebuilding the app: the
    /// selection is clamped to the new task count so a shrinking map cannot
    /// leave a dangling index. Integrity failures land on the error screen.
    pub fn reload_tasks(&mut self) {
        if self.error_screen.is_some() {
            return;
        }
        let ledger = Ledger::new(&self.ledger_path);
        self.tail_hash = read_tail_hash(&ledger);
        match latest_state_per_task(&ledger) {
            Ok(tasks) => {
                self.tasks = tasks;
                let visible_len = self.visible_tasks().len();
                self.selected = self.selected.min(visible_len.saturating_sub(1));
            }
            Err(err) => self.error_screen = Some(format!("{err}")),
        }
    }

    /// Handle a bare key code through the router.
    ///
    /// Thin wrapper over [`App::handle_key_event`] with empty modifiers, kept
    /// so existing callers and tests keep compiling.
    pub fn handle_key(&mut self, key: KeyCode) {
        self.handle_key_event(KeyEvent::new(key, KeyModifiers::empty()));
    }

    /// Handle a full key event: press-only filtering plus the Ctrl-C quit
    /// shortcut, then the same router as [`App::handle_key`].
    ///
    /// Priority: Ctrl-C > confirmation modal > input mode > help overlay >
    /// normal navigation. Ctrl-C quits from the normal view and through the
    /// help overlay; while a modal or input mode owns focus their handlers
    /// keep the keystroke.
    pub fn handle_key_event(&mut self, key: KeyEvent) {
        // Release events would double-handle keys on platforms that emit
        // them; only presses drive transitions.
        if key.kind != KeyEventKind::Press {
            return;
        }
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_c && self.pending_confirm.is_none() && self.input_mode.is_none() {
            self.should_quit = true;
            return;
        }
        self.route_key(key.code);
    }

    fn route_key(&mut self, key: KeyCode) {
        if self.pending_confirm.is_some() {
            self.handle_modal_key(key);
        } else if self.input_mode.is_some() {
            self.handle_input_key(key);
        } else if self.show_help {
            match key {
                KeyCode::Char('?') => self.show_help = false,
                KeyCode::Char('q') => self.should_quit = true,
                _ => {}
            }
        } else {
            self.handle_normal_key(key);
        }
    }

    /// Modal routing: `Enter` executes the pending action, `Esc` cancels,
    /// everything else is swallowed.
    fn handle_modal_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Enter => {
                if let Some(pending) = self.pending_confirm.take() {
                    match pending {
                        PendingConfirm::Single { task_id } => self.execute_single(&task_id),
                        PendingConfirm::Bulk { tasks } => self.execute_bulk(&tasks),
                    }
                }
            }
            KeyCode::Esc => self.pending_confirm = None,
            _ => {}
        }
    }

    /// Input-mode routing: printable characters and Backspace edit the
    /// buffer, `Enter` commits it per [`InputPurpose`], `Esc` discards.
    fn handle_input_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char(c) => {
                if let Some(mode) = self.input_mode.as_mut() {
                    mode.buffer.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(mode) = self.input_mode.as_mut() {
                    mode.buffer.pop();
                }
            }
            KeyCode::Enter => {
                let mode = self.input_mode.take().unwrap_or_default();
                match mode.purpose {
                    InputPurpose::Rationale => self.pending_rationale = Some(mode.buffer),
                    InputPurpose::Filter => self.commit_filter(mode.buffer),
                }
            }
            KeyCode::Esc => self.input_mode = None,
            _ => {}
        }
    }

    /// Commit a filter buffer: trimmed and lowercased; an empty result
    /// clears the filter. The selection resets into the filtered view.
    fn commit_filter(&mut self, buffer: String) {
        let trimmed = buffer.trim().to_lowercase();
        self.filter = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        self.selected = 0;
        self.refresh_detail();
    }

    /// Normal routing: navigation plus the validation entry points.
    fn handle_normal_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                if self.pane == Pane::Detail {
                    self.refresh_detail();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let last = self.visible_tasks().len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
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
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('/') => {
                self.input_mode = Some(InputMode {
                    buffer: String::new(),
                    purpose: InputPurpose::Filter,
                });
            }
            KeyCode::Char('v') => {
                if let Some(task_id) = self.selected_task_id().map(str::to_string) {
                    self.pending_confirm = Some(PendingConfirm::Single { task_id });
                }
            }
            KeyCode::Char('A') => {
                let verified: Vec<String> = self
                    .visible_tasks()
                    .into_iter()
                    .filter(|&(_, state)| *state == EvidenceState::Verified)
                    .map(|(task_id, _)| task_id.clone())
                    .collect();
                if verified.is_empty() {
                    self.push_toast("nothing to validate");
                } else {
                    self.pending_confirm = Some(PendingConfirm::Bulk { tasks: verified });
                }
            }
            KeyCode::Char('r') => self.input_mode = Some(InputMode::default()),
            _ => {}
        }
    }

    /// Run single-task validation for the confirmed modal.
    ///
    /// Consumes any committed rationale (falling back to the default
    /// wording), turns the outcome into a toast, and refreshes the task map
    /// plus detail cache. Engine errors move onto the error screen.
    fn execute_single(&mut self, task_id: &str) {
        let rationale = self
            .pending_rationale
            .take()
            .unwrap_or_else(|| "lazyforge operator validation".to_string());
        match validate_single(&self.ledger_path, task_id, &rationale) {
            Ok(outcome) => {
                if outcome.appended {
                    self.push_toast(format!("validated {task_id}"));
                } else if let Some(skip) = outcome.skip {
                    self.push_toast(format!("{task_id}: {}", skip.message()));
                }
            }
            Err(err) => self.error_screen = Some(err),
        }
        self.reload_tasks();
        self.refresh_detail();
    }

    /// Run bulk validation over the confirmed task list.
    ///
    /// The summary toast carries appended and skipped counts; per-task skip
    /// reasons stay in the engine report. Same refresh contract as
    /// [`App::execute_single`].
    fn execute_bulk(&mut self, tasks: &[String]) {
        const BULK_RATIONALE: &str = "lazyforge bulk validate";
        match validate_bulk(&self.ledger_path, tasks, BULK_RATIONALE) {
            Ok(report) => self.push_toast(format!(
                "done {}, skipped {}",
                report.appended,
                report.skipped.len()
            )),
            Err(err) => self.error_screen = Some(err),
        }
        self.reload_tasks();
        self.refresh_detail();
    }

    /// Handle a mouse wheel scroll over `pane`.
    ///
    /// List scrolling moves the selection exactly like the arrow keys;
    /// detail scrolling moves [`App::detail_offset`] three rows per notch,
    /// saturating at zero.
    pub fn handle_mouse_scroll(&mut self, direction: MouseDirection, pane: Pane) {
        match (pane, direction) {
            (Pane::List, MouseDirection::Down) => self.route_key(KeyCode::Down),
            (Pane::List, MouseDirection::Up) => self.route_key(KeyCode::Up),
            (Pane::Detail, MouseDirection::Down) => {
                self.detail_offset = self.detail_offset.saturating_add(3);
            }
            (Pane::Detail, MouseDirection::Up) => {
                self.detail_offset = self.detail_offset.saturating_sub(3);
            }
        }
    }

    /// Bound [`App::detail_offset`] to a content length so a shrunken
    /// ledger cannot leave the pane scrolled past its last line. Mirrors
    /// the render-time clamp in `ui::draw_detail`.
    pub fn clamp_offsets(&mut self, detail_len: usize) {
        self.detail_offset = self.detail_offset.min(detail_len.saturating_sub(1));
    }

    /// Push a toast with the standard 30-tick lifetime.
    pub fn push_toast(&mut self, msg: impl Into<String>) {
        self.toasts.push(Toast::new(msg, TOAST_TTL_TICKS));
    }

    /// The task id under the current selection (`None` when empty).
    ///
    /// Selection indexes into the filtered view, so the id comes from
    /// [`App::visible_tasks`], not the raw task map.
    pub fn selected_task_id(&self) -> Option<&str> {
        self.visible_tasks()
            .get(self.selected)
            .map(|(task_id, _)| task_id.as_str())
    }

    /// Tasks surviving the active filter, in BTreeMap order.
    ///
    /// `None` filter shows everything; a set filter matches case-insensitive
    /// substrings of the task id. All selection semantics index into this
    /// view.
    pub fn visible_tasks(&self) -> Vec<(&String, &EvidenceState)> {
        let Some(filter) = self.filter.as_deref() else {
            return self.tasks.iter().collect();
        };
        let needle = filter.to_lowercase();
        self.tasks
            .iter()
            .filter(|(task_id, _)| task_id.to_lowercase().contains(&needle))
            .collect()
    }
}

/// Chain tail hash for the status bar; empty when the chain cannot be read
/// (an empty ledger has no head yet).
fn read_tail_hash(ledger: &Ledger) -> String {
    ledger
        .verify_chain()
        .map(|c| c.head_hash)
        .unwrap_or_default()
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

    /// Seed claim + tool attestation: the task ends `Verified`.
    fn seed_verified(path: &Path, task: &str) {
        let mut ledger = Ledger::new(path);
        let c = claim(task);
        let v = promote(&c, &attestation(task)).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(v.to_ledger_entry()).unwrap();
    }

    fn entry_count(path: &Path) -> usize {
        Ledger::new(path).iter_entries().unwrap().len()
    }

    #[test]
    fn v_on_selected_verified_task_opens_single_modal() {
        let path = tmp_ledger_path("v-single");
        seed_verified(&path, "task-v");
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('v'));
        assert_eq!(
            app.pending_confirm,
            Some(PendingConfirm::Single {
                task_id: "task-v".to_string()
            })
        );
    }

    #[test]
    fn bulk_all_with_no_verified_tasks_toasts_and_stays_closed() {
        let path = tmp_ledger_path("a-empty");
        // task-a Validated + task-b ModelClaimed: zero Verified tasks.
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('A'));
        assert!(app.pending_confirm.is_none());
        assert!(
            app.toasts
                .iter()
                .any(|t| t.message() == "nothing to validate"),
            "expected the nothing-to-validate toast"
        );
    }

    #[test]
    fn bulk_all_collects_verified_tasks_in_map_order() {
        let path = tmp_ledger_path("a-bulk");
        seed_verified(&path, "t-1");
        seed_verified(&path, "t-2");
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('A'));
        assert_eq!(
            app.pending_confirm,
            Some(PendingConfirm::Bulk {
                tasks: vec!["t-1".to_string(), "t-2".to_string()]
            })
        );
    }

    #[test]
    fn modal_enter_executes_single_validation() {
        let path = tmp_ledger_path("enter-exec");
        seed_verified(&path, "task-x");
        let before = entry_count(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('v'));
        app.handle_key(KeyCode::Enter);

        let entries = Ledger::new(&path).iter_entries().unwrap();
        assert_eq!(entries.len(), before + 1, "exactly one new entry");
        let last = entries.last().unwrap();
        assert_eq!(last.kind, "Validation");
        assert_eq!(last.payload["state"], "Validated");
        assert_eq!(
            last.payload["rationale"], "lazyforge operator validation",
            "default rationale applies when none was committed"
        );
        assert!(
            app.toasts.iter().any(|t| t.message().contains("validated")),
            "expected a validated toast, got {:?}",
            app.toasts.iter().map(|t| t.message()).collect::<Vec<_>>()
        );
        assert!(app.pending_confirm.is_none());
    }

    #[test]
    fn modal_esc_cancels_without_touching_the_ledger() {
        let path = tmp_ledger_path("esc-cancel");
        seed_verified(&path, "task-y");
        let before = entry_count(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('v'));
        app.handle_key(KeyCode::Esc);

        assert!(app.pending_confirm.is_none());
        assert_eq!(entry_count(&path), before, "cancel writes nothing");
    }

    #[test]
    fn input_mode_captures_rationale_and_esc_discards() {
        let path = tmp_ledger_path("input-mode");
        seed_verified(&path, "task-i");
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('r'));
        assert!(app.input_mode.is_some());
        app.handle_key(KeyCode::Char('o'));
        app.handle_key(KeyCode::Char('k'));
        assert_eq!(
            app.input_mode.as_ref().map(|m| m.buffer.as_str()),
            Some("ok")
        );
        app.handle_key(KeyCode::Backspace);
        assert_eq!(
            app.input_mode.as_ref().map(|m| m.buffer.as_str()),
            Some("o"),
            "backspace pops the buffer"
        );
        app.handle_key(KeyCode::Char('k'));

        app.handle_key(KeyCode::Enter);
        assert!(app.input_mode.is_none());
        assert_eq!(app.pending_rationale.as_deref(), Some("ok"));

        // A fresh capture discarded by Esc leaves the earlier commit intact.
        app.handle_key(KeyCode::Char('r'));
        app.handle_key(KeyCode::Char('x'));
        app.handle_key(KeyCode::Esc);
        assert!(app.input_mode.is_none());
        assert_eq!(app.pending_rationale.as_deref(), Some("ok"));
    }

    #[test]
    fn modal_open_swallows_navigation_keys() {
        let path = tmp_ledger_path("modal-no-leak");
        seed_verified(&path, "t-1");
        seed_verified(&path, "t-2");
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('v'));
        assert!(app.pending_confirm.is_some(), "modal must be open");

        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Down);
        app.handle_key(KeyCode::Char('k'));
        app.handle_key(KeyCode::Up);
        assert_eq!(
            app.selected, 0,
            "selection must not move while the modal is open"
        );
        assert_eq!(
            app.pending_confirm,
            Some(PendingConfirm::Single {
                task_id: "t-1".to_string()
            }),
            "modal stays open and unchanged"
        );
    }

    #[test]
    fn execute_single_reloads_tasks_map_to_validated() {
        let path = tmp_ledger_path("reload-validated");
        seed_verified(&path, "task-r");
        let mut app = App::load(&path);
        assert_eq!(app.tasks.get("task-r"), Some(&EvidenceState::Verified));

        app.handle_key(KeyCode::Char('v'));
        app.handle_key(KeyCode::Enter);

        assert_eq!(
            app.tasks.get("task-r"),
            Some(&EvidenceState::Validated),
            "tasks map must reflect the appended validation"
        );
    }

    #[test]
    fn filter_commit_is_case_insensitive_and_clamps_selection() {
        let path = tmp_ledger_path("filter-commit");
        seed_verified(&path, "alpha");
        seed_verified(&path, "beta");
        let mut app = App::load(&path);

        // Park the selection on beta so the commit must reset it.
        app.handle_key(KeyCode::Down);
        app.handle_key(KeyCode::Down);
        assert_eq!(app.selected_task_id(), Some("beta"));

        app.handle_key(KeyCode::Char('/'));
        assert_eq!(
            app.input_mode.as_ref().map(|m| m.purpose),
            Some(InputPurpose::Filter),
            "'/' must open filter input mode"
        );
        // Uppercase keystrokes prove case-insensitive matching end to end.
        for c in ['A', 'L', 'P'] {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Enter);

        assert_eq!(app.filter.as_deref(), Some("alp"));
        let ids: Vec<&str> = app
            .visible_tasks()
            .iter()
            .map(|(task_id, _)| task_id.as_str())
            .collect();
        assert_eq!(ids, vec!["alpha"], "only alpha survives the filter");
        assert_eq!(app.selected, 0, "selection resets into the filtered view");
        assert_eq!(app.selected_task_id(), Some("alpha"));
    }

    #[test]
    fn bulk_collects_only_visible_verified_after_filter() {
        let path = tmp_ledger_path("filter-bulk");
        seed_verified(&path, "alpha");
        seed_verified(&path, "beta");
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('/'));
        for c in ['a', 'l', 'p'] {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Enter);
        app.handle_key(KeyCode::Char('A'));

        assert_eq!(
            app.pending_confirm,
            Some(PendingConfirm::Bulk {
                tasks: vec!["alpha".to_string()]
            }),
            "bulk composition must respect the active filter"
        );
    }

    #[test]
    fn empty_filter_commit_clears_the_filter() {
        let path = tmp_ledger_path("filter-clear");
        seed_verified(&path, "alpha");
        seed_verified(&path, "beta");
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('/'));
        app.handle_key(KeyCode::Char('a'));
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.filter.as_deref(), Some("a"));

        // A whitespace-only buffer trims to empty and clears the filter.
        app.handle_key(KeyCode::Char('/'));
        app.handle_key(KeyCode::Char(' '));
        app.handle_key(KeyCode::Enter);

        assert!(app.filter.is_none());
        assert_eq!(app.visible_tasks().len(), 2, "all tasks visible again");
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn question_mark_toggles_help_and_blocks_navigation() {
        let path = tmp_ledger_path("help-toggle");
        seed_verified(&path, "t-1");
        seed_verified(&path, "t-2");
        let mut app = App::load(&path);

        assert!(!app.show_help);
        app.handle_key(KeyCode::Char('?'));
        assert!(app.show_help);

        app.handle_key(KeyCode::Char('j'));
        assert_eq!(
            app.selected, 0,
            "navigation must not leak through the help overlay"
        );

        app.handle_key(KeyCode::Char('?'));
        assert!(!app.show_help, "'?' closes the overlay again");
    }

    #[test]
    fn mouse_scroll_on_list_moves_selection_like_arrow_keys() {
        let path = tmp_ledger_path("mouse-list");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_mouse_scroll(MouseDirection::Down, Pane::List);
        assert_eq!(app.selected_task_id(), Some("task-b"));

        app.handle_mouse_scroll(MouseDirection::Down, Pane::List);
        assert_eq!(app.selected_task_id(), Some("task-b"), "clamped at last");

        app.handle_mouse_scroll(MouseDirection::Up, Pane::List);
        assert_eq!(app.selected_task_id(), Some("task-a"));

        app.handle_mouse_scroll(MouseDirection::Up, Pane::List);
        assert_eq!(app.selected_task_id(), Some("task-a"), "clamped at first");
    }

    #[test]
    fn mouse_scroll_on_detail_offsets_by_three_and_saturates_at_zero() {
        let path = tmp_ledger_path("mouse-detail");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_mouse_scroll(MouseDirection::Up, Pane::Detail);
        assert_eq!(app.detail_offset, 0, "saturates at zero");

        app.handle_mouse_scroll(MouseDirection::Down, Pane::Detail);
        assert_eq!(app.detail_offset, 3);
        app.handle_mouse_scroll(MouseDirection::Down, Pane::Detail);
        assert_eq!(app.detail_offset, 6);

        app.handle_mouse_scroll(MouseDirection::Up, Pane::Detail);
        assert_eq!(app.detail_offset, 3);
    }

    #[test]
    fn clamp_offsets_bounds_detail_offset_to_content_length() {
        let path = tmp_ledger_path("clamp-offsets");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.detail_offset = 50;
        app.clamp_offsets(3);
        assert_eq!(app.detail_offset, 2);

        app.clamp_offsets(0);
        assert_eq!(app.detail_offset, 0, "empty content forces offset zero");
    }

    #[test]
    fn ctrl_c_quits_in_normal_state_but_plain_c_does_not() {
        let path = tmp_ledger_path("ctrl-c-normal");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()));
        assert!(!app.should_quit, "plain 'c' must not quit");

        app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_quits_while_help_overlay_is_open() {
        let path = tmp_ledger_path("ctrl-c-help");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('?'));
        assert!(app.show_help);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn legacy_handle_key_wrapper_still_routes_navigation_and_quit() {
        let path = tmp_ledger_path("legacy-wrapper");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.selected_task_id(), Some("task-b"));
        app.handle_key(KeyCode::Char('k'));
        assert_eq!(app.selected_task_id(), Some("task-a"));
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.pane, Pane::Detail);
        app.handle_key(KeyCode::Char('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn debug_impl_renders_struct_fields() {
        let path = tmp_ledger_path("debug-fmt");
        write_two_task_ledger(&path);
        let app = App::load(&path);

        let dbg = format!("{app:?}");
        assert!(
            dbg.starts_with("App {"),
            "debug output must name the struct, got: {dbg}"
        );
        for field in [
            "ledger_path",
            "tasks",
            "selected: 0",
            "pane: List",
            "should_quit: false",
        ] {
            assert!(dbg.contains(field), "debug output missing {field:?}: {dbg}");
        }
    }

    #[test]
    fn ledger_path_default_mirrors_cli_resolution() {
        let saved = std::env::var("PF_LEDGER").ok();

        std::env::remove_var("PF_LEDGER");
        assert_eq!(
            App::ledger_path_default(),
            PathBuf::from(".pf/ledger.jsonl"),
            "unset PF_LEDGER falls back to the CLI default"
        );

        std::env::set_var("PF_LEDGER", "");
        assert_eq!(
            App::ledger_path_default(),
            PathBuf::from(".pf/ledger.jsonl"),
            "an empty PF_LEDGER must fall back to the default"
        );

        std::env::set_var("PF_LEDGER", "/tmp/pf-tui-custom-ledger.jsonl");
        assert_eq!(
            App::ledger_path_default(),
            PathBuf::from("/tmp/pf-tui-custom-ledger.jsonl"),
            "a non-empty PF_LEDGER wins over the default"
        );

        match saved {
            Some(value) => std::env::set_var("PF_LEDGER", value),
            None => std::env::remove_var("PF_LEDGER"),
        }
    }

    #[test]
    fn q_quits_while_the_help_overlay_is_open() {
        let path = tmp_ledger_path("help-quit");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Char('?'));
        assert!(app.show_help);
        app.handle_key(KeyCode::Char('q'));
        assert!(app.should_quit, "'q' must quit through the help overlay");
    }

    #[test]
    fn list_pane_navigation_never_populates_the_detail_cache() {
        let path = tmp_ledger_path("nav-no-cache");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Down);
        app.handle_key(KeyCode::Up);
        assert!(
            app.entries_of_selected.is_empty(),
            "moving the selection in the list pane must not refresh detail entries"
        );
    }

    #[test]
    fn entering_detail_pane_loads_the_entry_cache() {
        let path = tmp_ledger_path("enter-cache");
        write_two_task_ledger(&path);
        let mut app = App::load(&path);

        app.handle_key(KeyCode::Enter);
        assert_eq!(app.pane, Pane::Detail);
        assert!(
            !app.entries_of_selected.is_empty(),
            "entering the detail pane must load the selected task's entries"
        );
    }

    #[test]
    fn tail_hash_equals_the_chain_head_of_a_valid_ledger() {
        let path = tmp_ledger_path("tail-hash");
        write_two_task_ledger(&path);
        let app = App::load(&path);

        let expected = Ledger::new(&path).verify_chain().unwrap().head_hash;
        assert_eq!(app.tail_hash, expected);
        assert_eq!(app.tail_hash.len(), 64, "chain head is a sha256 hex string");
    }
}
