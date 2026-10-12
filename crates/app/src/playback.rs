use std::sync::{Arc, Mutex};

use redwax_player::{PlayerControl, PlayerState, QueueEdit, QueueEntryId, QueueRevision, Repeat, Result};
use redwax_ytm::{RadioContinuation, Track};
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::radio::Radio;

pub(crate) enum StationEvent {
    Ready {
        station_generation: u64,
        seed: Track,
        tracks: Vec<Track>,
        continuation: Option<RadioContinuation>,
        from_playing: bool,
        playing_video_id: Option<String>,
    },
    Failed {
        station_generation: u64,
        error: String,
    },
    Applied {
        station_generation: u64,
        label: String,
        error: Option<String>,
    },
    More {
        station_generation: u64,
        tracks: Vec<Track>,
        continuation: Option<RadioContinuation>,
        player_state: PlayerState,
    },
    MoreFailed {
        station_generation: u64,
        error: String,
        rejected: bool,
    },
    Appended {
        station_generation: u64,
        error: Option<String>,
    },
    Tick(PlayerState),
}

pub(crate) enum StationEffect {
    None,
    Cancelled,
    Empty,
    Apply {
        station_generation: u64,
        label: String,
        tracks: Vec<Track>,
        upcoming: bool,
    },
    Append {
        station_generation: u64,
        tracks: Vec<Track>,
    },
    Fetch {
        station_generation: u64,
        seed: Track,
        continuation: Option<RadioContinuation>,
    },
    Started(String),
    Retry {
        error: String,
        refresh_session: bool,
    },
    Error(String),
}

#[derive(Clone)]
pub struct Playback<P: PlayerControl> {
    inner: Arc<PlaybackInner<P>>,
}

struct PlaybackInner<P: PlayerControl> {
    player: P,
    visible_state: watch::Sender<PlayerState>,
    coordination: Mutex<Coordination>,
    queue_edits: tokio::sync::Mutex<()>,
}

struct Coordination {
    bootstrap: Bootstrap,
    recovery_state: Option<PlayerState>,
    producer: Producer,
    radio: Radio,
    next_edit_id: u64,
    active_clear_id: Option<u64>,
    unresolved_clear: Option<PendingClear>,
    page_tail_generation: u64,
}

enum Bootstrap {
    Restored(PlayerState),
    Feeding(PlayerState),
    Live,
}

enum Producer {
    None,
    PageTail(PageTail),
    Radio,
    Quiesced(Box<Producer>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueEditPreview {
    edit_id: u64,
    queue_revision: QueueRevision,
    affected_entries: usize,
    keeps_current: bool,
    playing_entry_id: Option<QueueEntryId>,
    playing_index: Option<usize>,
    producer_suffix: &'static str,
}

impl QueueEditPreview {
    pub fn affected(self) -> usize {
        self.affected_entries
    }

    pub fn keeps_current(self) -> bool {
        self.keeps_current
    }

    pub fn producer_suffix(self) -> &'static str {
        self.producer_suffix
    }
}

struct PageTail {
    page_key: String,
    application_generation: u64,
    expected_video_ids: Vec<String>,
    observed_entry_ids: Option<Vec<QueueEntryId>>,
    previous_queue_revision: Option<QueueRevision>,
    phase: PageTailPhase,
}

enum PageTailPhase {
    Applying {
        confirmed_queue: Option<(QueueRevision, Option<Vec<QueueEntryId>>)>,
    },
    AwaitingQueue {
        deadline_millis: u64,
    },
    Ready,
}

struct PageTailApplication<'a, P: PlayerControl> {
    playback: &'a Playback<P>,
    application_generation: Option<u64>,
}

impl<P: PlayerControl> PageTailApplication<'_, P> {
    fn finish(mut self, applied: bool) {
        if let Some(application_generation) = self.application_generation.take() {
            self.playback.page_tail_applied(application_generation, applied);
        }
    }
}

impl<P: PlayerControl> Drop for PageTailApplication<'_, P> {
    fn drop(&mut self) {
        if let Some(application_generation) = self.application_generation.take() {
            self.playback.page_tail_applied(application_generation, false);
        }
    }
}

