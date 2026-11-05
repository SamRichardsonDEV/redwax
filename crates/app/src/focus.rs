//! Full plays become serialized playlist edits with membership proven before each write.

use std::collections::{HashSet, VecDeque};

use redwax_player::PlayerState;
use redwax_ytm::{MusicSource, Playlist, PlaylistAdd, Track};
use tokio::task::AbortHandle;

use crate::request::{RequestId, RequestSlot};

#[derive(Default)]
pub(crate) struct FocusMode {
    destination: Option<Playlist>,
    completed_sequence: u64,
    pending: VecDeque<Track>,
    known_members: HashSet<(String, String)>,
    pending_video_ids: HashSet<String>,
    request: RequestSlot<Track>,
    status: Option<String>,
}

impl FocusMode {
    pub fn active(&self) -> bool {
        self.destination.is_some()
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn start(&mut self, destination: Playlist, state: &PlayerState) {
        self.stop();
        self.destination = Some(destination);
        self.completed_sequence = state.playback_sequence;
        self.status = Some("Waiting for a full play".to_string());
    }

    pub fn stop(&mut self) {
        if self.request.current().is_some() {
            self.status = Some("Pending save cancelled".to_string());
        }
        self.destination = None;
        self.pending.clear();
        self.pending_video_ids.clear();
        self.request.cancel();
    }

    pub fn observe(&mut self, state: &PlayerState) {
        let Some(destination) = &self.destination else {
            return;
        };
        for (sequence, track) in &state.completed_tracks {
            if *sequence <= self.completed_sequence {
                continue;
            }
            self.completed_sequence = *sequence;
            if !self
                .known_members
                .contains(&(destination.playlist_id.clone(), track.video_id.clone()))
                && self.pending_video_ids.insert(track.video_id.clone())
            {
                self.pending.push_back(track.clone());
            }
        }
    }

    pub fn next_request(&mut self) -> Option<(RequestId, Playlist, Track)> {
        if self.request.current().is_some() {
            return None;
        }
        let destination = self.destination.as_ref()?.clone();
        let track = self.pending.pop_front()?;
        self.status = Some(format!("Checking playlist for: {}", track.title));
        let request = self.request.begin(track.clone());
        Some((request, destination, track))
    }

    pub fn hold(&mut self, request: RequestId, task: AbortHandle) {
        self.request.hold(request, task);
    }

    pub fn complete(&mut self, request: RequestId, result: &Result<PlaylistAdd, String>) -> Option<Track> {
        let track = self.request.complete(request)?.clone();
        self.request.cancel();
        self.pending_video_ids.remove(&track.video_id);
        self.status = Some(match result {
            Ok(PlaylistAdd::Added) => format!("Saved: {}", track.title),
            Ok(PlaylistAdd::AlreadyPresent) => format!("Already in playlist: {}", track.title),
            Err(error) => format!("Could not save {}: {error}", track.title),
        });
        if result.is_ok()
            && let Some(destination) = &self.destination
        {
            self.known_members
                .insert((destination.playlist_id.clone(), track.video_id.clone()));
        }
        Some(track)
    }
}

pub(crate) async fn append(
    source: &impl MusicSource,
    destination: &Playlist,
    track: &Track,
) -> Result<PlaylistAdd, String> {
    if source
        .playlist_contains(destination, &track.video_id)
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(PlaylistAdd::AlreadyPresent);
    }
    source
        .add_to_playlist(&destination.playlist_id, std::slice::from_ref(&track.video_id))
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destination() -> Playlist {
        Playlist {
            playlist_id: "PLfocus".to_string(),
            title: "Focus".to_string(),
            subtitle: None,
            thumbnail_url: None,
        }
    }

    #[test]
    fn activation_requires_a_new_full_play_and_serializes_coalesced_completions() {
        let mut focus = FocusMode::default();
        let first = redwax_player::bare_track("one", "First");
        let second = redwax_player::bare_track("two", "Second");
        let mut state = PlayerState {
            playback_sequence: 4,
            completed_tracks: VecDeque::from([(3, first.clone())]),
            ..PlayerState::default()
        };
        focus.start(destination(), &state);
        state
            .completed_tracks
            .extend([(4, first.clone()), (5, first.clone()), (6, second)]);
        focus.observe(&state);
        let (request, _, track) = focus.next_request().unwrap();
        assert_eq!(track.video_id, "one");
        assert!(focus.next_request().is_none());
        focus.observe(&state);
        assert!(focus.complete(request, &Ok(PlaylistAdd::Added)).is_some());
        let (request, _, track) = focus.next_request().unwrap();
        assert_eq!(track.video_id, "two");
        focus.complete(request, &Ok(PlaylistAdd::Added));
        assert!(focus.next_request().is_none());
    }

    #[test]
    fn stopping_rejects_stale_writes_and_allows_a_fresh_membership_check_next_time() {
        let mut focus = FocusMode::default();
        let mut state = PlayerState::default();
        let track = redwax_player::bare_track("one", "First");
        focus.start(destination(), &state);
        state.playback_sequence = 1;
        state.completed_tracks.push_back((1, track.clone()));
        focus.observe(&state);
        let (request, _, _) = focus.next_request().unwrap();
        focus.stop();
        assert!(!focus.active());
        assert!(focus.complete(request, &Ok(PlaylistAdd::Added)).is_none());
        focus.start(destination(), &state);
        state.completed_tracks.push_back((2, track));
        focus.observe(&state);
        assert!(focus.next_request().is_some());
    }

    #[test]
    fn failed_membership_checks_do_not_permanently_exclude_a_song() {
        let mut focus = FocusMode::default();
        let mut state = PlayerState::default();
        let track = redwax_player::bare_track("one", "First");
        focus.start(destination(), &state);
        state.playback_sequence = 1;
        state.completed_tracks.push_back((1, track.clone()));
        focus.observe(&state);
        let (request, _, _) = focus.next_request().unwrap();
        focus.complete(request, &Err("Playlist read failed".to_string()));
        assert!(focus.status().unwrap().contains("Playlist read failed"));
        focus.stop();
        focus.start(destination(), &state);
        state.completed_tracks.push_back((2, track));
        focus.observe(&state);
        assert!(focus.next_request().is_some());
    }
}
