//! Terminal UI state driven by input, background completions, and player observations.
//! The cache-backed first frame precedes mpv, MPRIS, network requests, and library sync.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use image::DynamicImage;
use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui::style::Color;
use ratatui_image::Resize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use redwax_player::{PlayerControl, PlayerState, Repeat};
use redwax_ytm::{
    Album, Browsable, CompletePage, Continuation, Item, Lyrics, MusicSource, Page, PagePagination, Playlist,
    PlaylistAdd, PlaylistEntry, PlaylistRemoval, Shelf, Track, collect_page, format_duration,
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

pub use crate::browser::{Browser, BrowserRemoval};
use crate::config::Config;
use crate::icons::Icons;
use crate::keys::{Action, action, list_action};
pub use crate::likes::LikeRequest;
use crate::playback::{Playback, QueueEditPreview, StationEffect, StationEvent};
pub use crate::request::RequestId;
use crate::request::RequestSlot;
use crate::search_editor::{EditOutcome, SearchEditor};
use crate::terminal::Renderer;
use crate::theme::Theme;
use crate::themes::{LiveThemeRequest, ThemeController, ThemeId};
use crate::ui;
use crate::ui::hit::{Hit, PointerIntent, RowTarget};

/// Which bar a drag is on. Both behave the same way, so they are one thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slider {
    Seek,
    Volume,
}

/// The normalized slider value and geometry that owned the press.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scrub {
    pub slider: Slider,
    pub normalized_fraction: f64,
    pub geometry_revision: u64,
    pub area: Rect,
}

/// A one line field that owns the keyboard while it is open.
///
/// One field at a time is the whole point of the type: two of them open would
/// leave every key ambiguous, and an enum makes that unrepresentable rather
/// than something `handle_key` has to keep straight.
#[derive(Debug, Clone)]
pub enum Prompt {
    /// Finding on the open page.
    Find(Find),
    /// Typing a volume, as digits, applied on enter and dropped on escape.
    Volume(String),
    ClearQueue(ClearQueuePrompt),
    Logout(ui::confirm::ConfirmationState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageFocus {
    Browse,
    Search,
    Account,
}

enum Overlay {
    Prompt(Prompt),
    Help { filter_focused: bool },
    PlaylistPicker(Box<PlaylistPicker>),
    Accounts(ui::accounts::AccountPicker),
    Notification(Notification, ui::popup::Scroll),
}

enum Interaction {
    Browse,
    Search,
    Account,
    Overlay {
        previous_focus: PageFocus,
        overlay: Box<Overlay>,
    },
}

impl Interaction {
    fn page_focus(&self) -> PageFocus {
        match self {
            Interaction::Browse => PageFocus::Browse,
            Interaction::Search => PageFocus::Search,
            Interaction::Account => PageFocus::Account,
            Interaction::Overlay { previous_focus, .. } => *previous_focus,
        }
    }

    fn open(&mut self, overlay: Overlay) {
        let previous_focus = self.page_focus();
        *self = Interaction::Overlay {
            previous_focus,
            overlay: Box::new(overlay),
        };
    }

    fn close(&mut self) -> Option<Overlay> {
        let previous_focus = self.page_focus();
        let previous_interaction = std::mem::replace(
            self,
            match previous_focus {
                PageFocus::Browse => Interaction::Browse,
                PageFocus::Search => Interaction::Search,
                PageFocus::Account => Interaction::Account,
            },
        );
        match previous_interaction {
            Interaction::Overlay { overlay, .. } => Some(*overlay),
            Interaction::Browse | Interaction::Search | Interaction::Account => None,
        }
    }

    fn focus(&mut self, page_focus: PageFocus) {
        *self = match page_focus {
            PageFocus::Browse => Interaction::Browse,
            PageFocus::Search => Interaction::Search,
            PageFocus::Account => Interaction::Account,
        };
    }

    fn overlay(&self) -> Option<&Overlay> {
        match self {
            Interaction::Overlay { overlay, .. } => Some(overlay),
            Interaction::Browse | Interaction::Search | Interaction::Account => None,
        }
    }

    fn overlay_mut(&mut self) -> Option<&mut Overlay> {
        match self {
            Interaction::Overlay { overlay, .. } => Some(overlay),
            Interaction::Browse | Interaction::Search | Interaction::Account => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClearQueuePrompt {
    preview: QueueEditPreview,
    confirmation_state: ui::confirm::ConfirmationState,
}

/// How the themes cursor was asked to move, so one method paints the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemesMove {
    By(isize),
    To(usize),
    Edge(bool),
}

/// A find in progress on the open page.
#[derive(Debug, Default, Clone)]
pub struct Find {
    pub query: String,
    /// The row the cursor was on when the bar opened, restored by escape.
    pub original_row: Option<usize>,
    /// False once a keystroke leaves nothing on the page matching.
    pub has_match: bool,
    pub match_position: Option<(usize, usize)>,
}

pub const SIDEBAR_MIN_WIDTH: u16 = ui::layout::SIDEBAR_AT;
pub const WHEEL_ROWS: isize = 3;
pub const SIDEBAR_WIDTH: u16 = 46;
pub const ART_WIDTH: u16 = 44;
/// A square cover at `ART_WIDTH` columns, at the two to one cell most terminal
/// fonts have.
pub const ART_MAX_HEIGHT: u16 = 22;
/// The seam under the header is an underline, so it does not consume a row.
pub const HEADER_HEIGHT: u16 = 1;

pub const CHROME_HEIGHT: u16 = HEADER_HEIGHT + ui::player_bar::PLAYER_BAR_HEIGHT + 1;
pub const PALETTE_POLL_SECONDS: f64 = 2.0;
pub const PREFETCH_REST_MILLIS: u64 = 250;
pub const PREFETCH_TRACKS: usize = 3;
pub const STATE_WRITE_SECONDS: u64 = 30;
/// Tracks a partial page loads back to back before the rest waits for a reason.
/// Below this a user can reach the end of the list before it has arrived.
pub const EAGER_TRACKS: usize = 250;
/// How near the loaded end the cursor has to land to ask for the next batch.
pub const LOAD_AHEAD_ROWS: usize = 100;
/// Idle stretch after which a partial page finishes itself, so a list nobody
/// scrolls still ends up whole.
pub const IDLE_LOAD_MILLIS: u64 = 2_000;
/// Video ids one duration request batch asks about. The batch is the unit the
/// rate gate holds, so a short one keeps a page the user has left from running
/// long past the moment it mattered.
pub const DURATION_CHUNK: usize = 12;
pub const TICK_MILLIS: u64 = 125;
/// The settings file when there is one, the config when there is not. The
/// file is what the panel wrote last, so it outranks a default.
fn starting_eq(config: &Config) -> crate::eq::Eq {
    let eq_path = crate::eq::path_for(config);
    if eq_path.exists() {
        return crate::eq::Eq::read(&eq_path);
    }
    let mut eq = crate::eq::Eq {
        enabled: config.eq_enabled,
        ..crate::eq::Eq::default()
    };
    eq.load(&config.eq_preset);
    eq
}

/// Milliseconds since the first call, which is the process start. Toasts and
/// the prefetch rest are the only things that need a clock, and both want a
/// monotonic one.
pub fn clock() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Home,
    Search,
    Library,
    Queue,
    Lyrics,
    Themes,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 7] = [
        Tab::Home,
        Tab::Search,
        Tab::Library,
        Tab::Queue,
        Tab::Lyrics,
        Tab::Themes,
        Tab::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Home => "home",
            Tab::Search => "search",
            Tab::Library => "library",
            Tab::Queue => "queue",
            Tab::Lyrics => "lyrics",
            Tab::Themes => "themes",
            Tab::Settings => "settings",
        }
    }

    pub fn next(self) -> Tab {
        let current_tab_index = Tab::ALL
            .iter()
            .position(|candidate_tab| *candidate_tab == self)
            .unwrap_or(0);
        Tab::ALL[(current_tab_index + 1) % Tab::ALL.len()]
    }

    pub fn previous(self) -> Tab {
        let current_tab_index = Tab::ALL
            .iter()
            .position(|candidate_tab| *candidate_tab == self)
            .unwrap_or(0);
        Tab::ALL[(current_tab_index + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }

    pub fn target(self) -> Option<Target> {
        match self {
            Tab::Home => Some(Target::Home),
            Tab::Search => Some(Target::Search),
            Tab::Library => Some(Target::Library),
            Tab::Queue => Some(Target::Queue),
            Tab::Lyrics | Tab::Themes | Tab::Settings => None,
        }
    }
}

/// Which browser a background result belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Home,
    Search,
    Library,
    Queue,
}

/// What a background task is asked to fetch. `Target` says where the answer
/// goes, this says what to ask YouTube for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    Home,
    Search(String),
    Library,
    Browse(Browsable),
}

/// Everything that reaches the app from outside a key press.
#[derive(Debug)]
pub enum AppEvent {
    /// Completion carries the exact request that owns its navigation context.
    PageReady {
        target: Target,
        request: RequestId,
        page: Page,
    },
    PageFailed {
        target: Target,
        request: RequestId,
        error: String,
        /// YouTube refused the session, which a refresh from Firefox fixes.
        rejected: bool,
    },
    /// A batch of a page that arrived after the page itself. The items join
    /// the page's last shelf in place, so nothing on screen moves.
    PageMore {
        target: Target,
        request: RequestId,
        key: String,
        result: redwax_ytm::Result<(Vec<Item>, Option<Continuation>)>,
    },
    PagePrefetched {
        key: String,
        request: RequestId,
        page: Option<CompletePage>,
    },
    /// Lengths YouTube answered for rows it served without one.
    DurationsReady(Vec<(String, u32)>),
    /// What the genre lookup came back with for that track, if anything.
    GenreReady {
        video_id: String,
        genre: Option<String>,
    },
    /// Tracks of a browsable fetched for enqueueing.
    TracksReady {
        tracks: Vec<Track>,
        play_next: bool,
    },
    RadioReady {
        station_generation: u64,
        continuation: Option<redwax_ytm::RadioContinuation>,
        seed: Track,
        tracks: Vec<Track>,
        from_playing: bool,
    },
    RadioFailed {
        station_generation: u64,
        error: String,
    },
    RadioApplied {
        station_generation: u64,
        label: String,
        error: Option<String>,
    },
    RadioMore {
        station_generation: u64,
        tracks: Vec<Track>,
        continuation: Option<redwax_ytm::RadioContinuation>,
    },
    RadioMoreFailed {
        station_generation: u64,
        error: String,
        rejected: bool,
    },
    RadioAppended {
        station_generation: u64,
        error: Option<String>,
    },
    QueueCleared {
        result: Result<redwax_player::QueueEdit, String>,
    },
    QueueEntryRemoved {
        result: Result<redwax_player::QueueEdit, String>,
    },
    QueueEntryMoved {
        result: Result<redwax_player::QueueEdit, String>,
    },
    SessionLoggedOut {
        error: Option<String>,
        reauthenticate: bool,
    },
    SessionExpired {
        generation: u64,
    },
    LyricsReady {
        request: RequestId,
        track: Track,
        lyrics: Option<Lyrics>,
    },
    ArtReady {
        request: RequestId,
        video_id: String,
        image: Option<DynamicImage>,
    },
    /// The accent distilled from the art, when `accent_from_art` is on.
    AccentReady {
        request: RequestId,
        video_id: String,
        accent: Option<Color>,
    },
    AccountReady {
        request: RequestId,
        result: redwax_ytm::Result<Option<String>>,
    },
    AccountChoices {
        request: RequestId,
        result: Result<Vec<redwax_ytm::Account>, String>,
    },
    AccountChanged {
        request: RequestId,
        result: Result<String, String>,
    },
    LikeApplied {
        request: LikeRequest,
        error: Option<String>,
    },
    /// YouTube refused a removal the page had already made, so the row goes
    /// back where it was.
    PlaylistRemoveFailed {
        target: Target,
        removal: BrowserRemoval,
        error: String,
    },
    PlaylistTrackRemoved {
        playlist_id: String,
        video_id: String,
        playlist_entry: Option<PlaylistEntry>,
    },
    /// A playlist was made, so the library that lists playlists is stale.
    PlaylistCreated,
    PlayerChanged(PlayerState),
    /// A session refresh finished. `error` is the reason it did not.
    SessionRefreshed {
        error: Option<String>,
        retry: Option<Retry>,
    },
    /// The terminal palette changed while running.
    PaletteChanged {
        request: LiveThemeRequest,
        palette: crate::palette::Palette,
    },
    OmarchyPaletteChanged {
        request: LiveThemeRequest,
        palette: crate::palette::ValidatedPalette,
    },
    Notify {
        text: String,
        title: Option<String>,
        level: Level,
    },
    Tick,
}

/// What a session refresh should try again once it has landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    Account(RequestId),
    Page(Target, RequestId),
    Session(u64),
}

/// Re-imports the session from Firefox and reloads the client. Blocking, so
/// every caller puts it on a blocking task. `None` in tests and when there is
/// no session to refresh.
pub type SessionRefresh = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
pub type SessionLogout = SessionRefresh;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    pub fn seconds(self) -> u64 {
        match self {
            Level::Info => 3,
            Level::Warning => 6,
            Level::Error => 8,
        }
    }
}

/// What a mouse button asks for. Named after the intent rather than the
/// button, because the row and the chrome answer the same three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Click {
    Open,
    Queue,
    PlayNext,
}

/// What `P` is adding, once the picker knows which playlist takes it.
enum Addition {
    /// One track, by video id.
    Track(String),
    /// Every track of an album or a playlist, fetched when the choice is made.
    Whole(Browsable),
}

/// What the picker does to the playlist chosen.
enum Edit {
    Add(Addition),
    /// Take every row of this track out. The row ids are read from the chosen
    /// playlist, because YouTube serves them only on that playlist's own page.
    Remove(Track),
}

/// One thing the picker can edit, named the way a toast names it.
struct Subject {
    label: String,
    edit: Edit,
}

impl Subject {
    /// True when both name the same single track, so the highlighted row is
    /// never offered as a second choice when it is the track that plays.
    fn same_track(&self, alternate_subject: &Subject) -> bool {
        match (&self.edit, &alternate_subject.edit) {
            (Edit::Add(Addition::Track(first_video_id)), Edit::Add(Addition::Track(second_video_id))) => {
                first_video_id == second_video_id
            }
            (Edit::Remove(first_track), Edit::Remove(second_track)) => {
                first_track.video_id == second_track.video_id
            }
            _ => false,
        }
    }
}

/// Longer than any title a person types, short enough that a held key cannot
/// grow the field without end.
const PLAYLIST_NAME_LIMIT: usize = 150;

/// The playlist picker `P` and `D` open. It holds what is being edited, so
/// choosing a playlist is the whole of the answer.
pub struct PlaylistPicker {
    pub playlists: Vec<Playlist>,
    /// A row of the list. The row after the last playlist makes a new one.
    pub selected_playlist_index: usize,
    primary_subject: Subject,
    /// The highlighted row, for `tab` to swap with the track that plays.
    alternate_subject: Option<Subject>,
    pub new_playlist_name_input: Option<String>,
}

impl PlaylistPicker {
    pub fn primary_item_label(&self) -> &str {
        &self.primary_subject.label
    }

    pub fn alternate_item_label(&self) -> Option<&str> {
        self.alternate_subject
            .as_ref()
            .map(|alternate_subject| alternate_subject.label.as_str())
    }

    pub fn remove_mode(&self) -> bool {
        matches!(self.primary_subject.edit, Edit::Remove(_))
    }

    /// Rows the cursor can reach. Removing has no new playlist row, because
    /// nothing can be taken out of a playlist that does not exist yet.
    fn row_count(&self) -> usize {
        self.playlists.len() + usize::from(!self.remove_mode())
    }

    /// The playlist under the cursor, `None` on the new playlist row.
    fn selected_playlist(&self) -> Option<&Playlist> {
        self.playlists.get(self.selected_playlist_index)
    }

    fn swap_subject(&mut self) {
        if let Some(alternate_subject) = self.alternate_subject.as_mut() {
            std::mem::swap(&mut self.primary_subject, alternate_subject);
        }
    }
}

/// A toast, bottom right, a few seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub text: String,
    pub title: Option<String>,
    pub level: Level,
    pub expires_at_millis: u64,
}

#[derive(Clone)]
struct PageFetch {
    fetch: Fetch,
    cache_key: Option<String>,
    replace_current_page: bool,
    page_was_shown: bool,
    retry_attempted: bool,
}

/// One continuation request per visible page.
struct Loader {
    request_id: RequestId,
    /// The page cache key, which is also what a late batch is matched against.
    page_key: String,
    pagination: PagePagination,
    continuation_in_flight: bool,
    /// Items in the page's last shelf, which both loading rules measure against.
    loaded_item_count: usize,
}

/// A page's missing durations, being asked for. Dropped when the page is
/// left, which aborts the task the same moment.
struct Fill {
    task_handle: tokio::task::AbortHandle,
}

impl Drop for Fill {
    fn drop(&mut self) {
        self.task_handle.abort();
    }
}

/// What the app needs beyond the source and the player. Real in `main`, fake
/// in tests.
pub struct Deps {
    pub config: Config,
    pub icons: Icons,
    pub theme: Theme,
    pub page_cache: crate::page_cache::PageCache,
    pub art_cache: Arc<crate::art::ArtCache>,
    pub art_setup: crate::terminal::ArtSetup,
    pub saved: crate::state::SavedState,
    pub state_store: crate::state::PlaybackStateStore,
    pub session_refresh: Option<SessionRefresh>,
    pub session_logout: Option<SessionLogout>,
    pub session_generation: Arc<AtomicU64>,
    pub account_access: Option<Arc<dyn crate::session::AccountAccess>>,
}

/// The art box: the protocol built when an image arrived, and the rows it needs
/// at `ART_WIDTH`. Encoding happens off the draw path, when the image lands.
#[derive(Default)]
pub struct ArtBox {
    request_slot: RequestSlot<String>,
    pub image_picker: Option<Picker>,
    pub render_protocol: Option<Protocol>,
    pub cover_height: u16,
    pub requested_video_id: Option<String>,
    pub accent_color: Option<Color>,
}

#[derive(Clone, Copy)]
enum LoginState {
    Checking,
    SignedIn,
    Unconfirmed,
    SignedOut,
}

impl LoginState {
    fn label(self, identity: &str) -> &str {
        match self {
            Self::Checking => "Checking login",
            Self::SignedIn if !identity.is_empty() => identity,
            Self::SignedIn => "Signed in",
            Self::Unconfirmed => "Login unconfirmed",
            Self::SignedOut => "Sign in",
        }
    }
}

pub struct App<S: MusicSource, P: PlayerControl> {
    pub source: Arc<S>,
    pub player: P,
    pub playback: Playback<P>,
    pub deps: Deps,
    pub events: mpsc::UnboundedSender<AppEvent>,
    pub tab: Tab,
    pub home: Browser,
    pub search: Browser,
    pub library: Browser,
    pub queue: Browser,
    pub search_query: String,
    interaction: Interaction,
    search_editor: SearchEditor,
    pub account: String,
    login_state: LoginState,
    pub player_state: PlayerState,
    pub lyrics: crate::lyrics::LyricsPane,
    pub liked_overrides: HashMap<String, bool>,
    like_intents: crate::likes::Likes,
    pub notifications: Vec<Notification>,
    last_notification: Option<Notification>,
    help_scroll: ui::popup::Scroll,
    help_query: String,
    pub eq: crate::eq::EqPanel,
    pub quit: bool,
    /// What the terminal is showing, and whether it is still true.
    pub screen: crate::screen::Screen,
    /// The hit-map revision that includes the open modal. `None` rejects the
    /// uncovered frame before its first paint.
    modal_pointer_frame_revision: Option<u64>,
    /// The theme the terminal palette gives, before any art accent.
    pub base_theme: Theme,
    theme_controller: ThemeController,
    pub art: ArtBox,
    pub mpris: Arc<Mutex<Option<crate::mpris::Mpris>>>,
    /// Set when the cursor lands on a row, cleared once the prefetch has run.
    pub rested_at: Option<u64>,
    pub last_state_write: u64,
    /// One `g` has landed and the next one takes the cursor to the top.
    /// Any other key drops it, which is what makes `gg` a prefix.
    pub pending_g: bool,
    /// The seek bar the pointer is holding, and where along it. Set on press
    /// and cleared on release, which is the only moment mpv is asked to seek.
    pub scrub: Option<Scrub>,
    /// The themes tab, while it is open. Built when the tab is entered, so a
    /// theme dropped into the directory shows up without a relaunch.
    pub themes: Option<crate::themes::ThemesPane>,
    /// The queue entry the queue page starts at. Every row index on that page
    /// is an offset from here, so a jump or a remove has to add it back.
    pub first_queue_index: usize,
    account_request: RequestSlot<bool>,
    account_choices_request: RequestSlot<()>,
    account_change_request: RequestSlot<()>,
    session_logged_out: bool,
    session_logout_pending: bool,
    /// What each browser last asked for, so a refused fetch can be reissued.
    page_requests: HashMap<Target, RequestSlot<PageFetch>>,
    prefetch_requests: HashMap<String, RequestSlot<()>>,
    /// The partial page each browser is still filling in.
    page_loaders: HashMap<Target, Loader>,
    /// Lengths remembered from earlier visits, shared with the fill tasks.
    duration_cache: Arc<crate::durations::Durations>,
    /// Genres remembered from earlier runs, so a track costs one request ever.
    genre_cache: Arc<crate::genre::Genres>,
    /// How a genre is asked for. A field rather than a call, so the tests
    /// never reach the network.
    genre_lookup: crate::genre::Lookup,
    /// The page each browser is asking YouTube for durations on.
    duration_fill_tasks: HashMap<Target, Fill>,
    /// One duration batch at a time across the whole app, so filling a page
    /// never competes with the page the user is waiting on.
    duration_gate: Arc<tokio::sync::Semaphore>,
    /// When a key or the pointer last reached the app, which is what "idle"
    /// is measured from.
    last_input_millis: u64,
    background_tasks: JoinSet<()>,
}

