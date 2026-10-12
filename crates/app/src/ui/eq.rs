//! The equaliser, as a pane down the right with a rule between it and the
//! page. Eighteen columns rising from a baseline, the centre frequency under
//! each, and the state of the one band the keys are on.
//!
//! It takes only the rows it fills. A panel drawn the full height of the
//! screen reads as a mode the app is in, and one that stops where its content
//! stops reads as a thing beside the page, which is what it is.
//!
//! The columns are drawn a row at a time rather than a column at a time, so a
//! pane too short or too narrow for the whole panel simply paints fewer rows.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::eq::{BANDS, CENTRES, DISPLAY_MAX, Eq};
use crate::theme::Theme;
use crate::ui::hit::Hit;
use crate::ui::popup;
use crate::ui::{blank, put, spaces, span};

/// Cells one band takes when the pane has room for the labels.
const WIDE_SLOT: usize = 4;

/// Eighths of a cell, for the top of a column that lands between two rows.
const EIGHTHS: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// The mark across the panel at the gain that changes nothing.
const FLAT_MARK: char = '\u{2500}';

/// The rule between the pane and the page.
const DIVIDER: &str = "\u{2502}";

/// Rows the columns themselves take. Enough to read a gain at a glance and no
/// more, because the pane is only as tall as what it holds.
const SLIDER_ROWS: u16 = 10;

pub struct EqView<'a> {
    pub settings: &'a Eq,
    /// The band the keys are on.
    pub selected_band: usize,
    /// The genre of the playing track, once it is known.
    pub genre: Option<&'a str>,
    pub genre_match: bool,
    pub theme: &'a Theme,
}

pub fn eq(buffer: &mut Buffer, area: Rect, view: EqView<'_>) -> (Rect, Vec<(Rect, Hit)>) {
    let heading_line = title(&view);
    let report_lines: Vec<Line<'static>> = report(&view).into_iter().map(Line::from).collect();
    let mut sizing_lines = vec![heading_line.clone(), Line::default()];
    let widest_content_area = popup::inner(area);
    if widest_content_area.width as usize >= BANDS {
        sizing_lines.extend((0..SLIDER_ROWS).map(|_| Line::default()));
        if widest_content_area.width as usize >= BANDS * WIDE_SLOT {
            sizing_lines.push(Line::from(" ".repeat(BANDS * WIDE_SLOT)));
        }
    }
    sizing_lines.extend(report_lines.clone());
    let sizing_content = popup::paragraph(sizing_lines);
    let preferred_width = sizing_content
        .line_width()
        .max(BANDS * WIDE_SLOT)
        .saturating_add(4)
        .min(u16::MAX as usize) as u16;
    let panel = popup::fit(&sizing_content, area, preferred_width);
    let surface_style = Style::new().bg(view.theme.surface);
    blank(buffer, panel, surface_style);
    let content_area = popup::inner(panel);
    if content_area.width == 0 || content_area.height == 0 {
        return (panel, Vec::new());
    }

    let divider_style = Style::new().fg(view.theme.gutter).bg(view.theme.surface);
    for row_y in panel.y..panel.y + panel.height {
        put(buffer, panel, row_y, vec![span(DIVIDER, divider_style)]);
    }

    let heading_paragraph = popup::paragraph(heading_line);
    let heading_height = heading_paragraph
        .line_count(content_area.width)
        .min(content_area.height as usize) as u16;
    heading_paragraph.render(
        Rect::new(content_area.x, content_area.y, content_area.width, heading_height),
        buffer,
    );
    let heading_gap = u16::from(content_area.height > heading_height + 1);
    let slider_top = content_area.y + heading_height + heading_gap;
    let available_height = content_area.bottom().saturating_sub(slider_top);
    let band_slot_width = (content_area.width as usize / BANDS).min(WIDE_SLOT);
    let report_paragraph = popup::paragraph(report_lines);
    let report_height = report_paragraph
        .line_count(content_area.width)
        .min(available_height as usize) as u16;
    let label_height = u16::from(band_slot_width >= WIDE_SLOT);
    let slider_available_height = available_height.saturating_sub(report_height + label_height);
    let slider_height = match band_slot_width >= 1 && slider_available_height >= 2 {
        true => slider_available_height.min(SLIDER_ROWS),
        false => 0,
    };
    let slider_bottom = slider_top + slider_height;
    let labels_height = match slider_height > 0 {
        true => label_height,
        false => 0,
    };
    if labels_height != 0 {
        labels(buffer, content_area, slider_bottom, band_slot_width, &view);
    }
    let report_top = slider_bottom + labels_height;
    report_paragraph.render(
        Rect::new(
            content_area.x,
            report_top,
            content_area.width,
            content_area.bottom() - report_top,
        ),
        buffer,
    );
    let hit_regions = columns(
        buffer,
        content_area,
        slider_top,
        slider_bottom,
        band_slot_width,
        &view,
    );
    (panel, hit_regions)
}

