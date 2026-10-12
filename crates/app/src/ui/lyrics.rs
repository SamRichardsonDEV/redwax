//! The lyrics tab: a progress rule that marks where in the track the panel is,
//! then the words under it. Timed lyrics light the line the track is on and
//! follow it down the panel. Plain lyrics are the same text under the same rule.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use redwax_ytm::format_duration;
use redwax_ytm::{LyricLine, Lyrics, Track};

use crate::theme::Theme;
use crate::ui::hit::Hit;
use crate::ui::wrap_words;
use crate::ui::{RULE, clip, fill, put, span, width_of};

/// Rows the progress rule and the blank under it take from the panel.
const MARKER_ROWS: u16 = 2;

/// Cells every lyric line is indented. The current line spends the first of
/// them on its rule.
const INDENT: usize = 2;

/// Lines either side of the current one that stay in the foreground. Anything
/// further drops to muted, so the eye lands on the right place.
const NEAR_LINES: usize = 2;

/// One over the fraction of the panel the current line rests at.
const FOLLOW_AT: u16 = 3;

pub struct LyricsView<'a> {
    pub current_track: Option<&'a Track>,
    pub lyrics: Option<&'a Lyrics>,
    /// The title being fetched, which replaces the body until the words land.
    pub loading_track_title: Option<&'a str>,
    pub position_millis: u64,
    pub duration_millis: Option<u64>,
    pub scroll_offset: u16,
    /// False while the user's own scroll holds the panel, so the current line
    /// stops pulling it.
    pub auto_follow: bool,
    pub theme: &'a Theme,
}

/// Keep the settled viewport so the next key press continues after auto-follow.
pub struct LyricsFrame {
    pub scroll_offset: u16,
    pub visible_rows: u16,
    pub hit_regions: Vec<(Rect, Hit)>,
}

/// The line the track is on: the last one started, which holds through the gap
/// before the next one begins.
pub fn current_line(lines: &[LyricLine], position_millis: u64) -> Option<usize> {
    lines.iter().rposition(|line| line.start_ms <= position_millis)
}

pub fn lyrics(buffer: &mut Buffer, area: Rect, view: LyricsView<'_>) -> LyricsFrame {
    let theme = view.theme;
    fill(buffer, area, theme.background);
    let mut painted_frame = LyricsFrame {
        scroll_offset: view.scroll_offset,
        visible_rows: 0,
        hit_regions: Vec::new(),
    };
    if area.width < 12 || area.height < 4 {
        return painted_frame;
    }
    let content_area = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 1);
    let muted = Style::new().fg(theme.muted);
    let content_width = content_area.width as usize;

    if view.current_track.is_none() {
        put(
            buffer,
            content_area,
            content_area.y,
            vec![span("play something to see its lyrics", muted)],
        );
        return painted_frame;
    }

    put(buffer, content_area, content_area.y, marker(&view, content_width));
    let lyrics_top = content_area.y + MARKER_ROWS;
    let visible_rows = content_area.height.saturating_sub(MARKER_ROWS);
    painted_frame.visible_rows = visible_rows;

    if let Some(loading_track_title) = view.loading_track_title {
        put(
            buffer,
            content_area,
            lyrics_top,
            vec![span(
                format!("fetching lyrics for {loading_track_title}\u{2026}"),
                muted,
            )],
        );
        return painted_frame;
    }

    match view.lyrics {
        Some(Lyrics::Timed(lyric_lines)) => {
            timed(buffer, content_area, lyrics_top, visible_rows, lyric_lines, &view)
        }
        Some(Lyrics::Plain(lyrics_text)) => {
            painted_frame.scroll_offset =
                plain(buffer, content_area, lyrics_top, visible_rows, lyrics_text, &view);
            painted_frame
        }
        None => {
            put(
                buffer,
                content_area,
                lyrics_top,
                vec![span("no lyrics for this track", muted)],
            );
            painted_frame
        }
    }
}