impl<S: MusicSource, P: PlayerControl> App<S, P> {
    pub fn search_focused(&self) -> bool {
        matches!(self.interaction, Interaction::Search)
    }

    pub fn account_focused(&self) -> bool {
        self.interaction.page_focus() == PageFocus::Account
    }

    pub fn account_picker_visible(&self) -> bool {
        self.account_picker().is_some()
    }

    fn account_picker(&self) -> Option<&ui::accounts::AccountPicker> {
        match self.interaction.overlay() {
            Some(Overlay::Accounts(picker)) => Some(picker),
            _ => None,
        }
    }

    fn account_picker_mut(&mut self) -> Option<&mut ui::accounts::AccountPicker> {
        match self.interaction.overlay_mut() {
            Some(Overlay::Accounts(picker)) => Some(picker),
            _ => None,
        }
    }

    pub fn help_visible(&self) -> bool {
        matches!(self.interaction.overlay(), Some(Overlay::Help { .. }))
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        match self.interaction.overlay() {
            Some(Overlay::Prompt(prompt)) => Some(prompt),
            _ => None,
        }
    }

    pub fn playlist_picker(&self) -> Option<&PlaylistPicker> {
        match self.interaction.overlay() {
            Some(Overlay::PlaylistPicker(picker)) => Some(picker),
            _ => None,
        }
    }

    fn prompt_mut(&mut self) -> Option<&mut Prompt> {
        match self.interaction.overlay_mut() {
            Some(Overlay::Prompt(prompt)) => Some(prompt),
            _ => None,
        }
    }

    fn playlist_picker_mut(&mut self) -> Option<&mut PlaylistPicker> {
        match self.interaction.overlay_mut() {
            Some(Overlay::PlaylistPicker(picker)) => Some(picker),
            _ => None,
        }
    }

    fn take_playlist_picker(&mut self) -> Option<PlaylistPicker> {
        match self.close_overlay() {
            Some(Overlay::PlaylistPicker(picker)) => Some(*picker),
            _ => None,
        }
    }

    fn notification_popup_mut(&mut self) -> Option<(&mut Notification, &mut ui::popup::Scroll)> {
        match self.interaction.overlay_mut() {
            Some(Overlay::Notification(message, scroll)) => Some((message, scroll)),
            _ => None,
        }
    }

    fn help_filter_focused(&self) -> bool {
        matches!(
            self.interaction.overlay(),
            Some(Overlay::Help { filter_focused: true })
        )
    }

    fn set_help_filter_focused(&mut self, focused: bool) {
        if let Some(Overlay::Help { filter_focused }) = self.interaction.overlay_mut() {
            *filter_focused = focused;
        }
    }

    fn open_overlay(&mut self, overlay: Overlay) {
        if let Some(Overlay::Prompt(Prompt::ClearQueue(prompt))) = self.interaction.close() {
            self.playback.cancel_clear(prompt.preview);
        }
        self.interaction.open(overlay);
        self.scrub = None;
        self.modal_pointer_frame_revision = None;
    }

    fn close_overlay(&mut self) -> Option<Overlay> {
        self.interaction.close()
    }

