//! Validated YouTube credentials, cookie scope and request signing.

mod cookies;
pub mod diagnostics;

use std::path::Path;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;

pub use cookies::{CookieJar, CookieSummary};

const MUSIC_ORIGIN: &str = "https://music.youtube.com";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("credential file is not valid JSON")]
    InvalidJson,
    #[error("credential file has no cookie header")]
    MissingCookies,
    #[error("credentials have no YouTube signing cookie")]
    MissingSigningCookie,
    #[error("credential metadata is not valid for an HTTP request")]
    InvalidMetadata,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Inspect a login export without returning any credential material.
pub fn inspect_netscape(path: &Path) -> Result<CookieSummary> {
    let netscape_text = std::fs::read_to_string(path)?;
    Ok(CookieJar::from_netscape(&netscape_text).summary())
}

/// The selected account and optional delegated channel.
#[derive(Clone, PartialEq, Eq)]
pub struct SelectedIdentity {
    authuser: u32,
    page_id: Option<String>,
}

impl std::fmt::Debug for SelectedIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SelectedIdentity")
            .field("authuser", &self.authuser)
            .field("has_page_id", &self.page_id.is_some())
            .finish()
    }
}

impl SelectedIdentity {
    pub fn new(authuser: u32, page_id: Option<String>) -> SelectedIdentity {
        SelectedIdentity { authuser, page_id }
    }

    pub fn authuser(&self) -> u32 {
        self.authuser
    }

    pub fn page_id(&self) -> Option<&str> {
        self.page_id.as_deref()
    }
}

/// One immutable browser login with a redacted debug representation.
#[derive(Clone, PartialEq, Eq)]
pub struct BrowserSession {
    cookie: String,
    identity: SelectedIdentity,
}

impl std::fmt::Debug for BrowserSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserSession")
            .field("authuser", &self.identity.authuser)
            .field("has_page_id", &self.identity.page_id.is_some())
            .field("cookie", &"[redacted]")
            .finish()
    }
}

impl BrowserSession {
    pub fn new(cookie: String, identity: SelectedIdentity) -> Result<BrowserSession> {
        if cookie.trim().is_empty() {
            return Err(Error::MissingCookies);
        }
        if !cookies::header_has_signing_cookie(&cookie) {
            return Err(Error::MissingSigningCookie);
        }
        HeaderValue::from_str(&cookie).map_err(|_| Error::InvalidMetadata)?;
        if let Some(page_id) = identity.page_id() {
            HeaderValue::from_str(page_id).map_err(|_| Error::InvalidMetadata)?;
        }
        Ok(BrowserSession { cookie, identity })
    }

    /// Read the compatible `browser.json` header file.
    /// A missing file means signed out. A present invalid file is an error.
    pub fn read(path: &Path) -> Result<Option<BrowserSession>> {
        let browser_json = match std::fs::read_to_string(path) {
            Ok(browser_json) => browser_json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::Io(error)),
        };
        Self::from_browser_json(&browser_json).map(Some)
    }

    pub fn from_browser_json(browser_json: &str) -> Result<BrowserSession> {
        let saved_headers: Value = serde_json::from_str(browser_json).map_err(|_| Error::InvalidJson)?;
        let saved_header = |header_name: &str| -> Result<Option<String>> {
            let header_value = saved_headers
                .as_object()
                .into_iter()
                .flat_map(|header_object| header_object.iter())
                .find(|(saved_name, _)| saved_name.eq_ignore_ascii_case(header_name))
                .map(|(_, value)| value);
            match header_value {
                None => Ok(None),
                Some(Value::String(value)) => Ok(Some(value.clone())),
                Some(_) => Err(Error::InvalidMetadata),
            }
        };
        let cookie_header = saved_header("cookie")?
            .filter(|value| !value.is_empty())
            .ok_or(Error::MissingCookies)?;
        let authuser: u32 = match saved_header("x-goog-authuser")? {
            Some(saved_authuser) => saved_authuser.parse().map_err(|_| Error::InvalidMetadata)?,
            None => 0,
        };
        let identity = SelectedIdentity::new(
            authuser,
            saved_header("x-goog-pageid")?.filter(|page_id| !page_id.is_empty()),
        );
        Self::new(cookie_header, identity)
    }

    pub fn identity(&self) -> &SelectedIdentity {
        &self.identity
    }

    pub fn cookie_header(&self) -> &str {
        &self.cookie
    }

    /// Headers that must be derived from the same immutable cookie snapshot.
    pub fn music_headers(&self, unix_seconds: u64) -> Result<HeaderMap> {
        let authorization = cookies::header_authorization(&self.cookie, MUSIC_ORIGIN, unix_seconds)
            .ok_or(Error::MissingSigningCookie)?;
        let mut headers = HeaderMap::new();
        insert_sensitive(&mut headers, "cookie", &self.cookie)?;
        insert_sensitive(&mut headers, "authorization", &authorization)?;
        insert_sensitive(
            &mut headers,
            "x-goog-authuser",
            &self.identity.authuser.to_string(),
        )?;
        if let Some(page_id) = self.identity.page_id() {
            insert_sensitive(&mut headers, "x-goog-pageid", page_id)?;
        }
        Ok(headers)
    }
}

