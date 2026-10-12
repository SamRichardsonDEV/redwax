//! The widgets, ported from the Python `widgets.py`: `header` (wordmark, tabs,
//! account, a seam with the accent under the active tab), `page_list` (shelf
//! headers as unselectable rows, items as rows laid out by width: title,
//! artist, album columns at 72 cells and up, title and artist at 40, title
//! alone below), `sidebar` (art, now playing, up next), `player_bar` (three
//! rows: what plays, the seek bar, the modes), `keys_bar` (the hint row),
//! `lyrics`, `toasts`, `help`, `picker` (the playlist picker), `eq` (the
//! equaliser panel), `anim` (the moving parts).
//!
//! Every widget is a function of state and theme onto a `Rect`, so tests
//! render them into a `TestBackend`. Row and cell arithmetic follows the
//! Python functions `track_row`, `browsable_row` and `_cell` exactly, so the
//! screens match. Unicode width from the `unicode-width` crate.

pub mod actions;
pub mod anim;
pub mod confirm;
pub mod eq;
pub mod header;
pub mod help;
pub mod hit;
pub mod keys_bar;
pub mod layout;
pub mod lyrics;
pub mod page_list;
pub mod picker;
pub mod player_bar;
pub(crate) mod popup;
pub mod rows;
pub mod settings;
pub mod sidebar;
pub mod themes;
pub mod toasts;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub use anim::Motion;
pub use hit::{Hit, HitMap};
pub use rows::{Row, RowMarks, browsable_row, page_rows, row_text, shelf_header, track_row};

/// The rule that marks a shelf heading and the up next block.
pub const RULE: &str = "\u{258e}";

pub fn width_of(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// The prefix of `text` that fits in `width` cells, an ellipsis in the last
/// cell when something was cut.
pub fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if width_of(text) <= width {
        return text.to_string();
    }
    let mut clipped_text = String::new();
    let mut used_cells = 0;
    let mut utf8_buffer = [0u8; 4];
    for character in text.chars() {
        let character_width = width_of(character.encode_utf8(&mut utf8_buffer));
        if used_cells + character_width > width - 1 {
            break;
        }
        clipped_text.push(character);
        used_cells += character_width;
    }
    clipped_text.push('\u{2026}');
    clipped_text
}

/// `text` in exactly `width` cells, cropped with an ellipsis or padded.
pub fn fit(text: &str, width: usize) -> String {
    let mut fitted_text = clip(text, width);
    let padding_cells = width.saturating_sub(width_of(&fitted_text));
    fitted_text.push_str(&" ".repeat(padding_cells));
    fitted_text
}

/// Exactly `width` cells: the text cropped or padded to width-1, then one cell
/// of gutter. Python's `_cell`.
pub fn cell(text: &str, width: usize, style: Style) -> Span<'static> {
    if width <= 1 {
        return Span::raw(String::new());
    }
    let mut body = fit(text, width - 1);
    body.push(' ');
    Span::styled(body, style)
}

pub fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

pub fn spaces(count: usize) -> Span<'static> {
    Span::raw(" ".repeat(count))
}

/// Cells a line of spans occupies.
pub fn line_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|part| width_of(&part.content)).sum()
}

/// Paint one row of spans at `y`, clipped to `area`.
pub fn put(buffer: &mut Buffer, area: Rect, row_y: u16, line_spans: Vec<Span<'static>>) {
    if area.width == 0 || row_y < area.y || row_y >= area.y.saturating_add(area.height) {
        return;
    }
    buffer.set_line(area.x, row_y, &Line::from(line_spans), area.width);
}

/// Fill a rectangle with `background`, so a surface reads as one block. A
/// transparent theme paints nothing, which is how the wallpaper shows through.
pub fn fill(buffer: &mut Buffer, area: Rect, background: Color) {
    if background == Color::Reset || area.width == 0 || area.height == 0 {
        return;
    }
    buffer.set_style(area, Style::new().bg(background));
}

/// Blank every cell in `area`, for overlays that must not show what is under them.
pub fn blank(buffer: &mut Buffer, area: Rect, style: Style) {
    for row_y in area.y..area.y.saturating_add(area.height) {
        for column_x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buffer.cell_mut((column_x, row_y)) {
                cell.reset();
                cell.set_style(style);
            }
        }
    }
}

/// A foreground laid over a background at `foreground_weight`, the way
/// Textual paints `$panel 60%`. A non-RGB background keeps the foreground
/// because there is nothing to blend against.
pub fn mix(foreground: Color, background: Color, foreground_weight: f32) -> Color {
    let (
        Color::Rgb(foreground_red, foreground_green, foreground_blue),
        Color::Rgb(background_red, background_green, background_blue),
    ) = (foreground, background)
    else {
        return foreground;
    };
    let blend_channel = |foreground_channel: u8, background_channel: u8| {
        (foreground_channel as f32 * foreground_weight
            + background_channel as f32 * (1.0 - foreground_weight))
            .round() as u8
    };
    Color::Rgb(
        blend_channel(foreground_red, background_red),
        blend_channel(foreground_green, background_green),
        blend_channel(foreground_blue, background_blue),
    )
}

/// `text` broken by word into at most `lines` lines of `width` columns, and
/// the words that did not fit. A word wider than the line is cut, because the
/// alternative is a line that overruns the pane it was given.
pub fn wrap_words(text: &str, width: usize, lines: usize) -> (Vec<String>, String) {
    if width == 0 || lines == 0 {
        return (Vec::new(), text.to_string());
    }
    let mut word_pieces: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let mut remaining_word = word.to_string();
        while width_of(&remaining_word) > width {
            word_pieces.push(remaining_word.chars().take(width).collect());
            remaining_word = remaining_word.chars().skip(width).collect();
        }
        word_pieces.push(remaining_word);
    }
    let mut wrapped_lines: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut remaining_pieces: Vec<String> = Vec::new();
    for word_piece in word_pieces {
        if !remaining_pieces.is_empty() {
            remaining_pieces.push(word_piece);
            continue;
        }
        let candidate_line = match current_line.is_empty() {
            true => word_piece.clone(),
            false => format!("{current_line} {word_piece}"),
        };
        if width_of(&candidate_line) <= width {
            current_line = candidate_line;
            continue;
        }
        wrapped_lines.push(std::mem::take(&mut current_line));
        match wrapped_lines.len() == lines {
            true => remaining_pieces.push(word_piece),
            false => current_line = word_piece,
        }
    }
    if !current_line.is_empty() && wrapped_lines.len() < lines {
        wrapped_lines.push(current_line);
    }
    (wrapped_lines, remaining_pieces.join(" "))
}
