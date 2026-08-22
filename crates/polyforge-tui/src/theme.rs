//! LazyForge theme: one palette module to rule all rendering.
//!
//! Invariant: NO other file in this crate may contain Color::Rgb or hex
//! literals; every style flows through these semantic roles.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const SUCCESS: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
pub const MUTED: Color = Color::DarkGray;
pub const SURFACE: Color = Color::Black;
pub const BORDER: Color = Color::Rgb(64, 64, 64);
pub const TEXT_DIM: Color = Color::Gray;

/// Selection highlight: reversed text plus an accent border.
pub fn selection_style() -> Style {
    Style::default()
        .fg(SURFACE)
        .bg(ACCENT)
        .add_modifier(Modifier::REVERSED)
}

/// Rounded titled block border style.
pub fn block_border_style() -> Style {
    Style::default().fg(BORDER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub(crate) fn selection_style_uses_accent_on_surface() {
        let style = selection_style();
        assert_eq!(style.fg, Some(SURFACE));
        assert_eq!(style.bg, Some(ACCENT));
    }

    #[test]
    pub(crate) fn block_border_style_uses_border_color() {
        assert_eq!(block_border_style().fg, Some(BORDER));
    }
}
