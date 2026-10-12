//! One row of a page, and the flattening of a page into rows.
//!
//! Columns are told apart by a hairline in the gutter colour rather than by a
//! box, which is the one rule the rice keeps everywhere. The marker column is
//! the same width for a track and for a browsable, so a mixed shelf lines up.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use redwax_ytm::{Item, Page, Shelf, Track};

use crate::icons::Icons;
use crate::theme::Theme;
use crate::ui::anim::{self, Motion};
use crate::ui::{RULE, cell, span};

/// The state icon, the equaliser and one cell of gutter.
pub const MARKER_WIDTH: usize = 2 + anim::EQUALISER_WIDTH + 1;

/// The hairline plus the cell of padding after it. The padding before it is the
/// gutter cell `cell` already leaves at the end of every column.
const SEPARATOR: &str = "\u{2502} ";
const SEPARATOR_WIDTH: usize = 2;

const HEART_WIDTH: usize = 2;
const DURATION_WIDTH: usize = 7;

/// A line of the flattened page. Only `Item` rows take the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// The gap between two shelves.
    Blank,
    Header(usize),
    /// The column names over a shelf that holds tracks.
    Columns,
    Item(usize, usize),
    /// The page subtitle, shown when a page has no items at all.
    Note,
}

impl Row {
    pub fn selectable(self) -> bool {
        matches!(self, Row::Item(_, _))
    }
}

/// Everything a row says, folded to lower case, for the on-page find. A row
/// that is not an item has no text worth matching, so it never matches.
pub fn row_text(page: &Page, row: Row) -> String {
    let Row::Item(shelf_index, item_index) = row else {
        return String::new();
    };
    let searchable_text = match &page.shelves[shelf_index].items[item_index] {
        Item::Track(track) => format!(
            "{} {} {}",
            track.title,
            track.artist_line(),
            track.album.as_deref().unwrap_or("")
        ),
        Item::Album(album) => album.title.clone(),
        Item::Playlist(playlist) => playlist.title.clone(),
        Item::Artist(artist) => artist.name.clone(),
    };
    searchable_text.to_lowercase()
}

/// What a track row shows about the track beyond its own fields.
#[derive(Debug, Clone, Copy)]
pub struct RowMarks {
    pub playing: bool,
    /// Already gone by. Drawn flat, so the eye finds the playing row without
    /// having to read the list.
    pub played: bool,
    pub liked: bool,
    pub motion: Motion,
}

/// Shelf titles are dropped only when the page is one plain track list, which
/// is what an album or the queue is. A shelf with tracks in it gets the column
/// names whether or not it kept its title, because the grid is what they name.
pub fn page_rows(page: &Page) -> Vec<Row> {
    let show_headers = page.shelves.len() > 1 || page.shelves.iter().any(|shelf| shelf.title != "Tracks");
    let mut display_rows = Vec::new();
    for (shelf_index, shelf) in page.shelves.iter().enumerate() {
        if show_headers {
            if shelf_index > 0 {
                display_rows.push(Row::Blank);
            }
            display_rows.push(Row::Header(shelf_index));
        }
        if shelf.items.iter().any(|item| matches!(item, Item::Track(_))) {
            display_rows.push(Row::Columns);
        }
        // The home feed puts songs, playlists and artists in one shelf, which
        // reads as a table whose columns keep changing meaning. Only the first
        // shelf of a page is grouped, and only when it holds more than one
        // kind, so every other page keeps the order YouTube sent.
        let item_order: Vec<usize> = match shelf_index == 0 {
            true => grouped(shelf),
            false => (0..shelf.items.len()).collect(),
        };
        for item_index in item_order {
            display_rows.push(Row::Item(shelf_index, item_index));
        }
    }
    if display_rows.is_empty() && page.subtitle.is_some() {
        display_rows.push(Row::Note);
    }
    display_rows
}

