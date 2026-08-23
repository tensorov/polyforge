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
use polyforge_tui::theme;
use polyforge_tui::ui;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
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

/// Seed claim + tool attestation: the task ends `Verified`.
fn seed_verified(path: &Path, task: &str) {
    let mut ledger = Ledger::new(path);
    let c = claim(task);
    let v = promote(&c, &attestation(task)).unwrap();
    ledger.append(c.to_ledger_entry()).unwrap();
    ledger.append(v.to_ledger_entry()).unwrap();
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

#[test]
fn single_confirm_modal_renders() {
    let path = tmp_ledger_path("modal-single");
    seed_verified(&path, "task-v");
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('v'));
    assert!(
        app.pending_confirm.is_some(),
        "'v' must open the confirmation modal"
    );

    let screen = render_to_string(&app, 100, 30);
    assert!(
        screen.contains("Confirm validation"),
        "modal title missing:\n{screen}"
    );
    assert!(screen.contains("task-v"), "task id missing:\n{screen}");
    assert!(
        screen.contains("lazyforge-operator"),
        "validator identity missing:\n{screen}"
    );
    assert!(
        screen.contains("[Enter] confirm"),
        "modal footer missing:\n{screen}"
    );
}

#[test]
fn bulk_confirm_modal_renders() {
    let path = tmp_ledger_path("modal-bulk");
    seed_verified(&path, "alpha");
    seed_verified(&path, "beta");
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('A'));
    assert!(
        app.pending_confirm.is_some(),
        "'A' must open the bulk modal when Verified tasks exist"
    );

    let screen = render_to_string(&app, 100, 30);
    assert!(
        screen.contains("Confirm bulk validation"),
        "bulk modal title missing:\n{screen}"
    );
    assert!(
        screen.contains("tasks: 2"),
        "task count line missing:\n{screen}"
    );
    assert!(screen.contains("alpha"), "task alpha missing:\n{screen}");
    assert!(screen.contains("beta"), "task beta missing:\n{screen}");
}

#[test]
fn rationale_input_mode_renders() {
    let path = tmp_ledger_path("modal-rationale");
    seed_verified(&path, "task-i");
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('r'));
    app.handle_key(KeyCode::Char('o'));
    app.handle_key(KeyCode::Char('k'));
    assert!(
        app.input_mode.is_some(),
        "'r' must enter rationale input mode"
    );

    let screen = render_to_string(&app, 100, 30);
    assert!(
        screen.contains("Rationale"),
        "input block title missing:\n{screen}"
    );
    assert!(screen.contains("ok"), "buffer content missing:\n{screen}");
}

#[test]
fn help_overlay_renders() {
    let path = tmp_ledger_path("help-overlay");
    seed_verified(&path, "task-h");
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('?'));
    assert!(app.show_help, "'?' must open the help overlay");

    let screen = render_to_string(&app, 100, 30);
    assert!(
        screen.contains("Keymap"),
        "overlay title missing:\n{screen}"
    );
    assert!(
        screen.contains("validate"),
        "'v validate' row missing:\n{screen}"
    );
    assert!(
        screen.contains("bulk validate"),
        "'A bulk validate' row missing:\n{screen}"
    );
    assert!(
        screen.contains("filter"),
        "'/ filter' row missing:\n{screen}"
    );
}

#[test]
fn filter_indicator_in_status_bar() {
    let path = tmp_ledger_path("filter-status");
    seed_verified(&path, "alpha");
    seed_verified(&path, "beta");
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('/'));
    for c in ['a', 'l', 'p'] {
        app.handle_key(KeyCode::Char(c));
    }
    app.handle_key(KeyCode::Enter);

    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("filter: alp"),
        "status bar must carry the active filter:\n{screen}"
    );
    // The filtered list hides beta everywhere on screen.
    assert!(
        !screen.contains("beta"),
        "filtered-out task must not render:\n{screen}"
    );
}

#[test]
fn filtered_list_snapshot() {
    // The committed '/' filter narrows the rendered list to matching task
    // ids only: alpha stays on screen, beta disappears from every pane.
    let path = tmp_ledger_path("filter-list");
    write_two_task_ledger(&path);
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('/'));
    for c in ['a', 'l', 'p'] {
        app.handle_key(KeyCode::Char(c));
    }
    app.handle_key(KeyCode::Enter);

    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("Tasks [1]"),
        "list title must count only visible tasks:\n{screen}"
    );
    assert!(screen.contains("alpha"), "matching task missing:\n{screen}");
    assert!(
        !screen.contains("beta"),
        "filtered-out task must not render:\n{screen}"
    );
}

