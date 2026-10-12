//! YouTube Music transport and response parsing stay behind semantic models.

mod innertube;
mod models;
mod pagination;
mod radio;
mod walker;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use futures::stream::StreamExt;
use serde_json::{Value, json};

pub use models::{
    Album, Artist, Browsable, Continuation, Item, LyricLine, Lyrics, Page, Playlist, PlaylistEntry, Shelf,
    Track, format_duration,
};
pub use pagination::{CompletePage, IncompletePage, PagePagination, collect_page};
pub use radio::RadioContinuation;
pub use redwax_auth::BrowserSession as Session;

pub const LIKED_SONGS_PLAYLIST_ID: &str = "LM";

pub const THUMBNAIL_SIZE: u32 = 544;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not signed in")]
    SignedOut,
    #[error("YouTube rejected the session, run `redwax login`")]
    Rejected,
    #[error("network: {0}")]
    Network(String),
    #[error("YouTube rejected the playlist change ({status})")]
    PlaylistEditRejected { status: String },
    #[error("the playlist does not expose editable entries for this account")]
    PlaylistNotEditable,
    #[error("could not read the {page} page, raw response saved to {}", dump.display())]
    Empty { page: &'static str, dump: PathBuf },
    #[error("incomplete page: {0}")]
    IncompletePage(&'static str),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Repeated saves succeed without claiming that a new playlist row was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistAdd {
    Added,
    AlreadyPresent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistRemoval {
    Removed { occurrences: usize },
    AlreadyAbsent,
}

/// One identity a Google login can act as: the login itself, or a brand
/// channel under it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Account {
    pub authuser: u32,
    pub page_id: Option<String>,
    pub name: String,
    pub handle: Option<String>,
}

impl Account {
    pub fn label(&self) -> String {
        match &self.handle {
            Some(handle) => format!("{} {}", self.name, handle),
            None => self.name.clone(),
        }
    }

    /// Handles match with or without the `@` prefix.
    pub fn matches(&self, account_query: &str) -> bool {
        let normalized_query = account_query.trim().trim_start_matches('@').to_lowercase();
        let normalized_handle = self
            .handle
            .as_deref()
            .unwrap_or("")
            .trim_start_matches('@')
            .to_lowercase();
        !normalized_query.is_empty()
            && (normalized_query == self.name.to_lowercase() || normalized_query == normalized_handle)
    }
}

/// Account numbers start at one. Handles accept either spelling of the `@` prefix.
pub fn find_account<'a>(accounts: &'a [Account], account_query: &str) -> Option<&'a Account> {
    let account_query = account_query.trim();
    if let Ok(account_number) = account_query.parse::<usize>()
        && account_number >= 1
    {
        return accounts.get(account_number - 1);
    }
    accounts.iter().find(|account| account.matches(account_query))
}

/// Where dumps of unreadable pages go. The app passes its cache dir.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub session: Option<Session>,
    pub dump_dir: PathBuf,
    pub home_rows: usize,
}

/// Semantic operations shared by the network client and offline interface tests.
pub trait MusicSource: Send + Sync + 'static {
    fn signed_in(&self) -> bool;

    /// The signed-in identity's handle or name. `None` means signed out or an
    /// otherwise successful response with no active identity.
    fn whoami(&self) -> impl Future<Output = Result<Option<String>>> + Send;

    fn home(&self) -> impl Future<Output = Result<Page>> + Send;

    fn search(&self, query: &str) -> impl Future<Output = Result<Page>> + Send;

    /// Failed sections appear in the subtitle so available library sections remain usable.
    fn library(&self) -> impl Future<Output = Result<Page>> + Send;

    /// A playlist can be incomplete. Use `collect_page` before edits that need all members.
    fn browse(&self, item: &Browsable) -> impl Future<Output = Result<Page>> + Send;

    /// The default refuses because an empty success would look like completion.
    fn continue_page(
        &self,
        continuation: &Continuation,
    ) -> impl Future<Output = Result<(Vec<Item>, Option<Continuation>)>> + Send {
        let _ = continuation;
        async {
            Err(Error::IncompletePage(
                "this source does not support page continuations",
            ))
        }
    }

    /// The first radio batch seeded by `track`, with the seed first.
    fn radio(&self, track: &Track) -> impl Future<Output = Result<Vec<Track>>> + Send;

    /// Queue continuation stays opaque because its request context must travel with its token.
    fn start_radio(
        &self,
        track: &Track,
    ) -> impl Future<Output = Result<(Vec<Track>, Option<RadioContinuation>)>> + Send {
        async move { self.radio(track).await.map(|tracks| (tracks, None)) }
    }

    fn continue_radio(
        &self,
        continuation: &RadioContinuation,
    ) -> impl Future<Output = Result<(Vec<Track>, Option<RadioContinuation>)>> + Send {
        let _ = continuation;
        async { Ok((Vec::new(), None)) }
    }

    /// Plain lyrics text, `None` when YouTube has none. Cached per video id.
    fn lyrics(&self, track: &Track) -> impl Future<Output = Result<Option<String>>> + Send;

    /// Line timed lyrics when YouTube has them, the plain text when it has
    /// only that, `None` when the track has no lyrics at all.
    fn timed_lyrics(&self, track: &Track) -> impl Future<Output = Result<Option<Lyrics>>> + Send;

    fn set_liked(&self, track: &Track, liked: bool) -> impl Future<Output = Result<()>> + Send;

    /// Existing songs remain single entries. Unsupported sources must refuse the edit.
    fn add_to_playlist(
        &self,
        playlist_id: &str,
        video_ids: &[String],
    ) -> impl Future<Output = Result<PlaylistAdd>> + Send {
        let _ = (playlist_id, video_ids);
        async { Err(Error::Other("this source cannot edit a playlist".to_string())) }
    }

    /// Remove one exact playlist occurrence through its opaque edit authority.
    fn remove_playlist_entry(
        &self,
        playlist_id: &str,
        video_id: &str,
        entry: &PlaylistEntry,
    ) -> impl Future<Output = Result<()>> + Send {
        let _ = (playlist_id, video_id, entry);
        async { Err(Error::Other("this source cannot edit a playlist".to_string())) }
    }

    /// Remove every occurrence of `video_id` after proving the playlist is complete.
    fn remove_all_from_playlist(
        &self,
        playlist: &Playlist,
        video_id: &str,
    ) -> impl Future<Output = Result<PlaylistRemoval>> + Send {
        let _ = (playlist, video_id);
        async { Err(Error::Other("this source cannot edit a playlist".to_string())) }
    }

    /// New playlists are private. Unsupported sources must refuse the edit.
    fn create_playlist(
        &self,
        title: &str,
        video_ids: &[String],
    ) -> impl Future<Output = Result<String>> + Send {
        let _ = (title, video_ids);
        async { Err(Error::Other("this source cannot make a playlist".to_string())) }
    }

    /// Identities the login can act as, the login itself first.
    fn accounts(&self) -> impl Future<Output = Result<Vec<Account>>> + Send;

    /// Duration in seconds. `None` leaves the displayed duration blank.
    fn track_length(&self, video_id: &str) -> impl Future<Output = Result<Option<u32>>> + Send {
        let _ = video_id;
        async { Ok(None) }
    }

    /// Unknown durations are omitted from the batch result.
    fn track_lengths(&self, video_ids: &[String]) -> impl Future<Output = Vec<(String, u32)>> + Send {
        let video_ids = video_ids.to_vec();
        async move {
            futures::stream::iter(video_ids)
                .map(|video_id| async move {
                    let duration_seconds = self.track_length(&video_id).await.ok().flatten();
                    duration_seconds.map(|duration_seconds| (video_id, duration_seconds))
                })
                .buffer_unordered(TRACK_LENGTH_REQUESTS)
                .filter_map(std::future::ready)
                .collect()
                .await
        }
    }
}

