//! One POST to `youtubei/v1/player`, and the format picking that follows it.
//!
//! Steps 3 and 4 differ only in the identity they claim and the credentials
//! they carry, so both go through `player` and share the format code.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::{AuthContext, PageAuth};
use crate::{Error, Failure, FailureKind, Result, Tier};

/// A client identity from yt-dlp's `INNERTUBE_CLIENTS` table.
pub struct Identity {
    pub host: &'static str,
    pub client_name: &'static str,
    pub client_number: u32,
    pub user_agent: &'static str,
    /// Device fields the identity is rejected without.
    pub device: &'static [(&'static str, &'static str)],
}

/// The signed-in identity. Its version moves with YouTube, so it is a default
/// that `client_version` replaces once the live page has been read.
pub const WEB_REMIX: Identity = Identity {
    host: "music.youtube.com",
    client_name: "WEB_REMIX",
    client_number: 67,
    user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0",
    device: &[],
};

pub const WEB_REMIX_VERSION: &str = "1.20260901.12.00";

/// This client supplies direct audio formats instead of SABR-only responses.
pub const VISIONOS: Identity = Identity {
    host: "www.youtube.com",
    client_name: "VISIONOS",
    client_number: 101,
    user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) \
                 Version/26.0 Safari/605.1.15",
    device: &[
        ("deviceMake", "Apple"),
        ("deviceModel", "RealityDevice17,1"),
        ("osName", "visionOS"),
        ("osVersion", "26.5.23O471"),
    ],
};

pub const VISIONOS_VERSION: &str = "1.02";

pub struct PlayerRequest<'a> {
    pub identity: &'a Identity,
    pub client_version: &'a str,
    pub video_id: &'a str,
    pub auth: Option<&'a AuthContext>,
    pub visitor_id: Option<&'a str>,
    /// Must come from the same player script that solves this response.
    pub signature_timestamp: Option<u32>,
    /// Sent only on a second try, after the first stream URL was refused.
    pub po_token: Option<&'a str>,
}

impl PlayerRequest<'_> {
    fn visitor_data(&self) -> Option<&str> {
        match self.auth {
            Some(auth) => auth.visitor_data(),
            None => self.visitor_id,
        }
    }
}

/// One process-wide connection pool, so a warm resolve pays no TLS handshake.
pub fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest builds with the features this crate selects")
    })
}

/// A classified failure with sanitized evidence from its source.
pub fn failed(kind: FailureKind, what: &str, detail: impl std::fmt::Display) -> Error {
    Error::Failed(Failure::new(kind, what, detail))
}

fn safe_reqwest_error(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_body() {
        "body failed"
    } else if error.is_decode() {
        "decode failed"
    } else if error.is_request() {
        "request failed"
    } else if error.is_status() {
        "HTTP status error"
    } else {
        "transport failed"
    }
}

