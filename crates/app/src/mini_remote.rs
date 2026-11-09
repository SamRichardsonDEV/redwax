//! A companion binds to one bus owner so another session cannot inherit its controls.

use std::collections::HashMap;

use mpris_server::zbus::proxy::{Builder, CacheProperties};
use mpris_server::zbus::zvariant::{OwnedObjectPath, OwnedValue};
use mpris_server::zbus::{Connection, Proxy, fdo};
use redwax_player::Repeat;
use tokio::sync::Mutex;

const BUS_NAME: &str = "org.mpris.MediaPlayer2.redwax";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const TRACK_LIST_INTERFACE: &str = "org.mpris.MediaPlayer2.TrackList";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

type Properties = HashMap<String, OwnedValue>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiniTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MiniState {
    pub focus_active: bool,
    pub title: String,
    pub artist: String,
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub queue: Vec<MiniTrack>,
    pub current: Option<String>,
}

#[derive(Debug, Clone)]
pub enum MiniAction {
    TogglePause,
    Next,
    Previous,
    SeekTo(f64),
    SetVolume(u8),
    SetShuffle(bool),
    SetRepeat(Repeat),
    Play(String),
}

pub struct Remote {
    name: String,
    player: Proxy<'static>,
    track_list: Proxy<'static>,
    properties: Proxy<'static>,
    queue: Mutex<(Vec<OwnedObjectPath>, Vec<MiniTrack>)>,
}

impl Remote {
    pub async fn find(player: Option<&str>) -> Result<Option<Self>, String> {
        let connection = Connection::session()
            .await
            .map_err(|error| format!("Mini player needs the session bus: {error}"))?;
        let bus = fdo::DBusProxy::new(&connection)
            .await
            .map_err(|error| error.to_string())?;
        let selected = match player {
            Some(name) if is_redwax_name(name) => name.to_string(),
            Some(_) => return Err("Choose a redwax MPRIS session with --player".to_string()),
            None => {
                let mut names: Vec<String> = bus
                    .list_names()
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .filter(|name| is_redwax_name(name.as_str()))
                    .map(|name| name.to_string())
                    .collect();
                names.sort();
                match names.as_slice() {
                    [name] => name.clone(),
                    [] => return Ok(None),
                    _ => {
                        return Err(format!(
                            "Several redwax sessions are running. Choose one with --player: {}",
                            names.join(", ")
                        ));
                    }
                }
            }
        };
        let bus_name = mpris_server::zbus::names::BusName::try_from(selected.as_str())
            .map_err(|error| error.to_string())?;
        let owner = bus
            .get_name_owner(bus_name)
            .await
            .map_err(|error| format!("The redwax session is unavailable: {error}"))?
            .to_string();
        let remote = Remote {
            name: selected,
            player: uncached_proxy(&connection, &owner, PLAYER_INTERFACE).await?,
            track_list: uncached_proxy(&connection, &owner, TRACK_LIST_INTERFACE).await?,
            properties: uncached_proxy(&connection, &owner, PROPERTIES_INTERFACE).await?,
            queue: Mutex::new((Vec::new(), Vec::new())),
        };
        let root = uncached_proxy(&connection, &owner, "org.mpris.MediaPlayer2").await?;
        let has_track_list: bool = root
            .get_property("HasTrackList")
            .await
            .map_err(|error| error.to_string())?;
        if !has_track_list {
            return Err("Restart redwax with the updated version to use the mini player.".to_string());
        }
        Ok(Some(remote))
    }

    async fn require_owner(&self) -> Result<(), String> {
        let bus = fdo::DBusProxy::new(self.player.connection())
            .await
            .map_err(|error| error.to_string())?;
        let name = mpris_server::zbus::names::BusName::try_from(self.name.as_str())
            .map_err(|error| error.to_string())?;
        let owner = bus
            .get_name_owner(name)
            .await
            .map_err(|error| error.to_string())?;
        if owner.as_str() != self.player.destination().as_str() {
            return Err("The selected redwax session has restarted".to_string());
        }
        Ok(())
    }

