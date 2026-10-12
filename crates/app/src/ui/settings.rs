//! Persistent choices that change how redwax responds to input.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::config::Config;
use crate::theme::Theme;
use crate::ui::hit::Hit;
use crate::ui::{fill, mix, put, span};

pub fn settings(buffer: &mut Buffer, area: Rect, config: &Config, theme: &Theme) -> Vec<(Rect, Hit)> {
    fill(buffer, area, theme.background);
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let horizontal_padding = if area.width >= 5 { 2 } else { 0 };
    let vertical_padding = if area.height >= 3 { 1 } else { 0 };
    let content_area = Rect::new(
        area.x + horizontal_padding,
        area.y + vertical_padding,
        area.width.saturating_sub(horizontal_padding * 2),
        area.height.saturating_sub(vertical_padding),
    );
    if content_area.width == 0 || content_area.height == 0 {
        return Vec::new();
    }

    let accent = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let muted = Style::new().fg(theme.muted);
    put(
        buffer,
        content_area,
        content_area.y,
        vec![span("SETTINGS", accent)],
    );
    if content_area.height == 1 {
        return Vec::new();
    }
    put(
        buffer,
        content_area,
        content_area.y + 1,
        vec![span("enter or space to change", muted)],
    );

    let row_y = content_area.y.saturating_add(3);
    if row_y >= content_area.y.saturating_add(content_area.height) {
        return Vec::new();
    }
    let setting_row_area = Rect::new(area.x, row_y, area.width, 1);
    fill(buffer, setting_row_area, mix(theme.panel, theme.background, 0.2));
    let editing_mode = match config.search_vim_mode {
        true => "[Vim]",
        false => "[Standard]",
    };
    put(
        buffer,
        content_area,
        row_y,
        vec![
            span("  Search editing   ", Style::new().fg(theme.foreground)),
            span(editing_mode, accent),
        ],
    );

    let help_start_y = row_y.saturating_add(2);
    put(
        buffer,
        content_area,
        help_start_y,
        vec![span(
            "Vim opens search in Insert mode. Escape enters Normal mode. Press i to return.",
            muted,
        )],
    );
    put(
        buffer,
        content_area,
        help_start_y.saturating_add(1),
        vec![span(
            "A second escape leaves search. The choice is saved to config.toml.",
            muted,
        )],
    );

    vec![(setting_row_area, Hit::ToggleSearchVim)]
}