/// The real client. Built at launch without touching the network.
pub struct Client {
    options: ClientOptions,
    transport: innertube::Transport,
    /// Lyrics cost two round trips, so every half is remembered per video id:
    /// the browse id a `next` call hands back, the plain text, and the timed
    /// lines the mobile identity is served.
    lyrics_ids: RwLock<HashMap<String, Option<String>>>,
    lyrics: RwLock<HashMap<String, Option<String>>>,
    timed_lyrics: RwLock<HashMap<String, Option<Lyrics>>>,
}

impl Client {
    pub fn new(mut options: ClientOptions) -> Client {
        let transport = innertube::Transport::new(options.session.take());
        Client {
            options,
            transport,
            lyrics_ids: RwLock::new(HashMap::new()),
            lyrics: RwLock::new(HashMap::new()),
            timed_lyrics: RwLock::new(HashMap::new()),
        }
    }

    pub fn reload(&self, session: Option<Session>) {
        self.transport.reload(session);
        self.lyrics_ids
            .write()
            .expect("lock is never held across a panic")
            .clear();
        self.lyrics
            .write()
            .expect("lock is never held across a panic")
            .clear();
        self.timed_lyrics
            .write()
            .expect("lock is never held across a panic")
            .clear();
    }

    /// A failed dump keeps the intended path so diagnostics can still name the unreadable page.
    fn dump(&self, page_name: &'static str, responses: &[Value]) -> Error {
        let timestamp_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or_default();
        let dump_path = self
            .options
            .dump_dir
            .join(format!("{page_name}-{timestamp_millis}.json"));
        let redacted_responses: Vec<Value> = responses.iter().map(scrub_response).collect();
        let dump_document = match redacted_responses.as_slice() {
            [only] => only.clone(),
            many => Value::Array(many.to_vec()),
        };
        if ensure_private_dir(&self.options.dump_dir).is_ok()
            && let Ok(serialized_dump) = serde_json::to_string(&dump_document)
        {
            let _ = write_private(&dump_path, serialized_dump.as_bytes());
            trim_dumps(&self.options.dump_dir);
        }
        Error::Empty {
            page: page_name,
            dump: dump_path,
        }
    }

    async fn browse_page(&self, browse_id: &str, minimum_item_count: usize) -> Result<Vec<Value>> {
        self.transport
            .browse_with_continuations(json!({ "browseId": browse_id }), minimum_item_count)
            .await
    }

    async fn album_page(&self, album: &Album) -> Result<Page> {
        let responses = self.browse_page(&album.browse_id, 1).await?;
        let page = album_from(&responses, album);
        if page.is_empty() {
            return Err(self.dump("album", &responses));
        }
        Ok(page)
    }

    /// Returning a continuation lets long playlists display before collection finishes.
    async fn playlist_page(&self, playlist: &Playlist) -> Result<Page> {
        let browse_id = match playlist.playlist_id.starts_with("VL") {
            true => playlist.playlist_id.clone(),
            false => format!("VL{}", playlist.playlist_id),
        };
        let response = self
            .transport
            .post("browse", json!({ "browseId": &browse_id }))
            .await?;
        let mut page = playlist_from(std::slice::from_ref(&response), playlist);
        if page.is_empty() {
            return Err(self.dump("playlist", std::slice::from_ref(&response)));
        }
        page.more = continuation_of(&response, &browse_id);
        Ok(page)
    }

    async fn artist_page(&self, artist: &Artist) -> Result<Page> {
        let responses = self.browse_page(&artist.channel_id, 1).await?;
        let Some(initial_response) = responses.first() else {
            return Err(self.dump("artist", &responses));
        };
        let page = artist_from(initial_response, artist);
        if page.is_empty() {
            return Err(self.dump("artist", &responses));
        }
        Ok(page)
    }

    async fn watch(&self, video_id: &str, is_radio_request: bool) -> Result<Value> {
        let mut request_body = radio::request_body(video_id);
        let request_fields = request_body.as_object_mut().expect("built as an object");
        if !is_radio_request {
            request_fields.remove("params");
            request_fields.insert(
                "watchEndpointMusicSupportedConfigs".to_string(),
                json!({
                    "watchEndpointMusicConfig": {
                        "hasPersistentPlaylistPanel": true,
                        "musicVideoType": "MUSIC_VIDEO_TYPE_ATV",
                    }
                }),
            );
        }
        let response = self.transport.post("next", request_body).await?;
        let lyrics_page_id = lyrics_browse_id(&response);
        self.lyrics_ids
            .write()
            .expect("lock is never held across a panic")
            .insert(video_id.to_string(), lyrics_page_id);
        Ok(response)
    }

    async fn timed_lines(&self, browse_id: &str) -> Result<Option<Vec<LyricLine>>> {
        let response = self
            .transport
            .post_mobile("browse", json!({ "browseId": browse_id }))
            .await?;
        let lines = timed_lyric_lines(&response);
        Ok((!lines.is_empty()).then_some(lines))
    }

    async fn lyrics_id_for(&self, video_id: &str) -> Result<Option<String>> {
        if let Some(cached_browse_id) = self
            .lyrics_ids
            .read()
            .expect("lock is never held across a panic")
            .get(video_id)
        {
            return Ok(cached_browse_id.clone());
        }
        self.watch(video_id, false).await?;
        Ok(self
            .lyrics_ids
            .read()
            .expect("lock is never held across a panic")
            .get(video_id)
            .cloned()
            .flatten())
    }

    async fn edit_playlist(&self, playlist_id: &str, actions: Vec<Value>) -> Result<()> {
        if !self.signed_in() {
            return Err(Error::SignedOut);
        }
        let body = edit_playlist_body(playlist_id, actions);
        let response = self.transport.post("browse/edit_playlist", body).await?;
        match edit_status(&response) {
            Some(status) => Err(Error::PlaylistEditRejected {
                status: status.to_string(),
            }),
            None => Ok(()),
        }
    }

    async fn remove_playlist_entries(
        &self,
        playlist_id: &str,
        entries: &[(&str, &PlaylistEntry)],
    ) -> Result<()> {
        let actions = entries
            .iter()
            .map(|(video_id, entry)| {
                json!({
                    "action": "ACTION_REMOVE_VIDEO",
                    "removedVideoId": video_id,
                    "setVideoId": entry.token(),
                })
            })
            .collect();
        self.edit_playlist(playlist_id, actions).await
    }

    async fn playlist_members(&self, playlist_id: &str) -> Result<HashSet<String>> {
        let playlist_id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
        let browse_id = format!("VL{playlist_id}");
        let response = self
            .transport
            .post("browse", json!({ "browseId": browse_id }))
            .await?;
        let mut shelves = Vec::new();
        collect_key(&response, "musicPlaylistShelfRenderer", &mut shelves);
        if shelves.is_empty() {
            collect_key(&response, "musicShelfRenderer", &mut shelves);
        }
        let Some(shelf) = shelves.first() else {
            return Err(Error::Other(
                "Could not verify the songs already in this playlist".to_string(),
            ));
        };
        // Suggestions are outside the track shelf and must not count as saved songs.
        let page = Page {
            title: "Playlist members".to_string(),
            shelves: vec![Shelf::new("Tracks", walker::items(shelf))],
            subtitle: None,
            thumbnail_url: None,
            more: continuation_of(shelf, &browse_id),
        };
        let page = collect_page(self, page)
            .await
            .map_err(|incomplete| incomplete.error)?
            .into_page();
        Ok(page.tracks().map(|track| track.video_id.clone()).collect())
    }
}