    /// Builds the whole state without touching the network or mpv. The home
    /// and library pages come from the page cache when present, and the queue
    /// from `deps.saved`.
    pub fn new(source: Arc<S>, player: P, deps: Deps, events: mpsc::UnboundedSender<AppEvent>) -> Self {
        let theme_controller = ThemeController::new(&deps.config.theme);
        let presented_theme = theme_controller.present(deps.theme.clone(), deps.config.transparent, None);
        // Read before the first frame: the times have to be on the very first
        // draw of a remembered page, not a repaint later.
        let duration_cache = Arc::new(crate::durations::Durations::new(crate::durations::path_for(
            &deps.config,
        )));
        let equalizer_settings = starting_eq(&deps.config);
        let genre_cache = Arc::new(crate::genre::Genres::new(crate::genre::path_for(&deps.config)));
        let mut restored_queue = deps.saved.queue.clone();
        deps.page_cache.restore_track_metadata(&mut restored_queue);
        duration_cache.fill_tracks(&mut restored_queue);
        let mut player_state = PlayerState {
            queue: restored_queue,
            index: deps.saved.index,
            position: deps.saved.position,
            paused: true,
            volume: deps.saved.volume.unwrap_or(deps.config.volume),
            repeat: deps.saved.repeat,
            shuffle: deps.saved.shuffle,
            muted: deps.saved.muted,
            ..PlayerState::default()
        };
        player_state.duration = player_state
            .track()
            .and_then(|track| track.duration_seconds)
            .map(f64::from);
        let playback = Playback::new(player.clone(), player_state.clone());

        // An empty history is the honest state before the first fetch, and it
        // keeps the fetch from pushing a second page on top of a placeholder.
        let mut home_browser = Browser::default();
        if let Some(mut page) = deps.page_cache.get("home") {
            duration_cache.fill(&mut page);
            home_browser.show(page);
            home_browser.bind_cache_key(Some("home".to_owned()));
        }
        let mut library_browser = Browser::default();
        if let Some(mut page) = deps.page_cache.get("library") {
            duration_cache.fill(&mut page);
            library_browser.show(page);
            library_browser.bind_cache_key(Some("library".to_owned()));
        }
        let mut search_browser = Browser::default();
        let mut blank_search_page = Page::new("Search", Vec::new());
        blank_search_page.subtitle = Some("type above, enter to search".to_string());
        search_browser.show(blank_search_page);
        let mut queue_browser = Browser::default();
        let initial_queue_view = queue_view(&player_state);
        let first_queue_index = initial_queue_view.first_queue_index;
        queue_browser.replace(initial_queue_view.page);
        if let Some(playing_index) = player_state.index {
            let playing_row_offset = playing_index.saturating_sub(first_queue_index);
            let playing_projected_row = queue_browser.rows().iter().position(
                |projected_row| {
                    matches!(projected_row, ui::Row::Item(_, item_index) if *item_index == playing_row_offset)
                },
            );
            queue_browser.select_row(playing_projected_row);
        }

        let art_box = ArtBox {
            request_slot: RequestSlot::default(),
            image_picker: None,
            render_protocol: None,
            cover_height: ART_MAX_HEIGHT,
            requested_video_id: None,
            accent_color: None,
        };

        App {
            source,
            player,
            playback,
            events,
            tab: if player_state.track().is_some() {
                Tab::Queue
            } else {
                Tab::Home
            },
            home: home_browser,
            search: search_browser,
            library: library_browser,
            queue: queue_browser,
            search_query: String::new(),
            interaction: Interaction::Browse,
            search_editor: SearchEditor::new(deps.config.search_vim_mode),
            account: String::new(),
            login_state: LoginState::Checking,
            player_state,
            lyrics: crate::lyrics::LyricsPane::default(),
            liked_overrides: HashMap::new(),
            like_intents: crate::likes::Likes::default(),
            notifications: Vec::new(),
            last_notification: None,
            help_scroll: ui::popup::Scroll::default(),
            help_query: String::new(),
            eq: crate::eq::EqPanel::new(equalizer_settings, deps.config.eq_genre_match),
            quit: false,
            screen: crate::screen::Screen::new(),
            modal_pointer_frame_revision: None,
            base_theme: presented_theme,
            theme_controller,
            art: art_box,
            mpris: Arc::new(Mutex::new(None)),
            rested_at: None,
            last_state_write: clock(),
            pending_g: false,
            scrub: None,
            themes: None,
            first_queue_index,
            account_request: RequestSlot::default(),
            account_choices_request: RequestSlot::default(),
            account_change_request: RequestSlot::default(),
            session_logged_out: false,
            session_logout_pending: false,
            page_requests: HashMap::new(),
            prefetch_requests: HashMap::new(),
            page_loaders: HashMap::new(),
            duration_cache,
            genre_cache,
            genre_lookup: crate::genre::itunes(),
            duration_fill_tasks: HashMap::new(),
            duration_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            last_input_millis: clock(),
            background_tasks: JoinSet::new(),
            deps,
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.base_theme
    }

    pub fn spawn_runtime_task(&mut self, runtime_task: impl Future<Output = ()> + Send + 'static) {
        self.background_tasks.spawn(runtime_task);
    }

    pub fn sidebar_visible(&self) -> bool {
        self.screen.width() >= SIDEBAR_MIN_WIDTH
    }

    /// The animation clock the widgets read. `playback_moving` freezes the equaliser
    /// while the player is paused or still fetching its stream.
    pub fn motion(&self) -> ui::Motion {
        let animations_enabled = self.deps.config.animations;
        let track_is_playing = self.player_state.track().is_some();
        ui::Motion {
            animation_frame: self.screen.animation_frame(),
            animations_enabled,
            playback_moving: animations_enabled
                && track_is_playing
                && !self.player_state.paused
                && !self.player_state.loading,
        }
    }

    /// Whether a tick has anything to advance. A paused or empty player leaves
    /// the frame where it is, which is what keeps an idle redraw at nothing.
    fn animating(&self) -> bool {
        self.deps.config.animations && self.player_state.track().is_some() && !self.player_state.paused
    }

    /// Start everything that was deferred past the first frame: mpv, MPRIS,
    /// the home and library fetches, the account check, the palette poll.
    pub fn start_background(&mut self) {
        let playback = self.playback.clone();
        let event_sender = self.events.clone();
        let restored_volume = self.player_state.volume;
        let restored_repeat = self.player_state.repeat;
        let restored_shuffle = self.player_state.shuffle;
        let restored_muted = self.player_state.muted;
        // A flat equaliser is sent as nothing at all, so mpv runs the plain
        // chain it would have run without redwax.
        let audio_filter_graph = self.eq.graph();
        self.background_tasks.spawn(async move {
            if let Err(error) = playback.start().await {
                let _ = event_sender.send(AppEvent::Notify {
                    text: error.to_string(),
                    title: Some("Playback unavailable".to_string()),
                    level: Level::Error,
                });
                return;
            }
            let _ = playback.set_volume(restored_volume).await;
            let _ = playback.set_repeat(restored_repeat).await;
            let _ = playback.set_shuffle(restored_shuffle).await;
            if restored_muted {
                let _ = playback.toggle_mute().await;
            }
            if let Some(audio_filter_graph) = audio_filter_graph {
                let _ = playback.set_audio_filter(Some(&audio_filter_graph)).await;
            }
        });

        // The state file restored a track without asking mpv to play it, so
        // its art has to be asked for here or the sidebar stays empty.
        if let Some(track) = self.player_state.track().cloned() {
            self.player.prefetch(vec![track.clone()]);
            self.load_art(&track);
            self.start_genre(&track);
        }
        self.start_mpris();
        self.load_page(Target::Home, Some("home".to_string()), false);
        self.start_account();
        let selected_theme = self.theme_controller.selected().clone();
        if let Some(live_theme_request) = self.theme_controller.activate(selected_theme) {
            self.start_palette_poll(live_theme_request);
        }
    }

    /// Media keys and playerctl reach redwax over MPRIS. Without a session bus
    /// there is no service, and a task that fails to join is the same answer.
    fn start_mpris(&mut self) {
        let playback = self.playback.clone();
        let event_sender = self.events.clone();
        let mpris_slot = self.mpris.clone();
        self.background_tasks.spawn(async move {
            let failure_report = match crate::mpris::Mpris::start(playback).await {
                Ok(mpris) => {
                    if let Ok(mut active_mpris) = mpris_slot.lock() {
                        *active_mpris = Some(mpris);
                    }
                    return;
                }
                Err(error) => error,
            };
            let _ = event_sender.send(AppEvent::Notify {
                text: format!("Media keys unavailable: {failure_report}"),
                title: None,
                level: Level::Warning,
            });
        });
    }

    /// Re-import the browser session on a blocking task. Each request
    /// records whether it already used this recovery path, while the shared
    /// session coordinator coalesces refreshes across request kinds.
    fn refresh_session(&mut self, retry: Option<Retry>) -> bool {
        if self.session_logged_out {
            return false;
        }
        let Some(refresh_session) = self.deps.session_refresh.clone() else {
            return false;
        };
        let retry = retry.or_else(|| {
            Some(Retry::Session(
                self.deps.session_generation.load(Ordering::SeqCst),
            ))
        });
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let refresh_task_result = tokio::task::spawn_blocking(move || refresh_session()).await;
            let refresh_error = match refresh_task_result {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(reason),
                Err(_) => Some("the session refresh did not finish".to_string()),
            };
            let _ = event_sender.send(AppEvent::SessionRefreshed {
                error: refresh_error,
                retry,
            });
        });
        true
    }

    fn session_refreshed(&mut self, refresh_error: Option<String>, retry_intent: Option<Retry>) {
        if self.session_logged_out {
            return;
        }
        let current_retry = match retry_intent {
            Some(Retry::Session(generation)) => {
                generation == self.deps.session_generation.load(Ordering::SeqCst)
            }
            Some(Retry::Account(request)) => self.account_request.get(request).is_some(),
            Some(Retry::Page(target, request)) => self
                .page_requests
                .get(&target)
                .is_some_and(|slot| slot.get(request).is_some()),
            None => true,
        };
        if !current_retry {
            return;
        }
        if let Some(refresh_error) = refresh_error {
            self.notify(refresh_error, Some("Session recovery"), Level::Warning);
            self.start_logout(true);
            return;
        }
        self.notify("Session refreshed from your browser", None, Level::Info);
        match retry_intent {
            Some(Retry::Account(request_id)) => self.retry_account(request_id),
            Some(Retry::Page(target, request_id)) => {
                self.retry_fetch(target, request_id);
                self.start_account_request(true);
            }
            Some(Retry::Session(_)) | None => self.start_account_request(true),
        }
    }

    /// Reissue whatever that browser last asked for, which is what the user is
    /// still looking at an empty page of.
    fn retry_fetch(&mut self, target: Target, request_id: RequestId) {
        let Some(fetch_context) = self
            .page_requests
            .get(&target)
            .and_then(|request_slot| request_slot.get(request_id))
            .cloned()
        else {
            return;
        };
        self.fetch_with_retry(
            target,
            fetch_context.fetch,
            fetch_context.cache_key,
            fetch_context.replace_current_page,
            true,
        );
    }

    /// The cursor has moved. A one-shot timer wakes the loop when it has
    /// rested for `PREFETCH_REST_MILLIS`, so the prefetch never waits for the
    /// next periodic tick.
    fn arm_rest(&mut self) {
        if let Some(target) = self.tab.target() {
            self.pump_loader(target);
        }
        self.rested_at = Some(clock());
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(PREFETCH_REST_MILLIS)).await;
            let _ = event_sender.send(AppEvent::Tick);
        });
    }

    fn start_account(&mut self) {
        self.start_account_request(false);
    }

    fn retry_account(&mut self, request_id: RequestId) {
        if self.account_request.get(request_id).is_some() {
            self.start_account_request(true);
        }
    }

    fn start_account_request(&mut self, retry_attempted: bool) {
        if self.source.signed_in() && self.account.is_empty() {
            self.login_state = LoginState::Checking;
        }
        let request_id = self.account_request.begin(retry_attempted);
        let source = self.source.clone();
        let event_sender = self.events.clone();
        let request_task = self.background_tasks.spawn(async move {
            let account_result = source.whoami().await;
            let _ = event_sender.send(AppEvent::AccountReady {
                request: request_id,
                result: account_result,
            });
        });
        self.account_request.hold(request_id, request_task);
    }

    fn open_account_picker(&mut self) {
        if self.session_logout_pending {
            self.notify("Signing out. Please wait.", Some("Account"), Level::Info);
            return;
        }
        self.eq.hide();
        self.interaction.focus(PageFocus::Account);
        let signed_in = self.source.signed_in() && !self.session_logged_out;
        self.open_overlay(Overlay::Accounts(ui::accounts::AccountPicker::loading(signed_in)));
        self.load_account_choices();
    }

    fn load_account_choices(&mut self) {
        let Some(access) = self.deps.account_access.clone() else {
            if let Some(picker) = self.account_picker_mut() {
                picker.status =
                    ui::accounts::Status::Failed("Browser login is unavailable in this session.".to_owned());
            }
            return;
        };
        if let Some(picker) = self.account_picker_mut() {
            picker.status = ui::accounts::Status::Loading;
        }
        let request = self.account_choices_request.begin(());
        let events = self.events.clone();
        let task = self.background_tasks.spawn(async move {
            let result = tokio::task::spawn_blocking(move || access.accounts())
                .await
                .unwrap_or_else(|_| Err("Browser logins could not be read. Try refreshing.".to_owned()));
            let _ = events.send(AppEvent::AccountChoices { request, result });
        });
        self.account_choices_request.hold(request, task);
    }

    fn account_picker_key(&mut self, mut key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::ALT) {
            return;
        }
        let pending_g = std::mem::take(&mut self.pending_g);
        if key.code == KeyCode::Char('g') && !key.modifiers.contains(KeyModifiers::CONTROL) {
            if !pending_g {
                self.pending_g = true;
                return;
            }
            key.code = KeyCode::Home;
        }
        let switching = self
            .account_picker()
            .is_some_and(|picker| matches!(picker.status, ui::accounts::Status::Switching));
        if switching {
            return;
        }
        if key.code == KeyCode::Esc {
            self.account_choices_request.cancel();
            self.close_overlay();
            return;
        }
        match list_action(key).or_else(|| action(key)) {
            Some(Action::CursorDown) => {
                if let Some(picker) = self.account_picker_mut() {
                    picker.move_cursor(1);
                }
            }
            Some(Action::CursorUp) => {
                if let Some(picker) = self.account_picker_mut() {
                    picker.move_cursor(-1);
                }
            }
            Some(Action::First) => {
                if let Some(picker) = self.account_picker_mut() {
                    picker.choose_row(0);
                }
            }
            Some(Action::Last) => {
                if let Some(picker) = self.account_picker_mut() {
                    picker.choose_row(usize::MAX);
                }
            }
            Some(Action::PageDown) => {
                if let Some(picker) = self.account_picker_mut() {
                    picker.move_cursor(5);
                }
            }
            Some(Action::PageUp) => {
                if let Some(picker) = self.account_picker_mut() {
                    picker.move_cursor(-5);
                }
            }
            Some(Action::Open) => self.choose_account_row(),
            Some(
                Action::TogglePause
                | Action::NextTrack
                | Action::PreviousTrack
                | Action::Volume(_)
                | Action::ToggleMute,
            ) => {
                if let Some(action) = action(key) {
                    self.act(action);
                }
            }
            _ => {}
        }
    }

    fn choose_account_row(&mut self) {
        let Some(picker) = self.account_picker() else {
            return;
        };
        if picker.busy() {
            return;
        }
        if picker.cursor == picker.accounts.len() {
            self.load_account_choices();
            return;
        }
        if picker.cursor > picker.accounts.len() {
            self.open_logout_confirmation();
            return;
        }
        let Some(account) = picker.accounts.get(picker.cursor).cloned() else {
            return;
        };
        let Some(access) = self.deps.account_access.clone() else {
            return;
        };
        self.cancel_account_reads();
        if let Some(picker) = self.account_picker_mut() {
            picker.status = ui::accounts::Status::Switching;
        }
        let request = self.account_change_request.begin(());
        let events = self.events.clone();
        let task = self.background_tasks.spawn(async move {
            let result = tokio::task::spawn_blocking(move || access.switch(account))
                .await
                .unwrap_or_else(|_| Err("Account switch did not finish. Try again.".to_owned()));
            let _ = events.send(AppEvent::AccountChanged { request, result });
        });
        self.account_change_request.hold(request, task);
    }

    fn cancel_account_reads(&mut self) {
        self.deps.session_generation.fetch_add(1, Ordering::SeqCst);
        self.account_request.cancel();
        for request in self.page_requests.values_mut() {
            request.cancel();
        }
        for request in self.prefetch_requests.values_mut() {
            request.cancel();
        }
        self.page_loaders.clear();
        self.duration_fill_tasks.clear();
        self.home.set_busy(false);
        self.search.set_busy(false);
        self.library.set_busy(false);
    }

    fn account_changed(&mut self, identity: String) {
        self.cancel_account_reads();
        self.account = identity;
        self.login_state = LoginState::SignedIn;
        self.session_logged_out = false;
        self.liked_overrides.clear();
        self.like_intents.forget_account();
        self.home.replace(Page::new("Home", Vec::new()));
        self.search.replace(Page::new("Search", Vec::new()));
        self.library.replace(Page::new("Library", Vec::new()));
        if self.deps.page_cache.clear().is_err() {
            self.notify(
                "Old page cache could not be cleared.",
                Some("Account"),
                Level::Warning,
            );
        }
        self.close_overlay();
        self.interaction.focus(PageFocus::Browse);
        self.notify(
            format!("Signed in as {}", self.account),
            Some("Account"),
            Level::Info,
        );
        self.load_page(Target::Home, Some("home".to_owned()), true);
        self.load_page(Target::Library, Some("library".to_owned()), true);
        if !self.search_query.trim().is_empty() {
            let query = self.search_query.trim().to_owned();
            let key = crate::page_cache::search_key(&query);
            self.fetch(Target::Search, Fetch::Search(query), Some(key), true);
        }
    }

    /// The rice can switch themes while redwax runs, so the palette is read
    /// again every couple of seconds and the theme follows it.
    fn start_palette_poll(&mut self, live_theme_request: LiveThemeRequest) {
        let event_sender = self.events.clone();
        match live_theme_request.source {
            crate::themes::LiveThemeSource::Terminal => {
                let outer_terminal = self.deps.art_setup.outer_terminal.clone();
                self.background_tasks.spawn(async move {
                    let mut last_palette: Option<crate::palette::Palette> = None;
                    loop {
                        tokio::time::sleep(Duration::from_secs_f64(PALETTE_POLL_SECONDS)).await;
                        let terminal_name = outer_terminal.clone();
                        let palette_task = tokio::task::spawn_blocking(move || {
                            crate::palette::terminal_palette(terminal_name.as_deref())
                        })
                        .await;
                        let Ok(Some(palette)) = palette_task else {
                            continue;
                        };
                        if last_palette.as_ref() == Some(&palette) {
                            continue;
                        }
                        last_palette = Some(palette.clone());
                        let _ = event_sender.send(AppEvent::PaletteChanged {
                            request: live_theme_request,
                            palette,
                        });
                    }
                });
            }
            crate::themes::LiveThemeSource::Omarchy => {
                self.background_tasks.spawn(async move {
                    let mut last_palette: Option<crate::palette::ValidatedPalette> = None;
                    loop {
                        tokio::time::sleep(Duration::from_secs_f64(PALETTE_POLL_SECONDS)).await;
                        let palette_task = tokio::task::spawn_blocking(crate::themes::omarchy_palette).await;
                        let Ok(Some(palette)) = palette_task else {
                            continue;
                        };
                        if last_palette.as_ref() == Some(&palette) {
                            continue;
                        }
                        last_palette = Some(palette.clone());
                        let _ = event_sender.send(AppEvent::OmarchyPaletteChanged {
                            request: live_theme_request,
                            palette,
                        });
                    }
                });
            }
        }
    }

    /// The picker queries stdio, so `main` builds it after the first frame and
    /// before the event stream reads a single key.
    pub fn set_picker(&mut self, image_picker: Picker) {
        self.art.image_picker = Some(image_picker);
    }

    // ----------------------------------------------------------------- keys

    /// Every key from the table in `keys`, with the search box and the help
    /// panel taking precedence when they have focus.
    pub fn handle_key(&mut self, key_event: KeyEvent) {
        self.screen.touch();
        self.last_input_millis = clock();
        if self.account_picker_visible() {
            self.account_picker_key(key_event);
            return;
        }
        if let Some((_, notification_scroll)) = self.notification_popup_mut() {
            match key_event.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('!') => {
                    self.close_overlay();
                }
                _ => {
                    notification_scroll.key(key_event.code);
                }
            }
            return;
        }
        // The picker swallows keys that would otherwise reach the covered page.
        if self.playlist_picker().is_some() {
            self.picker_key(key_event);
            return;
        }
        if self.help_visible() {
            self.help_key(key_event);
            return;
        }
        if self.eq.visible() && self.eq_key(key_event) {
            return;
        }
        // An open field answers before the page, because every key belongs to
        // it until it closes.
        match self.prompt() {
            Some(Prompt::Find(_)) => return self.find_key(key_event),
            Some(Prompt::Volume(_)) => return self.volume_key(key_event),
            Some(Prompt::ClearQueue(_)) | Some(Prompt::Logout(_)) => {
                return self.confirmation_key(key_event);
            }
            None => {}
        }
        if self.search_focused() && self.edit_query(key_event) {
            return;
        }
        // `g` is a prefix rather than a key: it does nothing until a second
        // one lands, and any other key drops it.
        let pending_g_was_set = std::mem::take(&mut self.pending_g);
        if !self.search_focused()
            && key_event.code == KeyCode::Char('g')
            && !key_event.modifiers.contains(KeyModifiers::CONTROL)
        {
            match pending_g_was_set {
                true => self.act(Action::First),
                false => self.pending_g = true,
            }
            return;
        }
        if self.account_focused() {
            if let Some(decoded_action) = list_action(key_event).or_else(|| action(key_event)) {
                match decoded_action {
                    Action::Open => self.open_account_picker(),
                    Action::CursorDown | Action::CursorUp | Action::Back => {
                        self.interaction.focus(PageFocus::Browse);
                    }
                    action => self.act(action),
                }
            }
            return;
        }
        // The themes tab is a list of its own, so the movement keys reach it
        // before the browser they would otherwise drive.
        if self.tab == Tab::Themes && self.themes_key(key_event) {
            return;
        }
        if self.tab == Tab::Settings
            && !key_event
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && matches!(key_event.code, KeyCode::Enter | KeyCode::Char(' '))
        {
            self.toggle_search_vim();
            return;
        }
        let decoded_action = match self.search_focused() {
            true => action(key_event),
            false => list_action(key_event).or_else(|| action(key_event)),
        };
        if let Some(decoded_action) = decoded_action {
            self.act(decoded_action);
        }
    }

    pub fn input_action(&self, key_event: KeyEvent) -> Option<Action> {
        if !matches!(self.interaction, Interaction::Browse) {
            return None;
        }
        list_action(key_event).or_else(|| action(key_event))
    }

    pub fn timing_action(&self, key_event: KeyEvent) -> Option<Action> {
        self.input_action(key_event)
    }

    /// Bracketed paste reaches only the field that visibly owns text input.
    pub fn handle_paste(&mut self, text: &str) {
        let pasted_text: String = text.chars().filter(|character| !character.is_control()).collect();
        if pasted_text.is_empty() {
            return;
        }
        if self.help_visible() && self.help_filter_focused() {
            self.help_query.push_str(&pasted_text);
            self.help_scroll = ui::popup::Scroll::default();
        } else if let Some(picker) = self.playlist_picker_mut()
            && let Some(name_input) = picker.new_playlist_name_input.as_mut()
        {
            let remaining_characters = PLAYLIST_NAME_LIMIT.saturating_sub(name_input.chars().count());
            name_input.extend(pasted_text.chars().take(remaining_characters));
        } else if let Some(Prompt::Find(find)) = self.prompt_mut() {
            find.query.push_str(&pasted_text);
            self.settle_find();
        } else if self.search_focused() {
            self.search_query.push_str(&pasted_text);
            self.search_editor.focus(&self.search_query);
        }
        self.screen.touch();
    }

    /// A resize invalidates any pointer gesture measured against the old frame.
    pub fn resized(&mut self) {
        self.scrub = None;
        self.screen.touch();
    }

    fn help_key(&mut self, key_event: KeyEvent) {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        if self.help_filter_focused() {
            match key_event.code {
                KeyCode::Char(character) => self.help_query.push(character),
                KeyCode::Backspace => {
                    self.help_query.pop();
                }
                KeyCode::Esc if !self.help_query.is_empty() => self.help_query.clear(),
                KeyCode::Esc | KeyCode::Enter => self.set_help_filter_focused(false),
                _ => {}
            }
            self.help_scroll = ui::popup::Scroll::default();
            self.screen.touch();
            return;
        }
        match key_event.code {
            KeyCode::Char('/') => self.set_help_filter_focused(true),
            KeyCode::Char('?') | KeyCode::Esc if self.help_query.is_empty() => {
                self.close_overlay();
            }
            KeyCode::Esc => self.help_query.clear(),
            _ => {
                self.help_scroll.key(key_event.code);
            }
        }
        self.screen.touch();
    }

    /// Move, choose or cancel. Every other key is swallowed while the picker
    /// is up, because the page under it is not what the user is looking at.
    fn picker_key(&mut self, key_event: KeyEvent) {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        let Some(picker) = self.playlist_picker_mut() else {
            return;
        };
        // An open name field owns every key, so a playlist can be called
        // anything, `q` and `P` included.
        if let Some(name_input) = picker.new_playlist_name_input.as_mut() {
            match key_event.code {
                KeyCode::Char(character) if name_input.chars().count() < PLAYLIST_NAME_LIMIT => {
                    name_input.push(character)
                }
                KeyCode::Backspace => {
                    name_input.pop();
                }
                KeyCode::Enter if !name_input.trim().is_empty() => self.create_playlist(),
                KeyCode::Esc => picker.new_playlist_name_input = None,
                _ => {}
            }
            return;
        }
        let last_row_index = picker.row_count().saturating_sub(1);
        match key_event.code {
            KeyCode::Char('j') | KeyCode::Down => {
                picker.selected_playlist_index = (picker.selected_playlist_index + 1).min(last_row_index)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                picker.selected_playlist_index = picker.selected_playlist_index.saturating_sub(1)
            }
            KeyCode::Char('g') | KeyCode::Home => picker.selected_playlist_index = 0,
            KeyCode::Char('G') | KeyCode::End => picker.selected_playlist_index = last_row_index,
            KeyCode::Tab => picker.swap_subject(),
            KeyCode::Enter if picker.selected_playlist().is_some() => self.choose_playlist(),
            KeyCode::Enter if !picker.remove_mode() => picker.new_playlist_name_input = Some(String::new()),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('P') | KeyCode::Char('D') => {
                self.close_overlay();
            }
            _ => {}
        }
    }

    /// True when the themes tab consumed the key. Only movement and choosing:
    /// everything else, the transport included, still reaches the app, so the
    /// music keeps playing while a theme is picked.
    fn themes_key(&mut self, key_event: KeyEvent) -> bool {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key_event.code {
            KeyCode::Char('j') | KeyCode::Down => self.themes_cursor(ThemesMove::By(1)),
            KeyCode::Char('k') | KeyCode::Up => self.themes_cursor(ThemesMove::By(-1)),
            KeyCode::Char('G') | KeyCode::End => self.themes_cursor(ThemesMove::Edge(true)),
            KeyCode::Home => self.themes_cursor(ThemesMove::Edge(false)),
            KeyCode::Enter => self.keep_theme(),
            _ => return false,
        }
        true
    }

    /// True when the equaliser panel consumed the key. Every change reaches
    /// mpv at once, so a band is heard while the panel is still open.
    fn eq_key(&mut self, key_event: KeyEvent) -> bool {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        let Some(eq_effect) = self.eq.key(key_event.code) else {
            return false;
        };
        self.settle_eq(eq_effect);
        true
    }

    /// Pay for what the panel changed. It has no player and spawns no tasks,
    /// so reaching mpv and asking for a genre are the app's half of the deal.
    fn settle_eq(&mut self, eq_effect: crate::eq::Effect) {
        match eq_effect {
            crate::eq::Effect::Redraw => {}
            crate::eq::Effect::Filter => {
                let audio_filter_graph = self.eq.graph();
                self.command(move |player| async move {
                    player.set_audio_filter(audio_filter_graph.as_deref()).await
                });
            }
            crate::eq::Effect::Genre => {
                if let Some(track) = self.player_state.track().cloned() {
                    self.start_genre(&track);
                }
            }
        }
    }

    /// Ask what the playing track is, once ever per track. Nothing here waits
    /// on the answer and nothing tells the user when there is none.
    fn start_genre(&mut self, track: &Track) {
        self.eq.forget_genre();
        if !self.eq.genre_match() {
            return;
        }
        let video_id = track.video_id.clone();
        if let Some(genre) = self.genre_cache.get(&video_id) {
            self.genre_ready(video_id, Some(genre));
            return;
        }
        if !self.genre_cache.take_unasked(&video_id) {
            return;
        }
        let primary_artist = track.artists.first().cloned().unwrap_or_default();
        let track_title = track.title.clone();
        let genre_lookup = self.genre_lookup.clone();
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let genre = genre_lookup(primary_artist, track_title).await;
            let _ = event_sender.send(AppEvent::GenreReady { video_id, genre });
        });
    }

    fn genre_ready(&mut self, video_id: String, genre: Option<String>) {
        // A lookup that lands after the track has moved on belongs to nobody.
        let playing_video_id = self.player_state.track().map(|track| track.video_id.as_str());
        if playing_video_id != Some(video_id.as_str()) {
            return;
        }
        let Some(genre) = genre else {
            return;
        };
        self.genre_cache.learn(&video_id, &genre);
        if let Some(eq_effect) = self.eq.genre_answered(genre) {
            self.settle_eq(eq_effect);
        }
    }

    fn write_eq(&mut self) {
        self.eq.write_if_due(&crate::eq::path_for(&self.deps.config));
    }

    fn edit_query(&mut self, key_event: KeyEvent) -> bool {
        match self.search_editor.handle_key(&mut self.search_query, key_event) {
            EditOutcome::Consumed => true,
            EditOutcome::Submit => {
                self.submit_search();
                true
            }
            EditOutcome::Blur => {
                self.interaction.focus(PageFocus::Browse);
                !matches!(key_event.code, KeyCode::Up | KeyCode::Down)
            }
        }
    }

    fn focus_search(&mut self) {
        self.search_editor.focus(&self.search_query);
        self.interaction.focus(PageFocus::Search);
        self.pending_g = false;
    }

    fn toggle_search_vim(&mut self) {
        let vim_enabled = !self.deps.config.search_vim_mode;
        match self.deps.config.set_search_vim_mode(vim_enabled) {
            Ok(()) => {
                self.search_editor.set_vim(vim_enabled, &self.search_query);
                let editing_mode_label = if vim_enabled { "Vim" } else { "Standard" };
                self.notify(
                    format!("Search editing: {editing_mode_label}"),
                    Some("Settings"),
                    Level::Info,
                );
            }
            Err(error) => self.notify(error, Some("Settings"), Level::Error),
        }
    }

    // --------------------------------------------------------------- mouse

    /// Every button and the wheel, against the regions the last frame left.
    /// A click moves the cursor onto the row first, so the highlight follows
    /// the pointer rather than lagging one action behind it.
    pub fn handle_mouse(&mut self, mouse_event: MouseEvent) {
        self.last_input_millis = clock();
        let (column, row) = (mouse_event.column, mouse_event.row);
        if let Some((_, scroll)) = self.notification_popup_mut() {
            match mouse_event.kind {
                MouseEventKind::ScrollDown => scroll.move_by(WHEEL_ROWS),
                MouseEventKind::ScrollUp => scroll.move_by(-WHEEL_ROWS),
                _ => {}
            }
            self.screen.touch();
            return;
        }
        if self.modal_pointer_owner()
            && self.modal_pointer_frame_revision != Some(self.screen.hit_map().revision())
        {
            self.screen.touch();
            return;
        }
        if self
            .scrub
            .is_some_and(|scrub| scrub.geometry_revision != self.screen.geometry_revision())
        {
            self.scrub = None;
        }
        match mouse_event.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.update_scrub(column);
                return;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.update_scrub(column);
                if let Some(scrub) = self.scrub.take() {
                    self.screen.touch();
                    match scrub.slider {
                        Slider::Seek => self.seek_to_fraction(scrub.normalized_fraction),
                        Slider::Volume => {
                            let volume_percent = (scrub.normalized_fraction * 100.0).round() as u8;
                            self.command(
                                move |player| async move { player.set_volume(volume_percent).await },
                            );
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        let click = match mouse_event.kind {
            MouseEventKind::Down(MouseButton::Left) => Click::Open,
            MouseEventKind::Down(MouseButton::Right) => Click::Queue,
            MouseEventKind::Down(MouseButton::Middle) => Click::PlayNext,
            MouseEventKind::ScrollDown => {
                self.screen.touch();
                self.wheel(column, row, WHEEL_ROWS);
                return;
            }
            MouseEventKind::ScrollUp => {
                self.screen.touch();
                self.wheel(column, row, -WHEEL_ROWS);
                return;
            }
            _ => return,
        };
        self.screen.touch();
        let Some((_, pointer_intent)) = self.screen.hit_map().intent_at(column, row) else {
            return;
        };
        if self.modal_pointer_owner()
            && !matches!(
                &pointer_intent,
                PointerIntent::Hit(Hit::Overlay | Hit::AccountRow(_) | Hit::Confirmation(_))
            )
        {
            return;
        }
        if click != Click::Open {
            if let PointerIntent::Hit(Hit::Row(row_target)) = pointer_intent {
                self.click_row(row_target, click);
            }
            return;
        }
        match pointer_intent {
            PointerIntent::Seek(normalized_fraction) => {
                let Some(Hit::SeekBar(area)) = self.screen.hit_map().at(column, row) else {
                    return;
                };
                self.scrub = Some(Scrub {
                    slider: Slider::Seek,
                    normalized_fraction,
                    geometry_revision: self.screen.geometry_revision(),
                    area,
                });
            }
            PointerIntent::Volume(normalized_fraction) => {
                let Some(Hit::Volume(area)) = self.screen.hit_map().at(column, row) else {
                    return;
                };
                self.scrub = Some(Scrub {
                    slider: Slider::Volume,
                    normalized_fraction,
                    geometry_revision: self.screen.geometry_revision(),
                    area,
                });
            }
            PointerIntent::EqBand {
                band_index,
                vertical_fraction,
            } => {
                let eq_effect = self
                    .eq
                    .set(band_index, vertical_fraction as f32 * crate::eq::DISPLAY_MAX);
                self.settle_eq(eq_effect);
            }
            PointerIntent::Hit(hit) => self.open_hit(hit),
        }
    }

    fn update_scrub(&mut self, screen_column: u16) {
        let Some(scrub) = self.scrub.as_mut() else {
            return;
        };
        // Repaints keep a drag alive only while its original slider still exists.
        let slider_present = self.screen.hit_map().regions().iter().any(|(area, hit)| {
            *area == scrub.area
                && matches!(
                    (scrub.slider, hit),
                    (Slider::Seek, Hit::SeekBar(_)) | (Slider::Volume, Hit::Volume(_))
                )
        });
        if !slider_present {
            self.scrub = None;
            return;
        }
        let clamped_column = screen_column.clamp(scrub.area.x, scrub.area.right().saturating_sub(1));
        let Some((_, pointer_intent)) = self.screen.hit_map().intent_at(clamped_column, scrub.area.y) else {
            self.scrub = None;
            return;
        };
        match (scrub.slider, pointer_intent) {
            (Slider::Seek, PointerIntent::Seek(normalized_fraction))
            | (Slider::Volume, PointerIntent::Volume(normalized_fraction)) => {
                scrub.normalized_fraction = normalized_fraction
            }
            _ => {
                self.scrub = None;
                return;
            }
        }
        self.screen.touch();
    }

    fn open_hit(&mut self, hit: Hit) {
        match hit {
            Hit::Row(row_target) => self.click_row(row_target, Click::Open),
            Hit::Notification(notification_index) => self.read_notification(Some(notification_index)),
            Hit::Tab(tab) => self.switch_tab(tab),
            Hit::Account => self.open_account_picker(),
            Hit::AccountRow(row) => {
                if let Some(picker) = self.account_picker_mut() {
                    picker.choose_row(row);
                }
                self.choose_account_row();
            }
            Hit::KeyHint(action) => self.act(action),
            Hit::SeekBar(_) | Hit::Volume(_) | Hit::EqBand(_, _) => {}
            Hit::Shuffle => self.toggle_shuffle(),
            Hit::Repeat => self.cycle_repeat(),
            Hit::Previous => self.act(Action::PreviousTrack),
            Hit::PlayPause => self.act(Action::TogglePause),
            Hit::Next => self.act(Action::NextTrack),
            Hit::Mute => self.command(|player| async move { player.toggle_mute().await }),
            Hit::UpNext(upcoming_offset) => self.jump_ahead(upcoming_offset),
            Hit::LyricLine(line_index) => self.seek_to_lyric(line_index),
            Hit::Search => {
                self.switch_tab(Tab::Search);
                self.focus_search();
            }
            Hit::ToggleSearchVim => self.toggle_search_vim(),
            Hit::Crumb(depth) => {
                let navigation_changed = self
                    .active_browser_mut()
                    .is_some_and(|browser| browser.to_depth(depth));
                if navigation_changed && let Some(target) = self.tab.target() {
                    self.leave_page(target);
                }
            }
            Hit::ThemeRow(row) => {
                self.themes_cursor(ThemesMove::To(row));
                self.keep_theme();
            }
            Hit::Confirmation(choice) => {
                if let Some(confirmation_state) = self.confirmation_state_mut() {
                    confirmation_state.select(choice);
                }
                self.settle_confirmation(choice);
            }
            Hit::List(_) | Hit::Overlay => {}
        }
    }

    /// Left plays or opens, right queues, middle plays next, all on the row
    /// the pointer is over rather than the row the cursor was on.
    fn click_row(&mut self, row_target: RowTarget, click: Click) {
        if self.tab.target() != Some(row_target.browser_target)
            || !self.browser(row_target.browser_target).accepts(&row_target)
        {
            return;
        }
        let projected_row_index =
            self.browser(row_target.browser_target)
                .rows()
                .iter()
                .position(|candidate_row| {
                    matches!(
                        candidate_row,
                        ui::Row::Item(shelf_index, item_index)
                            if (*shelf_index, *item_index)
                                == (row_target.shelf_index, row_target.item_index)
                    )
                });
        self.browser_mut(row_target.browser_target)
            .select_row(projected_row_index);
        self.arm_rest();
        match click {
            Click::Open => self.open_selected(),
            Click::Queue => self.queue_highlighted(false),
            Click::PlayNext => self.queue_highlighted(true),
        }
    }

    fn wheel(&mut self, screen_column: u16, screen_row: u16, row_delta: isize) {
        if self.modal_pointer_owner() {
            if self.help_visible() {
                self.help_scroll.move_by(row_delta);
            } else if let Some(picker) = self.account_picker_mut() {
                picker.move_cursor(row_delta);
            }
            return;
        }
        match self.screen.hit_map().at(screen_column, screen_row) {
            Some(Hit::Notification(notification_index)) => {
                self.read_notification(Some(notification_index));
                return;
            }
            Some(Hit::Overlay) => {
                if self.help_visible() {
                    self.help_scroll.move_by(row_delta);
                }
                return;
            }
            _ => {}
        }
        // The panel is over the list, so a band under the pointer answers the
        // wheel before the list that is drawn under it does.
        if let Some(Hit::EqBand(band_index, _)) = self.screen.hit_map().at(screen_column, screen_row) {
            let eq_step = match row_delta < 0 {
                true => crate::eq::STEP,
                false => -crate::eq::STEP,
            };
            let eq_effect = self.eq.nudge(band_index, eq_step);
            self.settle_eq(eq_effect);
            return;
        }
        // Only the active tab is painted, so a list under the pointer is that
        // tab's list and the lyrics are the whole pane.
        if self.tab == Tab::Lyrics || self.screen.hit_map().list_at(screen_column, screen_row).is_some() {
            self.move_cursor(row_delta);
        }
    }

    fn modal_pointer_owner(&self) -> bool {
        matches!(
            self.interaction.overlay(),
            Some(
                Overlay::Help { .. }
                    | Overlay::PlaylistPicker(_)
                    | Overlay::Accounts(_)
                    | Overlay::Prompt(Prompt::ClearQueue(_) | Prompt::Logout(_))
            )
        )
    }

    /// Seek to a fraction of the playing track. A track of no known length is
    /// not seekable, so this is a no-op rather than an error.
    fn seek_to_fraction(&mut self, normalized_fraction: f64) {
        let Some(duration_seconds) = self
            .player_state
            .duration
            .filter(|duration_seconds| *duration_seconds > 0.0)
        else {
            return;
        };
        let target_seconds = duration_seconds * normalized_fraction.clamp(0.0, 1.0);
        self.command(move |player| async move { player.seek_to(target_seconds).await });
    }

    /// Play the up next line that was clicked, counted from what plays now.
    fn jump_ahead(&mut self, upcoming_offset: usize) {
        let Some(playing_index) = self.player_state.index else {
            return;
        };
        let target_index = playing_index + 1 + upcoming_offset;
        if target_index >= self.player_state.queue.len() {
            return;
        }
        self.command(move |player| async move { player.jump(target_index).await });
    }

    /// A click on a timed line plays from where that line starts.
    fn seek_to_lyric(&mut self, line_index: usize) {
        let Some(Lyrics::Timed(timed_lines)) = self.lyrics.text() else {
            return;
        };
        let Some(lyric_line) = timed_lines.get(line_index) else {
            return;
        };
        let target_seconds = lyric_line.start_ms as f64 / 1000.0;
        self.command(move |player| async move { player.seek_to(target_seconds).await });
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::TogglePause => {
                if !self.feed_restored_queue() {
                    self.command(|player| async move { player.toggle_pause().await });
                }
            }
            Action::NextTrack => self.command(|player| async move { player.next().await }),
            Action::PreviousTrack => self.command(|player| async move { player.previous().await }),
            Action::Seek(seconds) => {
                let seek_delta = f64::from(seconds);
                self.command(move |player| async move { player.seek(seek_delta).await });
            }
            Action::Volume(volume_delta) => {
                self.command(move |player| async move { player.adjust_volume(volume_delta).await })
            }
            Action::ToggleMute => self.command(|player| async move { player.toggle_mute().await }),
            Action::SetVolume => {
                self.open_overlay(Overlay::Prompt(Prompt::Volume(
                    self.player_state.volume.to_string(),
                )));
            }
            Action::Radio => self.radio(),
            Action::LikeHighlighted => {
                self.like(self.highlighted_track().cloned(), "Highlight a track to like it")
            }
            Action::ToggleShuffle => self.toggle_shuffle(),
            Action::CycleRepeat => self.cycle_repeat(),
            Action::RemoveFromQueue => self.remove_from_queue(),
            Action::MoveQueueUp => self.move_queue_entry(-1),
            Action::MoveQueueDown => self.move_queue_entry(1),
            Action::JumpToQueue => self.jump_to_queue(),
            Action::RemoveFromPlaylist => self.remove_from_playlist(),
            Action::AddToPlaylist => self.open_add_picker(false),
            Action::AddHighlightedToPlaylist => self.open_add_picker(true),
            Action::DeleteHighlighted => {
                if self.tab == Tab::Queue {
                    self.remove_from_queue();
                } else if self.active_browser().and_then(Browser::open_playlist).is_some() {
                    self.remove_highlighted_row();
                } else {
                    self.notify("Open a playlist or the queue to remove a song", None, Level::Info);
                }
            }
            Action::ClearQueue => {
                self.open_clear_confirmation();
            }
            Action::PreviousTab => self.navigate_header(false),
            Action::NextTab => self.navigate_header(true),
            Action::SwitchTab(tab) => self.switch_tab(tab),
            Action::FindOnPage => match self.tab {
                Tab::Search => self.focus_search(),
                _ => self.open_find(),
            },
            Action::FocusAccount => {
                self.eq.hide();
                let focus = if self.account_focused() {
                    PageFocus::Browse
                } else {
                    PageFocus::Account
                };
                self.interaction.focus(focus);
            }
            Action::OpenAccount => self.open_account_picker(),
            Action::PreviousSection => self.jump_section(false),
            Action::NextSection => self.jump_section(true),
            Action::Back => self.back(),
            Action::Reload => self.reload(),
            Action::ToggleHelp => {
                if self.help_visible() {
                    self.close_overlay();
                } else {
                    self.open_overlay(Overlay::Help {
                        filter_focused: false,
                    });
                }
                self.help_scroll = ui::popup::Scroll::default();
                self.eq.hide();
            }
            Action::ShowNotification => self.read_notification(None),
            Action::ToggleEq => {
                self.eq.toggle();
                if self.help_visible() {
                    self.close_overlay();
                }
            }
            Action::Quit => self.quit = true,
            Action::CursorDown => self.move_cursor(1),
            Action::CursorUp => self.move_cursor(-1),
            Action::First => self.cursor_to_edge(false),
            Action::Last => self.cursor_to_edge(true),
            Action::PageDown if self.tab == Tab::Lyrics => self.lyrics.scrolled_page(true, clock()),
            Action::PageUp if self.tab == Tab::Lyrics => self.lyrics.scrolled_page(false, clock()),
            Action::PageDown => self.move_cursor(self.screen.visible_rows().max(1) as isize),
            Action::PageUp => self.move_cursor(-(self.screen.visible_rows().max(1) as isize)),
            Action::Open => self.open_selected(),
            Action::OpenAlbum => self.open_album(),
            Action::CycleFilter => {
                if let Some(browser) = self.active_browser_mut() {
                    browser.cycle_filter(true);
                    self.arm_rest();
                }
            }
            Action::Logout => self.open_logout_confirmation(),
        }
    }

    /// Hand mpv the queue the state file restored, at the track the sidebar is
    /// already showing. Returns true when it did, so a play key can stop there.
    ///
    /// The playback coordinator keeps the restored queue visible until mpv
    /// accepts it, so a failed feed can be retried without losing the snapshot.
    fn feed_restored_queue(&mut self) -> bool {
        if !self.playback.restored_pending() || self.player_state.queue.is_empty() {
            return false;
        }
        self.command(|playback| async move { playback.resume_restored().await.map(|_| ()) });
        true
    }

    /// Player calls are async and key handling is not, so every command is a
    /// task. The screen follows the player's own state change, never a guess.
    fn command<F, Fut>(&mut self, run_command: F)
    where
        F: FnOnce(Playback<P>) -> Fut + Send + 'static,
        Fut: Future<Output = redwax_player::Result<()>> + Send + 'static,
    {
        let playback = self.playback.clone();
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            if let Err(error) = run_command(playback).await {
                let _ = event_sender.send(AppEvent::Notify {
                    text: error.to_string(),
                    title: Some("Playback".to_string()),
                    level: Level::Error,
                });
            }
        });
    }

    fn move_cursor(&mut self, row_delta: isize) {
        if self.tab == Tab::Lyrics {
            self.lyrics.scrolled(row_delta, clock());
            return;
        }
        if let Some(browser) = self.active_browser_mut() {
            browser.move_cursor(row_delta);
        }
        self.arm_rest();
    }

    fn cursor_to_edge(&mut self, select_last: bool) {
        if self.tab == Tab::Lyrics {
            self.lyrics.scrolled_to_edge(select_last, clock());
            return;
        }
        if let Some(browser) = self.active_browser_mut() {
            browser.cursor_to_edge(select_last);
        }
        self.arm_rest();
    }

    /// Paint everything in the theme this name asks for. The terminal theme
    /// has no palette here, so it falls back to what the last run saved, which
    /// is what the first frame uses too.
    fn wear(&mut self, theme_id: ThemeId) {
        let live_theme_request = self.theme_controller.activate(theme_id);
        let resolved_theme = self.theme_controller.resolve_or_cached(&self.deps.config);
        self.base_theme = self.theme_controller.present(
            resolved_theme,
            self.deps.config.transparent,
            self.art.accent_color,
        );
        if let Some(live_theme_request) = live_theme_request {
            self.start_palette_poll(live_theme_request);
        }
        self.screen.touch();
    }

    /// Keep what the cursor is on: write it to the config so it survives a
    /// relaunch, and say so, because a write with no word for it looks like
    /// nothing happened.
    fn keep_theme(&mut self) {
        let Some(theme_id) = self.themes.as_ref().and_then(|pane| pane.selected()).cloned() else {
            return;
        };
        let config_path = self.deps.config.source.clone();
        match self
            .theme_controller
            .commit(&mut self.deps.config, theme_id.clone())
        {
            Ok(live_theme_request) => {
                if let Some(pane) = self.themes.as_mut() {
                    pane.keep();
                }
                if let Some(live_theme_request) = live_theme_request {
                    self.start_palette_poll(live_theme_request);
                }
                self.notify(format!("{} kept", theme_id.name()), Some("Theme"), Level::Info);
            }
            Err(error) => self.notify(
                format!("could not write {}: {error}", config_path.display()),
                Some("Theme"),
                Level::Error,
            ),
        }
    }

    /// Move the themes cursor and paint what it lands on.
    fn themes_cursor(&mut self, requested_move: ThemesMove) {
        let Some(pane) = self.themes.as_mut() else {
            return;
        };
        match requested_move {
            ThemesMove::By(row_delta) => pane.move_cursor(row_delta),
            ThemesMove::To(row_index) => pane.cursor_to(row_index),
            ThemesMove::Edge(select_last) => pane.cursor_to_edge(select_last),
        }
        if let Some(theme_id) = pane.selected().cloned() {
            self.wear(theme_id);
        }
    }

    fn switch_tab(&mut self, tab: Tab) {
        // Leaving the themes tab without choosing puts the old theme back, so
        // a look around costs nothing.
        if self.tab == Tab::Themes && tab != Tab::Themes {
            let previous_theme = self.themes.as_ref().map(|pane| pane.previous().clone());
            if let Some(previous_theme) = previous_theme {
                self.wear(previous_theme);
            }
            self.themes = None;
        }
        if tab == Tab::Themes && self.themes.is_none() {
            self.themes = Some(crate::themes::ThemesPane::open(&self.deps.config));
        }
        self.tab = tab;
        if let Some(Overlay::Prompt(Prompt::ClearQueue(prompt))) = self.close_overlay() {
            self.playback.cancel_clear(prompt.preview);
        }
        self.interaction.focus(PageFocus::Browse);
        if tab == Tab::Lyrics {
            if let Some(track) = self.player_state.track().cloned()
                && self.lyrics.wants(&track)
            {
                self.load_lyrics(&track);
            }
            return;
        }
        self.arm_rest();
    }

    fn navigate_header(&mut self, forward: bool) {
        if self.account_focused() {
            self.switch_tab(if forward { Tab::Home } else { Tab::Settings });
        } else if (forward && self.tab == Tab::Settings) || (!forward && self.tab == Tab::Home) {
            self.interaction.focus(PageFocus::Account);
        } else {
            self.switch_tab(if forward {
                self.tab.next()
            } else {
                self.tab.previous()
            });
        }
    }

    /// Open the find bar over the crumbs of the open page.
    fn open_find(&mut self) {
        if self.tab.target().is_none() {
            return;
        }
        let original_row = self.active_browser().and_then(|browser| browser.cursor());
        self.open_overlay(Overlay::Prompt(Prompt::Find(Find {
            original_row,
            has_match: true,
            ..Find::default()
        })));
    }

    /// Type into the find bar. The cursor follows every keystroke, so the page
    /// under the bar is always showing what the query has reached.
    fn find_key(&mut self, key_event: KeyEvent) {
        let match_direction = match key_event.code {
            KeyCode::Char('n') if key_event.modifiers.contains(KeyModifiers::CONTROL) => Some(true),
            KeyCode::Char('p') if key_event.modifiers.contains(KeyModifiers::CONTROL) => Some(false),
            _ => None,
        };
        if let Some(forward) = match_direction {
            let Some(Prompt::Find(find)) = self.prompt() else {
                return;
            };
            if find.query.is_empty() {
                return;
            }
            let find_query = find.query.clone();
            let match_position = self
                .active_browser_mut()
                .and_then(|browser| browser.find_next(&find_query, forward));
            if let Some(Prompt::Find(find)) = self.prompt_mut() {
                find.has_match = match_position.is_some();
                find.match_position = match_position;
            }
            self.arm_rest();
            return;
        }
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match key_event.code {
            KeyCode::Char(character) => {
                let Some(Prompt::Find(find)) = self.prompt_mut() else {
                    return;
                };
                find.query.push(character);
            }
            KeyCode::Backspace => {
                let Some(Prompt::Find(find)) = self.prompt_mut() else {
                    return;
                };
                find.query.pop();
            }
            // Enter keeps the row the find reached, escape gives it back.
            KeyCode::Enter => {
                self.close_overlay();
                return;
            }
            KeyCode::Esc => {
                let original_row = match self.prompt() {
                    Some(Prompt::Find(find)) => find.original_row,
                    _ => return,
                };
                self.close_overlay();
                if let Some(browser) = self.active_browser_mut() {
                    browser.select_row(original_row);
                }
                return;
            }
            _ => return,
        }
        self.settle_find();
    }

    fn settle_find(&mut self) {
        let Some(Prompt::Find(find)) = self.prompt() else {
            return;
        };
        let find_query = find.query.clone();
        let original_row = find.original_row;
        let has_match = match find_query.is_empty() {
            true => {
                if let Some(browser) = self.active_browser_mut() {
                    browser.select_row(original_row);
                }
                true
            }
            false => self
                .active_browser_mut()
                .is_some_and(|browser| browser.find(&find_query)),
        };
        let match_position = match has_match {
            true => self
                .active_browser()
                .and_then(|browser| browser.find_position(&find_query)),
            false => None,
        };
        if let Some(Prompt::Find(find)) = self.prompt_mut() {
            find.has_match = has_match;
            find.match_position = match_position;
        }
        self.arm_rest();
    }

    /// Digits into the volume field. Enter sets it, escape leaves it alone,
    /// and nothing reaches mpv until one of the two.
    fn volume_key(&mut self, key_event: KeyEvent) {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        let Some(Prompt::Volume(entry)) = self.prompt_mut() else {
            return;
        };
        match key_event.code {
            KeyCode::Char(digit) if digit.is_ascii_digit() && entry.len() < 3 => entry.push(digit),
            KeyCode::Backspace => {
                entry.pop();
            }
            KeyCode::Enter => {
                let Some(volume_percent) = entry
                    .parse::<u16>()
                    .ok()
                    .map(|volume_percent| volume_percent.min(100) as u8)
                else {
                    self.notify("Enter a volume from 0 to 100", Some("Volume"), Level::Warning);
                    return;
                };
                self.close_overlay();
                self.command(move |player| async move { player.set_volume(volume_percent).await });
            }
            KeyCode::Esc => {
                self.close_overlay();
            }
            _ => {}
        }
    }

    fn open_clear_confirmation(&mut self) {
        let Some(preview) = self.playback.preview_clear_upcoming() else {
            self.notify("There are no upcoming tracks to clear", None, Level::Info);
            return;
        };
        self.eq.hide();
        self.open_overlay(Overlay::Prompt(Prompt::ClearQueue(ClearQueuePrompt {
            preview,
            confirmation_state: ui::confirm::ConfirmationState::default(),
        })));
    }

    fn confirmation_key(&mut self, key_event: KeyEvent) {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match key_event.code {
            KeyCode::Esc | KeyCode::Char('n') => self.cancel_confirmation(),
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(confirmation_state) = self.confirmation_state_mut() {
                    confirmation_state.move_to(false);
                }
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('y') => {
                if let Some(confirmation_state) = self.confirmation_state_mut() {
                    confirmation_state.move_to(true);
                }
            }
            KeyCode::Enter => {
                let Some(choice) = self
                    .confirmation_state_mut()
                    .map(|confirmation_state| confirmation_state.selected())
                else {
                    return;
                };
                self.settle_confirmation(choice);
            }
            _ => {}
        }
        self.screen.touch();
    }

    fn cancel_confirmation(&mut self) {
        if let Some(Overlay::Prompt(Prompt::ClearQueue(prompt))) = self.close_overlay() {
            self.playback.cancel_clear(prompt.preview);
        }
    }

    fn confirmation_state_mut(&mut self) -> Option<&mut ui::confirm::ConfirmationState> {
        match self.prompt_mut() {
            Some(Prompt::ClearQueue(prompt)) => Some(&mut prompt.confirmation_state),
            Some(Prompt::Logout(confirmation_state)) => Some(confirmation_state),
            _ => None,
        }
    }

    fn settle_confirmation(&mut self, choice: ui::hit::ConfirmationChoice) {
        if choice == ui::hit::ConfirmationChoice::Cancel {
            self.cancel_confirmation();
            return;
        }
        let Some(Overlay::Prompt(prompt)) = self.close_overlay() else {
            return;
        };
        if matches!(prompt, Prompt::Logout(_)) {
            self.start_logout(false);
            return;
        }
        let Prompt::ClearQueue(prompt) = prompt else {
            return;
        };
        let playback = self.playback.clone();
        let event_sender = self.events.clone();
        let state_store = self.deps.state_store.clone();
        self.background_tasks.spawn(async move {
            let clear_result = playback
                .clear_upcoming(prompt.preview)
                .await
                .map_err(|error| error.to_string());
            let clear_result = match clear_result {
                Ok(redwax_player::QueueEdit::Applied) => {
                    let confirmed_state = playback.confirmed_state();
                    let pending_checkpoint = state_store.submit(&confirmed_state);
                    checkpoint(pending_checkpoint)
                        .await
                        .map(|()| redwax_player::QueueEdit::Applied)
                        .map_err(|error| {
                            format!("Queue changed, but redwax could not save the new state: {error}")
                        })
                }
                other_result => other_result,
            };
            let _ = event_sender.send(AppEvent::QueueCleared { result: clear_result });
        });
    }

    fn open_logout_confirmation(&mut self) {
        if !self.source.signed_in() {
            self.notify("Already signed out", Some("Account"), Level::Info);
            return;
        }
        if self.deps.session_logout.is_none() {
            self.notify("Logout is unavailable", Some("Account"), Level::Warning);
            return;
        }
        self.eq.hide();
        self.open_overlay(Overlay::Prompt(Prompt::Logout(
            ui::confirm::ConfirmationState::default(),
        )));
    }

    fn start_logout(&mut self, reauthenticate: bool) {
        if self.session_logged_out {
            return;
        }
        self.session_logged_out = true;
        self.session_logout_pending = true;
        self.cancel_account_reads();
        self.account_choices_request.cancel();
        self.account_change_request.cancel();
        self.account.clear();
        self.login_state = LoginState::SignedOut;
        self.close_overlay();
        let Some(logout_session) = self.deps.session_logout.clone() else {
            self.handle_event(AppEvent::SessionLoggedOut {
                error: None,
                reauthenticate,
            });
            return;
        };
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let logout_task_result = tokio::task::spawn_blocking(move || logout_session()).await;
            let logout_error = match logout_task_result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some("logout did not finish".to_string()),
            };
            let _ = event_sender.send(AppEvent::SessionLoggedOut {
                error: logout_error,
                reauthenticate,
            });
        });
    }

    fn jump_section(&mut self, forward: bool) {
        if let Some(browser) = self.active_browser_mut() {
            browser.jump_section(forward);
        }
        self.arm_rest();
    }

    fn back(&mut self) {
        if matches!(self.prompt(), Some(Prompt::Find(_))) {
            self.find_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            return;
        }
        if self.search_focused() {
            self.interaction.focus(PageFocus::Browse);
            return;
        }
        let Some(browser) = self.active_browser_mut() else {
            return;
        };
        if browser.back()
            && let Some(target) = self.tab.target()
        {
            self.leave_page(target);
        }
    }

    fn reload(&mut self) {
        if self.player_state.availability == redwax_player::PlaybackAvailability::Unavailable
            || self.playback.needs_reconciliation()
        {
            self.command(|player| async move { player.retry_playback().await });
        }
        match self.tab {
            Tab::Home => self.load_page(Target::Home, Some("home".to_string()), true),
            Tab::Library => self.load_page(Target::Library, Some("library".to_string()), true),
            _ => {}
        }
    }

    fn submit_search(&mut self) {
        let search_query = self.search_query.trim().to_string();
        self.interaction.focus(PageFocus::Browse);
        if search_query.is_empty() {
            return;
        }
        let cache_key = crate::page_cache::search_key(&search_query);
        self.fetch(Target::Search, Fetch::Search(search_query), Some(cache_key), true);
    }

    fn open_album(&mut self) {
        let selected_track = self.highlighted_track();
        let Some(track) = selected_track.cloned() else {
            return;
        };
        let Some(album_browse_id) = track.album_browse_id.clone() else {
            self.notify("This track has no album page", Some("Album"), Level::Info);
            return;
        };
        let album = Album {
            browse_id: album_browse_id,
            title: track.album.clone().unwrap_or_else(|| track.title.clone()),
            artists: track.artists.clone(),
            year: None,
            thumbnail_url: track.thumbnail_url.clone(),
        };
        let browser_target = match self.tab.target() {
            Some(browser_target @ (Target::Home | Target::Search | Target::Library)) => browser_target,
            Some(Target::Queue) | None => {
                self.switch_tab(Tab::Home);
                Target::Home
            }
        };
        let browsable = Browsable::Album(album);
        self.fetch(
            browser_target,
            Fetch::Browse(browsable.clone()),
            Some(browsable.key()),
            false,
        );
    }

    fn open_selected(&mut self) {
        if self.tab == Tab::Search && !self.search_focused() && self.search.selected().is_none() {
            self.focus_search();
            return;
        }
        if self.tab == Tab::Queue {
            let Some((_, selected_item_index)) = self.queue.selected() else {
                return;
            };
            let queue_index = self.first_queue_index + selected_item_index;
            let start_position =
                match self.playback.restored_pending() && self.player_state.index == Some(queue_index) {
                    true => self.player_state.position,
                    false => 0.0,
                };
            self.command(move |player| async move { player.select(queue_index, start_position).await });
            return;
        }
        let Some(browser) = self.active_browser() else {
            return;
        };
        let Some((shelf_index, item_index)) = browser.selected() else {
            return;
        };
        let Some(page) = browser.top() else {
            return;
        };
        let shelf = &page.shelves[shelf_index];
        match &shelf.items[item_index] {
            Item::Track(_) => {
                let tracks: Vec<Track> = shelf.tracks().cloned().collect();
                let selected_track_index = shelf_track_index(shelf, item_index);
                let page_tail_key = self
                    .tab
                    .target()
                    .and_then(|target| self.page_loaders.get(&target))
                    .map(|loader| loader.page_key.clone());
                self.command(move |player| async move {
                    player
                        .play_page_tail(tracks, selected_track_index, page_tail_key)
                        .await
                });
            }
            selected_item => {
                let Some(browsable) = selected_item.as_browsable() else {
                    return;
                };
                let Some(browser_target) = self.tab.target() else {
                    return;
                };
                let cache_key = browsable.key();
                self.fetch(browser_target, Fetch::Browse(browsable), Some(cache_key), false);
            }
        }
    }

    fn queue_highlighted(&mut self, play_next: bool) {
        if self.tab == Tab::Queue {
            return;
        }
        let Some(selected_item) = self.active_browser().and_then(Browser::selected_item).cloned() else {
            return;
        };
        let queue_destination = match play_next {
            true => "next",
            false => "to queue",
        };
        match selected_item {
            Item::Track(track) => {
                let track_description = track_label(&track);
                self.command(move |player| async move { player.enqueue(vec![track], play_next).await });
                self.notify(
                    format!("{track_description} added {queue_destination}"),
                    None,
                    Level::Info,
                );
            }
            selected_item => {
                let Some(browsable) = selected_item.as_browsable() else {
                    return;
                };
                let source = self.source.clone();
                let event_sender = self.events.clone();
                self.background_tasks.spawn(async move {
                    let complete_page = match source.browse(&browsable).await {
                        Ok(page) => collect_page(source.as_ref(), page)
                            .await
                            .map_err(|incomplete| incomplete.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    match complete_page {
                        Ok(complete_page) => {
                            let tracks = complete_page.as_page().tracks().cloned().collect();
                            let _ = event_sender.send(AppEvent::TracksReady { tracks, play_next });
                        }
                        Err(error) => {
                            let _ = event_sender.send(AppEvent::Notify {
                                text: error,
                                title: Some("YouTube Music".to_string()),
                                level: Level::Error,
                            });
                        }
                    }
                });
            }
        }
    }

    fn radio(&mut self) {
        let Some(seed) = self.seed_track() else {
            self.notify("Highlight a track to start a radio from it", None, Level::Info);
            return;
        };
        self.radio_from(seed);
    }

    fn radio_from(&mut self, seed: Track) {
        let from_playing = self
            .player_state
            .track()
            .is_some_and(|track| track.video_id == seed.video_id);
        let seed_description = track_label(&seed);
        self.replace_notification(
            format!("Starting radio from {seed_description}"),
            "Radio",
            Level::Info,
        );
        let source = self.source.clone();
        let event_sender = self.events.clone();
        let station_generation = self.playback.request_station(seed.clone());
        let station_task = self.background_tasks.spawn(async move {
            match source.start_radio(&seed).await {
                Ok((tracks, continuation)) => {
                    let _ = event_sender.send(AppEvent::RadioReady {
                        station_generation,
                        seed,
                        tracks,
                        continuation,
                        from_playing,
                    });
                }
                Err(error) => {
                    let _ = event_sender.send(AppEvent::RadioFailed {
                        station_generation,
                        error: error.to_string(),
                    });
                }
            }
        });
        self.playback.hold_station_task(station_generation, station_task);
    }

    fn pump_radio(&mut self) {
        let station_effect = self
            .playback
            .transition_station(StationEvent::Tick(self.player_state.clone()), clock());
        self.update_radio_label();
        self.apply_station_effect(station_effect);
    }

    fn apply_station_effect(&mut self, station_effect: StationEffect) {
        match station_effect {
            StationEffect::None => {}
            StationEffect::Cancelled => self.replace_notification(
                "Radio cancelled because playback changed",
                "Radio",
                Level::Warning,
            ),
            StationEffect::Empty => {
                self.replace_notification("Radio returned no tracks", "Radio", Level::Warning)
            }
            StationEffect::Started(label) => {
                self.replace_notification(format!("Radio from {label}"), "Radio", Level::Info);
                self.pump_radio();
            }
            StationEffect::Retry {
                error,
                refresh_session,
            } => {
                if refresh_session {
                    self.refresh_session(None);
                }
                self.notify(
                    format!("{error}. Retrying shortly."),
                    Some("Radio"),
                    Level::Warning,
                );
            }
            StationEffect::Error(error) => {
                self.replace_notification(error, "Radio", Level::Error);
            }
            StationEffect::Apply {
                station_generation,
                label,
                tracks,
                upcoming,
            } => {
                let playback = self.playback.clone();
                let event_sender = self.events.clone();
                let apply_task = self.background_tasks.spawn(async move {
                    let apply_result = if upcoming {
                        playback
                            .apply_radio_upcoming(station_generation, tracks)
                            .await
                            .map(|_| ())
                    } else {
                        playback.apply_radio(station_generation, tracks).await.map(|_| ())
                    };
                    let _ = event_sender.send(AppEvent::RadioApplied {
                        station_generation,
                        label,
                        error: apply_result.err().map(|error| error.to_string()),
                    });
                });
                self.playback.hold_station_task(station_generation, apply_task);
            }
            StationEffect::Append {
                station_generation,
                tracks,
            } => {
                let playback = self.playback.clone();
                let event_sender = self.events.clone();
                let append_task = self.background_tasks.spawn(async move {
                    let append_error = playback
                        .append_radio(station_generation, tracks)
                        .await
                        .err()
                        .map(|error| error.to_string());
                    let _ = event_sender.send(AppEvent::RadioAppended {
                        station_generation,
                        error: append_error,
                    });
                });
                self.playback.hold_station_task(station_generation, append_task);
            }
            StationEffect::Fetch {
                station_generation,
                seed,
                continuation,
            } => {
                self.fetch_station_more(station_generation, seed, continuation);
            }
        }
    }

    fn fetch_station_more(
        &mut self,
        station_generation: u64,
        seed: Track,
        continuation: Option<redwax_ytm::RadioContinuation>,
    ) {
        let source = self.source.clone();
        let event_sender = self.events.clone();
        let fetch_task = self.background_tasks.spawn(async move {
            let fetch_result = match continuation {
                Some(continuation) => source.continue_radio(&continuation).await,
                None => source.start_radio(&seed).await,
            };
            match fetch_result {
                Ok((tracks, continuation)) => {
                    let _ = event_sender.send(AppEvent::RadioMore {
                        station_generation,
                        tracks,
                        continuation,
                    });
                }
                Err(error) => {
                    let _ = event_sender.send(AppEvent::RadioMoreFailed {
                        station_generation,
                        rejected: matches!(error, redwax_ytm::Error::Rejected),
                        error: error.to_string(),
                    });
                }
            }
        });
        self.playback.hold_station_task(station_generation, fetch_task);
    }

    fn update_radio_label(&mut self) {
        let radio_enabled = self.playback.radio_enabled();
        if self.queue.set_queue_label(radio_enabled) {
            self.screen.touch();
        }
    }

    fn like(&mut self, track: Option<Track>, missing: &'static str) {
        let Some(track) = track else {
            self.notify(missing, None, Level::Info);
            return;
        };
        let liked = !self.is_liked(&track);
        self.set_liked(track, liked);
    }

    fn set_liked(&mut self, track: Track, liked: bool) {
        if !self.source.signed_in() {
            self.notify("Run `redwax login` to like tracks", None, Level::Warning);
            return;
        }
        let confirmed = self.is_liked(&track);
        self.liked_overrides.insert(track.video_id.clone(), liked);
        if let Some(like_request) = self.like_intents.desire(track, liked, confirmed) {
            self.send_like(like_request);
        }
    }

    fn send_like(&mut self, like_request: LikeRequest) {
        let source = self.source.clone();
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let like_error = source
                .set_liked(&like_request.track, like_request.liked)
                .await
                .err()
                .map(|error| error.to_string());
            let _ = event_sender.send(AppEvent::LikeApplied {
                request: like_request,
                error: like_error,
            });
        });
    }

    fn toggle_shuffle(&mut self) {
        let playback = self.playback.clone();
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let (notification_text, notification_title, notification_level) =
                match playback.toggle_shuffle().await {
                    Ok(true) => ("Shuffle on".to_string(), None, Level::Info),
                    Ok(false) => ("Shuffle off".to_string(), None, Level::Info),
                    Err(error) => (error.to_string(), Some("Playback".to_string()), Level::Error),
                };
            let _ = event_sender.send(AppEvent::Notify {
                text: notification_text,
                title: notification_title,
                level: notification_level,
            });
        });
    }

    fn cycle_repeat(&mut self) {
        let playback = self.playback.clone();
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let (notification_text, notification_title, notification_level) =
                match playback.cycle_repeat().await {
                    Ok(Repeat::Off) => ("Repeat off".to_string(), None, Level::Info),
                    Ok(Repeat::All) => ("Repeat queue".to_string(), None, Level::Info),
                    Ok(Repeat::One) => ("Repeat track".to_string(), None, Level::Info),
                    Err(error) => (error.to_string(), Some("Playback".to_string()), Level::Error),
                };
            let _ = event_sender.send(AppEvent::Notify {
                text: notification_text,
                title: notification_title,
                level: notification_level,
            });
        });
    }

    fn jump_to_queue(&mut self) {
        self.switch_tab(Tab::Queue);
        let Some(playing_index) = self.player_state.index else {
            return;
        };
        let visible_item_index = playing_index.saturating_sub(self.first_queue_index);
        let projected_row_index =
            self.queue.rows().iter().position(
                |row| matches!(row, ui::Row::Item(_, item_index) if *item_index == visible_item_index),
            );
        if let Some(projected_row_index) = projected_row_index {
            self.queue.select_row(Some(projected_row_index));
        }
    }

    fn remove_from_playlist(&mut self) {
        let on_playlist_page = self.active_browser().and_then(Browser::open_playlist).is_some();
        match on_playlist_page {
            true => self.remove_highlighted_row(),
            false => self.open_remove_picker(),
        }
    }

    fn remove_highlighted_row(&mut self) {
        let Some(browser_target) = self.tab.target() else {
            return;
        };
        let Some(playlist) = self.active_browser().and_then(Browser::open_playlist).cloned() else {
            return;
        };
        if playlist.playlist_id == redwax_ytm::LIKED_SONGS_PLAYLIST_ID {
            if let Some(track) = self.highlighted_track().cloned() {
                self.set_liked(track, false);
            }
            return;
        }
        let Some(track) = self.highlighted_track().cloned() else {
            return;
        };
        let Some(playlist_entry) = track.playlist_entry.clone() else {
            self.notify(
                format!("{} is not yours to edit", playlist.title),
                Some("Playlist"),
                Level::Warning,
            );
            return;
        };
        let Some(browser_removal) = self.browser_mut(browser_target).remove_selected_playlist_item() else {
            return;
        };

        let source = self.source.clone();
        let event_sender = self.events.clone();
        let playlist_id = playlist.playlist_id.clone();
        let track_description = track_label(&track);
        let playlist_title = playlist.title.clone();
        self.background_tasks.spawn(async move {
            let completion_event = match source
                .remove_playlist_entry(&playlist_id, &track.video_id, &playlist_entry)
                .await
            {
                Ok(()) => {
                    let _ = event_sender.send(AppEvent::PlaylistTrackRemoved {
                        playlist_id: playlist_id.clone(),
                        video_id: track.video_id.clone(),
                        playlist_entry: Some(playlist_entry),
                    });
                    AppEvent::Notify {
                        text: format!("{track_description} removed from {playlist_title}"),
                        title: None,
                        level: Level::Info,
                    }
                }
                Err(error) => AppEvent::PlaylistRemoveFailed {
                    target: browser_target,
                    removal: browser_removal,
                    error: error.to_string(),
                },
            };
            let _ = event_sender.send(completion_event);
        });
    }

    fn playlist_track_removed(
        &mut self,
        playlist_id: &str,
        video_id: &str,
        playlist_entry: Option<&PlaylistEntry>,
    ) {
        for browser in [&mut self.home, &mut self.search, &mut self.library] {
            browser.remove_playlist_track(playlist_id, video_id, playlist_entry);
        }
        let cache_key = format!("playlist:{playlist_id}");
        if let Some(request_slot) = self.prefetch_requests.get_mut(&cache_key) {
            request_slot.cancel();
        }
        if let Some(mut cached_page) = self.deps.page_cache.get(&cache_key) {
            cached_page.remove_playlist_track(video_id, playlist_entry);
            if let Ok(complete_page) = CompletePage::try_from(cached_page) {
                self.deps.page_cache.put(&cache_key, &complete_page);
            }
        }
    }

    /// Ask which playlist takes the track that plays, with the highlighted row
    /// a `tab` away. The tracks of an album or playlist are fetched once the
    /// choice is made, so opening the picker costs no request.
    fn open_add_picker(&mut self, highlighted_only: bool) {
        let playing_subject = self
            .player_state
            .track()
            .filter(|_| !highlighted_only)
            .map(|track| Subject {
                label: track_label(track),
                edit: Edit::Add(Addition::Track(track.video_id.clone())),
            });
        let highlighted_subject = self
            .active_browser()
            .and_then(Browser::selected_item)
            .and_then(|item| {
                let (subject_label, addition) = match item {
                    Item::Track(track) => (track_label(track), Addition::Track(track.video_id.clone())),
                    selected_item => (
                        selected_item.title().to_string(),
                        Addition::Whole(selected_item.as_browsable()?),
                    ),
                };
                Some(Subject {
                    label: subject_label,
                    edit: Edit::Add(addition),
                })
            });
        self.open_picker(
            playing_subject,
            highlighted_subject,
            "Play or highlight something to add it to a playlist",
        );
    }

    /// Ask which playlist loses the track that plays, with the highlighted
    /// track a `tab` away.
    fn open_remove_picker(&mut self) {
        let track_subject = |track: &Track| Subject {
            label: track_label(track),
            edit: Edit::Remove(track.clone()),
        };
        let playing_subject = self.player_state.track().map(track_subject);
        let highlighted_subject = self.highlighted_track().map(track_subject);
        self.open_picker(
            playing_subject,
            highlighted_subject,
            "Play or highlight a track to remove it from a playlist",
        );
    }

    /// Open the picker on the track that plays, with the highlighted row as the
    /// other choice. With nothing playing, the row is the only choice.
    fn open_picker(
        &mut self,
        playing_subject: Option<Subject>,
        highlighted_subject: Option<Subject>,
        missing_subject_message: &'static str,
    ) {
        if !self.source.signed_in() {
            self.notify("Run `redwax login` to edit a playlist", None, Level::Warning);
            return;
        }
        let (primary_subject, alternate_subject) = match (playing_subject, highlighted_subject) {
            (Some(playing_subject), highlighted_subject) => {
                let alternate_subject = highlighted_subject
                    .filter(|highlighted_subject| !highlighted_subject.same_track(&playing_subject));
                (playing_subject, alternate_subject)
            }
            (None, Some(highlighted_subject)) => (highlighted_subject, None),
            (None, None) => {
                self.notify(missing_subject_message, None, Level::Info);
                return;
            }
        };
        let playlists = self.account_playlists();
        if playlists.is_empty() && matches!(primary_subject.edit, Edit::Remove(_)) {
            self.notify("No playlists in your library yet", None, Level::Info);
            return;
        }
        self.eq.hide();
        self.open_overlay(Overlay::PlaylistPicker(Box::new(PlaylistPicker {
            playlists,
            selected_playlist_index: 0,
            primary_subject,
            alternate_subject,
            new_playlist_name_input: None,
        })));
    }

    /// Generated mixes and collection entries cannot use the playlist edit endpoint.
    fn account_playlists(&self) -> Vec<Playlist> {
        let Some(page) = self.library.pages().next() else {
            return Vec::new();
        };
        page.shelves
            .iter()
            .flat_map(|shelf| &shelf.items)
            .filter_map(|item| match item {
                Item::Playlist(playlist) => Some(playlist.clone()),
                _ => None,
            })
            .filter(|playlist| {
                playlist
                    .playlist_id
                    .strip_prefix("VL")
                    .unwrap_or(&playlist.playlist_id)
                    .starts_with("PL")
            })
            .collect()
    }

    fn refresh_picker_playlists(&mut self) {
        let playlists = self.account_playlists();
        let Some(picker) = self.playlist_picker_mut() else {
            return;
        };
        let selected_playlist_id = picker
            .selected_playlist()
            .map(|playlist| playlist.playlist_id.clone());
        let selected_new_playlist =
            !picker.remove_mode() && picker.selected_playlist_index == picker.playlists.len();
        picker.playlists = playlists;
        picker.selected_playlist_index = match selected_playlist_id {
            Some(playlist_id) => picker
                .playlists
                .iter()
                .position(|playlist| playlist.playlist_id == playlist_id)
                .unwrap_or(0),
            None if selected_new_playlist => picker.playlists.len(),
            None => 0,
        };
    }

    fn choose_playlist(&mut self) {
        let Some(picker) = self.take_playlist_picker() else {
            return;
        };
        let Some(playlist) = picker.selected_playlist().cloned() else {
            return;
        };
        let Subject {
            label: subject_label,
            edit: playlist_edit,
        } = picker.primary_subject;
        let source = self.source.clone();
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let (notification_text, notification_level) = match playlist_edit {
                Edit::Add(addition) => add_tracks(source.as_ref(), &playlist, addition, &subject_label).await,
                Edit::Remove(track) => {
                    let removal_result =
                        remove_track(source.as_ref(), &playlist, &track, &subject_label).await;
                    if matches!(removal_result.1, Level::Info) {
                        let _ = event_sender.send(AppEvent::PlaylistTrackRemoved {
                            playlist_id: playlist.playlist_id.clone(),
                            video_id: track.video_id,
                            playlist_entry: None,
                        });
                    }
                    removal_result
                }
            };
            let _ = event_sender.send(AppEvent::Notify {
                text: notification_text,
                title: Some("Playlist".to_string()),
                level: notification_level,
            });
        });
    }

    /// Make the playlist the picker named, holding what the picker was adding.
    /// The library is fetched again once it exists, so the next picker lists it.
    fn create_playlist(&mut self) {
        let Some(picker) = self.take_playlist_picker() else {
            return;
        };
        let Some(playlist_name) = picker
            .new_playlist_name_input
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            return;
        };
        let playlist_name = playlist_name.to_string();
        let Subject {
            label: subject_label,
            edit: Edit::Add(addition),
        } = picker.primary_subject
        else {
            return;
        };
        let source = self.source.clone();
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let creation_result = match addition_ids(source.as_ref(), addition).await {
                Ok(video_ids) => source
                    .create_playlist(&playlist_name, &video_ids)
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error),
            };
            let (notification_text, notification_level) = match creation_result {
                Ok(_) => {
                    let _ = event_sender.send(AppEvent::PlaylistCreated);
                    (format!("Made {playlist_name} with {subject_label}"), Level::Info)
                }
                Err(error) => (error, Level::Error),
            };
            let _ = event_sender.send(AppEvent::Notify {
                text: notification_text,
                title: Some("Playlist".to_string()),
                level: notification_level,
            });
        });
    }

    fn remove_from_queue(&mut self) {
        if self.tab != Tab::Queue {
            return;
        }
        let Some((_, selected_item_index)) = self.queue.selected() else {
            return;
        };
        let queue_index = self.first_queue_index + selected_item_index;
        let queue_entry_id = self
            .player_state
            .queue_entries
            .get(queue_index)
            .map(|queue_entry| queue_entry.id());
        let queue_revision = self.player_state.queue_revision;
        let playback = self.playback.clone();
        let event_sender = self.events.clone();
        let state_store = self.deps.state_store.clone();
        self.background_tasks.spawn(async move {
            let removal_result = match queue_entry_id {
                Some(queue_entry_id) => playback.remove_entry(queue_entry_id, queue_revision).await,
                None => playback
                    .remove(queue_index)
                    .await
                    .map(|()| redwax_player::QueueEdit::Applied),
            }
            .map_err(|error| error.to_string());
            let removal_result = match removal_result {
                Ok(redwax_player::QueueEdit::Applied) => {
                    let confirmed_state = playback.confirmed_state();
                    let pending_checkpoint = state_store.submit(&confirmed_state);
                    checkpoint(pending_checkpoint)
                        .await
                        .map(|()| redwax_player::QueueEdit::Applied)
                        .map_err(|error| {
                            format!("Queue changed, but redwax could not save the new state: {error}")
                        })
                }
                other_result => other_result,
            };
            let _ = event_sender.send(AppEvent::QueueEntryRemoved {
                result: removal_result,
            });
        });
    }

    fn move_queue_entry(&mut self, direction: isize) {
        if self.tab != Tab::Queue {
            return;
        }
        let Some((_, selected_item_index)) = self.queue.selected() else {
            return;
        };
        let queue_index = self.first_queue_index + selected_item_index;
        let Some(queue_entry_id) = self
            .player_state
            .queue_entries
            .get(queue_index)
            .map(|queue_entry| queue_entry.id())
        else {
            self.notify("Wait for the queue to finish loading", Some("Queue"), Level::Info);
            return;
        };
        let Some(destination_index) = queue_index
            .checked_add_signed(direction)
            .filter(|destination_index| *destination_index < self.player_state.queue.len())
        else {
            return;
        };
        let queue_revision = self.player_state.queue_revision;
        let playback = self.playback.clone();
        let event_sender = self.events.clone();
        let state_store = self.deps.state_store.clone();
        self.background_tasks.spawn(async move {
            let move_result = playback
                .move_entry(queue_entry_id, queue_revision, destination_index)
                .await
                .map_err(|error| error.to_string());
            let move_result = match move_result {
                Ok(redwax_player::QueueEdit::Applied) => {
                    let confirmed_state = playback.confirmed_state();
                    let pending_checkpoint = state_store.submit(&confirmed_state);
                    checkpoint(pending_checkpoint)
                        .await
                        .map(|()| redwax_player::QueueEdit::Applied)
                        .map_err(|error| {
                            format!("Queue changed, but redwax could not save the new state: {error}")
                        })
                }
                other_result => other_result,
            };
            let _ = event_sender.send(AppEvent::QueueEntryMoved { result: move_result });
        });
    }

    fn seed_track(&self) -> Option<Track> {
        self.highlighted_track()
            .cloned()
            .or_else(|| self.player_state.track().cloned())
    }

    // --------------------------------------------------------------- events

    pub fn handle_event(&mut self, app_event: AppEvent) {
        if !matches!(app_event, AppEvent::Tick) {
            self.screen.touch();
        }
        match app_event {
            AppEvent::PageReady {
                target,
                request: request_id,
                page,
            } => self.page_ready(target, request_id, page),
            AppEvent::PageFailed {
                target,
                request: request_id,
                error,
                rejected,
            } => {
                let Some(fetch_context) = self
                    .page_requests
                    .get_mut(&target)
                    .and_then(|request_slot| request_slot.complete(request_id))
                    .cloned()
                else {
                    return;
                };
                self.browser_mut(target).set_busy(false);
                if rejected
                    && !fetch_context.retry_attempted
                    && self.refresh_session(Some(Retry::Page(target, request_id)))
                {
                    return;
                }
                if rejected {
                    self.start_logout(true);
                    return;
                }
                // A remembered page is still on screen, so a failure to refresh
                // it is a warning and not an error.
                let notification_level = match fetch_context.page_was_shown {
                    true => Level::Warning,
                    false => Level::Error,
                };
                self.notify(error, Some("YouTube Music"), notification_level);
            }
            AppEvent::PageMore {
                target,
                request: request_id,
                key: cache_key,
                result: continuation_result,
            } => self.page_more(target, request_id, cache_key, continuation_result),
            AppEvent::PagePrefetched { key, request, page } => {
                let Some(request_slot) = self.prefetch_requests.get_mut(&key) else {
                    return;
                };
                if request_slot.complete(request).is_none() {
                    return;
                }
                request_slot.cancel();
                if let Some(page) = page
                    && self.deps.page_cache.get(&key).is_none()
                {
                    self.deps.page_cache.put(&key, &page);
                }
            }
            AppEvent::DurationsReady(found_durations) => self.durations_ready(found_durations),
            AppEvent::GenreReady { video_id, genre } => self.genre_ready(video_id, genre),
            AppEvent::TracksReady { tracks, play_next } => {
                let track_count = tracks.len();
                let queue_destination = match play_next {
                    true => "next",
                    false => "to queue",
                };
                self.command(move |player| async move { player.enqueue(tracks, play_next).await });
                self.notify(
                    format!("{track_count} tracks added {queue_destination}"),
                    None,
                    Level::Info,
                );
            }
            AppEvent::RadioReady {
                station_generation,
                seed,
                tracks,
                continuation,
                from_playing,
            } => {
                let playing_video_id = self
                    .playback
                    .snapshot()
                    .track()
                    .map(|track| track.video_id.clone());
                let station_effect = self.playback.transition_station(
                    StationEvent::Ready {
                        station_generation,
                        seed,
                        tracks,
                        continuation,
                        from_playing,
                        playing_video_id,
                    },
                    clock(),
                );
                self.apply_station_effect(station_effect);
            }
            AppEvent::RadioFailed {
                station_generation,
                error,
            } => {
                let station_effect = self.playback.transition_station(
                    StationEvent::Failed {
                        station_generation,
                        error,
                    },
                    clock(),
                );
                self.apply_station_effect(station_effect);
            }
            AppEvent::RadioApplied {
                station_generation,
                label,
                error,
            } => {
                let station_effect = self.playback.transition_station(
                    StationEvent::Applied {
                        station_generation,
                        label,
                        error,
                    },
                    clock(),
                );
                self.apply_station_effect(station_effect);
            }
            AppEvent::RadioMore {
                station_generation,
                tracks,
                continuation,
            } => {
                let station_effect = self.playback.transition_station(
                    StationEvent::More {
                        station_generation,
                        tracks,
                        continuation,
                        player_state: self.playback.snapshot(),
                    },
                    clock(),
                );
                self.apply_station_effect(station_effect);
            }
            AppEvent::RadioMoreFailed {
                station_generation,
                error,
                rejected,
            } => {
                let station_effect = self.playback.transition_station(
                    StationEvent::MoreFailed {
                        station_generation,
                        error,
                        rejected,
                    },
                    clock(),
                );
                self.apply_station_effect(station_effect);
            }
            AppEvent::RadioAppended {
                station_generation,
                error,
            } => {
                let station_effect = self.playback.transition_station(
                    StationEvent::Appended {
                        station_generation,
                        error,
                    },
                    clock(),
                );
                self.apply_station_effect(station_effect);
            }
            AppEvent::QueueCleared { result } => match result {
                Ok(redwax_player::QueueEdit::Applied) => {
                    self.notify("Upcoming queue cleared", None, Level::Info)
                }
                Ok(redwax_player::QueueEdit::Stale) => self.notify(
                    "The queue changed. Review it before clearing.",
                    Some("Queue"),
                    Level::Info,
                ),
                Err(error) => self.notify(error, Some("Playback"), Level::Error),
            },
            AppEvent::QueueEntryRemoved { result } => match result {
                Ok(redwax_player::QueueEdit::Applied) => {}
                Ok(redwax_player::QueueEdit::Stale) => self.notify(
                    "That queue entry is no longer present",
                    Some("Queue"),
                    Level::Info,
                ),
                Err(error) => self.notify(error, Some("Playback"), Level::Error),
            },
            AppEvent::QueueEntryMoved { result } => match result {
                Ok(redwax_player::QueueEdit::Applied) => self.notify("Queue entry moved", None, Level::Info),
                Ok(redwax_player::QueueEdit::Stale) => self.notify(
                    "That queue entry is no longer present",
                    Some("Queue"),
                    Level::Info,
                ),
                Err(error) => self.notify(error, Some("Playback"), Level::Error),
            },
            AppEvent::SessionExpired { generation } => {
                if generation == self.deps.session_generation.load(Ordering::SeqCst) {
                    self.start_logout(true);
                }
            }
            AppEvent::SessionLoggedOut {
                error,
                reauthenticate,
            } => {
                self.session_logout_pending = false;
                self.cancel_account_reads();
                self.liked_overrides.clear();
                self.like_intents.forget_account();
                if self.deps.page_cache.clear().is_err() {
                    self.notify(
                        "Old page cache could not be cleared.",
                        Some("Account"),
                        Level::Warning,
                    );
                }
                self.account.clear();
                self.login_state = LoginState::SignedOut;
                self.session_logged_out = true;
                self.home.replace(Page::new("Home", Vec::new()));
                self.search.replace(Page::new("Search", Vec::new()));
                let mut signed_out_library = Page::new("Library", Vec::new());
                signed_out_library.subtitle = Some("sign in to see your library".to_string());
                self.library.replace(signed_out_library);
                match error {
                    Some(error) => self.notify(
                        format!("Signed out, but credential cleanup failed: {error}"),
                        Some("Logout"),
                        Level::Warning,
                    ),
                    None => self.notify("Signed out", Some("Account"), Level::Info),
                }
                if reauthenticate {
                    self.open_account_picker();
                    self.notify(
                        "Automatic sign-in failed. Sign in to YouTube Music in your browser. Then choose an account here or run `redwax login`.",
                        Some("Sign in again"),
                        Level::Warning,
                    );
                }
            }
            AppEvent::LyricsReady {
                request: request_id,
                track,
                lyrics,
            } => {
                self.lyrics.landed(request_id, track.video_id, lyrics);
            }
            AppEvent::ArtReady {
                request: request_id,
                video_id,
                image,
            } => self.art_ready(request_id, video_id, image),
            AppEvent::AccentReady {
                request: request_id,
                video_id,
                accent,
            } => {
                if self.art.request_slot.get(request_id).is_some()
                    && self.art.requested_video_id.as_deref() == Some(video_id.as_str())
                {
                    self.art.accent_color = accent;
                    let resolved_theme = self.theme_controller.resolve_or_cached(&self.deps.config);
                    self.base_theme = self.theme_controller.present(
                        resolved_theme,
                        self.deps.config.transparent,
                        self.art.accent_color,
                    );
                }
            }
            AppEvent::AccountReady {
                request: request_id,
                result: account_result,
            } => {
                if self.session_logged_out {
                    return;
                }
                let Some(retry_attempted) = self.account_request.complete(request_id).copied() else {
                    return;
                };
                match account_result {
                    Ok(Some(identity)) if !identity.is_empty() => {
                        self.account = identity;
                        self.login_state = LoginState::SignedIn;
                        self.load_page(Target::Library, Some("library".to_string()), false);
                    }
                    Ok(_)
                        if self.source.signed_in()
                            && self.account.is_empty()
                            && !retry_attempted
                            && self.refresh_session(Some(Retry::Account(request_id))) =>
                    {
                        self.login_state = LoginState::Checking;
                    }
                    Ok(_) if self.source.signed_in() && retry_attempted => {
                        self.start_logout(true);
                    }
                    Ok(_) => {
                        self.login_state = if self.source.signed_in() {
                            if self.account.is_empty() {
                                LoginState::Unconfirmed
                            } else {
                                LoginState::SignedIn
                            }
                        } else {
                            self.account.clear();
                            LoginState::SignedOut
                        };
                        self.load_page(Target::Library, Some("library".to_string()), false);
                    }
                    Err(redwax_ytm::Error::Rejected) if !retry_attempted => {
                        self.account.clear();
                        self.login_state = if self.refresh_session(Some(Retry::Account(request_id))) {
                            LoginState::Checking
                        } else {
                            self.start_logout(true);
                            LoginState::SignedOut
                        };
                    }
                    Err(error) => {
                        if matches!(error, redwax_ytm::Error::Rejected | redwax_ytm::Error::SignedOut) {
                            self.start_logout(true);
                        } else if self.account.is_empty() {
                            self.login_state = LoginState::Unconfirmed;
                        }
                        self.notify(error.to_string(), Some("YouTube Music"), Level::Error);
                    }
                }
            }
            AppEvent::AccountChoices { request, result } => {
                if self.account_choices_request.complete(request).is_none() {
                    return;
                }
                let current = self.account.clone();
                if let Some(picker) = self.account_picker_mut() {
                    picker.loaded(result, &current);
                }
            }
            AppEvent::AccountChanged { request, result } => {
                if self.account_change_request.complete(request).is_none() {
                    return;
                }
                match result {
                    Ok(identity) => self.account_changed(identity),
                    Err(error) => {
                        if let Some(picker) = self.account_picker_mut() {
                            picker.status = ui::accounts::Status::Failed(error.clone());
                        }
                        self.notify(error, Some("Account"), Level::Error);
                    }
                }
            }
            AppEvent::LikeApplied {
                request: like_request,
                error,
            } => match self.like_intents.complete(like_request, error) {
                crate::likes::Completion::Obsolete => {}
                crate::likes::Completion::Next(next_request) => self.send_like(next_request),
                crate::likes::Completion::Settled {
                    track,
                    confirmed_liked,
                    error,
                } => {
                    self.liked_overrides
                        .insert(track.video_id.clone(), confirmed_liked);
                    if let Some(error) = error {
                        self.notify(error, Some("YouTube Music"), Level::Error);
                        return;
                    }
                    if !confirmed_liked {
                        self.playlist_track_removed(
                            redwax_ytm::LIKED_SONGS_PLAYLIST_ID,
                            &track.video_id,
                            None,
                        );
                    }
                    let verb = if confirmed_liked { "Liked" } else { "Unliked" };
                    self.notify(format!("{verb} {}", track_label(&track)), None, Level::Info);
                }
            },
            AppEvent::PlaylistTrackRemoved {
                playlist_id,
                video_id,
                playlist_entry,
            } => self.playlist_track_removed(&playlist_id, &video_id, playlist_entry.as_ref()),
            AppEvent::PlaylistRemoveFailed {
                target,
                removal,
                error,
            } => {
                self.browser_mut(target).restore_removal(removal);
                self.notify(error, Some("Playlist"), Level::Error);
            }
            AppEvent::PlaylistCreated => {
                self.load_page(Target::Library, Some("library".to_string()), false);
            }
            AppEvent::SessionRefreshed { error, retry } => self.session_refreshed(error, retry),
            AppEvent::PlayerChanged(state) => self.player_changed(state),
            AppEvent::PaletteChanged { request, palette } => {
                if let Some(theme) =
                    self.theme_controller
                        .accept_live_and_cache(request, &palette, &self.deps.config)
                {
                    self.base_theme = self.theme_controller.present(
                        theme,
                        self.deps.config.transparent,
                        self.art.accent_color,
                    );
                }
            }
            AppEvent::OmarchyPaletteChanged { request, palette } => {
                if let Some(theme) = self.theme_controller.accept_validated_live_and_cache(
                    request,
                    &palette,
                    &self.deps.config,
                ) {
                    self.base_theme = self.theme_controller.present(
                        theme,
                        self.deps.config.transparent,
                        self.art.accent_color,
                    );
                }
            }
            AppEvent::Notify { text, title, level } => {
                self.notify(text, title.as_deref(), level);
            }
            AppEvent::Tick => self.tick(),
        }
    }

    fn tick(&mut self) {
        let now_millis = clock();
        let previous_notification_count = self.notifications.len();
        self.notifications
            .retain(|notification| notification.expires_at_millis > now_millis);
        // A toast changes shade twice in its last second, so that second is
        // the only idle stretch a tick still has to repaint.
        let notification_is_fading = self.notifications.len() != previous_notification_count
            || self
                .notifications
                .iter()
                .any(|notification| notification.expires_at_millis.saturating_sub(now_millis) < 1000);
        if notification_is_fading {
            self.screen.touch();
        }
        self.screen.tick(self.animating());
        if self.lyrics.resume_follow(now_millis) && self.tab == Tab::Lyrics {
            self.screen.touch();
        }
        while let Some(task_result) = self.background_tasks.try_join_next() {
            self.report_task_failure(task_result);
        }
        if let Some(target) = self.tab.target() {
            self.pump_loader(target);
            self.start_fill(target);
        }
        self.pump_radio();
        self.duration_cache.write_if_due();
        self.write_eq();
        if let Some(cursor_rest_started_millis) = self.rested_at
            && now_millis.saturating_sub(cursor_rest_started_millis) >= PREFETCH_REST_MILLIS
        {
            self.rested_at = None;
            self.prefetch_highlighted();
        }
        // playerctl quit reaches the app as a flag, because MPRIS runs on its
        // own connection and cannot drive the key path.
        if let Ok(held) = self.mpris.lock()
            && held.as_ref().is_some_and(crate::mpris::Mpris::quit_requested)
        {
            self.quit = true;
        }
        if now_millis.saturating_sub(self.last_state_write) >= STATE_WRITE_SECONDS * 1000 {
            self.last_state_write = now_millis;
            self.write_state();
        }
    }

    pub fn write_state(&mut self) {
        let pending_checkpoint = self.deps.state_store.submit(&self.player_state);
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            if let Err(error) = checkpoint(pending_checkpoint).await {
                let _ = event_sender.send(AppEvent::Notify {
                    text: format!("Playback state could not be saved: {error}"),
                    title: Some("Playback".to_string()),
                    level: Level::Error,
                });
            }
        });
    }

    fn report_task_failure(&mut self, task_result: Result<(), tokio::task::JoinError>) {
        if let Err(error) = task_result
            && !error.is_cancelled()
        {
            self.notify(
                format!("A background task stopped unexpectedly: {error}"),
                Some("Redwax"),
                Level::Error,
            );
        }
    }

    fn player_changed(&mut self, observed_state: PlayerState) {
        let previous_track = self.player_state.track().cloned();
        let selected_queue_index = match self.tab {
            Tab::Queue => self
                .queue
                .selected()
                .map(|(_, item_index)| self.first_queue_index + item_index),
            _ => None,
        };
        let selected_queue_entry = selected_queue_index
            .and_then(|queue_index| self.player_state.queue_entries.get(queue_index))
            .map(|queue_entry| queue_entry.id());
        let coordinated_state = self.playback.observe(observed_state);
        let queue_changed = coordinated_state.queue != self.player_state.queue
            || coordinated_state.queue_revision != self.player_state.queue_revision
            || coordinated_state.index != self.player_state.index;
        let checkpoint_due = queue_changed
            || coordinated_state.repeat != self.player_state.repeat
            || coordinated_state.shuffle != self.player_state.shuffle
            || coordinated_state.muted != self.player_state.muted
            || coordinated_state.volume != self.player_state.volume
            || coordinated_state.paused != self.player_state.paused;
        let new_playback_error = coordinated_state
            .error
            .clone()
            .filter(|error| self.player_state.error.as_ref() != Some(error));
        let new_playback_warning = coordinated_state.warning.clone().filter(|warning| {
            self.player_state.warning.as_ref() != Some(warning)
                || coordinated_state.track().map(|track| &track.video_id)
                    != previous_track.as_ref().map(|track| &track.video_id)
        });
        self.player_state = coordinated_state;
        if checkpoint_due {
            let checkpoint_state = self.player_state.clone();
            let pending_checkpoint = self.deps.state_store.submit(&checkpoint_state);
            let event_sender = self.events.clone();
            self.background_tasks.spawn(async move {
                if let Err(error) = checkpoint(pending_checkpoint).await {
                    let _ = event_sender.send(AppEvent::Notify {
                        text: format!("Playback state could not be saved: {error}"),
                        title: Some("Playback".to_string()),
                        level: Level::Error,
                    });
                }
            });
        }
        if queue_changed {
            let queue_view = queue_view(&self.player_state);
            self.first_queue_index = queue_view.first_queue_index;
            self.queue.replace(queue_view.page);
            let retained_queue_index = selected_queue_entry
                .and_then(|selected_entry_id| {
                    self.player_state
                        .queue_entries
                        .iter()
                        .position(|queue_entry| queue_entry.id() == selected_entry_id)
                })
                .or_else(|| {
                    selected_queue_index
                        .map(|queue_index| queue_index.min(self.player_state.queue.len().saturating_sub(1)))
                });
            if let Some(item_index) =
                retained_queue_index.and_then(|queue_index| queue_index.checked_sub(self.first_queue_index))
            {
                let projected_row_index = self.queue.rows().iter().position(|projected_row| {
                    matches!(projected_row, ui::Row::Item(_, candidate_item_index) if *candidate_item_index == item_index)
                });
                self.queue.select_row(projected_row_index);
            }
        }
        self.pump_radio();

        let current_track = self.player_state.track().cloned();
        if current_track.as_ref().map(|track| &track.video_id)
            != previous_track.as_ref().map(|track| &track.video_id)
        {
            if let Some(track) = &current_track {
                self.load_art(track);
                self.start_genre(track);
                if self.tab == Tab::Lyrics {
                    let track = track.clone();
                    self.load_lyrics(&track);
                }
            } else {
                self.lyrics.clear();
                self.art.request_slot.cancel();
                self.art.render_protocol = None;
                self.art.requested_video_id = None;
                self.art.cover_height = self.deps.config.art_height;
                self.art.accent_color = None;
                let resolved_theme = self.theme_controller.resolve_or_cached(&self.deps.config);
                self.base_theme =
                    self.theme_controller
                        .present(resolved_theme, self.deps.config.transparent, None);
                self.eq.forget_genre();
            }
        }
        if let (Some(current_track), Some(previous_track)) = (&current_track, &previous_track)
            && current_track.video_id == previous_track.video_id
            && current_track.thumbnail_url != previous_track.thumbnail_url
        {
            self.load_art(current_track);
        }
        if let Some(error) = new_playback_error {
            self.notify(error, Some("Playback"), Level::Error);
        }
        if let Some(warning) = new_playback_warning {
            self.notify(warning, Some("Playback"), Level::Warning);
        }
    }

    fn page_ready(&mut self, target: Target, request_id: RequestId, mut page: Page) {
        let Some(fetch_context) = self
            .page_requests
            .get_mut(&target)
            .and_then(|request_slot| request_slot.complete(request_id))
            .cloned()
        else {
            return;
        };
        self.duration_cache.fill(&mut page);
        let prefetch_tracks: Vec<Track> = page.tracks().take(PREFETCH_TRACKS).cloned().collect();
        let continuation = page.more.clone();
        let loaded_item_count = page.shelves.last().map_or(0, |shelf| shelf.items.len());
        let opened_from = match &fetch_context.fetch {
            Fetch::Browse(item) => Some(item.clone()),
            _ => None,
        };
        let browser = self.browser_mut(target);
        if fetch_context.page_was_shown {
            browser.refresh_top(page);
        } else if fetch_context.replace_current_page {
            browser.replace(page);
        } else {
            browser.show_from(page, opened_from);
        }
        browser.set_busy(false);
        browser.bind_cache_key(fetch_context.cache_key.clone());
        if target == Target::Library {
            self.refresh_picker_playlists();
        }
        self.player.prefetch(prefetch_tracks);
        if let (Some(continuation), Some(cache_key)) = (continuation, fetch_context.cache_key.clone()) {
            self.page_loaders.insert(
                target,
                Loader {
                    request_id,
                    page_key: cache_key,
                    pagination: PagePagination::new(Some(continuation)),
                    continuation_in_flight: false,
                    loaded_item_count,
                },
            );
            self.pump_loader(target);
        }
        self.start_fill(target);
        if !self.page_loaders.contains_key(&target)
            && let Some(cache_key) = fetch_context.cache_key
        {
            self.cache_page(target, cache_key);
        }
    }

    fn page_more(
        &mut self,
        target: Target,
        request_id: RequestId,
        cache_key: String,
        continuation_result: redwax_ytm::Result<(Vec<Item>, Option<Continuation>)>,
    ) {
        // A batch for a page the browser has left belongs to nobody.
        let Some(loader) = self.page_loaders.get_mut(&target) else {
            return;
        };
        if loader.request_id != request_id || loader.page_key != cache_key || !loader.continuation_in_flight {
            return;
        }
        loader.continuation_in_flight = false;
        let validated_batch = continuation_result.and_then(|(items, continuation)| {
            loader.pagination.advance(&items, continuation.clone())?;
            Ok((items, continuation))
        });
        let (mut items, continuation) = match validated_batch {
            Ok(batch) => batch,
            Err(error) => {
                self.page_loaders.remove(&target);
                self.playback.stop_page_tail(&cache_key);
                if matches!(error, redwax_ytm::Error::Rejected) {
                    let retry_attempted = self
                        .page_requests
                        .get(&target)
                        .and_then(|slot| slot.get(request_id))
                        .is_none_or(|context| context.retry_attempted);
                    if !retry_attempted && self.refresh_session(Some(Retry::Page(target, request_id))) {
                        return;
                    }
                    self.start_logout(true);
                    return;
                }
                self.notify(
                    format!("Only part of this page loaded: {error}. Reload the page to try again."),
                    Some("YouTube Music"),
                    Level::Warning,
                );
                return;
            }
        };
        loader.loaded_item_count += items.len();

        self.duration_cache.fill_items(&mut items);
        let arrived_tracks: Vec<Track> = items.iter().filter_map(Item::as_track).cloned().collect();
        self.browser_mut(target).append(items, continuation.clone());
        if target == Target::Library {
            self.refresh_picker_playlists();
        }
        self.queue_arrivals(&cache_key, &arrived_tracks);
        self.start_fill(target);

        if continuation.is_some() {
            self.pump_loader(target);
            return;
        }
        self.page_loaders.remove(&target);
        self.cache_page(target, cache_key);
    }

    fn pump_loader(&mut self, target: Target) {
        let Some(loader) = self.page_loaders.get(&target) else {
            return;
        };
        let Some(continuation) = loader.pagination.next().cloned() else {
            return;
        };
        if loader.continuation_in_flight || !self.wants_more(target, loader) {
            return;
        }
        let cache_key = loader.page_key.clone();
        let request_id = loader.request_id;
        self.page_loaders
            .get_mut(&target)
            .expect("the loader was read a line above")
            .continuation_in_flight = true;

        let source = self.source.clone();
        let event_sender = self.events.clone();
        self.background_tasks.spawn(async move {
            let continuation_result = source.continue_page(&continuation).await;
            let _ = event_sender.send(AppEvent::PageMore {
                target,
                request: request_id,
                key: cache_key,
                result: continuation_result,
            });
        });
    }

    /// When the next batch goes out: while the list is short enough to reach
    /// the end of, when the cursor is near what has arrived, or once the app
    /// has been idle long enough to finish the page unasked.
    fn wants_more(&self, target: Target, loader: &Loader) -> bool {
        if loader.loaded_item_count < EAGER_TRACKS {
            return true;
        }
        if clock().saturating_sub(self.last_input_millis) >= IDLE_LOAD_MILLIS {
            return true;
        }
        let cursor_row = self.browser(target).cursor().unwrap_or(0);
        cursor_row + LOAD_AHEAD_ROWS >= loader.loaded_item_count
    }

    /// The page a browser was showing is gone, so nothing still being fetched
    /// for it belongs to anybody.
    fn leave_page(&mut self, target: Target) {
        self.page_loaders.remove(&target);
        self.duration_fill_tasks.remove(&target);
        if let Some(request_slot) = self.page_requests.get_mut(&target) {
            request_slot.cancel();
        }
        self.browser_mut(target).set_busy(false);
    }

    /// Ask YouTube for the durations the page was served without. One task per
    /// page, the rows on screen first, so the times a user can see land first.
    fn start_fill(&mut self, target: Target) {
        if self
            .duration_fill_tasks
            .get(&target)
            .is_some_and(|fill| !fill.task_handle.is_finished())
        {
            return;
        }
        let missing_video_ids = self.duration_cache.take_unasked(
            self.browser(target)
                .missing_duration_ids(self.screen.visible_rows() as usize),
        );
        if missing_video_ids.is_empty() {
            return;
        }
        let source = self.source.clone();
        let event_sender = self.events.clone();
        let duration_gate = self.duration_gate.clone();
        let task_handle = self.background_tasks.spawn(async move {
            for video_id_chunk in missing_video_ids.chunks(DURATION_CHUNK) {
                let Ok(duration_permit) = duration_gate.acquire().await else {
                    return;
                };
                let found_durations = source.track_lengths(video_id_chunk).await;
                drop(duration_permit);
                if !found_durations.is_empty()
                    && event_sender
                        .send(AppEvent::DurationsReady(found_durations))
                        .is_err()
                {
                    return;
                }
            }
        });
        self.duration_fill_tasks.insert(target, Fill { task_handle });
    }

    /// Stamp what arrived onto every page that is holding a blank, the queue
    /// included, and put a finished page back into the cache with its times.
    fn durations_ready(&mut self, found_durations: Vec<(String, u32)>) {
        let duration_cache = self.duration_cache.clone();
        if !duration_cache.learn(&found_durations) {
            return;
        }
        for target in [Target::Home, Target::Search, Target::Library] {
            let page_was_updated = self.browser_mut(target).fill_durations(&duration_cache);
            let page_is_complete = self.browser(target).top().is_some_and(|page| page.more.is_none());
            if page_was_updated
                && page_is_complete
                && !self.page_loaders.contains_key(&target)
                && let Some(cache_key) = self.browser(target).cache_key().map(str::to_owned)
            {
                self.cache_page(target, cache_key);
            }
        }
        if duration_cache.fill_tracks(&mut self.player_state.queue) {
            self.player_state.duration = self
                .player_state
                .track()
                .and_then(|track| track.duration_seconds)
                .map(f64::from);
            let queue_view = queue_view(&self.player_state);
            self.first_queue_index = queue_view.first_queue_index;
            self.queue.replace(queue_view.page);
            self.update_radio_label();
        }
        duration_cache.write_if_due();
    }

    fn cache_page(&mut self, target: Target, cache_key: String) {
        let Some(page) = self.browser(target).top().cloned() else {
            return;
        };
        let Ok(complete_page) = CompletePage::try_from(page) else {
            return;
        };
        if let Some(request_slot) = self.prefetch_requests.get_mut(&cache_key) {
            request_slot.cancel();
        }
        self.deps.page_cache.put(&cache_key, &complete_page);
    }

    fn queue_arrivals(&mut self, page_key: &str, tracks: &[Track]) {
        if tracks.is_empty() {
            return;
        }
        let page_key = page_key.to_string();
        let arrived_tracks = tracks.to_vec();
        self.command(move |player| async move {
            player
                .append_page_tail(&page_key, arrived_tracks)
                .await
                .map(|_| ())
        });
    }

    pub fn load_page(&mut self, target: Target, cache_key: Option<String>, replace_current_page: bool) {
        let fetch = match target {
            Target::Home => Fetch::Home,
            Target::Library => Fetch::Library,
            Target::Search => Fetch::Search(self.search_query.trim().to_string()),
            Target::Queue => return,
        };
        self.fetch(target, fetch, cache_key, replace_current_page);
    }

    fn fetch(&mut self, target: Target, fetch: Fetch, cache_key: Option<String>, replace_current_page: bool) {
        self.fetch_with_retry(target, fetch, cache_key, replace_current_page, false);
    }

    fn fetch_with_retry(
        &mut self,
        target: Target,
        fetch: Fetch,
        cache_key: Option<String>,
        replace_current_page: bool,
        retry_attempted: bool,
    ) {
        let cached_page = (!self.session_logged_out)
            .then(|| {
                cache_key
                    .as_deref()
                    .and_then(|cache_key| self.deps.page_cache.get(cache_key))
            })
            .flatten();
        let page_was_shown = cached_page.is_some();
        // The first frame already put the remembered page on screen, so showing
        // it again here would stack a second copy on the history.
        if let Some(mut page) = cached_page {
            self.duration_cache.fill(&mut page);
            let prefetch_tracks: Vec<Track> = page.tracks().take(PREFETCH_TRACKS).cloned().collect();
            let opened_from = match &fetch {
                Fetch::Browse(item) => Some(item.clone()),
                _ => None,
            };
            let browser = self.browser_mut(target);
            if browser.top() != Some(&page) {
                if replace_current_page {
                    browser.replace(page);
                } else {
                    browser.show_from(page, opened_from);
                }
            }
            self.browser_mut(target).bind_cache_key(cache_key.clone());
            self.player.prefetch(prefetch_tracks);
        }
        self.leave_page(target);
        self.browser_mut(target).set_busy(true);
        let request_id = self.page_requests.entry(target).or_default().begin(PageFetch {
            fetch: fetch.clone(),
            cache_key: cache_key.clone(),
            replace_current_page,
            page_was_shown,
            retry_attempted,
        });
        let source = self.source.clone();
        let event_sender = self.events.clone();
        let task_handle = self.background_tasks.spawn(async move {
            let fetch_result = match &fetch {
                Fetch::Home => source.home().await,
                Fetch::Library => source.library().await,
                Fetch::Search(query) => source.search(query).await,
                Fetch::Browse(item) => source.browse(item).await,
            };
            match fetch_result {
                Ok(page) => {
                    let _ = event_sender.send(AppEvent::PageReady {
                        target,
                        request: request_id,
                        page,
                    });
                }
                Err(error) => {
                    let _ = event_sender.send(AppEvent::PageFailed {
                        target,
                        request: request_id,
                        rejected: matches!(error, redwax_ytm::Error::Rejected),
                        error: error.to_string(),
                    });
                }
            }
        });
        self.page_requests
            .get_mut(&target)
            .expect("the request was just started")
            .hold(request_id, task_handle);
    }

    /// Get ahead of enter: the stream of the track the cursor rests on and the
    /// two after it, or the page of a browsable so opening it is instant.
    fn prefetch_highlighted(&mut self) {
        let Some(browser) = self.active_browser() else {
            return;
        };
        let Some((shelf_index, item_index)) = browser.selected() else {
            return;
        };
        let Some(page) = browser.top() else {
            return;
        };
        let shelf = &page.shelves[shelf_index];
        match &shelf.items[item_index] {
            Item::Track(_) => {
                let tracks: Vec<Track> = shelf.tracks().cloned().collect();
                let selected_track_index = shelf_track_index(shelf, item_index);
                let prefetch_tracks: Vec<Track> = tracks
                    .into_iter()
                    .skip(selected_track_index)
                    .take(PREFETCH_TRACKS)
                    .collect();
                self.player.prefetch(prefetch_tracks);
            }
            selected_item => {
                if self.tab == Tab::Queue || self.session_logged_out {
                    return;
                }
                let Some(browsable) = selected_item.as_browsable() else {
                    return;
                };
                let cache_key = browsable.key();
                if self.deps.page_cache.get(&cache_key).is_some()
                    || self
                        .prefetch_requests
                        .get(&cache_key)
                        .is_some_and(|request| request.current().is_some())
                {
                    return;
                }
                let request_id = self
                    .prefetch_requests
                    .entry(cache_key.clone())
                    .or_default()
                    .begin(());
                let source = self.source.clone();
                let event_sender = self.events.clone();
                let requested_key = cache_key.clone();
                let task_handle = self.background_tasks.spawn(async move {
                    let page = source
                        .browse(&browsable)
                        .await
                        .ok()
                        .and_then(|page| CompletePage::try_from(page).ok());
                    let _ = event_sender.send(AppEvent::PagePrefetched {
                        key: requested_key,
                        request: request_id,
                        page,
                    });
                });
                self.prefetch_requests
                    .get_mut(&cache_key)
                    .expect("the speculative request was just started")
                    .hold(request_id, task_handle);
            }
        }
    }

    fn load_lyrics(&mut self, track: &Track) {
        let request_id = self.lyrics.fetching(track);
        let source = self.source.clone();
        let event_sender = self.events.clone();
        let requested_track = track.clone();
        let task_handle = self.background_tasks.spawn(async move {
            let lyrics = match source.timed_lyrics(&requested_track).await {
                Ok(found_lyrics) => found_lyrics,
                Err(error) => Some(Lyrics::Plain(format!("Could not fetch lyrics: {error}"))),
            };
            let _ = event_sender.send(AppEvent::LyricsReady {
                request: request_id,
                track: requested_track,
                lyrics,
            });
        });
        self.lyrics.hold(request_id, task_handle);
    }

    fn load_art(&mut self, track: &Track) {
        self.art.request_slot.cancel();
        self.art.requested_video_id = Some(track.video_id.clone());
        self.art.render_protocol = None;
        self.art.cover_height = self.deps.config.art_height;
        self.art.accent_color = None;
        let resolved_theme = self.theme_controller.resolve_or_cached(&self.deps.config);
        self.base_theme = self
            .theme_controller
            .present(resolved_theme, self.deps.config.transparent, None);
        let Some(thumbnail_url) = track.thumbnail_url.clone() else {
            return;
        };
        let request_id = self.art.request_slot.begin(thumbnail_url.clone());
        let art_cache = self.deps.art_cache.clone();
        let event_sender = self.events.clone();
        let video_id = track.video_id.clone();
        let accent_from_art = self.deps.config.accent_from_art;
        let task_handle = self.background_tasks.spawn(async move {
            let image = art_cache.fetch(&thumbnail_url).await;
            if accent_from_art {
                let accent_image = image.clone();
                let accent_task_result = tokio::task::spawn_blocking(move || {
                    accent_image.map(|image| crate::art::accent_color(&image))
                })
                .await;
                let _ = event_sender.send(AppEvent::AccentReady {
                    request: request_id,
                    video_id: video_id.clone(),
                    accent: accent_task_result.ok().flatten(),
                });
            }
            let _ = event_sender.send(AppEvent::ArtReady {
                request: request_id,
                video_id,
                image,
            });
        });
        self.art.request_slot.hold(request_id, task_handle);
    }

    fn art_ready(&mut self, request_id: RequestId, video_id: String, image: Option<DynamicImage>) {
        if self.art.request_slot.complete(request_id).is_none()
            || self.art.requested_video_id.as_deref() != Some(video_id.as_str())
        {
            return;
        }
        let Some(image) = image else {
            self.art.render_protocol = None;
            self.art.cover_height = self.deps.config.art_height;
            return;
        };
        let image_picker = self.art.image_picker.clone().unwrap_or_else(Picker::halfblocks);
        let font_size = image_picker.font_size();
        let image_width_pixels = u32::from(ART_WIDTH) * u32::from(font_size.width);
        let image_height_pixels =
            u64::from(image.height()) * u64::from(image_width_pixels) / u64::from(image.width().max(1));
        let image_height_cells = image_height_pixels.div_ceil(u64::from(font_size.height.max(1))) as u16;
        self.art.cover_height = image_height_cells.min(self.deps.config.art_height);
        if self.art.cover_height == 0 {
            self.art.render_protocol = None;
            return;
        }
        self.art.render_protocol = image_picker
            .new_protocol(
                image,
                Size::new(ART_WIDTH, self.art.cover_height),
                Resize::Fit(None),
            )
            .ok();
    }

    // ----------------------------------------------------------------- draw

    /// Paint the whole screen. Layout: header (`HEADER_HEIGHT`), body (sidebar
    /// `SIDEBAR_WIDTH` wide when the width is at least `SIDEBAR_MIN_WIDTH`,
    /// then the active tab), player bar (`PLAYER_BAR_HEIGHT` rows), and the
    /// keys bar (1 row) only where there is no sidebar to hold the keys box.
    /// Popups fit their wrapped content within the available frame.
    ///
    /// A frame nothing has changed since is put back from the last one, so an
    /// idle tick costs a copy rather than a layout pass.
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if let Some(previous_frame) = self.screen.restore(area) {
            frame.buffer_mut().clone_from(previous_frame);
            return;
        }
        let measured_layout = ui::layout::FrameLayout::measure(area, true, self.deps.config.art_height);
        let Some(frame_layout) = measured_layout else {
            let theme = self.base_theme.clone();
            let motion = self.motion();
            let mut hit_map = ui::HitMap::bounded(area);
            if let Some((message, scroll)) = self.notification_popup_mut() {
                ui::toasts::message(frame.buffer_mut(), area, message, scroll, &theme);
                hit_map.add(area, ui::Hit::Overlay);
            } else {
                hit_map.extend(ui::toasts::toasts(
                    frame.buffer_mut(),
                    area,
                    &self.notifications,
                    &theme,
                    motion,
                ));
            }
            self.screen.record_painted_frame(frame.buffer_mut(), hit_map);
            return;
        };
        let motion = self.motion();
        let theme = self.base_theme.clone();
        let icons = self.deps.icons;
        let mut hit_map = ui::HitMap::bounded(area);
        let buffer = frame.buffer_mut();
        ui::fill(buffer, area, theme.background);

        let page_is_loading = self.home.busy() || self.search.busy() || self.library.busy();
        hit_map.extend(ui::header::header(
            buffer,
            frame_layout.header,
            ui::header::HeaderView {
                mode: frame_layout.header_mode,
                active_tab: self.tab,
                account: self.login_state.label(&self.account),
                account_focused: self.account_focused(),
                radio_loading: page_is_loading,
                icons: &icons,
                theme: &theme,
            },
        ));

        let body = Rect::new(
            area.x,
            frame_layout.content.y,
            area.width,
            frame_layout.content.height,
        );

        if let Some(sidebar) = frame_layout.sidebar {
            let upcoming_tracks = self.player_state.upcoming(ui::sidebar::QUEUE_ROWS);
            hit_map.extend(ui::sidebar::sidebar(
                buffer,
                sidebar.area,
                ui::sidebar::SidebarView {
                    current_track: self.player_state.track(),
                    upcoming_tracks: &upcoming_tracks,
                    cover_art: ui::sidebar::ArtView {
                        protocol: self.art.render_protocol.as_ref(),
                        cover_height: self.art.cover_height.min(sidebar.cover_max_height),
                    },
                    theme: &theme,
                    motion,
                },
            ));
        }
        let content = frame_layout.content;

        if self.tab == Tab::Lyrics {
            let current_track = self.player_state.track().cloned();
            let lyrics_frame = ui::lyrics::lyrics(
                buffer,
                content,
                ui::lyrics::LyricsView {
                    current_track: current_track.as_ref(),
                    lyrics: self.lyrics.text(),
                    loading_track_title: self.lyrics.loading(),
                    position_millis: (self.player_state.position.max(0.0) * 1000.0) as u64,
                    duration_millis: self
                        .player_state
                        .duration
                        .filter(|seconds| *seconds > 0.0)
                        .map(|seconds| (seconds * 1000.0) as u64),
                    scroll_offset: self.lyrics.scroll(),
                    auto_follow: self.lyrics.follows(),
                    theme: &theme,
                },
            );
            self.lyrics.scrolled_to(lyrics_frame.scroll_offset);
            self.lyrics.measured(lyrics_frame.visible_rows);
            self.screen.record_visible_rows(lyrics_frame.visible_rows);
            hit_map.extend(lyrics_frame.hit_regions);
        } else if self.tab == Tab::Themes {
            let chosen_theme = self.deps.config.theme.clone();
            if let Some(pane) = &self.themes {
                hit_map.extend(ui::themes::themes(
                    buffer,
                    content,
                    ui::themes::ThemesView {
                        entries: pane.entries(),
                        cursor: pane.cursor(),
                        chosen_theme: &chosen_theme,
                        theme: &theme,
                    },
                ));
            }
        } else if self.tab == Tab::Settings {
            hit_map.extend(ui::settings::settings(buffer, content, &self.deps.config, &theme));
        } else {
            let searchable = self.tab == Tab::Search;
            self.screen
                .record_visible_rows(ui::page_list::visible_height(content, searchable));
            let playing_video_id = self.player_state.track().map(|track| track.video_id.clone());
            let liked_overrides = self.liked_overrides.clone();
            let search_query = self.search_query.clone();
            let list_focused = !self.search_focused();
            let search_box = match searchable {
                true => Some(ui::page_list::SearchBox {
                    query: &search_query,
                    focused: self.search_focused(),
                    cursor_byte_index: self.search_editor.cursor(),
                    editing_mode: self.search_editor.label(),
                }),
                false => None,
            };
            let active_find = match self.prompt() {
                Some(Prompt::Find(find)) => Some(find.clone()),
                _ => None,
            };
            let find_bar = active_find.as_ref().map(|find| ui::page_list::FindBar {
                query: &find.query,
                has_match: find.has_match,
                match_position: find.match_position,
            });
            let tab = self.tab;
            let first_queue_index = self.first_queue_index;
            let browser_target = tab.target().unwrap_or(Target::Queue);
            let browser = match tab {
                Tab::Home => &mut self.home,
                Tab::Search => &mut self.search,
                Tab::Library => &mut self.library,
                _ => &mut self.queue,
            };
            let browser_paint = ui::page_list::browser(
                buffer,
                content,
                ui::page_list::BrowserView {
                    browser,
                    browser_target,
                    icons: &icons,
                    theme: &theme,
                    playing_track: match (tab, &playing_video_id) {
                        (Tab::Queue, _) => match self.player_state.index {
                            Some(current_queue_index) => ui::page_list::Playing::Row(
                                current_queue_index.saturating_sub(first_queue_index),
                            ),
                            None => ui::page_list::Playing::Nothing,
                        },
                        (_, Some(playing_video_id)) => ui::page_list::Playing::Track(playing_video_id),
                        (_, None) => ui::page_list::Playing::Nothing,
                    },
                    liked_overrides: &liked_overrides,
                    search_box,
                    find_bar,
                    list_focused,
                    motion,
                },
            );
            if let Some(scroll_offset) = browser_paint.scroll_offset {
                browser.reconcile_viewport(scroll_offset);
            }
            self.screen.record_visible_rows(browser_paint.visible_rows);
            hit_map.extend(browser_paint.hit_regions);
        }

        let volume_entry = match self.prompt() {
            Some(Prompt::Volume(entry)) => Some(entry.clone()),
            _ => None,
        };
        let current_track_liked = self
            .player_state
            .track()
            .map(|track| self.is_liked(track))
            .unwrap_or(false);
        hit_map.extend(ui::player_bar::player_bar(
            buffer,
            frame_layout.player,
            ui::player_bar::BarView {
                player_state: &self.player_state,
                seek_preview_fraction: self
                    .scrub
                    .filter(|scrub| scrub.slider == Slider::Seek)
                    .map(|scrub| scrub.normalized_fraction),
                volume_preview_percent: self
                    .scrub
                    .filter(|scrub| scrub.slider == Slider::Volume)
                    .map(|scrub| (scrub.normalized_fraction * 100.0).round() as u8),
                volume_entry: volume_entry.as_deref(),
                current_track_liked,
                icons: &icons,
                theme: &theme,
                motion,
            },
        ));
        if let Some(hints) = frame_layout.hints {
            hit_map.extend(ui::keys_bar::keys_bar(buffer, hints, &theme));
        }
        if self.eq.visible() {
            let (eq_panel, eq_hit_regions) = ui::eq::eq(
                buffer,
                body,
                ui::eq::EqView {
                    settings: self.eq.settings(),
                    selected_band: self.eq.band(),
                    genre: self.eq.genre(),
                    genre_match: self.eq.genre_match(),
                    theme: &theme,
                },
            );
            hit_map.add(eq_panel, ui::Hit::Overlay);
            hit_map.extend(eq_hit_regions);
        }
        hit_map.extend(ui::toasts::toasts(
            buffer,
            body,
            &self.notifications,
            &theme,
            motion,
        ));
        if self.modal_pointer_owner() {
            hit_map.add(area, ui::Hit::Overlay);
        }
        if self.help_visible() {
            let panel =
                ui::help::searchable_help(buffer, body, &theme, &mut self.help_scroll, &self.help_query);
            hit_map.add(panel, ui::Hit::Overlay);
        }
        if let Some(picker) = self.playlist_picker() {
            let picker_panel = ui::picker::picker(
                buffer,
                body,
                ui::picker::PickerView {
                    remove_mode: picker.remove_mode(),
                    primary_item_label: picker.primary_item_label(),
                    alternate_item_label: picker.alternate_item_label(),
                    playlists: &picker.playlists,
                    selected_playlist_index: picker.selected_playlist_index,
                    new_playlist_name_input: picker.new_playlist_name_input.as_deref(),
                    theme: &theme,
                },
            );
            hit_map.add(picker_panel, ui::Hit::Overlay);
        }
        if let Some(picker) = self.account_picker() {
            hit_map.extend(ui::accounts::accounts(
                buffer,
                body,
                picker,
                &self.account,
                &theme,
            ));
        }
        if let Some(Prompt::ClearQueue(prompt)) = self.prompt() {
            let message = format!(
                "Clear {} upcoming tracks{}?",
                prompt.preview.affected(),
                prompt.preview.producer_suffix(),
            );
            let confirmation_paint = ui::confirm::confirmation(
                buffer,
                area,
                ui::confirm::ConfirmationView {
                    title: "CLEAR QUEUE",
                    message: &message,
                    detail: prompt
                        .preview
                        .keeps_current()
                        .then_some("The playing track continues."),
                    confirm_label: "Clear queue",
                    cancel_label: "Keep queue",
                    state: &prompt.confirmation_state,
                    theme: &theme,
                },
            );
            hit_map.extend(confirmation_paint.hit_regions);
        }
        if let Some(Prompt::Logout(confirmation_state)) = self.prompt() {
            let confirmation_paint = ui::confirm::confirmation(
                buffer,
                area,
                ui::confirm::ConfirmationView {
                    title: "LOG OUT",
                    message: "Remove this account from redwax?",
                    detail: Some("Playback continues. Account pages and edits are signed out."),
                    confirm_label: "Log out",
                    cancel_label: "Stay signed in",
                    state: confirmation_state,
                    theme: &theme,
                },
            );
            hit_map.extend(confirmation_paint.hit_regions);
        }
        if let Some((message, scroll)) = self.notification_popup_mut() {
            ui::toasts::message(buffer, area, message, scroll, &theme);
            hit_map.add(area, ui::Hit::Overlay);
        }
        self.screen.record_painted_frame(frame.buffer_mut(), hit_map);
        self.modal_pointer_frame_revision = self
            .modal_pointer_owner()
            .then(|| self.screen.hit_map().revision());
    }

    // -------------------------------------------------------------- lookups

    pub fn active_browser(&self) -> Option<&Browser> {
        match self.tab {
            Tab::Home => Some(&self.home),
            Tab::Search => Some(&self.search),
            Tab::Library => Some(&self.library),
            Tab::Queue => Some(&self.queue),
            Tab::Lyrics | Tab::Themes | Tab::Settings => None,
        }
    }

    pub fn active_browser_mut(&mut self) -> Option<&mut Browser> {
        match self.tab {
            Tab::Home => Some(&mut self.home),
            Tab::Search => Some(&mut self.search),
            Tab::Library => Some(&mut self.library),
            Tab::Queue => Some(&mut self.queue),
            Tab::Lyrics | Tab::Themes | Tab::Settings => None,
        }
    }

    pub fn browser(&self, target: Target) -> &Browser {
        match target {
            Target::Home => &self.home,
            Target::Search => &self.search,
            Target::Library => &self.library,
            Target::Queue => &self.queue,
        }
    }

    /// Pages still filling themselves in, which a test reads to know a loader
    /// started or stopped.
    pub fn pages_loading(&self) -> usize {
        self.page_loaders.len()
    }

    /// Pages still asking YouTube for durations, which a test reads to know a
    /// fill started or stopped.
    pub fn pages_filling(&self) -> usize {
        self.duration_fill_tasks
            .values()
            .filter(|fill| !fill.task_handle.is_finished())
            .count()
    }

    pub fn durations(&self) -> &crate::durations::Durations {
        &self.duration_cache
    }

    pub fn genres(&self) -> &crate::genre::Genres {
        &self.genre_cache
    }

    /// Answer the genre question with this rather than with iTunes. The tests
    /// set it before anything plays.
    pub fn set_genre_lookup(&mut self, lookup: crate::genre::Lookup) {
        self.genre_lookup = lookup;
    }

    pub fn browser_mut(&mut self, target: Target) -> &mut Browser {
        match target {
            Target::Home => &mut self.home,
            Target::Search => &mut self.search,
            Target::Library => &mut self.library,
            Target::Queue => &mut self.queue,
        }
    }

    pub fn highlighted_track(&self) -> Option<&Track> {
        self.active_browser()
            .and_then(Browser::selected_item)
            .and_then(Item::as_track)
    }

    pub fn highlighted_browsable(&self) -> Option<Browsable> {
        self.active_browser()
            .and_then(Browser::selected_item)
            .and_then(Item::as_browsable)
    }

    pub fn is_liked(&self, track: &Track) -> bool {
        self.liked_overrides
            .get(&track.video_id)
            .copied()
            .unwrap_or(track.liked)
    }

    pub fn notify(&mut self, text: impl Into<String>, title: Option<&str>, level: Level) {
        let notification = Notification {
            text: text.into(),
            title: title.map(str::to_string),
            level,
            expires_at_millis: clock() + level.seconds() * 1000,
        };
        self.last_notification = Some(notification.clone());
        self.notifications.push(notification);
        self.screen.touch();
    }

    fn read_notification(&mut self, notification_index: Option<usize>) {
        let message = match notification_index {
            Some(notification_index) => self.notifications.get(notification_index),
            None => self.notifications.last().or(self.last_notification.as_ref()),
        };
        if let Some(message) = message {
            self.open_overlay(Overlay::Notification(
                message.clone(),
                ui::popup::Scroll::default(),
            ));
            self.screen.touch();
        }
    }

    fn replace_notification(&mut self, text: impl Into<String>, title: &str, level: Level) {
        self.notifications
            .retain(|notification| notification.title.as_deref() != Some(title));
        self.notify(text, Some(title), level);
    }

    /// The state file contents for `SavedState::write`.
    pub fn saved_state(&self) -> crate::state::SavedState {
        crate::state::SavedState::from_player(&self.player_state)
    }

    /// Close MPRIS, stop the player and write the state file. The terminal is
    /// restored by `main`, which owns it.
    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.playback.stop_producer();
        self.background_tasks.abort_all();
        while let Some(task_result) = self.background_tasks.join_next().await {
            self.report_task_failure(task_result);
        }
        let confirmed_state = self.playback.observe(self.playback.confirmed_state());
        self.player_state = confirmed_state;
        let pending_checkpoint = self.deps.state_store.submit(&self.player_state);
        if let Ok(mut active_mpris) = self.mpris.lock()
            && let Some(mpris) = active_mpris.take()
        {
            mpris.close();
        }
        let close_result = self.playback.close().await.map_err(|error| error.to_string());
        let save_result = checkpoint(pending_checkpoint).await;
        self.write_eq();
        match (close_result, save_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(close_error), Ok(())) => Err(format!("Playback could not close cleanly: {close_error}")),
            (Ok(()), Err(save_error)) => Err(format!("Playback state could not be saved: {save_error}")),
            (Err(close_error), Err(save_error)) => Err(format!(
                "Playback could not close cleanly: {close_error}. Playback state could not be saved: {save_error}"
            )),
        }
    }
}

