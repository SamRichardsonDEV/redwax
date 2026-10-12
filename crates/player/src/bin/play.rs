//! `redwax-play [--ao null] [--prefetch] [--skip-after SECS] <video_id>...`
//!
//! Plays the ids in order and prints one timestamped line per event, the
//! milliseconds since process start first: `start`, `resolved <id> <ms since
//! it was asked for>`, `loaded <id>`, `audible <id> <ms since the loadfile or
//! the playlist-next that led to it>`, `next`, `eof`. With `--prefetch` the
//! ids after the first are resolved while the first plays and swapped into
//! mpv's playlist, so the `audible` line after `next` measures mpv's floor.
//! Streams come from yt-dlp `-g`, signed in when the cookies file is there.
//! `REDWAX_YTDLP` names another yt-dlp and `REDWAX_YTDLP_ARGS` adds arguments
//! to it, for the day YouTube wants a token the plain command cannot make.
//! The line format is read by `scripts/measure.sh` and is part of the contract.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use redwax_player::{Playable, Player, PlayerOptions, StreamSource, bare_track, runtime_dir};
use tokio::sync::OnceCell;
use tokio::task::JoinSet;

const USAGE: &str = "usage: redwax-play [--ao null] [--prefetch] [--skip-after SECS] <video_id>...";
const RESOLVE_BUDGET: Duration = Duration::from_secs(30);
const RUN_BUDGET: Duration = Duration::from_secs(60);
const FORMATS: &str = "774/141/251/140";

static START: OnceLock<Instant> = OnceLock::new();

fn log(text: &str) {
    let process_started_at = START.get().copied().unwrap_or_else(Instant::now);
    println!("{} {}", process_started_at.elapsed().as_millis(), text);
}

#[derive(Default)]
struct ParsedArguments {
    use_null_audio_output: bool,
    prefetch: bool,
    skip_after_seconds: Option<f64>,
    video_ids: Vec<String>,
}

fn parse(arguments: impl Iterator<Item = String>) -> Result<ParsedArguments, String> {
    let mut parsed_arguments = ParsedArguments::default();
    let mut arguments = arguments;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--ao" => {
                let audio_output = arguments.next().ok_or("--ao needs a value")?;
                if audio_output != "null" {
                    return Err(format!("only --ao null is supported, not {audio_output}"));
                }
                parsed_arguments.use_null_audio_output = true;
            }
            "--prefetch" => parsed_arguments.prefetch = true,
            "--skip-after" => {
                let seconds_text = arguments.next().ok_or("--skip-after needs a value in seconds")?;
                let skip_after_seconds = seconds_text
                    .parse()
                    .map_err(|_| format!("not a number: {seconds_text}"))?;
                parsed_arguments.skip_after_seconds = Some(skip_after_seconds);
            }
            "-h" | "--help" => return Err(USAGE.to_string()),
            option if option.starts_with('-') => return Err(format!("unknown option {option}")),
            video_id => parsed_arguments.video_ids.push(video_id.to_string()),
        }
    }
    if parsed_arguments.video_ids.is_empty() {
        return Err(USAGE.to_string());
    }
    Ok(parsed_arguments)
}

/// One yt-dlp call per id, deduplicated and remembered.
struct YtDlp {
    cookies_path: Option<PathBuf>,
    resolution_slots: Mutex<HashMap<String, Arc<ResolutionSlot>>>,
    resolution_tasks: Mutex<JoinSet<()>>,
}

struct ResolutionSlot {
    requested_at: Instant,
    resolved_stream: OnceCell<Option<Playable>>,
}

#[derive(Clone)]
struct Streams(Arc<YtDlp>);

impl YtDlp {
    fn new() -> YtDlp {
        let cookies_path = std::env::var_os("HOME")
            .map(|home_directory| PathBuf::from(home_directory).join(".config/redwax/cookies.txt"))
            .filter(|path| path.exists());
        YtDlp {
            cookies_path,
            resolution_slots: Mutex::new(HashMap::new()),
            resolution_tasks: Mutex::new(JoinSet::new()),
        }
    }

