//! Fixed hints keep navigation predictable when the page changes.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Widget};

use crate::app::Tab;
use crate::keys::Action;
use crate::theme::Theme;
use crate::ui::hit::Hit;
use crate::ui::{fill, put, span, width_of};

type Hint = (&'static str, &'static str, Action, Option<Action>);

enum HintRow {
    Navigation,
    Pair([Hint; 2]),
}

const ROWS: &[HintRow] = &[
    HintRow::Navigation,
    HintRow::Pair([
        ("enter", "open", Action::Open, None),
        ("esc", "back", Action::Back, None),
    ]),
    HintRow::Pair([
        ("1-7", "views", Action::SwitchTab(Tab::Home), None),
        ("tab", "account", Action::FocusAccount, None),
    ]),
    HintRow::Pair([
        ("gg", "top", Action::First, None),
        ("G", "bottom", Action::Last, None),
    ]),
    HintRow::Pair([
        ("{", "prev section", Action::PreviousSection, None),
        ("}", "next section", Action::NextSection, None),
    ]),
    HintRow::Pair([
        ("/", "find", Action::FindOnPage, None),
        ("e", "EQ", Action::ToggleEq, None),
    ]),
    HintRow::Pair([
        ("space", "pause", Action::TogglePause, None),
        ("n/p", "skip", Action::NextTrack, Some(Action::PreviousTrack)),
    ]),
    HintRow::Pair([
        ("-/=", "volume", Action::Volume(-5), Some(Action::Volume(5))),
        ("m", "mute", Action::ToggleMute, None),
    ]),
    HintRow::Pair([
        (",", "back 5s", Action::Seek(-5), None),
        (".", "forward 5s", Action::Seek(5), None),
    ]),
    HintRow::Pair([
        ("s", "shuffle", Action::ToggleShuffle, None),
        ("R", "repeat", Action::CycleRepeat, None),
    ]),
    HintRow::Pair([
        ("q", "queue", Action::JumpToQueue, None),
        ("+", "like", Action::LikeHighlighted, None),
    ]),
    HintRow::Pair([
        ("r", "radio", Action::Radio, None),
        (
            "P/D",
            "playlist",
            Action::AddToPlaylist,
            Some(Action::RemoveFromPlaylist),
        ),
    ]),
    HintRow::Pair([
        ("b", "album", Action::OpenAlbum, None),
        ("f", "filter", Action::CycleFilter, None),
    ]),
    HintRow::Pair([
        ("?", "help", Action::ToggleHelp, None),
        ("Q", "quit", Action::Quit, None),
    ]),
];

pub const KEYS_MIN_HEIGHT: u16 = 3;
pub const KEYS_BOX_HEIGHT: u16 = ROWS.len() as u16 + 2;

pub fn keys_box(buffer: &mut Buffer, area: Rect, theme: &Theme) -> Vec<(Rect, Hit)> {
    if area.height < KEYS_MIN_HEIGHT || area.width < 12 {
        return Vec::new();
    }
    Block::bordered()
        .title("─ KEYS ")
        .border_style(Style::new().fg(theme.gutter))
        .title_style(Style::new().fg(theme.accent).add_modifier(Modifier::BOLD))
        .render(area, buffer);
    let inner = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 2);
    let mut hits = Vec::new();
    for (row, hint_row) in ROWS.iter().take(inner.height as usize).enumerate() {
        let line = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        match hint_row {
            HintRow::Navigation => hits.extend(navigation(buffer, line, theme)),
            HintRow::Pair(pair) => {
                let left_width = inner.width / 2;
                for (column, hint) in pair.iter().enumerate() {
                    let cell = if column == 0 {
                        Rect::new(line.x, line.y, left_width, 1)
                    } else {
                        Rect::new(line.x + left_width, line.y, inner.width - left_width, 1)
                    };
                    hits.extend(draw_hint(buffer, cell, hint, theme));
                }
            }
        }
    }
    hits
}

fn navigation(buffer: &mut Buffer, area: Rect, theme: &Theme) -> Vec<(Rect, Hit)> {
    put(
        buffer,
        area,
        area.y,
        vec![
            span("hjkl / arrows", Style::new().fg(theme.accent)),
            span("  move", Style::new().fg(theme.foreground)),
        ],
    );
    [
        Action::PreviousTab,
        Action::CursorDown,
        Action::CursorUp,
        Action::NextTab,
    ]
    .into_iter()
    .enumerate()
    .take(area.width as usize)
    .map(|(index, action)| {
        (
            Rect::new(area.x + index as u16, area.y, 1, 1),
            Hit::KeyHint(action),
        )
    })
    .collect()
}

fn draw_hint(buffer: &mut Buffer, area: Rect, hint: &Hint, theme: &Theme) -> Vec<(Rect, Hit)> {
    if area.is_empty() {
        return Vec::new();
    }
    let (key, label, action, secondary) = *hint;
    put(
        buffer,
        area,
        area.y,
        vec![
            span(key, Style::new().fg(theme.accent)),
            span(format!(" {label}"), Style::new().fg(theme.foreground)),
        ],
    );
    let width = (width_of(key) + 1 + width_of(label)).min(area.width as usize) as u16;
    let mut hits = vec![(Rect::new(area.x, area.y, width, 1), Hit::KeyHint(action))];
    if let Some(secondary) = secondary
        && let Some((first, second)) = key.split_once('/')
    {
        let offset = width_of(first) as u16 + 1;
        if offset < area.width {
            hits.push((
                Rect::new(
                    area.x + offset,
                    area.y,
                    (width_of(second) as u16).min(area.width - offset),
                    1,
                ),
                Hit::KeyHint(secondary),
            ));
        }
    }
    hits
}

pub fn keys_bar(buffer: &mut Buffer, area: Rect, theme: &Theme) -> Vec<(Rect, Hit)> {
    fill(buffer, area, theme.surface);
    if area.width < 6 {
        return Vec::new();
    }
    let content = Rect::new(area.x + 2, area.y, area.width - 4, 1);
    let groups: &[&[(&str, Option<Action>)]] = &[
        &[
            ("h", Some(Action::PreviousTab)),
            ("j", Some(Action::CursorDown)),
            ("k", Some(Action::CursorUp)),
            ("l", Some(Action::NextTab)),
            (" / arrows", None),
        ],
        &[
            ("space", Some(Action::TogglePause)),
            (" ", None),
            ("n", Some(Action::NextTrack)),
            ("/", None),
            ("p", Some(Action::PreviousTrack)),
        ],
        &[(",", Some(Action::Seek(-5))), (" back 5s", None)],
        &[(".", Some(Action::Seek(5))), (" forward 5s", None)],
        &[("q", Some(Action::JumpToQueue)), (" queue", None)],
        &[("+", Some(Action::LikeHighlighted)), (" like", None)],
        &[
            ("?", Some(Action::ToggleHelp)),
            (" ", None),
            ("Q", Some(Action::Quit)),
        ],
    ];
    let mut hits = Vec::new();
    let mut used = 0u16;
    for group in groups {
        let group_width = group.iter().map(|(text, _)| width_of(text)).sum::<usize>() as u16;
        if used + group_width > content.width {
            break;
        }
        for &(text, action) in *group {
            let width = width_of(text) as u16;
            let cell = Rect::new(content.x + used, content.y, width, 1);
            let color = if action.is_some() {
                theme.accent
            } else {
                theme.muted
            };
            put(buffer, cell, cell.y, vec![span(text, Style::new().fg(color))]);
            if let Some(action) = action {
                hits.push((cell, Hit::KeyHint(action)));
            }
            used += width;
        }
        used += 2;
    }
    hits
}