/// Every video id an addition names, an album or playlist read to its end.
async fn addition_ids<S: MusicSource>(source: &S, addition: Addition) -> Result<Vec<String>, String> {
    let browsable = match addition {
        Addition::Track(video_id) => return Ok(vec![video_id]),
        Addition::Whole(browsable) => browsable,
    };
    let first_page = source
        .browse(&browsable)
        .await
        .map_err(|error| error.to_string())?;
    let complete_page = collect_page(source, first_page)
        .await
        .map_err(|incomplete| incomplete.to_string())?;
    Ok(complete_page
        .as_page()
        .tracks()
        .map(|track| track.video_id.clone())
        .collect())
}

/// Add what `addition` names to `playlist`, and the toast that says how it went.
async fn add_tracks<S: MusicSource>(
    source: &S,
    playlist: &Playlist,
    addition: Addition,
    subject_label: &str,
) -> (String, Level) {
    let video_ids = match addition_ids(source, addition).await {
        Ok(video_ids) => video_ids,
        Err(error) => return (error, Level::Error),
    };
    match source.add_to_playlist(&playlist.playlist_id, &video_ids).await {
        Ok(PlaylistAdd::Added) => (
            format!("{subject_label} added to {}", playlist.title),
            Level::Info,
        ),
        Ok(PlaylistAdd::AlreadyPresent) => (
            format!("{subject_label} is already in {}", playlist.title),
            Level::Info,
        ),
        Err(error) => (error.to_string(), Level::Error),
    }
}