fn title(view: &EqView<'_>) -> Line<'static> {
    let status_text = match view.settings.enabled {
        true => "on",
        false => "off",
    };
    let heading_span = span(
        "EQUALISER",
        Style::new()
            .fg(view.theme.accent)
            .bg(view.theme.surface)
            .add_modifier(Modifier::BOLD),
    );
    let status_span = span(
        status_text,
        Style::new().fg(view.theme.muted).bg(view.theme.surface),
    );
    Line::from(vec![heading_span, spaces(2), status_span])
}

/// The rows under the columns, most useful first, so a short pane keeps the
/// ones that say where the keys are.
fn report(view: &EqView<'_>) -> Vec<Vec<Span<'static>>> {
    let theme = view.theme;
    let muted = Style::new().fg(theme.muted).bg(theme.surface);
    let lit = Style::new().fg(theme.accent).bg(theme.surface);
    let plain = Style::new().fg(theme.foreground).bg(theme.surface);

    let selected_band = view.selected_band.min(BANDS - 1);
    let mut report_lines = vec![
        vec![
            span(format!("{:<8}", "band"), muted),
            span(format!("{:<9}", format!("{} Hz", CENTRES[selected_band])), lit),
            span(format!("{:.2}x", view.settings.get(selected_band)), plain),
        ],
        vec![
            span(format!("{:<8}", "preset"), muted),
            span(view.settings.preset_name(), plain),
        ],
    ];

    let genre_status = match (view.genre_match, view.genre) {
        (false, _) => span("off", muted),
        (true, None) => span("on, no answer yet", muted),
        (true, Some(genre)) => {
            let genre_preset = crate::genre::preset_for(genre).unwrap_or("no preset");
            span(format!("{genre} \u{2192} {genre_preset}"), plain)
        }
    };
    report_lines.push(vec![span(format!("{:<8}", "genre"), muted), genre_status]);
    if view.genre_match {
        report_lines.push(vec![span("a band you set yourself stops genre matching", muted)]);
    }
    report_lines.push(vec![span(
        "h l band   k j gain   p preset   g genre   r reset   space on",
        muted,
    )]);
    report_lines
}

fn labels(buffer: &mut Buffer, content_area: Rect, row_y: u16, band_slot_width: usize, view: &EqView<'_>) {
    let mut line_spans = Vec::new();
    for (band_index, centre_hertz) in CENTRES.iter().enumerate() {
        let style = match band_index == view.selected_band {
            true => Style::new().fg(view.theme.accent).bg(view.theme.surface),
            false => Style::new().fg(view.theme.muted).bg(view.theme.surface),
        };
        let frequency_label = label(*centre_hertz);
        line_spans.push(span(
            format!("{:>width$} ", frequency_label, width = band_slot_width - 1),
            style,
        ));
    }
    put(buffer, content_area, row_y, line_spans);
}

/// The centre in at most three cells, so eighteen of them fit one row.
fn label(frequency_hertz: u32) -> String {
    if frequency_hertz < 1000 {
        return frequency_hertz.to_string();
    }
    let kilohertz = frequency_hertz / 1000;
    let hundreds_hertz = (frequency_hertz % 1000) / 100;
    match kilohertz < 10 && hundreds_hertz > 0 {
        true => format!("{kilohertz}k{hundreds_hertz}"),
        false => format!("{kilohertz}k"),
    }
}

/// The gain a click at `y` in that column asks for. The top row is the tallest
/// the panel draws, which is `DISPLAY_MAX` and not the filter's own ceiling.
pub fn gain_at(row_y: u16, band_column: Rect) -> f32 {
    let column_rows = band_column.height.max(1);
    let rows_from_top = row_y.saturating_sub(band_column.y).min(column_rows - 1);
    let vertical_fraction = f32::from(column_rows - rows_from_top) / f32::from(column_rows);
    vertical_fraction * DISPLAY_MAX
}

/// Every column, a row at a time, and the region each one answers a click on.
fn columns(
    buffer: &mut Buffer,
    content_area: Rect,
    slider_top: u16,
    slider_bottom: u16,
    band_slot_width: usize,
    view: &EqView<'_>,
) -> Vec<(Rect, Hit)> {
    let slider_rows = slider_bottom.saturating_sub(slider_top);
    if slider_rows == 0 {
        return Vec::new();
    }
    let band_bar_width = band_slot_width.saturating_sub(1).max(1);
    let filled_eighths: Vec<u16> = (0..BANDS)
        .map(|band_index| {
            let gain_fraction = (view.settings.get(band_index) / DISPLAY_MAX).clamp(0.0, 1.0);
            (gain_fraction * f32::from(slider_rows) * 8.0).round() as u16
        })
        .collect();
    let flat_row_from_bottom = ((crate::eq::FLAT / DISPLAY_MAX) * f32::from(slider_rows)) as u16;

    for row_y in slider_top..slider_bottom {
        let rows_from_bottom = slider_bottom - 1 - row_y;
        let mut line_spans = Vec::new();
        for (band_index, band_height_eighths) in filled_eighths.iter().enumerate() {
            let cell_eighths = band_height_eighths.saturating_sub(rows_from_bottom * 8).min(8) as usize;
            let band_style = match band_index == view.selected_band {
                true => Style::new().fg(view.theme.accent).bg(view.theme.surface),
                false => Style::new().fg(view.theme.gutter).bg(view.theme.surface),
            };
            let (glyph, glyph_style) = match cell_eighths {
                0 if rows_from_bottom == flat_row_from_bottom => {
                    (FLAT_MARK, Style::new().fg(view.theme.dim).bg(view.theme.surface))
                }
                0 => (' ', band_style),
                _ => (EIGHTHS[cell_eighths - 1], band_style),
            };
            line_spans.push(span(glyph.to_string().repeat(band_bar_width), glyph_style));
            if band_slot_width > band_bar_width {
                line_spans.push(spaces(band_slot_width - band_bar_width));
            }
        }
        put(buffer, content_area, row_y, line_spans);
    }

    (0..BANDS)
        .map(|band_index| {
            let column_x = content_area.x + (band_index * band_slot_width) as u16;
            let band_column = Rect::new(column_x, slider_top, band_bar_width as u16, slider_rows);
            (band_column, Hit::EqBand(band_index, band_column))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_genre_widens_the_report_without_clipping() {
        let area = Rect::new(0, 0, 180, 40);
        let mut buffer = Buffer::empty(area);
        let settings = Eq::default();
        let theme = crate::theme::redwax_theme();
        let genre =
            "Electronic with a detailed description that extends beyond the old fixed equaliser panel";
        let (panel, hits) = eq(
            &mut buffer,
            area,
            EqView {
                settings: &settings,
                selected_band: 0,
                genre: Some(genre),
                genre_match: true,
                theme: &theme,
            },
        );
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains(genre));
        assert!(panel.width > 77);
        assert!(panel.height < area.height);
        assert_eq!(hits.len(), BANDS);
        for (rect, _) in hits {
            assert_eq!(rect.intersection(panel), rect);
        }
    }

    #[test]
    fn a_narrow_report_has_no_invisible_slider_targets() {
        let area = Rect::new(3, 4, 16, 24);
        let mut buffer = Buffer::empty(area);
        let (panel, hits) = eq(
            &mut buffer,
            area,
            EqView {
                settings: &Eq::default(),
                selected_band: 0,
                genre: None,
                genre_match: false,
                theme: &crate::theme::redwax_theme(),
            },
        );
        assert!(hits.is_empty());
        assert_eq!(panel.intersection(area), panel);
    }
}