fn add_playlist_actions(video_ids: &[String]) -> Vec<Value> {
    video_ids
        .iter()
        .map(|video_id| json!({ "action": "ACTION_ADD_VIDEO", "addedVideoId": video_id }))
        .collect()
}

fn playlist_entries_to_remove(page: &Page, video_id: &str) -> Result<Option<Vec<(String, PlaylistEntry)>>> {
    let matching_tracks: Vec<&Track> = page.tracks().filter(|track| track.video_id == video_id).collect();
    if matching_tracks.is_empty() {
        return Ok(None);
    }
    matching_tracks
        .into_iter()
        .map(|track| {
            track
                .playlist_entry
                .clone()
                .map(|entry| (track.video_id.clone(), entry))
                .ok_or(Error::PlaylistNotEditable)
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// Browse IDs carry `VL`, but the edit endpoint requires the underlying playlist ID.
fn edit_playlist_body(playlist_id: &str, actions: Vec<Value>) -> Value {
    json!({
        "playlistId": playlist_id.strip_prefix("VL").unwrap_or(playlist_id),
        "actions": actions,
    })
}

/// Successful responses can omit status.
fn edit_status(response: &Value) -> Option<&str> {
    let status = response.get("status")?.as_str()?;
    (!status.contains("SUCCEEDED")).then_some(status)
}

/// Sharing must be an explicit choice after creation.
fn create_playlist_body(title: &str, video_ids: &[String]) -> Value {
    json!({
        "title": title,
        "description": "",
        "privacyStatus": "PRIVATE",
        "videoIds": video_ids,
    })
}

/// Success without a usable playlist ID cannot authorize later edits.
fn created_playlist_id(response: &Value) -> Option<&str> {
    response.get("playlistId")?.as_str().filter(|id| !id.is_empty())
}

const HISTORY_LIMIT: usize = 30;
const TRACK_LENGTH_REQUESTS: usize = 3;
/// Enough to see a layout change without filling the cache directory.
const MAX_DUMPS: usize = 10;

const LIBRARY_SECTIONS: [(&str, &str); 4] = [
    ("Playlists", "FEmusic_liked_playlists"),
    ("Albums", "FEmusic_liked_albums"),
    ("Artists", "FEmusic_library_corpus_track_artists"),
    ("History", "FEmusic_history"),
];

impl MusicSource for Client {
    fn signed_in(&self) -> bool {
        self.transport.session().is_some()
    }

    async fn whoami(&self) -> Result<Option<String>> {
        if !self.signed_in() {
            return Ok(None);
        }
        let response = self.transport.post("account/account_menu", json!({})).await?;
        Ok(active_identity(&response).and_then(|identity| identity.handle.or(Some(identity.name))))
    }

    async fn home(&self) -> Result<Page> {
        let requested_shelf_count = self.options.home_rows.max(1);
        let responses = self
            .transport
            .browse_with_continuations(json!({ "browseId": "FEmusic_home" }), requested_shelf_count)
            .await?;
        let page = home_from(&responses, requested_shelf_count);
        if page.is_empty() {
            return Err(self.dump("home", &responses));
        }
        Ok(page)
    }

    async fn search(&self, query: &str) -> Result<Page> {
        let response = self.transport.post("search", json!({ "query": query })).await?;
        let page = search_from(&response, query);
        if page.is_empty() {
            return Err(self.dump("search", std::slice::from_ref(&response)));
        }
        Ok(page)
    }

    async fn library(&self) -> Result<Page> {
        if !self.signed_in() {
            return Ok(Page {
                title: "Library".to_string(),
                subtitle: Some("Run `redwax login` to see your playlists, likes and history.".to_string()),
                ..Page::default()
            });
        }

        let liked_songs_playlist = Item::Playlist(Playlist {
            playlist_id: LIKED_SONGS_PLAYLIST_ID.to_string(),
            title: "Liked songs".to_string(),
            subtitle: Some("Your thumbs-ups".to_string()),
            thumbnail_url: None,
        });
        let mut shelves = vec![Shelf::new("Collections", vec![liked_songs_playlist])];
        let mut section_failures: Vec<String> = Vec::new();

        let (playlist_responses, album_responses, artist_responses, history_responses) = tokio::join!(
            self.browse_page(LIBRARY_SECTIONS[0].1, 1),
            self.browse_page(LIBRARY_SECTIONS[1].1, 1),
            self.browse_page(LIBRARY_SECTIONS[2].1, 1),
            self.browse_page(LIBRARY_SECTIONS[3].1, 1),
        );
        for ((title, _), section_result) in LIBRARY_SECTIONS.iter().zip([
            playlist_responses,
            album_responses,
            artist_responses,
            history_responses,
        ]) {
            match section_result {
                // One dead endpoint must not hide the rest.
                Err(error) => section_failures.push(format!("{title}: {error}")),
                Ok(responses) => {
                    let mut items: Vec<Item> = responses.iter().flat_map(walker::items).collect();
                    if *title == "History" {
                        items.truncate(HISTORY_LIMIT);
                    }
                    shelves.push(Shelf::new(*title, items));
                }
            }
        }

        shelves.retain(|shelf| !shelf.items.is_empty());
        Ok(Page {
            title: "Library".to_string(),
            shelves,
            subtitle: (!section_failures.is_empty()).then(|| section_failures.join("; ")),
            thumbnail_url: None,
            ..Page::default()
        })
    }

    async fn browse(&self, item: &Browsable) -> Result<Page> {
        match item {
            Browsable::Album(album) => self.album_page(album).await,
            Browsable::Playlist(playlist) => self.playlist_page(playlist).await,
            Browsable::Artist(artist) => self.artist_page(artist).await,
        }
    }

    async fn continue_page(&self, continuation: &Continuation) -> Result<(Vec<Item>, Option<Continuation>)> {
        let continuation_request = json!({
            "browseId": &continuation.browse_id,
            "continuation": &continuation.token,
        });
        let response = self.transport.post("browse", continuation_request).await?;
        let items = walker::items(&response);
        let next_continuation = continuation_of(&response, &continuation.browse_id);
        Ok((items, next_continuation))
    }

    async fn radio(&self, track: &Track) -> Result<Vec<Track>> {
        self.start_radio(track).await.map(|(tracks, _)| tracks)
    }

    async fn start_radio(&self, track: &Track) -> Result<(Vec<Track>, Option<RadioContinuation>)> {
        let response = self.watch(&track.video_id, true).await?;
        let seed_request = radio::request_body(&track.video_id);
        let (tracks, continuation) = radio::initial_batch(&response, &seed_request, track)
            .ok_or_else(|| Error::Other("radio response has no queue panel".to_string()))?;
        if tracks.len() < 2 && continuation.is_none() {
            return Err(self.dump("radio", std::slice::from_ref(&response)));
        }
        Ok((tracks, continuation))
    }

    async fn continue_radio(
        &self,
        continuation: &RadioContinuation,
    ) -> Result<(Vec<Track>, Option<RadioContinuation>)> {
        let response = self
            .transport
            .post_continuation("next", continuation.request().clone(), continuation.token())
            .await?;
        radio::next_batch(&response, continuation.request())
            .ok_or_else(|| Error::Other("radio continuation has no queue panel".to_string()))
    }

    async fn lyrics(&self, track: &Track) -> Result<Option<String>> {
        if let Some(cached_lyrics) = self
            .lyrics
            .read()
            .expect("lock is never held across a panic")
            .get(&track.video_id)
        {
            return Ok(cached_lyrics.clone());
        }
        let plain_lyrics = match self.lyrics_id_for(&track.video_id).await? {
            None => None,
            Some(browse_id) => {
                let response = self
                    .transport
                    .post("browse", json!({ "browseId": browse_id }))
                    .await?;
                lyrics_text(&response)
            }
        };
        self.lyrics
            .write()
            .expect("lock is never held across a panic")
            .insert(track.video_id.clone(), plain_lyrics.clone());
        Ok(plain_lyrics)
    }

    async fn timed_lyrics(&self, track: &Track) -> Result<Option<Lyrics>> {
        if let Some(cached_lyrics) = self
            .timed_lyrics
            .read()
            .expect("lock is never held across a panic")
            .get(&track.video_id)
        {
            return Ok(cached_lyrics.clone());
        }
        let loaded_lyrics = match self.lyrics_id_for(&track.video_id).await? {
            None => None,
            Some(browse_id) => match self.timed_lines(&browse_id).await? {
                Some(lines) => Some(Lyrics::Timed(lines)),
                None => self.lyrics(track).await?.map(Lyrics::Plain),
            },
        };
        self.timed_lyrics
            .write()
            .expect("lock is never held across a panic")
            .insert(track.video_id.clone(), loaded_lyrics.clone());
        Ok(loaded_lyrics)
    }

    async fn set_liked(&self, track: &Track, liked: bool) -> Result<()> {
        if !self.signed_in() {
            return Err(Error::SignedOut);
        }
        let endpoint = if liked { "like/like" } else { "like/removelike" };
        let body = json!({ "target": { "videoId": track.video_id } });
        self.transport.post(endpoint, body).await.map(|_| ())
    }

    async fn add_to_playlist(&self, playlist_id: &str, video_ids: &[String]) -> Result<PlaylistAdd> {
        if video_ids.is_empty() {
            return Ok(PlaylistAdd::AlreadyPresent);
        }
        let mut seen_video_ids = HashSet::new();
        let video_ids: Vec<String> = video_ids
            .iter()
            .filter(|video_id| seen_video_ids.insert(video_id.as_str()))
            .cloned()
            .collect();
        let edit_rejection = match self
            .edit_playlist(playlist_id, add_playlist_actions(&video_ids))
            .await
        {
            Ok(()) => return Ok(PlaylistAdd::Added),
            Err(error @ Error::PlaylistEditRejected { .. }) => error,
            Err(error) => return Err(error),
        };
        // YouTube rejects a whole batch when one song is already present.
        // Inspect only after rejection so ordinary saves need no extra lookup.
        let existing_video_ids = self.playlist_members(playlist_id).await?;
        let missing_video_ids: Vec<String> = video_ids
            .iter()
            .filter(|video_id| !existing_video_ids.contains(*video_id))
            .cloned()
            .collect();
        if missing_video_ids.is_empty() {
            return Ok(PlaylistAdd::AlreadyPresent);
        }
        if missing_video_ids.len() == video_ids.len() {
            return Err(edit_rejection);
        }
        self.edit_playlist(playlist_id, add_playlist_actions(&missing_video_ids))
            .await
            .map(|()| PlaylistAdd::Added)
    }

    async fn remove_playlist_entry(
        &self,
        playlist_id: &str,
        video_id: &str,
        entry: &PlaylistEntry,
    ) -> Result<()> {
        let entries = [(video_id, entry)];
        self.remove_playlist_entries(playlist_id, &entries).await
    }

    async fn remove_all_from_playlist(&self, playlist: &Playlist, video_id: &str) -> Result<PlaylistRemoval> {
        let page = self.playlist_page(playlist).await?;
        let page = collect_page(self, page)
            .await
            .map_err(|incomplete| incomplete.error)?
            .into_page();
        let Some(entries) = playlist_entries_to_remove(&page, video_id)? else {
            return Ok(PlaylistRemoval::AlreadyAbsent);
        };
        let entries_to_remove: Vec<(&str, &PlaylistEntry)> = entries
            .iter()
            .map(|(video_id, entry)| (video_id.as_str(), entry))
            .collect();
        let occurrences = entries_to_remove.len();
        self.remove_playlist_entries(&playlist.playlist_id, &entries_to_remove)
            .await?;
        Ok(PlaylistRemoval::Removed { occurrences })
    }

    async fn create_playlist(&self, title: &str, video_ids: &[String]) -> Result<String> {
        if !self.signed_in() {
            return Err(Error::SignedOut);
        }
        let body = create_playlist_body(title, video_ids);
        let response = self.transport.post("playlist/create", body).await?;
        created_playlist_id(&response)
            .map(str::to_string)
            .ok_or_else(|| Error::Other(format!("YouTube did not make the playlist {title}")))
    }

    async fn track_length(&self, video_id: &str) -> Result<Option<u32>> {
        let Ok(response) = self.transport.player(video_id).await else {
            return Ok(None);
        };
        Ok(length_seconds(&response))
    }

    async fn accounts(&self) -> Result<Vec<Account>> {
        if !self.signed_in() {
            return Err(Error::SignedOut);
        }
        let response = self.transport.post("account/account_menu", json!({})).await?;
        let authuser = self
            .transport
            .session()
            .map(|session| session.identity().authuser())
            .unwrap_or(0);
        Ok(accounts_in(&response, authuser))
    }
}

/// `videoDetails.lengthSeconds` of a player response, which YouTube sends as a
/// quoted decimal. A live stream reports zero, which is not a length.
fn length_seconds(response: &Value) -> Option<u32> {
    let duration_value = response.get("videoDetails")?.get("lengthSeconds")?;
    let duration_seconds = match duration_value.as_str() {
        Some(text) => text.parse().ok()?,
        None => u32::try_from(duration_value.as_u64()?).ok()?,
    };
    (duration_seconds > 0).then_some(duration_seconds)
}

/// Where the track shelf of `response` carries on, `None` when it is whole.
fn continuation_of(response: &Value, browse_id: &str) -> Option<Continuation> {
    innertube::shelf_token(response).map(|token| Continuation {
        browse_id: browse_id.to_string(),
        token,
    })
}

/// The tracks of the one shelf a page of tracks holds, plus every track in the
/// continuations, which arrive with no shelf of their own.
fn tracks_of(responses: &[Value]) -> Vec<Track> {
    let mut tracks = Vec::new();
    for (response_index, response) in responses.iter().enumerate() {
        let response_tracks: Vec<Track> = if response_index == 0 {
            walker::shelves(response, "Tracks")
                .into_iter()
                .find(|shelf| shelf.tracks().next().is_some())
                .map(|shelf| shelf.tracks().cloned().collect())
                .unwrap_or_default()
        } else {
            walker::items(response)
                .iter()
                .filter_map(Item::as_track)
                .cloned()
                .collect()
        };
        tracks.extend(response_tracks);
    }
    tracks
}

/// The home feed as the app shows it, empty rows dropped.
fn home_from(responses: &[Value], rows: usize) -> Page {
    let mut shelves: Vec<Shelf> = responses
        .iter()
        .flat_map(|response| walker::shelves(response, "For you"))
        .filter(|shelf| !shelf.items.is_empty())
        .collect();
    shelves.truncate(rows);
    Page::new("Home", shelves)
}

/// YouTube's own category labels are inconsistent ("Results" one day, "Songs"
/// the next), so results are shelved by kind, keeping only its "Top result" call.
fn search_from(response: &Value, query: &str) -> Page {
    let search_items = walker::items(response);
    let mut shelves: Vec<Shelf> = Vec::new();
    let mut remaining_items = search_items.as_slice();
    if has_key(response, "musicCardShelfRenderer")
        && let Some((top_result, remaining_results)) = search_items.split_first()
    {
        shelves.push(Shelf::new("Top result", vec![top_result.clone()]));
        remaining_items = remaining_results;
    }
    for item in remaining_items {
        let title = kind_shelf(item);
        match shelves.iter_mut().find(|shelf| shelf.title == title) {
            Some(shelf) => shelf.items.push(item.clone()),
            None => shelves.push(Shelf::new(title, vec![item.clone()])),
        }
    }
    Page::new(format!("Search: {query}"), shelves)
}

/// Album tracks carry no artist, album or art of their own, so they inherit the
/// page's.
fn album_from(responses: &[Value], album: &Album) -> Page {
    let Some(initial_response) = responses.first() else {
        return Page::default();
    };
    let header = Header::of(initial_response);
    let album_thumbnail_url = header.thumbnail.clone().or_else(|| album.thumbnail_url.clone());
    let title = header.title.clone().unwrap_or_else(|| album.title.clone());
    let tracks: Vec<Item> = tracks_of(responses)
        .into_iter()
        .map(|track| {
            Item::Track(Track {
                album: track.album.or_else(|| Some(title.clone())),
                album_browse_id: track.album_browse_id.or_else(|| Some(album.browse_id.clone())),
                thumbnail_url: track.thumbnail_url.or_else(|| album_thumbnail_url.clone()),
                artists: if track.artists.is_empty() {
                    album.artists.clone()
                } else {
                    track.artists
                },
                ..track
            })
        })
        .collect();
    Page {
        title,
        shelves: vec![Shelf::new("Tracks", tracks)],
        subtitle: header.subtitle,
        thumbnail_url: album_thumbnail_url,
        ..Page::default()
    }
}

fn playlist_from(responses: &[Value], playlist: &Playlist) -> Page {
    let Some(initial_response) = responses.first() else {
        return Page::default();
    };
    let header = Header::of(initial_response);
    let tracks: Vec<Item> = tracks_of(responses).into_iter().map(Item::Track).collect();
    Page {
        title: header.title.unwrap_or_else(|| playlist.title.clone()),
        shelves: vec![Shelf::new("Tracks", tracks)],
        subtitle: header.subtitle,
        thumbnail_url: header.thumbnail.or_else(|| playlist.thumbnail_url.clone()),
        ..Page::default()
    }
}

fn artist_from(response: &Value, artist: &Artist) -> Page {
    let header = Header::of(response);
    let mut shelves = walker::shelves(response, "Songs");
    shelves.retain(|shelf| !shelf.items.is_empty());
    Page {
        title: header.title.unwrap_or_else(|| artist.name.clone()),
        shelves,
        subtitle: header.subtitle,
        thumbnail_url: header.thumbnail.or_else(|| artist.thumbnail_url.clone()),
        ..Page::default()
    }
}

/// The one titled block above a page's items: an album, playlist or artist head.
struct Header {
    title: Option<String>,
    subtitle: Option<String>,
    thumbnail: Option<String>,
}

/// Shelves carry headers of their own, so the page header is looked for outside
/// them.
const NOT_A_PAGE_HEADER: [&str; 5] = [
    "musicCarouselShelfRenderer",
    "musicShelfRenderer",
    "musicPlaylistShelfRenderer",
    "musicCardShelfRenderer",
    "gridRenderer",
];

impl Header {
    fn of(response: &Value) -> Header {
        let Some(header_renderer) = page_header(response) else {
            return Header {
                title: None,
                subtitle: None,
                thumbnail: None,
            };
        };
        let header_text = |key: &str| header_renderer.get(key).and_then(walker::text);
        let subtitle_parts: Vec<String> = ["straplineTextOne", "subtitle", "secondSubtitle"]
            .iter()
            .filter_map(|key| header_text(key))
            .collect();
        Header {
            title: header_text("title"),
            subtitle: (!subtitle_parts.is_empty()).then(|| subtitle_parts.join(" \u{b7} ")),
            thumbnail: walker::thumbnail(header_renderer),
        }
    }
}

fn page_header(node: &Value) -> Option<&Value> {
    match node {
        Value::Array(entries) => entries.iter().find_map(page_header),
        Value::Object(object) => {
            for (key, value) in object {
                if key.ends_with("HeaderRenderer") && !key.contains("Shelf") {
                    return Some(value);
                }
            }
            object
                .iter()
                .filter(|(key, _)| !NOT_A_PAGE_HEADER.contains(&key.as_str()))
                .find_map(|(_, value)| page_header(value))
        }
        _ => None,
    }
}

fn kind_shelf(item: &Item) -> &'static str {
    match item {
        Item::Track(_) => "Songs",
        Item::Album(_) => "Albums",
        Item::Artist(_) => "Artists",
        Item::Playlist(_) => "Playlists",
    }
}

fn has_key(node: &Value, required_key: &str) -> bool {
    match node {
        Value::Array(entries) => entries.iter().any(|entry| has_key(entry, required_key)),
        Value::Object(object) => {
            object.contains_key(required_key) || object.values().any(|value| has_key(value, required_key))
        }
        _ => false,
    }
}

/// The lyrics tab's browse id, which YouTube prefixes `MPLY`.
fn lyrics_browse_id(response: &Value) -> Option<String> {
    let mut lyrics_page_id = None;
    find_prefixed(response, "browseId", "MPLY", &mut lyrics_page_id);
    lyrics_page_id
}

fn find_prefixed(node: &Value, key: &str, prefix: &str, matching_text: &mut Option<String>) {
    if matching_text.is_some() {
        return;
    }
    match node {
        Value::Array(entries) => {
            for entry in entries {
                find_prefixed(entry, key, prefix, matching_text);
            }
        }
        Value::Object(object) => {
            if let Some(value) = object.get(key).and_then(Value::as_str)
                && value.starts_with(prefix)
            {
                *matching_text = Some(value.to_string());
                return;
            }
            for value in object.values() {
                find_prefixed(value, key, prefix, matching_text);
            }
        }
        _ => {}
    }
}

/// Plain lyrics, from the description shelf or from the timed lines the mobile
/// client is served.
fn lyrics_text(response: &Value) -> Option<String> {
    let mut description = None;
    description_text(response, &mut description);
    if description.is_some() {
        return description;
    }
    let mut timed = None;
    timed_lyrics(response, &mut timed);
    timed
}

fn description_text(node: &Value, description: &mut Option<String>) {
    if description.is_some() {
        return;
    }
    match node {
        Value::Array(entries) => {
            for entry in entries {
                description_text(entry, description);
            }
        }
        Value::Object(object) => {
            if let Some(text) = object.get("description").and_then(walker::text) {
                *description = Some(text);
                return;
            }
            for value in object.values() {
                description_text(value, description);
            }
        }
        _ => {}
    }
}

fn timed_lyrics(node: &Value, lyrics_text: &mut Option<String>) {
    if lyrics_text.is_some() {
        return;
    }
    match node {
        Value::Array(entries) => {
            for entry in entries {
                timed_lyrics(entry, lyrics_text);
            }
        }
        Value::Object(object) => {
            if let Some(lines) = object.get("timedLyricsData").and_then(Value::as_array) {
                let lyric_lines: Vec<&str> = lines
                    .iter()
                    .filter_map(|line| line.get("lyricLine").and_then(Value::as_str))
                    .collect();
                if !lyric_lines.is_empty() {
                    *lyrics_text = Some(lyric_lines.join("\n"));
                    return;
                }
            }
            for value in object.values() {
                timed_lyrics(value, lyrics_text);
            }
        }
        _ => {}
    }
}

/// Every line of the first `timedLyricsData` array anywhere in the document.
///
/// An entry missing its text or either end of its window is dropped, because a
/// guessed timing shows the wrong line for the rest of the song.
fn timed_lyric_lines(response: &Value) -> Vec<LyricLine> {
    let mut timed_lyrics_data = None;
    first_array(response, "timedLyricsData", &mut timed_lyrics_data);
    timed_lyrics_data
        .map(|lines| lines.iter().filter_map(lyric_line).collect())
        .unwrap_or_default()
}

fn lyric_line(entry: &Value) -> Option<LyricLine> {
    let cue_range = entry.get("cueRange")?;
    Some(LyricLine {
        start_ms: milliseconds(cue_range.get("startTimeMilliseconds")?)?,
        end_ms: milliseconds(cue_range.get("endTimeMilliseconds")?)?,
        text: entry.get("lyricLine").and_then(Value::as_str)?.to_string(),
    })
}

/// YouTube writes these as decimal strings. A number is read too, so the day
/// it stops quoting them costs nothing.
fn milliseconds(node: &Value) -> Option<u64> {
    node.as_u64().or_else(|| node.as_str()?.parse().ok())
}

fn first_array<'a>(node: &'a Value, key: &str, matching_array: &mut Option<&'a Vec<Value>>) {
    if matching_array.is_some() {
        return;
    }
    match node {
        Value::Array(entries) => {
            for entry in entries {
                first_array(entry, key, matching_array);
            }
        }
        Value::Object(object) => {
            if let Some(array) = object.get(key).and_then(Value::as_array) {
                *matching_array = Some(array);
                return;
            }
            for value in object.values() {
                first_array(value, key, matching_array);
            }
        }
        _ => {}
    }
}

/// Every identity the account switcher lists, or the signed-in one when the
/// response carries no switcher.
fn accounts_in(response: &Value, authuser: u32) -> Vec<Account> {
    let mut account_items = Vec::new();
    collect_key(response, "accountItem", &mut account_items);
    let available_accounts: Vec<Account> = account_items
        .into_iter()
        .filter_map(|item| {
            let name = item.get("accountName").and_then(walker::text)?;
            let mut page_id_tokens = Vec::new();
            collect_key(item, "pageIdToken", &mut page_id_tokens);
            Some(Account {
                authuser,
                page_id: page_id_tokens
                    .iter()
                    .find_map(|token| token.get("pageId").and_then(Value::as_str))
                    .map(str::to_string),
                name,
                handle: item.get("channelHandle").and_then(walker::text),
            })
        })
        .collect();
    if !available_accounts.is_empty() {
        return available_accounts;
    }
    active_identity(response)
        .map(|identity| {
            vec![Account {
                authuser,
                page_id: None,
                name: identity.name,
                handle: identity.handle,
            }]
        })
        .unwrap_or_default()
}

struct Identity {
    name: String,
    handle: Option<String>,
}

fn active_identity(response: &Value) -> Option<Identity> {
    let mut headers = Vec::new();
    collect_key(response, "activeAccountHeaderRenderer", &mut headers);
    let header = headers.first()?;
    Some(Identity {
        name: header.get("accountName").and_then(walker::text)?,
        handle: header.get("channelHandle").and_then(walker::text),
    })
}

fn collect_key<'a>(node: &'a Value, key: &str, matching_objects: &mut Vec<&'a Value>) {
    match node {
        Value::Array(entries) => {
            for entry in entries {
                collect_key(entry, key, matching_objects);
            }
        }
        Value::Object(object) => {
            if let Some(matching_object) = object.get(key).filter(|value| value.is_object()) {
                matching_objects.push(matching_object);
            }
            for value in object.values() {
                collect_key(value, key, matching_objects);
            }
        }
        _ => {}
    }
}