fn player_body(request: &PlayerRequest<'_>) -> Value {
    let identity = request.identity;
    let mut client_context = serde_json::Map::new();
    client_context.insert("clientName".into(), json!(identity.client_name));
    client_context.insert("clientVersion".into(), json!(request.client_version));
    for (device_field, device_value) in identity.device {
        client_context.insert((*device_field).into(), json!(device_value));
    }
    client_context.insert("hl".into(), json!("en"));
    client_context.insert("gl".into(), json!("US"));
    if let Some(visitor_data) = request.visitor_data() {
        client_context.insert("visitorData".into(), json!(visitor_data));
    }
    let mut player_request_body = json!({
        "context": {"client": client_context},
        "videoId": request.video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    if let Some(timestamp) = request.signature_timestamp {
        player_request_body["playbackContext"] = json!({
            "contentPlaybackContext": {"signatureTimestamp": timestamp}
        });
    }
    if let Some(token) = request.po_token {
        player_request_body["serviceIntegrityDimensions"] = json!({"poToken": token});
    }
    player_request_body
}

fn player_request(request: &PlayerRequest<'_>, unix_seconds: u64) -> Result<reqwest::RequestBuilder> {
    let identity = request.identity;
    let player_request_body = player_body(request);
    let origin = format!("https://{}", identity.host);
    let mut request_builder = http()
        .post(format!("{origin}/youtubei/v1/player?prettyPrint=false"))
        .header("Content-Type", "application/json")
        .header("Accept", "*/*")
        .header("Origin", &origin)
        .header("User-Agent", identity.user_agent)
        .header("X-Youtube-Client-Name", identity.client_number.to_string())
        .header("X-Youtube-Client-Version", request.client_version);
    if let Some(auth_context) = request.auth {
        if identity.host != WEB_REMIX.host {
            return Err(failed(
                FailureKind::AuthenticationRejected,
                "authentication",
                "player identity does not match the account origin",
            ));
        }
        request_builder = request_builder.headers(auth_context.headers(unix_seconds)?);
    } else if let Some(visitor_data) = request.visitor_data() {
        let mut visitor_header = reqwest::header::HeaderValue::from_str(visitor_data).map_err(|_| {
            failed(
                FailureKind::UnknownResponse,
                "player request",
                "invalid visitor metadata",
            )
        })?;
        visitor_header.set_sensitive(true);
        request_builder = request_builder.header("X-Goog-Visitor-Id", visitor_header);
    }
    Ok(request_builder.json(&player_request_body))
}

pub async fn player(request: PlayerRequest<'_>) -> Result<Value> {
    let request_builder = player_request(&request, crate::unix_now())?;
    let http_response = crate::trace::timed("player request", request_builder.send())
        .await
        .map_err(|error| {
            crate::trace::record_runtime_evidence(
                "player.transport_failure",
                Duration::ZERO,
                "response=false",
            );
            failed(
                FailureKind::Transport,
                "player request",
                safe_reqwest_error(&error),
            )
        })?;
    let http_status = http_response.status();
    let response_text = http_response.text().await.map_err(|error| {
        crate::trace::record_runtime_evidence("player.body_failure", Duration::ZERO, "body=false");
        failed(
            FailureKind::Transport,
            "player response",
            safe_reqwest_error(&error),
        )
    })?;
    if !http_status.is_success() {
        let evidence = format!("status={}", http_status.as_u16());
        crate::trace::record_runtime_evidence("player.http_failure", Duration::ZERO, &evidence);
        let kind = match (request.auth.is_some(), http_status.as_u16()) {
            (true, 401 | 403) => FailureKind::AuthenticationRejected,
            _ => FailureKind::UnknownResponse,
        };
        return Err(failed(
            kind,
            "player request",
            format!("HTTP {}", http_status.as_u16()),
        ));
    }
    serde_json::from_str(&response_text).map_err(|error| {
        crate::trace::record_runtime_evidence("player.json_failure", Duration::ZERO, "valid_json=false");
        failed(FailureKind::UnknownResponse, "player response", error)
    })
}

/// The account binding a proof of origin token is issued against when signed
/// in. Absent from an anonymous response.
pub fn datasync_id(player_response: &Value) -> Option<String> {
    player_response
        .pointer("/responseContext/mainAppWebResponseContext/datasyncId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Check the stream with the unbounded request that mpv uses to open it.
/// A small range can succeed even when the full stream is forbidden.
/// Bodies larger than one kilobyte are accepted from their headers only.
pub async fn verify(url: &str, user_agent: &str) -> Result<u16> {
    crate::trace::timed("cdn check", async {
        let mut cdn_response = http()
            .get(url)
            .header("User-Agent", user_agent)
            .send()
            .await
            .map_err(|error| failed(FailureKind::Transport, "stream check", safe_reqwest_error(&error)))?;
        let http_status = cdn_response.status().as_u16();
        if (200..300).contains(&http_status) {
            let initial_range_length = if http_status == 206 {
                let initial_range_length = cdn_response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .and_then(initial_range_length);
                if initial_range_length.is_none() {
                    return Err(failed(
                        FailureKind::UnknownResponse,
                        "stream check",
                        "the partial response exceeds the initial range",
                    ));
                }
                initial_range_length
            } else {
                None
            };
            let content_length = cdn_response.content_length();
            if content_length.is_some()
                && initial_range_length.is_some()
                && content_length != initial_range_length
            {
                return Err(failed(
                    FailureKind::UnknownResponse,
                    "stream check",
                    "the partial response length disagrees with its range",
                ));
            }
            let content_length = content_length.or(initial_range_length);
            let Some(content_length) = content_length else {
                return Err(failed(
                    FailureKind::UnknownResponse,
                    "stream check",
                    "the successful response has no bounded length",
                ));
            };
            if content_length == 0 {
                return Err(failed(
                    FailureKind::CdnRefused,
                    "stream check body",
                    "the CDN returned no audio bytes",
                ));
            }
            if content_length > 1024 {
                if http_status == 206 {
                    return Err(failed(
                        FailureKind::UnknownResponse,
                        "stream check",
                        "the partial response exceeds the requested range",
                    ));
                }
                return Ok(http_status);
            }
            let response_chunk = cdn_response.chunk().await.map_err(|error| {
                failed(
                    FailureKind::Transport,
                    "stream check body",
                    safe_reqwest_error(&error),
                )
            })?;
            if response_chunk.is_none_or(|audio_bytes| audio_bytes.is_empty()) {
                return Err(failed(
                    FailureKind::CdnRefused,
                    "stream check body",
                    "the CDN returned no audio bytes",
                ));
            }
        }
        Ok(http_status)
    })
    .await
}

fn initial_range_length(content_range: &str) -> Option<u64> {
    let byte_range = content_range.strip_prefix("bytes ")?.split_once('/')?.0;
    let (range_start, range_end) = byte_range.split_once('-')?;
    let range_start: u64 = range_start.parse().ok()?;
    let range_end: u64 = range_end.parse().ok()?;
    (range_start == 0 && range_end < 1024).then_some(range_end + 1)
}

/// `Ok` only for a playable track. Everything else names the status YouTube gave.
pub fn playability(player_response: &Value, video_id: &str) -> Result<()> {
    let playability_status = playability_status(player_response);
    if playability_status == "OK" {
        return Ok(());
    }
    let kind = match playability_status {
        "LOGIN_REQUIRED" => FailureKind::AuthenticationRejected,
        "AGE_CHECK_REQUIRED" | "CONTENT_CHECK_REQUIRED" => FailureKind::ContentRestricted,
        "UNPLAYABLE" => FailureKind::Unavailable,
        _ => FailureKind::UnknownResponse,
    };
    Err(failed(kind, video_id, format!("status={playability_status}")))
}

fn playability_status(player_response: &Value) -> &str {
    player_response
        .pointer("/playabilityStatus/status")
        .and_then(Value::as_str)
        .unwrap_or("MISSING")
}

/// True when a changed client version can plausibly change playability.
pub fn can_refresh_client_version(player_response: &Value) -> bool {
    matches!(
        playability_status(player_response),
        "ERROR" | "UNPLAYABLE" | "MISSING"
    )
}

/// A fixed response fingerprint for runtime diagnostics.
pub fn player_response_evidence(
    player_response: &Value,
    client_version: &str,
    token_was_sent: bool,
) -> String {
    let playability_status = player_response
        .pointer("/playabilityStatus/status")
        .and_then(Value::as_str)
        .map(safe_label)
        .unwrap_or("MISSING");
    let adaptive_formats = player_response
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array);
    let audio_formats: Vec<&Value> = adaptive_formats
        .into_iter()
        .flatten()
        .filter(|format| {
            format
                .get("mimeType")
                .and_then(Value::as_str)
                .is_some_and(|mime_type| mime_type.starts_with("audio/"))
        })
        .collect();
    let audio_format_ids = audio_formats
        .iter()
        .filter_map(|format| format.get("itag").and_then(Value::as_u64))
        .map(|itag| itag.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let plain_url_count = audio_formats
        .iter()
        .filter(|format| format.get("url").is_some())
        .count();
    let cipher_count = audio_formats
        .iter()
        .filter(|format| format.get("signatureCipher").is_some())
        .count();
    format!(
        concat!(
            "client=web-remix version={} token={} status={playability_status} ",
            "streaming={} audio={} urls={plain_url_count} ciphers={cipher_count} ",
            "itags={} binding={}"
        ),
        safe_version(client_version),
        token_was_sent,
        adaptive_formats.is_some(),
        audio_formats.len(),
        if audio_format_ids.is_empty() {
            "none"
        } else {
            &audio_format_ids
        },
        datasync_id(player_response).is_some(),
        playability_status = playability_status,
        plain_url_count = plain_url_count,
        cipher_count = cipher_count,
    )
}

fn safe_version(version: &str) -> &str {
    match !version.is_empty()
        && version.len() <= 32
        && version.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        true => version,
        false => "other",
    }
}

fn safe_label(value: &str) -> &str {
    match !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
    {
        true => value,
        false => "OTHER",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFormat {
    pub tier: Tier,
    /// Present when YouTube served the URL plain.
    pub url: Option<String>,
    /// The `signatureCipher` query string, present when it did not.
    pub cipher: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioTrackPriority {
    Original,
    Default,
    WithoutMetadata,
    Alternate,
    Descriptive,
}

impl AudioTrackPriority {
    const PREFERRED: [AudioTrackPriority; 5] = [
        AudioTrackPriority::Original,
        AudioTrackPriority::Default,
        AudioTrackPriority::WithoutMetadata,
        AudioTrackPriority::Alternate,
        AudioTrackPriority::Descriptive,
    ];
}

fn audio_track_language(adaptive_format: &Value) -> Option<&str> {
    adaptive_format
        .pointer("/audioTrack/id")
        .and_then(Value::as_str)
        .and_then(|track_id| track_id.split('.').next())
        .filter(|language| !language.is_empty())
}

fn audio_track_name(adaptive_format: &Value) -> &str {
    adaptive_format
        .pointer("/audioTrack/displayName")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn is_auto_dubbed(adaptive_format: &Value) -> bool {
    adaptive_format
        .pointer("/audioTrack/isAutoDubbed")
        .and_then(Value::as_bool)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioContent {
    Original,
    Dubbed,
    Unmarked,
}

fn audio_content_from_url(url: &str) -> AudioContent {
    let Some(audio_tags) = query_value(url, "xtags") else {
        return AudioContent::Unmarked;
    };
    if audio_tags.contains("acont=original") {
        AudioContent::Original
    } else if audio_tags.contains("acont=dubbed") {
        AudioContent::Dubbed
    } else {
        AudioContent::Unmarked
    }
}

pub(super) fn is_dubbed_stream_url(url: &str) -> bool {
    audio_content_from_url(url) == AudioContent::Dubbed
}

fn audio_content(adaptive_format: &Value) -> AudioContent {
    let delivery_url = adaptive_format
        .get("url")
        .and_then(Value::as_str)
        .and_then(valid_stream_url)
        .or_else(|| {
            adaptive_format
                .get("signatureCipher")
                .and_then(Value::as_str)
                .and_then(parse_cipher)
                .map(|cipher| cipher.url)
        });
    delivery_url
        .as_deref()
        .map(audio_content_from_url)
        .unwrap_or(AudioContent::Unmarked)
}

fn preferred_audio_languages(adaptive_formats: &[Value]) -> (HashSet<&str>, HashSet<&str>) {
    let mut original_languages = HashSet::new();
    let mut default_languages = HashSet::new();
    for adaptive_format in adaptive_formats {
        let is_audio = adaptive_format
            .get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|mime_type| mime_type.starts_with("audio/"));
        if !is_audio {
            continue;
        }
        let Some(language) = audio_track_language(adaptive_format) else {
            continue;
        };
        let display_name = audio_track_name(adaptive_format).to_ascii_lowercase();
        if display_name.contains("original") || audio_content(adaptive_format) == AudioContent::Original {
            original_languages.insert(language);
        } else if !is_auto_dubbed(adaptive_format)
            && audio_content(adaptive_format) != AudioContent::Dubbed
            && adaptive_format
                .pointer("/audioTrack/audioIsDefault")
                .and_then(Value::as_bool)
                .unwrap_or_default()
        {
            default_languages.insert(language);
        }
    }
    (original_languages, default_languages)
}

fn audio_track_priority(
    adaptive_format: &Value,
    original_languages: &HashSet<&str>,
    default_languages: &HashSet<&str>,
) -> AudioTrackPriority {
    let audio_content = audio_content(adaptive_format);
    let Some(audio_track) = adaptive_format.get("audioTrack") else {
        return match audio_content {
            AudioContent::Original => AudioTrackPriority::Original,
            AudioContent::Dubbed => AudioTrackPriority::Alternate,
            AudioContent::Unmarked => AudioTrackPriority::WithoutMetadata,
        };
    };
    let display_name = audio_track_name(adaptive_format).to_ascii_lowercase();
    let language = audio_track_language(adaptive_format);
    if display_name.contains("descriptive") {
        return AudioTrackPriority::Descriptive;
    }
    if display_name.contains("original") || audio_content == AudioContent::Original {
        return AudioTrackPriority::Original;
    }
    if is_auto_dubbed(adaptive_format) || audio_content == AudioContent::Dubbed {
        return AudioTrackPriority::Alternate;
    }
    if language.is_some_and(|language| original_languages.contains(language)) {
        return AudioTrackPriority::Original;
    }
    if audio_track
        .get("audioIsDefault")
        .and_then(Value::as_bool)
        .unwrap_or_default()
        || language.is_some_and(|language| default_languages.contains(language))
    {
        return AudioTrackPriority::Default;
    }
    if language.is_none() && display_name.is_empty() {
        AudioTrackPriority::WithoutMetadata
    } else {
        AudioTrackPriority::Alternate
    }
}

fn valid_stream_url(url: &str) -> Option<String> {
    let parsed_url = url::Url::parse(url).ok()?;
    matches!(parsed_url.scheme(), "http" | "https")
        .then(|| url.to_owned())
        .filter(|_| parsed_url.host_str().is_some())
}

/// The best audio-only encode, preferring its original or default track before quality.
pub fn best_audio(player_response: &Value) -> Option<AudioFormat> {
    let adaptive_formats = player_response
        .pointer("/streamingData/adaptiveFormats")?
        .as_array()?;
    let (original_languages, default_languages) = preferred_audio_languages(adaptive_formats);
    let mut supported_audio_formats: Vec<(AudioTrackPriority, AudioFormat)> = Vec::new();
    for adaptive_format in adaptive_formats {
        let mime_type = adaptive_format
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !mime_type.starts_with("audio/") {
            continue;
        }
        let Some(tier) = adaptive_format
            .get("itag")
            .and_then(Value::as_u64)
            .and_then(|itag| Tier::from_itag(itag as u32))
        else {
            continue;
        };
        let validated_stream_url = adaptive_format
            .get("url")
            .and_then(Value::as_str)
            .and_then(valid_stream_url);
        let signature_cipher = adaptive_format
            .get("signatureCipher")
            .and_then(Value::as_str)
            .filter(|cipher| parse_cipher(cipher).is_some())
            .map(str::to_owned);
        if validated_stream_url.is_none() && signature_cipher.is_none() {
            continue;
        }
        supported_audio_formats.push((
            audio_track_priority(adaptive_format, &original_languages, &default_languages),
            AudioFormat {
                tier,
                url: validated_stream_url,
                cipher: signature_cipher,
            },
        ));
    }
    AudioTrackPriority::PREFERRED.iter().find_map(|priority| {
        Tier::PREFERRED.iter().find_map(|preferred_tier| {
            supported_audio_formats
                .iter()
                .find(|(candidate_priority, format)| {
                    candidate_priority == priority && format.tier == *preferred_tier
                })
                .map(|(_, format)| format.clone())
        })
    })
}

/// The three fields of a `signatureCipher`: the challenge, the parameter it is
/// sent back as, and the URL it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cipher {
    pub s: String,
    pub sp: String,
    pub url: String,
}

pub fn parse_cipher(query: &str) -> Option<Cipher> {
    let mut signature_challenge = None;
    let mut signature_parameter_name = None;
    let mut cipher_url = None;
    for (wire_field, wire_value) in url::form_urlencoded::parse(query.as_bytes()) {
        match wire_field.as_ref() {
            "s" => signature_challenge = Some(wire_value.into_owned()),
            "sp" => signature_parameter_name = Some(wire_value.into_owned()),
            "url" => cipher_url = Some(wire_value.into_owned()),
            _ => {}
        }
    }
    Some(Cipher {
        s: signature_challenge?,
        sp: signature_parameter_name.unwrap_or_else(|| "signature".to_owned()),
        url: valid_stream_url(&cipher_url?)?,
    })
}

/// The base URL a format plays from, before either challenge is answered.
pub fn base_url(format: &AudioFormat) -> Option<String> {
    match (&format.url, &format.cipher) {
        (Some(url), _) => Some(url.clone()),
        (None, Some(signature_cipher)) => {
            parse_cipher(signature_cipher).map(|parsed_cipher| parsed_cipher.url)
        }
        (None, None) => None,
    }
}

pub fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

/// Everything outside the unreserved set is escaped, so a solved value cannot
/// end a query segment early.
fn encode(value: &str) -> String {
    let mut encoded_value = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded_value.push(byte as char)
            }
            _ => encoded_value.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded_value
}

/// Replaces one query value and leaves every other segment byte for byte, so
/// the signature still covers the URL it was computed over.
pub fn with_query_value(url: &str, key: &str, value: &str) -> String {
    let Some((url_base, query)) = url.split_once('?') else {
        return format!("{url}?{key}={}", encode(value));
    };
    let mut replaced = false;
    let mut query_segments: Vec<String> = Vec::new();
    for query_segment in query.split('&') {
        if query_segment == key || query_segment.starts_with(&format!("{key}=")) {
            replaced = true;
            query_segments.push(format!("{key}={}", encode(value)));
        } else {
            query_segments.push(query_segment.to_owned());
        }
    }
    if !replaced {
        query_segments.push(format!("{key}={}", encode(value)));
    }
    format!("{url_base}?{}", query_segments.join("&"))
}

/// The playable URL: the signature appended under the name the cipher asked
/// for, then `n` swapped for its solution.
pub fn assemble(
    format: &AudioFormat,
    solved_signature: Option<&str>,
    solved_n_parameter: Option<&str>,
) -> Option<String> {
    let mut stream_url = base_url(format)?;
    if let Some(parsed_cipher) = format.cipher.as_deref().and_then(parse_cipher) {
        let solved_signature = solved_signature?;
        stream_url = format!("{stream_url}&{}={}", parsed_cipher.sp, encode(solved_signature));
    }
    if query_value(&stream_url, "n").is_some() {
        stream_url = with_query_value(&stream_url, "n", solved_n_parameter?);
    }
    Some(stream_url)
}

/// Fields that name the account, the session or this machine. Dropped before a
/// response is written anywhere a fixture could be committed from.
const PRIVATE_KEYS: [&str; 13] = [
    "responseContext",
    "trackingParams",
    "serviceTrackingParams",
    "frameworkUpdates",
    "visitorData",
    "datasyncId",
    "playbackTracking",
    "playerConfig",
    "attestation",
    "adPlacements",
    "adBreakHeartbeatParams",
    "heartbeatParams",
    "storyboards",
];

/// Removes the account fields, and the `ip` parameter every stream URL carries.
pub fn scrub(response_value: &mut Value) {
    match response_value {
        Value::Object(object_fields) => {
            for key in PRIVATE_KEYS {
                object_fields.remove(key);
            }
            for child_value in object_fields.values_mut() {
                scrub(child_value);
            }
        }
        Value::Array(array_items) => array_items.iter_mut().for_each(scrub),
        Value::String(text) if text.contains("ip=") => *text = strip_ip(text),
        _ => {}
    }
}

/// Drops every `ip` query parameter, and only that name, so `fvip` and
/// `clientip` survive.
fn strip_ip(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut scrubbed_text = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let starts_parameter = index == 0 || bytes[index - 1] == b'?' || bytes[index - 1] == b'&';
        if starts_parameter && text[index..].starts_with("ip=") {
            let parameter_end = text[index..]
                .find('&')
                .map(|separator_offset| index + separator_offset + 1)
                .unwrap_or(bytes.len());
            index = parameter_end;
            // The separator went with the parameter, so a trailing one is trimmed.
            if index >= bytes.len() {
                while scrubbed_text.ends_with('&') || scrubbed_text.ends_with('?') {
                    scrubbed_text.pop();
                }
            }
            continue;
        }
        let character = text[index..]
            .chars()
            .next()
            .expect("index sits on a character boundary");
        scrubbed_text.push(character);
        index += character.len_utf8();
    }
    scrubbed_text
}

fn scrape<'a>(page_body: &'a str, config_key: &str) -> Option<&'a str> {
    let key_prefix = format!("\"{config_key}\":\"");
    let value_start = page_body.find(&key_prefix)? + key_prefix.len();
    let remaining_page = &page_body[value_start..];
    let value_end = remaining_page.find('"')?;
    Some(&remaining_page[..value_end])
}

async fn page(page_url: &str, user_agent: &str) -> Result<String> {
    let page_response = http()
        .get(page_url)
        .header("User-Agent", user_agent)
        .send()
        .await
        .map_err(|error| failed(FailureKind::Transport, "page request", safe_reqwest_error(&error)))?;
    let http_status = page_response.status();
    if !http_status.is_success() {
        return Err(failed(
            FailureKind::UnknownResponse,
            "page request",
            format!("HTTP {}", http_status.as_u16()),
        ));
    }
    page_response
        .text()
        .await
        .map_err(|error| failed(FailureKind::Transport, "page body", safe_reqwest_error(&error)))
}

#[derive(Clone, Default)]
pub struct SignedInPage {
    pub auth: PageAuth,
    pub client_version: Option<String>,
    pub player_url: Option<String>,
    pub video_token: bool,
}

fn signed_in_page_config(page_body: &str) -> Option<SignedInPage> {
    const MARKER: &str = "ytcfg.set(";

    let mut merged_page = SignedInPage::default();
    let mut found_config = false;
    let mut client_version_seen = false;
    let mut player_url_seen = false;
    for (config_offset, _) in page_body.match_indices(MARKER) {
        let mut config_deserializer =
            serde_json::Deserializer::from_str(&page_body[config_offset + MARKER.len()..]);
        let Ok(config_object) = Value::deserialize(&mut config_deserializer) else {
            continue;
        };
        found_config = true;
        merged_page.auth.merge_config(&config_object);
        match config_object.get("INNERTUBE_CLIENT_VERSION") {
            Some(Value::String(client_version)) => {
                client_version_seen = true;
                merged_page.client_version = Some(client_version.clone());
            }
            Some(Value::Null) => {
                client_version_seen = true;
                merged_page.client_version = None;
            }
            _ => {}
        }
        match config_object.get("PLAYER_JS_URL") {
            Some(Value::String(player_url)) => {
                player_url_seen = true;
                merged_page.player_url = absolute_player_url(player_url);
            }
            Some(Value::Null) => {
                player_url_seen = true;
                merged_page.player_url = None;
            }
            _ => {}
        }
        if let Some(player_context_configs) = config_object.get("WEB_PLAYER_CONTEXT_CONFIGS") {
            merged_page.video_token = player_context_configs.as_object().is_some_and(|context_map| {
                context_map.values().any(|player_context| {
                    player_context
                        .get("serializedExperimentFlags")
                        .and_then(Value::as_str)
                        .is_some_and(|experiment_flags| {
                            url::form_urlencoded::parse(experiment_flags.as_bytes()).any(
                                |(flag_name, flag_value)| {
                                    flag_name == "html5_generate_content_po_token" && flag_value == "true"
                                },
                            )
                        })
                })
            });
        }
    }
    if !client_version_seen {
        merged_page.client_version = scrape(page_body, "INNERTUBE_CLIENT_VERSION").map(str::to_owned);
    }
    if !player_url_seen {
        merged_page.player_url = scrape(page_body, "PLAYER_JS_URL").and_then(absolute_player_url);
    }
    found_config.then_some(merged_page)
}

fn absolute_player_url(configured_url: &str) -> Option<String> {
    let decoded_url = configured_url.replace("\\/", "/");
    let decoded_url = decoded_url.as_str();
    if decoded_url.starts_with("https://") {
        Some(decoded_url.to_owned())
    } else if decoded_url.starts_with("//") {
        Some(format!("https:{decoded_url}"))
    } else if decoded_url.starts_with('/') {
        Some(format!("https://www.youtube.com{decoded_url}"))
    } else {
        None
    }
}

/// Reads the token policy from the authenticated page because player responses
/// omit the experiment flag that changes the GVS token binding.
pub async fn signed_in_page(auth_context: &AuthContext) -> Result<SignedInPage> {
    let request_builder = auth_context.page_request(crate::unix_now())?;
    let page_response = request_builder.send().await.map_err(|error| {
        crate::trace::record_runtime_evidence(
            "token.binding_page",
            Duration::ZERO,
            "source=account-page result=transport-error",
        );
        failed(
            FailureKind::Transport,
            "account page request",
            safe_reqwest_error(&error),
        )
    })?;
    let http_status = page_response.status();
    if !http_status.is_success() {
        let evidence = format!(
            "source=account-page result=http-error status={}",
            http_status.as_u16()
        );
        crate::trace::record_runtime_evidence("token.binding_page", Duration::ZERO, &evidence);
        let kind = match http_status.as_u16() {
            401 | 403 => FailureKind::AuthenticationRejected,
            _ => FailureKind::UnknownResponse,
        };
        return Err(failed(
            kind,
            "account page request",
            format!("HTTP {}", http_status.as_u16()),
        ));
    }
    let page_body = page_response.text().await.map_err(|error| {
        crate::trace::record_runtime_evidence(
            "token.binding_page",
            Duration::ZERO,
            "source=account-page result=body-error",
        );
        failed(
            FailureKind::Transport,
            "account page body",
            safe_reqwest_error(&error),
        )
    })?;
    let signed_in_page = signed_in_page_config(&page_body).ok_or_else(|| {
        crate::trace::record_runtime_evidence(
            "token.binding_page",
            Duration::ZERO,
            "source=account-page result=context-missing",
        );
        failed(FailureKind::UnknownResponse, "account page", "no player context")
    })?;
    let evidence = format!(
        "source=account-page binding={} video_token={}",
        signed_in_page.auth.data_sync_id().is_some(),
        signed_in_page.video_token,
    );
    crate::trace::record_runtime_evidence("token.binding_page", Duration::ZERO, &evidence);
    Ok(signed_in_page)
}

/// Values YouTube moves on its own schedule, kept on disk so a launch pays for
/// the player request and nothing else. Twelve hours matches how often the
/// client version and the player build actually change.
const SITE_LIFETIME: u64 = 12 * 3600;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Stamped {
    value: String,
    fetched: u64,
}

/// Values learned from YouTube that belong to one resolver cache directory.
pub(crate) struct SiteState {
    cache_path: PathBuf,
    cached_values: Mutex<HashMap<String, Stamped>>,
    visitor_id_cache: tokio::sync::Mutex<Option<String>>,
}

impl SiteState {
    pub fn new(cache_dir: &Path) -> SiteState {
        let cache_path = cache_dir.join("innertube.json");
        let _ = super::protect_private_file(&cache_path);
        let cached_values = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        SiteState {
            cache_path,
            cached_values: Mutex::new(cached_values),
            visitor_id_cache: tokio::sync::Mutex::new(None),
        }
    }

    pub fn cached_value(&self, key: &str) -> Option<String> {
        let unix_seconds = crate::unix_now();
        let cached_values = self
            .cached_values
            .lock()
            .expect("the site lock is never held across an await");
        cached_values
            .get(key)
            .filter(|cached_value| unix_seconds.saturating_sub(cached_value.fetched) < SITE_LIFETIME)
            .map(|cached_value| cached_value.value.clone())
    }

    pub fn store_value(&self, key: &str, value: &str) {
        let cache_snapshot = {
            let mut cached_values = self
                .cached_values
                .lock()
                .expect("the site lock is never held across an await");
            cached_values.insert(
                key.to_owned(),
                Stamped {
                    value: value.to_owned(),
                    fetched: crate::unix_now(),
                },
            );
            cached_values.clone()
        };
        if let Ok(serialized_cache) = serde_json::to_vec(&cache_snapshot) {
            // The file names the session, so it is readable by this user only.
            let _ = super::write_private(&self.cache_path, &serialized_cache);
        }
    }

    /// The version a request claims. Disk state avoids an extra network request.
    pub fn music_version(&self) -> String {
        self.cached_value("web_remix_version")
            .unwrap_or_else(|| WEB_REMIX_VERSION.to_owned())
    }

    pub async fn visitor_id(&self) -> Option<String> {
        let mut visitor_id_cache = self.visitor_id_cache.lock().await;
        if let Some(visitor_id) = visitor_id_cache.as_ref() {
            return Some(visitor_id.clone());
        }
        if let Some(cached_visitor_id) = self.cached_value("visitor_id") {
            *visitor_id_cache = Some(cached_visitor_id.clone());
            return Some(cached_visitor_id);
        }
        let youtube_homepage = crate::trace::timed(
            "visitor id",
            page("https://www.youtube.com/", VISIONOS.user_agent),
        )
        .await
        .ok()?;
        let visitor_id = scrape(&youtube_homepage, "VISITOR_DATA")?.to_owned();
        self.store_value("visitor_id", &visitor_id);
        *visitor_id_cache = Some(visitor_id.clone());
        Some(visitor_id)
    }
}

/// Reads the version the live page advertises after stale context evidence.
pub async fn refresh_music_version() -> Option<String> {
    let music_homepage = crate::trace::timed(
        "client version",
        page("https://music.youtube.com/", WEB_REMIX.user_agent),
    )
    .await
    .ok()?;
    scrape(&music_homepage, "INNERTUBE_CLIENT_VERSION").map(str::to_owned)
}

/// The anonymous identity is refused without one. It names the session, so the
/// file it lives in is written for this user only.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_values_are_isolated_by_owner_and_cache_directory() {
        let root = std::env::temp_dir().join(format!("redwax-site-state-{}", std::process::id()));
        let first_dir = root.join("first");
        let second_dir = root.join("second");
        let _ = std::fs::remove_dir_all(&root);
        let first = SiteState::new(&first_dir);
        let second = SiteState::new(&second_dir);

        first.store_value("player_id", "first-player");

        assert_eq!(first.cached_value("player_id").as_deref(), Some("first-player"));
        assert!(second.cached_value("player_id").is_none());
        assert_eq!(
            SiteState::new(&first_dir).cached_value("player_id").as_deref(),
            Some("first-player")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    const HIGH: &str = include_str!("../../tests/fixtures/player_web_remix_high.json");
    const NO_HIGH: &str = include_str!("../../tests/fixtures/player_web_remix_no_high.json");

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture is valid JSON")
    }

    #[test]
    fn tier_picking_prefers_the_premium_opus_encode() {
        let format = best_audio(&parse(HIGH)).expect("the high fixture lists audio formats");
        assert_eq!(format.tier, Tier::Opus283);
        assert!(format.cipher.is_some(), "the signed in response ciphers its URLs");
    }

    #[test]
    fn player_request_carries_the_selected_scripts_signature_timestamp() {
        let body = player_body(&PlayerRequest {
            identity: &WEB_REMIX,
            client_version: "1.2.3",
            video_id: "video",
            auth: None,
            visitor_id: None,
            signature_timestamp: Some(20_260_929),
            po_token: None,
        });
        assert_eq!(
            body.pointer("/playbackContext/contentPlaybackContext/signatureTimestamp"),
            Some(&json!(20_260_929))
        );
    }

    #[test]
    fn signed_in_request_uses_one_live_identity_in_headers_and_body() {
        let page = signed_in_page_config(
            r#"
            ytcfg.set({"SESSION_INDEX": "2", "DATASYNC_ID": "fixture-channel||fixture-user",
                "VISITOR_DATA": "old-visitor", "LOGGED_IN": true});
            ytcfg.set({"VISITOR_DATA": null,
                "INNERTUBE_CONTEXT": {"client": {"visitorData": "fixture-visitor"}}});
        "#,
        )
        .unwrap();
        let cookies = redwax_auth::CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfixture-sid\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\tfixture-login\n\
             www.youtube.com\tFALSE\t/\tTRUE\t0\tSAPISID\twrong-host\n",
        );
        let auth = AuthContext::new(cookies, Some(2), Some("fixture-channel"), 100)
            .unwrap()
            .with_page(&page.auth)
            .unwrap();
        let request = player_request(
            &PlayerRequest {
                identity: &WEB_REMIX,
                client_version: "1.2.3",
                video_id: "video",
                auth: Some(&auth),
                visitor_id: Some("wrong-anonymous-visitor"),
                signature_timestamp: Some(123),
                po_token: None,
            },
            100,
        )
        .unwrap()
        .build()
        .unwrap();
        let headers = request.headers();
        assert_eq!(request.url().host_str(), Some("music.youtube.com"));
        assert_eq!(headers["origin"], headers["x-origin"]);
        assert_eq!(headers["x-goog-authuser"], "2");
        assert_eq!(headers["x-goog-pageid"], "fixture-channel");
        assert_eq!(headers["x-goog-visitor-id"], "fixture-visitor");
        assert_eq!(headers["cookie"], "SAPISID=fixture-sid; LOGIN_INFO=fixture-login");
        assert!(headers["authorization"].to_str().unwrap().ends_with("_u"));
        let body: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body.pointer("/context/client/visitorData"),
            Some(&json!("fixture-visitor"))
        );
        assert_eq!(
            body.pointer("/playbackContext/contentPlaybackContext/signatureTimestamp"),
            Some(&json!(123))
        );
    }

    #[test]
    fn authenticated_context_cannot_be_sent_to_another_player_origin() {
        let cookies = redwax_auth::CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfixture-sid\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\tfixture-login\n",
        );
        let auth = AuthContext::new(cookies, None, None, 100).unwrap();
        let request = PlayerRequest {
            identity: &VISIONOS,
            client_version: "1.2.3",
            video_id: "video",
            auth: Some(&auth),
            visitor_id: None,
            signature_timestamp: None,
            po_token: None,
        };
        assert!(player_request(&request, 100).is_err());
    }

    #[test]
    fn anonymous_request_keeps_its_visitor_without_account_headers() {
        let request = player_request(
            &PlayerRequest {
                identity: &VISIONOS,
                client_version: "1.2.3",
                video_id: "video",
                auth: None,
                visitor_id: Some("fixture-visitor"),
                signature_timestamp: None,
                po_token: None,
            },
            100,
        )
        .unwrap()
        .build()
        .unwrap();
        let headers = request.headers();
        assert_eq!(headers["x-goog-visitor-id"], "fixture-visitor");
        for name in [
            "cookie",
            "authorization",
            "x-origin",
            "x-goog-authuser",
            "x-goog-pageid",
        ] {
            assert!(!headers.contains_key(name));
        }
        let body: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body.pointer("/context/client/visitorData"),
            Some(&json!("fixture-visitor"))
        );
    }

    #[test]
    fn tier_picking_falls_to_standard_opus_when_no_high_encode_exists() {
        let format = best_audio(&parse(NO_HIGH)).expect("the fixture lists audio formats");
        assert_eq!(format.tier, Tier::Opus160);
    }

    #[test]
    fn both_fixtures_are_playable() {
        assert!(playability(&parse(HIGH), "fixture").is_ok());
        assert!(playability(&parse(NO_HIGH), "fixture").is_ok());
    }

    #[test]
    fn an_unplayable_response_names_its_status() {
        let response = parse(r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in"}}"#);
        let message = playability(&response, "abc").unwrap_err().to_string();
        assert!(message.contains("LOGIN_REQUIRED"), "{message}");
        assert!(message.contains("abc"), "{message}");
    }

    #[test]
    fn client_version_retry_excludes_clear_permanent_statuses() {
        for status in ["LOGIN_REQUIRED", "AGE_CHECK_REQUIRED", "CONTENT_CHECK_REQUIRED"] {
            let response = json!({"playabilityStatus": {"status": status}});
            assert!(!can_refresh_client_version(&response), "{status}");
        }
        for status in ["ERROR", "UNPLAYABLE"] {
            let response = json!({"playabilityStatus": {"status": status}});
            assert!(can_refresh_client_version(&response), "{status}");
        }
        assert!(can_refresh_client_version(&json!({})));
    }

    #[test]
    fn content_range_must_stay_within_the_probe_budget() {
        assert_eq!(initial_range_length("bytes 0-1023/9999"), Some(1024));
        assert_eq!(initial_range_length("bytes 0-0/9999"), Some(1));
        assert_eq!(initial_range_length("bytes 1-1024/9999"), None);
        assert_eq!(initial_range_length("bytes 0-1024/9999"), None);
        assert_eq!(initial_range_length("invalid"), None);
    }

    #[tokio::test]
    async fn stream_check_rejects_urls_that_only_allow_small_ranges() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            let response = match request.contains("\r\nrange:") {
                true => {
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/9999\r\nContent-Length: 4\r\nConnection: close\r\n\r\nOggS"
                }
                false => "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            };
            socket.write_all(response.as_bytes()).unwrap();
        });

        assert_eq!(
            verify(&format!("http://{address}/audio"), "test-agent")
                .await
                .unwrap(),
            403
        );
        server.join().unwrap();
    }

    #[test]
    fn response_evidence_keeps_shape_and_drops_values() {
        let response = parse(
            r#"{"playabilityStatus":{"status":"OK"},
               "responseContext":{"mainAppWebResponseContext":{"datasyncId":"private-binding"}},
               "streamingData":{"adaptiveFormats":[
                 {"itag":774,"mimeType":"audio/webm","signatureCipher":"url=secret&sig=private"},
                 {"itag":251,"mimeType":"audio/webm","url":"https://secret.invalid/?n=private"},
                 {"itag":18,"mimeType":"video/mp4","url":"https://video.invalid/"}
               ]}}"#,
        );
        let evidence = player_response_evidence(&response, "1.2.3", false);
        assert_eq!(
            evidence,
            "client=web-remix version=1.2.3 token=false status=OK streaming=true audio=2 urls=1 ciphers=1 itags=774,251 binding=true"
        );
        assert!(!evidence.contains("private"));
        assert!(!evidence.contains("secret"));
    }

    #[test]
    fn signed_in_page_reads_the_video_token_experiment() {
        let page = r#"ytcfg.set({"DATASYNC_ID":"delegated||user",
            "PLAYER_JS_URL":"/s/player/f572e43c/player_ias.vflset/en_US/base.js",
            "WEB_PLAYER_CONTEXT_CONFIGS":{
            "WEB_PLAYER_CONTEXT_CONFIG_ID_KEVLAR_WATCH":{"serializedExperimentFlags":
            "other=true&html5_generate_content_po_token=true"}}});"#;
        let config = signed_in_page_config(page).expect("the player config parses");
        assert_eq!(config.auth.data_sync_id(), Some("delegated||user"));
        assert_eq!(
            config.player_url.as_deref(),
            Some("https://www.youtube.com/s/player/f572e43c/player_ias.vflset/en_US/base.js")
        );
        assert!(config.video_token);

        let page = page.replace(
            "html5_generate_content_po_token=true",
            "html5_generate_content_po_token=false",
        );
        assert!(!signed_in_page_config(&page).unwrap().video_token);
    }

    #[test]
    fn player_script_urls_must_resolve_to_https() {
        assert_eq!(
            absolute_player_url("//www.youtube.com/s/player/id/base.js").as_deref(),
            Some("https://www.youtube.com/s/player/id/base.js")
        );
        assert_eq!(
            absolute_player_url(r#"\/s\/player\/id\/base.js"#).as_deref(),
            Some("https://www.youtube.com/s/player/id/base.js")
        );
        assert!(absolute_player_url("http://example.invalid/base.js").is_none());
        assert!(absolute_player_url("relative/base.js").is_none());
    }

    #[test]
    fn signed_in_page_merges_config_blocks_with_last_policy_winning() {
        let page = r#"
            ytcfg.set({"DATASYNC_ID":"account","INNERTUBE_CLIENT_VERSION":"1.2.3"});
            ytcfg.set({"WEB_PLAYER_CONTEXT_CONFIGS":{"WATCH":{
                "serializedExperimentFlags":"html5_generate_content_po_token=true"}}});
            ytcfg.set({"WEB_PLAYER_CONTEXT_CONFIGS":{"WATCH":{
                "serializedExperimentFlags":"html5_generate_content_po_token=false"}}});
        "#;
        let config = signed_in_page_config(page).expect("at least one config block parses");
        assert_eq!(config.auth.data_sync_id(), Some("account"));
        assert_eq!(config.client_version.as_deref(), Some("1.2.3"));
        assert!(!config.video_token);
    }

    #[test]
    fn signed_in_page_nulls_clear_earlier_config_values() {
        let page = r#"
            ytcfg.set({"DATASYNC_ID":"account","INNERTUBE_CLIENT_VERSION":"1.2.3",
                "WEB_PLAYER_CONTEXT_CONFIGS":{"WATCH":{"serializedExperimentFlags":
                "html5_generate_content_po_token=true"}}});
            ytcfg.set({"DATASYNC_ID":null,"INNERTUBE_CLIENT_VERSION":null,
                "WEB_PLAYER_CONTEXT_CONFIGS":null});
        "#;
        let config = signed_in_page_config(page).expect("both config blocks parse");
        assert!(config.auth.data_sync_id().is_none());
        assert!(config.client_version.is_none());
        assert!(!config.video_token);
    }

    #[test]
    fn signed_in_page_accepts_legacy_config_without_player_contexts() {
        let config = signed_in_page_config(r#"ytcfg.set({"DATASYNC_ID":"account"});"#)
            .expect("the account config parses");
        assert_eq!(config.auth.data_sync_id(), Some("account"));
        assert!(!config.video_token);
    }

    #[test]
    fn tier_picking_skips_a_higher_format_without_a_delivery_url() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":774,"mimeType":"audio/webm",
                    "signatureCipher":"s=challenge&url=not-a-url"},
                {"itag":251,"mimeType":"audio/webm","url":"https://example.invalid/audio"}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the lower playable format survives");
        assert_eq!(format.tier, Tier::Opus160);
    }

    #[test]
    fn original_audio_precedes_a_higher_quality_dub() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":774,"mimeType":"audio/webm","bitrate":180000,
                    "url":"https://example.invalid/bangla?xtags=acont%3Ddubbed-auto%3Alang%3Dbn",
                    "audioTrack":{"id":"bn.10","displayName":"Bangla",
                        "audioIsDefault":false,"isAutoDubbed":true}},
                {"itag":774,"mimeType":"audio/webm","bitrate":170000,
                    "url":"https://example.invalid/hindi?xtags=acont%3Ddubbed-auto%3Alang%3Dhi",
                    "audioTrack":{"id":"hi.10","displayName":"Hindi",
                        "audioIsDefault":false,"isAutoDubbed":true}},
                {"itag":140,"mimeType":"audio/mp4","bitrate":120000,
                    "url":"https://example.invalid/original?xtags=acont%3Doriginal%3Alang%3Den-US",
                    "audioTrack":{"id":"en-US.4","displayName":"English (US) original",
                        "audioIsDefault":true}}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the original format is playable");
        assert_eq!(format.tier, Tier::Aac128);
    }

    #[test]
    fn delivery_tags_identify_tracks_without_audio_track_metadata() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":251,"mimeType":"audio/webm",
                    "url":"https://example.invalid/dub?xtags=acont%3Ddubbed-auto%3Alang%3Dbn"},
                {"itag":140,"mimeType":"audio/mp4",
                    "url":"https://example.invalid/original?xtags=acont%3Doriginal%3Alang%3Den"}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the tagged original is playable");
        assert_eq!(format.tier, Tier::Aac128);
    }

    #[test]
    fn track_marker_on_an_unusable_variant_applies_to_its_other_encodes() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":774,"mimeType":"audio/webm","url":"not-a-url",
                    "audioTrack":{"id":"en.0","displayName":"English original"}},
                {"itag":140,"mimeType":"audio/mp4","url":"https://example.invalid/original",
                    "audioTrack":{"id":"en.1"}},
                {"itag":251,"mimeType":"audio/webm","url":"https://example.invalid/dub",
                    "audioTrack":{"id":"bn.0","displayName":"Bengali"}}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the lower original encode is playable");
        assert_eq!(format.tier, Tier::Aac128);
    }

    #[test]
    fn default_audio_precedes_a_higher_quality_alternate() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":774,"mimeType":"audio/webm","url":"https://example.invalid/alternate",
                    "audioTrack":{"id":"hi.0","displayName":"Hindi",
                        "audioIsDefault":true,"isAutoDubbed":true}},
                {"itag":140,"mimeType":"audio/mp4","url":"https://example.invalid/default",
                    "audioTrack":{"id":"en.0","displayName":"English","audioIsDefault":true}}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the default format is playable");
        assert_eq!(format.tier, Tier::Aac128);
    }

    #[test]
    fn explicit_dub_marker_overrides_inferred_original_language() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":251,"mimeType":"audio/webm","url":"https://example.invalid/dub",
                    "audioTrack":{"id":"en-US.10","displayName":"English (US)",
                        "isAutoDubbed":true}},
                {"itag":140,"mimeType":"audio/mp4","url":"https://example.invalid/original",
                    "audioTrack":{"id":"en-US.4","displayName":"English (US) original",
                        "audioIsDefault":true}}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the same-language original is playable");
        assert_eq!(format.tier, Tier::Aac128);
    }

    #[test]
    fn descriptive_audio_stays_below_a_normal_original_track() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":774,"mimeType":"audio/webm","url":"https://example.invalid/described",
                    "audioTrack":{"id":"en-US-desc.4",
                        "displayName":"English (US) original descriptive"}},
                {"itag":140,"mimeType":"audio/mp4","url":"https://example.invalid/original",
                    "audioTrack":{"id":"en-US.4","displayName":"English (US) original",
                        "audioIsDefault":true}}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the normal original is playable");
        assert_eq!(format.tier, Tier::Aac128);
    }

    #[test]
    fn original_marker_inside_a_cipher_precedes_a_plain_dub() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":251,"mimeType":"audio/webm",
                    "url":"https://example.invalid/dub?xtags=acont%3Ddubbed-auto%3Alang%3Dhi"},
                {"itag":140,"mimeType":"audio/mp4",
                    "signatureCipher":"s=challenge&sp=sig&url=https%3A%2F%2Fexample.invalid%2Foriginal%3Fxtags%3Dacont%253Doriginal%253Alang%253Den"}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the ciphered original format is playable");
        assert_eq!(format.tier, Tier::Aac128);
        assert!(format.cipher.is_some());
    }

    #[test]
    fn formats_without_track_metadata_keep_the_quality_order() {
        let response = parse(
            r#"{"streamingData":{"adaptiveFormats":[
                {"itag":140,"mimeType":"audio/mp4","url":"https://example.invalid/standard"},
                {"itag":774,"mimeType":"audio/webm","url":"https://example.invalid/high"}
            ]}}"#,
        );
        let format = best_audio(&response).expect("the metadata-free formats are playable");
        assert_eq!(format.tier, Tier::Opus283);
    }

    #[test]
    fn cipher_url_assembly_appends_the_signature_and_swaps_n() {
        let format = best_audio(&parse(HIGH)).expect("the high fixture lists audio formats");
        let cipher = parse_cipher(format.cipher.as_deref().unwrap()).expect("fixture cipher parses");
        assert!(!cipher.s.is_empty());
        let before = query_value(&cipher.url, "n").expect("a player URL always carries n");
        let url = assemble(&format, Some("SOLVED-SIG"), Some("SOLVED-N")).expect("assembly succeeds");
        assert!(url.contains(&format!("&{}=SOLVED-SIG", cipher.sp)), "{url}");
        assert_eq!(query_value(&url, "n").as_deref(), Some("SOLVED-N"));
        assert!(
            !url.contains(&format!("n={before}")),
            "the throttling n must be gone"
        );
    }

    #[test]
    fn assembly_of_a_plain_url_needs_no_signature() {
        let format = AudioFormat {
            tier: Tier::Opus160,
            url: Some("https://example.invalid/videoplayback?itag=251&expire=1".to_owned()),
            cipher: None,
        };
        assert_eq!(
            assemble(&format, None, None).as_deref(),
            Some("https://example.invalid/videoplayback?itag=251&expire=1")
        );
    }

    #[test]
    fn scrubbing_drops_the_account_fields_and_the_ip_parameter() {
        let mut value = parse(
            r#"{"responseContext":{"visitorData":"x"},"streamingData":{"adaptiveFormats":[
               {"url":"https://host/p?itag=251&ip=192.0.2.1&fvip=1"},
               {"url":"https://host/p?ip=192.0.2.1"},
               {"url":"https://host/p?a=1&clientip=9&ip=198.51.100.2"}]}}"#,
        );
        scrub(&mut value);
        assert!(value.get("responseContext").is_none());
        let urls = value
            .pointer("/streamingData/adaptiveFormats")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(urls[0]["url"], "https://host/p?itag=251&fvip=1");
        assert_eq!(urls[1]["url"], "https://host/p");
        assert_eq!(urls[2]["url"], "https://host/p?a=1&clientip=9");
    }

    #[test]
    fn a_replaced_query_value_leaves_every_other_segment_alone() {
        let url = "https://host/path?a=1&n=old&b=%2Fkeep%2F";
        assert_eq!(
            with_query_value(url, "n", "new"),
            "https://host/path?a=1&n=new&b=%2Fkeep%2F"
        );
    }
}
