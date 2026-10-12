//! The surfaces redwax paints, as ratatui colours: one built from the
//! terminal's palette, Tokyo Night as the fallback. Ported from the Python
//! `theme.py`, whose tests in `test_palette.py` are the specification.

use ratatui::style::Color;

use crate::palette::{HexColor, Palette, ValidatedPalette};

pub fn tokyo_night() -> Palette {
    Palette {
        source: None,
        background: "#1a1b26".into(),
        foreground: "#c0caf5".into(),
        ansi: [
            "#16161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
            "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
        ]
        .map(String::from),
        selection_background: Some("#292e42".into()),
    }
}

pub const DEFAULT_ACCENT: Color = Color::Rgb(0x7a, 0xa2, 0xf7);

/// One accent, no borders, no radius. Surfaces are told apart by tone, or by
/// seams alone when `transparent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub accent: Color,
    /// Page background. `Color::Reset` when transparent.
    pub background: Color,
    /// Header, sidebar, player bar, keys bar. `Color::Reset` when transparent.
    pub surface: Color,
    /// Highlighted row, search box when focused, toasts. Always solid.
    pub panel: Color,
    pub foreground: Color,
    /// `$text-muted`: counts, durations, hints.
    pub muted: Color,
    /// `$foreground-muted`: artist column, breadcrumbs.
    pub dim: Color,
    /// Seams and the unfilled part of the seek bar.
    pub gutter: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub transparent: bool,
}

impl Theme {
    /// The same theme with its one accent swapped, for art-driven colouring.
    pub fn with_accent(&self, accent: Color, theme_name: &str) -> Theme {
        Theme {
            name: theme_name.to_string(),
            accent,
            ..self.clone()
        }
    }

    /// Backgrounds become the terminal default, the only colour a compositor
    /// treats as translucent. The highlight keeps the solid panel colour.
    pub fn transparent(mut self) -> Theme {
        self.transparent = true;
        self.background = Color::Reset;
        self.surface = Color::Reset;
        self
    }
}

/// Map 16 ANSI colours onto the surfaces. Blue is the accent. ANSI black is
/// the second surface whenever it sits close to the background (brightness
/// difference under 0.15), else the background darkened by 0.06. The panel is
/// the selection colour when it is brighter than the background, else the
/// background lightened by 0.08. The gutter is bright black when brighter than
/// the panel, else the panel lightened by 0.08. Muted is the gutter blended
/// 20% towards the foreground. Dim is ANSI 7. Brightness, darken, lighten and
/// blend follow Textual's `Color` (perceived brightness
/// `0.299 r + 0.587 g + 0.114 b`, darken and lighten move CIE L*ab lightness,
/// blend is linear in RGB), so both versions paint the same pixels.
pub fn theme_from_palette(palette: &Palette, theme_name: &str) -> Theme {
    let validated_palette = palette
        .validated()
        .expect("trusted palettes must contain only six-digit RGB colours");
    theme_from_validated_palette(&validated_palette, theme_name)
}

/// Map a palette after its external input has been validated once.
pub fn theme_from_validated_palette(palette: &ValidatedPalette, theme_name: &str) -> Theme {
    let background = Rgb::from(palette.background());
    let foreground = Rgb::from(palette.foreground());
    let black = Rgb::from(palette.ansi(0).expect("validated palettes have sixteen colours"));
    let bright_black = Rgb::from(palette.ansi(8).expect("validated palettes have sixteen colours"));

    let surface = if black.brightness() - background.brightness() < 0.15 {
        black
    } else {
        background.darken(0.06)
    };
    let selection = palette.selection_background().map(Rgb::from);
    let panel = match selection {
        Some(panel) if panel.brightness() > background.brightness() => panel,
        _ => background.lighten(0.08),
    };
    let gutter = if bright_black.brightness() > panel.brightness() {
        bright_black
    } else {
        panel.lighten(0.08)
    };

    Theme {
        name: theme_name.to_string(),
        accent: Rgb::from(palette.ansi(4).expect("validated palettes have sixteen colours")).color(),
        background: background.color(),
        surface: surface.color(),
        panel: panel.color(),
        foreground: foreground.color(),
        muted: gutter.blend(foreground, 0.2).color(),
        dim: Rgb::from(palette.ansi(7).expect("validated palettes have sixteen colours")).color(),
        gutter: gutter.color(),
        success: Rgb::from(palette.ansi(2).expect("validated palettes have sixteen colours")).color(),
        warning: Rgb::from(palette.ansi(3).expect("validated palettes have sixteen colours")).color(),
        error: Rgb::from(palette.ansi(1).expect("validated palettes have sixteen colours")).color(),
        transparent: false,
    }
}

/// Tokyo Night with the default accent, the values `theme_from_palette`
/// produces for `tokyo_night()`. Written out so the first frame needs no
/// colour maths and `test_theme_maps_surfaces_from_the_palette` pins both.
pub fn redwax_theme() -> Theme {
    Theme {
        name: "redwax".into(),
        accent: DEFAULT_ACCENT,
        background: Color::Rgb(0x1a, 0x1b, 0x26),
        surface: Color::Rgb(0x16, 0x16, 0x1e),
        panel: Color::Rgb(0x29, 0x2e, 0x42),
        foreground: Color::Rgb(0xc0, 0xca, 0xf5),
        muted: Color::Rgb(0x5a, 0x62, 0x84),
        dim: Color::Rgb(0xa9, 0xb1, 0xd6),
        gutter: Color::Rgb(0x41, 0x48, 0x68),
        success: Color::Rgb(0x9e, 0xce, 0x6a),
        warning: Color::Rgb(0xe0, 0xaf, 0x68),
        error: Color::Rgb(0xf7, 0x76, 0x8e),
        transparent: false,
    }
}

