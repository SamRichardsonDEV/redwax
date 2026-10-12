//! Selected-item actions with explicit subjects and disabled reasons.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::commands::{ActionTarget, ActionsMenu, SubjectRule, command};
use crate::keys::Action;
use crate::theme::Theme;
use crate::ui::popup;
use crate::ui::{blank, clip, put, span, width_of};

pub struct ActionsPaint {
    pub panel: Rect,
    /// Row geometry by menu index. The App maps these to its bounded hit type.
    pub row_regions: Vec<(Rect, usize)>,
}

pub fn actions(buffer: &mut Buffer, bounds: Rect, menu: &ActionsMenu, theme: &Theme) -> ActionsPaint {
    let subject = menu.subject();
    let subject_hint = match menu.subject_count() > 1 {
        true => "   h/l changes subject",
        false => "",
    };
    let heading = match subject {
        Some((target, action_subject)) => format!(
            "ACTIONS  {}  {}{subject_hint}",
            target_label(*target),
            action_subject.title
        ),
        None => "ACTIONS  APPLICATION".to_string(),
    };
    let key_width = menu
        .entries()
        .iter()
        .map(|entry| width_of(shortcut(entry.action, subject.map(|(target, _)| *target))))
        .max()
        .unwrap_or(0);
    let preferred_width = menu
        .entries()
        .iter()
        .map(|entry| {
            key_width
                + usize::from(key_width > 0) * 2
                + width_of(&entry.label)
                + entry
                    .disabled_reason
                    .as_deref()
                    .map(|reason| width_of(reason) + 3)
                    .unwrap_or(0)
        })
        .chain(std::iter::once(width_of(&heading)))
        .max()
        .unwrap_or(0)
        .saturating_add(6)
        .min(u16::MAX as usize) as u16;
    let sizing_content = popup::paragraph(
        std::iter::once(heading.clone())
            .chain(menu.entries().iter().map(|entry| entry.label.clone()))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let panel = popup::fit(&sizing_content, bounds, preferred_width);
    blank(buffer, panel, Style::new().bg(theme.surface));
    let content_area = popup::inner(panel);
    if content_area.is_empty() {
        return ActionsPaint {
            panel,
            row_regions: Vec::new(),
        };
    }
    put(
        buffer,
        content_area,
        content_area.y,
        vec![span(
            clip(&heading, content_area.width as usize),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        )],
    );
    let mut row_regions = Vec::new();
    let visible_entry_count = content_area.height.saturating_sub(1) as usize;
    let first_visible_index = menu
        .cursor()
        .saturating_sub(visible_entry_count.saturating_sub(1))
        .min(menu.entries().len().saturating_sub(visible_entry_count));
    for (visible_entry_offset, menu_entry) in menu
        .entries()
        .iter()
        .skip(first_visible_index)
        .take(visible_entry_count)
        .enumerate()
    {
        let menu_index = first_visible_index + visible_entry_offset;
        let row_y = content_area.y + 1 + visible_entry_offset as u16;
        let is_selected = menu_index == menu.cursor();
        let row_style = match (is_selected, menu_entry.disabled_reason.is_some()) {
            (true, false) => Style::new()
                .fg(theme.accent)
                .bg(theme.panel)
                .add_modifier(Modifier::BOLD),
            (true, true) => Style::new().fg(theme.muted).bg(theme.panel),
            (false, true) => Style::new().fg(theme.muted),
            (false, false) => Style::new().fg(theme.foreground),
        };
        let disabled_suffix = menu_entry
            .disabled_reason
            .as_deref()
            .map(|reason| format!("   {reason}"))
            .unwrap_or_default();
        let shortcut_key = shortcut(
            menu_entry.action,
            subject.map(|(action_target, _)| *action_target),
        );
        let row_label = match key_width {
            0 => format!("{}{}", menu_entry.label, disabled_suffix),
            _ => format!(
                "{shortcut_key:>key_width$}  {}{}",
                menu_entry.label, disabled_suffix
            ),
        };
        put(
            buffer,
            content_area,
            row_y,
            vec![span(clip(&row_label, content_area.width as usize), row_style)],
        );
        row_regions.push((
            Rect::new(content_area.x, row_y, content_area.width, 1),
            menu_index,
        ));
    }
    ActionsPaint { panel, row_regions }
}

fn target_label(target: ActionTarget) -> &'static str {
    match target {
        ActionTarget::Selected => "SELECTED",
        ActionTarget::Playing => "PLAYING",
    }
}

