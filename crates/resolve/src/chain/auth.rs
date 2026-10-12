//! Cookie scope and account identity must agree with every request signature.

use redwax_auth::CookieJar;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;

use super::innertube;
use crate::{FailureKind, Result};

const ORIGIN: &str = "https://music.youtube.com";
const PLAYER_URL: &str = "https://music.youtube.com/youtubei/v1/player";

#[derive(Clone, Default)]
pub(crate) struct PageAuth {
    data_sync_id: Option<String>,
    session_index: Option<u32>,
    delegated_session_id: Option<String>,
    user_session_id: Option<String>,
    visitor_data: Option<String>,
    context_visitor_data: Option<String>,
    logged_in: Option<bool>,
}

impl PageAuth {
    pub fn merge_config(&mut self, page_config: &Value) {
        for (config_key, destination) in [
            ("DATASYNC_ID", &mut self.data_sync_id),
            ("DELEGATED_SESSION_ID", &mut self.delegated_session_id),
            ("USER_SESSION_ID", &mut self.user_session_id),
            ("VISITOR_DATA", &mut self.visitor_data),
        ] {
            if let Some(config_value) = page_config.get(config_key) {
                *destination = config_value
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
            }
        }
        if let Some(session_index_value) = page_config.get("SESSION_INDEX") {
            self.session_index = session_index_value
                .as_u64()
                .and_then(|index| u32::try_from(index).ok())
                .or_else(|| {
                    session_index_value
                        .as_str()
                        .and_then(|index_text| index_text.parse().ok())
                });
        }
        if let Some(logged_in_value) = page_config.get("LOGGED_IN") {
            self.logged_in = logged_in_value.as_bool();
        }
        if let Some(innertube_context) = page_config.get("INNERTUBE_CONTEXT") {
            self.context_visitor_data = innertube_context
                .pointer("/client/visitorData")
                .and_then(Value::as_str)
                .filter(|visitor_data| !visitor_data.is_empty())
                .map(str::to_owned);
        }
    }

    pub fn data_sync_id(&self) -> Option<&str> {
        self.data_sync_id.as_deref()
    }

    fn sessions(&self) -> (Option<&str>, Option<&str>) {
        let (delegated_session_id, user_session_id) = match self.data_sync_id.as_deref() {
            Some(data_sync_id) => match data_sync_id.split_once("||") {
                Some((delegated_session_id, user_session_id)) if !user_session_id.is_empty() => (
                    (!delegated_session_id.is_empty()).then_some(delegated_session_id),
                    Some(user_session_id),
                ),
                Some((user_session_id, _)) => (None, Some(user_session_id)),
                None => (None, Some(data_sync_id)),
            },
            None => (None, None),
        };
        (
            self.delegated_session_id.as_deref().or(delegated_session_id),
            self.user_session_id.as_deref().or(user_session_id),
        )
    }
}

/// Keeps live account selection and its cookies together while signatures age.
pub(crate) struct AuthContext {
    cookie_jar: CookieJar,
    requested_account: (Option<u32>, Option<String>),
    authuser: Option<u32>,
    page_id: Option<String>,
    user_session_id: Option<String>,
    visitor_data: Option<String>,
    logged_in: bool,
}

impl AuthContext {
    pub fn new(
        cookie_jar: CookieJar,
        authuser: Option<u32>,
        page_id: Option<&str>,
        unix_seconds: u64,
    ) -> Result<Self> {
        validate(&player_cookies(&cookie_jar, unix_seconds))?;
        Ok(Self {
            cookie_jar,
            requested_account: (authuser, page_id.map(str::to_owned)),
            authuser,
            page_id: page_id.map(str::to_owned),
            user_session_id: None,
            visitor_data: None,
            logged_in: false,
        })
    }