/// `#rrggbb` to a colour. Anything else is `None`.
pub fn parse_hex(hex_text: &str) -> Option<Color> {
    let hex_digits = hex_text.trim().trim_start_matches('#');
    if hex_digits.len() != 6 {
        return None;
    }
    let rgb_value = u32::from_str_radix(hex_digits, 16).ok()?;
    Some(Color::Rgb(
        (rgb_value >> 16) as u8,
        (rgb_value >> 8) as u8,
        rgb_value as u8,
    ))
}

/// Textual's `Color` maths, ported so the Rust and Python versions agree to
/// the byte. Alpha is dropped because every colour redwax paints is opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

/// CIE L*ab, the space Textual moves lightness in.
struct Lab(f64, f64, f64);

impl Rgb {
    fn color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }

    fn normalized(self) -> (f64, f64, f64) {
        (
            f64::from(self.0) / 255.0,
            f64::from(self.1) / 255.0,
            f64::from(self.2) / 255.0,
        )
    }

    /// Human perceptual brightness, 1 for white and 0 for black.
    fn brightness(self) -> f64 {
        let (red, green, blue) = self.normalized();
        (299.0 * red + 587.0 * green + 114.0 * blue) / 1000.0
    }

    fn darken(self, amount: f64) -> Rgb {
        let Lab(lightness, green_red_axis, blue_yellow_axis) = self.lab();
        Lab(lightness - amount * 100.0, green_red_axis, blue_yellow_axis).rgb()
    }

    fn lighten(self, amount: f64) -> Rgb {
        self.darken(-amount)
    }

    /// `factor` of 0 is this colour and 1 is `destination`.
    fn blend(self, destination: Rgb, factor: f64) -> Rgb {
        if factor <= 0.0 {
            return self;
        }
        if factor >= 1.0 {
            return destination;
        }
        let mix_channel = |source_channel: u8, destination_channel: u8| {
            (f64::from(source_channel)
                + (f64::from(destination_channel) - f64::from(source_channel)) * factor) as i64
                as u8
        };
        Rgb(
            mix_channel(self.0, destination.0),
            mix_channel(self.1, destination.1),
            mix_channel(self.2, destination.2),
        )
    }

    /// Standard RGB with a D65 two degree illuminant, through XYZ.
    fn lab(self) -> Lab {
        let (red, green, blue) = self.normalized();
        let linearize = |channel: f64| {
            if channel > 0.04045 {
                ((channel + 0.055) / 1.055).powf(2.4)
            } else {
                channel / 12.92
            }
        };
        let (linear_red, linear_green, linear_blue) = (linearize(red), linearize(green), linearize(blue));
        let xyz_x = (linear_red * 41.24 + linear_green * 35.76 + linear_blue * 18.05) / 95.047;
        let xyz_y = (linear_red * 21.26 + linear_green * 71.52 + linear_blue * 7.22) / 100.0;
        let xyz_z = (linear_red * 1.93 + linear_green * 11.92 + linear_blue * 95.05) / 108.883;
        let curve_offset = 16.0 / 116.0;
        let lab_curve = |xyz_component: f64| {
            if xyz_component > 0.008856 {
                xyz_component.cbrt()
            } else {
                7.787 * xyz_component + curve_offset
            }
        };
        let (lab_x, lab_y, lab_z) = (lab_curve(xyz_x), lab_curve(xyz_y), lab_curve(xyz_z));
        Lab(
            116.0 * lab_y - 16.0,
            500.0 * (lab_x - lab_y),
            200.0 * (lab_y - lab_z),
        )
    }
}

impl From<HexColor> for Rgb {
    fn from(colour: HexColor) -> Rgb {
        let (red, green, blue) = colour.components();
        Rgb(red, green, blue)
    }
}

impl Lab {
    fn rgb(self) -> Rgb {
        let Lab(lightness, green_red_axis, blue_yellow_axis) = self;
        let lab_y = (lightness + 16.0) / 116.0;
        let lab_x = green_red_axis / 500.0 + lab_y;
        let lab_z = lab_y - blue_yellow_axis / 200.0;
        let curve_offset = 16.0 / 116.0;
        let xyz_y = if lab_y > 0.2068930344 {
            lab_y.powi(3)
        } else {
            (lab_y - curve_offset) / 7.787
        };
        let xyz_x = if lab_x > 0.2068930344 {
            0.95047 * lab_x.powi(3)
        } else {
            0.122059 * (lab_x - curve_offset)
        };
        let xyz_z = if lab_z > 0.2068930344 {
            1.08883 * lab_z.powi(3)
        } else {
            0.139827 * (lab_z - curve_offset)
        };
        let linear_red = xyz_x * 3.2406 + xyz_y * -1.5372 + xyz_z * -0.4986;
        let linear_green = xyz_x * -0.9689 + xyz_y * 1.8758 + xyz_z * 0.0415;
        let linear_blue = xyz_x * 0.0557 + xyz_y * -0.2040 + xyz_z * 1.0570;
        let apply_gamma = |linear_channel: f64| {
            if linear_channel > 0.0031308 {
                1.055 * linear_channel.powf(1.0 / 2.4) - 0.055
            } else {
                12.92 * linear_channel
            }
        };
        let to_u8_channel =
            |linear_channel: f64| ((apply_gamma(linear_channel) * 255.0) as i64).clamp(0, 255) as u8;
        Rgb(
            to_u8_channel(linear_red),
            to_u8_channel(linear_green),
            to_u8_channel(linear_blue),
        )
    }
}