fn shortcut(action: Action, target: Option<ActionTarget>) -> &'static str {
    let shortcut_action = match (target, action) {
        (Some(ActionTarget::Playing), Action::LikeHighlighted) => Action::Like,
        (Some(ActionTarget::Playing), Action::AddHighlightedToPlaylist) => Action::AddToPlaylist,
        _ => action,
    };
    command(shortcut_action)
        .filter(|command| {
            let target_matches = matches!(
                (target, command.subject),
                (_, SubjectRule::None)
                    | (Some(ActionTarget::Selected), SubjectRule::Selected)
                    | (Some(ActionTarget::Playing), SubjectRule::Playing)
                    | (Some(_), SubjectRule::SelectedOrPlaying)
            );
            target_matches && command.binding.show && command.binding.key != "o menu"
        })
        .map(|command| command.binding.key)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{ActionMenuContext, ActionSubject, SubjectKind};

    #[test]
    fn disabled_reason_is_visible_and_has_row_geometry() {
        let menu = ActionsMenu::open(ActionMenuContext {
            selected: Some(ActionSubject {
                identity: "video-1".into(),
                kind: SubjectKind::Track,
                title: "Track".into(),
                has_album: false,
            }),
            playing: None,
            selected_is_queue_entry: false,
            selected_playlist: None,
            playlist_editable: false,
            signed_in: false,
            playback_retry_available: true,
        })
        .unwrap();
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let painted = actions(&mut buffer, area, &menu, &crate::theme::redwax_theme());
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("Sign in to like tracks"));
        assert!(text.contains("ctrl+p"));
        assert_eq!(painted.row_regions.len(), menu.entries().len());
        assert!(
            painted
                .row_regions
                .iter()
                .all(|(row, _)| row.intersection(painted.panel) == *row)
        );
    }

    #[test]
    fn a_short_panel_keeps_the_selected_action_visible() {
        let mut menu = ActionsMenu::open(ActionMenuContext {
            selected: Some(ActionSubject {
                identity: "queue-1".into(),
                kind: SubjectKind::Track,
                title: "Track".into(),
                has_album: true,
            }),
            playing: None,
            selected_is_queue_entry: true,
            selected_playlist: Some("Playlist".into()),
            playlist_editable: true,
            signed_in: true,
            playback_retry_available: true,
        })
        .unwrap();
        menu.move_cursor(isize::MAX);
        let area = Rect::new(0, 0, 48, 8);
        let mut buffer = Buffer::empty(area);

        let painted = actions(&mut buffer, area, &menu, &crate::theme::redwax_theme());

        assert!(
            painted
                .row_regions
                .iter()
                .any(|(_, index)| *index == menu.cursor())
        );
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Sign out"));
    }

    #[test]
    fn heading_and_shortcuts_name_the_active_subject() {
        let mut menu = ActionsMenu::open(ActionMenuContext {
            selected: Some(ActionSubject {
                identity: "album-1".into(),
                kind: SubjectKind::Album,
                title: "Selected album".into(),
                has_album: false,
            }),
            playing: Some(ActionSubject {
                identity: "video-1".into(),
                kind: SubjectKind::Track,
                title: "Playing track".into(),
                has_album: true,
            }),
            selected_is_queue_entry: false,
            selected_playlist: None,
            playlist_editable: false,
            signed_in: true,
            playback_retry_available: false,
        })
        .unwrap();
        let area = Rect::new(0, 0, 64, 20);
        let mut buffer = Buffer::empty(area);

        actions(&mut buffer, area, &menu, &crate::theme::redwax_theme());
        let selected_text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(selected_text.contains("ACTIONS  SELECTED  Selected album"));

        menu.choose_subject(1);
        buffer.reset();
        actions(&mut buffer, area, &menu, &crate::theme::redwax_theme());
        let playing_text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(playing_text.contains("ACTIONS  PLAYING  Playing track"));
        assert!(playing_text.contains("L  Like or unlike"));
        assert!(playing_text.contains("P  Add to playlist"));
        assert!(!playing_text.contains("+  Like or unlike"));
        assert!(!playing_text.contains("ctrl+p  Add to playlist"));
    }
}