/// Take every occurrence of `track` out of `playlist`, after the source has
/// proved it observed the complete playlist.
async fn remove_track<S: MusicSource>(
    source: &S,
    playlist: &Playlist,
    track: &Track,
    subject_label: &str,
) -> (String, Level) {
    match source.remove_all_from_playlist(playlist, &track.video_id).await {
        Ok(PlaylistRemoval::Removed { .. }) => (
            format!("{subject_label} removed from {}", playlist.title),
            Level::Info,
        ),
        Ok(PlaylistRemoval::AlreadyAbsent) => (
            format!("{subject_label} is not in {}", playlist.title),
            Level::Warning,
        ),
        Err(error) => (error.to_string(), Level::Error),
    }
}

pub fn track_label(track: &Track) -> String {
    format!("{} \u{b7} {}", track.title, track.artist_line())
}

/// The selected occurrence survives filtering out non-track rows from a mixed shelf.
fn shelf_track_index(shelf: &Shelf, item_index: usize) -> usize {
    shelf
        .items
        .iter()
        .take(item_index)
        .filter(|item| matches!(item, Item::Track(_)))
        .count()
}

async fn checkpoint(pending_checkpoint: crate::state::PendingCheckpoint) -> Result<(), String> {
    tokio::task::spawn_blocking(move || pending_checkpoint.write())
        .await
        .map_err(|_| "the playback checkpoint task did not finish".to_string())?
        .map_err(|error| error.to_string())
}

