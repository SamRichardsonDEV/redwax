//! Download sync: the browser's marks in, the local library brought level.
//!
//! Runs after startup and every 15 minutes. Nothing here
//! deletes: a track that loses its mark is moved to `.evicted/` under the
//! library, so a mark pressed again costs a move and not a re-download.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use redwax_marks::{FirefoxProfile, Marks, read_marks};
use redwax_resolve::Resolver;
use redwax_ytm::{Album, Browsable, MusicSource, Playlist, collect_page};

/// How long an unmarked file stays in the library before it moves aside. Long
/// enough that a playlist edited on the phone and undone the same week costs
/// nothing.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(14 * 24 * 3600);

/// Downloads run behind playback, so two at a time leaves the resolver's own
/// three permits room for the tracks the user is waiting on.
const PARALLEL_DOWNLOADS: usize = 2;

/// Where unmarked files go. A leading dot keeps it out of the library index.
pub const EVICTED_DIR: &str = ".evicted";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOptions {
    pub library_dir: PathBuf,
    pub profile: Option<String>,
    pub grace: Duration,
}

impl Default for SyncOptions {
    fn default() -> SyncOptions {
        SyncOptions {
            library_dir: crate::config::Config::default().library_dir,
            profile: None,
            grace: DEFAULT_GRACE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncReport {
    /// Distinct tracks marked, after playlists and albums are expanded.
    pub marked: usize,
    /// Marked tracks already in the library when the sync started.
    pub present: usize,
    pub downloaded: Vec<PathBuf>,
    pub evicted: Vec<PathBuf>,
    /// What failed, as an id and a reason. Independent downloads still run,
    /// but any failure suppresses eviction for the pass.
    pub failed: Vec<(String, String)>,
    /// Why eviction was allowed or suppressed for this pass.
    pub eviction: EvictionDecision,
    /// Unmarked files considered before a conservative decision suppressed them.
    pub eviction_candidates: usize,
}

/// The difference between the marks and the library, before anything moves.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncPlan {
    pub marked: usize,
    pub present: usize,
    /// Marked ids with no file, in a stable order.
    pub download: Vec<String>,
    /// Files whose id lost its mark. The grace period is applied later,
    /// because `plan` has no clock.
    pub evict: Vec<PathBuf>,
}

/// Whether this pass carries enough evidence to move unmarked files aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvictionDecision {
    Allowed,
    IncompleteMarks,
    #[default]
    EmptyMarks,
    DownloadFailed,
}

impl EvictionDecision {
    pub fn reason(self) -> Option<&'static str> {
        match self {
            EvictionDecision::Allowed => None,
            EvictionDecision::IncompleteMarks => Some("marked playlists or albums were incomplete"),
            EvictionDecision::EmptyMarks => Some("no marked tracks were found"),
            EvictionDecision::DownloadFailed => Some("a required download failed"),
        }
    }

    fn allows_eviction(self) -> bool {
        self == EvictionDecision::Allowed
    }
}

impl std::fmt::Display for EvictionDecision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.reason().unwrap_or("allowed"))
    }
}

/// One pass-wide decision shared by dry run and execution.
#[derive(Debug)]
pub struct SyncAssessment {
    plan: SyncPlan,
    failures: Vec<(String, String)>,
    eviction: EvictionDecision,
    eviction_candidates: usize,
}

impl SyncAssessment {
    /// The authorized plan. Its eviction list is empty whenever eviction is
    /// suppressed, so a caller cannot accidentally execute the raw candidates.
    pub fn plan(&self) -> &SyncPlan {
        &self.plan
    }

    pub fn failures(&self) -> &[(String, String)] {
        &self.failures
    }

    pub fn eviction(&self) -> EvictionDecision {
        self.eviction
    }

    /// Candidates before the pass-wide safety decision was applied.
    pub fn eviction_candidates(&self) -> usize {
        self.eviction_candidates
    }

