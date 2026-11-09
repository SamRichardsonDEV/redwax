//! Input choices, the compact player view, and focus mode settings.

use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};

use crate::config::Config;
use crate::theme::Theme;
use crate::ui::hit::Hit;
use crate::ui::popup;
use crate::ui::{fill, mix, put, span};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    #[default]
    SearchEditing,
    MiniPlayer,
    FocusMode,
    FocusPlaylist,
}

impl Setting {
    pub const ALL: [Self; 4] = [
        Self::SearchEditing,
        Self::MiniPlayer,
        Self::FocusMode,
        Self::FocusPlaylist,
    ];

    pub fn next(self, forward: bool) -> Self {
        let selected_index = Self::ALL.iter().position(|setting| *setting == self).unwrap_or(0);
        let next_index = match forward {
            true => (selected_index + 1).min(Self::ALL.len() - 1),
            false => selected_index.saturating_sub(1),
        };
        Self::ALL[next_index]
    }
}

pub fn settings(
    buffer: &mut Buffer,
    area: Rect,
    config: &Config,
    focus: (bool, Option<&str>),
    selected: Setting,
    theme: &Theme,
) -> Vec<(Rect, Hit)> {
    let (focus_active, focus_status) = focus;
    popup::frame(buffer, area, theme.background, theme.muted);
    let content_area = popup::inner(area);
    if content_area.is_empty() {
        return Vec::new();
    }
    let row_bounds = area.inner(Margin::new(1, 1));

    let accent = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let muted = Style::new().fg(theme.muted);
    let heading_height = match content_area.height {
        0..=2 => 0,
        3..=6 => 1,
        _ => 3,
    };
    if heading_height > 0 {
        put(
            buffer,
            content_area,
            content_area.y,
            vec![span("SETTINGS", accent)],
        );
    }
    if heading_height > 1 {
        put(
            buffer,
            content_area,
            content_area.y + 1,
            vec![span("up/down to choose, enter or space to change", muted)],
        );
    }

    let editing_mode = match config.search_vim_mode {
        true => "Vim",
        false => "Standard",
    };
    let focus_label = match focus_active {
        true => "On",
        false => "Off",
    };
    let focus_playlist = config
        .focus_playlist_title
        .as_deref()
        .or(config.focus_playlist_id.as_deref())
        .unwrap_or("Choose playlist");
    let rows = [
        (
            Setting::SearchEditing,
            "Search editing",
            editing_mode,
            Hit::ToggleSearchVim,
        ),
        (
            Setting::MiniPlayer,
            "Mini player",
            "Open here",
            Hit::OpenMiniPlayer,
        ),
        (
            Setting::FocusMode,
            "Focus mode",
            focus_label,
            Hit::ToggleFocusSetting,
        ),
        (
            Setting::FocusPlaylist,
            "  Playlist",
            focus_playlist,
            Hit::ChooseFocusPlaylist,
        ),
    ];
    let available_height = content_area.height - heading_height;
    let row_step = if available_height >= 10 { 2 } else { 1 };
    let visible_rows = ((available_height - 1) / row_step + 1).min(rows.len() as u16) as usize;
    let selected_index = rows
        .iter()
        .position(|(setting, _, _, _)| *setting == selected)
        .unwrap_or(0);
    let first_visible_index = selected_index.saturating_sub(visible_rows - 1);
    let mut targets = Vec::new();
    let mut help_start_y = content_area.y + heading_height;
    for (visible_index, (setting, label, value, hit)) in rows
        .into_iter()
        .skip(first_visible_index)
        .take(visible_rows)
        .enumerate()
    {
        let child_offset = u16::from(setting == Setting::FocusPlaylist && visible_index > 0 && row_step > 1);
        let row_y = content_area.y + heading_height + visible_index as u16 * row_step - child_offset;
        help_start_y = row_y.saturating_add(2);
        let row_area = Rect::new(row_bounds.x, row_y, row_bounds.width, 1);
        let is_selected = selected == setting;
        if is_selected {
            fill(buffer, row_area, mix(theme.panel, theme.background, 0.2));
        }
        put(
            buffer,
            content_area,
            row_y,
            vec![
                span(if is_selected { "> " } else { "  " }, accent),
                span(format!("{label:<17}"), Style::new().fg(theme.foreground)),
                span(format!("[{value}]"), accent),
            ],
        );
        targets.push((row_area, hit));
    }

    let help = match selected {
        Setting::SearchEditing => [
            "Vim opens search in Insert mode. Escape enters Normal mode. Press i to return.",
            "A second escape leaves search. The choice is saved to config.toml.",
        ],
        Setting::MiniPlayer => [
            "Switches this terminal to the compact player without starting another playback session.",
            "F returns to the full app. Q saves and closes this session.",
        ],
        Setting::FocusMode => [
            "Enter turns focus mode on or off. The Playlist row below chooses the destination.",
            "Full plays are saved once. Skips, seeks and existing playlist songs are excluded.",
        ],
        Setting::FocusPlaylist => [
            "Choose a playlist you own. Changing it while focus mode is on switches the destination.",
            "Choosing a playlist while focus mode is off leaves it off.",
        ],
    };
    put(buffer, content_area, help_start_y, vec![span(help[0], muted)]);
    put(
        buffer,
        content_area,
        help_start_y.saturating_add(1),
        vec![span(help[1], muted)],
    );
    if matches!(selected, Setting::FocusMode | Setting::FocusPlaylist) {
        let destination = config
            .focus_playlist_title
            .as_deref()
            .or(config.focus_playlist_id.as_deref());
        let detail = match (focus_active, destination) {
            (true, Some(destination)) => format!("Saving to: {destination}"),
            (false, Some(destination)) => format!("Destination: {destination}. Focus mode is off."),
            _ => "Choose a playlist below or when you turn focus mode on.".to_string(),
        };
        put(
            buffer,
            content_area,
            help_start_y.saturating_add(2),
            vec![span(detail, muted)],
        );
        if let Some(status) = focus_status {
            put(
                buffer,
                content_area,
                help_start_y.saturating_add(3),
                vec![span(status.to_string(), muted)],
            );
        }
    }

    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selected_focus_mode_remains_visible_in_short_settings() {
        let config = Config {
            focus_playlist_id: Some("PLfocus".to_string()),
            focus_playlist_title: Some("Deep work".to_string()),
            ..Config::default()
        };
        let theme = crate::theme::redwax_theme();
        for height in [3, 5, 8, 16] {
            let area = Rect::new(0, 0, 80, height);
            let mut buffer = Buffer::empty(area);
            let targets = settings(
                &mut buffer,
                area,
                &config,
                (true, None),
                Setting::FocusMode,
                &theme,
            );
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("Focus mode"));
            assert!(text.contains("[On]"));
            assert!(targets.iter().any(|(_, hit)| *hit == Hit::ToggleFocusSetting));
            assert!(targets.iter().all(|(row, _)| row.intersection(area) == *row));
        }
    }

    #[test]
    fn the_selected_playlist_remains_visible_in_short_settings() {
        let config = Config {
            focus_playlist_id: Some("PLfocus".to_string()),
            focus_playlist_title: Some("Deep work".to_string()),
            ..Config::default()
        };
        let theme = crate::theme::redwax_theme();
        for height in [3, 5, 8, 16] {
            let area = Rect::new(0, 0, 80, height);
            let mut buffer = Buffer::empty(area);
            let targets = settings(
                &mut buffer,
                area,
                &config,
                (false, None),
                Setting::FocusPlaylist,
                &theme,
            );
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("Playlist"));
            assert!(text.contains("[Deep work]"));
            assert!(targets.iter().any(|(_, hit)| *hit == Hit::ChooseFocusPlaylist));
            assert!(targets.iter().all(|(row, _)| row.intersection(area) == *row));
        }
    }

    #[test]
    fn the_playlist_row_is_indented_directly_under_focus_mode() {
        let area = Rect::new(0, 0, 100, 20);
        let mut buffer = Buffer::empty(area);
        let targets = settings(
            &mut buffer,
            area,
            &Config::default(),
            (false, None),
            Setting::FocusMode,
            &crate::theme::redwax_theme(),
        );
        let row = |wanted| {
            targets
                .iter()
                .find_map(|(area, hit)| (*hit == wanted).then_some(*area))
                .unwrap()
        };
        let focus = row(Hit::ToggleFocusSetting);
        let playlist = row(Hit::ChooseFocusPlaylist);
        let text = |row_y| {
            (0..area.width)
                .map(|column| buffer[(column, row_y)].symbol())
                .collect::<String>()
        };
        assert_eq!(playlist.y, focus.y + 1);
        assert_eq!(
            text(playlist.y).find("Playlist"),
            text(focus.y).find("Focus mode").map(|column| column + 2)
        );
        assert!(text(playlist.y).contains("[Choose playlist]"));
    }

    #[test]
    fn the_last_collection_result_remains_visible_beside_the_destination() {
        let area = Rect::new(0, 0, 120, 25);
        let theme = crate::theme::redwax_theme();
        let config = Config {
            focus_playlist_title: Some("Deep work".to_string()),
            ..Config::default()
        };
        for selected in [Setting::FocusMode, Setting::FocusPlaylist] {
            let mut buffer = Buffer::empty(area);
            settings(
                &mut buffer,
                area,
                &config,
                (true, Some("Saved Alpine Glow to Deep work")),
                selected,
                &theme,
            );
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("Saving to: Deep work"));
            assert!(text.contains("Saved Alpine Glow to Deep work"));
        }
    }
}
