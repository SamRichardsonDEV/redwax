//! Album art: fetch, cache on disk and in memory, and distil an accent colour
//! from it. Ported from the Python `art.py`, whose tests in
//! `test_art_and_config.py` are the specification.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::style::Color;
use sha1::{Digest, Sha1};

use crate::theme::DEFAULT_ACCENT;

pub const MEMORY_LIMIT: usize = 32;

/// Never raises. A miss on the network, a corrupt file, or an image that does
/// not decode all read as `None`. Disk entries are named by the sha1 of the
/// URL with an `.img` extension, the same names the Python version wrote.
pub struct ArtCache {
    pub directory: PathBuf,
    /// Least recently used last, so eviction takes the front.
    memory_cache: Mutex<Vec<(String, DynamicImage)>>,
    http_client: reqwest::Client,
}

impl ArtCache {
    pub fn new(directory: PathBuf) -> ArtCache {
        let _ = crate::private_file::prepare_directory(&directory);
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .unwrap_or_default();
        ArtCache {
            directory,
            memory_cache: Mutex::new(Vec::new()),
            http_client,
        }
    }

    /// Memory first, then disk, then one GET with a 10 s timeout following
    /// redirects. The decoded image is kept in memory, oldest of 32 evicted.
    pub async fn fetch(&self, image_url: &str) -> Option<DynamicImage> {
        if let Some(cached_image) = self.remembered(image_url) {
            return Some(cached_image);
        }
        let encoded_image = match self.read_disk(image_url).await {
            Some(disk_image) => disk_image,
            None => {
                let downloaded_image = self.download(image_url).await?;
                self.write_disk(image_url, &downloaded_image).await;
                downloaded_image
            }
        };
        let decoded_image = image::load_from_memory(&encoded_image).ok()?;
        let rgb_image = DynamicImage::ImageRgb8(decoded_image.to_rgb8());
        self.remember(image_url, &rgb_image);
        Some(rgb_image)
    }

    async fn download(&self, image_url: &str) -> Option<Vec<u8>> {
        let response = self.http_client.get(image_url).send().await.ok()?;
        let successful_response = response.error_for_status().ok()?;
        successful_response
            .bytes()
            .await
            .ok()
            .map(|response_bytes| response_bytes.to_vec())
    }

    fn remembered(&self, image_url: &str) -> Option<DynamicImage> {
        let mut memory_cache = self.memory_cache.lock().ok()?;
        let cached_image_index = memory_cache
            .iter()
            .position(|(cached_url, _)| cached_url == image_url)?;
        let cached_entry = memory_cache.remove(cached_image_index);
        let cached_image = cached_entry.1.clone();
        memory_cache.push(cached_entry);
        Some(cached_image)
    }

    fn remember(&self, image_url: &str, image: &DynamicImage) {
        let Ok(mut memory_cache) = self.memory_cache.lock() else {
            return;
        };
        memory_cache.retain(|(cached_url, _)| cached_url != image_url);
        memory_cache.push((image_url.to_string(), image.clone()));
        if memory_cache.len() > MEMORY_LIMIT {
            memory_cache.remove(0);
        }
    }

    fn disk_path(&self, image_url: &str) -> PathBuf {
        let url_digest = hex::encode(Sha1::digest(image_url.as_bytes()));
        self.directory.join(format!("{url_digest}.img"))
    }

    async fn read_disk(&self, image_url: &str) -> Option<Vec<u8>> {
        tokio::fs::read(self.disk_path(image_url)).await.ok()
    }

    async fn write_disk(&self, image_url: &str, encoded_image: &[u8]) {
        let cache_path = self.disk_path(image_url);
        let encoded_image = encoded_image.to_vec();
        let _ = tokio::task::spawn_blocking(move || crate::private_file::write(&cache_path, &encoded_image))
            .await;
    }
}