/// The queue tab is a view of the player, not a page of its own.
/// Tracks already played that the queue keeps above the playing one, so it
/// reads as a list that has moved rather than one that starts at the top.
pub const QUEUE_PASSED: usize = 5;

/// The queue tab: a page, and the entry it starts at.
///
/// The two travel together because a row on that page is an offset from
/// `first_queue_index`, and a page built without its offset is a page whose
/// rows name the wrong tracks. Handing back both stops the two drifting apart.
pub struct QueueView {
    pub first_queue_index: usize,
    pub page: Page,
}

/// The queue from `QUEUE_PASSED` tracks behind the playing one. The whole
/// queue is never shown: the tracks a long way behind are not worth the rows,
/// and starting the page near the playing track is what makes a skip look like
/// the list moved.
pub fn queue_view(player_state: &PlayerState) -> QueueView {
    QueueView {
        first_queue_index: queue_start(player_state),
        page: queue_page(player_state),
    }
}

/// The queue entry the page starts at.
///
/// Clamped to the queue, because mpv can report a position that outran it: a
/// removal shortens the playlist before the new position arrives, and a start
/// past the end is a slice that panics rather than a page that is wrong.
fn queue_start(player_state: &PlayerState) -> usize {
    player_state
        .index
        .unwrap_or(0)
        .saturating_sub(QUEUE_PASSED)
        .min(player_state.queue.len().saturating_sub(1))
}