struct PendingClear {
    before_clear: PlayerState,
    retained_entry: Option<(Option<QueueEntryId>, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClearOutcome {
    Applied,
    Unchanged,
    Diverged,
    Unknown,
}

impl<P: PlayerControl> Playback<P> {
    pub fn new(player: P, restored_state: PlayerState) -> Playback<P> {
        let (visible_state, _) = watch::channel(restored_state.clone());
        Playback {
            inner: Arc::new(PlaybackInner {
                player,
                visible_state,
                queue_edits: tokio::sync::Mutex::new(()),
                coordination: Mutex::new(Coordination {
                    bootstrap: Bootstrap::Restored(restored_state),
                    recovery_state: None,
                    producer: Producer::None,
                    radio: Radio::default(),
                    next_edit_id: 0,
                    active_clear_id: None,
                    unresolved_clear: None,
                    page_tail_generation: 0,
                }),
            }),
        }
    }

    pub fn observe(&self, observed_state: PlayerState) -> PlayerState {
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        if observed_state.availability == redwax_player::PlaybackAvailability::Reconnecting
            && coordination.recovery_state.is_none()
        {
            let visible_state = self.inner.visible_state.borrow().clone();
            if !visible_state.queue.is_empty() {
                coordination.recovery_state = Some(visible_state);
            }
        }
        let recovery_is_ready = coordination
            .recovery_state
            .as_ref()
            .is_some_and(|recovered_state| {
                observed_state.availability == redwax_player::PlaybackAvailability::RestoredPaused
                    && observed_state.queue_entries.len() == recovered_state.queue.len()
                    && observed_state.queue.len() == recovered_state.queue.len()
            });
        let visible_state = if let Some(recovered_state) = coordination
            .recovery_state
            .as_ref()
            .filter(|_| !recovery_is_ready)
        {
            PlayerState {
                queue: recovered_state.queue.clone(),
                queue_entries: recovered_state.queue_entries.clone(),
                queue_revision: recovered_state.queue_revision,
                index: recovered_state.index,
                position: recovered_state.position,
                duration: recovered_state.duration,
                paused: true,
                loading: observed_state.availability == redwax_player::PlaybackAvailability::Reconnecting,
                ..observed_state
            }
        } else {
            if recovery_is_ready {
                coordination.recovery_state = None;
            }
            match &coordination.bootstrap {
                Bootstrap::Restored(restored) | Bootstrap::Feeding(restored)
                    if observed_state.queue.is_empty() =>
                {
                    PlayerState {
                        queue: restored.queue.clone(),
                        queue_entries: restored.queue_entries.clone(),
                        queue_revision: restored.queue_revision,
                        index: restored.index,
                        position: restored.position,
                        duration: restored.duration,
                        paused: true,
                        loading: observed_state.pending_track.is_some(),
                        ..observed_state
                    }
                }
                _ => {
                    coordination.bootstrap = Bootstrap::Live;
                    observed_state
                }
            }
        };
        if let Producer::PageTail(page_tail) = &mut coordination.producer
            && !page_tail.observe(&visible_state, crate::app::clock())
        {
            coordination.producer = Producer::None;
        }
        reconcile_pending_clear(&mut coordination, &visible_state, false);
        self.inner.visible_state.send_replace(visible_state.clone());
        visible_state
    }

    pub fn restored_pending(&self) -> bool {
        matches!(
            self.inner
                .coordination
                .lock()
                .expect("playback coordination")
                .bootstrap,
            Bootstrap::Restored(_)
        )
    }

    pub fn confirmed_state(&self) -> PlayerState {
        self.inner.player.snapshot()
    }

    pub fn needs_reconciliation(&self) -> bool {
        self.inner
            .coordination
            .lock()
            .expect("playback coordination")
            .unresolved_clear
            .is_some()
    }

    pub async fn resume_restored(&self) -> Result<bool> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        let restored_state = {
            let mut coordination = self.inner.coordination.lock().expect("playback coordination");
            let Bootstrap::Restored(restored_state) = &coordination.bootstrap else {
                return Ok(false);
            };
            let restored_state = restored_state.clone();
            coordination.bootstrap = Bootstrap::Feeding(restored_state.clone());
            restored_state
        };
        if restored_state.queue.is_empty() {
            self.inner
                .coordination
                .lock()
                .expect("playback coordination")
                .bootstrap = Bootstrap::Live;
            return Ok(false);
        }
        let restored_queue_index = restored_state
            .index
            .unwrap_or(0)
            .min(restored_state.queue.len().saturating_sub(1));
        let play_result = self
            .inner
            .player
            .play_from(
                restored_state.queue.clone(),
                restored_queue_index,
                restored_state.position,
            )
            .await;
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        coordination.bootstrap = if play_result.is_ok() {
            Bootstrap::Live
        } else {
            Bootstrap::Restored(restored_state)
        };
        play_result.map(|()| true)
    }

    pub fn stop_producer(&self) {
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        coordination.stop_producer();
    }

    pub fn preview_clear_upcoming(&self) -> Option<QueueEditPreview> {
        let visible_state = self.snapshot();
        let keeps_current = visible_state
            .index
            .is_some_and(|playing_index| playing_index < visible_state.queue.len());
        let affected_entries = visible_state
            .queue
            .len()
            .saturating_sub(usize::from(keeps_current));
        if affected_entries == 0 {
            return None;
        }
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        coordination.next_edit_id = coordination.next_edit_id.wrapping_add(1);
        let edit_id = coordination.next_edit_id;
        coordination.active_clear_id = Some(edit_id);
        Some(QueueEditPreview {
            edit_id,
            queue_revision: visible_state.queue_revision,
            affected_entries,
            keeps_current,
            playing_entry_id: visible_state
                .index
                .and_then(|playing_index| visible_state.queue_entries.get(playing_index))
                .map(|queue_entry| queue_entry.id()),
            playing_index: visible_state.index,
            producer_suffix: match &coordination.producer {
                Producer::Radio => " and stop radio",
                Producer::PageTail(_) => " and stop automatic queue loading",
                Producer::None | Producer::Quiesced(_) => "",
            },
        })
    }

    pub fn cancel_clear(&self, preview: QueueEditPreview) {
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        if coordination.active_clear_id == Some(preview.edit_id) {
            coordination.active_clear_id = None;
        }
    }

    pub async fn clear_upcoming(&self, preview: QueueEditPreview) -> Result<QueueEdit> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        let state_before_clear = self.inner.player.snapshot();
        let preview_is_current = preview_matches(&state_before_clear, preview);
        {
            let mut coordination = self.inner.coordination.lock().expect("playback coordination");
            if coordination.active_clear_id != Some(preview.edit_id) || !preview_is_current {
                if coordination.active_clear_id == Some(preview.edit_id) {
                    coordination.active_clear_id = None;
                }
                return Ok(QueueEdit::Stale);
            }
            coordination.active_clear_id = None;
            coordination.unresolved_clear = None;
            let active_producer = std::mem::replace(&mut coordination.producer, Producer::None);
            coordination.producer = Producer::Quiesced(Box::new(active_producer));
        }

        let retained_entry = state_before_clear.index.and_then(|playing_index| {
            state_before_clear
                .queue_entries
                .get(playing_index)
                .map(|queue_entry| (Some(queue_entry.id()), queue_entry.track().video_id.clone()))
                .or_else(|| {
                    state_before_clear
                        .queue
                        .get(playing_index)
                        .map(|track| (None, track.video_id.clone()))
                })
        });
        let mut player_state_updates = self.inner.player.state();
        let clear_result = self.inner.player.clear().await;
        let mut clear_outcome = self
            .observe_clear_outcome(
                &mut player_state_updates,
                retained_entry.as_ref(),
                &state_before_clear,
            )
            .await;
        if matches!(clear_outcome, ClearOutcome::Unknown) {
            clear_outcome = match self.inner.player.reconcile_queue().await {
                Ok(reconciled_state) => authoritative_clear_outcome(
                    &reconciled_state,
                    retained_entry.as_ref(),
                    &state_before_clear,
                ),
                Err(_) => ClearOutcome::Unknown,
            };
        }
        match (clear_result, clear_outcome) {
            (_, ClearOutcome::Applied) => {
                let mut coordination = self.inner.coordination.lock().expect("playback coordination");
                coordination.radio.stop();
                coordination.producer = Producer::None;
                coordination.unresolved_clear = None;
                Ok(QueueEdit::Applied)
            }
            (Err(error), ClearOutcome::Unchanged) => {
                self.resume_quiesced();
                Err(error)
            }
            (Ok(()), ClearOutcome::Unchanged) => {
                self.resume_quiesced();
                Err(redwax_player::Error::Mpv(
                    "mpv did not clear the upcoming queue".to_string(),
                ))
            }
            (_, ClearOutcome::Diverged) => {
                self.stop_producer();
                Err(redwax_player::Error::Mpv(
                    "the queue changed while it was being cleared. Review the current queue".to_string(),
                ))
            }
            (Err(error), ClearOutcome::Unknown) => {
                self.remember_unresolved_clear(state_before_clear, retained_entry);
                Err(redwax_player::Error::Mpv(format!(
                    "queue clear outcome is unknown after {error}. Use Retry playback to reconcile"
                )))
            }
            (Ok(()), ClearOutcome::Unknown) => {
                self.remember_unresolved_clear(state_before_clear, retained_entry);
                Err(redwax_player::Error::Mpv(
                    "queue clear outcome is unknown. Use Retry playback to reconcile".to_string(),
                ))
            }
        }
    }