fn insert_sensitive(headers: &mut HeaderMap, name: &'static str, text: &str) -> Result<()> {
    let mut sensitive_value = HeaderValue::from_str(text).map_err(|_| Error::InvalidMetadata)?;
    sensitive_value.set_sensitive(true);
    headers.insert(name, sensitive_value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_session_keeps_secrets_out_of_debug() {
        let session = BrowserSession::from_browser_json(
            r#"{"cookie":"SAPISID=secret; LOGIN_INFO=private","x-goog-authuser":"2","x-goog-pageid":"channel"}"#,
        )
        .expect("the fixture has a signing cookie");

        let debug = format!("{session:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("private"));
        assert!(!debug.contains("channel"));
        assert_eq!(session.identity().authuser(), 2);
        assert_eq!(session.identity().page_id(), Some("channel"));
        assert!(!format!("{:?}", session.identity()).contains("channel"));
    }

    #[test]
    fn browser_session_rejects_a_cookie_without_signing_material() {
        let result = BrowserSession::from_browser_json(r#"{"cookie":"PREF=f1"}"#);
        assert!(matches!(result, Err(Error::MissingSigningCookie)));
    }

    #[test]
    fn browser_session_rejects_invalid_headers_at_the_parse_boundary() {
        let cookie = "SAPISID=secret\nInjected: value".to_owned();
        assert!(matches!(
            BrowserSession::new(cookie, SelectedIdentity::new(0, None)),
            Err(Error::InvalidMetadata)
        ));
        let identity = SelectedIdentity::new(0, Some("channel\nInjected: value".to_owned()));
        assert!(matches!(
            BrowserSession::new("SAPISID=secret".to_owned(), identity),
            Err(Error::InvalidMetadata)
        ));
    }

    #[test]
    fn browser_session_rejects_malformed_present_identity_metadata() {
        for text in [
            r#"{"cookie":"SAPISID=secret","x-goog-authuser":"account-zero"}"#,
            r#"{"cookie":"SAPISID=secret","x-goog-authuser":0}"#,
            r#"{"cookie":"SAPISID=secret","x-goog-pageid":7}"#,
            r#"{"cookie":["SAPISID=secret"]}"#,
        ] {
            assert!(matches!(
                BrowserSession::from_browser_json(text),
                Err(Error::InvalidMetadata)
            ));
        }

        let legacy = BrowserSession::from_browser_json(r#"{"cookie":"SAPISID=secret"}"#)
            .expect("an absent authuser keeps the legacy primary-account default");
        assert_eq!(legacy.identity().authuser(), 0);
    }

    #[test]
    fn signing_matches_the_known_vector() {
        assert_eq!(
            cookies::header_authorization("SAPISID=AAAAAAAAAAAAAAAA", MUSIC_ORIGIN, 1_700_000_000).unwrap(),
            "SAPISIDHASH 1700000000_fff3f1c7f1891571ba9c0a66cf1432d4b8de3eb2"
        );
    }

    #[test]
    fn browser_sessions_and_cookie_jars_share_every_signing_scheme() {
        for signing_name in ["SAPISID", "__Secure-1PAPISID", "__Secure-3PAPISID"] {
            let jar = CookieJar::from_netscape(&format!(
                ".youtube.com\tTRUE\t/\tTRUE\t0\t{signing_name}\tsecret\n\
                 .youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\tprivate\n",
            ));
            assert!(jar.authenticated());
            let session = BrowserSession::new(jar.header(), SelectedIdentity::new(0, None)).unwrap();
            let headers = session.music_headers(1_700_000_000).unwrap();
            assert_eq!(
                headers["authorization"].to_str().unwrap(),
                jar.authorization(MUSIC_ORIGIN, 1_700_000_000, None).unwrap()
            );
            assert!(headers["authorization"].is_sensitive());
            assert!(headers["cookie"].is_sensitive());
        }
    }

    #[test]
    fn cookie_summary_reports_capability_without_values() {
        let jar = CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\tprivate\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsecret\n",
        );
        let summary = jar.summary();
        assert_eq!(summary.count, 2);
        assert!(summary.authenticated());
        assert!(!format!("{summary:?}").contains("private"));
        assert!(!format!("{summary:?}").contains("secret"));
    }
}
