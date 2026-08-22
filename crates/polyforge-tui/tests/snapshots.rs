//! Snapshot tests for the LazyForge rendering layer.
//!
//! Each test drives [`polyforge_tui::ui::draw`] against a ratatui
//! [`TestBackend`] and asserts on the flattened buffer CONTENT (substring
//! checks), never byte-exact art, so minor layout tweaks do not churn these
//! snapshots. Expected strings are inline; no snapshot dependency.
//!
//! Determinism: ledgers are seeded through the same pure constructors the
//! app-state tests use, toasts are tick-driven, and nothing reads the wall
//! clock.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use polyforge_core::evidence::{promote, EvidenceEntry as TriStateEvidence};
use polyforge_core::ledger::Ledger;
use polyforge_tui::app::App;
use polyforge_tui::ui;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_ledger_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("pf-tui-snapshot-tests");
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

/// Task "alpha" reaches Validated (claim + tool attestation + validation);
/// task "beta" stays ModelClaimed (bare claim).
fn write_two_task_ledger(path: &Path) {
    let mut ledger = Ledger::new(path);
    let c = claim("alpha");
    let v = promote(&c, &attestation("alpha")).unwrap();
    let d = promote(&v, &validation("alpha")).unwrap();
    ledger.append(c.to_ledger_entry()).unwrap();
    ledger.append(v.to_ledger_entry()).unwrap();
    ledger.append(d.to_ledger_entry()).unwrap();
    ledger.append(claim("beta").to_ledger_entry()).unwrap();
}

/// Render one frame of `app` at `width` x `height` and flatten the backend
/// buffer into a string (one line per terminal row).
fn render_to_string(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|frame| ui::draw(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

fn assert_two_task_content(screen: &str) {
    // List pane title with task count, plus both task ids and their badges.
    assert!(
        screen.contains("Tasks [2]"),
        "list title missing:\n{screen}"
    );
    assert!(screen.contains("alpha"), "task alpha missing:\n{screen}");
    assert!(screen.contains("beta"), "task beta missing:\n{screen}");
    assert!(
        screen.contains("Validated"),
        "alpha badge missing:\n{screen}"
    );
    assert!(
        screen.contains("ModelClaimed"),
        "beta badge missing:\n{screen}"
    );
    // Detail pane title for the default selection (first BTreeMap key).
    assert!(
        screen.contains("Detail: alpha"),
        "detail title missing:\n{screen}"
    );
}

#[test]
fn wide_120x30_side_by_side() {
    let path = tmp_ledger_path("wide");
    write_two_task_ledger(&path);
    let app = App::load(&path);
    assert!(app.error_screen.is_none());

    let screen = render_to_string(&app, 120, 30);
    assert_two_task_content(&screen);
}

#[test]
fn narrow_88x24_stacked() {
    // The stacked branch spans widths [80, 100): below 80 columns the console
    // refuses to render entirely (see too_small_79x23), so the narrow case is
    // exercised at 88x24 rather than a width that would hit the guard.
    let path = tmp_ledger_path("stacked");
    write_two_task_ledger(&path);
    let app = App::load(&path);

    let screen = render_to_string(&app, 88, 24);
    assert_two_task_content(&screen);
}

#[test]
fn too_small_79x23() {
    let path = tmp_ledger_path("too-small");
    write_two_task_ledger(&path);
    let app = App::load(&path);

    let screen = render_to_string(&app, 79, 23);
    assert!(
        screen.contains("terminal too small"),
        "too-small screen missing:\n{screen}"
    );
    assert!(
        !screen.contains("Tasks ["),
        "no panes may render when too small:\n{screen}"
    );
}

#[test]
fn empty_ledger_empty_state() {
    let path = tmp_ledger_path("empty");
    let app = App::load(&path);
    assert!(app.tasks.is_empty());

    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("No tasks in ledger"),
        "empty state missing:\n{screen}"
    );
}

#[test]
fn corrupt_fail_closed() {
    let path = tmp_ledger_path("corrupt");
    write_two_task_ledger(&path);
    std::fs::write(&path, b"not json at all\n").unwrap();

    let app = App::load(&path);
    assert!(
        app.error_screen.is_some(),
        "corrupt ledger must fail closed"
    );

    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("Ledger error"),
        "error screen title missing:\n{screen}"
    );
    assert!(
        !screen.contains("Tasks ["),
        "panes must not render on the error screen:\n{screen}"
    );
}