#[test]
fn toast_visible_then_expired() {
    // A pushed toast renders while it has ticks left and vanishes once
    // every toast has been ticked past its TTL (tick-driven, no wall clock).
    let path = tmp_ledger_path("toast-ttl");
    seed_verified(&path, "task-t");
    let mut app = App::load(&path);
    assert!(app.toasts.is_empty());

    app.push_toast("test msg");

    let visible = render_to_string(&app, 100, 30);
    assert!(
        visible.contains("test msg"),
        "fresh toast must render:\n{visible}"
    );

    // No tick_toasts helper exists on App; drive Toast::tick directly until
    // nothing is visible any more.
    while app.toasts.iter().any(|t| t.visible()) {
        for toast in app.toasts.iter_mut() {
            toast.tick();
        }
    }

    let expired = render_to_string(&app, 100, 30);
    assert!(
        !expired.contains("test msg"),
        "expired toast must not render:\n{expired}"
    );
}

#[test]
fn bulk_summary_after_execute() {
    // Confirming the bulk modal with Enter executes every listed validation
    // and surfaces the summary toast with appended/skipped counts.
    let path = tmp_ledger_path("bulk-execute");
    seed_verified(&path, "alpha");
    seed_verified(&path, "beta");
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('A'));
    assert!(
        app.pending_confirm.is_some(),
        "'A' must open the bulk modal when Verified tasks exist"
    );
    app.handle_key(KeyCode::Enter);
    assert!(app.pending_confirm.is_none(), "modal closes on execute");

    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("done 2, skipped 0"),
        "bulk summary toast missing:\n{screen}"
    );
}

fn render_buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|frame| ui::draw(frame, app)).expect("draw");
    terminal.backend().buffer().clone()
}

fn seed_cycle_ledger(path: &Path, task: &str, cycles: usize) {
    let mut ledger = Ledger::new(path);
    for _ in 0..cycles {
        let c = claim(task);
        let v = promote(&c, &attestation(task)).unwrap();
        let d = promote(&v, &validation(task)).unwrap();
        ledger.append(c.to_ledger_entry()).unwrap();
        ledger.append(v.to_ledger_entry()).unwrap();
        ledger.append(d.to_ledger_entry()).unwrap();
    }
}

#[test]
fn exact_minimum_80x24_renders_the_full_console() {
    let path = tmp_ledger_path("min-boundary");
    write_two_task_ledger(&path);
    let app = App::load(&path);

    let screen = render_to_string(&app, 80, 24);
    assert!(
        screen.contains("Tasks ["),
        "80x24 is exactly the minimum and must render normally:\n{screen}"
    );
    assert!(
        !screen.contains("terminal too small"),
        "too-small screen must not appear at the exact minimum:\n{screen}"
    );
}

#[test]
fn under_height_triggers_too_small_even_when_wide() {
    let path = tmp_ledger_path("short-screen");
    write_two_task_ledger(&path);
    let app = App::load(&path);

    let screen = render_to_string(&app, 120, 23);
    assert!(
        screen.contains("terminal too small"),
        "23 rows must refuse to render even at 120 columns:\n{screen}"
    );
}

#[test]
fn wide_layout_places_detail_pane_right_of_center() {
    let path = tmp_ledger_path("wide-split");
    write_two_task_ledger(&path);
    let app = App::load(&path);

    let screen = render_to_string(&app, 120, 30);
    let pos = screen
        .lines()
        .enumerate()
        .find_map(|(y, row)| row.find("Detail: alpha").map(|x| (x, y)))
        .expect("detail title must render in wide mode");
    assert!(
        pos.0 > 25,
        "at 120 columns the side-by-side layout puts the detail pane in \
         the right half; found its title at x={}:\n{screen}",
        pos.0
    );
}

#[test]
fn narrow_layout_stacks_detail_pane_below_the_list() {
    let path = tmp_ledger_path("stacked-split");
    write_two_task_ledger(&path);
    let app = App::load(&path);

    let screen = render_to_string(&app, 88, 24);
    let ty = screen
        .lines()
        .position(|row| row.contains("Detail: alpha"))
        .expect("detail title must render in stacked mode");
    assert!(
        ty >= 10,
        "below 100 columns the detail pane must sit in the lower half; \
         title row was {ty}:\n{screen}"
    );
}

