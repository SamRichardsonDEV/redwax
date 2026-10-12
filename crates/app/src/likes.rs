use std::collections::HashMap;

use redwax_ytm::Track;

#[derive(Debug, Clone)]
pub struct LikeRequest {
    revision: u64,
    pub(crate) track: Track,
    pub(crate) liked: bool,
}

struct LikeIntent {
    confirmed_liked: bool,
    desired_request: LikeRequest,
    running_request: LikeRequest,
}

#[derive(Default)]
pub(crate) struct Likes {
    revision_counter: u64,
    intents_by_video_id: HashMap<String, LikeIntent>,
}

pub(crate) enum Completion {
    Obsolete,
    Next(LikeRequest),
    Settled {
        track: Track,
        confirmed_liked: bool,
        error: Option<String>,
    },
}

impl Likes {
    /// A remote mutation can already be accepted when cancellation reaches it.
    /// Serialize each track and reconcile only the newest desired value.
    pub(crate) fn desire(&mut self, track: Track, liked: bool, confirmed_liked: bool) -> Option<LikeRequest> {
        self.revision_counter = self
            .revision_counter
            .checked_add(1)
            .expect("like revisions cannot wrap during one application lifetime");
        let desired_request = LikeRequest {
            revision: self.revision_counter,
            track,
            liked,
        };
        if let Some(track_intent) = self.intents_by_video_id.get_mut(&desired_request.track.video_id) {
            track_intent.desired_request = desired_request;
            return None;
        }
        self.intents_by_video_id.insert(
            desired_request.track.video_id.clone(),
            LikeIntent {
                confirmed_liked,
                desired_request: desired_request.clone(),
                running_request: desired_request.clone(),
            },
        );
        Some(desired_request)
    }

    pub(crate) fn complete(&mut self, completed_request: LikeRequest, error: Option<String>) -> Completion {
        let Some(track_intent) = self
            .intents_by_video_id
            .get_mut(&completed_request.track.video_id)
        else {
            return Completion::Obsolete;
        };
        if track_intent.running_request.revision != completed_request.revision {
            return Completion::Obsolete;
        }
        if error.is_none() {
            track_intent.confirmed_liked = completed_request.liked;
        }
        if track_intent.desired_request.revision != completed_request.revision {
            track_intent.running_request = track_intent.desired_request.clone();
            return Completion::Next(track_intent.running_request.clone());
        }
        let completed_intent = self
            .intents_by_video_id
            .remove(&completed_request.track.video_id)
            .expect("the accepted mutation owns this intent");
        Completion::Settled {
            track: completed_request.track,
            confirmed_liked: completed_intent.confirmed_liked,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track {
            video_id: "synthetic-like".to_owned(),
            title: "Synthetic".to_owned(),
            artists: Vec::new(),
            album: None,
            album_browse_id: None,
            duration_seconds: None,
            thumbnail_url: None,
            liked: false,
            playlist_entry: None,
        }
    }

    #[test]
    fn an_old_failure_cannot_undo_a_newer_intent() {
        let mut likes = Likes::default();
        let first_request = likes
            .desire(track(), true, false)
            .expect("the first mutation starts");
        assert!(likes.desire(track(), false, false).is_none());
        let Completion::Next(latest_request) =
            likes.complete(first_request.clone(), Some("old failure".to_owned()))
        else {
            panic!("the newest intent must be sent after the old mutation finishes");
        };
        assert!(!latest_request.liked);
        assert!(matches!(
            likes.complete(first_request, None),
            Completion::Obsolete
        ));
        assert!(matches!(
            likes.complete(latest_request, None),
            Completion::Settled {
                confirmed_liked: false,
                error: None,
                ..
            }
        ));
    }

    #[test]
    fn a_current_failure_restores_the_last_confirmed_value() {
        let mut likes = Likes::default();
        let first_request = likes
            .desire(track(), true, false)
            .expect("the first mutation starts");
        likes.desire(track(), false, false);
        let Completion::Next(latest_request) = likes.complete(first_request, None) else {
            panic!("a later intent is pending")
        };
        assert!(matches!(
            likes.complete(latest_request, Some("rejected".to_owned())),
            Completion::Settled {
                confirmed_liked: true,
                error: Some(_),
                ..
            }
        ));
    }
}
