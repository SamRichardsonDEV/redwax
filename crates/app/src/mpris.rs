//! MPRIS: the D-Bus interface media keys, playerctl and the bar speak to
//! players through. redwax registers as `org.mpris.MediaPlayer2.redwax` on
//! the session bus, falling back to `.instance<pid>` when the name is taken.
//! Commands arrive and are handed to the app through `PlayerControl`. State
//! flows the other way from the player's `watch` channel as property changes.
//! Ported from the Python `mpris.py` and `test_mpris.py`.
//!
//! Uses mpris-server's trait form (`RootInterface`, `PlayerInterface`,
//! `Server::new`) with the `tokio` feature, because its `Player` builder is
//! `Rc` based and cannot live on the multi-threaded runtime.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use mpris_server::zbus::{Connection, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, Property, RootInterface, Server,
    Signal, Time, TrackId, Uri, Volume,
};
use redwax_player::{PlayerState, Repeat};
use redwax_ytm::Track;
use tokio::task::JoinHandle;

use redwax_player::PlayerControl;

pub const BUS_NAME: &str = "redwax";
pub const IDENTITY: &str = "redwax";

/// A position that moves further than this between two updates is a seek, not
/// playback, so clients are told to redraw their progress bar.
const SEEK_JUMP_SECONDS: f64 = 2.0;

pub fn playback_status(player_state: &PlayerState) -> PlaybackStatus {
    match (player_state.track(), player_state.paused) {
        (None, _) => PlaybackStatus::Stopped,
        (Some(_), true) => PlaybackStatus::Paused,
        (Some(_), false) => PlaybackStatus::Playing,
    }
}

pub fn loop_status(repeat: Repeat) -> LoopStatus {
    match repeat {
        Repeat::Off => LoopStatus::None,
        Repeat::All => LoopStatus::Playlist,
        Repeat::One => LoopStatus::Track,
    }
}

pub fn repeat_from(status: LoopStatus) -> Repeat {
    match status {
        LoopStatus::None => Repeat::Off,
        LoopStatus::Playlist => Repeat::All,
        LoopStatus::Track => Repeat::One,
    }
}

/// `mpris:trackid` `/org/redwax/track/<index>`, `mpris:length` in
/// microseconds, `xesam:title`, `xesam:artist`, `xesam:album`,
/// `mpris:artUrl`. Empty when nothing plays.
pub fn metadata(player_state: &PlayerState) -> Metadata {
    let Some(track) = player_state.track() else {
        return Metadata::new();
    };
    let mut metadata = Metadata::builder()
        .trackid(track_id(player_state))
        .title(track.title.clone())
        .artist(artists(track))
        .url(track.url())
        .build();
    metadata.set_album(track.album.clone());
    metadata.set_art_url(track.thumbnail_url.clone());
    let duration_seconds = player_state
        .duration
        .or_else(|| track.duration_seconds.map(f64::from))
        .filter(|seconds| *seconds > 0.0);
    metadata.set_length(duration_seconds.map(|seconds| Time::from_micros((seconds * 1e6) as i64)));
    metadata
}

/// The observed queue-entry identity keeps duplicate tracks distinct and stays
/// stable when mpv reorders the queue.
fn track_id(player_state: &PlayerState) -> TrackId {
    let Some(queue_index) = player_state.index else {
        return TrackId::NO_TRACK;
    };
    let queue_entry_identity = player_state
        .queue_entries
        .get(queue_index)
        .map(|entry| entry.id().to_string())
        .unwrap_or_else(|| format!("legacy_{queue_index}"));
    TrackId::try_from(format!("/org/redwax/track/{queue_entry_identity}")).unwrap_or(TrackId::NO_TRACK)
}

fn artists(track: &Track) -> Vec<String> {
    if track.artists.is_empty() {
        vec![track.artist_line()]
    } else {
        track.artists.clone()
    }
}

pub struct Mpris {
    /// `Quit` cannot stop the app from here, so the app polls this instead.
    quit: Arc<AtomicBool>,
    bus_name: String,
    /// Taken by `close` to release the bus name and stop the watcher.
    running: Mutex<Option<Running>>,
}

/// Everything `close` has to take down, in one place. The server is held as
/// `Any` because `Mpris` hides which player type it serves.
struct Running {
    watcher: JoinHandle<()>,
    server: Box<dyn std::any::Any + Send + Sync>,
}

impl Mpris {
    /// Connect to the session bus and serve. Property changes are emitted for
    /// every change on `player.state()` until `close`. Errors when there is
    /// no session bus, which the app reports as media keys unavailable.
    pub async fn start<P: PlayerControl>(player: P) -> Result<Mpris, String> {
        let bus_name_suffix = free_bus_name().await?;
        let quit = Arc::new(AtomicBool::new(false));
        let service = Service {
            player: player.clone(),
            quit: Arc::clone(&quit),
        };
        let server = Server::new(&bus_name_suffix, service)
            .await
            .map_err(|error| format!("MPRIS name {bus_name_suffix} refused: {error}"))?;
        let bus_name = server.bus_name().to_string();
        let server = Arc::new(server);
        // The watcher holds a weak reference, so `close` frees the bus name
        // without waiting for the task to notice it was aborted.
        let watcher = tokio::spawn(watch(Arc::downgrade(&server), player));
        Ok(Mpris {
            quit,
            bus_name,
            running: Mutex::new(Some(Running {
                watcher,
                server: Box::new(server),
            })),
        })
    }