    /// The runtime supplies one outcome per authorized download.
    /// A missing outcome cannot stand for a successful download.
    pub fn finish(
        mut self,
        sync_options: &SyncOptions,
        download_outcomes: Vec<Result<PathBuf, (String, String)>>,
        current_time: SystemTime,
    ) -> SyncReport {
        let expected_download_count = self.plan.download.len();
        let received_download_count = download_outcomes.len();
        let mut downloaded_paths = Vec::new();
        let mut has_failed_download = received_download_count != expected_download_count;
        if has_failed_download {
            self.failures.push((
                "downloads".to_owned(),
                format!("received {received_download_count} of {expected_download_count} download outcomes"),
            ));
        }
        for download_outcome in download_outcomes {
            match download_outcome {
                Ok(downloaded_path) => downloaded_paths.push(downloaded_path),
                Err(failure) => {
                    has_failed_download = true;
                    self.failures.push(failure);
                }
            }
        }
        downloaded_paths.sort();
        if has_failed_download && self.eviction.allows_eviction() {
            self.eviction = EvictionDecision::DownloadFailed;
        }
        let evicted_paths = if self.eviction.allows_eviction() {
            evict(
                &sync_options.library_dir,
                &self.plan.evict,
                sync_options.grace,
                current_time,
            )
        } else {
            Vec::new()
        };
        SyncReport {
            marked: self.plan.marked,
            present: self.plan.present,
            downloaded: downloaded_paths,
            evicted: evicted_paths,
            failed: self.failures,
            eviction: self.eviction,
            eviction_candidates: self.eviction_candidates,
        }
    }
}

/// Marked ids against the files on disk. `marks.tracks` is the expanded set:
/// playlists and albums are resolved to their tracks before this is called.
pub fn plan(library_marks: &Marks, library_files: &HashMap<String, PathBuf>) -> SyncPlan {
    let marked_video_ids: BTreeSet<&String> = library_marks.tracks.iter().collect();
    let missing_video_ids: Vec<String> = marked_video_ids
        .iter()
        .filter(|video_id| !library_files.contains_key(**video_id))
        .map(|video_id| (*video_id).clone())
        .collect();
    let mut unmarked_paths: Vec<PathBuf> = library_files
        .iter()
        .filter(|(video_id, _)| !marked_video_ids.contains(video_id))
        .map(|(_, source_path)| source_path.clone())
        .collect();
    unmarked_paths.sort();
    SyncPlan {
        marked: marked_video_ids.len(),
        present: marked_video_ids.len() - missing_video_ids.len(),
        download: missing_video_ids,
        evict: unmarked_paths,
    }
}

/// Video id to file for every track under `library_directory`, which is what the resolver's
/// step 1 searches. `.evicted` and every other dotted directory are skipped.
pub fn library_index(library_directory: &Path) -> HashMap<String, PathBuf> {
    redwax_resolve::LibraryIndex::scan(library_directory)
        .entries()
        .map(|(video_id, source_path)| (video_id.to_owned(), source_path.to_path_buf()))
        .collect()
}

/// Every marked track id, with the marked playlists and albums expanded.
///
/// A page that fails to load joins the failures rather than shrinking the
/// mark set, because a shrunk set reads as an instruction to evict.
pub async fn expand(marks: &Marks, music_source: &impl MusicSource) -> (Marks, Vec<(String, String)>) {
    let mut marked_video_ids: BTreeSet<String> = marks.tracks.iter().cloned().collect();
    let mut expansion_failures = Vec::new();
    for playlist_id in &marks.playlists {
        let marked_collection = Browsable::Playlist(Playlist {
            playlist_id: playlist_id.clone(),
            title: String::new(),
            subtitle: None,
            thumbnail_url: None,
        });
        gather(
            music_source,
            &marked_collection,
            playlist_id,
            &mut marked_video_ids,
            &mut expansion_failures,
        )
        .await;
    }
    for browse_id in &marks.albums {
        let marked_collection = Browsable::Album(Album {
            browse_id: browse_id.clone(),
            title: String::new(),
            artists: Vec::new(),
            year: None,
            thumbnail_url: None,
        });
        gather(
            music_source,
            &marked_collection,
            browse_id,
            &mut marked_video_ids,
            &mut expansion_failures,
        )
        .await;
    }
    let expanded_marks = Marks {
        tracks: marked_video_ids.into_iter().collect(),
        ..marks.clone()
    };
    (expanded_marks, expansion_failures)
}

async fn gather(
    music_source: &impl MusicSource,
    marked_collection: &Browsable,
    collection_id: &str,
    marked_video_ids: &mut BTreeSet<String>,
    expansion_failures: &mut Vec<(String, String)>,
) {
    match music_source.browse(marked_collection).await {
        Ok(page) => match collect_page(music_source, page).await {
            Ok(page) => marked_video_ids.extend(page.as_page().tracks().map(|track| track.video_id.clone())),
            Err(incomplete) => {
                marked_video_ids.extend(incomplete.page.tracks().map(|track| track.video_id.clone()));
                expansion_failures.push((collection_id.to_owned(), incomplete.to_string()));
            }
        },
        Err(error) => expansion_failures.push((collection_id.to_owned(), error.to_string())),
    }
}