    pub fn with_page(mut self, page: &PageAuth) -> Result<Self> {
        if page.logged_in == Some(false) {
            return Err(innertube::failed(
                FailureKind::AuthenticationRejected,
                "authentication",
                "account page is signed out",
            ));
        }
        if self
            .requested_account
            .0
            .zip(page.session_index)
            .is_some_and(|(wanted, actual)| wanted != actual)
        {
            return Err(innertube::failed(
                FailureKind::AuthenticationRejected,
                "authentication",
                "account page selected a different session",
            ));
        }
        let (delegated_session_id, user_session_id) = page.sessions();
        if let Some(requested_page_id) = self.requested_account.1.as_deref()
            && (delegated_session_id.is_some() || page.data_sync_id.is_some())
            && delegated_session_id != Some(requested_page_id)
        {
            return Err(innertube::failed(
                FailureKind::AuthenticationRejected,
                "authentication",
                "account page selected a different channel",
            ));
        }
        self.authuser = page.session_index.or(self.requested_account.0);
        self.page_id = if delegated_session_id.is_some() || page.data_sync_id.is_some() {
            delegated_session_id.map(str::to_owned)
        } else {
            self.requested_account.1.clone()
        };
        self.user_session_id = user_session_id
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_owned);
        self.visitor_data = page
            .visitor_data
            .clone()
            .or_else(|| page.context_visitor_data.clone());
        self.logged_in = page.logged_in == Some(true);
        Ok(self)
    }

    pub fn matches(&self, cookie_jar: &CookieJar, authuser: Option<u32>, page_id: Option<&str>) -> bool {
        &self.cookie_jar == cookie_jar
            && self.requested_account.0 == authuser
            && self.requested_account.1.as_deref() == page_id
    }

    pub fn page_request(&self, unix_seconds: u64) -> Result<reqwest::RequestBuilder> {
        let music_origin_url =
            reqwest::Url::parse(ORIGIN).expect("the authentication origin is a fixed HTTPS URL");
        let cookie_header_text = self
            .cookie_jar
            .for_request(&music_origin_url, unix_seconds)
            .header();
        let mut cookie_header = HeaderValue::from_str(&cookie_header_text).map_err(|_| {
            innertube::failed(
                FailureKind::AuthenticationRejected,
                "authentication",
                "invalid page cookies",
            )
        })?;
        cookie_header.set_sensitive(true);
        let mut request_builder = innertube::http()
            .get(music_origin_url)
            .header("User-Agent", innertube::WEB_REMIX.user_agent)
            .header("Cookie", cookie_header);
        if let Some(authuser) = self.authuser {
            request_builder = request_builder
                .query(&[("authuser", authuser)])
                .header("X-Goog-AuthUser", authuser.to_string());
        }
        if let Some(page_id) = self.page_id.as_deref() {
            let mut page_id_header = HeaderValue::from_str(page_id).map_err(|_| {
                innertube::failed(
                    FailureKind::AuthenticationRejected,
                    "authentication",
                    "invalid channel metadata",
                )
            })?;
            page_id_header.set_sensitive(true);
            request_builder = request_builder
                .query(&[("pageid", page_id)])
                .header("X-Goog-PageId", page_id_header);
        }
        Ok(request_builder)
    }

    pub fn visitor_data(&self) -> Option<&str> {
        self.visitor_data.as_deref()
    }

    pub fn headers(&self, unix_seconds: u64) -> Result<HeaderMap> {
        let player_cookie_jar = player_cookies(&self.cookie_jar, unix_seconds);
        validate(&player_cookie_jar)?;
        let authorization_header = player_cookie_jar
            .authorization(ORIGIN, unix_seconds, self.user_session_id.as_deref())
            .ok_or_else(|| {
                innertube::failed(
                    FailureKind::MissingCredentials,
                    "authentication",
                    "no signing cookie",
                )
            })?;
        let evidence = format!(
            "schemes={} session_bound={} delegated={} visitor={} bootstrap_logged_in={}",
            authorization_header.split_whitespace().count() / 2,
            self.user_session_id.is_some(),
            self.page_id.is_some(),
            self.visitor_data.is_some(),
            self.logged_in,
        );
        crate::trace::record_runtime_evidence("native.auth", std::time::Duration::ZERO, &evidence);
        let mut request_headers = HeaderMap::new();
        for (header_name, header_text) in [
            ("cookie", Some(player_cookie_jar.header())),
            ("authorization", Some(authorization_header)),
            ("x-origin", Some(ORIGIN.to_owned())),
            (
                "x-goog-authuser",
                self.authuser
                    .or_else(|| self.page_id.as_ref().map(|_| 0))
                    .map(|index| index.to_string()),
            ),
            ("x-goog-pageid", self.page_id.clone()),
            ("x-goog-visitor-id", self.visitor_data.clone()),
            (
                "x-youtube-bootstrap-logged-in",
                self.logged_in.then(|| "true".to_owned()),
            ),
        ] {
            if let Some(header_text) = header_text {
                let mut header_value = HeaderValue::from_str(&header_text).map_err(|_| {
                    innertube::failed(
                        FailureKind::AuthenticationRejected,
                        "authentication",
                        "invalid request metadata",
                    )
                })?;
                header_value.set_sensitive(true);
                request_headers.insert(header_name, header_value);
            }
        }
        Ok(request_headers)
    }
}