/// The shelf's items by kind, songs then albums then playlists then artists,
/// keeping the order YouTube sent inside each kind. A shelf of one kind comes
/// back untouched, which is every shelf but the top of the home feed.
fn grouped(shelf: &Shelf) -> Vec<usize> {
    let item_kind_rank = |item: &Item| match item {
        Item::Track(_) => 0u8,
        Item::Album(_) => 1,
        Item::Playlist(_) => 2,
        Item::Artist(_) => 3,
    };
    let mut ordered_indices: Vec<usize> = (0..shelf.items.len()).collect();
    ordered_indices.sort_by_key(|item_index| item_kind_rank(&shelf.items[*item_index]));
    ordered_indices
}

pub fn shelf_header(shelf: &Shelf, theme: &Theme) -> Vec<Span<'static>> {
    vec![
        span(RULE, Style::new().fg(theme.accent)),
        span(
            shelf.title.to_uppercase(),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        span("  ", Style::new()),
        span(shelf.items.len().to_string(), Style::new().fg(theme.muted)),
    ]
}

pub fn track_row(
    track: &Track,
    row_width: usize,
    icons: &Icons,
    theme: &Theme,
    marks: RowMarks,
) -> Vec<Span<'static>> {
    let accent_bold = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let playback_marker = match marks.playing {
        true => span(
            format!(
                "{} {} ",
                icons.play,
                anim::equaliser(marks.motion.animation_frame, marks.motion.playback_moving,)
            ),
            accent_bold,
        ),
        false => span(" ".repeat(MARKER_WIDTH), Style::new()),
    };
    let liked_marker = match marks.liked {
        true => span(format!(" {}", icons.liked), Style::new().fg(theme.accent)),
        false => span(" ".repeat(HEART_WIDTH), Style::new()),
    };
    // Home feed rows carry no duration, and a blank beats a row of --:--.
    let duration_text = match track.duration_seconds {
        Some(_) => track.duration_text(),
        None => String::new(),
    };
    let duration_span = span(
        format!("{duration_text:>DURATION_WIDTH$}"),
        Style::new().fg(theme.muted),
    );

    let title_style = match (marks.playing, marks.played) {
        (true, _) => accent_bold,
        (false, true) => Style::new().fg(theme.muted),
        (false, false) => Style::new().add_modifier(Modifier::BOLD),
    };
    let detail_style = match marks.played {
        true => Style::new().fg(theme.muted),
        false => Style::new().fg(theme.dim),
    };
    let album_style = Style::new().fg(theme.muted);

    let mut line_spans = vec![playback_marker];
    line_spans.extend(laid_out(
        row_width,
        theme,
        [
            &track.title,
            &track.artist_line(),
            track.album.as_deref().unwrap_or(""),
        ],
        [title_style, detail_style, album_style],
    ));
    line_spans.push(liked_marker);
    line_spans.push(duration_span);
    line_spans
}

/// Three texts on the column boundaries `width` gives, hairlines between them,
/// dropping the columns a narrow row has no room for.
///
/// Every row of a page goes through here, which is what keeps the seams of a
/// playlist row over the seams of the track row above it.
fn laid_out(
    row_width: usize,
    theme: &Theme,
    column_texts: [&str; 3],
    column_styles: [Style; 3],
) -> Vec<Span<'static>> {
    let seam = || span(SEPARATOR, Style::new().fg(theme.gutter));
    let mut line_spans = Vec::new();
    match columns(row_width) {
        Columns::Three { title, artist, album } => {
            for (column_text, (column_width, column_style)) in column_texts
                .iter()
                .zip([title, artist, album].into_iter().zip(column_styles))
            {
                line_spans.push(cell(column_text, column_width, column_style));
                line_spans.push(seam());
            }
        }
        Columns::Two { title, artist } => {
            for (column_text, (column_width, column_style)) in column_texts
                .iter()
                .zip([title, artist].into_iter().zip(column_styles))
            {
                line_spans.push(cell(column_text, column_width, column_style));
                line_spans.push(seam());
            }
        }
        Columns::One(title_width) => {
            line_spans.push(cell(column_texts[0], title_width, column_styles[0]));
        }
    }
    line_spans
}

