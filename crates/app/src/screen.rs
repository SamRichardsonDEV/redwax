//! What the terminal is currently showing, and whether it is still true.
//!
//! The whole point is the repaint rule. A tick that changed nothing must not
//! cost a layout pass, so the last frame is kept and put back. Getting that
//! wrong either burns a CPU on a still screen or leaves the screen stale, and
//! it is easy to get wrong from the outside: the kept frame is only usable
//! while the terminal is the same size it was painted at. Callers say what
//! changed and ask whether to paint. They never see the buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::ui::HitMap;

#[derive(Debug, Default)]
pub struct Screen {
    terminal_width: u16,
    terminal_height: u16,
    /// Ticks the animation has run. It stops while the music does, so a still
    /// screen settles and the next tick paints nothing.
    animation_frame: u64,
    /// Something the screen shows has changed.
    needs_repaint: bool,
    /// The last painted frame, put back whenever a tick changed nothing.
    last_painted_buffer: Option<Buffer>,
    /// Where the last frame put everything the pointer can reach.
    hit_map: HitMap,
    /// The rows the last draw fitted, so a page jump moves by a real page.
    visible_rows: u16,
    /// Changes whenever resize makes active pointer geometry obsolete.
    geometry_revision: u64,
    painted_frame_revision: u64,
}

impl Screen {
    /// The rows a list had last time it was drawn. Ten before the first draw,
    /// which is a page worth of movement rather than none.
    pub fn new() -> Screen {
        Screen {
            visible_rows: 10,
            ..Screen::default()
        }
    }

    pub fn width(&self) -> u16 {
        self.terminal_width
    }

    pub fn animation_frame(&self) -> u64 {
        self.animation_frame
    }

    pub fn visible_rows(&self) -> u16 {
        self.visible_rows
    }

    pub fn hit_map(&self) -> &HitMap {
        &self.hit_map
    }

    pub fn geometry_revision(&self) -> u64 {
        self.geometry_revision
    }

    /// Something changed, so the next draw paints rather than restores.
    pub fn touch(&mut self) {
        self.needs_repaint = true;
    }

    /// Advance the animation. True when the screen is now worth repainting.
    pub fn tick(&mut self, animations_active: bool) -> bool {
        if animations_active {
            self.animation_frame = self.animation_frame.wrapping_add(1);
            self.needs_repaint = true;
        }
        self.needs_repaint
    }

    /// The frame to put back rather than paint, if there is one. A size change
    /// invalidates it, because a buffer painted at another size is the wrong
    /// shape for this one.
    pub fn restore(&mut self, terminal_area: Rect) -> Option<&Buffer> {
        if self
            .last_painted_buffer
            .as_ref()
            .is_some_and(|painted_buffer| painted_buffer.area != terminal_area)
        {
            self.last_painted_buffer = None;
            self.hit_map.clear();
            self.visible_rows = 0;
            self.geometry_revision = self.geometry_revision.wrapping_add(1);
        }
        self.terminal_width = terminal_area.width;
        self.terminal_height = terminal_area.height;
        match self.needs_repaint {
            true => None,
            false => self.last_painted_buffer.as_ref(),
        }
    }

    /// A frame was painted. The kept copy and the hit map are replaced
    /// together, because a pointer answered from one and a screen showing the
    /// other is how a click lands on the wrong row.
    pub fn record_painted_frame(&mut self, painted_buffer: &Buffer, mut hit_map: HitMap) {
        self.painted_frame_revision = self.painted_frame_revision.wrapping_add(1);
        hit_map.install(painted_buffer.area, self.painted_frame_revision);
        self.hit_map = hit_map;
        self.needs_repaint = false;
        self.last_painted_buffer = Some(painted_buffer.clone());
    }

    /// The rows the draw measured, before the rest of the frame is painted.
    pub fn record_visible_rows(&mut self, visible_rows: u16) {
        self.visible_rows = visible_rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(buffer_area: Rect) -> Buffer {
        Buffer::empty(buffer_area)
    }

    #[test]
    fn a_fresh_screen_paints_rather_than_restores() {
        let mut screen = Screen::new();
        let area = Rect::new(0, 0, 80, 24);
        assert!(screen.restore(area).is_none(), "restored before painting once");
        assert_eq!(screen.width(), 80, "the size is taken even when nothing is kept");
        assert_eq!(
            screen.visible_rows(),
            10,
            "a page move before the first draw is not zero"
        );
    }

    #[test]
    fn a_painted_screen_is_restored_until_something_changes() {
        let mut screen = Screen::new();
        let area = Rect::new(0, 0, 80, 24);
        screen.restore(area);
        screen.record_visible_rows(17);
        screen.record_painted_frame(&buffer(area), HitMap::default());
        assert_eq!(screen.visible_rows(), 17);
        assert!(screen.restore(area).is_some(), "a still screen was repainted");

        screen.touch();
        assert!(screen.restore(area).is_none(), "a changed screen was restored");
    }

    #[test]
    fn a_resize_throws_the_kept_frame_away() {
        let mut screen = Screen::new();
        let area = Rect::new(0, 0, 80, 24);
        screen.restore(area);
        screen.record_painted_frame(&buffer(area), HitMap::default());

        let wider = Rect::new(0, 0, 100, 24);
        assert!(
            screen.restore(wider).is_none(),
            "a frame painted at another size was put back"
        );
        assert_eq!(screen.width(), 100);
    }

    #[test]
    fn a_resize_discards_targets_until_the_next_paint() {
        let mut screen = Screen::new();
        let area = Rect::new(0, 0, 80, 24);
        let mut hit_map = HitMap::default();
        hit_map.add(Rect::new(0, 0, 4, 1), crate::ui::Hit::PlayPause);
        screen.restore(area);
        screen.record_visible_rows(17);
        screen.record_painted_frame(&buffer(area), hit_map);

        let revision = screen.geometry_revision();
        assert!(screen.restore(Rect::new(0, 0, 4, 4)).is_none());
        assert_ne!(screen.geometry_revision(), revision);
        assert!(screen.hit_map().at(0, 0).is_none());
        assert_eq!(screen.visible_rows(), 0);
        assert!(screen.restore(area).is_none());
    }

    #[test]
    fn a_still_screen_stops_counting_frames() {
        let mut screen = Screen::new();
        let area = Rect::new(0, 0, 80, 24);
        screen.restore(area);
        screen.record_painted_frame(&buffer(area), HitMap::default());

        assert!(!screen.tick(false), "a still screen asked to be repainted");
        assert_eq!(screen.animation_frame(), 0);
        assert!(screen.tick(true));
        assert_eq!(screen.animation_frame(), 1);
    }
}
