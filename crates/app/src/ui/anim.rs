//! The three moving parts: an equaliser beside whatever is playing, a marquee
//! for a title too long for its cell, and the shade a toast fades through.
//! The volume is a bar in `player_bar` and holds still, so it lives there.
//!
//! Every function is pure in the frame counter `App` advances once per tick, so
//! a test drives the animation by feeding ticks and reading the buffer. With
//! `animations = false` each one returns its still form.

use unicode_width::UnicodeWidthChar;

use crate::ui::{fit, width_of};

/// Cells the equaliser occupies, lit or still.
pub const EQUALISER_WIDTH: usize = 4;

/// The animation clock as every widget sees it. `App` owns the frame counter
/// and only advances it while the music plays, so a still screen settles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Motion {
    pub animation_frame: u64,
    /// The `animations` config key. False makes every helper return its still
    /// form, which is what a screen recording or a slow terminal wants.
    pub animations_enabled: bool,
    /// False while paused and while loading, so the equaliser rests.
    pub playback_moving: bool,
}

const BARS: [char; 7] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
];

/// Eight frames of four bars. Each bar rises and falls on its own count, which
/// is what keeps the loop from reading as a wave.
const SEQUENCE: [[usize; EQUALISER_WIDTH]; 8] = [
    [1, 4, 2, 5],
    [3, 6, 1, 3],
    [5, 3, 4, 6],
    [6, 1, 6, 2],
    [4, 2, 5, 4],
    [2, 5, 3, 6],
    [1, 6, 1, 3],
    [3, 3, 4, 1],
];

/// The frame a stopped equaliser rests on, low enough to read as silence.
const STILL: [usize; EQUALISER_WIDTH] = [2, 4, 2, 4];

/// Ticks between two marquee steps.
const STEP_TICKS: u64 = 3;

/// Ticks a marquee rests at each end, two seconds at `TICK_MILLIS`.
const PAUSE_TICKS: u64 = 16;

/// Blank cells between the tail of a marquee and its own head.
const GAP: &str = "    ";

/// Four cells of equaliser. `playback_moving` is false while paused, while loading and
/// whenever the config turned animation off.
pub fn equaliser(animation_frame: u64, playback_moving: bool) -> String {
    let bar_heights = match playback_moving {
        true => SEQUENCE[(animation_frame % SEQUENCE.len() as u64) as usize],
        false => STILL,
    };
    bar_heights.iter().map(|bar_height| BARS[*bar_height]).collect()
}

/// `text` in exactly `width` cells. Text that fits is padded. Text that does
/// not is scrolled when animations are enabled, and clipped with an ellipsis
/// when they are disabled.
pub fn scrolling(text: &str, width: usize, animation_frame: u64, animations_enabled: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let text_width = width_of(text);
    if !animations_enabled || text_width <= width {
        return fit(text, width);
    }
    let scrolling_content = format!("{text}{GAP}");
    let cycle_width = width_of(&scrolling_content) as u64;
    let last_start_cell = (text_width - width) as u64;
    let travel_ticks = STEP_TICKS * last_start_cell;
    let cycle_tick = animation_frame % (2 * PAUSE_TICKS + STEP_TICKS * cycle_width);
    let start_cell = if cycle_tick < PAUSE_TICKS {
        0
    } else if cycle_tick < PAUSE_TICKS + travel_ticks {
        (cycle_tick - PAUSE_TICKS) / STEP_TICKS
    } else if cycle_tick < 2 * PAUSE_TICKS + travel_ticks {
        last_start_cell
    } else {
        last_start_cell + (cycle_tick - 2 * PAUSE_TICKS - travel_ticks) / STEP_TICKS
    };
    window(&scrolling_content, start_cell as usize, width)
}

/// `width` cells of `content` from cell `start`, wrapping round. A wide glyph
/// the window cuts in half is drawn as a space, so the row never shifts.
fn window(content: &str, start_cell: usize, width: usize) -> String {
    let cell_slots = cells_of(content);
    if cell_slots.is_empty() {
        return " ".repeat(width);
    }
    let mut visible_text = String::new();
    let mut output_cells = 0;
    while output_cells < width {
        match cell_slots[(start_cell + output_cells) % cell_slots.len()] {
            Some(glyph) => {
                let glyph_width = UnicodeWidthChar::width(glyph).unwrap_or(1);
                match output_cells + glyph_width <= width {
                    true => {
                        visible_text.push(glyph);
                        output_cells += glyph_width;
                    }
                    false => {
                        visible_text.push(' ');
                        output_cells += 1;
                    }
                }
            }
            None => {
                visible_text.push(' ');
                output_cells += 1;
            }
        }
    }
    visible_text
}

/// One entry per cell: the glyph in its first cell, `None` in the second cell
/// of a wide one. Zero width marks are dropped, because they hold no cell.
fn cells_of(content: &str) -> Vec<Option<char>> {
    let mut cell_slots = Vec::new();
    for glyph in content.chars() {
        let glyph_width = UnicodeWidthChar::width(glyph).unwrap_or(0);
        if glyph_width == 0 {
            continue;
        }
        cell_slots.push(Some(glyph));
        cell_slots.extend(std::iter::repeat_n(None, glyph_width - 1));
    }
    cell_slots
}

/// How far a toast has run into its last second, 0 at a second left and 2 when
/// it is about to go. The caller maps that onto foreground, dim and muted.
pub fn fade_step(remaining_millis: u64, animations_enabled: bool) -> usize {
    if !animations_enabled || remaining_millis >= 1000 {
        return 0;
    }
    match remaining_millis {
        667..=999 => 0,
        334..=666 => 1,
        _ => 2,
    }
}