    pub async fn state(&self) -> Result<MiniState, String> {
        self.require_owner().await?;
        let mut properties: Properties = self
            .properties
            .call("GetAll", &(PLAYER_INTERFACE,))
            .await
            .map_err(|error| format!("The redwax session is unavailable: {error}"))?;
        let mut metadata = properties
            .remove("Metadata")
            .and_then(|value| Properties::try_from(value).ok())
            .unwrap_or_default();
        let duration = metadata
            .remove("mpris:length")
            .and_then(|value| i64::try_from(value).ok())
            .map_or(0.0, seconds);
        let focus_active = metadata
            .remove(crate::mpris::FOCUS_METADATA_KEY)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false);
        let track = decode_track(metadata);
        let position = properties
            .remove("Position")
            .and_then(|value| i64::try_from(value).ok())
            .map_or(0.0, seconds);
        let volume = properties
            .remove("Volume")
            .and_then(|value| f64::try_from(value).ok())
            .filter(|value| value.is_finite())
            .unwrap_or(0.0);
        let status = properties
            .remove("PlaybackStatus")
            .and_then(|value| String::try_from(value).ok())
            .unwrap_or_default();
        let shuffle = properties
            .remove("Shuffle")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false);
        let repeat = match properties
            .remove("LoopStatus")
            .and_then(|value| String::try_from(value).ok())
            .as_deref()
        {
            Some("Playlist") => Repeat::All,
            Some("Track") => Repeat::One,
            _ => Repeat::Off,
        };
        let current = (!track.id.is_empty() && track.id != NO_TRACK).then_some(track.id);
        Ok(MiniState {
            focus_active,
            title: track.title,
            artist: track.artist,
            position,
            duration,
            paused: status != "Playing",
            volume: (volume.clamp(0.0, 1.0) * 100.0).round() as u8,
            shuffle,
            repeat,
            queue: self
                .read_queue()
                .await
                .map_err(|error| format!("The redwax session is unavailable: {error}"))?,
            current,
        })
    }

    pub async fn command(&self, action: MiniAction) -> Result<(), String> {
        self.require_owner().await?;
        match action {
            MiniAction::TogglePause => self.player.call("PlayPause", &()).await,
            MiniAction::Next => self.player.call("Next", &()).await,
            MiniAction::Previous => self.player.call("Previous", &()).await,
            MiniAction::SetVolume(volume) => {
                return self
                    .player
                    .set_property("Volume", f64::from(volume.min(100)) / 100.0)
                    .await
                    .map_err(|error| error.to_string());
            }
            MiniAction::SetShuffle(shuffle) => {
                return self
                    .player
                    .set_property("Shuffle", shuffle)
                    .await
                    .map_err(|error| error.to_string());
            }
            MiniAction::SetRepeat(repeat) => {
                let status = match repeat {
                    Repeat::Off => "None",
                    Repeat::All => "Playlist",
                    Repeat::One => "Track",
                };
                return self
                    .player
                    .set_property("LoopStatus", status)
                    .await
                    .map_err(|error| error.to_string());
            }
            MiniAction::SeekTo(position) => {
                if !position.is_finite() {
                    return Err("Seek position must be finite".to_string());
                }
                let mut metadata: Properties = self
                    .player
                    .get_property("Metadata")
                    .await
                    .map_err(|error| error.to_string())?;
                let Some(track_id) = metadata
                    .remove("mpris:trackid")
                    .and_then(|value| OwnedObjectPath::try_from(value).ok())
                    .filter(|path| path.as_str() != NO_TRACK)
                else {
                    return Ok(());
                };
                let microseconds = (position.max(0.0) * 1e6) as i64;
                self.player.call("SetPosition", &(track_id, microseconds)).await
            }
            MiniAction::Play(id) => {
                if !id.starts_with("/org/redwax/track/") {
                    return Err("The selected queue entry is invalid".to_string());
                }
                let track_id = OwnedObjectPath::try_from(id).map_err(|error| error.to_string())?;
                self.track_list.call("GoTo", &(track_id,)).await
            }
        }
        .map_err(|error| error.to_string())
    }

    async fn read_queue(&self) -> Result<Vec<MiniTrack>, String> {
        let tracks: Vec<OwnedObjectPath> = self
            .track_list
            .get_property("Tracks")
            .await
            .map_err(|error| error.to_string())?;
        let mut cached = self.queue.lock().await;
        if cached.0 != tracks {
            let metadata: Vec<Properties> = self
                .track_list
                .call("GetTracksMetadata", &(tracks.clone(),))
                .await
                .map_err(|error| error.to_string())?;
            let queue = metadata.into_iter().map(decode_track).collect();
            *cached = (tracks, queue);
        }
        Ok(cached.1.clone())
    }
}

pub(crate) fn is_redwax_name(name: &str) -> bool {
    name == BUS_NAME || name.starts_with("org.mpris.MediaPlayer2.redwax.instance")
}

async fn uncached_proxy(
    connection: &Connection,
    owner: &str,
    interface: &'static str,
) -> Result<Proxy<'static>, String> {
    // MPRIS does not emit routine position changes, so cached progress would freeze.
    Builder::<Proxy<'static>>::new(connection)
        .destination(owner.to_string())
        .and_then(|builder| builder.path(OBJECT_PATH))
        .and_then(|builder| builder.interface(interface))
        .map_err(|error| error.to_string())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|error| error.to_string())
}

fn decode_track(mut metadata: Properties) -> MiniTrack {
    let id = metadata
        .remove("mpris:trackid")
        .and_then(|value| OwnedObjectPath::try_from(value).ok())
        .map(|path| path.to_string())
        .unwrap_or_default();
    let title = metadata
        .remove("xesam:title")
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_default();
    let artist = metadata
        .remove("xesam:artist")
        .and_then(|value| Vec::<String>::try_from(value).ok())
        .unwrap_or_default()
        .join(", ");
    MiniTrack {
        id,
        title: title
            .chars()
            .filter(|character| !character.is_control())
            .collect(),
        artist: artist
            .chars()
            .filter(|character| !character.is_control())
            .collect(),
    }
}

fn seconds(microseconds: i64) -> f64 {
    microseconds.max(0) as f64 / 1e6
}
