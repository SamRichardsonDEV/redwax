use std::collections::HashSet;

use redwax_player::{PlayerState, QueueEntryId};
use redwax_ytm::{RadioContinuation, Track};
use tokio::task::AbortHandle;

const REFILL_AHEAD: usize = 5;
const RETRY_MILLIS: u64 = 30_000;
const APPLY_TIMEOUT_MILLIS: u64 = 5_000;

#[derive(Default)]
pub(crate) struct Radio {
    station_generation: u64,
    active_station: Option<Station>,
}

struct Station {
    seed_track: Track,
    seen_video_ids: HashSet<String>,
    observed_entry_ids: Option<Vec<QueueEntryId>>,
    continuation: Option<RadioContinuation>,
    phase: Phase,
    task_handle: Option<AbortHandle>,
}

enum Phase {
    Starting,
    Applying,
    AwaitingQueue {
        expected_video_ids: HashSet<String>,
        deadline_millis: u64,
    },
    Ready(u64),
    Fetching,
    Appending {
        appended_video_ids: Vec<String>,
        continuation: Option<RadioContinuation>,
    },
}

impl Station {
    fn owns(&mut self, player_state: &PlayerState) -> bool {
        let queue_is_recognized = !player_state.queue.is_empty()
            && player_state
                .queue
                .iter()
                .all(|track| self.seen_video_ids.contains(&track.video_id))
            && player_state
                .pending_track
                .as_ref()
                .is_none_or(|track| self.seen_video_ids.contains(&track.video_id));
        if !queue_is_recognized || player_state.queue_entries.len() != player_state.queue.len() {
            return queue_is_recognized;
        }
        let queue_entry_ids: Vec<_> = player_state
            .queue_entries
            .iter()
            .map(|queue_entry| queue_entry.id())
            .collect();
        if self
            .observed_entry_ids
            .as_ref()
            .is_some_and(|previously_observed_entry_ids| {
                let current_entry_ids: HashSet<_> = queue_entry_ids.iter().collect();
                previously_observed_entry_ids
                    .iter()
                    .any(|entry_id| !current_entry_ids.contains(entry_id))
            })
        {
            return false;
        }
        self.observed_entry_ids = Some(queue_entry_ids);
        true
    }
}

impl Drop for Station {
    fn drop(&mut self) {
        if let Some(task_handle) = self.task_handle.take() {
            task_handle.abort();
        }
    }
}

impl Radio {
    pub fn begin(&mut self, seed_track: Track) -> u64 {
        self.stop();
        self.active_station = Some(Station {
            seed_track,
            seen_video_ids: HashSet::new(),
            observed_entry_ids: None,
            continuation: None,
            phase: Phase::Starting,
            task_handle: None,
        });
        self.station_generation
    }

    pub fn stop(&mut self) {
        self.station_generation = self.station_generation.wrapping_add(1);
        self.active_station = None;
    }

    pub fn enabled(&self) -> bool {
        self.active_station
            .as_ref()
            .is_some_and(|station| !matches!(station.phase, Phase::Starting | Phase::Applying))
    }

    pub fn accepts(&self, station_generation: u64) -> bool {
        self.station_generation == station_generation && self.active_station.is_some()
    }

    pub fn hold(&mut self, station_generation: u64, task_handle: AbortHandle) {
        if self.accepts(station_generation) {
            let station = self
                .active_station
                .as_mut()
                .expect("the generation owns a station");
            if let Some(previous_task_handle) = station.task_handle.replace(task_handle) {
                previous_task_handle.abort();
            }
        } else {
            task_handle.abort();
        }
    }