fn player_cookies(cookie_jar: &CookieJar, unix_seconds: u64) -> CookieJar {
    let player_url = reqwest::Url::parse(PLAYER_URL).expect("the player endpoint is a fixed HTTPS URL");
    cookie_jar.for_request(&player_url, unix_seconds)
}

fn validate(cookie_jar: &CookieJar) -> Result<()> {
    if cookie_jar.authenticated() {
        Ok(())
    } else {
        Err(innertube::failed(
            FailureKind::MissingCredentials,
            "cookies",
            "no valid login and signing cookies, run the login again",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOKIES: &str = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfixture-sid\n\
        .youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\tfixture-login\n\
        .youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-1PAPISID\tfixture-first\n\
        .youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-3PAPISID\tfixture-third\n";

    fn context(authuser: Option<u32>, page_id: Option<&str>) -> AuthContext {
        AuthContext::new(CookieJar::from_netscape(COOKIES), authuser, page_id, 100)
            .expect("the synthetic cookies contain login and signing cookies")
    }

    fn page(value: Value) -> PageAuth {
        let mut page = PageAuth::default();
        page.merge_config(&value);
        page
    }

    #[test]
    fn live_delegated_identity_binds_headers_and_signatures() {
        let auth = context(Some(2), Some("fixture-channel"))
            .with_page(&page(serde_json::json!({
                "SESSION_INDEX": "2", "DATASYNC_ID": "fixture-channel||fixture-user",
                "VISITOR_DATA": "fixture-visitor", "LOGGED_IN": true
            })))
            .expect("the live account matches the requested account");
        let headers = auth.headers(100).expect("synthetic header values are valid");
        assert_eq!(headers["x-goog-authuser"], "2");
        assert_eq!(headers["x-goog-pageid"], "fixture-channel");
        assert_eq!(headers["x-goog-visitor-id"], "fixture-visitor");
        assert_eq!(headers["x-youtube-bootstrap-logged-in"], "true");
        let authorization = headers["authorization"].to_str().unwrap();
        assert_eq!(authorization.matches("_u").count(), 3);
        assert_eq!(
            authorization,
            CookieJar::from_netscape(COOKIES)
                .authorization(ORIGIN, 100, Some("fixture-user"))
                .unwrap()
        );
        assert!(!format!("{headers:?}").contains("fixture"));
    }

    #[test]
    fn primary_account_has_no_delegated_header() {
        let auth = context(None, None)
            .with_page(&page(
                serde_json::json!({"DATASYNC_ID": "fixture-user||", "SESSION_INDEX": 0}),
            ))
            .unwrap();
        let headers = auth.headers(100).unwrap();
        assert!(!headers.contains_key("x-goog-pageid"));
        assert!(headers["authorization"].to_str().unwrap().contains("_u"));
    }

    #[test]
    fn live_selection_is_used_when_saved_identity_is_absent() {
        let auth = context(None, None)
            .with_page(&page(serde_json::json!({
                "SESSION_INDEX": 2, "DATASYNC_ID": "fixture-channel||fixture-user"
            })))
            .unwrap();
        let headers = auth.headers(100).unwrap();
        assert_eq!(headers["x-goog-authuser"], "2");
        assert_eq!(headers["x-goog-pageid"], "fixture-channel");
        assert!(auth.matches(&CookieJar::from_netscape(COOKIES), None, None));
    }

    #[test]
    fn explicit_session_ids_override_datasync_parts() {
        let page = page(serde_json::json!({
            "DATASYNC_ID": "old-channel||old-user", "DELEGATED_SESSION_ID": "fixture-channel",
            "USER_SESSION_ID": "fixture-user"
        }));
        assert_eq!(page.sessions(), (Some("fixture-channel"), Some("fixture-user")));
    }

    #[test]
    fn wrong_account_and_signed_out_pages_are_rejected() {
        assert!(
            context(Some(2), None)
                .with_page(&page(serde_json::json!({"SESSION_INDEX": 1})))
                .is_err()
        );
        assert!(
            context(None, Some("fixture-channel"))
                .with_page(&page(
                    serde_json::json!({"DATASYNC_ID": "other-channel||fixture-user"})
                ))
                .is_err()
        );
        assert!(
            context(None, Some("fixture-channel"))
                .with_page(&page(serde_json::json!({"DATASYNC_ID": "fixture-user||"})))
                .is_err()
        );
        assert!(
            context(None, None)
                .with_page(&page(serde_json::json!({"LOGGED_IN": false})))
                .is_err()
        );
    }

    #[test]
    fn missing_page_preserves_saved_selection_and_fresh_signatures() {
        let auth = context(Some(2), Some("fixture-channel"));
        let first = auth.headers(100).unwrap();
        let next = auth.headers(101).unwrap();
        assert_eq!(first["cookie"], next["cookie"]);
        assert_eq!(next["x-goog-authuser"], "2");
        assert_eq!(next["x-goog-pageid"], "fixture-channel");
        assert_ne!(first["authorization"], next["authorization"]);
        assert!(!next.contains_key("x-youtube-bootstrap-logged-in"));
    }

    #[test]
    fn cache_identity_includes_cookie_scope_and_requested_account() {
        let jar = CookieJar::from_netscape(COOKIES);
        let auth = context(Some(2), Some("fixture-channel"));
        assert!(auth.matches(&jar, Some(2), Some("fixture-channel")));
        assert!(!auth.matches(&jar, Some(1), Some("fixture-channel")));
        assert!(!auth.matches(&jar, Some(2), Some("other-channel")));
        let changed_scope = CookieJar::from_netscape(&COOKIES.replace("\t/\t", "\t/youtubei\t"));
        assert!(!auth.matches(&changed_scope, Some(2), Some("fixture-channel")));
    }

    #[test]
    fn cached_context_rechecks_cookie_expiry() {
        let text = COOKIES.replace("\t0\t", "\t101\t");
        let auth = AuthContext::new(CookieJar::from_netscape(&text), None, None, 100).unwrap();
        assert!(auth.headers(102).is_err());
    }

    #[test]
    fn account_page_uses_saved_selection_and_its_own_cookie_path() {
        let text = format!("{COOKIES}music.youtube.com\tFALSE\t/youtubei\tTRUE\t0\tAPI_ONLY\tfixture-api\n");
        let auth = AuthContext::new(
            CookieJar::from_netscape(&text),
            Some(2),
            Some("fixture-channel"),
            100,
        )
        .unwrap();
        let request = auth.page_request(100).unwrap().build().unwrap();
        let query: std::collections::HashMap<_, _> = request.url().query_pairs().collect();
        assert_eq!(query.get("authuser").map(|value| value.as_ref()), Some("2"));
        assert_eq!(
            query.get("pageid").map(|value| value.as_ref()),
            Some("fixture-channel")
        );
        assert_eq!(request.headers()["x-goog-authuser"], "2");
        assert_eq!(request.headers()["x-goog-pageid"], "fixture-channel");
        assert!(!request.headers()["cookie"].to_str().unwrap().contains("API_ONLY"));
        assert!(
            auth.headers(100).unwrap()["cookie"]
                .to_str()
                .unwrap()
                .contains("API_ONLY")
        );
        assert!(!format!("{:?}", request.headers()).contains("fixture"));
    }

    #[test]
    fn invalid_live_metadata_fails_without_disclosing_its_value() {
        let auth = context(None, None)
            .with_page(&page(
                serde_json::json!({"VISITOR_DATA": "private-visitor\ninvalid"}),
            ))
            .unwrap();
        let error = auth.headers(100).unwrap_err().to_string();
        assert!(error.contains("invalid request metadata"));
        assert!(!error.contains("private-visitor"));
    }

    #[test]
    fn null_metadata_clears_previous_config_without_retaining_old_visitor() {
        let mut page = page(serde_json::json!({
            "DATASYNC_ID": "fixture-channel||fixture-user", "SESSION_INDEX": 2,
            "USER_SESSION_ID": "explicit-user", "DELEGATED_SESSION_ID": "explicit-channel",
            "VISITOR_DATA": "explicit-visitor", "INNERTUBE_CONTEXT": {"client": {"visitorData": "nested-visitor"}},
            "LOGGED_IN": true
        }));
        page.merge_config(&serde_json::json!({
            "DATASYNC_ID": null, "SESSION_INDEX": null, "USER_SESSION_ID": null,
            "DELEGATED_SESSION_ID": null, "VISITOR_DATA": null, "INNERTUBE_CONTEXT": null, "LOGGED_IN": null
        }));
        let auth = context(None, None).with_page(&page).unwrap();
        let headers = auth.headers(100).unwrap();
        assert_eq!(page.sessions(), (None, None));
        assert!(!headers.contains_key("x-goog-authuser"));
        assert!(!headers.contains_key("x-goog-pageid"));
        assert!(!headers.contains_key("x-goog-visitor-id"));
        assert!(!headers["authorization"].to_str().unwrap().contains("_u"));
    }
}