    async fn observe_clear_outcome(
        &self,
        player_state_updates: &mut watch::Receiver<PlayerState>,
        retained_entry: Option<&(Option<QueueEntryId>, String)>,
        state_before_clear: &PlayerState,
    ) -> ClearOutcome {
        let observation_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let observed_state = player_state_updates.borrow().clone();
            let observed_outcome = clear_outcome(&observed_state, retained_entry, state_before_clear);
            if !matches!(observed_outcome, ClearOutcome::Unknown) {
                return observed_outcome;
            }
            let remaining_wait = observation_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_wait.is_zero() {
                return ClearOutcome::Unknown;
            }
            match tokio::time::timeout(remaining_wait, player_state_updates.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return ClearOutcome::Unknown,
            }
        }
    }

    fn resume_quiesced(&self) {
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        if let Producer::Quiesced(quiesced_producer) =
            std::mem::replace(&mut coordination.producer, Producer::None)
        {
            coordination.producer = *quiesced_producer;
        }
        coordination.unresolved_clear = None;
    }

    fn remember_unresolved_clear(
        &self,
        state_before_clear: PlayerState,
        retained_entry: Option<(Option<QueueEntryId>, String)>,
    ) {
        self.inner
            .coordination
            .lock()
            .expect("playback coordination")
            .unresolved_clear = Some(PendingClear {
            before_clear: state_before_clear,
            retained_entry,
        });
    }

    async fn reconcile_clear(&self) -> Result<()> {
        let reconciliation_needed = self
            .inner
            .coordination
            .lock()
            .expect("playback coordination")
            .unresolved_clear
            .is_some();
        if !reconciliation_needed {
            return Ok(());
        }
        let reconciled_state = self.inner.player.reconcile_queue().await?;
        let visible_state = self.observe(reconciled_state);
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        reconcile_pending_clear(&mut coordination, &visible_state, true);
        Ok(())
    }

    pub async fn play_page_tail(
        &self,
        tracks: Vec<Track>,
        selected_track_index: usize,
        page_tail_key: Option<String>,
    ) -> Result<()> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        self.stop_producer();
        let previous_state = self.inner.player.snapshot();
        let application_generation = {
            let mut coordination = self.inner.coordination.lock().expect("playback coordination");
            coordination.bootstrap = Bootstrap::Live;
            page_tail_key.filter(|_| !tracks.is_empty()).map(|page_key| {
                coordination.page_tail_generation = coordination
                    .page_tail_generation
                    .checked_add(1)
                    .expect("page tail generations cannot wrap during one application lifetime");
                let application_generation = coordination.page_tail_generation;
                coordination.producer = Producer::PageTail(PageTail {
                    page_key,
                    application_generation,
                    expected_video_ids: tracks.iter().map(|track| track.video_id.clone()).collect(),
                    observed_entry_ids: None,
                    previous_queue_revision: (!previous_state
                        .queue
                        .iter()
                        .map(|track| &track.video_id)
                        .eq(tracks.iter().map(|track| &track.video_id)))
                    .then_some(previous_state.queue_revision),
                    phase: PageTailPhase::Applying {
                        confirmed_queue: None,
                    },
                });
                application_generation
            })
        };
        let application = application_generation.map(|application_generation| PageTailApplication {
            playback: self,
            application_generation: Some(application_generation),
        });
        let play_result = self.inner.player.play(tracks, selected_track_index).await;
        if let Some(application) = application {
            application.finish(play_result.is_ok());
        }
        play_result
    }

    fn page_tail_applied(&self, application_generation: u64, applied: bool) {
        let player_state = self.inner.player.snapshot();
        let now_millis = crate::app::clock();
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        let Producer::PageTail(page_tail) = &mut coordination.producer else {
            return;
        };
        if page_tail.application_generation != application_generation {
            return;
        }
        if !applied {
            coordination.producer = Producer::None;
            return;
        }
        page_tail.acknowledge(&player_state, now_millis);
    }

    pub async fn select(&self, queue_index: usize, restored_position: f64) -> Result<()> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        let restored_state = {
            let coordination = self.inner.coordination.lock().expect("playback coordination");
            match &coordination.bootstrap {
                Bootstrap::Restored(restored_state) | Bootstrap::Feeding(restored_state) => {
                    Some(restored_state.clone())
                }
                Bootstrap::Live => None,
            }
        };
        if let Some(restored_state) = restored_state {
            let start_position = if restored_state.index == Some(queue_index) {
                restored_position
            } else {
                0.0
            };
            self.stop_producer();
            self.inner
                .coordination
                .lock()
                .expect("playback coordination")
                .bootstrap = Bootstrap::Live;
            self.inner
                .player
                .play_from(restored_state.queue, queue_index, start_position)
                .await
        } else {
            self.inner.player.jump(queue_index).await
        }
    }

    pub async fn append_page_tail(&self, page_key: &str, tracks: Vec<Track>) -> Result<bool> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        if tracks.is_empty() {
            return Ok(false);
        }
        let application_generation = {
            let mut coordination = self.inner.coordination.lock().expect("playback coordination");
            if !matches!(&coordination.producer, Producer::PageTail(page_tail) if page_tail.page_key == page_key)
            {
                return Ok(false);
            }
            coordination.page_tail_generation = coordination
                .page_tail_generation
                .checked_add(1)
                .expect("page tail generations cannot wrap during one application lifetime");
            let application_generation = coordination.page_tail_generation;
            let Producer::PageTail(page_tail) = &mut coordination.producer else {
                unreachable!("the page tail was checked while holding the coordination lock");
            };
            page_tail.application_generation = application_generation;
            page_tail
                .expected_video_ids
                .extend(tracks.iter().map(|track| track.video_id.clone()));
            page_tail.phase = PageTailPhase::Applying {
                confirmed_queue: None,
            };
            application_generation
        };
        let application = PageTailApplication {
            playback: self,
            application_generation: Some(application_generation),
        };
        let append_result = self.inner.player.enqueue(tracks, false).await;
        application.finish(append_result.is_ok());
        append_result.map(|()| true)
    }

    pub fn stop_page_tail(&self, page_key: &str) {
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        if matches!(&coordination.producer, Producer::PageTail(page_tail) if page_tail.page_key == page_key) {
            coordination.producer = Producer::None;
        }
    }

    pub(crate) fn request_station(&self, seed: Track) -> u64 {
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        coordination.producer = Producer::Radio;
        coordination.radio.begin(seed)
    }

    fn radio_accepts(&self, station_generation: u64) -> bool {
        let coordination = self.inner.coordination.lock().expect("playback coordination");
        matches!(&coordination.producer, Producer::Radio) && coordination.radio.accepts(station_generation)
    }

    pub(crate) fn hold_station_task(&self, station_generation: u64, task_handle: AbortHandle) {
        self.inner
            .coordination
            .lock()
            .expect("playback coordination")
            .radio
            .hold(station_generation, task_handle);
    }

    pub(crate) async fn apply_radio(&self, station_generation: u64, tracks: Vec<Track>) -> Result<bool> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        if !self.radio_accepts(station_generation) {
            return Ok(false);
        }
        self.inner.player.play(tracks, 0).await?;
        Ok(self.radio_accepts(station_generation))
    }

    pub(crate) async fn apply_radio_upcoming(
        &self,
        station_generation: u64,
        tracks: Vec<Track>,
    ) -> Result<bool> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        if !self.radio_accepts(station_generation) {
            return Ok(false);
        }
        self.inner.player.replace_upcoming(tracks).await?;
        Ok(self.radio_accepts(station_generation))
    }

    pub(crate) async fn append_radio(&self, station_generation: u64, tracks: Vec<Track>) -> Result<bool> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        if !self.radio_accepts(station_generation) {
            return Ok(false);
        }
        self.inner.player.enqueue(tracks, false).await?;
        Ok(self.radio_accepts(station_generation))
    }

    pub(crate) fn radio_enabled(&self) -> bool {
        self.inner
            .coordination
            .lock()
            .expect("playback coordination")
            .radio
            .enabled()
    }

    pub(crate) fn transition_station(&self, station_event: StationEvent, now_millis: u64) -> StationEffect {
        let mut coordination = self.inner.coordination.lock().expect("playback coordination");
        if let StationEvent::Tick(player_state) = &station_event
            && let Producer::PageTail(page_tail) = &mut coordination.producer
            && !page_tail.observe(player_state, now_millis)
        {
            coordination.producer = Producer::None;
        }
        if !matches!(coordination.producer, Producer::Radio) {
            return StationEffect::None;
        }
        let station_generation = match &station_event {
            StationEvent::Ready {
                station_generation, ..
            }
            | StationEvent::Failed {
                station_generation, ..
            }
            | StationEvent::Applied {
                station_generation, ..
            }
            | StationEvent::More {
                station_generation, ..
            }
            | StationEvent::MoreFailed {
                station_generation, ..
            }
            | StationEvent::Appended {
                station_generation, ..
            } => Some(*station_generation),
            StationEvent::Tick(_) => None,
        };
        if station_generation.is_some_and(|generation| !coordination.radio.accepts(generation)) {
            return StationEffect::None;
        }
        match station_event {
            StationEvent::Ready {
                station_generation,
                seed,
                tracks,
                continuation,
                from_playing,
                playing_video_id,
            } => {
                if from_playing && playing_video_id.as_deref() != Some(seed.video_id.as_str()) {
                    coordination.stop_producer();
                    return StationEffect::Cancelled;
                }
                let Some(unique_tracks) =
                    coordination
                        .radio
                        .prepare(station_generation, tracks, continuation)
                else {
                    return StationEffect::None;
                };
                if unique_tracks.is_empty() {
                    coordination.stop_producer();
                    return StationEffect::Empty;
                }
                StationEffect::Apply {
                    station_generation,
                    label: format!("{} · {}", seed.title, seed.artist_line()),
                    tracks: if from_playing {
                        unique_tracks.into_iter().skip(1).collect()
                    } else {
                        unique_tracks
                    },
                    upcoming: from_playing,
                }
            }
            StationEvent::Failed { error, .. } => {
                coordination.stop_producer();
                StationEffect::Error(error)
            }
            StationEvent::Applied {
                station_generation,
                label,
                error,
            } => {
                if let Some(error) = error {
                    coordination.stop_producer();
                    return StationEffect::Error(error);
                }
                if coordination.radio.started(station_generation, now_millis) {
                    StationEffect::Started(label)
                } else {
                    StationEffect::None
                }
            }
            StationEvent::More {
                station_generation,
                tracks,
                continuation,
                player_state,
            } => {
                let Some(unique_tracks) = coordination.radio.received(
                    station_generation,
                    tracks,
                    continuation,
                    &player_state,
                    now_millis,
                ) else {
                    return StationEffect::None;
                };
                if unique_tracks.is_empty() {
                    return StationEffect::Retry {
                        error: "Radio returned no new tracks".to_string(),
                        refresh_session: false,
                    };
                }
                StationEffect::Append {
                    station_generation,
                    tracks: unique_tracks,
                }
            }
            StationEvent::MoreFailed {
                station_generation,
                error,
                rejected,
            } => {
                if coordination.radio.failed(station_generation, now_millis) {
                    StationEffect::Retry {
                        error,
                        refresh_session: rejected,
                    }
                } else {
                    StationEffect::None
                }
            }
            StationEvent::Appended {
                station_generation,
                error,
            } => {
                if let Some(error) = error {
                    coordination.stop_producer();
                    return StationEffect::Error(format!("Radio could not extend the queue: {error}"));
                }
                if coordination.radio.appended(station_generation, now_millis) {
                    station_refill_effect(&mut coordination.radio, &self.snapshot(), now_millis)
                } else {
                    StationEffect::None
                }
            }
            StationEvent::Tick(player_state) => {
                station_refill_effect(&mut coordination.radio, &player_state, now_millis)
            }
        }
    }
}

