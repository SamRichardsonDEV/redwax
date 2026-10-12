//! The transport, three rows tall so every part of it can be read across the
//! room. Row one is what plays, row two the seek bar between the two times,
//! row three the modes and the volume. Nothing overlaps: the title is the one
//! part that gives ground, and it gives it before the artist does.
//!
//! Every part of row two and row three is a pointer target, so the returned
//! regions are the same arithmetic the paint used.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use redwax_player::{PlaybackAvailability, PlayerState, Repeat};
use redwax_ytm::format_duration;

use crate::icons::Icons;
use crate::theme::Theme;
use crate::ui::anim::{self, Motion};
use crate::ui::hit::Hit;
use crate::ui::layout::{PlayerMode, player_mode};
use crate::ui::{fill, fit, line_width, put, spaces, span, width_of};

/// Rows the transport takes. `App::draw` reserves them under the body.
pub const PLAYER_BAR_HEIGHT: u16 = 3;

/// Cells the volume bar takes, head included.
pub const VOLUME_BAR_WIDTH: usize = 12;

/// Cells between two mode groups on the bottom row.
const GROUP_GAP: usize = 3;

/// Cells between the title and the artist on the top row.
const TITLE_GAP: usize = 3;

/// The rule that marks a mode as on, and the cells it holds when it is off, so
/// the row does not shift as a mode is toggled.
const MODE_RULE: &str = "\u{258e} ";
const MODE_RULE_WIDTH: usize = 2;

pub struct BarView<'a> {
    pub player_state: &'a PlayerState,
    /// Where the pointer is holding the bar head, as a fraction of the track.
    /// The bar follows this rather than the player while a drag is in flight,
    /// so the head goes where the hand goes and mpv is asked once, on release.
    pub seek_preview_fraction: Option<f64>,
    /// The level the pointer is holding the volume bar at, drawn instead of
    /// the player's own until the button comes up.
    pub volume_preview_percent: Option<u8>,
    /// The digits being typed into the volume field, while `v` has it open.
    pub volume_entry: Option<&'a str>,
    pub current_track_liked: bool,
    pub icons: &'a Icons,
    pub theme: &'a Theme,
    pub motion: Motion,
}

pub fn player_bar(buffer: &mut Buffer, area: Rect, view: BarView<'_>) -> Vec<(Rect, Hit)> {
    fill(buffer, area, view.theme.surface);
    if area.width < 16 || area.height < PLAYER_BAR_HEIGHT {
        return Vec::new();
    }
    let content_area = Rect::new(area.x + 2, area.y, area.width - 4, PLAYER_BAR_HEIGHT);
    let content_width = content_area.width as usize;
    put(
        buffer,
        content_area,
        content_area.y,
        now_playing(&view, content_width),
    );

    let (seek_spans, seek_area) = seek(&view, content_area);
    put(buffer, content_area, content_area.y + 1, seek_spans);
    let (mode_spans, mut hit_regions) = modes(&view, content_area);
    put(buffer, content_area, content_area.y + 2, mode_spans);
    if let Some(seek_area) = seek_area {
        hit_regions.push((seek_area, Hit::SeekBar(seek_area)));
    }
    hit_regions
}