/// The page alone, for a caller that only draws it.
pub fn queue_page(player_state: &PlayerState) -> Page {
    if player_state.queue.is_empty() {
        let mut page = Page::new("Queue", Vec::new());
        page.subtitle = Some("empty. press a on any track, album or playlist to add it".to_string());
        return page;
    }
    let first_queue_index = queue_start(player_state);
    let mut page = Page::new(
        "Queue",
        vec![Shelf::new(
            "Tracks",
            player_state.queue[first_queue_index..]
                .iter()
                .cloned()
                .map(Item::Track)
                .collect(),
        )],
    );
    let playing_position = player_state
        .index
        .filter(|index| *index < player_state.queue.len())
        .map(|index| format!("playing {} of {}", index + 1, player_state.queue.len()))
        .unwrap_or_else(|| "nothing playing".to_string());
    let remaining_duration = remaining_queue_duration(player_state)
        .map(|seconds| format!("{} remaining", format_duration(Some(seconds))))
        .unwrap_or_else(|| "? remaining".to_string());
    let track_noun = if player_state.queue.len() == 1 {
        "track"
    } else {
        "tracks"
    };
    page.subtitle = Some(format!(
        "{} {track_noun} loaded \u{b7} {playing_position} \u{b7} {remaining_duration}",
        player_state.queue.len(),
    ));
    page
}

fn remaining_queue_duration(player_state: &PlayerState) -> Option<u32> {
    let playing_index = player_state
        .index
        .filter(|index| *index < player_state.queue.len())?;
    player_state.queue[playing_index..].iter().enumerate().try_fold(
        0_u32,
        |total_seconds, (upcoming_offset, track)| {
            let track_seconds = track.duration_seconds?;
            let remaining_track_seconds = if upcoming_offset == 0 {
                track_seconds.saturating_sub(player_state.position.max(0.0) as u32)
            } else {
                track_seconds
            };
            total_seconds.checked_add(remaining_track_seconds)
        },
    )
}

/// Which art protocol the terminal gets, from `terminal::detect`. `None` means
/// ask the terminal, which is the only path that touches stdio.
pub fn protocol_for(renderer: Renderer) -> ProtocolType {
    match renderer {
        Renderer::Kitty => ProtocolType::Kitty,
        Renderer::Sixel => ProtocolType::Sixel,
        Renderer::Halfblocks => ProtocolType::Halfblocks,
    }
}
