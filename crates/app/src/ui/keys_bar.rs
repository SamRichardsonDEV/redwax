//! The key hints, drawn two ways from one list: a box in the sidebar when the
//! terminal is wide enough for one, and a single row along the bottom when it
//! is not. Every hint names an action the app can run, so a click on one does
//! what the key beside it does.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::commands::command;
use crate::keys::Action;
use crate::theme::Theme;
use crate::ui::hit::Hit;
use crate::ui::{fill, fit, put, spaces, span, width_of};

/// Cells between two hints in the bottom row.
const HINT_GAP: usize = 3;

/// Columns the box lists the hints down.
const HINT_COLUMNS: usize = 2;

/// Cells the key takes in a box column, the widest key and its gutter.
const KEY_WIDTH: usize = 6;

/// Rows the box needs before it is worth drawing: two borders and one row.
pub const KEYS_MIN_HEIGHT: u16 = 3;

/// Every hint, in the order a short box keeps. It drops rows from the bottom,
/// so the last hints are the ones a user needs reminding of least.
pub const HINTS: [(Action, &str); 30] = [
    (Action::MoveQueueUp, "move up"),
    (Action::MoveQueueDown, "move down"),
    (Action::RemoveFromQueue, "remove"),
    (Action::CursorDown, "move"),
    (Action::OpenActions, "actions"),
    (Action::Open, "play / open"),
    (Action::LikeHighlighted, "like selected"),
    (Action::AddToQueue, "queue"),
    (Action::FindOnPage, "find"),
    (Action::CycleFilter, "filter"),
    (Action::NextSection, "section"),
    (Action::NextTab, "views"),
    (Action::TogglePause, "pause"),
    (Action::Play, "resume playing"),
    (Action::NextTrack, "next"),
    (Action::PreviousTrack, "previous"),
    (Action::PlayNext, "play next"),
    (Action::Radio, "radio"),
    (Action::Like, "like playing"),
    (Action::ToggleShuffle, "shuffle"),
    (Action::CycleRepeat, "repeat"),
    (Action::SetVolume, "volume"),
    (Action::JumpToQueue, "queue view"),
    (Action::RemoveFromPlaylist, "remove"),
    (Action::AddToPlaylist, "playlist"),
    (Action::SwitchTab(crate::app::Tab::Search), "search"),
    (Action::Back, "back / leave"),
    (Action::ToggleEq, "equaliser"),
    (Action::ToggleHelp, "help"),
    (Action::Quit, "quit"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintContext {
    Browse,
    Queue,
    Search,
    Playback,
}

fn hints(context: HintContext) -> Vec<(Action, &'static str)> {
    HINTS
        .iter()
        .copied()
        .filter(|(action, _)| match context {
            HintContext::Browse => !matches!(
                *action,
                Action::RemoveFromQueue | Action::MoveQueueUp | Action::MoveQueueDown
            ),
            HintContext::Queue => !matches!(
                *action,
                Action::Radio | Action::CycleFilter | Action::RemoveFromPlaylist
            ),
            HintContext::Search => !matches!(
                *action,
                Action::NextSection
                    | Action::PreviousSection
                    | Action::RemoveFromPlaylist
                    | Action::RemoveFromQueue
                    | Action::MoveQueueUp
                    | Action::MoveQueueDown
            ),
            HintContext::Playback => matches!(
                *action,
                Action::TogglePause
                    | Action::NextTrack
                    | Action::PreviousTrack
                    | Action::Like
                    | Action::ToggleShuffle
                    | Action::CycleRepeat
                    | Action::SetVolume
                    | Action::JumpToQueue
                    | Action::ToggleHelp
            ),
        })
        .collect()
}

/// Rows the box needs to list every hint.
pub const KEYS_ROWS: u16 = HINTS.len().div_ceil(HINT_COLUMNS) as u16;

/// Rows the whole box takes when it holds every hint.
pub const KEYS_BOX_HEIGHT: u16 = KEYS_ROWS + 2;

/// The hints in a single-line box, drawn the way the up next box is, with the
/// heading in the top border and the hints listed down two columns. A box
/// shorter than the hints keeps the rows it has room for.
pub fn keys_box(buffer: &mut Buffer, area: Rect, theme: &Theme) -> Vec<(Rect, Hit)> {
    contextual_keys_box(buffer, area, theme, HintContext::Browse)
}

pub fn contextual_keys_box(
    buffer: &mut Buffer,
    area: Rect,
    theme: &Theme,
    context: HintContext,
) -> Vec<(Rect, Hit)> {
    if area.height < KEYS_MIN_HEIGHT || area.width < 12 {
        return Vec::new();
    }
    let seam = Style::new().fg(theme.gutter);
    let box_width = area.width as usize;
    let visible_hint_rows = (area.height - 2) as usize;

    let heading = " KEYS ";
    let border_fill_width = box_width.saturating_sub(3 + width_of(heading));
    put(
        buffer,
        area,
        area.y,
        vec![
            span("\u{250c}\u{2500}", seam),
            span(
                heading,
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            span("\u{2500}".repeat(border_fill_width), seam),
            span("\u{2510}", seam),
        ],
    );

    let column_width = box_width.saturating_sub(4) / HINT_COLUMNS;
    let contextual_hints = hints(context);
    let mut hit_regions = Vec::new();
    for hint_row in 0..visible_hint_rows {
        let row_y = area.y + 1 + hint_row as u16;
        let mut line_spans = vec![span("\u{2502} ", seam)];
        for column in 0..HINT_COLUMNS {
            let Some((hint_action, hint_label)) = contextual_hints.get(column * visible_hint_rows + hint_row)
            else {
                break;
            };
            let Some(command) = command(*hint_action) else {
                continue;
            };
            let display_key = command.binding.key;
            line_spans.push(span(
                format!("{display_key:>width$} ", width = KEY_WIDTH - 1),
                Style::new().fg(theme.accent),
            ));
            line_spans.push(span(
                fit(hint_label, column_width.saturating_sub(KEY_WIDTH)),
                Style::new().fg(theme.foreground),
            ));
            let column_x = area.x + 2 + (column * column_width) as u16;
            hit_regions.push((
                Rect::new(column_x, row_y, column_width as u16, 1),
                Hit::KeyHint(*hint_action),
            ));
        }
        put(buffer, area, row_y, line_spans);
        // The right seam is written after the hints so a wide label cannot
        // paint over it.
        if let Some(cell) = buffer.cell_mut((area.x + area.width - 1, row_y)) {
            cell.set_symbol("\u{2502}").set_style(seam);
        }
    }

    put(
        buffer,
        area,
        area.y + area.height - 1,
        vec![
            span("\u{2514}", seam),
            span("\u{2500}".repeat(box_width.saturating_sub(2)), seam),
            span("\u{2518}", seam),
        ],
    );
    hit_regions
}

/// The hint row along the bottom, which is the fallback for a terminal too
/// narrow to draw the sidebar the box lives in.
pub fn keys_bar(buffer: &mut Buffer, area: Rect, theme: &Theme) -> Vec<(Rect, Hit)> {
    contextual_keys_bar(buffer, area, theme, HintContext::Browse)
}

pub fn contextual_keys_bar(
    buffer: &mut Buffer,
    area: Rect,
    theme: &Theme,
    context: HintContext,
) -> Vec<(Rect, Hit)> {
    fill(buffer, area, theme.surface);
    if area.width < 6 {
        return Vec::new();
    }
    let content_area = Rect::new(area.x + 2, area.y, area.width - 4, 1);
    let mut line_spans = Vec::new();
    let mut hit_regions = Vec::new();
    let mut next_column = 0usize;
    for (hint_action, hint_label) in hints(context) {
        let Some(command) = command(hint_action) else {
            continue;
        };
        let display_key = command.binding.key;
        let hint_width = width_of(display_key) + 1 + width_of(hint_label);
        if next_column + hint_width <= content_area.width as usize {
            hit_regions.push((
                Rect::new(
                    content_area.x + next_column as u16,
                    content_area.y,
                    hint_width as u16,
                    1,
                ),
                Hit::KeyHint(hint_action),
            ));
        }
        line_spans.push(span(display_key, Style::new().fg(theme.accent)));
        line_spans.push(span(format!(" {hint_label}"), Style::new().fg(theme.foreground)));
        line_spans.push(spaces(HINT_GAP));
        next_column += hint_width + HINT_GAP;
    }
    put(buffer, content_area, content_area.y, line_spans);
    hit_regions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_queue_hints_prioritise_edit_commands() {
        let area = Rect::new(0, 0, 80, 1);
        let mut buffer = Buffer::empty(area);

        let regions = contextual_keys_bar(
            &mut buffer,
            area,
            &crate::theme::redwax_theme(),
            HintContext::Queue,
        );

        for action in [
            Action::MoveQueueUp,
            Action::MoveQueueDown,
            Action::RemoveFromQueue,
        ] {
            assert!(regions.iter().any(|(_, hit)| hit == &Hit::KeyHint(action)));
        }
    }
}
