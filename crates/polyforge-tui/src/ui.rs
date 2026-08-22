//! Rendering layer for LazyForge.
//!
//! Pure functions from [`App`] state to ratatui widgets. Every style flows
//! through the semantic roles in [`crate::theme`]; no `Color::Rgb` or hex
//! literals may appear here. Nothing in this module mutates app state and
//! nothing reads the wall clock, so TestBackend snapshots stay deterministic.

use polyforge_core::EvidenceState;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::prelude::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, InputMode, InputPurpose, Pane, PendingConfirm};
use crate::theme;
use crate::validate::VALIDATOR;

/// Below this width (or [`MIN_ROWS`] rows) the console refuses to render.
const MIN_COLS: u16 = 80;
/// Minimum usable height; see [`MIN_COLS`].
const MIN_ROWS: u16 = 24;
/// At this width or wider the panes sit side by side instead of stacked.
const WIDE_COLS: u16 = 100;
/// List pane share of the width in the side-by-side layout.
const LIST_PANE_PCT: u16 = 40;
/// Modal popup share of the terminal width.
const MODAL_WIDTH_PCT: u16 = 60;
/// Task ids listed inside the bulk confirmation before truncation.
const BULK_LIST_LIMIT: usize = 10;

/// Render one frame of the console onto `f`.
///
/// Priority order: the too-small screen wins over everything (fail closed on
/// unusable geometry), then the ledger error screen, then the normal two-pane
/// layout with a toast row above the status bar.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_COLS || area.height < MIN_ROWS {
        draw_too_small(f, area);
        return;
    }
    if let Some(error_text) = app.error_screen.as_deref() {
        draw_error(f, error_text, area);
        return;
    }

    // One reserved row for toasts sits directly above the status bar.
    let [main, toast_row, status_row] = Layout::new(
        Direction::Vertical,
        [
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ],
    )
    .areas(area);

    let [list_area, detail_area] = if area.width >= WIDE_COLS {
        Layout::new(
            Direction::Horizontal,
            [
                Constraint::Percentage(LIST_PANE_PCT),
                Constraint::Percentage(100 - LIST_PANE_PCT),
            ],
        )
        .areas(main)
    } else {
        Layout::new(
            Direction::Vertical,
            [Constraint::Percentage(50), Constraint::Percentage(50)],
        )
        .areas(main)
    };

    draw_list(f, app, list_area);
    draw_detail(f, app, detail_area);
    draw_toasts(f, app, toast_row);
    draw_status_bar(f, app, status_row);

    // Overlays paint last so they sit on top of every pane. Precedence:
    // confirmation modal > input mode > help overlay.
    if let Some(pending) = &app.pending_confirm {
        draw_confirm_modal(f, app, pending);
    } else if let Some(mode) = &app.input_mode {
        draw_input_popup(f, mode);
    } else if app.show_help {
        draw_help(f);
    }
}

/// Full-screen refusal for terminals under 80x24: centered muted text on the
/// surface color, nothing else drawn.
fn draw_too_small(f: &mut Frame, area: ratatui::prelude::Rect) {
    let text = Text::from(vec![
        Line::from("terminal too small"),
        Line::from(format!("needs at least {MIN_COLS}x{MIN_ROWS}")),
    ]);
    let paragraph = Paragraph::new(text)
        .alignment(Alignment::Center)
        .style(Style::default().fg(theme::MUTED).bg(theme::SURFACE));
    f.render_widget(paragraph, area);
}

/// Fail-closed full-screen view for a corrupt or unreadable ledger. The other
/// panes are never drawn so no partial data can leak.
fn draw_error(f: &mut Frame, error_text: &str, area: ratatui::prelude::Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title("Ledger error")
        .border_style(theme::block_border_style().fg(theme::ERROR));
    let paragraph = Paragraph::new(Text::from(error_text))
        .block(block)
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(theme::ERROR));
    f.render_widget(paragraph, area);
}

/// Task list pane: one row per visible task in BTreeMap order, `task_id`
/// plus a state badge; the selected row carries the theme selection style.
fn draw_list(f: &mut Frame, app: &App, area: ratatui::prelude::Rect) {
    let visible = app.visible_tasks();
    let title = format!("Tasks [{}]", visible.len());
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(theme::block_border_style());

    let empty_hint = if app.tasks.is_empty() {
        "No tasks in ledger"
    } else {
        "No tasks match filter"
    };
    if visible.is_empty() {
        let paragraph = Paragraph::new(Text::from(empty_hint))
            .block(block)
            .alignment(Alignment::Center)
            .style(Style::default().fg(theme::MUTED));
        f.render_widget(paragraph, area);
        return;
    }

    let rows: Vec<Line> = visible
        .into_iter()
        .enumerate()
        .map(|(index, (task_id, state))| task_row(task_id, *state, index == app.selected))
        .collect();
    let paragraph = Paragraph::new(Text::from(rows)).block(block);
    f.render_widget(paragraph, area);
}

