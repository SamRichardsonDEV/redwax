//! The two accent cases from `test_art_and_config.py`.

use image::{DynamicImage, Rgb, RgbImage};
use ratatui::style::Color;
use redwax::art::accent_color;
use redwax::theme::DEFAULT_ACCENT;

#[test]
fn test_accent_prefers_the_vivid_colour_over_a_bigger_grey_area() {
    let mut image = RgbImage::from_pixel(100, 100, Rgb([40, 40, 40]));
    for x in 0..30 {
        for y in 0..100 {
            image.put_pixel(x, y, Rgb([20, 90, 200]));
        }
    }
    let accent = accent_color(&DynamicImage::ImageRgb8(image));
    let Color::Rgb(r, g, b) = accent else {
        panic!("the accent is always a true colour")
    };
    assert!(b > r && b > g, "expected a blue accent, got {accent:?}");
    // Pinned to what the Python version prints for the same image.
    assert_eq!(accent, Color::Rgb(0x2e, 0x77, 0xea));
}

#[test]
fn test_accent_falls_back_for_monochrome_art() {
    let grey = RgbImage::from_pixel(10, 10, Rgb([128, 128, 128]));
    assert_eq!(accent_color(&DynamicImage::ImageRgb8(grey)), DEFAULT_ACCENT);
}
