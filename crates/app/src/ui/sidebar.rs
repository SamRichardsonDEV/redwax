//! The now playing panel: art, what is playing, the key hints in a box, and
//! the queue in a box under them, laid out the way a phone lays out a player.
//!
//! The art box keeps a fixed `ART_WIDTH` and takes its height from the image,
//! so a 16:9 video thumbnail is not stretched into the square an album cover
//! fills. One cell of padding either side, so a square cover fills every
//! column the sidebar was given.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use ratatui_image::Image;
use ratatui_image::protocol::Protocol;
use redwax_player::{PlayerState, Repeat};
use redwax_ytm::Track;

use crate::icons::Icons;
use crate::theme::Theme;
use crate::ui::anim::{self, Motion};
use crate::ui::hit::Hit;
use crate::ui::keys_bar::{HintContext, KEYS_BOX_HEIGHT, KEYS_MIN_HEIGHT, contextual_keys_box};
use crate::ui::layout::cover_max_height;
use crate::ui::{clip, fill, mix, put, spaces, span, width_of};

/// Rows the transport takes: one of air, then the buttons.
const TRANSPORT_HEIGHT: u16 = 2;

/// Cells between two transport buttons, so the row reads as five controls
/// rather than one run of glyphs.
const BUTTON_GAP: usize = 5;

/// Tracks the queue box shows when it has the room.
pub const QUEUE_ROWS: usize = 20;

/// Rows the queue box needs before it is worth drawing. The heading rides in
/// the top border, so it is that border, one track and the bottom border.
const QUEUE_MIN_HEIGHT: u16 = 3;

pub struct ArtView<'a> {
    pub protocol: Option<&'a Protocol>,
    /// Rows the image needs at `ART_WIDTH`, already capped by the config.
    pub cover_height: u16,
}

pub struct SidebarView<'a> {
    pub current_track: Option<&'a Track>,
    /// The transport row reads the modes and whether anything is paused.
    pub player_state: &'a PlayerState,
    pub current_track_liked: bool,
    pub upcoming_tracks: &'a [Track],
    pub cover_art: ArtView<'a>,
    pub icons: &'a Icons,
    pub theme: &'a Theme,
    pub hint_context: HintContext,
    pub motion: Motion,
}

pub fn sidebar(buffer: &mut Buffer, area: Rect, view: SidebarView<'_>) -> Vec<(Rect, Hit)> {
    let theme = view.theme;
    fill(buffer, area, theme.surface);
    if area.width < 8 || area.height < 4 {
        return Vec::new();
    }
    let content_area = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 1);
    let muted = Style::new().fg(theme.muted);
    let content_width = content_area.width as usize;
    let content_bottom = content_area.y + content_area.height;
    let mut hit_regions = Vec::new();

    let cover_height = cover_max_height(area, view.cover_art.cover_height);
    let cover_area = Rect::new(content_area.x, content_area.y, content_area.width, cover_height);
    if cover_height > 0 {
        match view.cover_art.protocol {
            Some(protocol) => Image::new(protocol).render(cover_area, buffer),
            None => fill(buffer, cover_area, mix(theme.panel, theme.background, 0.4)),
        }
    }

    let mut next_row_y = cover_area.y + cover_area.height + u16::from(cover_height > 0);
    match view.current_track {
        None => {
            put(
                buffer,
                content_area,
                next_row_y,
                vec![span("nothing playing", muted)],
            );
            next_row_y += 1;
        }
        Some(track) => {
            put(
                buffer,
                content_area,
                next_row_y,
                vec![span(
                    anim::scrolling(
                        &track.title,
                        content_width,
                        view.motion.animation_frame,
                        view.motion.animations_enabled,
                    ),
                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                )],
            );
            put(
                buffer,
                content_area,
                next_row_y + 1,
                vec![span(
                    clip(&track.artist_line(), content_width),
                    Style::new().fg(theme.foreground),
                )],
            );
            put(
                buffer,
                content_area,
                next_row_y + 2,
                vec![span(
                    clip(track.album.as_deref().unwrap_or(""), content_width),
                    muted,
                )],
            );
            next_row_y += 3;
        }
    }

    if next_row_y + TRANSPORT_HEIGHT <= content_bottom {
        hit_regions.extend(transport(buffer, content_area, next_row_y + 1, &view));
        next_row_y += TRANSPORT_HEIGHT;
    }

    if next_row_y < content_bottom {
        hit_regions.extend(boxes(
            buffer,
            Rect::new(
                content_area.x,
                next_row_y,
                content_area.width,
                content_bottom - next_row_y,
            ),
            &view,
        ));
    }
    hit_regions
}