/// One list row. Selected rows render as a single plain span so the full-row
/// selection style is not patched over by per-span badge colors.
fn task_row(task_id: &str, state: EvidenceState, selected: bool) -> Line<'static> {
    let badge = format!("{state:?}");
    if selected {
        return Line::styled(format!("{task_id} {badge}"), theme::selection_style());
    }
    Line::from(vec![
        Span::raw(format!("{task_id} ")),
        Span::styled(badge, state_badge_style(state)),
    ])
}

/// Semantic color per evidence state, straight from the theme palette.
fn state_badge_style(state: EvidenceState) -> Style {
    match state {
        EvidenceState::Refuted => Style::default().fg(theme::ERROR),
        EvidenceState::Verified => Style::default().fg(theme::SUCCESS),
        EvidenceState::Validated => Style::default().fg(theme::ACCENT),
        EvidenceState::ModelClaimed => Style::default().fg(theme::TEXT_DIM),
    }
}

/// Detail pane for the selected task: `<kind>  <state>` per entry, or a hint
/// when there is nothing to show yet.
fn draw_detail(f: &mut Frame, app: &App, area: ratatui::prelude::Rect) {
    let selected_task = app
        .visible_tasks()
        .get(app.selected)
        .map(|(id, _)| id.as_str());
    let title = match selected_task {
        Some(task_id) => format!("Detail: {task_id}"),
        None => "Detail".to_string(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(theme::block_border_style());

    let body: Text = if app.tasks.is_empty() {
        Text::from(hint_line("Select a task"))
    } else if app.pane == Pane::List {
        Text::from(hint_line("Enter to open"))
    } else {
        Text::from(
            app.entries_of_selected
                .iter()
                .map(|(kind, state)| {
                    Line::from(vec![
                        Span::raw(kind.clone()),
                        Span::raw("  "),
                        Span::raw(state.clone()),
                    ])
                })
                .collect::<Vec<Line>>(),
        )
    };

    let paragraph = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
    // Scroll only the real entry listing; hint bodies stay pinned to the
    // top. The offset is clamped against the content length so a shrunken
    // ledger cannot scroll the pane past its last line.
    let rendered = if app.pane == Pane::Detail && !app.entries_of_selected.is_empty() {
        let max_offset = app.entries_of_selected.len().saturating_sub(1);
        let offset = app.detail_offset.min(max_offset);
        let row = u16::try_from(offset).unwrap_or(u16::MAX);
        paragraph.scroll((row, 0))
    } else {
        paragraph
    };
    f.render_widget(rendered, area);
}

/// Muted single-line hint used inside empty panes.
fn hint_line(text: &str) -> Line<'_> {
    Line::styled(text.to_string(), Style::default().fg(theme::MUTED))
}

/// Toast strip above the status bar: visible toasts joined with `" | "`.
fn draw_toasts(f: &mut Frame, app: &App, area: ratatui::prelude::Rect) {
    let messages: Vec<&str> = app
        .toasts
        .iter()
        .filter(|toast| toast.visible())
        .map(|toast| toast.message())
        .collect();
    if messages.is_empty() {
        return;
    }
    let line = Line::styled(
        messages.join(" | "),
        Style::default().fg(theme::WARN).bg(theme::SURFACE),
    );
    f.render_widget(Paragraph::new(Text::from(line)), area);
}

/// Bottom status bar: truncated ledger path, chain head, active filter,
/// task count, focused pane.
fn draw_status_bar(f: &mut Frame, app: &App, area: ratatui::prelude::Rect) {
    let pane_indicator = match app.pane {
        Pane::List => "List",
        Pane::Detail => "Detail",
    };
    let mut parts: Vec<String> = Vec::new();
    let head: String = app.tail_hash.chars().take(12).collect();
    if !head.is_empty() {
        parts.push(head);
    }
    if let Some(filter) = app.filter.as_deref() {
        parts.push(format!("filter: {filter}"));
    }
    parts.push(format!("{} tasks", app.tasks.len()));
    parts.push(pane_indicator.to_string());
    let tail = format!(" | {}", parts.join(" | "));
    let budget = usize::from(area.width).saturating_sub(tail.chars().count());
    let path = truncate_chars(&app.ledger_path.display().to_string(), budget);

    let line = Line::from(vec![
        Span::styled(path, Style::default().fg(theme::TEXT_DIM)),
        Span::styled(tail, Style::default().fg(theme::MUTED)),
    ]);
    f.render_widget(Paragraph::new(Text::from(line)), area);
}

/// Char-boundary-safe truncation with an ellipsis marker when cut.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('~');
    out
}