fn station_refill_effect(radio: &mut Radio, player_state: &PlayerState, now_millis: u64) -> StationEffect {
    match radio.refill(player_state, now_millis) {
        Some((station_generation, seed, continuation)) => StationEffect::Fetch {
            station_generation,
            seed,
            continuation,
        },
        None => StationEffect::None,
    }
}

impl Coordination {
    fn stop_producer(&mut self) {
        self.radio.stop();
        self.producer = Producer::None;
        self.unresolved_clear = None;
    }
}

fn clear_landed(player_state: &PlayerState, retained_entry: Option<&(Option<QueueEntryId>, String)>) -> bool {
    let Some((queue_entry_id, video_id)) = retained_entry else {
        return player_state.queue.is_empty();
    };
    if player_state.queue.len() != 1 || player_state.queue[0].video_id != *video_id {
        return false;
    }
    match queue_entry_id {
        Some(queue_entry_id) if !player_state.queue_entries.is_empty() => {
            player_state.queue_entries.len() == 1 && player_state.queue_entries[0].id() == *queue_entry_id
        }
        _ => true,
    }
}

fn preview_matches(player_state: &PlayerState, preview: QueueEditPreview) -> bool {
    player_state.queue_revision == preview.queue_revision
        && player_state.index == preview.playing_index
        && preview.playing_entry_id.is_none_or(|playing_entry_id| {
            player_state
                .index
                .and_then(|playing_index| player_state.queue_entries.get(playing_index))
                .is_some_and(|playing_entry| playing_entry.id() == playing_entry_id)
        })
        && player_state
            .queue
            .len()
            .saturating_sub(usize::from(preview.keeps_current))
            == preview.affected_entries
}

