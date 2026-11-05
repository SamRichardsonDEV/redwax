//! The same compact view can control a local player or an attached session.

use std::io::Stdout;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::{Frame, Terminal};
use redwax_player::{PlaybackAvailability, PlayerState};
use redwax_ytm::Track;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::icons::{Icons, icon_set};
use crate::keys::Action;
pub use crate::mini_remote::Remote;
use crate::mini_remote::{MiniAction, MiniState, MiniTrack};
use crate::theme::Theme;
use crate::ui::anim::Motion;
use crate::ui::hit::{Hit, HitMap, PointerIntent};
use crate::ui::player_bar::{BarView, PLAYER_BAR_HEIGHT, player_bar};
use crate::ui::{fill, fit, put, span, width_of};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const ERROR_DURATION: Duration = Duration::from_secs(5);
const MIN_WIDTH: u16 = 16;
const MIN_HEIGHT: u16 = PLAYER_BAR_HEIGHT;

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: &Config,
    remote: Remote,
) -> Result<(), String> {
    let theme_controller = crate::themes::ThemeController::new(&config.theme);
    let theme = theme_controller.present(
        theme_controller.resolve_or_cached(config),
        config.transparent,
        None,
    );
    let (commands, command_receiver) = mpsc::channel(16);
    let (update_sender, mut updates) = mpsc::channel(2);
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(remote_loop(remote, command_receiver, update_sender));
    let mut terminal_events = EventStream::new();
    let mut view = MiniView::new(config);
    let mut command_generation = None;

    while !view.closed() {
        terminal
            .draw(|frame| view.draw(frame, &theme))
            .map_err(|error| error.to_string())?;
        let command = tokio::select! {
            event = terminal_events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => view.key(key),
                Some(Ok(Event::Mouse(mouse))) => view.mouse(mouse),
                Some(Ok(Event::Resize(_, _))) => {
                    view.clear_hits();
                    None
                }
                Some(Ok(_)) => None,
                Some(Err(error)) => return Err(error.to_string()),
                None => break,
            },
            update = updates.recv() => {
                match update {
                    Some(Update::State { generation, state }) => {
                        command_generation = Some(generation);
                        view.replace_state(state);
                    }
                    Some(Update::Disconnected(error)) => {
                        command_generation = None;
                        view.disconnected(error);
                    }
                    None => return Err("The mini player connection task stopped unexpectedly.".into()),
                }
                None
            }
        };
        if let (Some(command), Some(generation)) = (command, command_generation) {
            match commands.try_send((generation, command.clone())) {
                Ok(()) => view.preview(&command),
                Err(_) => view.fail("Playback is busy. Try again."),
            }
        }
    }
    Ok(())
}

enum Update {
    State { generation: u64, state: MiniState },
    Disconnected(String),
}

async fn remote_loop(
    remote: Remote,
    mut commands: mpsc::Receiver<(u64, MiniAction)>,
    updates: mpsc::Sender<Update>,
) {
    let name = remote.name().to_string();
    let mut remote = Some(remote);
    let mut generation = 0_u64;
    let mut reconnect_at = tokio::time::Instant::now();
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if let Some(active) = remote.as_ref() {
            let action = tokio::select! {
                biased;
                command = commands.recv() => match command {
                    Some((request_generation, action)) if request_generation == generation => Some(action),
                    Some(_) => continue,
                    None => return,
                },
                _ = ticker.tick() => None,
            };
            let result = tokio::time::timeout(CALL_TIMEOUT, async {
                if let Some(action) = action {
                    active.command(action).await?;
                }
                active.state().await
            })
            .await
            .map_err(|_| "Redwax did not answer within 3 seconds".to_string())
            .and_then(|result| result);
            match result {
                Ok(state) => {
                    if updates.send(Update::State { generation, state }).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    generation = generation.wrapping_add(1);
                    remote = None;
                    while commands.try_recv().is_ok() {}
                    if updates.send(Update::Disconnected(error)).await.is_err() {
                        return;
                    }
                    reconnect_at = tokio::time::Instant::now() + RECONNECT_INTERVAL;
                }
            }
        } else {
            tokio::select! {
                _ = tokio::time::sleep_until(reconnect_at) => {}
                command = commands.recv() => {
                    if command.is_none() {
                        return;
                    }
                    continue;
                }
            }
            let result = reconnect(&name).await;
            while commands.try_recv().is_ok() {}
            match result {
                Ok((connected, state)) => {
                    remote = Some(connected);
                    if updates.send(Update::State { generation, state }).await.is_err() {
                        return;
                    }
                    ticker.reset();
                }
                Err(error) => {
                    if updates.send(Update::Disconnected(error)).await.is_err() {
                        return;
                    }
                    reconnect_at = tokio::time::Instant::now() + RECONNECT_INTERVAL;
                }
            }
        }
    }
}

