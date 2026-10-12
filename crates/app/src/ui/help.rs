//! The key reference keeps every binding reachable in a bounded terminal.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::commands::catalogue;
use crate::theme::Theme;
use crate::ui::popup::{self, Scroll};
use crate::ui::{blank, span, width_of};

#[cfg(test)]
pub(crate) fn help(buffer: &mut Buffer, area: Rect, theme: &Theme, scroll: &mut Scroll) -> Rect {
    searchable_help(buffer, area, theme, scroll, "")
}

pub(crate) fn searchable_help(
    buffer: &mut Buffer,
    area: Rect,
    theme: &Theme,
    scroll: &mut Scroll,
    query: &str,
) -> Rect {
    let accent = Style::new().fg(theme.accent).bg(theme.surface);
    let plain = Style::new().fg(theme.muted).bg(theme.surface);
    let normalized_query = query.trim().to_ascii_lowercase();
    let matching_commands = catalogue()
        .filter(|command| command.binding.show)
        .filter(|command| normalized_query.is_empty() || command.search_text().contains(&normalized_query))
        .collect::<Vec<_>>();
    let key_width = matching_commands
        .iter()
        .map(|command| width_of(command.binding.key))
        .max()
        .unwrap_or(0);
    let mut content_lines = vec![
        Line::from(span("COMMANDS", accent.add_modifier(Modifier::BOLD))),
        Line::from(span(
            match normalized_query.is_empty() {
                true => "/ filter commands".to_string(),
                false => format!("filter: {normalized_query}"),
            },
            plain,
        )),
    ];
    let mut wrote_group = false;
    for group in [
        crate::commands::Group::Playback,
        crate::commands::Group::Selection,
        crate::commands::Group::Queue,
        crate::commands::Group::Navigation,
        crate::commands::Group::View,
        crate::commands::Group::Application,
    ] {
        let mut group_commands = matching_commands
            .iter()
            .filter(|command| command.group == group)
            .peekable();
        if group_commands.peek().is_none() {
            continue;
        }
        if wrote_group {
            content_lines.push(Line::default());
        }
        content_lines.push(Line::from(span(
            group.label(),
            accent.add_modifier(Modifier::BOLD),
        )));
        wrote_group = true;
        for command in group_commands {
            let binding = command.binding;
            let padding_width = key_width.saturating_sub(width_of(binding.key)) + 2;
            content_lines.push(Line::from(vec![
                span(binding.key, accent),
                span(" ".repeat(padding_width), plain),
                span(binding.description, plain),
            ]));
        }
    }
    if matching_commands.is_empty() {
        content_lines.push(Line::from(span("No matching commands", plain)));
    }
    let help_content = popup::paragraph(content_lines.clone());
    content_lines.push(Line::from("/ filter   j k scroll   escape close"));
    let sizing_content = popup::paragraph(content_lines);
    let preferred_width = sizing_content
        .line_width()
        .saturating_add(4)
        .min(u16::MAX as usize) as u16;
    let panel = popup::fit(&sizing_content, area, preferred_width);
    blank(buffer, panel, Style::new().bg(theme.surface));
    let content_area = popup::inner(panel);
    if content_area.width == 0 || content_area.height == 0 {
        scroll.measure(0, 0);
        return panel;
    }

    let footer_height = u16::from(content_area.height >= 2);
    let body_area = Rect::new(
        content_area.x,
        content_area.y,
        content_area.width,
        content_area.height - footer_height,
    );
    scroll.measure(help_content.line_count(body_area.width), body_area.height);
    help_content
        .scroll((scroll.offset(), 0))
        .render(body_area, buffer);
    if footer_height != 0 {
        let footer_area = Rect::new(
            content_area.x,
            body_area.y + body_area.height,
            content_area.width,
            1,
        );
        popup::paragraph(Line::from(span(
            format!("/ filter  esc close  {}  j/k scroll", scroll.footer()),
            plain,
        )))
        .render(footer_area, buffer);
    }
    panel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_binding_remains_reachable_in_a_short_reference() {
        let area = Rect::new(0, 0, 80, 12);
        let mut buffer = Buffer::empty(area);
        let theme = crate::theme::redwax_theme();
        let mut scroll = Scroll::default();
        help(&mut buffer, area, &theme, &mut scroll);
        scroll.move_by(isize::MAX);
        help(&mut buffer, area, &theme, &mut scroll);

        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Quit"));
        assert!(scroll.offset() > 0);
    }

    #[test]
    fn a_full_reference_does_not_take_unused_terminal_height() {
        let area = Rect::new(0, 0, 160, 80);
        let mut buffer = Buffer::empty(area);
        let mut scroll = Scroll::default();
        let panel = help(&mut buffer, area, &crate::theme::redwax_theme(), &mut scroll);
        assert!(panel.height < area.height);
        assert!(panel.width > 48);
        assert_eq!(panel.right(), area.right());
    }
}
