//! A browser pane: the search box on the search tab, the breadcrumb line, and
//! the page itself as rows. The scrollbar column is reserved whether or not a
//! scrollbar is drawn, because a row sized without it overflows by one cell the
//! moment the page grows.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use redwax_ytm::{Item, Page};

use crate::app::{Browser, Target};
use crate::icons::Icons;
use crate::theme::Theme;
use crate::ui::anim::Motion;
use crate::ui::hit::{Hit, RowTarget};
use crate::ui::{Row, RowMarks, browsable_row, fill, put, shelf_header, span, track_row, width_of};

/// The scrollbar thumb and the track it runs in.
const THUMB: &str = "\u{2503}";
const TRACK: &str = "\u{2502}";

/// Cells a row loses to the option padding and the scrollbar column.
const ROW_CHROME: u16 = 3;

/// Crumbs kept before the path is elided, the current page included.
const CRUMBS_SHOWN: usize = 3;

/// The separator between two crumbs, and the gap before the page subtitle.
const CRUMB_SEPARATOR: &str = "  \u{203a}  ";
const SUBTITLE_GAP: &str = "    \u{b7}    ";

pub struct BrowserView<'a> {
    pub browser: &'a Browser,
    /// Which browser this is, so a click on a row names the list it is in.
    pub browser_target: Target,
    pub icons: &'a Icons,
    pub theme: &'a Theme,
    pub playing_track: Playing<'a>,
    pub liked_overrides: &'a HashMap<String, bool>,
    /// Drawn only on the search tab.
    pub search_box: Option<SearchBox<'a>>,
    /// The on-page find, drawn over the crumbs while its bar is open.
    pub find_bar: Option<FindBar<'a>>,
    pub list_focused: bool,
    pub motion: Motion,
}

/// How the page names the track that is playing.
///
/// A browse page holds each track once, so an id is enough and finding the
/// playing row costs nothing. A queue can hold the same track many times, and
/// an id would light every copy, so its rows are named by position instead.
/// One enum rather than two optional fields, because there is no such thing as
/// both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playing<'a> {
    Nothing,
    Track(&'a str),
    /// The item index in the shelf, which also says what is behind it.
    Row(usize),
}

pub struct SearchBox<'a> {
    pub query: &'a str,
    pub focused: bool,
    pub cursor_byte_index: usize,
    pub editing_mode: &'static str,
}

pub struct FindBar<'a> {
    pub query: &'a str,
    /// False when nothing on the page holds the query, which the bar says.
    pub has_match: bool,
    /// One-based position and total matches in the visible projection.
    pub match_position: Option<(usize, usize)>,
}

/// The rows visible after scrolling, so the caller can move by a page. The
/// crumbs keep a blank row under them, which is the row past the old count.
pub fn visible_height(area: Rect, searchable: bool) -> u16 {
    let chrome_rows = if searchable { 5 } else { 4 };
    area.height.saturating_sub(chrome_rows)
}

pub struct BrowserPaint {
    pub hit_regions: Vec<(Rect, Hit)>,
    pub scroll_offset: Option<usize>,
    pub visible_rows: u16,
}

pub fn browser(buffer: &mut Buffer, area: Rect, view: BrowserView<'_>) -> BrowserPaint {
    let minimum_height = if view.search_box.is_some() { 6 } else { 5 };
    if area.width < 6 || area.height < minimum_height {
        return BrowserPaint {
            hit_regions: Vec::new(),
            scroll_offset: None,
            visible_rows: 0,
        };
    }
    let theme = view.theme;
    let content_area = Rect::new(area.x + 1, area.y, area.width - 2, area.height);
    let mut next_row_y = content_area.y;
    let mut hit_regions = Vec::new();

    // The box is the first row of the pane, so the thing being typed into is
    // the thing at the top rather than something floating below the tabs.
    if let Some(search_box_view) = &view.search_box {
        let search_area = Rect::new(content_area.x + 1, next_row_y, content_area.width - 2, 1);
        search_box(buffer, search_area, search_box_view, theme);
        hit_regions.push((search_area, Hit::Search));
        next_row_y += 1;
    }
    next_row_y += 1;
    let crumb_area = Rect::new(
        content_area.x + 1,
        next_row_y,
        content_area.width.saturating_sub(2),
        1,
    );
    match &view.find_bar {
        // The bar stands where the crumbs do, so the page under it does not
        // move by a row the moment a find opens.
        Some(find_bar_view) => find_bar(buffer, crumb_area, find_bar_view, theme),
        None => hit_regions.extend(crumbs(buffer, crumb_area, view.browser, theme)),
    }
    next_row_y += 3;

    let list_height = (content_area.y + content_area.height).saturating_sub(next_row_y);
    if list_height == 0 {
        return BrowserPaint {
            hit_regions,
            scroll_offset: None,
            visible_rows: 0,
        };
    }
    let list_area = Rect::new(content_area.x, next_row_y, content_area.width, list_height);
    let scroll_offset = view.browser.viewport_scroll(list_height as usize);
    hit_regions.extend(page_list(buffer, list_area, view, scroll_offset));
    BrowserPaint {
        hit_regions,
        scroll_offset: Some(scroll_offset),
        visible_rows: list_height,
    }
}

