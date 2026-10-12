//! Download sync. The diff and the eviction are tested against a temp library,
//! and the download itself only in the live tests, because it costs a real
//! yt-dlp run against the account.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use redwax::sync::{
    EVICTED_DIR, EvictionDecision, SyncOptions, assess, evict, expand, library_index, plan, sync_once,
};
use redwax_marks::Marks;
use redwax_ytm::{Account, Browsable, Continuation, Item, MusicSource, Page, Result, Shelf, Track};

// ------------------------------------------------------------------ fixtures

fn temp_dir(tag: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("redwax-sync-{tag}-{unique}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn track(video_id: &str) -> Track {
    Track {
        video_id: video_id.to_owned(),
        title: format!("Track {video_id}"),
        artists: vec!["Artist".to_owned()],
        album: None,
        album_browse_id: None,
        duration_seconds: Some(200),
        thumbnail_url: None,
        liked: false,
        playlist_entry: None,
    }
}

fn write_track(library: &Path, artist: &str, album: &str, title: &str, video_id: &str) -> PathBuf {
    let dir = library.join(artist).join(album);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{title} [{video_id}].webm"));
    std::fs::write(&path, b"audio").unwrap();
    path
}

fn age(path: &Path, by: Duration) {
    modified_at(path, SystemTime::now() - by);
}

fn modified_at(path: &Path, when: SystemTime) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();
}

/// A `MusicSource` that answers only `browse`, from a table the test owns.
struct FakeSource {
    pages: HashMap<String, Page>,
    continuations: Mutex<HashMap<String, ContinuationReply>>,
    broken: bool,
}

enum ContinuationReply {
    Batch(Vec<Track>, Option<Continuation>),
    Failure(String),
}

impl FakeSource {
    fn with(pages: &[(&str, Vec<Track>)]) -> FakeSource {
        FakeSource {
            pages: pages
                .iter()
                .map(|(key, tracks)| ((*key).to_owned(), page(tracks.clone(), None)))
                .collect(),
            continuations: Mutex::new(HashMap::new()),
            broken: false,
        }
    }

    fn paged(pages: &[(&str, Vec<Vec<Track>>)]) -> FakeSource {
        let mut initial = HashMap::new();
        let mut continuations = HashMap::new();
        for (key, batches) in pages {
            let first_token = (batches.len() > 1).then(|| format!("{key}:1"));
            initial.insert(
                (*key).to_owned(),
                page(
                    batches.first().cloned().unwrap_or_default(),
                    first_token.as_deref(),
                ),
            );
            for index in 1..batches.len() {
                let token = format!("{key}:{index}");
                let next = (index + 1 < batches.len()).then(|| continuation(&format!("{key}:{}", index + 1)));
                continuations.insert(token, ContinuationReply::Batch(batches[index].clone(), next));
            }
        }
        FakeSource {
            pages: initial,
            continuations: Mutex::new(continuations),
            broken: false,
        }
    }

    fn failing_after(key: &str, prefix: Vec<Track>) -> FakeSource {
        FakeSource {
            pages: HashMap::from([(key.to_owned(), page(prefix, Some("fail")))]),
            continuations: Mutex::new(HashMap::from([(
                "fail".to_owned(),
                ContinuationReply::Failure("continuation unavailable".to_owned()),
            )])),
            broken: false,
        }
    }

    fn repeating(key: &str, prefix: Vec<Track>) -> FakeSource {
        FakeSource {
            pages: HashMap::from([(key.to_owned(), page(prefix, Some("repeat")))]),
            continuations: Mutex::new(HashMap::from([(
                "repeat".to_owned(),
                ContinuationReply::Batch(vec![track("Bq56cLjoIgo")], Some(continuation("repeat"))),
            )])),
            broken: false,
        }
    }

    fn broken() -> FakeSource {
        FakeSource {
            pages: HashMap::new(),
            continuations: Mutex::new(HashMap::new()),
            broken: true,
        }
    }
}

fn continuation(token: &str) -> Continuation {
    Continuation {
        browse_id: "VLtest".to_owned(),
        token: token.to_owned(),
    }
}

fn page(tracks: Vec<Track>, more: Option<&str>) -> Page {
    let mut page = Page::new(
        "Fake",
        vec![Shelf::new(
            "Tracks",
            tracks.into_iter().map(Item::Track).collect(),
        )],
    );
    page.more = more.map(continuation);
    page
}

impl MusicSource for FakeSource {
    fn signed_in(&self) -> bool {
        true
    }