/// Elapsed, the rule with its head at the track's position, then the total.
/// The same arithmetic the transport's seek bar uses, so the two agree.
fn marker(view: &LyricsView<'_>, content_width: usize) -> Vec<Span<'static>> {
    let theme = view.theme;
    let dim = Style::new().fg(theme.dim);
    let accent = Style::new().fg(theme.accent);
    let elapsed_text = format_duration(Some((view.position_millis / 1000) as u32));
    let total_text = format_duration(
        view.duration_millis
            .map(|duration_millis| (duration_millis / 1000) as u32),
    );
    let bar_width = content_width
        .saturating_sub(width_of(&elapsed_text) + width_of(&total_text) + 2)
        .max(2);
    let progress_fraction = match view.duration_millis {
        Some(duration_millis) if duration_millis > 0 => {
            (view.position_millis as f64 / duration_millis as f64).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };
    let filled_width = ((bar_width as f64 * progress_fraction) as usize).min(bar_width - 1);
    vec![
        span(elapsed_text, dim),
        span(" ", dim),
        span("\u{2501}".repeat(filled_width), accent),
        span("\u{25cf}", accent.add_modifier(Modifier::BOLD)),
        span(
            "\u{2500}".repeat(bar_width - filled_width - 1),
            Style::new().fg(theme.gutter),
        ),
        span(" ", dim),
        span(total_text, dim),
    ]
}

/// One wrapped row of a lyric and the line it came from. `first` marks the row
/// that carries the rule when its line is the current one.
struct Wrapped {
    source_line_index: usize,
    text: String,
    first_wrapped_row: bool,
}

fn timed(
    buffer: &mut Buffer,
    content_area: Rect,
    lyrics_top: u16,
    visible_rows: u16,
    lyric_lines: &[LyricLine],
    view: &LyricsView<'_>,
) -> LyricsFrame {
    let theme = view.theme;
    let content_width = content_area.width as usize;
    let wrapped_rows = wrap_lines(lyric_lines, content_width.saturating_sub(INDENT));
    let current_line_index = current_line(lyric_lines, view.position_millis);

    let desired_scroll_offset = match (view.auto_follow, current_line_index) {
        (true, Some(current_line_index)) => {
            let current_row_index = wrapped_rows
                .iter()
                .position(|row| row.source_line_index == current_line_index && row.first_wrapped_row)
                .unwrap_or(0) as u16;
            current_row_index.saturating_sub(visible_rows / FOLLOW_AT)
        }
        _ => view.scroll_offset,
    };
    let scroll_offset = desired_scroll_offset.min((wrapped_rows.len() as u16).saturating_sub(visible_rows));

    let mut hit_regions = Vec::new();
    for (visible_row_offset, wrapped_row) in wrapped_rows
        .iter()
        .skip(scroll_offset as usize)
        .take(visible_rows as usize)
        .enumerate()
    {
        let row_y = lyrics_top + visible_row_offset as u16;
        let row_style =
            match current_line_index.map(|line_index| line_index.abs_diff(wrapped_row.source_line_index)) {
                Some(0) => Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                Some(1..=NEAR_LINES) | None => Style::new().fg(theme.foreground),
                Some(_) => Style::new().fg(theme.muted),
            };
        let line_prefix = match (
            current_line_index == Some(wrapped_row.source_line_index),
            wrapped_row.first_wrapped_row,
        ) {
            (true, true) => span(format!("{RULE} "), Style::new().fg(theme.accent)),
            _ => span(" ".repeat(INDENT), Style::new()),
        };
        put(
            buffer,
            content_area,
            row_y,
            vec![line_prefix, span(wrapped_row.text.clone(), row_style)],
        );
        hit_regions.push((
            Rect::new(content_area.x, row_y, content_area.width, 1),
            Hit::LyricLine(wrapped_row.source_line_index),
        ));
    }
    LyricsFrame {
        scroll_offset,
        visible_rows,
        hit_regions,
    }
}

/// Every lyric line broken to `width`, in order, tagged with the line it came
/// from. An empty line keeps a row of its own, because the gap is the shape of
/// the verse.
fn wrap_lines(lyric_lines: &[LyricLine], line_width: usize) -> Vec<Wrapped> {
    let mut wrapped_rows = Vec::new();
    for (line_index, lyric_line) in lyric_lines.iter().enumerate() {
        let (line_pieces, _) = wrap_words(&lyric_line.text, line_width.max(1), usize::MAX);
        if line_pieces.is_empty() {
            wrapped_rows.push(Wrapped {
                source_line_index: line_index,
                text: String::new(),
                first_wrapped_row: true,
            });
            continue;
        }
        for (piece_index, line_piece) in line_pieces.into_iter().enumerate() {
            wrapped_rows.push(Wrapped {
                source_line_index: line_index,
                text: line_piece,
                first_wrapped_row: piece_index == 0,
            });
        }
    }
    wrapped_rows
}

/// Untimed words, one row per line and nothing lit, because there is no
/// position to light. Returns the scroll it settled on.
fn plain(
    buffer: &mut Buffer,
    content_area: Rect,
    lyrics_top: u16,
    visible_rows: u16,
    lyrics_text: &str,
    view: &LyricsView<'_>,
) -> u16 {
    let style = Style::new().fg(view.theme.foreground);
    let content_width = content_area.width as usize;
    let total_lines = lyrics_text.lines().count() as u16;
    let scroll_offset = view.scroll_offset.min(total_lines.saturating_sub(visible_rows));
    for (visible_row_offset, lyric_line) in lyrics_text
        .lines()
        .skip(scroll_offset as usize)
        .take(visible_rows as usize)
        .enumerate()
    {
        put(
            buffer,
            content_area,
            lyrics_top + visible_row_offset as u16,
            vec![span(clip(lyric_line, content_width), style)],
        );
    }
    scroll_offset
}