    /// The well known name this instance owns, which carries `.instance<pid>`
    /// when another redwax was already on the bus.
    pub fn bus_name(&self) -> String {
        self.bus_name.clone()
    }

    /// True once a client has called `Quit`. The app polls it and exits.
    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        let running_service = self
            .running
            .lock()
            .ok()
            .and_then(|mut running_service_slot| running_service_slot.take());
        if let Some(running_service) = running_service {
            running_service.watcher.abort();
            drop(running_service.server);
        }
    }
}

impl Drop for Mpris {
    fn drop(&mut self) {
        self.close();
    }
}

/// `redwax`, or `redwax.instance<pid>` when another player already answers to
/// that name. The check is a question to the bus because zbus queues for a
/// taken name rather than refusing it, which would leave redwax unreachable.
async fn free_bus_name() -> Result<String, String> {
    let connection = Connection::session()
        .await
        .map_err(|error| format!("no session bus: {error}"))?;
    let bus = fdo::DBusProxy::new(&connection)
        .await
        .map_err(|error| format!("no session bus: {error}"))?;
    let process_id = std::process::id();
    for name_suffix in candidate_names(process_id) {
        let candidate_bus_name = format!("org.mpris.MediaPlayer2.{name_suffix}");
        let is_registered = match candidate_bus_name.as_str().try_into() {
            Ok(name) => bus.name_has_owner(name).await.unwrap_or(false),
            Err(_) => false,
        };
        if !is_registered {
            return Ok(name_suffix);
        }
    }
    Err("every MPRIS name for redwax is taken".to_string())
}

/// The plain name, then the `.instance<pid>` form MPRIS reserves for a second
/// player, then numbered ones so two instances in one process still differ.
fn candidate_names(process_id: u32) -> impl Iterator<Item = String> {
    let instance_bus_name = format!("{BUS_NAME}.instance{process_id}");
    [BUS_NAME.to_string(), instance_bus_name.clone()]
        .into_iter()
        .chain((2..8).map(move |instance_number| format!("{instance_bus_name}_{instance_number}")))
}

/// The state Python's `Mpris.update` compares, so a bar redraws exactly as
/// often as it did before.
#[derive(PartialEq)]
struct Emitted {
    track: Option<String>,
    status: PlaybackStatus,
    repeat: Repeat,
    shuffle: bool,
    volume: u8,
    muted: bool,
    duration: Option<f64>,
}

impl Emitted {
    fn of(player_state: &PlayerState) -> Emitted {
        Emitted {
            track: player_state.track().map(|track| track.video_id.clone()),
            status: playback_status(player_state),
            repeat: player_state.repeat,
            shuffle: player_state.shuffle,
            volume: player_state.volume,
            muted: player_state.muted,
            duration: player_state.duration,
        }
    }
}

async fn watch<P: PlayerControl>(server: Weak<Server<Service<P>>>, player: P) {
    let mut player_state_updates = player.state();
    let mut last_emitted_state: Option<Emitted> = None;
    let mut last_position_seconds = player_state_updates.borrow().position;
    loop {
        let Some(server) = server.upgrade() else {
            return;
        };
        let player_state = player_state_updates.borrow_and_update().clone();
        let emitted_state = Emitted::of(&player_state);
        let track_changed = last_emitted_state
            .as_ref()
            .is_none_or(|previously_emitted| previously_emitted.track != emitted_state.track);
        let position_jumped = (player_state.position - last_position_seconds).abs() > SEEK_JUMP_SECONDS;
        last_position_seconds = player_state.position;
        // A new track starts at zero, which is a jump but not a seek.
        if position_jumped && !track_changed {
            let seek_position = Time::from_micros((player_state.position * 1e6) as i64);
            let _ = server
                .emit(Signal::Seeked {
                    position: seek_position,
                })
                .await;
        }
        if last_emitted_state.as_ref() != Some(&emitted_state) {
            let mut changed_properties = vec![Property::PlaybackStatus(emitted_state.status)];
            if track_changed
                || last_emitted_state
                    .as_ref()
                    .is_none_or(|previously_emitted| previously_emitted.duration != emitted_state.duration)
            {
                changed_properties.push(Property::Metadata(metadata(&player_state)));
            }
            if last_emitted_state
                .as_ref()
                .is_none_or(|previously_emitted| previously_emitted.repeat != emitted_state.repeat)
            {
                changed_properties.push(Property::LoopStatus(loop_status(emitted_state.repeat)));
            }
            if last_emitted_state
                .as_ref()
                .is_none_or(|previously_emitted| previously_emitted.shuffle != emitted_state.shuffle)
            {
                changed_properties.push(Property::Shuffle(emitted_state.shuffle));
            }
            if last_emitted_state.as_ref().is_none_or(|previously_emitted| {
                (previously_emitted.volume, previously_emitted.muted)
                    != (emitted_state.volume, emitted_state.muted)
            }) {
                changed_properties.push(Property::Volume(volume_of(&player_state)));
            }
            last_emitted_state = Some(emitted_state);
            let _ = server.properties_changed(changed_properties).await;
        }
        drop(server);
        if player_state_updates.changed().await.is_err() {
            return;
        }
    }
}