#[test]
fn selected_row_carries_selection_style_and_badges_carry_state_colors() {
    let path = tmp_ledger_path("row-styles");
    write_two_task_ledger(&path);
    let app = App::load(&path);

    let buf = render_buffer(&app, 120, 30);
    let selected_row_cell = buf.cell((1, 1)).expect("alpha row cell");
    assert_eq!(
        selected_row_cell.fg,
        theme::SURFACE,
        "the selected row must be painted with the selection style"
    );
    let unselected_badge_cell = buf.cell((6, 2)).expect("beta badge cell");
    assert_eq!(
        unselected_badge_cell.fg,
        theme::TEXT_DIM,
        "an unselected badge must carry its state color"
    );
}

#[test]
fn list_pane_detail_shows_enter_hint() {
    let path = tmp_ledger_path("enter-hint");
    write_two_task_ledger(&path);
    let app = App::load(&path);

    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("Enter to open"),
        "with focus on the list pane the detail pane must show the hint:\n{screen}"
    );
}

#[test]
fn detail_pane_lists_entries_after_entering() {
    let path = tmp_ledger_path("detail-entries");
    write_two_task_ledger(&path);
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Enter);
    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("ToolAttestation"),
        "the detail pane must list the selected task's entries:\n{screen}"
    );
    assert!(
        !screen.contains("Enter to open"),
        "the hint must give way to real entries inside the detail pane:\n{screen}"
    );
}

#[test]
fn detail_scroll_offset_shifts_the_visible_entry_window() {
    let path = tmp_ledger_path("scroll-window");
    seed_cycle_ledger(&path, "task-s", 12);
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Enter);
    app.detail_offset = 1;

    let screen = render_to_string(&app, 120, 30);
    let first_content = screen
        .lines()
        .skip_while(|row| !row.contains("Detail: task-s"))
        .nth(1)
        .expect("a content row must follow the detail title");
    assert!(
        first_content.contains("ToolAttestation"),
        "offset 1 must hide the first entry line: the 36 entries repeat \
         with period three, so ToolAttestation replaces ModelClaim at the \
         top; top content row was {first_content:?}"
    );
}

#[test]
fn list_pane_hint_is_not_scrolled_by_a_stale_detail_offset() {
    let path = tmp_ledger_path("hint-scroll");
    write_two_task_ledger(&path);
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Enter);
    app.detail_offset = 5;
    app.handle_key(KeyCode::Enter);

    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("Enter to open"),
        "back on the list pane the hint must render unscrolled:\n{screen}"
    );
}

#[test]
fn empty_ledger_detail_pane_prompts_to_select() {
    let path = tmp_ledger_path("select-hint");
    let app = App::load(&path);

    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains("Select a task"),
        "an empty ledger must prompt in the detail pane:\n{screen}"
    );
}

#[test]
fn status_bar_carries_the_chain_head_prefix() {
    let path = tmp_ledger_path("status-head");
    write_two_task_ledger(&path);
    let app = App::load(&path);
    assert!(!app.tail_hash.is_empty());

    let head: String = app.tail_hash.chars().take(12).collect();
    let screen = render_to_string(&app, 120, 30);
    assert!(
        screen.contains(&head),
        "status bar must show the 12-char chain head {head}:\n{screen}"
    );
}

#[test]
fn bulk_modal_hides_overflow_line_when_everything_fits() {
    let path = tmp_ledger_path("bulk-small");
    seed_verified(&path, "t-01");
    seed_verified(&path, "t-02");
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('A'));
    assert!(app.pending_confirm.is_some());

    let screen = render_to_string(&app, 100, 30);
    assert!(
        screen.contains("Confirm bulk validation"),
        "bulk modal missing:\n{screen}"
    );
    assert!(
        !screen.contains("more"),
        "no overflow line may render when every task fits the list:\n{screen}"
    );
}

#[test]
fn bulk_modal_lists_overflow_count_beyond_ten_tasks() {
    let path = tmp_ledger_path("bulk-big");
    for i in 0..12 {
        seed_verified(&path, &format!("t-{i:02}"));
    }
    let mut app = App::load(&path);

    app.handle_key(KeyCode::Char('A'));
    assert!(app.pending_confirm.is_some());

    let screen = render_to_string(&app, 100, 30);
    assert!(
        screen.contains("+2 more"),
        "12 verified tasks must overflow the 10-row list by 2:\n{screen}"
    );
}
