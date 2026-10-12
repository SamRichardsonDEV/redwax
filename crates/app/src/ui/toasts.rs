//! Notification geometry follows wrapped content and keeps the newest message visible.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::Widget;

use crate::app::{Level, Notification, clock};
use crate::theme::Theme;
use crate::ui::anim::{self, Motion};
use crate::ui::{Hit, blank, clip, popup, put, span};

const PREFERRED_WIDTH: u16 = 40;
pub const MAX_TOASTS: usize = 3;

pub fn toasts(
    buffer: &mut Buffer,
    area: Rect,
    notifications: &[Notification],
    theme: &Theme,
    motion: Motion,
) -> Vec<(Rect, Hit)> {
    let toast_bounds = padded_bounds(area);
    if toast_bounds.is_empty() {
        return Vec::new();
    }
    let mut next_bottom = toast_bounds.bottom();
    let mut hit_regions = Vec::new();
    let now_millis = clock();
    for (notification_index, notification) in notifications.iter().enumerate().rev().take(MAX_TOASTS) {
        if next_bottom <= toast_bounds.y {
            break;
        }
        let available_area = Rect::new(
            toast_bounds.x,
            toast_bounds.y,
            toast_bounds.width,
            next_bottom - toast_bounds.y,
        );
        let message_paragraph = popup::paragraph(notification_content(notification)).style(
            Style::new()
                .fg(fading(notification, now_millis, theme, motion))
                .bg(theme.panel),
        );
        let mut toast_panel = popup::fit(&message_paragraph, available_area, PREFERRED_WIDTH);
        let wrapped_rows = message_paragraph.line_count(popup::inner(toast_panel).width);
        if !hit_regions.is_empty() && wrapped_rows > popup::inner(toast_panel).height as usize {
            break;
        }
        toast_panel.y = next_bottom - toast_panel.height;
        let mut content_area = popup::inner(toast_panel);
        let is_truncated = wrapped_rows > content_area.height as usize;
        surface(buffer, toast_panel, notification, theme);
        if is_truncated && content_area.height >= 2 {
            content_area.height -= 1;
            footer(buffer, popup::inner(toast_panel), "click or ! to read", theme);
        }
        message_paragraph.render(content_area, buffer);
        if is_truncated && popup::inner(toast_panel).height == 1 && content_area.width >= 2 {
            put(
                buffer,
                Rect::new(content_area.right() - 1, content_area.y, 1, 1),
                content_area.y,
                vec![span("!", Style::new().fg(theme.accent).bg(theme.panel))],
            );
        }
        hit_regions.push((toast_panel, Hit::Notification(notification_index)));
        next_bottom = toast_panel.y.saturating_sub(1).max(toast_bounds.y);
    }
    hit_regions
}

pub(crate) fn message(
    buffer: &mut Buffer,
    area: Rect,
    notification: &Notification,
    scroll: &mut popup::Scroll,
    theme: &Theme,
) -> Rect {
    let mut sizing_content = notification_content(notification);
    sizing_content.lines.push(Line::raw(""));
    let message_bounds = padded_bounds(area);
    let mut panel = popup::fit(&popup::paragraph(sizing_content), message_bounds, 60);
    panel.y += message_bounds.height.saturating_sub(panel.height) / 2;
    surface(buffer, panel, notification, theme);
    let mut content_area = popup::inner(panel);
    if content_area.height >= 2 {
        content_area.height -= 1;
    }
    let message_paragraph = popup::paragraph(notification_content(notification))
        .style(Style::new().fg(theme.foreground).bg(theme.panel));
    scroll.measure(
        message_paragraph.line_count(content_area.width),
        content_area.height,
    );
    message_paragraph
        .scroll((scroll.offset(), 0))
        .render(content_area, buffer);
    if popup::inner(panel).height >= 2 {
        let footer_hint = if content_area.width >= 35 {
            format!("j k scroll  esc close  {}", scroll.footer())
        } else if content_area.width >= 14 {
            format!("! close  {}", scroll.footer())
        } else {
            "! close".to_owned()
        };
        footer(buffer, popup::inner(panel), &footer_hint, theme);
    }
    panel
}

fn notification_content(notification: &Notification) -> Text<'_> {
    let mut notification_lines = Vec::new();
    if let Some(title) = notification.title.as_deref().filter(|title| !title.is_empty()) {
        notification_lines.extend(
            Text::from(title)
                .lines
                .into_iter()
                .map(|line| line.style(Style::new().add_modifier(Modifier::BOLD))),
        );
    }
    notification_lines.extend(Text::from(notification.text.as_str()).lines);
    Text::from(notification_lines)
}

fn padded_bounds(area: Rect) -> Rect {
    let horizontal_padding = u16::from(area.width >= 8);
    let vertical_padding = u16::from(area.height >= 4);
    Rect::new(
        area.x + horizontal_padding,
        area.y + vertical_padding,
        area.width.saturating_sub(horizontal_padding * 2),
        area.height.saturating_sub(vertical_padding * 2),
    )
}

fn surface(buffer: &mut Buffer, panel: Rect, notification: &Notification, theme: &Theme) {
    blank(buffer, panel, Style::new().bg(theme.panel));
    if panel.width < 6 {
        return;
    }
    let severity_color = match notification.level {
        Level::Error => theme.error,
        Level::Warning => theme.warning,
        Level::Info => theme.accent,
    };
    for row_y in panel.y..panel.bottom() {
        put(
            buffer,
            panel,
            row_y,
            vec![span("\u{2588}", Style::new().fg(severity_color).bg(theme.panel))],
        );
    }
}

fn footer(buffer: &mut Buffer, content_area: Rect, footer_text: &str, theme: &Theme) {
    if content_area.is_empty() {
        return;
    }
    let footer_text = if content_area.width < 18 && footer_text == "click or ! to read" {
        "! read"
    } else {
        footer_text
    };
    put(
        buffer,
        content_area,
        content_area.bottom() - 1,
        vec![span(
            clip(footer_text, content_area.width as usize),
            Style::new().fg(theme.muted).bg(theme.panel),
        )],
    );
}

fn fading(notification: &Notification, now_millis: u64, theme: &Theme, motion: Motion) -> Color {
    let remaining_millis = notification.expires_at_millis.saturating_sub(now_millis);
    match anim::fade_step(remaining_millis, motion.animations_enabled) {
        0 => theme.foreground,
        1 => theme.dim,
        _ => theme.muted,
    }
}
