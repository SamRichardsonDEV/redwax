//! Timed lyrics require an anonymous Android Music request. Other endpoints
//! share the selected browser session and WEB_REMIX identity.

use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use crate::{Error, Result, Session, walker};

pub const YTM_URL: &str = "https://music.youtube.com";
pub const CLIENT_NAME: &str = "WEB_REMIX";
const MOBILE_CLIENT_NAME: &str = "ANDROID_MUSIC";
/// Pinned, not derived from the date like the web version: the mobile app
/// ships a build number, and ytmusicapi sends this one.
const MOBILE_CLIENT_VERSION: &str = "7.21.50";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";
const TIMEOUT: Duration = Duration::from_secs(20);
/// A page that keeps handing back tokens without new items would loop forever,
/// so the follow stops here regardless of the requested count.
const MAX_CONTINUATIONS: usize = 12;

pub struct Transport {
    client: reqwest::Client,
    session: RwLock<Option<Session>>,
    client_version: String,
    /// Signed out, YouTube wants the id its own page hands the browser. It is
    /// fetched once and reused, because the fetch costs a whole round trip.
    visitor_id: OnceCell<String>,
}

impl Transport {
    pub fn new(session: Option<Session>) -> Transport {
        let client = reqwest::Client::builder()
            .gzip(true)
            .timeout(TIMEOUT)
            .build()
            .expect("reqwest builds with rustls compiled in");
        Transport {
            client,
            session: RwLock::new(session),
            client_version: current_client_version(),
            visitor_id: OnceCell::new(),
        }
    }

    /// A clone, not a borrow, because a login can replace it at any moment.
    pub fn session(&self) -> Option<Session> {
        self.session
            .read()
            .expect("lock is never held across a panic")
            .clone()
    }

    pub fn reload(&self, session: Option<Session>) {
        *self.session.write().expect("lock is never held across a panic") = session;
    }

    /// The request context every body starts from.
    pub fn context(&self) -> Value {
        json!({
            "context": {
                "client": {
                    "clientName": CLIENT_NAME,
                    "clientVersion": self.client_version,
                    "hl": "en",
                    "gl": "US",
                },
                "user": { "lockedSafetyMode": false },
            }
        })
    }

    /// POST `body` merged with `context()` to `endpoint` (`browse`, `search`,
    /// `next`, `player`, `like/like`, `account/account_menu`). Returns the
    /// parsed response. A 401 or 403 is `Error::Rejected`.
    pub async fn post(&self, endpoint: &str, body: Value) -> Result<Value> {
        let headers = self.headers().await?;
        self.send(endpoint, body, self.context(), headers, None).await
    }

    pub(crate) async fn post_continuation(&self, endpoint: &str, body: Value, token: &str) -> Result<Value> {
        let headers = self.headers().await?;
        self.send(endpoint, body, self.context(), headers, Some(token))
            .await
    }

    /// The `player` response for `video_id`, which carries `videoDetails`.
    /// The same request the resolver makes, under the same identity.
    pub(crate) async fn player(&self, video_id: &str) -> Result<Value> {
        self.post("player", json!({ "videoId": video_id })).await
    }

    /// POST as the Android music app with no account on the request. The only
    /// caller is the timed lyrics browse, which YouTube refuses under any
    /// other identity.
    pub(crate) async fn post_mobile(&self, endpoint: &str, body: Value) -> Result<Value> {
        let headers = self.anonymous_headers().await;
        self.send(endpoint, body, mobile_context(), headers, None).await
    }

