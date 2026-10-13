//! Account choices remain visible while a failed switch leaves the current login intact.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Widget};
use redwax_ytm::Account;

use crate::theme::Theme;
use crate::ui::hit::Hit;
use crate::ui::{blank, clip, popup, put, span};

#[derive(Debug)]
pub enum Status {
    Loading,
    Switching,
    Ready,
    Failed(String),
}

pub struct AccountPicker {
    pub accounts: Vec<Account>,
    pub cursor: usize,
    pub status: Status,
    pub signed_in: bool,
}

impl AccountPicker {
    pub fn loading(signed_in: bool) -> Self {
        Self {
            accounts: Vec::new(),
            cursor: 0,
            status: Status::Loading,
            signed_in,
        }
    }

    pub fn busy(&self) -> bool {
        matches!(self.status, Status::Loading | Status::Switching)
    }

    pub fn row_count(&self) -> usize {
        self.accounts.len() + 1 + usize::from(self.signed_in)
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if !self.busy() {
            self.cursor = self.cursor.saturating_add_signed(delta).min(self.row_count() - 1);
        }
    }

    pub fn choose_row(&mut self, row: usize) {
        if !self.busy() {
            self.cursor = row.min(self.row_count() - 1);
        }
    }

    pub fn loaded(&mut self, result: Result<Vec<Account>, String>, current: &str) {
        match result {
            Ok(accounts) => {
                self.cursor = accounts
                    .iter()
                    .position(|account| account.matches(current))
                    .unwrap_or(0);
                self.accounts = accounts;
                self.status = Status::Ready;
            }
            Err(error) => self.status = Status::Failed(error),
        }
        self.cursor = self.cursor.min(self.row_count() - 1);
    }
}

pub fn accounts(
    buffer: &mut Buffer,
    bounds: Rect,
    picker: &AccountPicker,
    current: &str,
    theme: &Theme,
) -> Vec<(Rect, Hit)> {
    let requested_height = picker.row_count().saturating_add(7).min(u16::MAX as usize) as u16;
    let width = bounds.width.min(64);
    let height = bounds.height.min(requested_height);
    let panel = Rect::new(
        bounds.x + bounds.width.saturating_sub(width) / 2,
        bounds.y + bounds.height.saturating_sub(height) / 2,
        width,
        height,
    );
    blank(buffer, panel, Style::new().bg(theme.surface));
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.gutter))
        .render(panel, buffer);
    let inner = popup::inner(panel);
    let mut hits = vec![(panel, Hit::Overlay)];
    if inner.is_empty() {
        return hits;
    }
    let heading_style = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    put(buffer, inner, inner.y, vec![span("ACCOUNT", heading_style)]);
    if inner.height < 2 {
        return hits;
    }
    let message = match &picker.status {
        Status::Loading => "Reading browser logins...",
        Status::Switching => "Switching account...",
        Status::Failed(error) => error.as_str(),
        Status::Ready if picker.accounts.is_empty() => {
            "Sign in to YouTube Music in your browser, then refresh."
        }
        Status::Ready => "Choose the library you want to use.",
    };
    popup::paragraph(message)
        .style(Style::new().fg(theme.muted))
        .render(
            Rect::new(inner.x, inner.y + 1, inner.width, (inner.height - 1).min(2)),
            buffer,
        );
    if inner.height < 4 {
        return hits;
    }
    let visible_rows = inner.height.saturating_sub(4) as usize;
    let first_row = picker
        .cursor
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(picker.row_count().saturating_sub(visible_rows));
    for row in first_row..picker.row_count().min(first_row + visible_rows) {
        let label = if let Some(account) = picker.accounts.get(row) {
            let suffix = if account.matches(current) {
                " (current)"
            } else {
                ""
            };
            format!("{}{suffix}", account.label())
        } else if row == picker.accounts.len() {
            "Refresh browser logins".to_owned()
        } else {
            "Sign out of redwax".to_owned()
        };
        let style = if row == picker.cursor && !picker.busy() {
            heading_style.bg(theme.panel)
        } else {
            Style::new()
                .fg(if picker.busy() {
                    theme.muted
                } else {
                    theme.foreground
                })
                .bg(theme.surface)
        };
        let row_area = Rect::new(inner.x, inner.y + 3 + (row - first_row) as u16, inner.width, 1);
        blank(buffer, row_area, style);
        put(
            buffer,
            row_area,
            row_area.y,
            vec![span(clip(&label, row_area.width as usize), style)],
        );
        if !picker.busy() {
            hits.push((row_area, Hit::AccountRow(row)));
        }
    }
    let footer = if matches!(picker.status, Status::Switching) {
        "Playback continues. Waiting for YouTube."
    } else {
        "Up/Down choose   Enter confirm   Esc close"
    };
    put(
        buffer,
        inner,
        inner.bottom() - 1,
        vec![span(
            clip(footer, inner.width as usize),
            Style::new().fg(theme.muted),
        )],
    );
    hits
}