/// The state icon, the equaliser, what plays, and the tier the stream came at
/// against the right edge.
fn now_playing(view: &BarView<'_>, width: usize) -> Vec<Span<'static>> {
    let theme = view.theme;
    let muted = Style::new().fg(theme.muted);
    let pending_track = view.player_state.pending_track.as_ref();
    let Some(current_track) = pending_track.or_else(|| view.player_state.track()) else {
        if view.player_state.availability == PlaybackAvailability::Unavailable {
            return vec![
                span(format!("{}  ", view.icons.play), Style::new().fg(theme.error)),
                span(
                    "playback unavailable   o actions to retry",
                    Style::new().fg(theme.error),
                ),
            ];
        }
        return vec![
            span(format!("{}  ", view.icons.play), muted),
            span("nothing playing", muted),
            span("   \u{b7}   / to search", muted),
        ];
    };

    // The glyph is the button, not the state: a paused player shows the play
    // triangle, because that is what pressing it does.
    let transport_icon = match pending_track {
        Some(_) => view.icons.loading,
        None => view.icons.transport(view.player_state),
    };
    let mut line_spans = vec![span(
        format!(
            "{transport_icon} {}  ",
            anim::equaliser(view.motion.animation_frame, view.motion.playback_moving,)
        ),
        Style::new().fg(theme.accent),
    )];
    if view.current_track_liked && pending_track.is_none() {
        line_spans.push(span(
            format!("{} ", view.icons.liked),
            Style::new().fg(theme.accent),
        ));
    }

    let (availability_label, availability_style) = match (pending_track, view.player_state.availability) {
        (Some(_), _) => ("resolving".to_string(), Style::new().fg(theme.muted)),
        (None, PlaybackAvailability::Unavailable) => {
            ("unavailable · o retry".to_string(), Style::new().fg(theme.error))
        }
        (None, PlaybackAvailability::Reconnecting) => {
            ("reconnecting".to_string(), Style::new().fg(theme.warning))
        }
        (None, PlaybackAvailability::RestoredPaused) => (
            "restored · space resume".to_string(),
            Style::new().fg(theme.muted),
        ),
        (None, PlaybackAvailability::Ready) => (
            view.player_state.tier_label.clone().unwrap_or_default(),
            Style::new().fg(theme.muted),
        ),
    };
    let availability_width = match availability_label.is_empty() {
        true => 0,
        false => width_of(&availability_label) + TITLE_GAP,
    };
    let artist_text = current_track.artist_line();
    let track_text_width = width
        .saturating_sub(line_width(&line_spans) + TITLE_GAP + availability_width)
        .max(2);
    let artist_width = width_of(&artist_text).min(track_text_width / 3);
    let title_width = track_text_width - artist_width;

    line_spans.push(span(
        anim::scrolling(
            &current_track.title,
            title_width,
            view.motion.animation_frame,
            view.motion.animations_enabled,
        ),
        Style::new().fg(theme.foreground).add_modifier(Modifier::BOLD),
    ));
    line_spans.push(spaces(TITLE_GAP));
    line_spans.push(span(fit(&artist_text, artist_width), muted));
    if !availability_label.is_empty() {
        let used_width = line_width(&line_spans);
        let padding_width = width.saturating_sub(used_width + width_of(&availability_label));
        line_spans.push(spaces(padding_width));
        line_spans.push(span(availability_label, availability_style));
    }
    line_spans
}

/// Elapsed, the bar, total. The bar takes every cell the two times leave, so it
/// grows with the terminal rather than staying a fixed stub. The rect is the
/// bar alone, because a click on it seeks to that fraction.
fn seek(view: &BarView<'_>, content_area: Rect) -> (Vec<Span<'static>>, Option<Rect>) {
    let theme = view.theme;
    let dim = Style::new().fg(theme.dim);
    let gutter = Style::new().fg(theme.gutter);
    let accent = Style::new().fg(theme.accent);
    let player_state = view.player_state;
    let width = content_area.width as usize;
    if player_state.pending_track.is_some() {
        return (
            vec![span(fit("Resolving audio   next to cancel", width), dim)],
            None,
        );
    }

    let (elapsed_text, total_text) = match player_state.track() {
        Some(_) => {
            let position_seconds = match (view.seek_preview_fraction, player_state.duration) {
                (Some(preview_fraction), Some(duration_seconds)) => {
                    duration_seconds * preview_fraction.clamp(0.0, 1.0)
                }
                _ => player_state.position,
            };
            (
                format_duration(Some(position_seconds as u32)),
                format_duration(
                    player_state
                        .duration
                        .map(|duration_seconds| duration_seconds as u32),
                ),
            )
        }
        None => (String::new(), String::new()),
    };
    let bar_width = width
        .saturating_sub(width_of(&elapsed_text) + width_of(&total_text) + 2)
        .max(2);
    if player_state.track().is_none() {
        return (vec![span("\u{2500}".repeat(bar_width + 2), gutter)], None);
    }

    let progress_fraction = match (view.seek_preview_fraction, player_state.duration) {
        (Some(preview_fraction), _) => preview_fraction.clamp(0.0, 1.0),
        (None, Some(duration_seconds)) if duration_seconds > 0.0 => {
            (player_state.position / duration_seconds).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };
    let filled_width = ((bar_width as f64 * progress_fraction) as usize).min(bar_width - 1);
    let seek_area = Rect::new(
        content_area.x + width_of(&elapsed_text) as u16 + 1,
        content_area.y + 1,
        bar_width as u16,
        1,
    );
    let line_spans = vec![
        span(elapsed_text, dim),
        spaces(1),
        span("\u{2501}".repeat(filled_width), accent),
        span("\u{25cf}", accent.add_modifier(Modifier::BOLD)),
        span("\u{2500}".repeat(bar_width - filled_width - 1), gutter),
        spaces(1),
        span(total_text, dim),
    ];
    (line_spans, Some(seek_area))
}

/// Shuffle, repeat and volume. Each group is a rule, an icon and a word, lit in
/// the accent when it is on and muted when it is off, so the state reads from
/// across the room rather than from the shape of one glyph.
fn modes(view: &BarView<'_>, content_area: Rect) -> (Vec<Span<'static>>, Vec<(Rect, Hit)>) {
    let theme = view.theme;
    let player_state = view.player_state;
    let icons = view.icons;
    let muted = Style::new().fg(theme.muted);
    let enabled_style = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let row_y = content_area.y + 2;

    let active_player_mode = player_mode(content_area.width + 4);
    let shuffle_icon = match player_state.shuffle {
        true => icons.shuffle,
        false => icons.shuffle_off,
    };
    let (repeat_icon, repeat_label) = match player_state.repeat {
        Repeat::Off => (icons.repeat_off, "off"),
        Repeat::All => (icons.repeat, "queue"),
        Repeat::One => (icons.repeat_one, "track"),
    };

    let mut line_spans = Vec::new();
    let mut hit_regions = Vec::new();
    let mut next_column = 0usize;

    let transport_gap = match active_player_mode {
        PlayerMode::Full => 2,
        PlayerMode::Compact => 1,
        PlayerMode::Minimal => 0,
    };
    for (transport_button_index, (button_icon, hit_target)) in [
        (icons.previous, Hit::Previous),
        (icons.transport(player_state), Hit::PlayPause),
        (icons.next, Hit::Next),
    ]
    .into_iter()
    .enumerate()
    {
        if transport_button_index > 0 {
            line_spans.push(spaces(transport_gap));
            next_column += transport_gap;
        }
        let button_text = format!(" {button_icon} ");
        let button_width = width_of(&button_text);
        hit_regions.push((
            Rect::new(content_area.x + next_column as u16, row_y, button_width as u16, 1),
            hit_target,
        ));
        line_spans.push(span(button_text, enabled_style));
        next_column += button_width;
    }
    if active_player_mode == PlayerMode::Minimal {
        return (line_spans, hit_regions);
    }
    line_spans.push(spaces(GROUP_GAP));
    next_column += GROUP_GAP;

    if active_player_mode == PlayerMode::Compact {
        for (button_icon, button_style, hit_target) in [
            (
                shuffle_icon,
                if player_state.shuffle {
                    enabled_style
                } else {
                    muted
                },
                Hit::Shuffle,
            ),
            (
                repeat_icon,
                if player_state.repeat == Repeat::Off {
                    muted
                } else {
                    enabled_style
                },
                Hit::Repeat,
            ),
        ] {
            let button_text = format!(" {button_icon} ");
            let button_width = width_of(&button_text);
            hit_regions.push((
                Rect::new(content_area.x + next_column as u16, row_y, button_width as u16, 1),
                hit_target,
            ));
            line_spans.push(span(button_text, button_style));
            line_spans.push(spaces(1));
            next_column += button_width + 1;
        }
        let (volume_icon, volume_style) = match player_state.muted {
            true => (icons.muted, muted),
            false => (icons.volume, enabled_style),
        };
        let volume_button_text = format!(" {volume_icon} ");
        let volume_button_width = width_of(&volume_button_text);
        hit_regions.push((
            Rect::new(
                content_area.x + next_column as u16,
                row_y,
                volume_button_width as u16,
                1,
            ),
            Hit::Mute,
        ));
        line_spans.push(span(volume_button_text, volume_style));
        let volume_percent = view.volume_preview_percent.unwrap_or(player_state.volume);
        line_spans.push(span(
            if player_state.muted && view.volume_preview_percent.is_none() {
                " mute".to_string()
            } else {
                format!(" {volume_percent}%")
            },
            muted,
        ));
        return (line_spans, hit_regions);
    }

    for (mode_enabled, mode_icon, mode_label, hit_target) in [
        (player_state.shuffle, shuffle_icon, "shuffle", Hit::Shuffle),
        (
            player_state.repeat != Repeat::Off,
            repeat_icon,
            repeat_label,
            Hit::Repeat,
        ),
    ] {
        let mode_spans = mode_group(mode_enabled, mode_icon, mode_label, enabled_style, muted, theme);
        let group_width = line_width(&mode_spans);
        hit_regions.push((
            Rect::new(content_area.x + next_column as u16, row_y, group_width as u16, 1),
            hit_target,
        ));
        line_spans.extend(mode_spans);
        line_spans.push(spaces(GROUP_GAP));
        next_column += group_width + GROUP_GAP;
    }

    let (volume_icon, volume_style) = match player_state.muted {
        true => (icons.muted, muted),
        false => (icons.volume, enabled_style),
    };
    let volume_icon_width = width_of(volume_icon) + 1;
    hit_regions.push((
        Rect::new(
            content_area.x + next_column as u16,
            row_y,
            volume_icon_width as u16,
            1,
        ),
        Hit::Mute,
    ));
    line_spans.push(span(format!("{volume_icon} "), volume_style));
    next_column += volume_icon_width;

    let volume_percent = view.volume_preview_percent.unwrap_or(player_state.volume);
    let show_muted_bar = player_state.muted && view.volume_preview_percent.is_none();
    let volume_bar_area = Rect::new(
        content_area.x + next_column as u16,
        row_y,
        VOLUME_BAR_WIDTH as u16,
        1,
    );
    line_spans.extend(volume_bar(volume_percent, show_muted_bar, theme));
    hit_regions.push((volume_bar_area, Hit::Volume(volume_bar_area)));
    // The readout is the field while it is open, so what is being typed shows
    // where the number it replaces was.
    match view.volume_entry {
        Some(entry) => {
            line_spans.push(span("  ", muted));
            line_spans.push(span(
                format!("{entry}\u{2588}"),
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            ));
            line_spans.push(span("%  enter, escape", muted));
        }
        None => line_spans.push(span(
            match show_muted_bar {
                true => "  muted".to_string(),
                false => format!("  {volume_percent}%"),
            },
            muted,
        )),
    }
    (line_spans, hit_regions)
}

fn mode_group(
    enabled: bool,
    icon: &str,
    label: &str,
    enabled_style: Style,
    muted: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let rule = match enabled {
        true => span(MODE_RULE, Style::new().fg(theme.accent)),
        false => spaces(MODE_RULE_WIDTH),
    };
    let style = match enabled {
        true => enabled_style,
        false => muted,
    };
    vec![rule, span(format!("{icon} {label}"), style)]
}

/// A level as a bar rather than a stack of blocks, because a bar is read at a
/// glance and five blocks are not. Muted shows the gutter with no head.
fn volume_bar(volume_percent: u8, show_muted_bar: bool, theme: &Theme) -> Vec<Span<'static>> {
    let gutter = Style::new().fg(theme.gutter);
    if show_muted_bar {
        return vec![span("\u{2500}".repeat(VOLUME_BAR_WIDTH), gutter)];
    }
    let accent = Style::new().fg(theme.accent);
    let filled_width = (VOLUME_BAR_WIDTH * usize::from(volume_percent) / 100).min(VOLUME_BAR_WIDTH - 1);
    vec![
        span("\u{2501}".repeat(filled_width), accent),
        span("\u{25cf}", accent.add_modifier(Modifier::BOLD)),
        span("\u{2500}".repeat(VOLUME_BAR_WIDTH - filled_width - 1), gutter),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_playback_names_the_request_and_disables_seeking_at_compact_widths() {
        let player_state = PlayerState {
            pending_track: Some(redwax_player::bare_track("pending", "Requested song")),
            loading: true,
            ..PlayerState::default()
        };
        let theme = crate::theme::redwax_theme();
        for width in [20, 80] {
            let area = Rect::new(0, 0, width, PLAYER_BAR_HEIGHT);
            let mut buffer = Buffer::empty(area);
            let regions = player_bar(
                &mut buffer,
                area,
                BarView {
                    player_state: &player_state,
                    seek_preview_fraction: None,
                    volume_preview_percent: None,
                    volume_entry: None,
                    current_track_liked: false,
                    icons: &crate::icons::TEXT,
                    theme: &theme,
                    motion: Motion::default(),
                },
            );
            let rendered_row = |row_y| {
                (0..width)
                    .map(|column_x| buffer[(column_x, row_y)].symbol())
                    .collect::<String>()
            };
            assert!(rendered_row(1).contains("Resolving"));
            assert!(!regions.iter().any(|(_, hit)| matches!(hit, Hit::SeekBar(_))));
            if width == 80 {
                assert!(rendered_row(0).contains("Requested song"));
                assert!(rendered_row(0).contains("resolving"));
                assert!(regions.iter().any(|(_, hit)| matches!(hit, Hit::Volume(_))));
            }
        }
    }

    #[test]
    fn unavailable_playback_names_the_recovery_route_without_a_track() {
        let player_state = PlayerState {
            availability: PlaybackAvailability::Unavailable,
            ..PlayerState::default()
        };
        let theme = crate::theme::redwax_theme();
        let view = BarView {
            player_state: &player_state,
            seek_preview_fraction: None,
            volume_preview_percent: None,
            volume_entry: None,
            current_track_liked: false,
            icons: &crate::icons::TEXT,
            theme: &theme,
            motion: Motion::default(),
        };

        let text = now_playing(&view, 80)
            .into_iter()
            .fold(String::new(), |mut text, span| {
                text.push_str(&span.content);
                text
            });

        assert!(text.contains("playback unavailable"));
        assert!(text.contains("o actions to retry"));
    }
}
