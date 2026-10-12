use std::collections::HashSet;
use std::fmt;

use crate::{Continuation, Error, Item, MusicSource, Page, Result, Shelf};

// Bound request work even when a source keeps returning unique continuations.
const MAX_CONTINUATION_REQUESTS: usize = 1_000;

/// A page with no remaining continuation.
#[derive(Debug)]
pub struct CompletePage(Page);

impl CompletePage {
    pub fn as_page(&self) -> &Page {
        &self.0
    }

    pub fn into_page(self) -> Page {
        self.0
    }
}

impl TryFrom<Page> for CompletePage {
    type Error = Error;

    fn try_from(page: Page) -> Result<CompletePage> {
        if page.more.is_some() {
            return Err(Error::IncompletePage("the page still has a continuation"));
        }
        Ok(CompletePage(page))
    }
}

/// Continuation state shared by incremental and eager page loading.
pub struct PagePagination {
    next: Option<Continuation>,
    seen: HashSet<Continuation>,
}

impl PagePagination {
    pub fn new(next: Option<Continuation>) -> PagePagination {
        let seen = next.iter().cloned().collect();
        PagePagination { next, seen }
    }

    pub fn next(&self) -> Option<&Continuation> {
        self.next.as_ref()
    }

    /// Accept a response only when it proves safe progress from the pending
    /// continuation. An error leaves the current continuation pending.
    pub fn advance(&mut self, items: &[Item], next: Option<Continuation>) -> Result<()> {
        if self.next.is_none() {
            return Err(Error::IncompletePage("a continuation batch was not expected"));
        }
        // An empty parse cannot prove that the remote list ended normally.
        if items.is_empty() {
            return Err(Error::IncompletePage("a continuation returned no items"));
        }
        if let Some(candidate) = &next {
            if self.seen.contains(candidate) {
                return Err(Error::IncompletePage("YouTube repeated a page continuation"));
            }
            if self.seen.len() >= MAX_CONTINUATION_REQUESTS {
                return Err(Error::IncompletePage(
                    "page collection reached the continuation limit",
                ));
            }
        }

        if let Some(candidate) = &next {
            self.seen.insert(candidate.clone());
        }
        self.next = next;
        Ok(())
    }
}

/// A useful prefix that cannot prove the remote list was complete.
/// Its `Display` and `Debug` outputs redact source details. `Error::source`
/// retains the underlying error for callers that explicitly inspect it.
pub struct IncompletePage {
    pub page: Box<Page>,
    pub error: Error,
}

impl fmt::Display for IncompletePage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "page collection stopped: {}", error_class(&self.error))
    }
}

impl fmt::Debug for IncompletePage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IncompletePage")
            .field("title", &self.page.title)
            .field("shelves", &self.page.shelves.len())
            .field("has_continuation", &self.page.more.is_some())
            .field("error", &error_class(&self.error))
            .finish()
    }
}

impl std::error::Error for IncompletePage {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

fn error_class(error: &Error) -> &'static str {
    match error {
        Error::SignedOut => "not signed in",
        Error::Rejected => "session rejected",
        Error::Network(_) => "network request failed",
        Error::PlaylistEditRejected { .. } => "playlist edit rejected",
        Error::PlaylistNotEditable => "playlist is not editable",
        Error::Empty { .. } => "page response was empty",
        Error::IncompletePage(reason) => reason,
        Error::Other(_) => "page source failed",
    }
}

