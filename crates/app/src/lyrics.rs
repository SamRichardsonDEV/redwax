//! What the lyrics tab is showing, and whether it is still following the song.
//!
//! The follow is the one rule worth hiding. A panel that scrolls itself to the
//! playing line is right until the user scrolls it themselves, and then it is
//! wrong until they stop. That is expressed here as a timestamp and a pause,
//! and nowhere else: callers ask `follows` and say `scrolled`.

use redwax_ytm::{Lyrics, Track};

use crate::request::{RequestId, RequestSlot};

/// How long the panel stays where the user put it before it follows the song
/// again. Long enough to read a verse, short enough that a panel left alone
/// catches up on its own.
pub const FOLLOW_PAUSE_MILLIS: u64 = 5_000;

#[derive(Debug, Default)]
pub struct LyricsPane {
    lyrics_request: RequestSlot<String>,
    /// The track the shown lyrics belong to. `None` while a fetch is out, so
    /// an answer for a track the user has already left is still recognised.
    shown_video_id: Option<String>,
    lyrics: Option<Lyrics>,
    /// The title being fetched, which is what the panel says while it waits.
    loading_track_title: Option<String>,
    scroll_offset: u16,
    /// When the user last scrolled. `None` means the panel follows the song.
    last_manual_scroll_millis: Option<u64>,
    viewport_rows: u16,
}

impl LyricsPane {
    pub fn text(&self) -> Option<&Lyrics> {
        self.lyrics.as_ref()
    }

    pub fn loading(&self) -> Option<&str> {
        self.loading_track_title.as_deref()
    }

    pub fn scroll(&self) -> u16 {
        self.scroll_offset
    }

    /// Where the draw left the panel after clamping it to the lines it has.
    pub fn scrolled_to(&mut self, scroll_offset: u16) {
        self.scroll_offset = scroll_offset;
    }

    pub fn measured(&mut self, visible_rows: u16) {
        self.viewport_rows = visible_rows;
    }

    pub fn scrolled_page(&mut self, forward: bool, now_millis: u64) {
        let page_rows = self.viewport_rows.max(1) as isize;
        self.scrolled(if forward { page_rows } else { -page_rows }, now_millis);
    }

    pub fn follows(&self) -> bool {
        self.last_manual_scroll_millis.is_none()
    }

    /// True when this track's lyrics are neither shown nor on their way, which
    /// is the whole of deciding to fetch.
    pub fn wants(&self, track: &Track) -> bool {
        self.shown_video_id.as_deref() != Some(track.video_id.as_str())
            && !self
                .lyrics_request
                .current()
                .is_some_and(|(_, requested_video_id)| requested_video_id == &track.video_id)
    }

    /// A fetch has started. The shown track is cleared, because what is on
    /// screen no longer belongs to what is playing.
    pub fn fetching(&mut self, track: &Track) -> RequestId {
        self.loading_track_title = Some(track.title.clone());
        self.shown_video_id = None;
        self.lyrics = None;
        self.lyrics_request.begin(track.video_id.clone())
    }

    pub(crate) fn hold(&mut self, request_id: RequestId, abort_handle: tokio::task::AbortHandle) {
        self.lyrics_request.hold(request_id, abort_handle);
    }

    pub fn clear(&mut self) {
        self.lyrics_request.cancel();
        self.loading_track_title = None;
        self.shown_video_id = None;
        self.lyrics = None;
        self.scroll_offset = 0;
        self.last_manual_scroll_millis = None;
        self.viewport_rows = 0;
    }

    /// The fetch landed. The panel goes back to following, at the top, because
    /// a new song has no scroll position worth keeping.
    pub fn landed(
        &mut self,
        request_id: RequestId,
        video_id: String,
        fetched_lyrics: Option<Lyrics>,
    ) -> bool {
        if self.lyrics_request.get(request_id) != Some(&video_id)
            || self.lyrics_request.complete(request_id).is_none()
        {
            return false;
        }
        self.loading_track_title = None;
        self.shown_video_id = Some(video_id);
        self.lyrics = fetched_lyrics;
        self.scroll_offset = 0;
        self.last_manual_scroll_millis = None;
        true
    }