/// Expand marks, compare them with the library, and authorize only safe work.
pub async fn assess(
    marks: &Marks,
    music_source: &impl MusicSource,
    library_files: &HashMap<String, PathBuf>,
) -> SyncAssessment {
    let (expanded_marks, expansion_failures) = expand(marks, music_source).await;
    let mut sync_plan = plan(&expanded_marks, library_files);
    let eviction_candidates = sync_plan.evict.len();
    let eviction_decision = if !expansion_failures.is_empty() {
        EvictionDecision::IncompleteMarks
    } else if expanded_marks.tracks.is_empty() {
        EvictionDecision::EmptyMarks
    } else {
        EvictionDecision::Allowed
    };
    if !eviction_decision.allows_eviction() {
        sync_plan.evict.clear();
    }
    SyncAssessment {
        plan: sync_plan,
        failures: expansion_failures,
        eviction: eviction_decision,
        eviction_candidates,
    }
}

/// Preserve relative paths when moving eviction candidates into `.evicted`.
/// The grace period allows recent download marks to settle.
pub fn evict(
    library_dir: &Path,
    eviction_candidates: &[PathBuf],
    grace_period: Duration,
    current_time: SystemTime,
) -> Vec<PathBuf> {
    let mut evicted_paths = Vec::new();
    for source_path in eviction_candidates {
        let Ok(last_modified) =
            std::fs::metadata(source_path).and_then(|file_metadata| file_metadata.modified())
        else {
            continue;
        };
        if current_time.duration_since(last_modified).unwrap_or_default() < grace_period {
            continue;
        }
        let relative_path = source_path
            .strip_prefix(library_dir)
            .unwrap_or(source_path.as_path());
        let evicted_path = library_dir.join(EVICTED_DIR).join(relative_path);
        if let Some(evicted_parent) = evicted_path.parent()
            && std::fs::create_dir_all(evicted_parent).is_err()
        {
            continue;
        }
        if std::fs::rename(source_path, &evicted_path).is_ok() {
            evicted_paths.push(evicted_path);
        }
    }
    evicted_paths
}

/// One track into `library_dir`, judged by the file rather than by the exit code.
///
/// yt-dlp fails the whole run when a postprocessor fails, and thumbnail
/// embedding is unsupported for the webm the opus tiers remux to. The audio is
/// written and remuxed before that step, so a file with the id in its name is a
/// download that worked.
async fn download_one(
    resolver: &Resolver,
    video_id: &str,
    library_dir: &Path,
) -> Result<PathBuf, (String, String)> {
    match resolver.download(video_id, library_dir).await {
        Ok(path) => Ok(path),
        Err(error) => match library_index(library_dir).remove(video_id) {
            Some(path) => Ok(path),
            None => Err((video_id.to_owned(), error.to_string())),
        },
    }
}

/// One pass: read the marks, download what is missing, move aside what is not
/// marked any more.
pub async fn sync_once(
    sync_options: &SyncOptions,
    resolver: &Resolver,
    music_source: &impl MusicSource,
) -> Result<SyncReport, String> {
    // The profile lookup and the SQLite reads are file work, kept off the
    // async worker so the UI loop never waits on the disk.
    let profile_name = sync_options.profile.clone();
    let library_marks = tokio::task::spawn_blocking(move || {
        let firefox_profile =
            FirefoxProfile::find_for(profile_name.as_deref(), redwax_marks::ProfileCapability::Marks)
                .map_err(|error| error.to_string())?;
        read_marks(&firefox_profile).map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| "the marks read did not finish".to_string())??;
    let library_directory = sync_options.library_dir.clone();
    let library_files = tokio::task::spawn_blocking(move || library_index(&library_directory))
        .await
        .map_err(|_| "the library scan did not finish".to_string())?;
    let sync_assessment = assess(&library_marks, music_source, &library_files).await;

    // Owned ids, not borrowed ones: a closure taking a reference here needs a
    // higher ranked bound the compiler will not infer inside a spawned task.
    let download_outcomes: Vec<Result<PathBuf, (String, String)>> =
        futures::stream::iter(sync_assessment.plan().download.clone())
            .map(|video_id| async move { download_one(resolver, &video_id, &sync_options.library_dir).await })
            .buffer_unordered(PARALLEL_DOWNLOADS)
            .collect()
            .await;
    Ok(sync_assessment.finish(sync_options, download_outcomes, SystemTime::now()))
}
