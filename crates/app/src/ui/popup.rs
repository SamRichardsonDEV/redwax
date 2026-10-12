//! Popup measurement uses the renderer's wrapping rules so their geometry agrees.

use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};

pub(crate) fn paragraph<'a>(text: impl Into<Text<'a>>) -> Paragraph<'a> {
    Paragraph::new(text).wrap(Wrap { trim: false })
}

pub(crate) fn inner(panel: Rect) -> Rect {
    let horizontal_padding = if panel.width >= 6 { 2 } else { 0 };
    let vertical_padding = u16::from(panel.height >= 3);
    Rect::new(
        panel.x + horizontal_padding,
        panel.y + vertical_padding,
        panel.width.saturating_sub(horizontal_padding * 2),
        panel.height.saturating_sub(vertical_padding * 2),
    )
}

pub(crate) fn fit(paragraph: &Paragraph<'_>, bounds: Rect, preferred_width: u16) -> Rect {
    if bounds.is_empty() {
        return bounds;
    }
    let horizontal_padding = if bounds.width >= 6 { 4 } else { 0 };
    let vertical_padding = if bounds.height >= 3 { 2 } else { 0 };
    let mut panel_width = (paragraph.line_width().min(80) + horizontal_padding as usize)
        .max(preferred_width as usize)
        .min(bounds.width as usize) as u16;
    let mut wrapped_lines = paragraph.line_count(panel_width.saturating_sub(horizontal_padding));
    if wrapped_lines > bounds.height.saturating_sub(vertical_padding) as usize {
        panel_width = bounds.width;
        wrapped_lines = paragraph.line_count(panel_width.saturating_sub(horizontal_padding));
    }
    let panel_height = wrapped_lines
        .saturating_add(vertical_padding as usize)
        .min(bounds.height as usize) as u16;
    Rect::new(bounds.right() - panel_width, bounds.y, panel_width, panel_height)
}

/// Resizing clamps the viewport before another key or wheel event can move it.
#[derive(Debug, Default)]
pub(crate) struct Scroll {
    row_offset: usize,
    maximum_offset: usize,
    visible_rows: usize,
    total_rows: usize,
}

impl Scroll {
    pub fn measure(&mut self, total_rows: usize, visible_rows: u16) {
        self.total_rows = total_rows;
        self.visible_rows = visible_rows as usize;
        self.maximum_offset = total_rows
            .saturating_sub(self.visible_rows)
            .min(u16::MAX as usize);
        self.row_offset = self.row_offset.min(self.maximum_offset);
    }

    pub fn offset(&self) -> u16 {
        self.row_offset as u16
    }

    pub fn move_by(&mut self, row_delta: isize) {
        self.row_offset = self
            .row_offset
            .saturating_add_signed(row_delta)
            .min(self.maximum_offset);
    }

    pub fn key(&mut self, key_code: KeyCode) -> bool {
        match key_code {
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(self.visible_rows.max(1) as isize),
            KeyCode::PageUp => self.move_by(-(self.visible_rows.max(1) as isize)),
            KeyCode::Home | KeyCode::Char('g') => self.row_offset = 0,
            KeyCode::End | KeyCode::Char('G') => self.row_offset = self.maximum_offset,
            _ => return false,
        }
        true
    }

    pub fn footer(&self) -> String {
        let first_visible_row = if self.total_rows == 0 {
            0
        } else {
            self.row_offset + 1
        };
        format!(
            "{first_visible_row}-{}/{}",
            (self.row_offset + self.visible_rows).min(self.total_rows),
            self.total_rows
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_and_bounds_agree_at_tiny_sizes_and_nonzero_origins() {
        for width in 0..=10 {
            for height in 0..=6 {
                let bounds = Rect::new(7, 3, width, height);
                let panel = fit(&paragraph("message\nsecond line"), bounds, 40);
                assert_eq!(panel.intersection(bounds), panel);
                let inner = inner(panel);
                assert_eq!(inner.intersection(panel), inner);
            }
        }
    }

    #[test]
    fn resize_clamps_scroll_to_the_new_last_page() {
        let mut scroll = Scroll::default();
        scroll.measure(30, 4);
        scroll.key(KeyCode::End);
        assert_eq!(scroll.offset(), 26);
        scroll.measure(12, 10);
        assert_eq!(scroll.offset(), 2);
        scroll.key(KeyCode::Home);
        scroll.move_by(-3);
        assert_eq!(scroll.offset(), 0);
    }
}