    /// The user moved the panel, which stops the follow for the pause.
    pub fn scrolled(&mut self, row_delta: isize, now_millis: u64) {
        self.scroll_offset = match row_delta < 0 {
            true => self.scroll_offset.saturating_sub(row_delta.unsigned_abs() as u16),
            false => self.scroll_offset.saturating_add(row_delta as u16),
        };
        self.last_manual_scroll_millis = Some(now_millis);
    }

    /// The user jumped to one end. `u16::MAX / 2` is past any real line count,
    /// and the draw clamps it back to the last line.
    pub fn scrolled_to_edge(&mut self, last_edge: bool, now_millis: u64) {
        self.scroll_offset = match last_edge {
            true => u16::MAX / 2,
            false => 0,
        };
        self.last_manual_scroll_millis = Some(now_millis);
    }

    /// Give the follow back once the pause has run out. True when that just
    /// happened, so the caller knows the panel is worth repainting.
    pub fn resume_follow(&mut self, now_millis: u64) -> bool {
        let Some(manual_scroll_millis) = self.last_manual_scroll_millis else {
            return false;
        };
        if now_millis.saturating_sub(manual_scroll_millis) < FOLLOW_PAUSE_MILLIS {
            return false;
        }
        self.last_manual_scroll_millis = None;
        true
    }

    /// Test seam: put the panel back to following at once.
    pub fn follow_now(&mut self) {
        self.last_manual_scroll_millis = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track {
            video_id: id.to_string(),
            title: "Alpine Glow".to_string(),
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
    fn a_fresh_pane_wants_anything_and_follows() {
        let pane = LyricsPane::default();
        assert!(pane.wants(&track("v1")));
        assert!(pane.follows());
    }

    #[test]
    fn a_landed_fetch_is_not_wanted_again() {
        let mut pane = LyricsPane::default();
        let request = pane.fetching(&track("v1"));
        assert_eq!(pane.loading(), Some("Alpine Glow"));
        assert!(
            !pane.wants(&track("v1")),
            "the pending request already owns this track"
        );

        pane.landed(request, "v1".to_string(), Some(Lyrics::Plain("la".to_string())));
        assert!(!pane.wants(&track("v1")));
        assert!(pane.wants(&track("v2")), "another track is a different fetch");
        assert_eq!(pane.loading(), None);
    }

    #[test]
    fn scrolling_stops_the_follow_until_the_pause_runs_out() {
        let mut pane = LyricsPane::default();
        pane.scrolled(3, 1_000);
        assert_eq!(pane.scroll(), 3);
        assert!(!pane.follows());

        assert!(
            !pane.resume_follow(1_000 + FOLLOW_PAUSE_MILLIS - 1),
            "gave up early"
        );
        assert!(!pane.follows());
        assert!(pane.resume_follow(1_000 + FOLLOW_PAUSE_MILLIS));
        assert!(pane.follows());
        // The pause is spent, so asking again is not another change.
        assert!(!pane.resume_follow(9_999_999));
    }

    #[test]
    fn a_new_song_takes_the_pane_back_to_the_top_and_the_follow() {
        let mut pane = LyricsPane::default();
        pane.scrolled(40, 1_000);
        let request = pane.fetching(&track("v2"));
        pane.landed(request, "v2".to_string(), None);
        assert_eq!(pane.scroll(), 0);
        assert!(pane.follows());
    }

    #[test]
    fn scrolling_up_from_the_top_stops_at_the_top() {
        let mut pane = LyricsPane::default();
        pane.scrolled(-5, 1_000);
        assert_eq!(pane.scroll(), 0);
    }

    #[test]
    fn obsolete_and_repeated_track_results_cannot_replace_the_current_lyrics() {
        let mut pane = LyricsPane::default();
        let old = pane.fetching(&track("v1"));
        let newer = pane.fetching(&track("v2"));
        assert!(!pane.landed(old, "v1".to_owned(), Some(Lyrics::Plain("old".to_owned()))));
        assert_eq!(pane.loading(), Some("Alpine Glow"));
        assert!(pane.landed(newer, "v2".to_owned(), Some(Lyrics::Plain("new".to_owned()))));
        let repeat = pane.fetching(&track("v1"));
        assert!(!pane.landed(old, "v1".to_owned(), None));
        assert!(pane.landed(repeat, "v1".to_owned(), None));
        pane.clear();
        assert!(!pane.landed(repeat, "v1".to_owned(), None));
    }
}
