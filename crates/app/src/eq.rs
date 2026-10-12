//! The eighteen band equaliser, as ffmpeg's `superequalizer` behind mpv's `af`
//! property. A gain is the filter's own multiplier, 1.0 flat and 20 the
//! ceiling, never decibels, so what the panel shows is what mpv is given.
//!
//! The settings file is a convenience and never an authority: a missing or
//! corrupt file reads as a flat equaliser that is switched off.

use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::genre::preset_for;

pub const BANDS: usize = 18;

/// The band centres `superequalizer` fixes, in hertz. Confirmed against
/// `ffmpeg -h filter=superequalizer`.
pub const CENTRES: [u32; BANDS] = [
    65, 92, 131, 185, 262, 370, 523, 740, 1047, 1480, 2093, 2960, 4186, 5920, 8372, 11840, 16744, 20000,
];

/// The gain the filter treats as no change.
pub const FLAT: f32 = 1.0;

/// The highest gain the filter takes. Anything near it is violent, which is
/// why every preset stays under 2.5.
pub const MAX_GAIN: f32 = 20.0;

/// The gain the tallest slider stands for. The keys still reach `MAX_GAIN`,
/// but a column drawn to 20 would leave every useful setting in one cell.
pub const DISPLAY_MAX: f32 = 4.0;

/// How near two gains count as the same preset. Two decimals reach mpv, so
/// anything finer than that is not a difference the filter hears.
const SAME: f32 = 0.005;

pub const CUSTOM: &str = "custom";

pub const PRESETS: &[(&str, [f32; BANDS])] = &[
    ("flat", [FLAT; BANDS]),
    (
        "bass",
        [
            2.00, 1.90, 1.70, 1.50, 1.30, 1.10, 1.00, 1.00, 1.00, 1.00, 1.00, 1.00, 1.00, 1.05, 1.10, 1.10,
            1.05, 1.00,
        ],
    ),
    (
        "treble",
        [
            1.00, 1.00, 1.00, 1.00, 1.00, 1.00, 1.00, 1.00, 1.00, 1.05, 1.15, 1.30, 1.45, 1.60, 1.70, 1.80,
            1.85, 1.80,
        ],
    ),
    (
        "vocal",
        [
            0.85, 0.85, 0.90, 0.95, 1.00, 1.10, 1.25, 1.40, 1.50, 1.50, 1.40, 1.30, 1.15, 1.05, 1.00, 0.95,
            0.90, 0.90,
        ],
    ),
    (
        "loudness",
        [
            1.80, 1.70, 1.55, 1.35, 1.15, 1.00, 0.90, 0.85, 0.85, 0.90, 1.00, 1.10, 1.25, 1.40, 1.55, 1.65,
            1.70, 1.65,
        ],
    ),
    (
        "warm",
        [
            1.25, 1.30, 1.35, 1.30, 1.20, 1.15, 1.10, 1.05, 1.00, 0.95, 0.90, 0.85, 0.80, 0.75, 0.70, 0.65,
            0.60, 0.60,
        ],
    ),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Eq {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "flat_gains")]
    pub gains: [f32; BANDS],
}

fn flat_gains() -> [f32; BANDS] {
    [FLAT; BANDS]
}

impl Default for Eq {
    fn default() -> Eq {
        Eq {
            enabled: false,
            gains: flat_gains(),
        }
    }
}

impl Eq {
    /// The lavfi graph for mpv's `af`, or nothing when there is no work to do.
    /// A flat equaliser costs no filter at all, which is the point of the
    /// second guard.
    pub fn graph(&self) -> Option<String> {
        if !self.enabled || self.is_flat() {
            return None;
        }
        let band_filters: Vec<String> = self
            .gains
            .iter()
            .enumerate()
            .map(|(band_index, gain_multiplier)| format!("{}b={gain_multiplier:.2}", band_index + 1))
            .collect();
        Some(format!("lavfi=[superequalizer={}]", band_filters.join(":")))
    }

    pub fn is_flat(&self) -> bool {
        self.gains.iter().all(|gain| (gain - FLAT).abs() < SAME)
    }