async fn reconnect(name: &str) -> Result<(Remote, MiniState), String> {
    tokio::time::timeout(CALL_TIMEOUT, async {
        let remote = Remote::find(Some(name))
            .await?
            .ok_or_else(|| "The selected redwax session is not running".to_string())?;
        let state = remote.state().await?;
        Ok::<_, String>((remote, state))
    })
    .await
    .map_err(|_| "Reconnecting to redwax timed out after 3 seconds".to_string())?
}

pub(crate) struct MiniView {
    state: Option<MiniState>,
    focus_active: bool,
    queue_open: bool,
    selected: Option<String>,
    error: Option<(String, Instant)>,
    connection_error: Option<String>,
    hits: HitMap,
    icons: Icons,
    animations: bool,
    animation_frame: u64,
    animation_at: Instant,
    audible_volume: u8,
    owned: bool,
    full_requested: bool,
    closed: bool,
}

impl MiniView {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            state: None,
            focus_active: false,
            queue_open: false,
            selected: None,
            error: None,
            connection_error: None,
            hits: HitMap::default(),
            icons: icon_set(&config.icons),
            animations: config.animations,
            animation_frame: 0,
            animation_at: Instant::now(),
            audible_volume: config.volume.max(1),
            owned: false,
            full_requested: false,
            closed: false,
        }
    }

    pub(crate) fn set_owned(&mut self, owned: bool) {
        self.owned = owned;
    }

    pub(crate) fn set_focus_active(&mut self, focus_active: bool) {
        self.focus_active = focus_active;
        if let Some(state) = self.state.as_mut() {
            state.focus_active = focus_active;
        }
    }

    pub(crate) fn closed(&self) -> bool {
        self.closed
    }

    pub(crate) fn take_full_request(&mut self) -> bool {
        std::mem::take(&mut self.full_requested)
    }

    pub(crate) fn clear_hits(&mut self) {
        self.hits.clear();
    }

    pub(crate) fn replace_player_state(&mut self, player_state: &PlayerState) {
        let restored = player_state.queue_entries.is_empty()
            && player_state.paused
            && !player_state.loading
            && matches!(
                player_state.availability,
                PlaybackAvailability::Ready | PlaybackAvailability::RestoredPaused
            );
        let queue: Vec<MiniTrack> = player_state
            .queue
            .iter()
            .enumerate()
            .map(|(index, track)| MiniTrack {
                id: player_state
                    .queue_entries
                    .get(index)
                    .map(|entry| format!("/org/redwax/track/{}", entry.id()))
                    .unwrap_or_else(|| {
                        if restored {
                            format!("restored:{index}")
                        } else {
                            format!("pending:{index}")
                        }
                    }),
                title: track.title.clone(),
                artist: track.artist_line(),
            })
            .collect();
        let current = player_state.index.and_then(|index| queue.get(index));
        self.replace_state(MiniState {
            focus_active: self.focus_active,
            title: current.map_or_else(String::new, |track| track.title.clone()),
            artist: current.map_or_else(String::new, |track| track.artist.clone()),
            position: player_state.position,
            duration: player_state
                .duration
                .or_else(|| {
                    player_state
                        .track()
                        .and_then(|track| track.duration_seconds.map(f64::from))
                })
                .unwrap_or(0.0),
            paused: player_state.paused,
            volume: if player_state.muted {
                0
            } else {
                player_state.volume
            },
            shuffle: player_state.shuffle,
            repeat: player_state.repeat,
            current: current.map(|track| track.id.clone()),
            queue,
        });
    }

    pub(crate) fn replace_state(&mut self, state: MiniState) {
        self.connection_error = None;
        self.focus_active = state.focus_active;
        if !state
            .queue
            .iter()
            .any(|track| Some(track.id.as_str()) == self.selected.as_deref())
        {
            self.selected = state
                .current
                .clone()
                .filter(|id| state.queue.iter().any(|track| &track.id == id))
                .or_else(|| state.queue.first().map(|track| track.id.clone()));
        }
        if state.volume > 0 {
            self.audible_volume = state.volume;
        }
        self.state = Some(state);
    }

    fn disconnected(&mut self, error: String) {
        self.state = None;
        self.selected = None;
        self.hits.clear();
        self.error = None;
        self.connection_error = Some(error);
    }

    pub(crate) fn preview(&mut self, action: &MiniAction) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match action {
            MiniAction::SetVolume(volume) => {
                state.volume = *volume;
                if *volume > 0 {
                    self.audible_volume = *volume;
                }
            }
            MiniAction::SetShuffle(shuffle) => state.shuffle = *shuffle,
            MiniAction::SetRepeat(repeat) => state.repeat = *repeat,
            _ => {}
        }
    }

    pub(crate) fn fail(&mut self, message: &str) {
        self.error = Some((message.to_owned(), Instant::now()));
    }

    fn selected_index(&self) -> usize {
        self.state
            .as_ref()
            .and_then(|state| {
                state
                    .queue
                    .iter()
                    .position(|track| Some(track.id.as_str()) == self.selected.as_deref())
            })
            .unwrap_or(0)
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let selected_index = self
            .selected_index()
            .saturating_add_signed(delta)
            .min(state.queue.len().saturating_sub(1));
        self.selected = state.queue.get(selected_index).map(|track| track.id.clone());
    }

    fn toggle_queue(&mut self) {
        self.queue_open = !self.queue_open;
        self.hits.clear();
        if self.queue_open
            && let Some(state) = self.state.as_ref()
        {
            self.selected = state
                .current
                .clone()
                .filter(|id| state.queue.iter().any(|track| &track.id == id))
                .or_else(|| state.queue.first().map(|track| track.id.clone()));
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<MiniAction> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.closed = true;
            return None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        let action = match key.code {
            KeyCode::Char('F') if self.owned => Action::Back,
            KeyCode::Char('Q') => Action::Quit,
            KeyCode::Esc if self.queue_open => Action::JumpToQueue,
            KeyCode::Esc => Action::Quit,
            KeyCode::Char('q') | KeyCode::Tab | KeyCode::Char('l') => Action::JumpToQueue,
            KeyCode::Down | KeyCode::Char('j') if self.queue_open => {
                self.move_selection(1);
                return None;
            }
            KeyCode::Up | KeyCode::Char('k') if self.queue_open => {
                self.move_selection(-1);
                return None;
            }
            KeyCode::Enter if self.queue_open => return self.selected.clone().map(MiniAction::Play),
            KeyCode::Char(' ') => Action::TogglePause,
            KeyCode::Left | KeyCode::Char('p') => Action::PreviousTrack,
            KeyCode::Right | KeyCode::Char('n') => Action::NextTrack,
            KeyCode::Char('-') => Action::Volume(-5),
            KeyCode::Char('=') => Action::Volume(5),
            KeyCode::Char('m') => Action::ToggleMute,
            KeyCode::Char('s') => Action::ToggleShuffle,
            KeyCode::Char('R') => Action::CycleRepeat,
            KeyCode::Char(',') => Action::Seek(-5),
            KeyCode::Char('.') => Action::Seek(5),
            KeyCode::Char('<') => Action::Seek(-30),
            KeyCode::Char('>') => Action::Seek(30),
            _ => return None,
        };
        self.action(action)
    }

    fn action(&mut self, action: Action) -> Option<MiniAction> {
        match action {
            Action::Back if self.owned => {
                self.full_requested = true;
                return None;
            }
            Action::Quit => {
                self.closed = true;
                return None;
            }
            Action::JumpToQueue => {
                self.toggle_queue();
                return None;
            }
            Action::Open if self.queue_open => return self.selected.clone().map(MiniAction::Play),
            _ => {}
        }
        let state = self.state.as_ref()?;
        match action {
            Action::TogglePause => Some(MiniAction::TogglePause),
            Action::PreviousTrack => Some(MiniAction::Previous),
            Action::NextTrack => Some(MiniAction::Next),
            Action::Volume(delta) => Some(MiniAction::SetVolume(
                (i16::from(state.volume) + delta).clamp(0, 100) as u8,
            )),
            Action::ToggleMute => Some(MiniAction::SetVolume(if state.volume == 0 {
                self.audible_volume
            } else {
                0
            })),
            Action::ToggleShuffle => Some(MiniAction::SetShuffle(!state.shuffle)),
            Action::CycleRepeat => Some(MiniAction::SetRepeat(state.repeat.next())),
            Action::Seek(delta) => Some(MiniAction::SeekTo((state.position + f64::from(delta)).max(0.0))),
            _ => None,
        }
    }

    pub(crate) fn mouse(&mut self, mouse: MouseEvent) -> Option<MiniAction> {
        match mouse.kind {
            MouseEventKind::ScrollDown if self.queue_open => self.move_selection(1),
            MouseEventKind::ScrollUp if self.queue_open => self.move_selection(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                let (_, intent) = self.hits.intent_at(mouse.column, mouse.row)?;
                return match intent {
                    PointerIntent::Seek(fraction) => self
                        .state
                        .as_ref()
                        .filter(|state| state.duration > 0.0)
                        .map(|state| MiniAction::SeekTo(state.duration * fraction)),
                    PointerIntent::Volume(fraction) => {
                        Some(MiniAction::SetVolume((fraction * 100.0).round() as u8))
                    }
                    PointerIntent::Hit(hit) => self.hit(hit),
                    _ => None,
                };
            }
            _ => {}
        }
        None
    }

    fn hit(&mut self, hit: Hit) -> Option<MiniAction> {
        match hit {
            Hit::Previous => self.action(Action::PreviousTrack),
            Hit::PlayPause => self.action(Action::TogglePause),
            Hit::Next => self.action(Action::NextTrack),
            Hit::Shuffle => self.action(Action::ToggleShuffle),
            Hit::Repeat => self.action(Action::CycleRepeat),
            Hit::Mute => self.action(Action::ToggleMute),
            Hit::KeyHint(action) => self.action(action),
            Hit::UpNext(index) => {
                let id = self.state.as_ref()?.queue.get(index)?.id.clone();
                self.selected = Some(id.clone());
                Some(MiniAction::Play(id))
            }
            _ => None,
        }
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame, theme: &Theme) {
        let area = frame.area();
        self.hits = HitMap::bounded(area);
        let buffer = frame.buffer_mut();
        fill(buffer, area, theme.surface);
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            put(
                buffer,
                area,
                area.y,
                vec![span("Resize to 16 x 3. Q closes.", Style::new().fg(theme.muted))],
            );
            return;
        }
        let now = Instant::now();
        let playback_moving = self.animations
            && self
                .state
                .as_ref()
                .is_some_and(|state| !state.paused && state.current.is_some());
        if playback_moving {
            let tick_millis = crate::app::TICK_MILLIS;
            let elapsed_ticks = now.duration_since(self.animation_at).as_millis() as u64 / tick_millis;
            self.animation_frame = self.animation_frame.wrapping_add(elapsed_ticks);
            self.animation_at += Duration::from_millis(elapsed_ticks * tick_millis);
        } else {
            self.animation_at = now;
        }
        let footer_height = u16::from(area.height > PLAYER_BAR_HEIGHT);
        let content = Rect::new(area.x, area.y, area.width, area.height - footer_height);
        let selected = self.selected_index();
        if let Some(state) = self.state.as_ref() {
            if self.queue_open {
                let queue_theme = theme.playback_details(state.focus_active);
                self.hits
                    .extend(draw_queue(buffer, content, state, selected, &queue_theme));
            } else {
                let player_state = render_state(state);
                self.hits.extend(player_bar(
                    buffer,
                    content,
                    BarView {
                        player_state: &player_state,
                        seek_preview_fraction: None,
                        volume_preview_percent: None,
                        volume_entry: None,
                        current_track_liked: false,
                        focus_active: state.focus_active,
                        icons: &self.icons,
                        theme,
                        motion: Motion {
                            animation_frame: self.animation_frame,
                            animations_enabled: self.animations,
                            playback_moving,
                        },
                    },
                ));
            }
        } else {
            let message = if self.connection_error.is_some() {
                "Reconnecting to redwax..."
            } else {
                "Connecting to redwax..."
            };
            put(
                buffer,
                content,
                content.y,
                vec![span(message, Style::new().fg(theme.warning))],
            );
            if let Some(error) = self.connection_error.as_deref() {
                let detail = error
                    .strip_prefix("The redwax session is unavailable: ")
                    .unwrap_or(error);
                put(
                    buffer,
                    content,
                    content.y + 1,
                    vec![span(detail.to_string(), Style::new().fg(theme.muted))],
                );
            }
            put(
                buffer,
                content,
                content.y + 2,
                vec![
                    span("Q", Style::new().fg(theme.accent)),
                    span(" close", Style::new().fg(theme.foreground)),
                ],
            );
            self.hits.add(
                Rect::new(content.x, content.y + 2, 7, 1),
                Hit::KeyHint(Action::Quit),
            );
        }
        let error = self
            .error
            .as_ref()
            .filter(|(_, started)| started.elapsed() < ERROR_DURATION);
        if footer_height > 0 {
            let footer = Rect::new(area.x, area.bottom() - 1, area.width, 1);
            if let Some((message, _)) = error {
                put(
                    buffer,
                    footer,
                    footer.y,
                    vec![span(message.clone(), Style::new().fg(theme.error))],
                );
            } else {
                self.hits
                    .extend(draw_footer(buffer, footer, self.queue_open, self.owned, theme));
            }
        } else if let Some((message, _)) = error {
            fill(
                buffer,
                Rect::new(content.x, content.y, content.width, 1),
                theme.surface,
            );
            put(
                buffer,
                content,
                content.y,
                vec![span(message.clone(), Style::new().fg(theme.error))],
            );
        }
    }
}