/// The column names over a shelf of tracks, on the boundaries and seams the
/// rows under it use. The narrow layout names nothing, because one column
/// needs no naming.
pub fn column_header(row_width: usize, theme: &Theme) -> Vec<Span<'static>> {
    let muted = Style::new().fg(theme.muted);
    if matches!(columns(row_width), Columns::One(_)) {
        return Vec::new();
    }
    let mut line_spans = vec![span(" ".repeat(MARKER_WIDTH), Style::new())];
    line_spans.extend(laid_out(
        row_width,
        theme,
        ["TITLE", "ARTIST", "ALBUM"],
        [muted; 3],
    ));
    line_spans.push(span(" ".repeat(HEART_WIDTH), Style::new()));
    line_spans.push(span(format!("{:>DURATION_WIDTH$}", "TIME"), muted));
    line_spans
}

/// Which columns a row of `width` cells holds, so the header and the rows
/// under it are laid out by one function rather than by two that agree.
enum Columns {
    Three {
        title: usize,
        artist: usize,
        album: usize,
    },
    Two {
        title: usize,
        artist: usize,
    },
    One(usize),
}

fn columns(row_width: usize) -> Columns {
    let text_width = row_width.saturating_sub(12);
    if text_width >= 72 {
        let available_width = text_columns_width(row_width, 3);
        let title = available_width * 45 / 100;
        let artist = available_width * 28 / 100;
        return Columns::Three {
            title,
            artist,
            album: available_width - title - artist,
        };
    }
    if text_width >= 40 {
        let available_width = text_columns_width(row_width, 2);
        let title = available_width * 58 / 100;
        return Columns::Two {
            title,
            artist: available_width - title,
        };
    }
    Columns::One(text_columns_width(row_width, 0))
}

/// Cells the text columns share once the marker, separator hairlines, the heart
/// and the duration have taken theirs. One cell stays spare, as it always was.
fn text_columns_width(row_width: usize, separator_count: usize) -> usize {
    row_width
        .saturating_sub(MARKER_WIDTH + separator_count * SEPARATOR_WIDTH + HEART_WIDTH + DURATION_WIDTH + 1)
}

/// A playlist, an album or an artist, on the columns the track rows use.
///
/// The columns were sized from the text before, so every row put its detail at
/// a different column and the text of one row ran under the heading of the
/// next. It reads as one table now: the name under TITLE, whatever the row
/// says about itself under ARTIST, and what kind of thing it is under ALBUM.
pub fn browsable_row(
    browsable_item: &Item,
    row_width: usize,
    icons: &Icons,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let (item_icon, title, detail_text, item_kind_label) = match browsable_item {
        Item::Album(album) => (
            icons.album,
            album.title.clone(),
            album.artists.join(", "),
            match &album.year {
                Some(year) => format!("Album \u{b7} {year}"),
                None => "Album".to_string(),
            },
        ),
        Item::Playlist(playlist) => (
            icons.playlist,
            playlist.title.clone(),
            playlist.subtitle.clone().unwrap_or_default(),
            "Playlist".to_string(),
        ),
        Item::Artist(artist) => (
            icons.artist,
            artist.name.clone(),
            String::new(),
            "Artist".to_string(),
        ),
        Item::Track(track) => (
            icons.track,
            track.title.clone(),
            track.artist_line(),
            track.album.clone().unwrap_or_default(),
        ),
    };

    let detail_style = Style::new().fg(theme.dim);
    let kind_style = Style::new().fg(theme.muted);
    let mut line_spans = vec![cell(item_icon, MARKER_WIDTH, detail_style)];
    line_spans.extend(laid_out(
        row_width,
        theme,
        [&title, &detail_text, &item_kind_label],
        [
            Style::new().add_modifier(Modifier::BOLD),
            detail_style,
            kind_style,
        ],
    ));
    // The trailing columns are blank rather than absent, so the seams of a
    // browsable row land where the seams of a track row do.
    line_spans.push(span(" ".repeat(HEART_WIDTH), Style::new()));
    line_spans.push(span(" ".repeat(DURATION_WIDTH), Style::new()));
    line_spans
}
