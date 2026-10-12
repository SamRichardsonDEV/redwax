use std::fmt;

use serde_json::{Value, json};

use crate::{Track, walker};

/// A queue token is usable only with the seed request that produced it.
#[derive(Clone, PartialEq, Eq)]
pub struct RadioContinuation {
    seed_request_body: Value,
    token: String,
}

impl fmt::Debug for RadioContinuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RadioContinuation { .. }")
    }
}

impl RadioContinuation {
    pub fn new(seed_track: &Track, token: String) -> RadioContinuation {
        RadioContinuation {
            seed_request_body: request_body(&seed_track.video_id),
            token,
        }
    }

    pub(crate) fn request(&self) -> &Value {
        &self.seed_request_body
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

pub(crate) fn request_body(video_id: &str) -> Value {
    json!({
        "enablePersistentPlaylistPanel": true,
        "isAudioOnly": true,
        "tunerSettingValue": "AUTOMIX_SETTING_NORMAL",
        "videoId": video_id,
        "playlistId": format!("RDAMVM{video_id}"),
        "params": "wAEB",
    })
}

pub(crate) fn initial_batch(
    response: &Value,
    seed_request_body: &Value,
    seed_track: &Track,
) -> Option<(Vec<Track>, Option<RadioContinuation>)> {
    let (mut tracks, continuation) = next_batch(response, seed_request_body)?;
    if tracks.first().map(|track| &track.video_id) != Some(&seed_track.video_id) {
        tracks.retain(|track| track.video_id != seed_track.video_id);
        tracks.insert(0, seed_track.clone());
    }
    Some((tracks, continuation))
}

pub(crate) fn next_batch(
    response: &Value,
    seed_request_body: &Value,
) -> Option<(Vec<Track>, Option<RadioContinuation>)> {
    let queue_renderer = queue_panel(response)?;
    let queue_entries = queue_renderer
        .get("contents")
        .or_else(|| queue_renderer.get("items"))?
        .as_array()?;
    let tracks: Vec<Track> = queue_entries
        .iter()
        .filter_map(|entry| {
            let primary_renderer = entry
                .pointer("/playlistPanelVideoWrapperRenderer/primaryRenderer")
                .unwrap_or(entry);
            walker::item(primary_renderer)?.as_track().cloned()
        })
        .collect();
    let continuation = continuation_token(queue_renderer).map(|token| RadioContinuation {
        seed_request_body: seed_request_body.clone(),
        token,
    });
    Some((tracks, continuation))
}

fn queue_panel(response: &Value) -> Option<&Value> {
    if let Some(panel) = response.pointer("/continuationContents/playlistPanelContinuation") {
        return Some(panel);
    }
    let tabs = response
        .pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs")?
        .as_array()?;
    tabs.iter()
        .find_map(|tab| tab.pointer("/tabRenderer/content/musicQueueRenderer/content/playlistPanelRenderer"))
}

fn continuation_token(panel: &Value) -> Option<String> {
    let token = panel
        .get("continuations")
        .and_then(Value::as_array)
        .and_then(|entries| {
            ["nextRadioContinuationData", "nextContinuationData"]
                .into_iter()
                .find_map(|key| {
                    entries.iter().find_map(|entry| {
                        entry
                            .get(key)?
                            .get("continuation")?
                            .as_str()
                            .filter(|token| !token.is_empty())
                    })
                })
        });
    if let Some(token) = token {
        return Some(token.to_string());
    }
    panel
        .get("contents")
        .or_else(|| panel.get("items"))?
        .as_array()?
        .iter()
        .find_map(|entry| {
            let endpoint = entry.pointer("/continuationItemRenderer/continuationEndpoint")?;
            if let Some(token) = endpoint.get("continuationCommand").and_then(command_token) {
                return Some(token.to_string());
            }
            endpoint
                .pointer("/commandExecutorCommand/commands")?
                .as_array()?
                .iter()
                .find_map(|command| command_token(command.get("continuationCommand")?).map(str::to_string))
        })
}

fn command_token(command: &Value) -> Option<&str> {
    if !matches!(
        command.get("request").and_then(Value::as_str),
        None | Some("CONTINUATION_REQUEST_TYPE_WATCH_NEXT" | "CONTINUATION_REQUEST_TYPE_BROWSE")
    ) {
        return None;
    }
    command.get("token")?.as_str().filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> Track {
        serde_json::from_value(json!({ "video_id": "seed", "title": "Selected song" })).unwrap()
    }

    fn row(video_id: &str) -> Value {
        json!({ "playlistPanelVideoRenderer": {
            "videoId": video_id, "title": { "runs": [{ "text": video_id }] },
        } })
    }

    fn initial_response(panel: Value) -> Value {
        json!({ "contents": { "singleColumnMusicWatchNextResultsRenderer": {
            "tabbedRenderer": { "watchNextTabbedResultsRenderer": { "tabs": [
                { "tabRenderer": { "content": { "unrelated": row("not-queued") } } },
                { "tabRenderer": { "content": { "musicQueueRenderer": { "content": {
                    "playlistPanelRenderer": panel,
                } } } } },
            ] } },
        } } })
    }

    #[test]
    fn initial_radio_preserves_the_whole_batch_and_moves_the_seed_first() {
        let contents: Vec<Value> = (0..40)
            .map(|index| {
                if index == 17 {
                    row("seed")
                } else {
                    row(&format!("song-{index}"))
                }
            })
            .collect();
        let response = initial_response(json!({
            "contents": contents,
            "continuations": [{ "nextRadioContinuationData": { "continuation": "page-2" } }],
            "recommendations": { "contents": [row("not-queued")] },
        }));
        let body = request_body("seed");
        let (tracks, continuation) = initial_batch(&response, &body, &seed()).unwrap();
        assert_eq!(tracks.len(), 40);
        assert_eq!(tracks.first(), Some(&seed()));
        assert_eq!(tracks.last().unwrap().video_id, "song-39");
        assert!(!tracks.iter().any(|track| track.video_id == "not-queued"));
        let continuation = continuation.unwrap();
        assert_eq!(continuation.token(), "page-2");
        assert_eq!(continuation.request(), &body);
    }

    #[test]
    fn radio_continuation_keeps_the_original_request_and_does_not_reinsert_the_seed() {
        let mut body = request_body("seed");
        body["playlistId"] = json!("radio-playlist");
        body["params"] = json!("radio-params");
        body["index"] = json!(3);
        let response = json!({
            "continuationContents": { "playlistPanelContinuation": {
                "contents": [row("next-song")],
                "playlistId": "response-playlist",
                "continuations": [{ "nextRadioContinuationData": { "continuation": "page-3" } }],
            } },
            "recommendations": { "contents": [row("unrelated-song")] },
        });
        let (tracks, next) = next_batch(&response, &body).unwrap();
        assert_eq!(
            tracks
                .iter()
                .map(|track| track.video_id.as_str())
                .collect::<Vec<_>>(),
            ["next-song"]
        );
        let next = next.unwrap();
        assert_eq!(next.request(), &body);
        assert_eq!(next.token(), "page-3");
    }

    #[test]
    fn queue_continuation_items_and_generic_data_are_scoped_to_the_queue() {
        for panel in [
            json!({ "contents": [row("song")], "continuations": [
                { "nextContinuationData": { "continuation": "wrong" } },
                { "nextRadioContinuationData": { "continuation": "page-2" } },
            ] }),
            json!({ "contents": [row("song")], "continuations": [
                { "nextContinuationData": { "continuation": "page-2" } },
            ] }),
            json!({ "contents": [row("song"), { "continuationItemRenderer": {
                "continuationEndpoint": { "continuationCommand": { "token": "page-2" } },
            } }] }),
            json!({ "contents": [row("song"), { "continuationItemRenderer": {
                "continuationEndpoint": { "commandExecutorCommand": { "commands": [
                    { "continuationCommand": { "request": "UNRELATED_REQUEST", "token": "wrong" } },
                    { "continuationCommand": { "request": "CONTINUATION_REQUEST_TYPE_BROWSE", "token": "page-2" } },
                ] } },
            } }] }),
        ] {
            let response = initial_response(panel);
            let (_, next) = next_batch(&response, &request_body("seed")).unwrap();
            assert_eq!(next.unwrap().token(), "page-2");
        }
        let panel = json!({ "contents": [row("song")], "unrelated": {
            "continuations": [{ "nextRadioContinuationData": { "continuation": "wrong" } }],
        } });
        let (_, next) = next_batch(&initial_response(panel), &request_body("seed")).unwrap();
        assert!(next.is_none());
    }

    #[test]
    fn queue_wrappers_use_only_the_primary_track() {
        let panel = json!({ "contents": [{ "playlistPanelVideoWrapperRenderer": {
            "primaryRenderer": row("song"), "counterpart": row("counterpart"),
            "secondaryRenderers": [row("alternate")],
        } }] });
        let (tracks, next) = next_batch(&initial_response(panel), &request_body("seed")).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].video_id, "song");
        assert!(next.is_none());
    }

