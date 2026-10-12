//! Wordmark, tab labels and the account on one line, with the seam drawn as an
//! underline beneath that line rather than a row of its own, so the bar costs
//! one row and still carries the accent under the active tab, the way the rice
//! marks the focused workspace.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::app::Tab;
use crate::icons::Icons;
use crate::theme::Theme;
use crate::ui::hit::Hit;
use crate::ui::layout::HeaderMode;
use crate::ui::{fill, put, span, width_of};

pub struct HeaderView<'a> {
    pub mode: HeaderMode,
    pub active_tab: Tab,
    pub account: &'a str,
    pub radio_loading: bool,
    pub icons: &'a Icons,
    pub theme: &'a Theme,
}

/// Returns the label of every tab as a clickable region.
pub fn header(buffer: &mut Buffer, area: Rect, view: HeaderView<'_>) -> Vec<(Rect, Hit)> {
    fill(buffer, area, view.theme.surface);
    let accent = Style::new().fg(view.theme.accent);
    let accent_bold = accent.add_modifier(Modifier::BOLD);
    let seam = Style::new()
        .underline_color(view.theme.gutter)
        .add_modifier(Modifier::UNDERLINED);
    let under_active = Style::new()
        .underline_color(view.theme.accent)
        .add_modifier(Modifier::UNDERLINED);

    if view.mode == HeaderMode::Compact {
        return compact_header(buffer, area, &view, seam, accent_bold.patch(under_active));
    }

    let HeaderView {
        active_tab,
        account,
        radio_loading,
        icons,
        theme,
        mode: _,
    } = view;
    let available_width = area.width as usize;

    let mut line_spans = vec![
        span(
            format!(" {} ", icons.brand),
            Style::new().fg(Color::Rgb(0xff, 0x00, 0x00)).patch(seam),
        ),
        span("redwax", Style::new().add_modifier(Modifier::BOLD).patch(seam)),
        span(" ".repeat(4), seam),
    ];
    let mut next_column: usize = line_spans
        .iter()
        .map(|line_span| width_of(&line_span.content))
        .sum();
    let all_tabs_width: usize = Tab::ALL.iter().map(|tab| width_of(tab.label()) + 4).sum();
    let tab_padding = if next_column + all_tabs_width > available_width {
        " "
    } else {
        "  "
    };
    let mut hit_regions = Vec::new();
    for tab in Tab::ALL {
        let tab_text = format!("{tab_padding}{}{tab_padding}", tab.label());
        let tab_width = width_of(&tab_text);
        if next_column < available_width {
            let clickable_width = (available_width - next_column).min(tab_width) as u16;
            hit_regions.push((
                Rect::new(area.x + next_column as u16, area.y, clickable_width, 1),
                Hit::Tab(tab),
            ));
        }
        if tab == active_tab {
            line_spans.push(span(tab_text, accent_bold.patch(under_active)));
        } else {
            line_spans.push(span(tab_text, Style::new().fg(theme.muted).patch(seam)));
        }
        next_column += tab_width;
    }

    let loading_status = match radio_loading {
        true => format!("{} loading   ", icons.loading),
        false => String::new(),
    };
    let account_status = match account.is_empty() {
        true => loading_status,
        false => format!("{loading_status}{} {account} ", icons.user),
    };
    let gap_width = available_width
        .saturating_sub(next_column + width_of(&account_status))
        .max(1);
    line_spans.push(span(" ".repeat(gap_width), seam));
    let status_style = match radio_loading && account.is_empty() {
        true => accent,
        false => Style::new().fg(theme.dim),
    };
    line_spans.push(span(account_status, status_style.patch(seam)));
    put(buffer, area, area.y, line_spans);

    hit_regions
}

fn compact_header(
    buffer: &mut Buffer,
    area: Rect,
    view: &HeaderView<'_>,
    seam: Style,
    active_style: Style,
) -> Vec<(Rect, Hit)> {
    let HeaderView {
        active_tab,
        account,
        radio_loading,
        icons,
        theme,
        ..
    } = *view;
    let active_tab_number = Tab::ALL.iter().position(|tab| *tab == active_tab).unwrap_or(0) + 1;
    let prefix = format!(" {} redwax   ", icons.brand);
    let active_label = format!("\u{2039} {} \u{203a}", active_tab.label());
    let navigation_hint = format!("  {active_tab_number}/{}  h l views", Tab::ALL.len());
    let account_status = match (radio_loading, account.is_empty()) {
        (true, true) => format!("{} loading ", icons.loading),
        (true, false) => format!("{} loading  {} {account} ", icons.loading, icons.user),
        (false, false) => format!("{} {account} ", icons.user),
        (false, true) => String::new(),
    };
    let used_width = width_of(&prefix) + width_of(&active_label) + width_of(&navigation_hint);
    let gap_width = (area.width as usize).saturating_sub(used_width + width_of(&account_status));
    let active_tab_x = area.x + width_of(&prefix) as u16;
    let active_tab_width = width_of(&active_label).min(area.width as usize) as u16;
    let line_spans = vec![
        span(prefix, seam),
        span(active_label, active_style),
        span(navigation_hint, Style::new().fg(theme.muted).patch(seam)),
        span(" ".repeat(gap_width), seam),
        span(account_status, Style::new().fg(theme.dim).patch(seam)),
    ];
    put(buffer, area, area.y, line_spans);
    match active_tab_x < area.x + area.width {
        true => vec![(
            Rect::new(
                active_tab_x,
                area.y,
                active_tab_width.min(area.x + area.width - active_tab_x),
                1,
            ),
            Hit::Tab(active_tab),
        )],
        false => Vec::new(),
    }
}