fn scrub_response(response: &Value) -> Value {
    redwax_auth::diagnostics::redact_response(response)
}

fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(destination_path: &Path, contents: &[u8]) -> std::io::Result<()> {
    redwax_auth::diagnostics::write_private(destination_path, contents)
}

fn trim_dumps(dump_directory: &Path) {
    let Ok(entries) = std::fs::read_dir(dump_directory) else {
        return;
    };
    let mut dumps: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "json"))
        .filter_map(|entry| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600));
            }
            let modified_at = entry.metadata().ok()?.modified().ok()?;
            Some((modified_at, entry.path()))
        })
        .collect();
    if dumps.len() <= MAX_DUMPS {
        return;
    }
    dumps.sort_by_key(|(modified_at, _)| *modified_at);
    for (_, path) in &dumps[..dumps.len() - MAX_DUMPS] {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    fn album(browse_id: &str) -> Album {
        Album {
            browse_id: browse_id.to_string(),
            title: "placeholder".to_string(),
            artists: vec![],
            year: None,
            thumbnail_url: None,
        }
    }

    /// The Python `test_album_track_inherits_album_context`, ported.
    #[test]
    fn test_album_track_inherits_album_context() {
        let page = album_from(&[fixture("album.json")], &album("MPREb_PITqkpE6ExP"));
        assert_eq!(page.title, "(What's The Story) Morning Glory?");
        assert!(page.thumbnail_url.is_some());
        assert!(
            page.subtitle
                .as_deref()
                .is_some_and(|line| line.contains("Oasis")),
            "{:?}",
            page.subtitle
        );

        let tracks: Vec<&Track> = page.tracks().collect();
        assert_eq!(tracks.len(), 12);
        assert_eq!(tracks[0].title, "Hello");
        assert_eq!(tracks[0].duration_seconds, Some(204));
        for track in &tracks {
            assert_eq!(track.album.as_deref(), Some("(What's The Story) Morning Glory?"));
            assert_eq!(track.album_browse_id.as_deref(), Some("MPREb_PITqkpE6ExP"));
            assert_eq!(track.thumbnail_url, page.thumbnail_url);
            assert!(
                track.duration_seconds.is_some(),
                "{} has no duration",
                track.title
            );
        }
    }

    #[test]
    fn a_track_of_its_own_keeps_what_it_carries() {
        let carried = json!({
            "contents": { "musicShelfRenderer": { "contents": [{
                "musicResponsiveListItemRenderer": {
                    "playlistItemData": { "videoId": "x" },
                    "flexColumns": [
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "T" },
                        ] } } },
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Eminem", "navigationEndpoint": {
                                "browseEndpoint": { "browseId": "UC5" },
                            } },
                            { "text": " \u{2022} " },
                            { "text": "Revival", "navigationEndpoint": {
                                "browseEndpoint": { "browseId": "MPREb_own" },
                            } },
                        ] } } },
                    ],
                    "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                        { "url": "https://art=w60-h60", "width": 60, "height": 60 },
                    ] } } },
                }
            }] } }
        });
        let page = album_from(&[carried], &album("MPREb_page"));
        let track = page.tracks().next().expect("one track");
        assert_eq!(track.album.as_deref(), Some("Revival"));
        assert_eq!(track.album_browse_id.as_deref(), Some("MPREb_own"));
        assert_eq!(track.artists, vec!["Eminem".to_string()]);
        assert_eq!(track.thumbnail_url.as_deref(), Some("https://art=w544-h544"));
    }

    #[test]
    fn home_drops_empty_rows_and_stops_at_the_row_count() {
        let page = home_from(&[fixture("home.json"), fixture("home_continuation.json")], 4);
        assert_eq!(page.title, "Home");
        assert_eq!(page.shelves.len(), 4);
        assert_eq!(page.shelves[0].title, "Listen again");
        assert!(page.shelves.iter().all(|shelf| !shelf.items.is_empty()));
        assert!(page.shelves.iter().all(|shelf| !shelf.title.is_empty()));
    }

    #[test]
    fn search_shelves_by_kind_with_the_top_result_first() {
        let page = search_from(&fixture("search.json"), "oasis wonderwall");
        assert_eq!(page.title, "Search: oasis wonderwall");
        assert_eq!(page.shelves[0].title, "Top result");
        assert_eq!(page.shelves[0].items.len(), 1);

        let titles: Vec<&str> = page.shelves.iter().map(|shelf| shelf.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Top result", "Songs", "Artists", "Playlists", "Albums"]
        );
        for shelf in &page.shelves[1..] {
            let kinds: Vec<&str> = shelf.items.iter().map(kind_shelf).collect();
            assert!(
                kinds.iter().all(|kind| *kind == shelf.title),
                "{} holds {kinds:?}",
                shelf.title
            );
        }
    }

    #[test]
    fn playlist_keeps_the_header_and_every_track() {
        let page = playlist_from(&[fixture("playlist.json")], &fixture_playlist());
        assert_ne!(page.title, "placeholder");
        assert!(page.tracks().count() >= 20, "{} tracks", page.tracks().count());
        assert!(page.thumbnail_url.is_some());
    }

    /// The fixture is the first browse response of a playlist longer than one
    /// page, trimmed to twenty rows plus the token that closes the shelf.
    #[test]
    fn a_long_playlist_page_names_where_it_carries_on() {
        let response = fixture("playlist.json");
        let page = playlist_from(std::slice::from_ref(&response), &fixture_playlist());
        let more = continuation_of(&response, "VLRDCLAK5uy_nvRGXHpFzon8qbPTZhuoSq3WO4ccYvmHM")
            .expect("the page is partial");
        assert_eq!(more.browse_id, "VLRDCLAK5uy_nvRGXHpFzon8qbPTZhuoSq3WO4ccYvmHM");
        assert!(!more.token.is_empty());
        assert!(page.tracks().count() >= 20, "{} tracks", page.tracks().count());
    }

    /// The response the capture stored for that token, which is what
    /// `continue_page` parses.
    #[test]
    fn a_stored_continuation_gives_more_tracks() {
        let response = fixture("playlist.json");
        let first: Vec<String> = playlist_from(std::slice::from_ref(&response), &fixture_playlist())
            .tracks()
            .map(|track| track.video_id.clone())
            .collect();
        let stored = response
            .get("continuationResponses")
            .and_then(|node| node.get(0))
            .expect("the capture stored the first continuation")
            .clone();

        let items = walker::items(&stored);
        let ids: Vec<&str> = items
            .iter()
            .filter_map(Item::as_track)
            .map(|track| track.video_id.as_str())
            .collect();
        assert!(ids.len() >= 20, "{} tracks", ids.len());
        assert!(ids.iter().all(|id| !first.iter().any(|seen| seen == id)));
    }

    fn fixture_playlist() -> Playlist {
        Playlist {
            playlist_id: "RDCLAK5uy_nvRGXHpFzon8qbPTZhuoSq3WO4ccYvmHM".to_string(),
            title: "placeholder".to_string(),
            subtitle: None,
            thumbnail_url: None,
        }
    }

    #[test]
    fn artist_keeps_youtubes_own_shelf_order() {
        let artist = Artist {
            channel_id: "UCmMUZbaYdNH0bEd1PAlAqsA".to_string(),
            name: "placeholder".to_string(),
            thumbnail_url: None,
        };
        let page = artist_from(&fixture("artist.json"), &artist);
        assert_eq!(page.title, "Oasis");
        let titles: Vec<&str> = page.shelves.iter().map(|shelf| shelf.title.as_str()).collect();
        assert_eq!(titles[0], "Top songs");
        assert!(titles.contains(&"Albums"), "{titles:?}");
        assert!(!titles.contains(&"Podcasts"), "{titles:?}");
    }

    #[test]
    fn lyrics_come_out_of_the_description_shelf() {
        let text = lyrics_text(&fixture("lyrics.json")).expect("lyrics text");
        assert_eq!(text, "Synthetic fixture lyrics\nA second sample line");
        assert_eq!(lyrics_text(&json!({ "contents": {} })), None);
    }

    #[test]
    fn timed_lyrics_are_read_when_the_plain_ones_are_absent() {
        let response = json!({
            "contents": { "elementRenderer": { "lyricsData": { "timedLyricsData": [
                { "lyricLine": "one" },
                { "lyricLine": "two" },
            ] } } }
        });
        assert_eq!(lyrics_text(&response).as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn timed_lyrics_fixture_parses() {
        let lines = timed_lyric_lines(&fixture("lyrics_timed.json"));
        assert_eq!(lines.len(), 24);
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[1].start_ms, 32_800);
        assert_eq!(lines[1].text, "Synthetic lyric line 2");
        assert!(lines.windows(2).all(|pair| pair[0].start_ms <= pair[1].start_ms));
        assert!(lines.iter().all(|line| line.end_ms >= line.start_ms));
    }

    #[test]
    fn a_line_missing_its_window_is_dropped() {
        let response = json!({ "model": { "lyricsData": { "timedLyricsData": [
            { "lyricLine": "first", "cueRange": {
                "startTimeMilliseconds": "0", "endTimeMilliseconds": "900" } },
            { "lyricLine": "no window" },
            { "cueRange": { "startTimeMilliseconds": "900", "endTimeMilliseconds": "1800" } },
            { "lyricLine": "second", "cueRange": {
                "startTimeMilliseconds": 1800, "endTimeMilliseconds": 2700 } },
        ] } } });
        let lines = timed_lyric_lines(&response);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "first");
        assert_eq!(
            lines[1],
            LyricLine {
                start_ms: 1800,
                end_ms: 2700,
                text: "second".to_string(),
            }
        );
        assert!(timed_lyric_lines(&json!({ "contents": {} })).is_empty());
    }

    #[test]
    fn the_lyrics_tab_browse_id_is_the_one_prefixed_mply() {
        let found = lyrics_browse_id(&fixture("watch_playlist_plain.json"));
        assert_eq!(found.as_deref(), Some("MPLYt_TB25Tyi00y7-7"));
    }

    #[test]
    fn session_reads_the_headers_login_saved() {
        let dir = std::env::temp_dir().join(format!("redwax-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("browser.json");
        std::fs::write(
            &path,
            r#"{"cookie":"SAPISID=a; PREF=b","x-goog-authuser":"1","x-goog-pageid":"12345"}"#,
        )
        .expect("write");
        let session = Session::read(&path).expect("readable").expect("a session");
        assert_eq!(session.identity().authuser(), 1);
        assert_eq!(session.identity().page_id(), Some("12345"));
        assert!(session.cookie_header().contains("SAPISID=a"));

        std::fs::write(&path, r#"{"x-goog-authuser":"0"}"#).expect("write");
        assert!(Session::read(&path).is_err());
        assert_eq!(Session::read(&dir.join("missing.json")).expect("readable"), None);
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn an_unreadable_page_is_dumped_and_the_oldest_dumps_are_dropped() {
        let dir = std::env::temp_dir().join(format!("redwax-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for index in 0..MAX_DUMPS + 3 {
            std::fs::write(dir.join(format!("home-{index}.json")), "{}").expect("stale dump");
        }

        let client = Client::new(ClientOptions {
            session: None,
            dump_dir: dir.clone(),
            home_rows: 6,
        });
        let response = json!({
            "empty": true,
            "nested": {
                "trackingParams": "track-secret",
                "visitorData": "visitor-secret",
                "continuation": "continuation-secret",
                "continuationEndpoint": {"continuationCommand": {"token": "capability-secret"}},
                "playlistSetVideoId": "entry-secret",
                "title": "kept",
            }
        });
        let Error::Empty { page, dump } = client.dump("home", &[response]) else {
            panic!("dump returns Empty");
        };
        assert_eq!(page, "home");
        assert!(dump.exists(), "{}", dump.display());
        let text = std::fs::read_to_string(&dump).expect("the dump is readable");
        assert!(!text.contains("kept"));
        assert!(text.contains("[redacted]"));
        for secret in [
            "track-secret",
            "visitor-secret",
            "continuation-secret",
            "capability-secret",
            "entry-secret",
        ] {
            assert!(!text.contains(secret));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir)
                    .expect("dump directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&dump)
                    .expect("dump metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(&dir).expect("dump dir").count(), MAX_DUMPS);
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}

#[cfg(test)]
mod length_tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    #[test]
    fn test_length_seconds_reads_the_quoted_decimal() {
        let response = json!({ "videoDetails": { "lengthSeconds": "241" } });
        assert_eq!(length_seconds(&response), Some(241));
        assert_eq!(
            length_seconds(&json!({ "videoDetails": { "lengthSeconds": 241 } })),
            Some(241)
        );
        assert_eq!(
            length_seconds(&json!({ "videoDetails": { "lengthSeconds": "0" } })),
            None
        );
        assert_eq!(length_seconds(&json!({ "videoDetails": {} })), None);
        assert_eq!(length_seconds(&json!({})), None);
    }

    #[test]
    fn test_length_seconds_reads_a_captured_player_response() {
        let response = fixture("player_web_remix_high.json");
        assert_eq!(length_seconds(&response), Some(241));
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;

    fn accounts() -> Vec<Account> {
        vec![
            Account {
                authuser: 1,
                page_id: None,
                name: "Sam".into(),
                handle: Some("@sam".into()),
            },
            Account {
                authuser: 1,
                page_id: Some("p".into()),
                name: "Brand".into(),
                handle: None,
            },
        ]
    }

    #[test]
    fn test_find_account_by_number_handle_or_name() {
        let all = accounts();
        assert_eq!(find_account(&all, "2").map(|a| a.name.as_str()), Some("Brand"));
        assert_eq!(find_account(&all, "@sam").map(|a| a.name.as_str()), Some("Sam"));
        assert_eq!(find_account(&all, "sam").map(|a| a.name.as_str()), Some("Sam"));
        assert_eq!(
            find_account(&all, "brand").map(|a| a.name.as_str()),
            Some("Brand")
        );
        assert!(find_account(&all, "0").is_none());
        assert!(find_account(&all, "nobody").is_none());
        for query in ["", "   ", "@", " @ "] {
            assert!(find_account(&all, query).is_none());
        }
    }
}

#[cfg(test)]
mod playlist_edit_tests {
    use super::*;

    fn row(video_id: &str, entry: Option<&str>) -> Item {
        Item::Track(Track {
            video_id: video_id.to_owned(),
            title: video_id.to_owned(),
            artists: Vec::new(),
            album: None,
            album_browse_id: None,
            duration_seconds: None,
            thumbnail_url: None,
            liked: false,
            playlist_entry: entry.map(str::to_owned).map(PlaylistEntry::new),
        })
    }

    #[test]
    fn an_add_names_every_video_once() {
        let ids = ["v1".to_string(), "v2".to_string()];
        let actions = ids
            .iter()
            .map(|video_id| json!({ "action": "ACTION_ADD_VIDEO", "addedVideoId": video_id }))
            .collect();
        let body = edit_playlist_body("VLPL123", actions);
        assert_eq!(body["playlistId"], "PL123", "the VL prefix reached the edit");
        let actions = body["actions"].as_array().expect("the body carries actions");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["action"], "ACTION_ADD_VIDEO");
        assert_eq!(actions[0]["addedVideoId"], "v1");
        assert_eq!(actions[1]["addedVideoId"], "v2");
    }

    #[test]
    fn a_removal_names_the_row_and_the_video() {
        let actions = vec![json!({
            "action": "ACTION_REMOVE_VIDEO",
            "removedVideoId": "v1",
            "setVideoId": "s1",
        })];
        let body = edit_playlist_body("PL123", actions);
        assert_eq!(body["playlistId"], "PL123");
        let action = &body["actions"][0];
        assert_eq!(action["action"], "ACTION_REMOVE_VIDEO");
        assert_eq!(action["removedVideoId"], "v1");
        assert_eq!(
            action["setVideoId"], "s1",
            "without the set video id YouTube cannot name the row"
        );
    }

    #[test]
    fn semantic_removal_preserves_duplicate_occurrences() {
        let page = Page::new(
            "Playlist",
            vec![Shelf::new(
                "Tracks",
                vec![
                    row("same", Some("first")),
                    row("other", Some("other")),
                    row("same", Some("second")),
                ],
            )],
        );

        let entries = playlist_entries_to_remove(&page, "same")
            .expect("the playlist is editable")
            .expect("the video is present");
        assert_eq!(entries.len(), 2);
        assert!(
            playlist_entries_to_remove(&page, "missing")
                .expect("absence is an ordinary result")
                .is_none()
        );
    }

    #[test]
    fn semantic_removal_refuses_a_matching_row_without_edit_authority() {
        let page = Page::new(
            "Playlist",
            vec![Shelf::new(
                "Tracks",
                vec![row("same", Some("first")), row("same", None)],
            )],
        );

        assert!(matches!(
            playlist_entries_to_remove(&page, "same"),
            Err(Error::PlaylistNotEditable)
        ));
    }

    #[test]
    fn a_new_playlist_is_private_and_holds_its_tracks_in_order() {
        let body = create_playlist_body("Road trip", &["v1".to_string(), "v2".to_string()]);
        assert_eq!(body["title"], "Road trip");
        assert_eq!(body["privacyStatus"], "PRIVATE", "a new playlist was not private");
        assert_eq!(body["videoIds"], json!(["v1", "v2"]));
    }

    #[test]
    fn a_created_playlist_is_named_by_the_id_in_the_answer() {
        assert_eq!(
            created_playlist_id(&json!({ "playlistId": "PLnew" })),
            Some("PLnew")
        );
        assert_eq!(created_playlist_id(&json!({ "playlistId": "" })), None);
        assert_eq!(
            created_playlist_id(&json!({ "status": "STATUS_SUCCEEDED" })),
            None
        );
    }

    #[test]
    fn only_a_status_that_is_not_a_success_is_an_error() {
        assert_eq!(edit_status(&json!({ "status": "STATUS_SUCCEEDED" })), None);
        assert_eq!(edit_status(&json!({})), None);
        assert_eq!(
            edit_status(&json!({ "status": "STATUS_FAILED" })),
            Some("STATUS_FAILED")
        );
    }
}

#[cfg(test)]
#[path = "tests/common/mod.rs"]
mod test_common;

#[cfg(test)]
#[path = "tests/fixtures.rs"]
mod fixture_tests;

#[cfg(test)]
#[path = "tests/mutations.rs"]
mod mutation_tests;

#[cfg(test)]
#[path = "tests/ytm_parsing.rs"]
mod parsing_tests;

#[cfg(test)]
#[path = "tests/live.rs"]
mod live_tests;