    async fn whoami(&self) -> redwax_ytm::Result<Option<String>> {
        Ok(None)
    }

    async fn home(&self) -> Result<Page> {
        Ok(Page::default())
    }

    async fn search(&self, _query: &str) -> Result<Page> {
        Ok(Page::default())
    }

    async fn library(&self) -> Result<Page> {
        Ok(Page::default())
    }

    async fn browse(&self, item: &Browsable) -> Result<Page> {
        if self.broken {
            return Err(redwax_ytm::Error::Network("no route to host".to_owned()));
        }
        Ok(self.pages.get(&item.key()).cloned().unwrap_or_default())
    }

    async fn continue_page(&self, continuation: &Continuation) -> Result<(Vec<Item>, Option<Continuation>)> {
        let reply = self
            .continuations
            .lock()
            .expect("the test does not hold the continuation lock")
            .remove(&continuation.token)
            .unwrap_or_else(|| ContinuationReply::Failure("unexpected continuation".to_owned()));
        match reply {
            ContinuationReply::Batch(tracks, next) => {
                Ok((tracks.into_iter().map(Item::Track).collect(), next))
            }
            ContinuationReply::Failure(error) => Err(redwax_ytm::Error::Network(error)),
        }
    }

    async fn radio(&self, _track: &Track) -> Result<Vec<Track>> {
        Ok(Vec::new())
    }

    async fn lyrics(&self, _track: &Track) -> Result<Option<String>> {
        Ok(None)
    }

    async fn timed_lyrics(&self, _track: &Track) -> Result<Option<redwax_ytm::Lyrics>> {
        Ok(None)
    }

    async fn set_liked(&self, _track: &Track, _liked: bool) -> Result<()> {
        Ok(())
    }

