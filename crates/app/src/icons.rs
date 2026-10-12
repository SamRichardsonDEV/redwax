//! Glyph sets. Nerd Font by default because the terminal font on a rice has
//! it, plain text otherwise.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Icons {
    pub brand: &'static str,
    pub play: &'static str,
    pub pause: &'static str,
    pub loading: &'static str,
    pub volume: &'static str,
    pub liked: &'static str,
    pub track: &'static str,
    pub album: &'static str,
    pub playlist: &'static str,
    pub artist: &'static str,
    pub user: &'static str,
    pub up_next: &'static str,
    pub previous: &'static str,
    pub next: &'static str,
    pub shuffle: &'static str,
    pub shuffle_off: &'static str,
    pub repeat: &'static str,
    pub repeat_one: &'static str,
    pub repeat_off: &'static str,
    pub muted: &'static str,
}

impl Icons {
    pub(crate) fn transport(self, player_state: &redwax_player::PlayerState) -> &'static str {
        if player_state.paused || player_state.track().is_none() {
            self.play
        } else if player_state.loading {
            self.loading
        } else {
            self.pause
        }
    }
}

pub const NERD: Icons = Icons {
    brand: "\u{f038d}",
    play: "\u{f040a}",
    pause: "\u{f03e4}",
    loading: "\u{f051f}",
    volume: "\u{f057e}",
    liked: "\u{f02d1}",
    track: "\u{f038d}",
    album: "\u{f0025}",
    playlist: "\u{f0cb8}",
    artist: "\u{f0803}",
    user: "\u{f0004}",
    up_next: "\u{f0411}",
    previous: "\u{f04ae}",
    next: "\u{f04ad}",
    shuffle: "\u{f049d}",
    shuffle_off: "\u{f049e}",
    repeat: "\u{f0456}",
    repeat_one: "\u{f0458}",
    repeat_off: "\u{f0457}",
    muted: "\u{f075f}",
};

pub const TEXT: Icons = Icons {
    brand: "♪",
    play: "▶",
    pause: "⏸",
    loading: "◌",
    volume: "♪",
    liked: "♥",
    track: "♪",
    album: "▤",
    playlist: "≣",
    artist: "◉",
    user: "@",
    up_next: "»",
    previous: "\u{23ee}",
    next: "\u{23ed}",
    shuffle: "⇄",
    shuffle_off: "⇄",
    repeat: "↻",
    repeat_one: "↻1",
    repeat_off: "↻",
    muted: "×",
};

/// The set named in the config, Nerd when the name is unknown.
pub fn icon_set(icon_set_name: &str) -> Icons {
    match icon_set_name {
        "text" => TEXT,
        _ => NERD,
    }
}