    pub fn get(&self, band_index: usize) -> f32 {
        self.gains.get(band_index).copied().unwrap_or(FLAT)
    }

    pub fn set(&mut self, band_index: usize, gain_multiplier: f32) {
        if let Some(band_gain) = self.gains.get_mut(band_index) {
            *band_gain = gain_multiplier.clamp(0.0, MAX_GAIN);
        }
    }

    pub fn nudge(&mut self, band_index: usize, gain_delta: f32) {
        self.set(band_index, self.get(band_index) + gain_delta);
    }

    pub fn reset(&mut self) {
        self.gains = flat_gains();
    }

    pub fn load(&mut self, preset: &str) -> bool {
        let Some((_, preset_gains)) = PRESETS.iter().find(|(preset_name, _)| *preset_name == preset) else {
            return false;
        };
        self.gains = *preset_gains;
        true
    }

    /// The preset the gains match, or `custom` when they match none.
    pub fn preset_name(&self) -> &'static str {
        PRESETS
            .iter()
            .find(|(_, preset_gains)| {
                preset_gains
                    .iter()
                    .zip(self.gains.iter())
                    .all(|(preset_gain, current_gain)| (preset_gain - current_gain).abs() < SAME)
            })
            .map(|(preset_name, _)| *preset_name)
            .unwrap_or(CUSTOM)
    }

    /// The preset after the one the gains match, so `p` walks the whole table.
    pub fn next_preset(&self) -> &'static str {
        let current_preset_index = PRESETS
            .iter()
            .position(|(preset_name, _)| *preset_name == self.preset_name());
        match current_preset_index {
            Some(current_preset_index) => PRESETS[(current_preset_index + 1) % PRESETS.len()].0,
            None => PRESETS[0].0,
        }
    }

    /// A missing or corrupt file reads as nothing remembered.
    pub fn read(settings_path: &Path) -> Eq {
        std::fs::read_to_string(settings_path)
            .ok()
            .and_then(|settings_json| serde_json::from_str(&settings_json).ok())
            .unwrap_or_default()
    }

    /// A temporary file and a rename, so a kill never leaves half an object.
    pub fn write(&self, settings_path: &Path) -> std::io::Result<()> {
        if let Some(settings_directory) = settings_path.parent() {
            std::fs::create_dir_all(settings_directory)?;
        }
        let temporary_path = settings_path.with_extension("json.tmp");
        std::fs::write(
            &temporary_path,
            serde_json::to_vec(self).expect("the equaliser serialises"),
        )?;
        std::fs::rename(temporary_path, settings_path)
    }
}