    pub fn prepare(
        &mut self,
        station_generation: u64,
        fetched_tracks: Vec<Track>,
        continuation: Option<RadioContinuation>,
    ) -> Option<Vec<Track>> {
        if !self.accepts(station_generation) {
            return None;
        }
        let station = self.active_station.as_mut()?;
        if !matches!(station.phase, Phase::Starting) {
            return None;
        }
        let unique_tracks: Vec<_> = fetched_tracks
            .into_iter()
            .filter(|track| station.seen_video_ids.insert(track.video_id.clone()))
            .collect();
        station.continuation = continuation;
        station.phase = Phase::Applying;
        Some(unique_tracks)
    }

    pub fn started(&mut self, station_generation: u64, now_millis: u64) -> bool {
        if !self.accepts(station_generation) {
            return false;
        }
        let station = self
            .active_station
            .as_mut()
            .expect("the generation owns a station");
        if !matches!(station.phase, Phase::Applying) {
            return false;
        }
        // mpv property events can arrive after the command acknowledgement.
        station.phase = Phase::AwaitingQueue {
            expected_video_ids: station.seen_video_ids.clone(),
            deadline_millis: now_millis.saturating_add(APPLY_TIMEOUT_MILLIS),
        };
        true
    }

    pub fn refill(
        &mut self,
        player_state: &PlayerState,
        now_millis: u64,
    ) -> Option<(u64, Track, Option<RadioContinuation>)> {
        let station = self.active_station.as_mut()?;
        let station_owns_queue = station.owns(player_state);
        if let Phase::AwaitingQueue {
            expected_video_ids,
            deadline_millis,
        } = &station.phase
        {
            let queued_video_ids: HashSet<_> = player_state
                .queue
                .iter()
                .map(|track| track.video_id.as_str())
                .collect();
            if !station_owns_queue
                || expected_video_ids
                    .iter()
                    .any(|video_id| !queued_video_ids.contains(video_id.as_str()))
            {
                if now_millis >= *deadline_millis {
                    self.stop();
                }
                return None;
            }
            station.phase = Phase::Ready(0);
        }
        // Appending emits partial playlists before its own completion event.
        if matches!(
            station.phase,
            Phase::Starting | Phase::Applying | Phase::Appending { .. }
        ) {
            return None;
        }
        if !station_owns_queue {
            self.stop();
            return None;
        }
        let Phase::Ready(refill_after_millis) = station.phase else {
            return None;
        };
        let tracks_ahead = player_state.index.map_or(0, |current_index| {
            player_state
                .queue
                .len()
                .saturating_sub(current_index.saturating_add(1))
        });
        if now_millis < refill_after_millis || tracks_ahead > REFILL_AHEAD {
            return None;
        }
        station.phase = Phase::Fetching;
        Some((
            self.station_generation,
            station.seed_track.clone(),
            station.continuation.clone(),
        ))
    }

    pub fn received(
        &mut self,
        station_generation: u64,
        fetched_tracks: Vec<Track>,
        mut continuation: Option<RadioContinuation>,
        player_state: &PlayerState,
        now_millis: u64,
    ) -> Option<Vec<Track>> {
        if !self.accepts(station_generation) {
            return None;
        }
        let station = self.active_station.as_mut()?;
        if !matches!(station.phase, Phase::Fetching) {
            return None;
        }
        if !station.owns(player_state) {
            self.stop();
            return None;
        }
        let mut batch_video_ids = HashSet::new();
        let unique_tracks: Vec<_> = fetched_tracks
            .into_iter()
            .filter(|track| {
                !station.seen_video_ids.contains(&track.video_id)
                    && batch_video_ids.insert(track.video_id.clone())
            })
            .collect();
        // Repeating a cursor would request the same batch without making progress.
        if continuation.is_some() && continuation == station.continuation {
            continuation = None;
        }
        if unique_tracks.is_empty() {
            station.continuation = continuation;
            station.phase = Phase::Ready(now_millis.saturating_add(RETRY_MILLIS));
        } else {
            station.phase = Phase::Appending {
                appended_video_ids: unique_tracks.iter().map(|track| track.video_id.clone()).collect(),
                continuation,
            };
        }
        Some(unique_tracks)
    }