    /// What mpv's own hook needs to reach the same formats for an entry that
    /// was never resolved here.
    fn raw_options(&self) -> String {
        let mut raw_options = Vec::new();
        if let Some(cookies_path) = &self.cookies_path {
            raw_options.push(format!("cookies={}", cookies_path.display()));
        }
        raw_options.push("js-runtimes=node".to_string());
        raw_options.push("remote-components=ejs:github".to_string());
        raw_options.push("extractor-args=youtube:player_client=web_music".to_string());
        raw_options.join(",")
    }

    fn slot(&self, video_id: &str) -> Arc<ResolutionSlot> {
        let mut resolution_slots = self.resolution_slots.lock().expect("stream slots");
        Arc::clone(resolution_slots.entry(video_id.to_string()).or_insert_with(|| {
            Arc::new(ResolutionSlot {
                requested_at: Instant::now(),
                resolved_stream: OnceCell::new(),
            })
        }))
    }

    async fn fetch(&self, video_id: &str) -> Option<Playable> {
        let resolution_slot = self.slot(video_id);
        let requested_at = resolution_slot.requested_at;
        resolution_slot
            .resolved_stream
            .get_or_init(|| async {
                let playable = self.run(video_id).await;
                if playable.is_some() {
                    log(&format!(
                        "resolved {video_id} {}",
                        requested_at.elapsed().as_millis()
                    ));
                }
                playable
            })
            .await
            .clone()
    }

    async fn run(&self, video_id: &str) -> Option<Playable> {
        let mut command_arguments: Vec<OsString> = ["-g", "-f", FORMATS, "--no-warnings", "--no-playlist"]
            .iter()
            .map(OsString::from)
            .collect();
        if let Some(cookies_path) = &self.cookies_path {
            command_arguments.push("--cookies".into());
            command_arguments.push(cookies_path.clone().into_os_string());
        }
        for command_argument in [
            "--js-runtimes",
            "node",
            "--remote-components",
            "ejs:github",
            "--extractor-args",
            "youtube:player_client=web_music",
        ] {
            command_arguments.push(command_argument.into());
        }
        if let Some(extra_arguments) = std::env::var_os("REDWAX_YTDLP_ARGS") {
            command_arguments.extend(
                extra_arguments
                    .to_string_lossy()
                    .split_whitespace()
                    .map(OsString::from),
            );
        }
        command_arguments.push(format!("https://music.youtube.com/watch?v={video_id}").into());

        let yt_dlp_binary = std::env::var_os("REDWAX_YTDLP").unwrap_or_else(|| OsString::from("yt-dlp"));
        let yt_dlp_process = tokio::process::Command::new(yt_dlp_binary)
            .args(&command_arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let process_output = tokio::time::timeout(RESOLVE_BUDGET, yt_dlp_process)
            .await
            .ok()?
            .ok()?;
        if !process_output.status.success() {
            return None;
        }
        let standard_output = String::from_utf8_lossy(&process_output.stdout);
        let stream_target = standard_output.lines().next()?.trim();
        if stream_target.is_empty() {
            return None;
        }
        Some(Playable {
            target: stream_target.to_string(),
            tier_label: tier_of(stream_target).to_string(),
            warning: None,
        })
    }
}

/// The itag Google served, read back from the URL it signed.
fn tier_of(target: &str) -> &'static str {
    let audio_format_id = url::Url::parse(target).ok().and_then(|stream_url| {
        stream_url
            .query_pairs()
            .find(|(key, _)| key == "itag")
            .map(|(_, format_id)| format_id.into_owned())
    });
    match audio_format_id.as_deref() {
        Some("774") => "774 opus 283k",
        Some("141") => "141 aac 256k",
        Some("251") => "251 opus 160k",
        Some("140") => "140 aac 128k",
        _ => "unknown tier",
    }
}

impl StreamSource for Streams {
    fn cached(&self, video_id: &str) -> Option<Playable> {
        let resolution_slot = self
            .0
            .resolution_slots
            .lock()
            .expect("stream slots")
            .get(video_id)
            .cloned()?;
        resolution_slot.resolved_stream.get().cloned().flatten()
    }

    fn resolve(&self, video_id: &str) -> impl Future<Output = Result<Option<Playable>, String>> + Send {
        let source = Arc::clone(&self.0);
        let video_id = video_id.to_string();
        async move { Ok(source.fetch(&video_id).await) }
    }

