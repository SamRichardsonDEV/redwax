//! Proof of origin tokens, from the bgutil provider yt-dlp already uses.
//!
//! YouTube refuses a stream URL with HTTP 403 when it wants one, so a token is
//! fetched only after a refusal, never on the happy path. The caller selects
//! the binding from YouTube's current player context before minting a token.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// The provider can shorten this lifetime, but cannot extend it.
const TOKEN_LIFETIME: Duration = Duration::from_secs(6 * 3600);

/// Avoid a token expiring between cache lookup and the CDN request.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// The provider signs a challenge in a browser engine, which is slow but not
/// this slow. Past this the token is not worth the wait.
const TOKEN_REQUEST_BUDGET: Duration = Duration::from_secs(20);

/// Both bindings must get a chance before the resolver's overall deadline.
const TOKEN_BATCH_BUDGET: Duration = Duration::from_millis(4_500);

#[derive(Clone)]
pub struct Provider {
    pub node: PathBuf,
    pub server_home: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    node_binary_path: PathBuf,
    provider_home: PathBuf,
    content_binding: String,
}

#[derive(Debug, Clone)]
struct CachedToken {
    proof_token: String,
    expires_at_instant: Instant,
}

fn cache() -> &'static Mutex<HashMap<CacheKey, CachedToken>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, CachedToken>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn flights() -> &'static Mutex<HashMap<CacheKey, Weak<tokio::sync::Mutex<()>>>> {
    static FLIGHTS: OnceLock<Mutex<HashMap<CacheKey, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Provider {
    /// `REDWAX_POT_PROVIDER` overrides the optional managed provider installation.
    /// `None` when the provider is not built, which is not an error.
    pub fn discover(node_binary_path: PathBuf) -> Option<Provider> {
        let provider_home = std::env::var("REDWAX_POT_PROVIDER")
            .map(PathBuf::from)
            .unwrap_or_else(|_| super::solver::home().join(".local/share/redwax/bgutil-pot-provider/server"));
        provider_home
            .join("build/generate_once.js")
            .is_file()
            .then_some(Provider {
                node: node_binary_path,
                server_home: provider_home,
            })
    }

    pub async fn token(&self, content_binding: &str) -> std::result::Result<String, String> {
        let cache_key = self.cache_key(content_binding);
        if let Some(proof_token) = self.cached(&cache_key) {
            return Ok(proof_token);
        }
        let flight_lock = self.flight(&cache_key);
        let _flight_guard = flight_lock.lock().await;
        if let Some(proof_token) = self.cached(&cache_key) {
            return Ok(proof_token);
        }
        self.request_token(cache_key, false).await
    }

    /// Mints a replacement after the CDN rejects the token returned by `token`.
    pub async fn refreshed_token(&self, content_binding: &str) -> std::result::Result<String, String> {
        let cache_key = self.cache_key(content_binding);
        let rejected_token = self.cached(&cache_key);
        let flight_lock = self.flight(&cache_key);
        let _flight_guard = flight_lock.lock().await;
        if let Some(current_token) = self.cached(&cache_key)
            && rejected_token.as_deref() != Some(current_token.as_str())
        {
            return Ok(current_token);
        }
        cache()
            .lock()
            .expect("the token lock is never held across an await")
            .remove(&cache_key);
        self.request_token(cache_key, true).await
    }

    /// Mints every candidate concurrently and returns results in input order.
    /// Unfinished provider processes are killed when the shared budget expires.
    pub async fn tokens(
        &self,
        content_bindings: &[String],
        refresh_requested: bool,
    ) -> Vec<std::result::Result<String, String>> {
        let mut token_tasks = tokio::task::JoinSet::new();
        for (binding_index, content_binding) in content_bindings.iter().cloned().enumerate() {
            let provider = self.clone();
            token_tasks.spawn(async move {
                let token_result = if refresh_requested {
                    provider.refreshed_token(&content_binding).await
                } else {
                    provider.token(&content_binding).await
                };
                (binding_index, token_result)
            });
        }

        let mut token_results: Vec<Option<std::result::Result<String, String>>> =
            std::iter::repeat_with(|| None)
                .take(content_bindings.len())
                .collect();
        let batch_completed = tokio::time::timeout(TOKEN_BATCH_BUDGET, async {
            while let Some(join_result) = token_tasks.join_next().await {
                if let Ok((binding_index, token_result)) = join_result {
                    token_results[binding_index] = Some(token_result);
                }
            }
        })
        .await
        .is_ok();
        if !batch_completed {
            token_tasks.shutdown().await;
        }
        token_results
            .into_iter()
            .map(|token_result| {
                token_result.unwrap_or_else(|| Err("token batch did not answer in time".to_owned()))
            })
            .collect()
    }

    async fn request_token(
        &self,
        cache_key: CacheKey,
        bypass_provider_cache: bool,
    ) -> std::result::Result<String, String> {
        let provider_process = tokio::process::Command::new(&self.node)
            .arg(self.server_home.join("build/generate_once.js"))
            .arg("-c")
            .arg(&cache_key.content_binding)
            .args(bypass_provider_cache.then_some("--bypass-cache"))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let process_output = tokio::time::timeout(TOKEN_REQUEST_BUDGET, provider_process)
            .await
            .map_err(|_| "token provider did not answer in time".to_owned())?
            .map_err(|error| format!("token provider: {error}"))?;
        if !process_output.status.success() {
            return Err(format!("token provider exited {}", process_output.status));
        }
        let standard_output = String::from_utf8_lossy(&process_output.stdout);
        // The script prints warnings before the reply, so the last line is the one.
        let reply_line = standard_output.lines().next_back().unwrap_or_default();
        let token_reply: Value =
            serde_json::from_str(reply_line).map_err(|error| format!("token reply: {error}"))?;
        let proof_token = token_reply
            .get("poToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "token reply names no poToken".to_owned())?;
        let token_lifetime = token_lifetime(&token_reply, SystemTime::now())?;
        cache()
            .lock()
            .expect("the token lock is never held across an await")
            .insert(
                cache_key,
                CachedToken {
                    proof_token: proof_token.to_owned(),
                    expires_at_instant: Instant::now() + token_lifetime,
                },
            );
        Ok(proof_token.to_owned())
    }

    fn cache_key(&self, content_binding: &str) -> CacheKey {
        CacheKey {
            node_binary_path: self.node.clone(),
            provider_home: self.server_home.clone(),
            content_binding: content_binding.to_owned(),
        }
    }

    fn flight(&self, cache_key: &CacheKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut active_flights = flights().lock().expect("the flight lock holds no async guards");
        active_flights.retain(|_, flight| flight.strong_count() > 0);
        if let Some(existing_flight) = active_flights.get(cache_key).and_then(Weak::upgrade) {
            return existing_flight;
        }
        let new_flight = Arc::new(tokio::sync::Mutex::new(()));
        active_flights.insert(cache_key.clone(), Arc::downgrade(&new_flight));
        new_flight
    }

    fn cached(&self, cache_key: &CacheKey) -> Option<String> {
        let mut cached_tokens = cache()
            .lock()
            .expect("the token lock is never held across an await");
        cached_tokens.retain(|_, token| token.expires_at_instant > Instant::now());
        cached_tokens
            .get(cache_key)
            .map(|token| token.proof_token.clone())
    }
}

fn token_lifetime(token_reply: &Value, current_time: SystemTime) -> std::result::Result<Duration, String> {
    let Some(expires_at_value) = token_reply.get("expiresAt") else {
        return Ok(TOKEN_LIFETIME);
    };
    let expires_at_text = expires_at_value
        .as_str()
        .ok_or_else(|| "token reply has an invalid expiresAt".to_owned())?;
    let expiration_unix_seconds =
        rfc3339_unix(expires_at_text).ok_or_else(|| "token reply has an invalid expiresAt".to_owned())?;
    let current_unix_seconds = current_time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_owned())?
        .as_secs();
    let remaining_lifetime =
        Duration::from_secs(expiration_unix_seconds.saturating_sub(current_unix_seconds))
            .saturating_sub(EXPIRY_MARGIN);
    if remaining_lifetime.is_zero() {
        return Err("token reply is already expired".to_owned());
    }
    Ok(remaining_lifetime.min(TOKEN_LIFETIME))
}

fn rfc3339_unix(text: &str) -> Option<u64> {
    let (date_text, time_text) = text.strip_suffix('Z')?.split_once('T')?;
    let mut date_parts = date_text.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some()
        || !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
    {
        return None;
    }
    let mut time_parts = time_text.split(':');
    let hour: u32 = time_parts.next()?.parse().ok()?;
    let minute: u32 = time_parts.next()?.parse().ok()?;
    let second_text = time_parts.next()?;
    let (second_text, fractional_seconds) = second_text
        .split_once('.')
        .map_or((second_text, None), |(whole_seconds, fractional_seconds)| {
            (whole_seconds, Some(fractional_seconds))
        });
    if fractional_seconds.is_some_and(|fractional_seconds| {
        fractional_seconds.is_empty() || !fractional_seconds.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return None;
    }
    let second: u32 = second_text.parse().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days_since_unix_epoch = days_from_civil(year, month, day);
    if days_since_unix_epoch < 0 {
        return None;
    }
    u64::try_from(days_since_unix_epoch)
        .ok()?
        .checked_mul(86_400)?
        .checked_add(u64::from(hour * 3600 + minute * 60 + second))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(mut year: i64, month: u32, day: u32) -> i64 {
    year -= if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_expiry_is_honoured_with_a_safety_margin() {
        let now = UNIX_EPOCH + Duration::from_secs(946_684_800);
        let reply = json!({"expiresAt": "2000-01-01T01:00:00.000Z"});
        assert_eq!(token_lifetime(&reply, now), Ok(Duration::from_secs(3570)));
    }

    #[test]
    fn provider_expiry_cannot_extend_the_cache_ceiling() {
        let now = UNIX_EPOCH + Duration::from_secs(946_684_800);
        let reply = json!({"expiresAt": "2000-01-02T01:00:00.000Z"});
        assert_eq!(token_lifetime(&reply, now), Ok(TOKEN_LIFETIME));
    }

    #[test]
    fn invalid_or_expired_provider_expiry_is_rejected() {
        let now = UNIX_EPOCH + Duration::from_secs(946_684_800);
        assert!(token_lifetime(&json!({"expiresAt": 0}), now).is_err());
        assert!(token_lifetime(&json!({"expiresAt": "not-a-date"}), now).is_err());
        assert!(token_lifetime(&json!({"expiresAt": "2000-01-01T00:00:20.000Z"}), now).is_err());
    }

    #[test]
    fn rfc3339_parser_checks_calendar_boundaries() {
        assert_eq!(rfc3339_unix("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(rfc3339_unix("2000-01-01T00:00:00.000Z"), Some(946_684_800));
        assert_eq!(rfc3339_unix("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(rfc3339_unix("2001-02-29T00:00:00Z"), None);
        assert_eq!(rfc3339_unix("2000-01-01T00:00:00.Z"), None);
        assert_eq!(rfc3339_unix("2000-01-01T00:00:00.textZ"), None);
        assert_eq!(rfc3339_unix("10000-01-01T00:00:00Z"), None);
    }

    #[tokio::test]
    #[ignore = "runs the token provider and talks to YouTube"]
    async fn a_token_binds_to_the_session_and_leaves_a_stream_url_playable() {
        let Some(provider) = Provider::discover(super::super::solver::NodeSolver::node_path()) else {
            panic!("install the provider with scripts/install_resolver.sh --with-provider");
        };
        let site = super::super::innertube::SiteState::new(&std::env::temp_dir());
        let binding = site.visitor_id().await.expect("the session page answers");
        let token = provider.token(&binding).await.expect("the provider answers");
        assert!(token.len() > 32, "a token is a long base64url string");
        assert_eq!(
            provider.token(&binding).await.as_deref(),
            Ok(token.as_str()),
            "the token is cached"
        );
    }
}
