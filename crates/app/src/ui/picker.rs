//! The playlist picker, limited to destinations supplied by the application.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use redwax_ytm::Playlist;

use crate::theme::Theme;
use crate::ui::popup;
use crate::ui::{RULE, blank, span, width_of};

/// The row under the playlists that makes a playlist rather than choosing one.
pub const NEW_PLAYLIST: &str = "+ New playlist";

const NAME_LABEL: &str = "Name  ";
const CARET: &str = "\u{258f}";

pub struct PickerView<'a> {
    /// Taking a track out rather than putting one in. There is no new
    /// playlist row then, because nothing can be taken out of an empty one.
    pub remove_mode: bool,
    /// What is being edited, named the way a toast would name it.
    pub primary_item_label: &'a str,
    /// What `tab` swaps in, when there is anything to swap.
    pub alternate_item_label: Option<&'a str>,
    pub playlists: &'a [Playlist],
    pub selected_playlist_index: usize,
    /// The name being typed for a new playlist, which takes the list's place.
    pub new_playlist_name_input: Option<&'a str>,
    pub theme: &'a Theme,
}

pub fn picker(buffer: &mut Buffer, area: Rect, view: PickerView<'_>) -> Rect {
    let theme = view.theme;
    let muted = Style::new().fg(theme.muted).bg(theme.surface);
    let plain = Style::new().fg(theme.foreground).bg(theme.surface);
    let heading_text = match view.remove_mode {
        true => "REMOVE FROM PLAYLIST",
        false => "ADD TO PLAYLIST",
    };
    let mut heading_lines = vec![
        Line::from(span(
            heading_text,
            Style::new()
                .fg(theme.accent)
                .bg(theme.surface)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(span(view.primary_item_label.to_string(), plain)),
    ];
    if let Some(alternate_item_label) = view.alternate_item_label {
        heading_lines.push(Line::from(span(format!("tab  {alternate_item_label}"), muted)));
    }
    heading_lines.push(Line::default());
    let heading = popup::paragraph(heading_lines.clone());
    let footer_hint = match (view.new_playlist_name_input, view.remove_mode) {
        (Some(_), _) => "enter make   escape back",
        (None, true) => "j k move   enter remove   escape cancel",
        (None, false) => "j k move   enter add   escape cancel",
    };
    let footer = popup::paragraph(Line::from(span(footer_hint, muted)));
    let playlist_titles: Vec<&str> = view
        .playlists
        .iter()
        .map(|playlist| playlist.title.as_str())
        .chain((!view.remove_mode).then_some(NEW_PLAYLIST))
        .collect();
    let mut sizing_lines = heading_lines;
    match view.new_playlist_name_input {
        Some(new_playlist_name) => {
            sizing_lines.push(Line::from(format!("{NAME_LABEL}{new_playlist_name}{CARET}")))
        }
        None => sizing_lines.extend(
            playlist_titles
                .iter()
                .map(|title| Line::from(format!("  {title}"))),
        ),
    }
    sizing_lines.push(Line::from(footer_hint));
    let sizing_content = popup::paragraph(sizing_lines);
    let preferred_width = sizing_content
        .line_width()
        .saturating_add(4)
        .min(u16::MAX as usize) as u16;
    let panel = popup::fit(&sizing_content, area, preferred_width);
    blank(buffer, panel, Style::new().bg(theme.surface));
    let content_area = popup::inner(panel);
    if content_area.width == 0 || content_area.height == 0 {
        return panel;
    }

    let selected_playlist_index = view
        .selected_playlist_index
        .min(playlist_titles.len().saturating_sub(1));
    let playlist_rows: Vec<Paragraph<'static>> = playlist_titles
        .iter()
        .enumerate()
        .map(|(playlist_index, playlist_title)| {
            playlist_row(
                playlist_title,
                playlist_index == selected_playlist_index,
                content_area.width,
                theme,
            )
        })
        .collect();
    let selected_height = match view.new_playlist_name_input {
        Some(_) => 1,
        None => playlist_rows
            .get(selected_playlist_index)
            .map_or(1, |playlist_row| playlist_row.line_count(content_area.width)),
    }
    .min(content_area.height as usize) as u16;
    // A short terminal gives the selected destination priority over labels.
    let spare_height = content_area.height.saturating_sub(selected_height);
    let footer_height = footer
        .line_count(content_area.width)
        .min((spare_height / 2) as usize) as u16;
    let heading_height = heading
        .line_count(content_area.width)
        .min((spare_height - footer_height) as usize) as u16;
    heading.render(
        Rect::new(content_area.x, content_area.y, content_area.width, heading_height),
        buffer,
    );
    let playlist_area = Rect::new(
        content_area.x,
        content_area.y + heading_height,
        content_area.width,
        content_area.height - heading_height - footer_height,
    );

    if let Some(new_playlist_name) = view.new_playlist_name_input {
        let name_label = match playlist_area.width as usize > width_of(NAME_LABEL) {
            true => NAME_LABEL,
            false => "",
        };
        let name_width =
            (playlist_area.width as usize).saturating_sub(width_of(name_label) + width_of(CARET));
        popup::paragraph(Line::from(vec![
            span(name_label, muted),
            span(format!("{}{CARET}", tail(new_playlist_name, name_width)), plain),
        ]))
        .render(
            Rect::new(playlist_area.x, playlist_area.y, playlist_area.width, 1),
            buffer,
        );
    } else {
        let mut first_visible_index = selected_playlist_index;
        let mut used_rows = selected_height as usize;
        while first_visible_index > 0 {
            let previous_row_height = playlist_rows[first_visible_index - 1].line_count(playlist_area.width);
            if used_rows + previous_row_height > playlist_area.height as usize {
                break;
            }
            first_visible_index -= 1;
            used_rows += previous_row_height;
        }
        let mut next_row_y = playlist_area.y;
        for playlist_row in playlist_rows.into_iter().skip(first_visible_index) {
            let row_height = playlist_row
                .line_count(playlist_area.width)
                .min((playlist_area.bottom() - next_row_y) as usize) as u16;
            if row_height == 0 {
                break;
            }
            playlist_row.render(
                Rect::new(playlist_area.x, next_row_y, playlist_area.width, row_height),
                buffer,
            );
            next_row_y += row_height;
        }
    }
    footer.render(
        Rect::new(
            content_area.x,
            playlist_area.bottom(),
            content_area.width,
            footer_height,
        ),
        buffer,
    );
    panel
}

fn playlist_row(
    playlist_title: &str,
    is_selected: bool,
    row_width: u16,
    theme: &Theme,
) -> Paragraph<'static> {
    let style = Style::new()
        .fg(if is_selected {
            theme.accent
        } else {
            theme.foreground
        })
        .bg(theme.surface);
    let prefix = match (row_width > 2, is_selected) {
        (true, true) => format!("{RULE} "),
        (true, false) => "  ".to_string(),
        (false, _) => String::new(),
    };
    popup::paragraph(Line::from(span(format!("{prefix}{playlist_title}"), style)))
}

/// The end of `text` that fits `width`, because the end is where typing lands.
fn tail(text: &str, maximum_width: usize) -> String {
    let first_visible_byte = text
        .char_indices()
        .map(|(byte_index, _)| byte_index)
        .find(|byte_index| width_of(&text[*byte_index..]) <= maximum_width)
        .unwrap_or(text.len());
    text[first_visible_byte..]
        .chars()
        .skip_while(|character| width_of(&character.to_string()) == 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_destination_survives_a_short_picker() {
        let playlists: Vec<Playlist> = (0..100)
            .map(|index| Playlist {
                playlist_id: format!("PL{index}"),
                title: format!("Destination {index}"),
                subtitle: None,
                thumbnail_url: None,
            })
            .collect();
        for height in [1, 3, 6] {
            let area = Rect::new(0, 0, 40, height);
            let mut buffer = Buffer::empty(area);
            picker(
                &mut buffer,
                area,
                PickerView {
                    remove_mode: true,
                    primary_item_label: "A track description longer than the destination name",
                    alternate_item_label: Some("Another item that the user can choose"),
                    playlists: &playlists,
                    selected_playlist_index: 99,
                    new_playlist_name_input: None,
                    theme: &crate::theme::redwax_theme(),
                },
            );
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("Destination 99"));
        }
    }

    #[test]
    fn playlist_names_can_grow_the_picker_beyond_its_old_width() {
        let playlists = [Playlist {
            playlist_id: "PLexample".to_string(),
            title: "A long playlist name with enough detail to need more than forty eight columns"
                .to_string(),
            subtitle: None,
            thumbnail_url: None,
        }];
        let area = Rect::new(0, 0, 140, 45);
        let mut buffer = Buffer::empty(area);
        let panel = picker(
            &mut buffer,
            area,
            PickerView {
                remove_mode: false,
                primary_item_label: "A track",
                alternate_item_label: None,
                playlists: &playlists,
                selected_playlist_index: 0,
                new_playlist_name_input: None,
                theme: &crate::theme::redwax_theme(),
            },
        );
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains(&playlists[0].title));
        assert!(panel.width > 48);
        assert!(panel.height < area.height);
    }

    #[test]
    fn the_name_tail_uses_terminal_cells_for_wide_and_combining_text() {
        assert_eq!(tail("prefix界界", 3), "界");
        assert_eq!(tail("prefixe\u{301}", 1), "e\u{301}");
        assert_eq!(tail("界", 1), "");
        assert_eq!(tail("e\u{301}", 0), "");
    }
}