fn search_box(buffer: &mut Buffer, area: Rect, search_box_view: &SearchBox<'_>, theme: &Theme) {
    let background = match search_box_view.focused {
        true => theme.panel,
        false => theme.surface,
    };
    fill(buffer, area, background);
    let mut line_spans = Vec::new();
    if search_box_view.focused {
        line_spans.push(span(
            format!(" {} ", search_box_view.editing_mode),
            Style::new()
                .fg(theme.background)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        line_spans.push(span(" ", Style::new().bg(background)));
        let query_width = (area.width as usize).saturating_sub(width_of(search_box_view.editing_mode) + 3);
        let cursor_byte_index = search_box_view
            .query
            .floor_char_boundary(search_box_view.cursor_byte_index.min(search_box_view.query.len()));
        let query_before_cursor = &search_box_view.query[..cursor_byte_index];
        let cells_to_hide = width_of(query_before_cursor).saturating_sub(query_width.saturating_sub(1));
        let mut hidden_cells = 0;
        let mut visible_start_byte = 0;
        for (byte_index, character) in query_before_cursor.char_indices() {
            if hidden_cells >= cells_to_hide {
                break;
            }
            hidden_cells += width_of(&character.to_string());
            visible_start_byte = byte_index + character.len_utf8();
        }
        let text_style = Style::new().fg(theme.foreground).bg(background);
        line_spans.push(span(
            &search_box_view.query[visible_start_byte..cursor_byte_index],
            text_style,
        ));
        let cursor_character = search_box_view.query[cursor_byte_index..].chars().next();
        let normal_mode = search_box_view.editing_mode == "NORMAL";
        let mut cursor_text = match (normal_mode, cursor_character) {
            (true, Some(character)) => character.to_string(),
            _ => "\u{258f}".to_string(),
        };
        let cursor_room = query_width.saturating_sub(width_of(
            &search_box_view.query[visible_start_byte..cursor_byte_index],
        ));
        if width_of(&cursor_text) > cursor_room {
            cursor_text = "\u{2588}".to_string();
        }
        line_spans.push(span(
            cursor_text,
            Style::new().fg(theme.background).bg(theme.accent),
        ));
        let text_after_cursor_byte = cursor_byte_index
            + if normal_mode {
                cursor_character.map_or(0, char::len_utf8)
            } else {
                0
            };
        line_spans.push(span(&search_box_view.query[text_after_cursor_byte..], text_style));
        if search_box_view.query.is_empty() {
            line_spans.push(span(
                " search youtube music",
                Style::new().fg(theme.muted).bg(background),
            ));
        }
        let editing_hint = match search_box_view.editing_mode {
            "NORMAL" => "   i insert · enter search · esc controls",
            "INSERT" => "   type · enter search · esc normal",
            _ => "   type · enter search · esc controls",
        };
        line_spans.push(span(editing_hint, Style::new().fg(theme.muted).bg(background)));
    } else {
        let unfocused_text = match search_box_view.query.is_empty() {
            true => " / search youtube music".to_string(),
            false => format!(" {}   / search again", search_box_view.query),
        };
        line_spans.push(span(unfocused_text, Style::new().fg(theme.muted).bg(background)));
    }
    put(buffer, area, area.y, line_spans);
}

/// `/query`, the way vim puts a search at the bottom, plus what to press to
/// keep or drop the row it reached.
fn find_bar(buffer: &mut Buffer, area: Rect, find_bar_view: &FindBar<'_>, theme: &Theme) {
    let accent = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let query_color = match find_bar_view.has_match {
        true => theme.foreground,
        false => theme.muted,
    };
    let mut line_spans = vec![
        span("/", accent),
        span(find_bar_view.query.to_string(), Style::new().fg(query_color)),
        span("\u{2588}", accent),
    ];
    let status_text = match (
        find_bar_view.query.is_empty(),
        find_bar_view.has_match,
        find_bar_view.match_position,
    ) {
        (true, _, _) => "   find on this page".to_string(),
        (false, true, Some((position, total))) => {
            format!("   {position}/{total} · ctrl+n next · ctrl+p previous · enter keep · escape restore")
        }
        (false, true, None) => "   enter keep · escape restore".to_string(),
        (false, false, _) => "   no match on this page".to_string(),
    };
    line_spans.push(span(status_text, Style::new().fg(theme.muted)));
    put(buffer, area, area.y, line_spans);
}

/// The path to this page, upper case, with the current page lit. The crumbs
/// are pointer targets, so a click on one goes back to that depth.
fn crumbs(buffer: &mut Buffer, area: Rect, browser: &Browser, theme: &Theme) -> Vec<(Rect, Hit)> {
    let Some(page) = browser.top() else {
        return Vec::new();
    };
    let muted = Style::new().fg(theme.muted);
    let accent = Style::new().fg(theme.accent);
    let shown_crumbs = browser.depth().min(CRUMBS_SHOWN);
    let first_shown_depth = browser.depth() - shown_crumbs;
    // Depth is the index into the history, so a click can truncate to it. The
    // ellipsis stands for the pages before the first shown one and takes none.
    let mut breadcrumbs: Vec<(Option<usize>, String)> = browser
        .pages()
        .skip(first_shown_depth)
        .enumerate()
        .map(|(history_offset, history_page)| {
            (
                Some(first_shown_depth + history_offset),
                history_page.title.to_uppercase(),
            )
        })
        .collect();
    if first_shown_depth > 0 {
        breadcrumbs.insert(0, (None, "\u{2026}".to_string()));
    }

    let mut line_spans = Vec::new();
    let mut hit_regions = Vec::new();
    let mut next_column = 0usize;
    let last_crumb_index = breadcrumbs.len() - 1;
    for (crumb_index, (history_depth, crumb_text)) in breadcrumbs.into_iter().enumerate() {
        let style = match crumb_index == last_crumb_index {
            true => accent.add_modifier(Modifier::BOLD),
            false => Style::new().fg(theme.dim),
        };
        let crumb_width = width_of(&crumb_text);
        if let Some(history_depth) = history_depth.filter(|_| next_column < area.width as usize) {
            let clickable_width = crumb_width.min(area.width as usize - next_column) as u16;
            if clickable_width > 0 {
                let crumb_area = Rect::new(area.x + next_column as u16, area.y, clickable_width, 1);
                hit_regions.push((crumb_area, Hit::Crumb(history_depth)));
            }
        }
        line_spans.push(span(crumb_text, style));
        next_column += crumb_width;
        if crumb_index != last_crumb_index {
            line_spans.push(span(CRUMB_SEPARATOR, accent));
            next_column += width_of(CRUMB_SEPARATOR);
        }
    }
    if let Some(subtitle) = &page.subtitle {
        line_spans.push(span(SUBTITLE_GAP, muted));
        line_spans.push(span(subtitle.clone(), muted));
    }
    if browser.busy() {
        line_spans.push(span("   ", Style::new()));
        line_spans.push(span("loading\u{2026}", accent));
    }
    if browser.filter() != crate::browser::ItemKind::All {
        line_spans.push(span(
            format!("   FILTER: {}", browser.filter().label()),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    }
    put(buffer, area, area.y, line_spans);
    hit_regions
}

fn page_list(
    buffer: &mut Buffer,
    area: Rect,
    view: BrowserView<'_>,
    scroll_offset: usize,
) -> Vec<(Rect, Hit)> {
    let BrowserView {
        browser,
        browser_target,
        icons,
        theme,
        playing_track,
        liked_overrides,
        list_focused,
        motion,
        ..
    } = view;
    fill(buffer, area, theme.background);
    // The list itself is a region, so the wheel knows which list it is over
    // even where the pointer rests past the last row.
    let mut hit_regions = vec![(area, Hit::List(browser_target))];
    let Some(page) = browser.top() else {
        return hit_regions;
    };
    let display_rows = browser.rows();
    let visible_row_count = area.height as usize;
    if visible_row_count == 0 || area.width <= ROW_CHROME {
        return hit_regions;
    }
    let row_width = (area.width - ROW_CHROME) as usize;

    let row_context = RowContext {
        icons,
        theme,
        playing_track,
        liked_overrides,
        motion,
    };
    let highlight = match list_focused {
        true => theme.panel,
        false => crate::ui::mix(theme.panel, theme.background, 0.6),
    };
    for (visible_row_offset, display_row) in display_rows
        .iter()
        .skip(scroll_offset)
        .take(visible_row_count)
        .enumerate()
    {
        let display_row_index = scroll_offset + visible_row_offset;
        let row_y = area.y + visible_row_offset as u16;
        let line_spans = row_spans(*display_row, page, row_width, &row_context);
        if browser.cursor() == Some(display_row_index) && display_row.selectable() {
            fill(buffer, Rect::new(area.x, row_y, area.width - 1, 1), highlight);
        }
        put(
            buffer,
            Rect::new(area.x + 1, row_y, area.width - 1, 1),
            row_y,
            line_spans,
        );
        if display_row.selectable() {
            let Row::Item(shelf_index, item_index) = display_row else {
                continue;
            };
            hit_regions.push((
                Rect::new(area.x, row_y, area.width - 1, 1),
                Hit::Row(RowTarget {
                    browser_target,
                    projection_revision: browser.revision(),
                    shelf_index: *shelf_index,
                    item_index: *item_index,
                    item: page.shelves[*shelf_index].items[*item_index].clone(),
                }),
            ));
        }
    }
    scrollbar(buffer, area, display_rows.len(), scroll_offset, theme);
    hit_regions
}

/// What a row needs beyond the row itself, so the signature stays short.
struct RowContext<'a> {
    icons: &'a Icons,
    theme: &'a Theme,
    playing_track: Playing<'a>,
    liked_overrides: &'a HashMap<String, bool>,
    motion: Motion,
}

fn row_spans(
    display_row: Row,
    page: &Page,
    row_width: usize,
    row_context: &RowContext<'_>,
) -> Vec<Span<'static>> {
    let RowContext {
        icons,
        theme,
        playing_track,
        liked_overrides,
        motion,
    } = row_context;
    let (icons, theme, playing_track) = (*icons, *theme, *playing_track);
    match display_row {
        Row::Blank => Vec::new(),
        Row::Header(shelf_index) => shelf_header(&page.shelves[shelf_index], theme),
        Row::Columns => crate::ui::rows::column_header(row_width, theme),
        Row::Note => vec![span(
            page.subtitle.clone().unwrap_or_default(),
            Style::new().fg(theme.muted),
        )],
        Row::Item(shelf_index, item_index) => match &page.shelves[shelf_index].items[item_index] {
            Item::Track(track) => {
                let is_liked = liked_overrides
                    .get(&track.video_id)
                    .copied()
                    .unwrap_or(track.liked);
                let (is_playing, was_played) = match playing_track {
                    Playing::Nothing => (false, false),
                    Playing::Track(video_id) => (video_id == track.video_id.as_str(), false),
                    Playing::Row(playing_index) => (playing_index == item_index, item_index < playing_index),
                };
                track_row(
                    track,
                    row_width,
                    icons,
                    theme,
                    RowMarks {
                        playing: is_playing,
                        played: was_played,
                        liked: is_liked,
                        motion: *motion,
                    },
                )
            }
            browsable_item => browsable_row(browsable_item, row_width, icons, theme),
        },
    }
}

/// Where in the list the view is, down the right edge. The thumb is a heavy
/// rule in the accent against a light one in the gutter, so it is told apart
/// by weight as well as by colour and stays readable on a mono terminal.
///
/// A list that fits gets no bar, because there is no position to report.
fn scrollbar(buffer: &mut Buffer, area: Rect, total_rows: usize, scroll_offset: usize, theme: &Theme) {
    let visible_row_count = area.height as usize;
    if total_rows <= visible_row_count {
        return;
    }
    let scrollbar_x = area.x + area.width - 1;
    let thumb_height = (visible_row_count * visible_row_count / total_rows).max(1);
    let thumb_travel = visible_row_count - thumb_height;
    let thumb_top = scroll_offset * thumb_travel / (total_rows - visible_row_count);
    for visible_row_offset in 0..visible_row_count {
        let row_is_inside_thumb =
            visible_row_offset >= thumb_top && visible_row_offset < thumb_top + thumb_height;
        let (glyph, scrollbar_color) = match row_is_inside_thumb {
            true => (THUMB, theme.accent),
            false => (TRACK, theme.gutter),
        };
        put(
            buffer,
            Rect::new(scrollbar_x, area.y + visible_row_offset as u16, 1, 1),
            area.y + visible_row_offset as u16,
            vec![span(glyph, Style::new().fg(scrollbar_color))],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_breadcrumb_targets_stop_at_the_painted_row() {
        let mut browser = Browser::default();
        browser.show(Page::new("A title that is much wider than this pane", Vec::new()));
        let area = Rect::new(4, 2, 12, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 8));

        let regions = crumbs(&mut buffer, area, &browser, &crate::theme::redwax_theme());

        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].0, area);
    }
}