/// The settings file, beside the page cache under `~/.cache/redwax`.
pub fn path_for(config: &Config) -> PathBuf {
    match config.page_cache_dir.parent() {
        Some(cache_directory) => cache_directory.join("eq.json"),
        None => PathBuf::from("eq.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("redwax-eq-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("the temporary directory is writable");
        directory
    }

    #[test]
    fn test_a_flat_equaliser_has_no_graph() {
        let mut eq = Eq::default();
        assert_eq!(eq.graph(), None, "off and flat");
        eq.enabled = true;
        assert_eq!(eq.graph(), None, "on and flat is still no filter");
        eq.set(0, 1.5);
        assert!(eq.graph().is_some());
        eq.enabled = false;
        assert_eq!(eq.graph(), None, "off wins over a raised band");
    }

    #[test]
    fn test_the_graph_names_every_band_to_two_decimals() {
        let mut eq = Eq {
            enabled: true,
            ..Eq::default()
        };
        eq.set(0, 2.0);
        eq.set(17, 0.5);
        let graph = eq.graph().expect("a raised band gives a graph");
        assert!(
            graph.starts_with("lavfi=[superequalizer=1b=2.00:2b=1.00:"),
            "{graph}"
        );
        assert!(graph.ends_with(":17b=1.00:18b=0.50]"), "{graph}");
        assert_eq!(graph.matches("b=").count(), BANDS);
    }

    #[test]
    fn test_a_gain_is_clamped_to_what_the_filter_takes() {
        let mut eq = Eq::default();
        eq.set(3, -4.0);
        assert_eq!(eq.get(3), 0.0);
        eq.set(3, 90.0);
        assert_eq!(eq.get(3), MAX_GAIN);
        eq.nudge(3, 5.0);
        assert_eq!(eq.get(3), MAX_GAIN, "no room left above the ceiling");
        eq.set(99, 2.0);
        assert_eq!(eq.get(99), FLAT, "a band that does not exist reads as flat");
    }

    #[test]
    fn test_every_preset_has_a_band_for_every_centre() {
        assert_eq!(CENTRES.len(), BANDS);
        assert_eq!(PRESETS[0].0, "flat");
        assert_eq!(PRESETS.len(), 6);
        for (name, gains) in PRESETS {
            assert_eq!(gains.len(), BANDS, "{name}");
            for gain in gains {
                assert!((0.5..=2.5).contains(gain), "{name} band {gain} is out of taste");
            }
        }
    }

    #[test]
    fn test_the_preset_name_follows_the_gains() {
        let mut eq = Eq::default();
        assert_eq!(eq.preset_name(), "flat");
        assert!(eq.load("bass"));
        assert_eq!(eq.preset_name(), "bass");
        assert_eq!(eq.next_preset(), "treble");
        eq.nudge(0, 0.4);
        assert_eq!(eq.preset_name(), CUSTOM);
        assert_eq!(eq.next_preset(), "flat", "custom walks back to the start");
        assert!(!eq.load("nothing named this"));
        eq.reset();
        assert_eq!(eq.preset_name(), "flat");
    }

    #[test]
    fn test_the_settings_survive_the_round_trip() {
        let path = dir("round").join("eq.json");
        let mut eq = Eq {
            enabled: true,
            ..Eq::default()
        };
        eq.load("warm");
        eq.write(&path).expect("the temporary directory is writable");

        let reread = Eq::read(&path);
        assert_eq!(reread, eq);
        assert_eq!(reread.preset_name(), "warm");
    }

    #[test]
    fn test_a_corrupt_file_reads_as_a_flat_equaliser() {
        let path = dir("corrupt").join("eq.json");
        std::fs::write(&path, "{not json").expect("the temporary directory is writable");
        assert_eq!(Eq::read(&path), Eq::default());
        assert_eq!(Eq::read(Path::new("/nonexistent/redwax/eq.json")), Eq::default());
    }
}

/// Cells the gain keys move a band by.
pub const STEP: f32 = 0.1;

/// Bands the shift keys jump by, so the far end of the row is a few presses
/// away rather than eighteen.
pub const JUMP: usize = 5;

/// What the app has to do once the panel has answered.
///
/// The panel changes its own state and says what that costs. It cannot send
/// the filter itself, because reaching mpv is a task and the panel is not
/// where tasks are spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Only what the panel draws changed.
    Redraw,
    /// The filter chain changed, so mpv has to hear the new graph.
    Filter,
    /// Genre matching was turned on or off, so the playing track's genre is
    /// worth asking about again.
    Genre,
}

/// The equaliser as the user drives it: the bands, the one the keys are on,
/// whether the panel is open, and whether a track change picks a preset.
///
/// Every rule about how a key moves a band lives here. The app hands over a
/// key and pays for the `Effect` it gets back, and knows none of the rest: not
/// the band count, not the step, not that a raised band switches the equaliser
/// on, and not that setting a band by hand ends genre matching.
///
/// `settings` is the one way out, and it is read only. The view needs every
/// gain to draw eighteen columns, so hiding them would buy nothing, but
/// nothing outside these methods may change one.
pub struct EqPanel {
    settings: Eq,
    selected_band: usize,
    visible: bool,
    /// The settings file is behind what the panel shows.
    has_unsaved_changes: bool,
    /// Whether a track change loads the preset its genre asks for. Config
    /// gives the first value and a band the user sets turns it off.
    genre_match: bool,
    /// The genre of the playing track, once anything answered for it.
    genre: Option<String>,
}

impl EqPanel {
    pub fn new(settings: Eq, genre_match: bool) -> EqPanel {
        EqPanel {
            settings,
            selected_band: 0,
            visible: false,
            has_unsaved_changes: false,
            genre_match,
            genre: None,
        }
    }