fn volume_of(player_state: &PlayerState) -> Volume {
    if player_state.muted {
        0.0
    } else {
        f64::from(player_state.volume) / 100.0
    }
}

struct Service<P: PlayerControl> {
    player: P,
    quit: Arc<AtomicBool>,
}

/// `PlayerControl` promises `Send` futures and mpris-server's traits need
/// `Send + Sync`, so every command runs on a task of its own. That also keeps
/// a slow player from blocking the bus connection.
macro_rules! command {
    ($self:ident, $what:literal, |$player:ident| $call:expr) => {{
        let $player = $self.player.clone();
        let task = tokio::spawn(async move { $call.await });
        finished(task, $what).await
    }};
}

async fn finished<E>(command_task: JoinHandle<Result<(), E>>, operation_name: &str) -> fdo::Result<()> {
    match command_task.await {
        Ok(Ok(())) => Ok(()),
        _ => Err(fdo::Error::Failed(format!("redwax could not {operation_name}"))),
    }
}

impl<P: PlayerControl> RootInterface for Service<P> {
    async fn raise(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        self.quit.store(true, Ordering::Relaxed);
        command!(self, "quit", |player| player.close())
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> mpris_server::zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok(IDENTITY.to_string())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok(IDENTITY.to_string())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl<P: PlayerControl> PlayerInterface for Service<P> {
    async fn next(&self) -> fdo::Result<()> {
        command!(self, "skip forward", |player| player.next())
    }

    async fn previous(&self) -> fdo::Result<()> {
        command!(self, "skip back", |player| player.previous())
    }

    async fn pause(&self) -> fdo::Result<()> {
        let player_state = self.player.snapshot();
        if player_state.paused || player_state.track().is_none() {
            return Ok(());
        }
        command!(self, "pause", |player| player.toggle_pause())
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        command!(self, "play or pause", |player| player.toggle_pause())
    }

    /// Stop clears the queue, because MPRIS stop means back to no track and
    /// redwax has no stopped state that keeps a queue.
    async fn stop(&self) -> fdo::Result<()> {
        command!(self, "stop", |player| player.clear())
    }

    async fn play(&self) -> fdo::Result<()> {
        let player_state = self.player.snapshot();
        if !player_state.paused && player_state.track().is_some() {
            return Ok(());
        }
        command!(self, "play", |player| player.toggle_pause())
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        let delta_seconds = offset.as_micros() as f64 / 1e6;
        command!(self, "seek", |player| player.seek(delta_seconds))
    }

    async fn set_position(&self, requested_track_id: TrackId, position: Time) -> fdo::Result<()> {
        if requested_track_id != track_id(&self.player.snapshot()) {
            return Ok(());
        }
        let position_seconds = position.as_micros() as f64 / 1e6;
        command!(self, "seek", |player| player.seek_to(position_seconds))
    }

    async fn open_uri(&self, _uri: Uri) -> fdo::Result<()> {
        Ok(())
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(playback_status(&self.player.snapshot()))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(loop_status(self.player.snapshot().repeat))
    }

    async fn set_loop_status(&self, status: LoopStatus) -> mpris_server::zbus::Result<()> {
        let repeat_mode = repeat_from(status);
        let _ = command!(self, "set repeat", |player| player.set_repeat(repeat_mode));
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: PlaybackRate) -> mpris_server::zbus::Result<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.player.snapshot().shuffle)
    }

    async fn set_shuffle(&self, shuffle: bool) -> mpris_server::zbus::Result<()> {
        let _ = command!(self, "set shuffle", |player| player.set_shuffle(shuffle));
        Ok(())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(metadata(&self.player.snapshot()))
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(volume_of(&self.player.snapshot()))
    }

    async fn set_volume(&self, volume: Volume) -> mpris_server::zbus::Result<()> {
        let volume_percent = (volume.clamp(0.0, 1.0) * 100.0).round() as u8;
        let _ = command!(self, "set the volume", |player| player.set_volume(volume_percent));
        Ok(())
    }

    async fn position(&self) -> fdo::Result<Time> {
        let position_seconds = self.player.snapshot().position;
        Ok(Time::from_micros((position_seconds * 1e6) as i64))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}