    fn prefetch(&self, video_ids: Vec<String>) {
        let mut resolution_tasks = self.0.resolution_tasks.lock().expect("stream tasks");
        while resolution_tasks.try_join_next().is_some() {}
        for video_id in video_ids {
            let source = Arc::clone(&self.0);
            resolution_tasks.spawn(async move {
                source.fetch(&video_id).await;
            });
        }
    }

    fn forget(&self, video_id: &str) {
        self.0
            .resolution_slots
            .lock()
            .expect("stream slots")
            .remove(video_id);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = START.set(Instant::now());
    let parsed_arguments = match parse(std::env::args().skip(1)) {
        Ok(parsed_arguments) => parsed_arguments,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    match play(parsed_arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn play(parsed_arguments: ParsedArguments) -> Result<(), String> {
    log("start");
    let stream_source = Streams(Arc::new(YtDlp::new()));
    let player_options = PlayerOptions {
        volume: 80,
        mpv_args: if parsed_arguments.use_null_audio_output {
            vec!["--ao=null".to_string()]
        } else {
            Vec::new()
        },
        ytdl_raw_options: stream_source.0.raw_options(),
        socket_dir: runtime_dir(),
        // Without the flag nothing is resolved ahead, so mpv's own hook does it
        // and the run shows what that costs.
        prefetch_ahead: if parsed_arguments.prefetch {
            parsed_arguments.video_ids.len()
        } else {
            0
        },
    };
    let player = Player::new(player_options, Some(stream_source.clone()));
    player.start().await.map_err(|error| error.to_string())?;

    let playlist_tracks: Vec<_> = parsed_arguments
        .video_ids
        .iter()
        .map(|video_id| bare_track(video_id, video_id))
        .collect();
    // Resolved here rather than inside `play`, so the first `audible` measures
    // mpv and not yt-dlp.
    let _ = stream_source.resolve(&parsed_arguments.video_ids[0]).await;

    let mut state_receiver = player.state();
    state_receiver.mark_unchanged();
    let mut playback_transition_started_at = Instant::now();
    player
        .play(playlist_tracks, 0)
        .await
        .map_err(|error| error.to_string())?;

    let run_deadline = tokio::time::Instant::now() + RUN_BUDGET;
    let mut current_queue_index = None;
    let mut audio_started = false;
    let mut skip_requested = false;
    // The position mpv last reported for the entry before this one. Its next
    // report can still carry that value, and it must not count as this entry.
    let mut previous_entry_position_seconds = 0.0;
    loop {
        tokio::select! {
            state_change = state_receiver.changed() => {
                if state_change.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(run_deadline) => {
                log("timeout");
                break;
            }
        }
        let player_state = state_receiver.borrow_and_update().clone();
        if let Some(error) = &player_state.error {
            log(&format!("error {error}"));
        }
        let Some(queue_index) = player_state.index else {
            if current_queue_index.is_some() {
                log("eof");
                break;
            }
            continue;
        };
        let Some(queue_entry) = player_state.queue.get(queue_index) else {
            continue;
        };
        if current_queue_index != Some(queue_index) {
            current_queue_index = Some(queue_index);
            audio_started = false;
            skip_requested = false;
            previous_entry_position_seconds = player_state.position;
            log(&format!("loaded {}", queue_entry.video_id));
            continue;
        }
        if !audio_started
            && player_state.position > 0.0
            && (previous_entry_position_seconds <= 0.0
                || player_state.position < previous_entry_position_seconds)
        {
            audio_started = true;
            log(&format!(
                "audible {} {}",
                queue_entry.video_id,
                playback_transition_started_at.elapsed().as_millis()
            ));
        }
        if let Some(skip_after_seconds) = parsed_arguments.skip_after_seconds
            && audio_started
            && !skip_requested
            && player_state.position >= skip_after_seconds
        {
            skip_requested = true;
            playback_transition_started_at = Instant::now();
            log("next");
            player.next().await.map_err(|error| error.to_string())?;
        }
    }

    player.close().await.map_err(|error| error.to_string())?;
    Ok(())
}