    async fn send(
        &self,
        endpoint: &str,
        request_body: Value,
        mut request_payload: Value,
        headers: HeaderMap,
        continuation: Option<&str>,
    ) -> Result<Value> {
        if let (Some(payload_fields), Some(request_fields)) =
            (request_payload.as_object_mut(), request_body.as_object())
        {
            for (key, value) in request_fields {
                payload_fields.insert(key.clone(), value.clone());
            }
        }

        let url = endpoint_url(endpoint, continuation)?;
        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(&request_payload)
            .send()
            .await
            .map_err(|error| Error::Network(format!("{endpoint}: {}", kind_of(&error))))?;

        let http_status = response.status();
        if http_status == reqwest::StatusCode::UNAUTHORIZED || http_status == reqwest::StatusCode::FORBIDDEN {
            return Err(Error::Rejected);
        }
        if !http_status.is_success() {
            return Err(Error::Network(format!(
                "{endpoint}: HTTP {}",
                http_status.as_u16()
            )));
        }
        response
            .json()
            .await
            .map_err(|_| Error::Network(format!("{endpoint}: response was not JSON")))
    }

    /// Follow `continuationItemRenderer` or `nextContinuationData` tokens
    /// until the requested count is reached or none remain. Used by home
    /// (rows) and the library. A playlist pages from the app instead, so the
    /// first screen of tracks never waits on the rest.
    pub async fn browse_with_continuations(
        &self,
        initial_request_body: Value,
        minimum_result_count: usize,
    ) -> Result<Vec<Value>> {
        let initial_response = self.post("browse", initial_request_body.clone()).await?;
        // A page carrying several shelves is paginating rows, one shelf is
        // paginating tracks. The unit is fixed by the first response so that
        // the loose shelf a continuation opens does not change it.
        let count_shelves = filled_shelves(&initial_response) > 1;
        let mut responses = vec![initial_response];

        for _ in 0..MAX_CONTINUATIONS {
            if measure(&responses, count_shelves) >= minimum_result_count {
                break;
            }
            let Some(token) = continuation_token(responses.last().expect("just pushed one")) else {
                break;
            };
            let mut continuation_request = initial_request_body.clone();
            if let Some(object) = continuation_request.as_object_mut() {
                object.insert("continuation".to_string(), Value::String(token));
            }
            let response = self.post("browse", continuation_request).await?;
            if measure(std::slice::from_ref(&response), count_shelves) == 0 {
                break;
            }
            responses.push(response);
        }
        Ok(responses)
    }

    async fn headers(&self) -> Result<HeaderMap> {
        let Some(session) = self.session() else {
            return Ok(self.anonymous_headers().await);
        };
        let mut headers = base_headers();
        headers.extend(
            session
                .music_headers(unix_seconds())
                .map_err(|_| Error::Rejected)?,
        );
        Ok(headers)
    }

    /// What the transport sends with no account named: the signed out path and
    /// the mobile lyrics browse both use it.
    async fn anonymous_headers(&self) -> HeaderMap {
        let mut headers = base_headers();
        let visitor_id = self.visitor().await;
        if let Some(visitor_header) =
            visitor_id.and_then(|visitor_id| HeaderValue::from_str(&visitor_id).ok())
        {
            headers.insert("x-goog-visitor-id", visitor_header);
        }
        headers
    }

    /// The visitor id YouTube stamps into its own page. `None` leaves the
    /// header off, which YouTube answers anyway for public pages.
    async fn visitor(&self) -> Option<String> {
        let visitor_lookup = self
            .visitor_id
            .get_or_try_init(|| async {
                let music_page_html = self
                    .client
                    .get(YTM_URL)
                    .header("user-agent", USER_AGENT)
                    .send()
                    .await
                    .map_err(|_| ())?
                    .text()
                    .await
                    .map_err(|_| ())?;
                visitor_id_in(&music_page_html).ok_or(())
            })
            .await;
        visitor_lookup.ok().cloned()
    }
}

fn endpoint_url(endpoint: &str, continuation: Option<&str>) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("{YTM_URL}/youtubei/v1/{endpoint}?alt=json"))
        .map_err(|_| Error::Network("invalid request endpoint".to_string()))?;
    if let Some(token) = continuation {
        // Both query parameters are required while the original radio body remains unchanged.
        url.query_pairs_mut()
            .append_pair("ctoken", token)
            .append_pair("continuation", token);
    }
    Ok(url)
}

