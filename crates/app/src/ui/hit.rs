//! What the pointer can land on. Every widget that draws something clickable
//! returns its regions, `App::draw` collects them, and `App::handle_mouse`
//! reads the map the last frame left. Nothing about the mouse is guessed from
//! coordinates: a region exists only where the paint put one.

use ratatui::layout::Rect;

use crate::app::{Tab, Target};
use crate::keys::Action;
use redwax_ytm::Item;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfirmationChoice {
    #[default]
    Cancel,
    Confirm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowTarget {
    pub browser_target: Target,
    pub projection_revision: u64,
    pub shelf_index: usize,
    pub item_index: usize,
    pub item: Item,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    Overlay,
    Notification(usize),
    Tab(Tab),
    /// A row of a browser list, by its index into the flattened page.
    Row(RowTarget),
    /// The whole list, under the rows, so the wheel knows which one it is over.
    List(Target),
    KeyHint(Action),
    /// The bar itself, because the fraction comes from the position in it.
    SeekBar(Rect),
    Shuffle,
    Repeat,
    /// The transport buttons under the sidebar's track lines.
    Previous,
    PlayPause,
    Next,
    Volume(Rect),
    Mute,
    UpNext(usize),
    /// A timed lyric line, by its index, because a click on it seeks there.
    LyricLine(usize),
    Search,
    /// A row of the themes tab, by its index into the catalogue.
    ThemeRow(usize),
    ToggleSearchVim,
    /// How deep in the browser history that crumb sits.
    Crumb(usize),
    /// One equaliser column, with the region it fills, because the gain a
    /// click asks for comes from where in that column the pointer landed.
    EqBand(usize, Rect),
    ActionRow(usize),
    Confirmation(ConfirmationChoice),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PointerIntent {
    Hit(Hit),
    Seek(f64),
    Volume(f64),
    EqBand {
        band_index: usize,
        vertical_fraction: f64,
    },
}

fn horizontal_fraction(area: Rect, column_x: u16) -> f64 {
    if area.width <= 1 {
        return 0.0;
    }
    f64::from(column_x.saturating_sub(area.x).min(area.width - 1)) / f64::from(area.width - 1)
}

/// The clickable regions of the last frame. Later regions win, so a row beats
/// the list it sits in.
#[derive(Debug, Default, Clone)]
pub struct HitMap {
    hit_regions: Vec<(Rect, Hit)>,
    frame_bounds: Option<Rect>,
    paint_revision: u64,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.hit_regions.clear();
    }

    pub fn bounded(frame_bounds: Rect) -> HitMap {
        HitMap {
            hit_regions: Vec::new(),
            frame_bounds: Some(frame_bounds),
            paint_revision: 0,
        }
    }

    pub(crate) fn install(&mut self, frame_bounds: Rect, paint_revision: u64) {
        self.frame_bounds = Some(frame_bounds);
        self.paint_revision = paint_revision;
        self.hit_regions.retain_mut(|(area, _)| {
            *area = area.intersection(frame_bounds);
            !area.is_empty()
        });
    }

    pub fn revision(&self) -> u64 {
        self.paint_revision
    }

    pub fn add(&mut self, hit_area: Rect, hit_target: Hit) {
        let clipped_hit_area = self
            .frame_bounds
            .map_or(hit_area, |frame_bounds| hit_area.intersection(frame_bounds));
        if !clipped_hit_area.is_empty() {
            self.hit_regions.push((clipped_hit_area, hit_target));
        }
    }

    pub fn extend(&mut self, hit_regions: Vec<(Rect, Hit)>) {
        for (hit_area, hit_target) in hit_regions {
            self.add(hit_area, hit_target);
        }
    }

    pub fn regions(&self) -> &[(Rect, Hit)] {
        &self.hit_regions
    }

    pub fn at(&self, column_x: u16, row_y: u16) -> Option<Hit> {
        self.hit_regions
            .iter()
            .rev()
            .find(|(hit_area, _)| covers(*hit_area, column_x, row_y))
            .map(|(_, hit_target)| hit_target.clone())
    }

    pub fn intent_at(&self, column_x: u16, row_y: u16) -> Option<(u64, PointerIntent)> {
        let hit = self.at(column_x, row_y)?;
        let pointer_intent = match hit {
            Hit::SeekBar(area) => PointerIntent::Seek(horizontal_fraction(area, column_x)),
            Hit::Volume(area) => PointerIntent::Volume(horizontal_fraction(area, column_x)),
            Hit::EqBand(band_index, area) => {
                let vertical_fraction = if area.height <= 1 {
                    0.0
                } else {
                    1.0 - f64::from(row_y.saturating_sub(area.y).min(area.height - 1))
                        / f64::from(area.height - 1)
                };
                PointerIntent::EqBand {
                    band_index,
                    vertical_fraction,
                }
            }
            hit => PointerIntent::Hit(hit),
        };
        Some((self.paint_revision, pointer_intent))
    }

    /// An overlay must stop wheel events from reaching its covered list.
    pub fn list_at(&self, column_x: u16, row_y: u16) -> Option<Target> {
        for (hit_area, hit_target) in self.hit_regions.iter().rev() {
            if !covers(*hit_area, column_x, row_y) {
                continue;
            }
            match hit_target {
                Hit::List(target) => return Some(*target),
                Hit::Overlay | Hit::Notification(_) => return None,
                _ => {}
            }
        }
        None
    }

    /// The first region with this hit, for a test that clicks by name.
    pub fn area_of(&self, predicate: impl Fn(&Hit) -> bool) -> Option<Rect> {
        self.hit_regions
            .iter()
            .find(|(_, hit_target)| predicate(hit_target))
            .map(|(hit_area, _)| *hit_area)
    }
}

fn covers(area: Rect, column_x: u16, row_y: u16) -> bool {
    column_x >= area.x && column_x < area.x + area.width && row_y >= area.y && row_y < area.y + area.height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_clipped_and_empty_targets_are_dropped() {
        let mut map = HitMap::bounded(Rect::new(10, 5, 20, 10));
        map.add(Rect::new(5, 4, 10, 3), Hit::PlayPause);
        map.add(Rect::new(0, 0, 2, 2), Hit::Next);

        assert_eq!(map.regions(), &[(Rect::new(10, 5, 5, 2), Hit::PlayPause)]);
    }

    #[test]
    fn slider_intent_reaches_both_exact_endpoints() {
        let area = Rect::new(7, 2, 11, 1);
        let mut map = HitMap::bounded(Rect::new(0, 0, 40, 10));
        map.add(area, Hit::SeekBar(area));

        assert_eq!(map.intent_at(7, 2).unwrap().1, PointerIntent::Seek(0.0));
        assert_eq!(map.intent_at(17, 2).unwrap().1, PointerIntent::Seek(1.0));
    }

    #[test]
    fn a_later_overlay_blocks_the_covered_list() {
        let bounds = Rect::new(0, 0, 40, 12);
        let covered = Rect::new(8, 3, 20, 6);
        let mut map = HitMap::bounded(bounds);
        map.add(bounds, Hit::List(Target::Home));
        map.add(covered, Hit::Overlay);

        assert_eq!(map.list_at(2, 2), Some(Target::Home));
        assert_eq!(map.list_at(10, 4), None);
        assert_eq!(map.at(10, 4), Some(Hit::Overlay));
    }
}