fn clear_outcome(
    player_state: &PlayerState,
    retained_entry: Option<&(Option<QueueEntryId>, String)>,
    state_before_clear: &PlayerState,
) -> ClearOutcome {
    if clear_landed(player_state, retained_entry) {
        return ClearOutcome::Applied;
    }
    if player_state.queue_revision == state_before_clear.queue_revision {
        return ClearOutcome::Unknown;
    }
    if same_queue(player_state, state_before_clear) {
        ClearOutcome::Unchanged
    } else {
        ClearOutcome::Diverged
    }
}

fn authoritative_clear_outcome(
    player_state: &PlayerState,
    retained_entry: Option<&(Option<QueueEntryId>, String)>,
    state_before_clear: &PlayerState,
) -> ClearOutcome {
    match clear_outcome(player_state, retained_entry, state_before_clear) {
        ClearOutcome::Unknown if same_queue(player_state, state_before_clear) => ClearOutcome::Unchanged,
        ClearOutcome::Unknown => ClearOutcome::Diverged,
        classified_outcome => classified_outcome,
    }
}

fn reconcile_pending_clear(
    coordination: &mut Coordination,
    observed_state: &PlayerState,
    authoritative_read: bool,
) {
    let classified_outcome = {
        let Some(pending_clear) = coordination.unresolved_clear.as_ref() else {
            return;
        };
        let observed_outcome = clear_outcome(
            observed_state,
            pending_clear.retained_entry.as_ref(),
            &pending_clear.before_clear,
        );
        if authoritative_read && matches!(observed_outcome, ClearOutcome::Unknown) {
            authoritative_clear_outcome(
                observed_state,
                pending_clear.retained_entry.as_ref(),
                &pending_clear.before_clear,
            )
        } else {
            observed_outcome
        }
    };
    match classified_outcome {
        ClearOutcome::Applied | ClearOutcome::Diverged => {
            coordination.radio.stop();
            coordination.producer = Producer::None;
            coordination.unresolved_clear = None;
        }
        ClearOutcome::Unchanged => {
            if let Producer::Quiesced(producer) =
                std::mem::replace(&mut coordination.producer, Producer::None)
            {
                coordination.producer = *producer;
            }
            coordination.unresolved_clear = None;
        }
        ClearOutcome::Unknown => {}
    }
}

fn same_queue(candidate_state: &PlayerState, reference_state: &PlayerState) -> bool {
    if candidate_state.index != reference_state.index || candidate_state.queue != reference_state.queue {
        return false;
    }
    if candidate_state.queue_entries.len() != candidate_state.queue.len()
        || reference_state.queue_entries.len() != reference_state.queue.len()
    {
        return true;
    }
    candidate_state
        .queue_entries
        .iter()
        .map(|queue_entry| queue_entry.id())
        .eq(reference_state
            .queue_entries
            .iter()
            .map(|queue_entry| queue_entry.id()))
}

impl PageTail {
    fn acknowledge(&mut self, player_state: &PlayerState, now_millis: u64) {
        let previous_phase = std::mem::replace(
            &mut self.phase,
            PageTailPhase::AwaitingQueue {
                deadline_millis: now_millis.saturating_add(5_000),
            },
        );
        if let PageTailPhase::Applying {
            confirmed_queue: Some((confirmed_revision, confirmed_entry_ids)),
        } = previous_phase
            && confirmed_revision == player_state.queue_revision
        {
            self.observed_entry_ids = confirmed_entry_ids;
            self.phase = PageTailPhase::Ready;
        }
    }