/// Confirmation popup for a pending validation action.
///
/// The single variant names one task and shows the rationale that will be
/// recorded (the committed one, or the default wording when none was typed).
/// The bulk variant lists up to [`BULK_LIST_LIMIT`] task ids with a
/// `+N more` overflow line. Both share the accent-bordered popup chrome.
fn draw_confirm_modal(f: &mut Frame, app: &App, pending: &PendingConfirm) {
    let (title, mut lines) = match pending {
        PendingConfirm::Single { task_id } => {
            let rationale = app
                .pending_rationale
                .as_deref()
                .unwrap_or("lazyforge operator validation");
            (
                "Confirm validation",
                vec![
                    Line::from(format!("task: {task_id}")),
                    Line::from(format!("validator: {VALIDATOR}")),
                    Line::from(format!("rationale: {rationale}")),
                    Line::from("commit/diff copied from the latest Verified entry"),
                ],
            )
        }
        PendingConfirm::Bulk { tasks } => {
            let mut bulk_lines = vec![Line::from(format!("tasks: {}", tasks.len()))];
            for task_id in tasks.iter().take(BULK_LIST_LIMIT) {
                bulk_lines.push(Line::from(format!("  {task_id}")));
            }
            let hidden = tasks.len().saturating_sub(BULK_LIST_LIMIT);
            if hidden > 0 {
                bulk_lines.push(Line::from(format!("  +{hidden} more")));
            }
            bulk_lines.push(Line::from("rationale: lazyforge bulk validate"));
            ("Confirm bulk validation", bulk_lines)
        }
    };
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "[Enter] confirm   [Esc] cancel",
        Style::default().fg(theme::MUTED),
    ));
    render_popup(f, title, lines);
}

/// Free-form capture popup: the buffer plus a trailing cursor marker. The
/// title names the capture purpose (`Rationale` or `Filter`).
fn draw_input_popup(f: &mut Frame, mode: &InputMode) {
    let title = match mode.purpose {
        InputPurpose::Rationale => "Rationale",
        InputPurpose::Filter => "Filter",
    };
    let lines = vec![
        Line::from(format!("{}_", mode.buffer)),
        Line::from(""),
        Line::styled(
            "[Enter] commit   [Esc] cancel",
            Style::default().fg(theme::MUTED),
        ),
    ];
    render_popup(f, title, lines);
}

/// Full-screen keymap overlay listing every binding, one line per key.
fn draw_help(f: &mut Frame) {
    const BINDINGS: [(&str, &str); 10] = [
        ("k / j / Up / Down", "move selection"),
        ("Enter", "toggle detail pane"),
        ("Tab", "switch pane"),
        ("v", "validate selected task"),
        ("A", "bulk validate visible Verified"),
        ("r", "rationale"),
        ("/", "filter tasks"),
        ("?", "toggle this help"),
        ("q", "quit"),
        ("mouse wheel", "scroll"),
    ];
    let lines: Vec<Line> = BINDINGS
        .iter()
        .map(|(key, desc)| Line::from(format!("{key:<18} {desc}")))
        .collect();
    render_popup(f, "Keymap", lines);
}

/// Shared popup chrome: centered rect, clear, rounded block with an accent
/// border so the modal reads as active against the muted panes behind it.
fn render_popup(f: &mut Frame, title: &str, lines: Vec<Line<'static>>) {
    let area = f.area();
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height);
    let popup_area = centered_rect(MODAL_WIDTH_PCT, height, area);

    f.render_widget(Clear, popup_area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(theme::ACCENT));
    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(theme::TEXT_DIM).bg(theme::SURFACE));
    f.render_widget(paragraph, popup_area);
}

/// Rect centered inside `area` at `width_pct` percent width and `height` rows,
/// clamped to the area so small terminals never get negative geometry.
fn centered_rect(width_pct: u16, height: u16, area: Rect) -> Rect {
    let width = area.width * width_pct / 100;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}