/// A vivid, mid-lightness colour that represents the art. Every colour of a
/// 48 by 48 downscale is scored by count times saturation to the 1.5 times
/// closeness of lightness to 0.5, skipping saturation under 0.25 and lightness
/// under 0.12 or over 0.92. The winner's lightness is clamped to 0.55 to 0.70
/// and its saturation raised to at least 0.6. Monochrome art gives
/// `DEFAULT_ACCENT`.
pub fn accent_color(image: &DynamicImage) -> Color {
    let thumbnail = image.resize_exact(48, 48, FilterType::CatmullRom).to_rgb8();
    let mut color_counts: BTreeMap<[u8; 3], f64> = BTreeMap::new();
    for pixel in thumbnail.pixels() {
        *color_counts.entry(pixel.0).or_default() += 1.0;
    }
    let mut best_score = 0.0;
    let mut best_hue_lightness_saturation = None;
    for (rgb_channels, pixel_count) in color_counts {
        let (hue, lightness, saturation) = rgb_to_hls(rgb_channels);
        if saturation < 0.25 || lightness < 0.12 || lightness > 0.92 {
            continue;
        }
        let color_score = pixel_count * saturation.powf(1.5) * (1.0 - (lightness - 0.5).abs());
        if color_score > best_score {
            best_score = color_score;
            best_hue_lightness_saturation = Some((hue, lightness, saturation));
        }
    }
    let Some((hue, lightness, saturation)) = best_hue_lightness_saturation else {
        return DEFAULT_ACCENT;
    };
    let (red, green, blue) = hls_to_rgb(hue, lightness.clamp(0.55, 0.70), saturation.max(0.6));
    let to_u8_channel = |channel: f64| (channel * 255.0).round() as u8;
    Color::Rgb(to_u8_channel(red), to_u8_channel(green), to_u8_channel(blue))
}

/// Python's `colorsys`, ported so the two versions pick the same colour.
fn rgb_to_hls(rgb: [u8; 3]) -> (f64, f64, f64) {
    let [red, green, blue] = rgb.map(|channel| f64::from(channel) / 255.0);
    let brightest_channel = red.max(green).max(blue);
    let darkest_channel = red.min(green).min(blue);
    let lightness = (brightest_channel + darkest_channel) / 2.0;
    if darkest_channel == brightest_channel {
        return (0.0, lightness, 0.0);
    }
    let channel_range = brightest_channel - darkest_channel;
    let saturation = if lightness <= 0.5 {
        channel_range / (brightest_channel + darkest_channel)
    } else {
        channel_range / (2.0 - brightest_channel - darkest_channel)
    };
    let red_distance = (brightest_channel - red) / channel_range;
    let green_distance = (brightest_channel - green) / channel_range;
    let blue_distance = (brightest_channel - blue) / channel_range;
    let hue = if red == brightest_channel {
        blue_distance - green_distance
    } else if green == brightest_channel {
        2.0 + red_distance - blue_distance
    } else {
        4.0 + green_distance - red_distance
    };
    ((hue / 6.0).rem_euclid(1.0), lightness, saturation)
}

fn hls_to_rgb(hue: f64, lightness: f64, saturation: f64) -> (f64, f64, f64) {
    if saturation == 0.0 {
        return (lightness, lightness, lightness);
    }
    let upper_channel_value = if lightness <= 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - (lightness * saturation)
    };
    let lower_channel_value = 2.0 * lightness - upper_channel_value;
    let one_third_turn = 1.0 / 3.0;
    (
        hls_channel(lower_channel_value, upper_channel_value, hue + one_third_turn),
        hls_channel(lower_channel_value, upper_channel_value, hue),
        hls_channel(lower_channel_value, upper_channel_value, hue - one_third_turn),
    )
}

fn hls_channel(lower_value: f64, upper_value: f64, hue: f64) -> f64 {
    let hue = hue.rem_euclid(1.0);
    if hue < 1.0 / 6.0 {
        return lower_value + (upper_value - lower_value) * hue * 6.0;
    }
    if hue < 0.5 {
        return upper_value;
    }
    if hue < 2.0 / 3.0 {
        return lower_value + (upper_value - lower_value) * (2.0 / 3.0 - hue) * 6.0;
    }
    lower_value
}