    fn observe(&mut self, player_state: &PlayerState, now_millis: u64) -> bool {
        let matches_expected = self.matches_expected_queue(player_state);
        let has_entry_identities = player_state.queue_entries.len() == player_state.queue.len();
        let queue_entry_ids = has_entry_identities.then(|| {
            player_state
                .queue_entries
                .iter()
                .map(|queue_entry| queue_entry.id())
                .collect::<Vec<_>>()
        });
        let preserves_entries = self.observed_entry_ids.as_ref().is_none_or(|previous_entry_ids| {
            queue_entry_ids.as_ref().is_none_or(|queue_entry_ids| {
                let current_entry_ids: std::collections::HashSet<_> = queue_entry_ids.iter().collect();
                previous_entry_ids
                    .iter()
                    .all(|entry_id| current_entry_ids.contains(entry_id))
                    && (!matches!(self.phase, PageTailPhase::Ready)
                        || queue_entry_ids.len() == previous_entry_ids.len())
            })
        });
        let queue_confirmed = matches_expected && preserves_entries;
        match &mut self.phase {
            PageTailPhase::Applying { confirmed_queue } => {
                *confirmed_queue = queue_confirmed.then_some((player_state.queue_revision, queue_entry_ids));
                true
            }
            PageTailPhase::AwaitingQueue { deadline_millis } => {
                if queue_confirmed {
                    self.observed_entry_ids = queue_entry_ids;
                    self.phase = PageTailPhase::Ready;
                    true
                } else {
                    now_millis < *deadline_millis
                }
            }
            PageTailPhase::Ready => queue_confirmed,
        }
    }

    fn matches_expected_queue(&self, player_state: &PlayerState) -> bool {
        if self.previous_queue_revision == Some(player_state.queue_revision)
            || player_state.queue.len() != self.expected_video_ids.len()
            || player_state
                .pending_track
                .as_ref()
                .is_some_and(|track| !self.expected_video_ids.contains(&track.video_id))
        {
            return false;
        }
        // Shuffle changes order, while repeated playlist occurrences must still count separately.
        let mut remaining_occurrences = std::collections::HashMap::<&str, usize>::new();
        for video_id in &self.expected_video_ids {
            *remaining_occurrences.entry(video_id.as_str()).or_default() += 1;
        }
        for track in &player_state.queue {
            let Some(remaining_count) = remaining_occurrences.get_mut(track.video_id.as_str()) else {
                return false;
            };
            if *remaining_count == 0 {
                return false;
            }
            *remaining_count -= 1;
        }
        true
    }
}

impl<P: PlayerControl> PlayerControl for Playback<P> {
    fn state(&self) -> watch::Receiver<PlayerState> {
        self.inner.visible_state.subscribe()
    }

    fn snapshot(&self) -> PlayerState {
        self.inner.visible_state.borrow().clone()
    }

    async fn start(&self) -> Result<()> {
        self.inner.player.start().await
    }

    async fn close(&self) -> Result<()> {
        self.stop_producer();
        self.inner.player.close().await
    }

    async fn retry_playback(&self) -> Result<()> {
        let _ = self.reconcile_clear().await;
        self.inner.player.retry_playback().await?;
        self.reconcile_clear().await
    }

    async fn reconcile_queue(&self) -> Result<PlayerState> {
        let reconciled_state = self.inner.player.reconcile_queue().await?;
        Ok(self.observe(reconciled_state))
    }