    async fn accounts(&self) -> Result<Vec<Account>> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------- unit tests

#[test]
fn library_index_reads_the_id_out_of_the_file_name() {
    let library = temp_dir("index");
    let kept = write_track(&library, "Artist", "Album", "Song [live]", "1-Fxb9jsOuI");
    write_track(&library, "Other", "Record", "Second", "ARsqVw10lTw");
    std::fs::write(library.join("Artist/Album/notes.txt"), b"x").unwrap();
    // yt-dlp leaves the cover beside the track under the same name.
    std::fs::write(
        library.join("Artist/Album/Song [live] [1-Fxb9jsOuI].webp"),
        b"art",
    )
    .unwrap();
    write_track(&library, EVICTED_DIR, "Album", "Gone", "EPMql3dljqU");

    let index = library_index(&library);

    assert_eq!(index.len(), 2);
    assert_eq!(index.get("1-Fxb9jsOuI"), Some(&kept));
    assert!(index.contains_key("ARsqVw10lTw"));
    // A file already moved aside is not a file the library holds.
    assert!(!index.contains_key("EPMql3dljqU"));
    std::fs::remove_dir_all(&library).unwrap();
}

#[test]
fn plan_downloads_the_missing_and_lists_the_unmarked() {
    let library = temp_dir("plan");
    let present = write_track(&library, "Artist", "Album", "Have", "1-Fxb9jsOuI");
    let stale = write_track(&library, "Artist", "Album", "Had", "EPMql3dljqU");
    let marks = Marks {
        tracks: vec!["1-Fxb9jsOuI".to_owned(), "ARsqVw10lTw".to_owned()],
        ..Marks::default()
    };

    let plan = plan(&marks, &library_index(&library));

    assert_eq!(plan.marked, 2);
    assert_eq!(plan.present, 1);
    assert_eq!(plan.download, vec!["ARsqVw10lTw".to_owned()]);
    assert_eq!(plan.evict, vec![stale]);
    assert!(present.exists());
    std::fs::remove_dir_all(&library).unwrap();
}

#[test]
fn eviction_waits_out_the_grace_period_and_keeps_the_shape() {
    let library = temp_dir("evict");
    let old = write_track(&library, "Artist", "Album", "Old", "EPMql3dljqU");
    let fresh = write_track(&library, "Artist", "Album", "Fresh", "Bq56cLjoIgo");
    age(&old, Duration::from_secs(20 * 24 * 3600));
    let grace = Duration::from_secs(14 * 24 * 3600);

    let moved = evict(&library, &[old.clone(), fresh.clone()], grace, SystemTime::now());

    assert_eq!(
        moved,
        vec![
            library
                .join(EVICTED_DIR)
                .join("Artist/Album/Old [EPMql3dljqU].webm")
        ]
    );
    assert!(!old.exists());
    assert!(fresh.exists());
    // Nothing is deleted, so the file is still readable where it landed.
    assert_eq!(std::fs::read(&moved[0]).unwrap(), b"audio");
    std::fs::remove_dir_all(&library).unwrap();
}

#[tokio::test]
async fn expand_turns_a_marked_playlist_into_its_tracks() {
    let source = FakeSource::with(&[(
        "playlist:PLtestfixture600000000000000000000",
        vec![track("ARsqVw10lTw"), track("EPMql3dljqU")],
    )]);
    let marks = Marks {
        tracks: vec!["1-Fxb9jsOuI".to_owned()],
        playlists: vec!["PLtestfixture600000000000000000000".to_owned()],
        ..Marks::default()
    };

    let (expanded, failed) = expand(&marks, &source).await;

    assert!(failed.is_empty());
    assert_eq!(
        expanded.tracks,
        vec![
            "1-Fxb9jsOuI".to_owned(),
            "ARsqVw10lTw".to_owned(),
            "EPMql3dljqU".to_owned()
        ]
    );
}

#[tokio::test]
async fn expand_reports_a_page_that_failed_rather_than_shrinking_the_marks() {
    let marks = Marks {
        tracks: vec!["1-Fxb9jsOuI".to_owned()],
        playlists: vec!["PLtestfixture600000000000000000000".to_owned()],
        albums: vec!["MPREb_example".to_owned()],
        ..Marks::default()
    };

    let (expanded, failed) = expand(&marks, &FakeSource::broken()).await;

    assert_eq!(expanded.tracks, vec!["1-Fxb9jsOuI".to_owned()]);
    assert_eq!(failed.len(), 2);
    assert_eq!(failed[0].0, "PLtestfixture600000000000000000000");
    assert_eq!(failed[1].0, "MPREb_example");
}

#[tokio::test]
async fn assessment_collects_every_playlist_and_album_batch() {
    let playlist = "PLtestfixture600000000000000000000";
    let album = "MPREb_example";
    let source = FakeSource::paged(&[
        (
            "playlist:PLtestfixture600000000000000000000",
            vec![vec![track("ARsqVw10lTw")], vec![track("EPMql3dljqU")]],
        ),
        (
            "album:MPREb_example",
            vec![vec![track("Bq56cLjoIgo")], vec![track("O9g7uwHAP3s")]],
        ),
    ]);
    let marks = Marks {
        tracks: vec!["1-Fxb9jsOuI".to_owned()],
        playlists: vec![playlist.to_owned()],
        albums: vec![album.to_owned()],
        ..Marks::default()
    };

    let assessment = assess(&marks, &source, &HashMap::new()).await;

    assert_eq!(assessment.eviction(), EvictionDecision::Allowed);
    assert!(assessment.failures().is_empty());
    assert_eq!(assessment.plan().marked, 5);
    assert_eq!(assessment.plan().download.len(), 5);
}

#[tokio::test]
async fn continuation_failure_keeps_safe_downloads_and_suppresses_eviction() {
    let playlist = "PLtestfixture600000000000000000000";
    let source = FakeSource::failing_after(
        "playlist:PLtestfixture600000000000000000000",
        vec![track("ARsqVw10lTw")],
    );
    let marks = Marks {
        tracks: vec!["1-Fxb9jsOuI".to_owned()],
        playlists: vec![playlist.to_owned()],
        ..Marks::default()
    };
    let library = temp_dir("incomplete-prefix");
    let stale = write_track(&library, "Artist", "Album", "Stale", "EPMql3dljqU");
    let options = SyncOptions {
        library_dir: library.clone(),
        profile: None,
        grace: Duration::ZERO,
    };

    let assessment = assess(&marks, &source, &library_index(&library)).await;

    assert_eq!(assessment.eviction(), EvictionDecision::IncompleteMarks);
    assert_eq!(assessment.eviction_candidates(), 1);
    assert!(assessment.plan().evict.is_empty());
    assert_eq!(assessment.plan().download.len(), 2);
    assert_eq!(assessment.failures().len(), 1);

    let downloaded: Vec<std::result::Result<PathBuf, (String, String)>> = assessment
        .plan()
        .download
        .iter()
        .map(|id| Ok(library.join(format!("downloaded-{id}"))))
        .collect();
    let report = assessment.finish(&options, downloaded, SystemTime::now());

    assert_eq!(report.downloaded.len(), 2);
    assert_eq!(report.eviction, EvictionDecision::IncompleteMarks);
    assert!(report.evicted.is_empty());
    assert!(stale.exists());
    std::fs::remove_dir_all(&library).unwrap();
}

#[tokio::test]
async fn an_initial_expansion_failure_suppresses_every_eviction_candidate() {
    let marks = Marks {
        tracks: vec!["1-Fxb9jsOuI".to_owned()],
        playlists: vec!["PLtestfixture600000000000000000000".to_owned()],
        ..Marks::default()
    };
    let library = temp_dir("initial-failure");
    let stale = write_track(&library, "Artist", "Album", "Stale", "EPMql3dljqU");

    let assessment = assess(&marks, &FakeSource::broken(), &library_index(&library)).await;

    assert_eq!(assessment.eviction(), EvictionDecision::IncompleteMarks);
    assert_eq!(assessment.eviction_candidates(), 1);
    assert!(assessment.plan().evict.is_empty());
    assert!(stale.exists());
    std::fs::remove_dir_all(&library).unwrap();
}

#[tokio::test]
async fn empty_marks_suppress_every_eviction_candidate() {
    let library = temp_dir("empty-marks");
    let stale = write_track(&library, "Artist", "Album", "Stale", "EPMql3dljqU");

    let assessment = assess(
        &Marks::default(),
        &FakeSource::with(&[]),
        &library_index(&library),
    )
    .await;

    assert_eq!(assessment.eviction(), EvictionDecision::EmptyMarks);
    assert_eq!(assessment.eviction_candidates(), 1);
    assert!(assessment.plan().evict.is_empty());
    assert!(stale.exists());
    std::fs::remove_dir_all(&library).unwrap();
}

#[tokio::test]
async fn a_repeated_cursor_suppresses_eviction_without_discarding_the_prefix() {
    let playlist = "PLtestfixture600000000000000000000";
    let source = FakeSource::repeating(
        "playlist:PLtestfixture600000000000000000000",
        vec![track("ARsqVw10lTw")],
    );
    let marks = Marks {
        playlists: vec![playlist.to_owned()],
        ..Marks::default()
    };
    let library = temp_dir("repeated-cursor");
    let stale = write_track(&library, "Artist", "Album", "Stale", "EPMql3dljqU");

    let assessment = assess(&marks, &source, &library_index(&library)).await;

    assert_eq!(assessment.eviction(), EvictionDecision::IncompleteMarks);
    assert_eq!(assessment.plan().marked, 1);
    assert_eq!(assessment.plan().download.len(), 1);
    assert!(assessment.plan().evict.is_empty());
    assert!(assessment.failures()[0].1.contains("repeat"));
    assert!(stale.exists());
    std::fs::remove_dir_all(&library).unwrap();
}

#[tokio::test]
async fn a_failed_or_missing_download_prevents_an_eligible_file_from_moving() {
    let marks = Marks {
        tracks: vec!["1-Fxb9jsOuI".to_owned()],
        ..Marks::default()
    };
    let library = temp_dir("download-failure");
    let stale = write_track(&library, "Artist", "Album", "Stale", "EPMql3dljqU");
    let options = SyncOptions {
        library_dir: library.clone(),
        profile: None,
        grace: Duration::ZERO,
    };
    for outcomes in [
        vec![Err(("1-Fxb9jsOuI".to_owned(), "download unavailable".to_owned()))],
        Vec::new(),
    ] {
        let assessment = assess(&marks, &FakeSource::with(&[]), &library_index(&library)).await;
        assert_eq!(assessment.eviction(), EvictionDecision::Allowed);
        assert_eq!(assessment.plan().evict, vec![stale.clone()]);
        let report = assessment.finish(&options, outcomes, SystemTime::now());

        assert_eq!(report.eviction, EvictionDecision::DownloadFailed);
        assert_eq!(report.failed.len(), 1);
        assert!(report.evicted.is_empty());
        assert_eq!(std::fs::read(&stale).unwrap(), b"audio");
    }
    std::fs::remove_dir_all(&library).unwrap();
}

#[tokio::test]
async fn an_allowed_assessment_moves_only_files_eligible_at_the_supplied_time() {
    let fixed_now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let grace = Duration::from_secs(14 * 24 * 3600);
    let library = temp_dir("fixed-clock");
    let wanted = write_track(&library, "Artist", "Album", "Wanted", "1-Fxb9jsOuI");
    let stale = write_track(&library, "Artist", "Album", "Stale", "EPMql3dljqU");
    modified_at(&stale, fixed_now - Duration::from_secs(20 * 24 * 3600));
    let marks = Marks {
        tracks: vec!["1-Fxb9jsOuI".to_owned()],
        ..Marks::default()
    };
    let options = SyncOptions {
        library_dir: library.clone(),
        profile: None,
        grace,
    };
    let assessment = assess(&marks, &FakeSource::with(&[]), &library_index(&library)).await;

    let report = assessment.finish(&options, Vec::new(), fixed_now);

    assert_eq!(report.eviction, EvictionDecision::Allowed);
    assert_eq!(report.evicted.len(), 1);
    assert!(!stale.exists());
    assert!(wanted.exists());
    assert_eq!(std::fs::read(&report.evicted[0]).unwrap(), b"audio");
    std::fs::remove_dir_all(&library).unwrap();
}

// ---------------------------------------------------------------- live tests

fn live_resolver(library: &Path) -> redwax_resolve::Resolver {
    let config = redwax::config::Config::default();
    let session =
        redwax_ytm::Session::read(&config.auth_path).expect("live sync needs valid session metadata");
    let mut options = redwax::services::resolver_options(&config, session.as_ref());
    options.library_dir = Some(library.to_path_buf());
    redwax_resolve::Resolver::new(options)
}

/// What the file actually holds, as `<codec> <sample rate>`. R11 asks for the
/// opus tiers, and PipeWire runs at 48 kHz, so a resample would be a fault.
fn probe(path: &Path) -> String {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,sample_rate",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .expect("ffprobe on PATH");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
#[ignore = "reads the live Firefox profile"]
fn live_marks_read_from_the_real_profile() {
    let profile =
        redwax_marks::FirefoxProfile::find_for(None, redwax_marks::ProfileCapability::Marks).unwrap();
    let marks = redwax_marks::read_marks(&profile).unwrap();
    println!(
        "marks: {} tracks, {} playlists, {} albums, {} stored locally",
        marks.tracks.len(),
        marks.playlists.len(),
        marks.albums.len(),
        marks.stored_locally.len()
    );
    assert!(marks.tracks.len() > 100);
    assert!(!marks.playlists.is_empty());
    assert!(marks.stored_locally.len() > 100);
}

/// One real download. Every marked track but one is stood in for by a
/// placeholder, so the sync has exactly one file to fetch.
#[tokio::test]
#[ignore = "downloads a track through yt-dlp"]
async fn live_sync_downloads_one_marked_track() {
    let library = temp_dir("live");
    let profile =
        redwax_marks::FirefoxProfile::find_for(None, redwax_marks::ProfileCapability::Marks).unwrap();
    let marks = redwax_marks::read_marks(&profile).unwrap();
    // A track known to carry the 774 tier, so the run proves the tier as well
    // as the plumbing. Any marked track will do when it is not marked.
    let high_tier = "1-Fxb9jsOuI".to_owned();
    let wanted = if marks.tracks.contains(&high_tier) {
        high_tier
    } else {
        marks.tracks.first().expect("a marked track").clone()
    };
    for video_id in marks.tracks.iter().filter(|id| **id != wanted) {
        write_track(&library, "Stand in", "Stand in", "Placeholder", video_id);
    }

    let options = SyncOptions {
        library_dir: library.clone(),
        profile: None,
        grace: Duration::from_secs(14 * 24 * 3600),
    };
    let source = FakeSource::with(&[]);
    let report = sync_once(&options, &live_resolver(&library), &source)
        .await
        .unwrap();

    println!(
        "sync: {} marked, {} present, {} downloaded, {} evicted, {:?} failed",
        report.marked,
        report.present,
        report.downloaded.len(),
        report.evicted.len(),
        report.failed
    );
    assert_eq!(report.downloaded.len(), 1, "failures: {:?}", report.failed);
    let written = &report.downloaded[0];
    assert!(written.exists(), "{}", written.display());
    let name = written.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.contains(&format!("[{wanted}]")), "{name}");
    assert!(std::fs::metadata(written).unwrap().len() > 100_000);
    println!("codec {}", probe(written));
    assert_eq!(probe(written), "opus 48000");
    println!("downloaded {}", written.display());
    assert!(report.evicted.is_empty());
    std::fs::remove_dir_all(&library).unwrap();
}