/// Shuffle, previous, play/pause, next, repeat, centred under the track lines
/// the way a phone puts them under the cover.
///
/// Play is the one button drawn as a solid block rather than a glyph on the
/// surface, which is how the centre of that row reads as the primary control
/// without a circle the cell grid cannot draw.
fn transport(
    buffer: &mut Buffer,
    content_area: Rect,
    row_y: u16,
    view: &SidebarView<'_>,
) -> Vec<(Rect, Hit)> {
    let theme = view.theme;
    let icons = view.icons;
    let player_state = view.player_state;

    let shuffle_icon = match player_state.shuffle {
        true => icons.shuffle,
        false => icons.shuffle_off,
    };
    let repeat_icon = match player_state.repeat {
        Repeat::Off => icons.repeat_off,
        Repeat::All => icons.repeat,
        Repeat::One => icons.repeat_one,
    };
    let playback_icon = icons.transport(player_state);

    let enabled_style = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let disabled_style = Style::new().fg(theme.foreground).bg(theme.surface);
    let primary_style = Style::new()
        .fg(theme.surface)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD);

    let transport_buttons = [
        (
            shuffle_icon,
            if player_state.shuffle {
                enabled_style
            } else {
                disabled_style
            },
            Hit::Shuffle,
        ),
        (icons.previous, disabled_style, Hit::Previous),
        (playback_icon, primary_style, Hit::PlayPause),
        (icons.next, disabled_style, Hit::Next),
        (
            repeat_icon,
            if player_state.repeat == Repeat::Off {
                disabled_style
            } else {
                enabled_style
            },
            Hit::Repeat,
        ),
    ];

    // Every button is padded to the same cells, so the row is symmetric even
    // where one glyph is wider than the rest.
    let button_widths: Vec<usize> = transport_buttons
        .iter()
        .map(|(icon, ..)| width_of(icon) + 2)
        .collect();
    let total_width = button_widths.iter().sum::<usize>() + BUTTON_GAP * (transport_buttons.len() - 1);
    let left_padding = (content_area.width as usize).saturating_sub(total_width) / 2;

    let mut line_spans = vec![spaces(left_padding)];
    let mut hit_regions = Vec::new();
    let mut next_column = left_padding;
    for (button_index, (button_icon, button_style, hit_target)) in transport_buttons.into_iter().enumerate() {
        if button_index > 0 {
            line_spans.push(spaces(BUTTON_GAP));
            next_column += BUTTON_GAP;
        }
        let icon_padding = button_widths[button_index] - width_of(button_icon);
        let button_text = format!(
            "{}{button_icon}{}",
            " ".repeat(icon_padding / 2),
            " ".repeat(icon_padding - icon_padding / 2)
        );
        hit_regions.push((
            Rect::new(
                content_area.x + next_column as u16,
                row_y,
                button_widths[button_index] as u16,
                1,
            ),
            hit_target,
        ));
        line_spans.push(span(button_text, button_style));
        next_column += button_widths[button_index];
    }
    put(buffer, content_area, row_y, line_spans);
    hit_regions
}

/// The keys box and the up next box, stacked on the sidebar's bottom edge so
/// the gap opens above them rather than under the last track.
///
/// Where both fit, both get what they ask for. Where they do not, the hints
/// give ground to half the room before up next gives any, and whatever up next
/// then leaves goes back to the hints. Letting the hints take their rows first
/// starved up next down to nothing on a short terminal.
fn boxes(buffer: &mut Buffer, area: Rect, view: &SidebarView<'_>) -> Vec<(Rect, Hit)> {
    let available_height = area.height;
    let desired_queue_height = match view.upcoming_tracks.is_empty() {
        true => 0,
        false => view.upcoming_tracks.len().min(QUEUE_ROWS) as u16 + 2,
    };
    let mut keys_height = KEYS_BOX_HEIGHT;
    let mut queue_height = desired_queue_height;
    if keys_height + queue_height > available_height {
        keys_height = KEYS_BOX_HEIGHT.min(available_height / 2);
        queue_height = desired_queue_height.min(available_height.saturating_sub(keys_height));
        keys_height = KEYS_BOX_HEIGHT.min(available_height - queue_height);
    }
    if keys_height < KEYS_MIN_HEIGHT {
        keys_height = 0;
        queue_height = desired_queue_height.min(available_height);
    }

    let bottom = area.y + available_height;
    let mut hit_regions = queue_box(
        buffer,
        Rect::new(area.x, bottom - queue_height, area.width, queue_height),
        view,
    );
    let keys_bottom = bottom - queue_height;
    hit_regions.extend(contextual_keys_box(
        buffer,
        Rect::new(area.x, keys_bottom - keys_height, area.width, keys_height),
        view.theme,
        view.hint_context,
    ));
    hit_regions
}

/// The queue in a single-line box, the way tmux draws a pane, with the
/// heading written into the top border. Shows `QUEUE_ROWS` tracks when the
/// room is there and fewer when it is not, so the box never runs off the
/// bottom of a short terminal. Its caller sits it on the bottom edge.
fn queue_box(buffer: &mut Buffer, area: Rect, view: &SidebarView<'_>) -> Vec<(Rect, Hit)> {
    let theme = view.theme;
    if area.height < QUEUE_MIN_HEIGHT || area.width < 8 || view.upcoming_tracks.is_empty() {
        return Vec::new();
    }
    let seam = Style::new().fg(theme.gutter);
    let visible_track_count = (area.height as usize - 2)
        .min(QUEUE_ROWS)
        .min(view.upcoming_tracks.len());
    let box_height = visible_track_count as u16 + 2;
    let box_width = area.width as usize;

    let heading = " UP NEXT ";
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

    let mut hit_regions = Vec::new();
    for (queue_offset, queue_track) in view.upcoming_tracks.iter().take(visible_track_count).enumerate() {
        let row_y = area.y + 1 + queue_offset as u16;
        put(
            buffer,
            area,
            row_y,
            vec![
                span("\u{2502} ", seam),
                span(format!("{:<2} ", queue_offset + 1), Style::new().fg(theme.muted)),
                span(
                    clip(&queue_track.title, box_width.saturating_sub(7)),
                    Style::new().fg(theme.dim),
                ),
            ],
        );
        // The right seam is written after the title so a title that fills the
        // row cannot paint over it.
        if let Some(cell) = buffer.cell_mut((area.x + area.width - 1, row_y)) {
            cell.set_symbol("\u{2502}").set_style(seam);
        }
        hit_regions.push((Rect::new(area.x, row_y, area.width, 1), Hit::UpNext(queue_offset)));
    }

    put(
        buffer,
        area,
        area.y + box_height - 1,
        vec![
            span("\u{2514}", seam),
            span("\u{2500}".repeat(box_width.saturating_sub(2)), seam),
            span("\u{2518}", seam),
        ],
    );
    hit_regions
}
