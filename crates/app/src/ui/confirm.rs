//! A modal decision panel with a safe cancel choice and bounded pointer rows.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::theme::Theme;
use crate::ui::hit::{ConfirmationChoice, Hit};
use crate::ui::popup;
use crate::ui::{blank, fit, put, span, width_of};

/// Selection state defaults to the reversible choice for every prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfirmationState {
    selected: ConfirmationChoice,
}

impl ConfirmationState {
    pub fn selected(self) -> ConfirmationChoice {
        self.selected
    }

    pub fn select(&mut self, choice: ConfirmationChoice) {
        self.selected = choice;
    }

    pub fn move_to(&mut self, confirm_selected: bool) {
        self.selected = match confirm_selected {
            true => ConfirmationChoice::Confirm,
            false => ConfirmationChoice::Cancel,
        };
    }
}

pub struct ConfirmationView<'a> {
    pub title: &'a str,
    pub message: &'a str,
    pub detail: Option<&'a str>,
    pub confirm_label: &'a str,
    pub cancel_label: &'a str,
    pub state: &'a ConfirmationState,
    pub theme: &'a Theme,
}

pub struct ConfirmationPaint {
    pub panel: Rect,
    pub hit_regions: Vec<(Rect, Hit)>,
}

pub fn confirmation(buffer: &mut Buffer, bounds: Rect, view: ConfirmationView<'_>) -> ConfirmationPaint {
    if bounds.is_empty() {
        return ConfirmationPaint {
            panel: bounds,
            hit_regions: Vec::new(),
        };
    }
    let button_labels = format!("{}    {}", view.cancel_label, view.confirm_label);
    let mut sizing_lines = vec![view.title, view.message];
    if let Some(detail) = view.detail.filter(|detail| !detail.is_empty()) {
        sizing_lines.push(detail);
    }
    sizing_lines.extend(["", button_labels.as_str()]);
    let sizing_content = popup::paragraph(sizing_lines.join("\n"));
    let preferred_width = width_of(view.title)
        .max(width_of(&button_labels))
        .saturating_add(6)
        .max(36)
        .min(u16::MAX as usize) as u16;
    let mut panel = popup::fit(&sizing_content, bounds, preferred_width);
    panel.x = bounds.x + bounds.width.saturating_sub(panel.width) / 2;
    panel.y = bounds.y + bounds.height.saturating_sub(panel.height) / 2;
    blank(buffer, panel, Style::new().bg(view.theme.surface));

    let mut hit_regions = vec![(bounds, Hit::Overlay)];
    let content_area = popup::inner(panel);
    if content_area.is_empty() {
        return ConfirmationPaint { panel, hit_regions };
    }
    put(
        buffer,
        content_area,
        content_area.y,
        vec![span(
            fit(view.title, content_area.width as usize),
            Style::new()
                .fg(view.theme.warning)
                .bg(view.theme.surface)
                .add_modifier(Modifier::BOLD),
        )],
    );

    let button_row_y = content_area.bottom().saturating_sub(1);
    let body_start_y = content_area.y.saturating_add(1);
    if button_row_y > body_start_y {
        let mut body_lines = vec![Line::from(view.message.to_string())];
        if let Some(detail) = view.detail.filter(|detail| !detail.is_empty()) {
            body_lines.push(Line::from(""));
            body_lines.push(Line::from(detail.to_string()));
        }
        popup::paragraph(body_lines)
            .style(Style::new().fg(view.theme.foreground).bg(view.theme.surface))
            .render(
                Rect::new(
                    content_area.x,
                    body_start_y,
                    content_area.width,
                    button_row_y - body_start_y,
                ),
                buffer,
            );
    }

    for (button_area, choice, label) in button_areas(content_area, view.cancel_label, view.confirm_label) {
        let is_selected = choice == view.state.selected();
        let foreground = match choice {
            ConfirmationChoice::Cancel => view.theme.foreground,
            ConfirmationChoice::Confirm => view.theme.warning,
        };
        let style = Style::new()
            .fg(foreground)
            .bg(if is_selected {
                view.theme.panel
            } else {
                view.theme.surface
            })
            .add_modifier(if is_selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        put(
            buffer,
            button_area,
            button_area.y,
            vec![span(fit(label, button_area.width as usize), style)],
        );
        hit_regions.push((button_area, Hit::Confirmation(choice)));
    }
    ConfirmationPaint { panel, hit_regions }
}

fn button_areas<'a>(
    content_area: Rect,
    cancel_label: &'a str,
    confirm_label: &'a str,
) -> Vec<(Rect, ConfirmationChoice, &'a str)> {
    if content_area.width < 2 || content_area.height == 0 {
        return Vec::new();
    }
    let row_y = content_area.bottom() - 1;
    let cancel_width = width_of(cancel_label).saturating_add(2).min(u16::MAX as usize) as u16;
    let confirm_width = width_of(confirm_label).saturating_add(2).min(u16::MAX as usize) as u16;
    let total_width = cancel_width.saturating_add(confirm_width).saturating_add(2);
    let (cancel_area, confirm_area) = if total_width <= content_area.width {
        let first_button_x = content_area.right() - total_width;
        (
            Rect::new(first_button_x, row_y, cancel_width, 1),
            Rect::new(first_button_x + cancel_width + 2, row_y, confirm_width, 1),
        )
    } else {
        let cancel_width = content_area.width.saturating_sub(1) / 2;
        (
            Rect::new(content_area.x, row_y, cancel_width, 1),
            Rect::new(
                content_area.x + cancel_width + 1,
                row_y,
                content_area.width - cancel_width - 1,
                1,
            ),
        )
    };
    vec![
        (cancel_area, ConfirmationChoice::Cancel, cancel_label),
        (confirm_area, ConfirmationChoice::Confirm, confirm_label),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_geometry_covers_the_frame_and_keeps_choices_in_the_panel() {
        let bounds = Rect::new(7, 3, 48, 12);
        let mut buffer = Buffer::empty(bounds);
        let painted = confirmation(
            &mut buffer,
            bounds,
            ConfirmationView {
                title: "CLEAR QUEUE",
                message: "Clear 24 upcoming tracks and stop radio?",
                detail: Some("The playing track continues."),
                confirm_label: "Clear queue",
                cancel_label: "Keep queue",
                state: &ConfirmationState::default(),
                theme: &crate::theme::redwax_theme(),
            },
        );

        assert_eq!(painted.panel.intersection(bounds), painted.panel);
        assert_eq!(painted.hit_regions.first(), Some(&(bounds, Hit::Overlay)));
        let choices = painted
            .hit_regions
            .iter()
            .filter(|(_, hit)| matches!(hit, Hit::Confirmation(_)))
            .collect::<Vec<_>>();
        assert_eq!(choices.len(), 2);
        assert!(
            choices
                .iter()
                .all(|(area, _)| area.intersection(painted.panel) == *area)
        );
        assert_eq!(
            ConfirmationState::default().selected(),
            ConfirmationChoice::Cancel
        );
    }
}