/// Follow every continuation while retaining a safe prefix on failure.
pub async fn collect_page<S: MusicSource>(
    music_source: &S,
    mut page: Page,
) -> std::result::Result<CompletePage, IncompletePage> {
    let mut pagination = PagePagination::new(page.more.clone());
    while let Some(continuation) = pagination.next().cloned() {
        let (items, next_continuation) = match music_source.continue_page(&continuation).await {
            Ok(continuation_batch) => continuation_batch,
            Err(error) => {
                return Err(IncompletePage {
                    page: Box::new(page),
                    error,
                });
            }
        };
        if let Err(error) = pagination.advance(&items, next_continuation) {
            return Err(IncompletePage {
                page: Box::new(page),
                error,
            });
        }

        if let Some(shelf) = page.shelves.last_mut() {
            shelf.items.extend(items);
        } else {
            page.shelves.push(Shelf::new("Items", items));
        }
        page.more = pagination.next().cloned();
    }

    Ok(CompletePage(page))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::{Account, Browsable, Lyrics, Track};

    use super::*;

    type Batch = Result<(Vec<Item>, Option<Continuation>)>;

    struct ScriptedSource {
        batches: Mutex<VecDeque<(Continuation, Batch)>>,
    }

    impl ScriptedSource {
        fn new(batches: impl IntoIterator<Item = (Continuation, Batch)>) -> ScriptedSource {
            ScriptedSource {
                batches: Mutex::new(batches.into_iter().collect()),
            }
        }
    }

    impl MusicSource for ScriptedSource {
        fn signed_in(&self) -> bool {
            false
        }

        async fn whoami(&self) -> Result<Option<String>> {
            Ok(None)
        }

        async fn home(&self) -> Result<Page> {
            unreachable!("pagination tests do not request home")
        }

        async fn search(&self, _: &str) -> Result<Page> {
            unreachable!("pagination tests do not search")
        }

        async fn library(&self) -> Result<Page> {
            unreachable!("pagination tests do not request the library")
        }

        async fn browse(&self, _: &Browsable) -> Result<Page> {
            unreachable!("pagination tests start with a page")
        }

        async fn continue_page(&self, continuation: &Continuation) -> Batch {
            let (expected, result) = self
                .batches
                .lock()
                .expect("the test holds no batch lock")
                .pop_front()
                .expect("the collector requested one of the scripted batches");
            assert_eq!(&expected, continuation);
            result
        }

        async fn radio(&self, _: &Track) -> Result<Vec<Track>> {
            unreachable!("pagination tests do not request radio")
        }

        async fn lyrics(&self, _: &Track) -> Result<Option<String>> {
            unreachable!("pagination tests do not request lyrics")
        }

        async fn timed_lyrics(&self, _: &Track) -> Result<Option<Lyrics>> {
            unreachable!("pagination tests do not request lyrics")
        }

        async fn set_liked(&self, _: &Track, _: bool) -> Result<()> {
            unreachable!("pagination tests do not change likes")
        }

        async fn accounts(&self) -> Result<Vec<Account>> {
            unreachable!("pagination tests do not request accounts")
        }
    }

    fn continuation(number: usize) -> Continuation {
        Continuation {
            browse_id: "VLplaylist".to_string(),
            token: format!("private-token-{number}"),
        }
    }

    fn item(video_id: &str) -> Item {
        Item::Track(Track {
            video_id: video_id.to_string(),
            title: video_id.to_string(),
            artists: Vec::new(),
            album: None,
            album_browse_id: None,
            duration_seconds: None,
            thumbnail_url: None,
            liked: false,
            playlist_entry: None,
        })
    }

    fn partial_page(items: Vec<Item>, more: Continuation) -> Page {
        Page {
            title: "Playlist".to_string(),
            shelves: vec![Shelf::new("Tracks", items)],
            subtitle: Some("kept subtitle".to_string()),
            thumbnail_url: Some("art".to_string()),
            more: Some(more),
        }
    }

    fn video_ids(page: &Page) -> Vec<&str> {
        page.tracks().map(|track| track.video_id.as_str()).collect()
    }

    #[test]
    fn only_a_page_without_a_continuation_proves_completion() {
        let complete = CompletePage::try_from(Page::new("Done", Vec::new())).expect("no continuation");
        assert_eq!(complete.as_page().title, "Done");

        let partial = partial_page(vec![item("a")], continuation(1));
        assert!(matches!(
            CompletePage::try_from(partial),
            Err(Error::IncompletePage(_))
        ));
    }

    #[test]
    fn a_repeated_initial_continuation_leaves_the_current_one_pending() {
        let first = continuation(1);
        let mut pagination = PagePagination::new(Some(first.clone()));
        let error = pagination
            .advance(&[item("a")], Some(first.clone()))
            .expect_err("the initial continuation is already seen");
        assert!(matches!(error, Error::IncompletePage(_)));
        assert_eq!(pagination.next(), Some(&first));
    }

    #[test]
    fn a_longer_continuation_cycle_leaves_the_current_one_pending() {
        let first = continuation(1);
        let second = continuation(2);
        let mut pagination = PagePagination::new(Some(first.clone()));
        pagination
            .advance(&[item("a")], Some(second.clone()))
            .expect("the second continuation is new");
        let error = pagination
            .advance(&[item("b")], Some(first))
            .expect_err("the continuation chain returned to its first token");
        assert!(matches!(error, Error::IncompletePage(_)));
        assert_eq!(pagination.next(), Some(&second));
    }

    #[test]
    fn an_empty_batch_is_incomplete_and_keeps_the_cursor() {
        for next in [None, Some(continuation(2))] {
            let first = continuation(1);
            let mut pagination = PagePagination::new(Some(first.clone()));
            let error = pagination
                .advance(&[], next)
                .expect_err("empty continuation batches prove nothing");
            assert!(matches!(error, Error::IncompletePage(_)));
            assert_eq!(pagination.next(), Some(&first));
        }
    }

    #[test]
    fn a_batch_without_a_pending_continuation_is_rejected() {
        let mut pagination = PagePagination::new(None);
        let error = pagination
            .advance(&[item("a")], None)
            .expect_err("no continuation request is pending");
        assert!(matches!(error, Error::IncompletePage(_)));
        assert!(pagination.next().is_none());
    }

    #[test]
    fn the_continuation_limit_does_not_publish_an_unrequestable_cursor() {
        let mut pagination = PagePagination::new(Some(continuation(0)));
        for number in 1..MAX_CONTINUATION_REQUESTS {
            pagination
                .advance(&[item("a")], Some(continuation(number)))
                .expect("the request is inside the limit");
        }
        let last = continuation(MAX_CONTINUATION_REQUESTS - 1);
        let error = pagination
            .advance(&[item("a")], Some(continuation(MAX_CONTINUATION_REQUESTS)))
            .expect_err("the next cursor would exceed the request limit");
        assert!(matches!(error, Error::IncompletePage(_)));
        assert_eq!(pagination.next(), Some(&last));
    }

    #[tokio::test]
    async fn complete_collection_keeps_metadata_order_and_duplicate_occurrences() {
        let first = continuation(1);
        let second = continuation(2);
        let source = ScriptedSource::new([
            (
                first.clone(),
                Ok((vec![item("b"), item("b")], Some(second.clone()))),
            ),
            (second, Ok((vec![item("c")], None))),
        ]);
        let mut page = partial_page(vec![item("a")], first);
        page.shelves.insert(0, Shelf::new("Earlier shelf", Vec::new()));
        let complete = collect_page(&source, page)
            .await
            .expect("the continuation reaches its end");
        let page = complete.as_page();
        assert_eq!(page.title, "Playlist");
        assert_eq!(page.subtitle.as_deref(), Some("kept subtitle"));
        assert_eq!(page.thumbnail_url.as_deref(), Some("art"));
        assert_eq!(page.shelves[0].title, "Earlier shelf");
        assert!(page.shelves[0].items.is_empty());
        assert_eq!(page.shelves[1].title, "Tracks");
        assert_eq!(video_ids(page), vec!["a", "b", "b", "c"]);
        assert!(page.more.is_none());
    }

    #[tokio::test]
    async fn collection_failure_keeps_the_prefix_cursor_and_safe_report() {
        let first = continuation(1);
        let second = continuation(2);
        let source = ScriptedSource::new([
            (first.clone(), Ok((vec![item("b")], Some(second.clone())))),
            (
                second.clone(),
                Err(Error::Other(format!("failed at {}", second.token))),
            ),
        ]);
        let incomplete = collect_page(&source, partial_page(vec![item("a")], first))
            .await
            .expect_err("the second request fails");
        assert_eq!(video_ids(&incomplete.page), vec!["a", "b"]);
        assert_eq!(incomplete.page.more.as_ref(), Some(&second));
        assert!(!incomplete.to_string().contains(&second.token));
        assert!(!format!("{incomplete:?}").contains(&second.token));
        let cause = std::error::Error::source(&incomplete).expect("the cause remains available");
        assert!(cause.to_string().contains(&second.token));
    }

    #[tokio::test]
    async fn an_empty_batch_keeps_the_useful_prefix_incomplete() {
        for next in [None, Some(continuation(2))] {
            let first = continuation(1);
            let source = ScriptedSource::new([(first.clone(), Ok((Vec::new(), next)))]);
            let incomplete = collect_page(&source, partial_page(vec![item("a")], first.clone()))
                .await
                .expect_err("an empty batch cannot prove completion");
            assert_eq!(video_ids(&incomplete.page), vec!["a"]);
            assert_eq!(incomplete.page.more.as_ref(), Some(&first));
        }
    }

    #[tokio::test]
    async fn collection_creates_a_shelf_when_the_prefix_has_none() {
        let first = continuation(1);
        let source = ScriptedSource::new([(first.clone(), Ok((vec![item("a")], None)))]);
        let page = Page {
            title: "Empty prefix".to_string(),
            shelves: Vec::new(),
            subtitle: None,
            thumbnail_url: None,
            more: Some(first),
        };
        let complete = collect_page(&source, page)
            .await
            .expect("the first batch reaches the end");
        assert_eq!(video_ids(complete.as_page()), vec!["a"]);
        assert_eq!(complete.as_page().shelves[0].title, "Items");
    }
}
