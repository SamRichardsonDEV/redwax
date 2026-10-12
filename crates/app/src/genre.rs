//! The genre of a track, and the equaliser preset it asks for.
//!
//! YouTube Music serves no genre on any response redwax reads, so the answer
//! comes from the iTunes Search API, which needs no key and no account. The
//! query carries the artist and the title and nothing else.
//!
//! One flat file beside the page cache remembers what was found, so a track
//! costs one request ever. A failure of any kind is no genre, never an error
//! the user is shown.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use serde::Deserialize;

use crate::config::Config;

/// How long a lookup is worth waiting for. It runs in the background, so the
/// only cost of a slow answer is a preset that lands a moment late.
const TIMEOUT: Duration = Duration::from_secs(6);

pub type BoxLookup = Pin<Box<dyn Future<Output = Option<String>> + Send>>;

/// The artist and the title onto a genre. A type rather than a call, so the
/// app tests never reach the network.
pub type Lookup = Arc<dyn Fn(String, String) -> BoxLookup + Send + Sync>;

pub fn itunes() -> Lookup {
    Arc::new(|artist, title| Box::pin(async move { lookup(&artist, &title).await }))
}

#[derive(Deserialize)]
struct ItunesSearchResponse {
    #[serde(default)]
    results: Vec<ItunesSearchResult>,
}

#[derive(Deserialize)]
struct ItunesSearchResult {
    #[serde(rename = "primaryGenreName")]
    genre: Option<String>,
}

pub async fn lookup(artist: &str, title: &str) -> Option<String> {
    let search_term = format!("{artist} {title}");
    let search_term = search_term.trim();
    if search_term.is_empty() {
        return None;
    }
    let request_url = format!(
        "https://itunes.apple.com/search?media=music&limit=1&term={}",
        encode(search_term)
    );
    let http_response = client().get(request_url).send().await.ok()?;
    if !http_response.status().is_success() {
        return None;
    }
    let search_response: ItunesSearchResponse = http_response.json().await.ok()?;
    let primary_genre = search_response.results.into_iter().next()?.genre?;
    match primary_genre.trim().is_empty() {
        true => None,
        false => Some(primary_genre),
    }
}

/// Percent-encode one query value. `reqwest` without its query feature and no
/// `url` crate here leaves this the smallest honest way to send a term.
fn encode(query_value: &str) -> String {
    let mut encoded_value = String::with_capacity(query_value.len());
    for raw_byte in query_value.as_bytes() {
        match raw_byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded_value.push(*raw_byte as char);
            }
            _ => encoded_value.push_str(&format!("%{raw_byte:02X}")),
        }
    }
    encoded_value
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

/// Which preset each family of genres asks for, in the order they are tried.
/// Two families land on `bass` rather than one, because the order is what
/// sends `Alternative Rap` to the drums and not to the guitars.
const FAMILIES: &[(&str, &[&str])] = &[
    (
        "bass",
        &["electronic", "dance", "house", "techno", "bass", "dubstep"],
    ),
    ("loudness", &["rock", "metal", "punk", "alternative"]),
    ("bass", &["hip hop", "rap", "r b", "soul"]),
    ("warm", &["classical", "jazz", "blues", "folk", "acoustic"]),
    ("vocal", &["pop", "soundtrack", "country"]),
];

/// The preset for a genre, or none, which leaves the equaliser where the user
/// put it. Punctuation reads as a space, so `Hip-Hop/Rap` and `R&B/Soul` land.
pub fn preset_for(genre: &str) -> Option<&'static str> {
    let flattened = flatten(genre);
    FAMILIES
        .iter()
        .find(|(_, genre_terms)| {
            genre_terms
                .iter()
                .any(|genre_term| flattened.contains(genre_term))
        })
        .map(|(preset, _)| *preset)
}

fn flatten(genre: &str) -> String {
    genre
        .chars()
        .map(|character| match character.is_ascii_alphanumeric() {
            true => character.to_ascii_lowercase(),
            false => ' ',
        })
        .collect()
}

/// A genre per video id, and whether the file is behind.
#[derive(Default)]
struct KnownGenres {
    genre_by_video_id: HashMap<String, String>,
    /// Ids looked up this run, answer or none. Not written to the file: a
    /// track iTunes does not have is worth one request per run, and re-asking
    /// it on every track change is worth none.
    requested_video_ids: HashSet<String>,
}

pub struct Genres {
    cache_path: PathBuf,
    known_genres: RwLock<KnownGenres>,
}

impl Genres {
    /// Read the file. Cheap enough for the launch path: one small object.
    pub fn new(cache_path: PathBuf) -> Genres {
        if let Some(cache_directory) = cache_path.parent() {
            let _ = crate::private_file::prepare_directory(cache_directory);
        }
        let genre_by_video_id = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Genres {
            cache_path,
            known_genres: RwLock::new(KnownGenres {
                genre_by_video_id,
                requested_video_ids: HashSet::new(),
            }),
        }
    }

