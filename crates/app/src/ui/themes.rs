//! The themes tab: a name, where it came from, and the colours themselves.
//!
//! A swatch beats a name. Somebody choosing a theme is choosing by eye, so
//! every row carries eight blocks of its own accent colours, and the cursor
//! paints the whole screen in the theme it is on. The list is what changes,
//! not a preview pane, because the preview is the app.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::theme::Theme;
use crate::themes::Entry;
use crate::ui::hit::Hit;
use crate::ui::{clip, fill, mix, put, span};

/// Cells the name column takes, so the swatches of every row line up.
const NAME_WIDTH: usize = 20;

/// Cells the origin column takes.
const ORIGIN_WIDTH: usize = 22;

/// The block a swatch is drawn with, twice per colour so a swatch is wide
/// enough to read the hue rather than guess it.
const SWATCH: &str = "\u{2588}\u{2588}";

/// Which ANSI slots a swatch shows: the six bright colours a theme is actually
/// told apart by, skipping the two greys that every theme shares.
const SWATCH_SLOTS: [usize; 6] = [1, 2, 3, 4, 5, 6];

pub struct ThemesView<'a> {
    pub entries: &'a [Entry],
    pub cursor: usize,
    /// The name in the config, which is the one that survives leaving.
    pub chosen_theme: &'a str,
    pub theme: &'a Theme,
}

pub fn themes(buffer: &mut Buffer, area: Rect, view: ThemesView<'_>) -> Vec<(Rect, Hit)> {
    let theme = view.theme;
    fill(buffer, area, theme.background);
    if area.width < 24 || area.height < 4 {
        return Vec::new();
    }
    let content_area = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 1);
    let muted = Style::new().fg(theme.muted);

    put(
        buffer,
        content_area,
        content_area.y,
        vec![span(
            "PICK ONE. THE SCREEN IS THE PREVIEW.",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        )],
    );
    put(
        buffer,
        content_area,
        content_area.y + 1,
        vec![span(
            "j k to look   enter to keep   escape to put the old one back",
            muted,
        )],
    );

    let first_theme_row_y = content_area.y + 3;
    let visible_theme_count =
        (content_area.y + content_area.height).saturating_sub(first_theme_row_y) as usize;
    // The cursor stays on screen without a scroll field, because the list is
    // short enough that a window around the cursor is the whole of it.
    let first_visible_index = view.cursor.saturating_sub(visible_theme_count.saturating_sub(1));
    let mut hit_regions = Vec::new();
    for (visible_theme_offset, theme_entry) in view
        .entries
        .iter()
        .skip(first_visible_index)
        .take(visible_theme_count)
        .enumerate()
    {
        let theme_index = first_visible_index + visible_theme_offset;
        let row_y = first_theme_row_y + visible_theme_offset as u16;
        let row_area = Rect::new(area.x, row_y, area.width, 1);
        if theme_index == view.cursor {
            fill(buffer, row_area, mix(theme.panel, theme.background, 0.2));
        }
        let is_chosen = theme_entry.matches_choice(view.chosen_theme);
        let name_style = match (theme_index == view.cursor, is_chosen) {
            (true, _) => Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            (false, true) => Style::new().fg(theme.foreground).add_modifier(Modifier::BOLD),
            (false, false) => Style::new().fg(theme.foreground),
        };
        let mut line_spans = vec![
            span(
                match is_chosen {
                    true => "  \u{25cf} ",
                    false => "    ",
                },
                Style::new().fg(theme.accent),
            ),
            span(
                format!("{:<NAME_WIDTH$}", clip(&theme_entry.name, NAME_WIDTH)),
                name_style,
            ),
            span(format!("{:<ORIGIN_WIDTH$}", theme_entry.origin.label()), muted),
        ];
        line_spans.extend(swatch(theme_entry, theme));
        put(buffer, content_area, row_y, line_spans);
        hit_regions.push((row_area, Hit::ThemeRow(theme_index)));
    }
    hit_regions
}

/// Six blocks of the theme's own colours, or a line saying it has none to show
/// yet because it is read at the moment it is applied.
fn swatch(theme_entry: &Entry, theme: &Theme) -> Vec<ratatui::text::Span<'static>> {
    let Some(palette) = &theme_entry.palette else {
        return vec![span("reads it when you pick it", Style::new().fg(theme.muted))];
    };
    SWATCH_SLOTS
        .iter()
        .filter_map(|color_index| palette.ansi(*color_index))
        .map(|color| span(SWATCH, Style::new().fg(color.color())))
        .collect()
}