/// Everything a request carries whatever identity it goes out under.
fn base_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
    headers.insert("accept", HeaderValue::from_static("*/*"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("origin", HeaderValue::from_static(YTM_URL));
    headers.insert("x-origin", HeaderValue::from_static(YTM_URL));
    headers
}

fn mobile_context() -> Value {
    json!({
        "context": {
            "client": {
                "clientName": MOBILE_CLIENT_NAME,
                "clientVersion": MOBILE_CLIENT_VERSION,
                "hl": "en",
                "gl": "US",
            },
            "user": { "lockedSafetyMode": false },
        }
    })
}

/// Reqwest's own message can carry the full URL, which for a player request
/// holds a signature, so only the kind of failure is reported.
fn kind_of(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "request failed"
    }
}

/// The `VISITOR_DATA` the site writes into its own `ytcfg.set({...})` call.
fn visitor_id_in(music_page_html: &str) -> Option<String> {
    let mut remaining_html = music_page_html;
    while let Some(config_call_offset) = remaining_html.find("ytcfg.set(") {
        remaining_html = &remaining_html[config_call_offset + "ytcfg.set(".len()..];
        let object_start_offset = remaining_html.find('{')?;
        let mut brace_depth = 0usize;
        let mut object_end_offset = None;
        for (character_byte_offset, character) in remaining_html[object_start_offset..].char_indices() {
            match character {
                '{' => brace_depth += 1,
                '}' => {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        object_end_offset = Some(object_start_offset + character_byte_offset + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let object_end_offset = object_end_offset?;
        if let Ok(page_config) =
            serde_json::from_str::<Value>(&remaining_html[object_start_offset..object_end_offset])
            && let Some(visitor_id) = page_config.get("VISITOR_DATA").and_then(Value::as_str)
        {
            return Some(visitor_id.to_string());
        }
        remaining_html = &remaining_html[object_end_offset..];
    }
    None
}

fn filled_shelves(response: &Value) -> usize {
    walker::shelves(response, "")
        .iter()
        .filter(|shelf| !shelf.items.is_empty())
        .count()
}

fn measure(responses: &[Value], count_shelves: bool) -> usize {
    responses
        .iter()
        .map(|response| {
            if count_shelves {
                filled_shelves(response)
            } else {
                walker::items(response).len()
            }
        })
        .sum()
}

/// The token that continues the shelf the page is built around, and no other.
///
/// A playlist names two: a `continuationItemRenderer` closing the track shelf,
/// and a `sectionListRenderer` one that opens the suggestion rows under it.
/// Following the second returns no tracks, so a caller after tracks asks for
/// this one alone.
pub(crate) fn shelf_token(response: &Value) -> Option<String> {
    tokens_in(response).shelf
}

/// The token that continues whatever list the page paginates: the shelf when it
/// names one, else the sections, which is how home and the library continue.
fn continuation_token(response: &Value) -> Option<String> {
    let continuation_tokens = tokens_in(response);
    continuation_tokens.shelf.or(continuation_tokens.section)
}

/// The first shelf token and the last section token, which is the pair a browse
/// response can carry.
#[derive(Default)]
struct Tokens {
    shelf: Option<String>,
    section: Option<String>,
}

fn tokens_in(response: &Value) -> Tokens {
    let mut continuation_tokens = Tokens::default();
    collect_tokens(response, &mut continuation_tokens);
    continuation_tokens
}

fn collect_tokens(node: &Value, continuation_tokens: &mut Tokens) {
    match node {
        Value::Array(entries) => {
            for entry in entries {
                collect_tokens(entry, continuation_tokens);
            }
        }
        Value::Object(object) => {
            if continuation_tokens.shelf.is_none()
                && let Some(renderer) = object.get("continuationItemRenderer")
            {
                continuation_tokens.shelf = first_token(renderer);
            }
            for key in ["nextContinuationData", "reloadContinuationData"] {
                if let Some(token) = object
                    .get(key)
                    .and_then(|node| node.get("continuation"))
                    .and_then(Value::as_str)
                {
                    continuation_tokens.section = Some(token.to_string());
                }
            }
            for value in object.values() {
                collect_tokens(value, continuation_tokens);
            }
        }
        _ => {}
    }
}

fn first_token(renderer: &Value) -> Option<String> {
    let mut continuation_token = None;
    token_in(renderer, &mut continuation_token);
    continuation_token
}

fn token_in(node: &Value, continuation_token: &mut Option<String>) {
    if continuation_token.is_some() {
        return;
    }
    match node {
        Value::Array(entries) => {
            for entry in entries {
                token_in(entry, continuation_token);
            }
        }
        Value::Object(object) => {
            if let Some(command) = object.get("continuationCommand")
                && let Some(token) = command.get("token").and_then(Value::as_str)
            {
                *continuation_token = Some(token.to_string());
                return;
            }
            for value in object.values() {
                token_in(value, continuation_token);
            }
        }
        _ => {}
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

/// `1.<yyyymmdd>.01.00`, the version ytmusicapi builds from today's date in UTC.
fn current_client_version() -> String {
    let (year, month, day) = civil_date(unix_seconds() / 86_400);
    format!("1.{year:04}{month:02}{day:02}.01.00")
}

/// Days since the epoch to a calendar date, by Howard Hinnant's civil_from_days.
fn civil_date(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_continuation_uses_two_encoded_query_parameters() {
        let token = "page+2&opaque=value#section%done";
        let url = endpoint_url("next", Some(token)).unwrap();
        assert_eq!(url.path(), "/youtubei/v1/next");
        assert_eq!(url.fragment(), None);
        let query: Vec<_> = url
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(
            query,
            [
                ("alt".to_string(), "json".to_string()),
                ("ctoken".to_string(), token.to_string()),
                ("continuation".to_string(), token.to_string()),
            ]
        );
        let plain = endpoint_url("browse", None).unwrap();
        assert_eq!(plain.query(), Some("alt=json"));
    }

    #[test]
    fn visitor_id_comes_out_of_the_page_config() {
        let page = r#"<script>ytcfg.set({"A":{"b":1}}); ytcfg.set({"VISITOR_DATA":"Cg1x"});</script>"#;
        assert_eq!(visitor_id_in(page).as_deref(), Some("Cg1x"));
        assert_eq!(visitor_id_in("<html></html>"), None);
    }

    fn fixture(name: &str) -> Value {
        let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    /// The section token continues the rows under the tracks, so a playlist
    /// that follows it stops at the first hundred.
    #[test]
    fn a_playlist_continues_by_its_shelf_and_not_by_its_sections() {
        let playlist = fixture("playlist.json");
        let shelf = shelf_token(&playlist).expect("the track shelf names its continuation");
        let section = "4qmFsgI9Ei1WTFJEQ0xBSzV1eV9udlJHWEhwRnpvbjhxYlBUWmh1b1NxM1dPNGNjWXZtSE0aDGtnRURDTTBHOEFFQQ%3D%3D";
        assert_ne!(shelf, section);
        assert_eq!(continuation_token(&playlist).as_deref(), Some(shelf.as_str()));
    }

    #[test]
    fn a_page_with_no_shelf_token_continues_by_its_sections() {
        let home = fixture("home.json");
        assert_eq!(shelf_token(&home), None);
        assert!(continuation_token(&home).is_some());
    }

    #[test]
    fn client_version_has_the_shape_youtube_accepts() {
        let version = current_client_version();
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 4, "{version}");
        assert_eq!(parts[0], "1");
        assert_eq!(parts[1].len(), 8, "{version}");
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()), "{version}");
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(20_705), (2026, 9, 9));
        assert_eq!(civil_date(19_723), (2024, 1, 1));
    }
}