    async fn play(&self, tracks: Vec<Track>, selected_track_index: usize) -> Result<()> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        self.stop_producer();
        self.inner
            .coordination
            .lock()
            .expect("playback coordination")
            .bootstrap = Bootstrap::Live;
        self.inner.player.play(tracks, selected_track_index).await
    }

    async fn play_from(
        &self,
        tracks: Vec<Track>,
        selected_track_index: usize,
        start_seconds: f64,
    ) -> Result<()> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        self.stop_producer();
        self.inner
            .coordination
            .lock()
            .expect("playback coordination")
            .bootstrap = Bootstrap::Live;
        self.inner
            .player
            .play_from(tracks, selected_track_index, start_seconds)
            .await
    }

    async fn enqueue(&self, tracks: Vec<Track>, play_next: bool) -> Result<()> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        self.stop_producer();
        self.inner.player.enqueue(tracks, play_next).await
    }

    async fn replace_upcoming(&self, tracks: Vec<Track>) -> Result<()> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        self.stop_producer();
        self.inner.player.replace_upcoming(tracks).await
    }

    async fn jump(&self, queue_index: usize) -> Result<()> {
        self.inner.player.jump(queue_index).await
    }

    async fn remove(&self, queue_index: usize) -> Result<()> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        self.stop_producer();
        self.inner.player.remove(queue_index).await
    }

    async fn remove_entry(
        &self,
        queue_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
    ) -> Result<QueueEdit> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        let visible_state = self.snapshot();
        if visible_state.queue_revision != queue_revision
            || visible_state
                .queue_entries
                .iter()
                .all(|queue_entry| queue_entry.id() != queue_entry_id)
        {
            return Ok(QueueEdit::Stale);
        }
        self.stop_producer();
        let mut player_state_updates = self.inner.player.state();
        let remove_result = self
            .inner
            .player
            .remove_entry(queue_entry_id, queue_revision)
            .await;
        let observation_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let observed_state = player_state_updates.borrow().clone();
            if observed_state.queue_revision != queue_revision
                && observed_state
                    .queue_entries
                    .iter()
                    .all(|queue_entry| queue_entry.id() != queue_entry_id)
            {
                return Ok(QueueEdit::Applied);
            }
            if observed_state.queue_revision != queue_revision
                && observed_state
                    .queue_entries
                    .iter()
                    .any(|queue_entry| queue_entry.id() == queue_entry_id)
            {
                return match remove_result {
                    Ok(QueueEdit::Stale) => Ok(QueueEdit::Stale),
                    Ok(QueueEdit::Applied) => Err(redwax_player::Error::Mpv(
                        "mpv did not remove the selected queue entry".to_string(),
                    )),
                    Err(error) => Err(error),
                };
            }
            let remaining_wait = observation_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_wait.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining_wait, player_state_updates.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        let reconciliation_result = self.inner.player.reconcile_queue().await;
        match (remove_result, reconciliation_result) {
            (_, Ok(reconciled_state))
                if reconciled_state
                    .queue_entries
                    .iter()
                    .all(|queue_entry| queue_entry.id() != queue_entry_id) =>
            {
                Ok(QueueEdit::Applied)
            }
            (Ok(QueueEdit::Stale), _) => Ok(QueueEdit::Stale),
            (Err(error), _) => Err(redwax_player::Error::Mpv(format!(
                "queue removal outcome is unknown after {error}. Use Retry playback to reconcile"
            ))),
            _ => Err(redwax_player::Error::Mpv(
                "queue removal outcome is unknown. Use Retry playback to reconcile".to_string(),
            )),
        }
    }

    async fn move_entry(
        &self,
        queue_entry_id: QueueEntryId,
        queue_revision: QueueRevision,
        destination_index: usize,
    ) -> Result<QueueEdit> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        let state_before_move = self.inner.player.snapshot();
        let Some(source_queue_index) = state_before_move
            .queue_entries
            .iter()
            .position(|queue_entry| queue_entry.id() == queue_entry_id)
            .filter(|_| state_before_move.queue_revision == queue_revision)
        else {
            return Ok(QueueEdit::Stale);
        };
        let destination_index = destination_index.min(state_before_move.queue.len().saturating_sub(1));
        if source_queue_index == destination_index {
            return Ok(QueueEdit::Applied);
        }
        self.stop_producer();
        let mut player_state_updates = self.inner.player.state();
        let move_result = self
            .inner
            .player
            .move_entry(queue_entry_id, queue_revision, destination_index)
            .await;
        let observation_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let observed_state = player_state_updates.borrow().clone();
            if observed_state.queue_revision != queue_revision {
                let observed_queue_index = observed_state
                    .queue_entries
                    .iter()
                    .position(|queue_entry| queue_entry.id() == queue_entry_id);
                return match (move_result, observed_queue_index) {
                    (_, Some(observed_queue_index)) if observed_queue_index == destination_index => {
                        Ok(QueueEdit::Applied)
                    }
                    (Ok(QueueEdit::Stale), _) => Ok(QueueEdit::Stale),
                    (Err(error), _) => Err(error),
                    _ => Err(redwax_player::Error::Mpv(
                        "mpv did not move the selected queue entry".to_string(),
                    )),
                };
            }
            let remaining_wait = observation_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_wait.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining_wait, player_state_updates.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        let reconciliation_result = self.inner.player.reconcile_queue().await;
        match (move_result, reconciliation_result) {
            (_, Ok(reconciled_state))
                if reconciled_state
                    .queue_entries
                    .get(destination_index)
                    .is_some_and(|queue_entry| queue_entry.id() == queue_entry_id) =>
            {
                Ok(QueueEdit::Applied)
            }
            (Ok(QueueEdit::Stale), _) => Ok(QueueEdit::Stale),
            (Err(error), _) => Err(redwax_player::Error::Mpv(format!(
                "queue move outcome is unknown after {error}. Use Retry playback to reconcile"
            ))),
            _ => Err(redwax_player::Error::Mpv(
                "queue move outcome is unknown. Use Retry playback to reconcile".to_string(),
            )),
        }
    }

    async fn clear(&self) -> Result<()> {
        let _edit_guard = self.inner.queue_edits.lock().await;
        self.stop_producer();
        self.inner.player.clear().await
    }

    async fn toggle_pause(&self) -> Result<()> {
        if self.resume_restored().await? {
            return Ok(());
        }
        self.inner.player.toggle_pause().await
    }

    async fn next(&self) -> Result<()> {
        self.inner.player.next().await
    }

    async fn previous(&self) -> Result<()> {
        self.inner.player.previous().await
    }

    async fn seek(&self, delta_seconds: f64) -> Result<()> {
        self.inner.player.seek(delta_seconds).await
    }

    async fn seek_to(&self, target_seconds: f64) -> Result<()> {
        self.inner.player.seek_to(target_seconds).await
    }

    async fn adjust_volume(&self, volume_delta: i16) -> Result<()> {
        self.inner.player.adjust_volume(volume_delta).await
    }

    async fn set_volume(&self, volume_percent: u8) -> Result<()> {
        self.inner.player.set_volume(volume_percent).await
    }

    async fn toggle_mute(&self) -> Result<()> {
        self.inner.player.toggle_mute().await
    }

    async fn set_audio_filter(&self, audio_filter_graph: Option<&str>) -> Result<()> {
        self.inner.player.set_audio_filter(audio_filter_graph).await
    }

    async fn set_repeat(&self, repeat_mode: Repeat) -> Result<()> {
        self.inner.player.set_repeat(repeat_mode).await
    }

    async fn cycle_repeat(&self) -> Result<Repeat> {
        self.inner.player.cycle_repeat().await
    }

    async fn set_shuffle(&self, shuffle_enabled: bool) -> Result<()> {
        self.inner.player.set_shuffle(shuffle_enabled).await
    }

    async fn toggle_shuffle(&self) -> Result<bool> {
        self.inner.player.toggle_shuffle().await
    }

    fn prefetch(&self, tracks: Vec<Track>) {
        self.inner.player.prefetch(tracks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(video_id: &str) -> Track {
        Track {
            video_id: video_id.to_string(),
            title: video_id.to_string(),
            artists: Vec::new(),
            album: None,
            album_browse_id: None,
            duration_seconds: None,
            thumbnail_url: None,
            liked: false,
            playlist_entry: None,
        }
    }

    fn unresolved(state_before_clear: PlayerState) -> Coordination {
        Coordination {
            bootstrap: Bootstrap::Live,
            recovery_state: None,
            producer: Producer::Quiesced(Box::new(Producer::Radio)),
            radio: Radio::default(),
            next_edit_id: 0,
            active_clear_id: None,
            page_tail_generation: 0,
            unresolved_clear: Some(PendingClear {
                before_clear: state_before_clear,
                retained_entry: None,
            }),
        }
    }

    #[test]
    fn uncertain_clear_waits_for_authoritative_queue_evidence() {
        let state_before_clear = PlayerState {
            queue: vec![track("one"), track("two")],
            queue_revision: QueueRevision::synthetic(1),
            ..PlayerState::default()
        };
        let mut coordination = unresolved(state_before_clear.clone());

        reconcile_pending_clear(&mut coordination, &state_before_clear, false);

        assert!(coordination.unresolved_clear.is_some());
        assert!(matches!(coordination.producer, Producer::Quiesced(_)));

        reconcile_pending_clear(&mut coordination, &state_before_clear, true);

        assert!(coordination.unresolved_clear.is_none());
        assert!(matches!(coordination.producer, Producer::Radio));
    }

    #[test]
    fn observed_clear_ends_the_quiesced_producer() {
        let state_before_clear = PlayerState {
            queue: vec![track("one"), track("two")],
            queue_revision: QueueRevision::synthetic(1),
            ..PlayerState::default()
        };
        let mut coordination = unresolved(state_before_clear);

        reconcile_pending_clear(&mut coordination, &PlayerState::default(), false);

        assert!(coordination.unresolved_clear.is_none());
        assert!(matches!(coordination.producer, Producer::None));
    }

    fn queued(video_ids: &[&str], first_entry_id: u64, revision: u64) -> PlayerState {
        let queue: Vec<_> = video_ids.iter().map(|video_id| track(video_id)).collect();
        let queue_entries = queue
            .iter()
            .enumerate()
            .map(|(index, track)| {
                redwax_player::QueueEntry::synthetic(first_entry_id + index as u64, track.clone())
            })
            .collect();
        PlayerState {
            queue,
            queue_entries,
            queue_revision: QueueRevision::synthetic(revision),
            ..PlayerState::default()
        }
    }

    fn applying_tail(video_ids: &[&str]) -> PageTail {
        PageTail {
            page_key: "playlist:synthetic".to_owned(),
            application_generation: 1,
            expected_video_ids: video_ids.iter().map(|video_id| (*video_id).to_owned()).collect(),
            observed_entry_ids: None,
            previous_queue_revision: None,
            phase: PageTailPhase::Applying {
                confirmed_queue: None,
            },
        }
    }

    #[test]
    fn page_tail_waits_for_replacement_and_every_intermediate_append() {
        let mut tail = applying_tail(&["one", "two", "three"]);
        assert!(tail.observe(&queued(&["old"], 10, 1), 0));
        assert!(tail.observe(&PlayerState::default(), 1));
        assert!(tail.observe(&queued(&["one"], 20, 2), 2));
        tail.acknowledge(&queued(&["one"], 20, 2), 3);
        assert!(tail.observe(&queued(&["one", "two"], 20, 3), 4));
        let complete = queued(&["one", "two", "three"], 20, 4);
        assert!(tail.observe(&complete, 5));
        assert!(matches!(tail.phase, PageTailPhase::Ready));

        tail.expected_video_ids
            .extend(["four".to_owned(), "five".to_owned()]);
        tail.phase = PageTailPhase::Applying {
            confirmed_queue: None,
        };
        assert!(tail.observe(&queued(&["one", "two", "three", "four"], 20, 5), 6));
        tail.acknowledge(&queued(&["one", "two", "three", "four"], 20, 5), 7);
        assert!(tail.observe(&queued(&["one", "two", "three", "four"], 20, 5), 8));
        let appended = queued(&["one", "two", "three", "four", "five"], 20, 6);
        assert!(tail.observe(&appended, 9));
        assert!(matches!(tail.phase, PageTailPhase::Ready));
        assert!(!tail.observe(&queued(&["one", "two", "three", "four", "five"], 30, 7), 10));
    }

    #[test]
    fn page_tail_acknowledgement_waits_for_buffered_newer_queue_events() {
        let mut tail = applying_tail(&["one", "two"]);
        let complete = queued(&["one", "two"], 10, 2);
        assert!(tail.observe(&complete, 0));
        let mut newer = complete.clone();
        newer.queue_revision = QueueRevision::synthetic(3);
        tail.acknowledge(&newer, 1);
        assert!(matches!(tail.phase, PageTailPhase::AwaitingQueue { .. }));
        assert!(tail.observe(&queued(&["one"], 10, 1), 2));
        assert!(tail.observe(&newer, 3));
        assert!(matches!(tail.phase, PageTailPhase::Ready));
    }

    #[test]
    fn page_tail_accepts_a_complete_queue_observed_before_acknowledgement() {
        let mut tail = applying_tail(&["one", "one", "two"]);
        let complete = queued(&["two", "one", "one"], 10, 1);
        assert!(tail.observe(&complete, 0));
        tail.acknowledge(&complete, 1);
        assert!(matches!(tail.phase, PageTailPhase::Ready));
        assert!(!tail.observe(&queued(&["two", "two", "one"], 10, 2), 2));
    }

    #[test]
    fn page_tail_confirmation_expires_instead_of_owning_an_unconfirmed_queue() {
        let mut tail = applying_tail(&["one", "two"]);
        tail.acknowledge(&queued(&["old"], 10, 1), 10);
        assert!(tail.observe(&queued(&["old"], 10, 1), 5_009));
        assert!(!tail.observe(&queued(&["old"], 10, 1), 5_010));
    }

    #[test]
    fn a_replacement_cannot_confirm_the_previous_queue_with_the_same_tracks() {
        let mut tail = applying_tail(&["one", "two"]);
        tail.previous_queue_revision = Some(QueueRevision::synthetic(1));
        let previous = queued(&["two", "one"], 10, 1);
        assert!(tail.observe(&previous, 0));
        tail.acknowledge(&previous, 1);
        assert!(matches!(tail.phase, PageTailPhase::AwaitingQueue { .. }));
        let replaced = queued(&["one", "two"], 20, 2);
        assert!(tail.observe(&replaced, 2));
        assert!(matches!(tail.phase, PageTailPhase::Ready));
    }

    #[test]
    fn shuffle_preserves_page_tail_ownership_through_a_later_append() {
        let mut tail = applying_tail(&["one", "two", "three"]);
        let mut reordered = queued(&["one", "two", "three"], 10, 1);
        assert!(tail.observe(&reordered, 0));
        tail.acknowledge(&reordered, 1);
        reordered.queue.reverse();
        reordered.queue_entries.reverse();
        reordered.queue_revision = QueueRevision::synthetic(2);
        assert!(tail.observe(&reordered, 2));

        tail.expected_video_ids.push("four".to_owned());
        tail.phase = PageTailPhase::Applying {
            confirmed_queue: None,
        };
        let new_track = track("four");
        reordered.queue.push(new_track.clone());
        reordered
            .queue_entries
            .push(redwax_player::QueueEntry::synthetic(20, new_track));
        reordered.queue_revision = QueueRevision::synthetic(3);
        assert!(tail.observe(&reordered, 3));
        tail.acknowledge(&reordered, 4);
        assert!(tail.observe(&reordered, 5));
        reordered.queue_entries = queued(&["three", "two", "one", "four"], 30, 4).queue_entries;
        assert!(!tail.observe(&reordered, 6));
    }
}