fn render_state(state: &MiniState) -> PlayerState {
    let current_track = state.current.as_ref().map(|_| Track {
        video_id: String::new(),
        title: state.title.clone(),
        artists: if state.artist.is_empty() {
            Vec::new()
        } else {
            vec![state.artist.clone()]
        },
        album: None,
        album_browse_id: None,
        duration_seconds: (state.duration > 0.0).then_some(state.duration as u32),
        thumbnail_url: None,
        liked: false,
        playlist_entry: None,
    });
    PlayerState {
        index: current_track.as_ref().map(|_| 0),
        queue: current_track.into_iter().collect(),
        position: state.position,
        duration: (state.duration > 0.0).then_some(state.duration),
        paused: state.paused,
        volume: state.volume,
        muted: state.volume == 0,
        shuffle: state.shuffle,
        repeat: state.repeat,
        ..PlayerState::default()
    }
}

fn draw_queue(
    buffer: &mut ratatui::buffer::Buffer,
    area: Rect,
    state: &MiniState,
    selected: usize,
    theme: &Theme,
) -> Vec<(Rect, Hit)> {
    let content = Rect::new(area.x + 2, area.y, area.width - 4, area.height);
    let heading = format!(
        "{}QUEUE  {} / {}",
        if state.focus_active { "FOCUS ON  " } else { "" },
        if state.queue.is_empty() { 0 } else { selected + 1 },
        state.queue.len()
    );
    put(
        buffer,
        content,
        content.y,
        vec![span(
            heading,
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        )],
    );
    if state.queue.is_empty() {
        put(
            buffer,
            content,
            content.y + 1,
            vec![span("The queue is empty.", Style::new().fg(theme.muted))],
        );
        return Vec::new();
    }
    let visible = usize::from(content.height.saturating_sub(1));
    let start = selected
        .saturating_sub(visible / 2)
        .min(state.queue.len().saturating_sub(visible));
    let mut hits = Vec::new();
    for (row, track) in state.queue.iter().enumerate().skip(start).take(visible) {
        let row_area = Rect::new(content.x, content.y + 1 + (row - start) as u16, content.width, 1);
        if row == selected {
            fill(buffer, row_area, theme.panel);
        }
        let marker = if state.current.as_deref() == Some(track.id.as_str()) {
            "\u{258e} "
        } else {
            "  "
        };
        let artist_width = if content.width >= 40 {
            width_of(&track.artist).min(usize::from(content.width) / 3)
        } else {
            0
        };
        let title_width =
            usize::from(content.width).saturating_sub(2 + artist_width + usize::from(artist_width > 0) * 3);
        let mut spans = vec![
            span(marker, Style::new().fg(theme.accent)),
            span(
                fit(&track.title, title_width),
                Style::new()
                    .fg(theme.foreground)
                    .add_modifier(if row == selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];
        if artist_width > 0 {
            spans.push(span("   ", Style::new().fg(theme.muted)));
            spans.push(span(fit(&track.artist, artist_width), Style::new().fg(theme.dim)));
        }
        put(buffer, row_area, row_area.y, spans);
        hits.push((row_area, Hit::UpNext(row)));
    }
    hits
}

fn draw_footer(
    buffer: &mut ratatui::buffer::Buffer,
    area: Rect,
    queue_open: bool,
    owned: bool,
    theme: &Theme,
) -> Vec<(Rect, Hit)> {
    let mut column = area.x + if area.width < 24 { 0 } else { 2 };
    let gap = if area.width < 24 { 1 } else { 3 };
    let mut hints = if queue_open {
        vec![
            ("q", "player", Action::JumpToQueue),
            ("Q", "close", Action::Quit),
            ("enter", "play", Action::Open),
        ]
    } else {
        vec![
            ("q", "queue", Action::JumpToQueue),
            ("Q", "close", Action::Quit),
            ("space", "pause", Action::TogglePause),
            ("n/p", "skip", Action::NextTrack),
        ]
    };
    if owned {
        hints.insert(1, ("F", "full", Action::Back));
    }
    let mut hits = Vec::new();
    for (key, label, action) in hints {
        let width = (width_of(key) + 1 + width_of(label)) as u16;
        if column + width > area.right() {
            break;
        }
        let hint_area = Rect::new(column, area.y, width, 1);
        put(
            buffer,
            hint_area,
            area.y,
            vec![
                span(key, Style::new().fg(theme.accent)),
                span(format!(" {label}"), Style::new().fg(theme.foreground)),
            ],
        );
        hits.push((hint_area, Hit::KeyHint(action)));
        if key == "n/p" {
            hits.push((
                Rect::new(column + 2, area.y, 1, 1),
                Hit::KeyHint(Action::PreviousTrack),
            ));
        }
        column += width + gap;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn state() -> MiniState {
        MiniState {
            title: "Focus song".to_string(),
            artist: "Focus artist".to_string(),
            duration: 180.0,
            position: 30.0,
            current: Some("/track/one".to_string()),
            queue: vec![MiniTrack {
                id: "/track/one".to_string(),
                title: "Focus song".to_string(),
                artist: "Focus artist".to_string(),
            }],
            ..MiniState::default()
        }
    }

    #[test]
    fn focus_reaches_local_and_remote_mini_views_and_restores_regular_colors() {
        let config = Config {
            icons: "text".to_string(),
            ..Config::default()
        };
        let mut view = MiniView::new(&config);
        let mut theme = crate::theme::redwax_theme();
        theme.error = ratatui::style::Color::Green;
        let mut terminal = Terminal::new(TestBackend::new(100, 4)).unwrap();
        view.replace_state(state());
        view.set_focus_active(true);
        terminal.draw(|frame| view.draw(frame, &theme)).unwrap();

        let buffer = terminal.backend().buffer();
        let top: String = (0..100).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(top.contains("FOCUS ON"));
        assert!(top.contains("Focus song"));
        assert!(top.contains("Focus artist"));
        for y in 0..PLAYER_BAR_HEIGHT {
            for x in 0..100 {
                let cell = &buffer[(x, y)];
                if !cell.symbol().trim().is_empty() {
                    assert_eq!(cell.fg, crate::theme::FOCUS_RED);
                }
            }
        }

        let mut remote_state = state();
        remote_state.focus_active = false;
        view.replace_state(remote_state);
        terminal.draw(|frame| view.draw(frame, &theme)).unwrap();
        let buffer = terminal.backend().buffer();
        let top: String = (0..100).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(!top.contains("FOCUS ON"));
        let title_x = (0..100).find(|x| buffer[(*x, 0)].symbol() == "F").unwrap();
        assert_eq!(buffer[(title_x, 0)].fg, theme.foreground);
    }

    #[test]
    fn local_player_updates_keep_focus_and_queue_view_keeps_the_warning() {
        let mut view = MiniView::new(&Config::default());
        view.set_focus_active(true);
        let player = PlayerState {
            queue: vec![redwax_player::bare_track("song", "Focus song")],
            index: Some(0),
            ..PlayerState::default()
        };
        view.replace_player_state(&player);
        assert!(view.state.as_ref().unwrap().focus_active);
        view.queue_open = true;
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        terminal
            .draw(|frame| view.draw(frame, &crate::theme::redwax_theme()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let top: String = (0..40).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(top.contains("FOCUS ON"));
        assert_eq!(buffer[(2, 0)].fg, crate::theme::FOCUS_RED);
    }
}