    pub fn get(&self, video_id: &str) -> Option<String> {
        self.read().genre_by_video_id.get(video_id).cloned()
    }

    pub fn len(&self) -> usize {
        self.read().genre_by_video_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True when this id has not been asked about yet, marking it asked. The
    /// caller is about to send exactly this one.
    pub fn take_unasked(&self, video_id: &str) -> bool {
        self.write_lock().requested_video_ids.insert(video_id.to_string())
    }

    /// Remember what iTunes answered, and put the file back. One track a time
    /// at most, so the rename costs nothing worth batching.
    pub fn learn(&self, video_id: &str, genre: &str) {
        let mut known_genres = self.write_lock();
        if known_genres
            .genre_by_video_id
            .get(video_id)
            .is_some_and(|cached_genre| cached_genre == genre)
        {
            return;
        }
        known_genres
            .genre_by_video_id
            .insert(video_id.to_string(), genre.to_string());
        write_genres(&self.cache_path, &known_genres.genre_by_video_id);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, KnownGenres> {
        self.known_genres
            .read()
            .expect("lock is never held across a panic")
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, KnownGenres> {
        self.known_genres
            .write()
            .expect("lock is never held across a panic")
    }
}

/// The cache file, beside the page cache under `~/.cache/redwax`.
pub fn path_for(config: &Config) -> PathBuf {
    match config.page_cache_dir.parent() {
        Some(cache_directory) => cache_directory.join("genres.json"),
        None => PathBuf::from("genres.json"),
    }
}

fn write_genres(cache_path: &std::path::Path, genre_by_video_id: &HashMap<String, String>) -> bool {
    let Ok(cache_json) = serde_json::to_string(genre_by_video_id) else {
        return false;
    };
    crate::private_file::write(cache_path, cache_json.as_bytes()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("redwax-genres-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("the temporary directory is writable");
        directory
    }

    #[test]
    fn test_a_real_genre_string_finds_its_preset() {
        let cases = [
            ("Electronic", Some("bass")),
            ("Dance", Some("bass")),
            ("Bass", Some("bass")),
            ("Hip-Hop/Rap", Some("bass")),
            ("R&B/Soul", Some("bass")),
            ("Alternative Rock", Some("loudness")),
            ("Heavy Metal", Some("loudness")),
            ("Punk", Some("loudness")),
            ("Classical", Some("warm")),
            ("Jazz", Some("warm")),
            ("Singer/Songwriter Folk", Some("warm")),
            ("Pop", Some("vocal")),
            ("K-Pop", Some("vocal")),
            ("Soundtrack", Some("vocal")),
            ("Country", Some("vocal")),
            ("Spoken Word", None),
            ("", None),
        ];
        for (genre, want) in cases {
            assert_eq!(preset_for(genre), want, "{genre}");
        }
    }

    #[test]
    fn test_every_preset_a_family_names_exists() {
        for (preset, _) in FAMILIES {
            assert!(
                crate::eq::PRESETS.iter().any(|(name, _)| name == preset),
                "{preset}"
            );
        }
    }

    #[test]
    fn test_a_term_is_encoded_for_the_query_string() {
        assert_eq!(encode("Aphex Twin Xtal"), "Aphex%20Twin%20Xtal");
        assert_eq!(encode("Sigur R\u{f3}s"), "Sigur%20R%C3%B3s");
        assert_eq!(encode("A&B/C?d"), "A%26B%2FC%3Fd");
    }

    #[test]
    fn test_a_learned_genre_survives_the_round_trip() {
        let path = dir("round").join("genres.json");
        let genres = Genres::new(path.clone());
        assert!(genres.is_empty());
        genres.learn("v1", "Electronic");
        assert_eq!(genres.get("v1"), Some("Electronic".to_string()));

        let reread = Genres::new(path);
        assert_eq!(reread.get("v1"), Some("Electronic".to_string()));
        assert_eq!(reread.get("v2"), None);
    }

    #[test]
    fn test_an_id_is_asked_about_once_a_run() {
        let genres = Genres::new(dir("asked").join("genres.json"));
        assert!(genres.take_unasked("v1"));
        assert!(!genres.take_unasked("v1"));
        assert!(genres.take_unasked("v2"));
    }

    #[test]
    fn test_a_corrupt_file_reads_as_nothing_remembered() {
        let path = dir("corrupt").join("genres.json");
        std::fs::write(&path, "{not json").expect("the temporary directory is writable");
        assert!(Genres::new(path).is_empty());
    }

    #[tokio::test]
    #[ignore = "live, reaches the iTunes Search API"]
    async fn test_itunes_answers_with_a_genre() {
        let genre = lookup("Aphex Twin", "Xtal").await.expect("iTunes has this track");
        println!("Aphex Twin Xtal: {genre}");
        assert_eq!(preset_for(&genre), Some("bass"), "{genre}");
    }
}