    /// What the panel is set to, for the view that draws it. Read only, so a
    /// gain still moves only through the methods that keep the rules.
    pub fn settings(&self) -> &Eq {
        &self.settings
    }

    pub fn band(&self) -> usize {
        self.selected_band
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn genre(&self) -> Option<&str> {
        self.genre.as_deref()
    }

    pub fn genre_match(&self) -> bool {
        self.genre_match
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    /// The graph mpv should be running, which is nothing at all when the
    /// equaliser is off or flat.
    pub fn graph(&self) -> Option<String> {
        self.settings.graph()
    }

    /// One key of the panel. `None` means the panel did not want it, so the
    /// page underneath gets a look.
    pub fn key(&mut self, key_code: KeyCode) -> Option<Effect> {
        match key_code {
            KeyCode::Char('e') | KeyCode::Esc => {
                self.visible = false;
                Some(Effect::Redraw)
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.selected_band = (self.selected_band + BANDS - 1) % BANDS;
                Some(Effect::Redraw)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.selected_band = (self.selected_band + 1) % BANDS;
                Some(Effect::Redraw)
            }
            KeyCode::Char('H') => {
                self.selected_band = self.selected_band.saturating_sub(JUMP);
                Some(Effect::Redraw)
            }
            KeyCode::Char('L') => {
                self.selected_band = (self.selected_band + JUMP).min(BANDS - 1);
                Some(Effect::Redraw)
            }
            KeyCode::Char('k') | KeyCode::Up => Some(self.nudge(self.selected_band, STEP)),
            KeyCode::Char('j') | KeyCode::Down => Some(self.nudge(self.selected_band, -STEP)),
            KeyCode::Char('0') => Some(self.set(self.selected_band, FLAT)),
            KeyCode::Char('r') => {
                self.settings.reset();
                self.settings.enabled = false;
                Some(self.touched())
            }
            KeyCode::Char('p') => {
                let next_preset = self.settings.next_preset();
                self.settings.load(next_preset);
                Some(self.touched())
            }
            KeyCode::Char('g') => {
                self.genre_match = !self.genre_match;
                Some(Effect::Genre)
            }
            KeyCode::Char(' ') => {
                self.settings.enabled = !self.settings.enabled;
                Some(self.applied())
            }
            _ => None,
        }
    }

    /// Move one band, which is what the wheel over a column does as well as
    /// the gain keys. The keys follow the band that moved.
    pub fn nudge(&mut self, band_index: usize, gain_delta: f32) -> Effect {
        self.selected_band = band_index.min(BANDS - 1);
        self.settings.nudge(self.selected_band, gain_delta);
        self.touched()
    }

    /// Put one band at a gain, which is what a click inside a column does.
    pub fn set(&mut self, band_index: usize, gain_multiplier: f32) -> Effect {
        self.selected_band = band_index.min(BANDS - 1);
        self.settings.set(self.selected_band, gain_multiplier);
        self.touched()
    }

    /// A genre answered for the playing track. The preset follows it unless
    /// the user has taken the panel over, and an unchanged preset costs
    /// nothing.
    pub fn genre_answered(&mut self, genre: String) -> Option<Effect> {
        let genre_preset = preset_for(&genre);
        self.genre = Some(genre);
        if !self.genre_match {
            return Some(Effect::Redraw);
        }
        let genre_preset = genre_preset?;
        if self.settings.preset_name() == genre_preset {
            return Some(Effect::Redraw);
        }
        self.settings.load(genre_preset);
        Some(self.applied())
    }

    /// A new track is playing, so the genre it had is no longer the answer.
    pub fn forget_genre(&mut self) {
        self.genre = None;
    }

    /// Put what the panel shows back on disk, if anything moved since the last
    /// time. Called from the tick rather than from each key, so a held key
    /// costs one write and not thirty.
    pub fn write_if_due(&mut self, settings_path: &std::path::Path) {
        if !self.has_unsaved_changes {
            return;
        }
        self.has_unsaved_changes = false;
        let _ = self.settings.write(settings_path);
    }

    /// A change the user made themselves. Genre matching stops for the rest of
    /// the session, so the next track never argues with what was just set.
    fn touched(&mut self) -> Effect {
        self.genre_match = false;
        self.applied()
    }

    /// A raised band switches the equaliser on, because a band that moves and
    /// is not heard reads as a broken panel.
    fn applied(&mut self) -> Effect {
        if !self.settings.is_flat() {
            self.settings.enabled = true;
        }
        self.has_unsaved_changes = true;
        Effect::Filter
    }
}

#[cfg(test)]
mod panel_tests {
    use super::*;

    fn panel() -> EqPanel {
        EqPanel::new(Eq::default(), true)
    }

    #[test]
    fn a_key_the_panel_does_not_want_falls_through() {
        assert_eq!(panel().key(KeyCode::Char('z')), None);
    }

    #[test]
    fn the_band_keys_wrap_and_the_jump_keys_clamp() {
        let mut panel = panel();
        assert_eq!(panel.key(KeyCode::Char('h')), Some(Effect::Redraw));
        assert_eq!(panel.band(), BANDS - 1, "left from the first band did not wrap");
        assert_eq!(panel.key(KeyCode::Char('l')), Some(Effect::Redraw));
        assert_eq!(panel.band(), 0);

        panel.key(KeyCode::Char('L'));
        assert_eq!(panel.band(), JUMP);
        for _ in 0..BANDS {
            panel.key(KeyCode::Char('L'));
        }
        assert_eq!(panel.band(), BANDS - 1, "the jump ran off the end");
    }

    #[test]
    fn a_raised_band_switches_the_equaliser_on_and_ends_genre_matching() {
        let mut panel = panel();
        assert!(!panel.settings().enabled);
        assert!(panel.genre_match());

        assert_eq!(panel.key(KeyCode::Char('k')), Some(Effect::Filter));
        assert!(panel.settings().enabled, "a raised band was not heard");
        assert!(
            !panel.genre_match(),
            "a band of the user's own kept genre matching"
        );
    }

    #[test]
    fn reset_puts_the_panel_back_to_how_it_ships() {
        let mut panel = panel();
        panel.key(KeyCode::Char('k'));
        assert!(!panel.settings().is_flat());

        assert_eq!(panel.key(KeyCode::Char('r')), Some(Effect::Filter));
        assert!(panel.settings().is_flat());
        assert!(!panel.settings().enabled);
        assert_eq!(panel.graph(), None, "a flat panel that is off still sent a graph");
    }

    #[test]
    fn a_genre_loads_its_preset_only_while_matching_is_on() {
        let mut panel = panel();
        assert_eq!(
            panel.genre_answered("Electronic".to_string()),
            Some(Effect::Filter)
        );
        assert_eq!(panel.genre(), Some("Electronic"));
        let after = panel.settings().preset_name().to_string();
        assert_ne!(after, "flat", "the genre preset was not loaded");

        // The same genre again changes nothing, so mpv is not asked twice.
        assert_eq!(
            panel.genre_answered("Electronic".to_string()),
            Some(Effect::Redraw)
        );

        // A band of the user's own ends matching, and the next genre is only
        // something to show.
        panel.key(KeyCode::Char('k'));
        assert_eq!(panel.genre_answered("Jazz".to_string()), Some(Effect::Redraw));
        assert_eq!(panel.genre(), Some("Jazz"));
    }

    #[test]
    fn toggling_genre_matching_asks_for_the_genre_again() {
        let mut panel = panel();
        assert_eq!(panel.key(KeyCode::Char('g')), Some(Effect::Genre));
        assert!(!panel.genre_match());
        assert_eq!(panel.key(KeyCode::Char('g')), Some(Effect::Genre));
        assert!(panel.genre_match());
    }

    #[test]
    fn escape_closes_the_panel_without_touching_the_sound() {
        let mut panel = panel();
        panel.toggle();
        assert!(panel.visible());
        assert_eq!(panel.key(KeyCode::Esc), Some(Effect::Redraw));
        assert!(!panel.visible());
        assert!(panel.settings().is_flat());
    }
}
