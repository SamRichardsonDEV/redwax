//! One frame measurement shared by painting, pointer targets and page sizing.

use ratatui::layout::Rect;

pub const HEADER_HEIGHT: u16 = 1;
pub const PLAYER_HEIGHT: u16 = 3;
pub const SIDEBAR_WIDTH: u16 = 46;
pub const SIDEBAR_AT: u16 = 135;

const MIN_FRAME_WIDTH: u16 = 8;
const MIN_CONTENT_HEIGHT: u16 = 2;
const HINT_HEIGHT: u16 = 1;
const COVER_CHROME_HEIGHT: u16 = 12;

pub fn cover_max_height(sidebar_area: Rect, configured_art_height: u16) -> u16 {
    configured_art_height.min(
        sidebar_area
            .height
            .saturating_sub(1)
            .saturating_sub(COVER_CHROME_HEIGHT),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderMode {
    Full,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerMode {
    Full,
    Compact,
    Minimal,
}

pub fn header_mode(width: u16) -> HeaderMode {
    if width >= 100 {
        HeaderMode::Full
    } else {
        HeaderMode::Compact
    }
}

pub fn player_mode(width: u16) -> PlayerMode {
    if width >= 80 {
        PlayerMode::Full
    } else if width >= 40 {
        PlayerMode::Compact
    } else {
        PlayerMode::Minimal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarLayout {
    pub area: Rect,
    /// Artwork leaves room for track identity, a short queue and key hints.
    pub cover_max_height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub header: Rect,
    pub content: Rect,
    pub sidebar: Option<SidebarLayout>,
    pub player: Rect,
    pub hints: Option<Rect>,
    pub header_mode: HeaderMode,
    pub player_mode: PlayerMode,
}

impl FrameLayout {
    /// Measure every persistent region from the frame once. `allow_sidebar`
    /// supports a future user toggle without duplicating the width rule.
    pub fn measure(frame_area: Rect, allow_sidebar: bool, configured_art_height: u16) -> Option<FrameLayout> {
        if frame_area.width < MIN_FRAME_WIDTH
            || frame_area.height < HEADER_HEIGHT + PLAYER_HEIGHT + HINT_HEIGHT + MIN_CONTENT_HEIGHT
        {
            return None;
        }
        let show_sidebar = allow_sidebar && frame_area.width >= SIDEBAR_AT;
        let hint_height = u16::from(!show_sidebar);
        let body_height = frame_area
            .height
            .saturating_sub(HEADER_HEIGHT + PLAYER_HEIGHT + hint_height);
        let body_area = Rect::new(
            frame_area.x,
            frame_area.y + HEADER_HEIGHT,
            frame_area.width,
            body_height,
        );
        let sidebar = show_sidebar.then(|| {
            let sidebar_area = Rect::new(
                body_area.x,
                body_area.y,
                SIDEBAR_WIDTH.min(body_area.width),
                body_area.height,
            );
            SidebarLayout {
                area: sidebar_area,
                cover_max_height: cover_max_height(sidebar_area, configured_art_height),
            }
        });
        let content = match sidebar {
            Some(sidebar) => Rect::new(
                body_area.x + sidebar.area.width,
                body_area.y,
                body_area.width - sidebar.area.width,
                body_area.height,
            ),
            None => body_area,
        };
        let player_y = frame_area.y + frame_area.height - PLAYER_HEIGHT - hint_height;
        let player = Rect::new(frame_area.x, player_y, frame_area.width, PLAYER_HEIGHT);
        let hints = (hint_height == 1).then(|| {
            Rect::new(
                frame_area.x,
                frame_area.y + frame_area.height - 1,
                frame_area.width,
                HINT_HEIGHT,
            )
        });

        Some(FrameLayout {
            header: Rect::new(frame_area.x, frame_area.y, frame_area.width, HEADER_HEIGHT),
            content,
            sidebar,
            player,
            hints,
            header_mode: header_mode(frame_area.width),
            player_mode: player_mode(frame_area.width),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_boundary_keeps_three_column_browser_width() {
        let below = FrameLayout::measure(Rect::new(0, 0, 134, 24), true, 22).unwrap();
        let at = FrameLayout::measure(Rect::new(0, 0, 135, 24), true, 22).unwrap();

        assert!(below.sidebar.is_none());
        assert_eq!(below.content.width, 134);
        assert_eq!(at.sidebar.unwrap().area.width, 46);
        assert_eq!(at.content.width, 89);
    }

    #[test]
    fn artwork_cannot_consume_sidebar_identity_and_controls() {
        let layout = FrameLayout::measure(Rect::new(0, 0, 135, 24), true, 30).unwrap();
        let sidebar = layout.sidebar.unwrap();

        assert_eq!(sidebar.cover_max_height, 7);
        assert_eq!(layout.content.height, 20);
    }

    #[test]
    fn compact_layout_has_a_hint_row_and_no_hidden_sidebar_area() {
        let layout = FrameLayout::measure(Rect::new(0, 0, 80, 24), true, 22).unwrap();

        assert_eq!(layout.header_mode, HeaderMode::Compact);
        assert_eq!(layout.player_mode, PlayerMode::Full);
        assert!(layout.sidebar.is_none());
        assert_eq!(layout.hints.unwrap().y, 23);
    }
}