    #[test]
    fn malformed_panels_and_empty_tokens_do_not_create_a_cursor() {
        let body = request_body("seed");
        assert!(
            next_batch(
                &json!({ "related": { "playlistPanelRenderer": { "contents": [row("song")] } } }),
                &body
            )
            .is_none()
        );
        assert!(next_batch(&initial_response(json!({ "contents": "invalid" })), &body).is_none());
        let response = initial_response(json!({ "contents": [row("song")], "continuations": [
            { "nextRadioContinuationData": { "continuation": "" } },
        ] }));
        let (_, next) = next_batch(&response, &body).unwrap();
        assert!(next.is_none());
        let empty = json!({ "continuationContents": { "playlistPanelContinuation": {
            "contents": [], "continuations": [{ "nextRadioContinuationData": { "continuation": "page-4" } }],
        } } });
        let (tracks, next) = next_batch(&empty, &body).unwrap();
        assert!(tracks.is_empty());
        assert_eq!(next.unwrap().token(), "page-4");
    }

    #[test]
    fn cursor_debug_hides_the_token_and_request_context() {
        let continuation = RadioContinuation::new(&seed(), "page-2".to_string());
        assert_eq!(format!("{continuation:?}"), "RadioContinuation { .. }");
        assert_eq!(continuation, continuation.clone());
        assert_eq!(continuation.request()["params"], "wAEB");
        assert_eq!(continuation.request()["playlistId"], "RDAMVMseed");
        assert_ne!(
            continuation,
            RadioContinuation::new(&seed(), "page-3".to_string())
        );
    }
}