    pub fn appended(&mut self, station_generation: u64, now_millis: u64) -> bool {
        if !self.accepts(station_generation) {
            return false;
        }
        let station = self
            .active_station
            .as_mut()
            .expect("the generation owns a station");
        if !matches!(station.phase, Phase::Appending { .. }) {
            return false;
        }
        let Phase::Appending {
            appended_video_ids,
            continuation,
        } = std::mem::replace(&mut station.phase, Phase::Ready(0))
        else {
            return false;
        };
        let expected_video_ids: HashSet<_> = appended_video_ids.into_iter().collect();
        station.seen_video_ids.extend(expected_video_ids.iter().cloned());
        station.continuation = continuation;
        station.phase = Phase::AwaitingQueue {
            expected_video_ids,
            deadline_millis: now_millis.saturating_add(APPLY_TIMEOUT_MILLIS),
        };
        true
    }

    pub fn failed(&mut self, station_generation: u64, now_millis: u64) -> bool {
        if !self.accepts(station_generation) {
            return false;
        }
        let station = self
            .active_station
            .as_mut()
            .expect("the generation owns a station");
        if !matches!(station.phase, Phase::Fetching) {
            return false;
        }
        station.phase = Phase::Ready(now_millis.saturating_add(RETRY_MILLIS));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(index: usize) -> Track {
        redwax_player::bare_track(&format!("song-{index}"), &format!("Song {index}"))
    }

    fn playing(count: usize, index: usize) -> PlayerState {
        PlayerState {
            queue: (0..count).map(track).collect(),
            index: Some(index),
            ..PlayerState::default()
        }
    }

    fn station(state: &PlayerState) -> (Radio, u64, RadioContinuation) {
        let mut radio = Radio::default();
        let request = radio.begin(state.queue[0].clone());
        let more = RadioContinuation::new(&state.queue[0], "next-page".to_string());
        radio
            .prepare(request, state.queue.clone(), Some(more.clone()))
            .unwrap();
        assert!(radio.started(request, 0));
        (radio, request, more)
    }

    #[test]
    fn refill_waits_for_five_remaining_tracks_and_never_overlaps() {
        let mut state = playing(12, 0);
        let (mut radio, _, more) = station(&state);
        assert!(radio.refill(&state, 0).is_none());
        state.index = Some(6);
        let (_, seed, cursor) = radio.refill(&state, 1).unwrap();
        assert_eq!(seed.video_id, "song-0");
        assert_eq!(cursor, Some(more));
        assert!(radio.refill(&state, 2).is_none());
    }

    #[test]
    fn appending_deduplicates_and_advances_without_restarting_the_station() {
        let mut state = playing(12, 6);
        let (mut radio, request, _) = station(&state);
        radio.refill(&state, 0).unwrap();
        let more = RadioContinuation::new(&state.queue[0], "later-page".to_string());
        let found = radio
            .received(
                request,
                vec![track(11), track(12), track(12), track(13)],
                Some(more.clone()),
                &state,
                1,
            )
            .unwrap();
        assert_eq!(
            found
                .iter()
                .map(|track| track.video_id.as_str())
                .collect::<Vec<_>>(),
            ["song-12", "song-13"]
        );
        state.queue.extend(found);
        assert!(radio.refill(&state, 2).is_none());
        assert!(radio.appended(request, 2));
        state.index = Some(8);
        assert_eq!(radio.refill(&state, 3).unwrap().2, Some(more));
    }

    #[test]
    fn failed_fetches_keep_the_cursor_and_back_off_without_disabling_radio() {
        let state = playing(12, 6);
        let (mut radio, request, more) = station(&state);
        radio.refill(&state, 0).unwrap();
        assert!(radio.failed(request, 1));
        assert!(radio.enabled());
        assert!(radio.refill(&state, RETRY_MILLIS).is_none());
        assert_eq!(radio.refill(&state, RETRY_MILLIS + 1).unwrap().2, Some(more));
    }

    #[test]
    fn an_empty_repeating_page_is_paced_and_reseeds_the_same_station() {
        let state = playing(12, 6);
        let (mut radio, request, more) = station(&state);
        radio.refill(&state, 0).unwrap();
        assert!(
            radio
                .received(request, vec![track(11)], Some(more), &state, 1)
                .unwrap()
                .is_empty()
        );
        assert!(radio.refill(&state, RETRY_MILLIS).is_none());
        let (_, seed, cursor) = radio.refill(&state, RETRY_MILLIS + 1).unwrap();
        assert_eq!(seed.video_id, "song-0");
        assert!(cursor.is_none());
    }

    #[test]
    fn clearing_or_replacing_the_station_discards_late_batches() {
        let state = playing(12, 6);
        let (mut radio, request, _) = station(&state);
        radio.refill(&state, 0).unwrap();
        radio.stop();
        assert!(
            radio
                .received(request, vec![track(12)], None, &state, 1)
                .is_none()
        );
        let new_request = radio.begin(track(40));
        assert_ne!(new_request, request);
        assert!(radio.prepare(request, state.queue.clone(), None).is_none());
    }

    #[test]
    fn a_foreign_queue_cannot_receive_a_late_radio_batch() {
        let state = playing(12, 6);
        let (mut radio, request, _) = station(&state);
        radio.refill(&state, 0).unwrap();
        let foreign = PlayerState {
            queue: vec![track(50)],
            index: Some(0),
            ..PlayerState::default()
        };
        assert!(
            radio
                .received(request, vec![track(12)], None, &foreign, 1)
                .is_none()
        );
        assert!(!radio.enabled());
    }

    #[test]
    fn an_append_acknowledgement_waits_for_its_new_tracks_to_arrive() {
        let mut state = playing(12, 6);
        let (mut radio, request, _) = station(&state);
        radio.refill(&state, 0).unwrap();
        let more = RadioContinuation::new(&state.queue[0], "later-page".to_string());
        let found = radio
            .received(request, vec![track(12)], Some(more.clone()), &state, 1)
            .unwrap();
        assert!(radio.appended(request, 2));
        assert!(radio.refill(&state, 3).is_none());
        state.queue.extend(found);
        state.index = Some(7);
        assert_eq!(radio.refill(&state, 4).unwrap().2, Some(more));
    }

    #[test]
    fn property_events_must_confirm_the_initial_queue_before_a_refill() {
        let full = playing(12, 6);
        let (mut radio, _, _) = station(&full);
        assert!(radio.refill(&playing(2, 0), 1).is_none());
        assert!(radio.refill(&full, 2).is_some());
        let (mut radio, _, _) = station(&full);
        assert!(radio.refill(&playing(2, 0), APPLY_TIMEOUT_MILLIS).is_none());
        assert!(!radio.enabled());
    }

    #[test]
    fn shuffled_queue_identities_keep_radio_ownership_but_replacements_do_not() {
        let mut state = playing(12, 0);
        state.queue_entries = state
            .queue
            .iter()
            .enumerate()
            .map(|(index, track)| redwax_player::QueueEntry::synthetic(index as u64 + 1, track.clone()))
            .collect();
        let (mut radio, request, _) = station(&state);
        assert!(radio.refill(&state, 0).is_none());
        state.queue.reverse();
        state.queue_entries.reverse();
        state.index = Some(6);
        assert!(radio.refill(&state, 1).is_some());
        assert!(radio.failed(request, 2));
        state.queue_entries = state
            .queue
            .iter()
            .enumerate()
            .map(|(index, track)| redwax_player::QueueEntry::synthetic(index as u64 + 100, track.clone()))
            .collect();
        assert!(radio.refill(&state, RETRY_MILLIS + 2).is_none());
        assert!(!radio.enabled());
    }
}
