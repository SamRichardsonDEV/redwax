//! The Netscape `cookies.txt` the login writes, and the SAPISIDHASH it feeds.
//!
//! YouTube treats a request as signed in only when the cookie header and the
//! `Authorization` hash agree, so both are built from one read of the file.

use sha1::{Digest, Sha1};

/// Scope and credentials travel together so a request cannot borrow another host's identity.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

/// Privacy-safe facts about a Netscape cookie snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CookieSummary {
    pub count: usize,
    pub has_login: bool,
    pub has_signing_cookie: bool,
}

impl CookieSummary {
    pub fn authenticated(self) -> bool {
        self.has_login && self.has_signing_cookie
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Cookie {
    domain: String,
    include_subdomains: bool,
    path: String,
    secure_only: bool,
    expires_at_unix_seconds: u64,
    name: String,
    value: String,
}

fn wanted(domain: &str) -> bool {
    domain == "youtube.com" || domain.ends_with(".youtube.com")
}

impl Cookie {
    fn matches(&self, request_url: &reqwest::Url, now_unix_seconds: u64) -> bool {
        let Some(request_host) = request_url.host_str() else {
            return false;
        };
        let domain_matches = request_host == self.domain
            || (self.include_subdomains
                && request_host
                    .strip_suffix(&self.domain)
                    .is_some_and(|prefix| prefix.ends_with('.')));
        let request_path = request_url.path();
        let path_matches = request_path == self.path
            || (request_path.starts_with(&self.path)
                && (self.path.ends_with('/') || request_path.as_bytes().get(self.path.len()) == Some(&b'/')));
        domain_matches
            && path_matches
            && matches!(request_url.scheme(), "http" | "https")
            && (!self.secure_only || request_url.scheme() == "https")
            && (self.expires_at_unix_seconds == 0 || self.expires_at_unix_seconds > now_unix_seconds)
    }
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn safe_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e))
}

fn netscape_flag(value: &str) -> Option<bool> {
    match value {
        "TRUE" => Some(true),
        "FALSE" => Some(false),
        _ => None,
    }
}

impl CookieJar {
    pub fn from_netscape(netscape_text: &str) -> CookieJar {
        let mut cookies: Vec<Cookie> = Vec::new();
        for line in netscape_text.lines() {
            // HttpOnly is metadata, not a Netscape comment or a host-only flag.
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let netscape_fields: Vec<&str> = line.split('\t').collect();
            if netscape_fields.len() != 7 {
                continue;
            }
            let domain = netscape_fields[0]
                .strip_prefix('.')
                .unwrap_or(netscape_fields[0])
                .to_ascii_lowercase();
            if !wanted(&domain)
                || !domain
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
                || domain.starts_with('.')
                || !netscape_fields[2].starts_with('/')
                || !safe_name(netscape_fields[5])
                || !safe_value(netscape_fields[6])
            {
                continue;
            }
            let (Some(include_subdomains), Some(secure_only), Ok(expires_at_unix_seconds)) = (
                netscape_flag(netscape_fields[1]),
                netscape_flag(netscape_fields[3]),
                netscape_fields[4].parse::<u64>(),
            ) else {
                continue;
            };
            let cookie = Cookie {
                domain,
                include_subdomains,
                path: netscape_fields[2].to_owned(),
                secure_only,
                expires_at_unix_seconds,
                name: netscape_fields[5].to_owned(),
                value: netscape_fields[6].to_owned(),
            };
            match cookies.iter_mut().find(|existing| {
                existing.domain == cookie.domain
                    && existing.path == cookie.path
                    && existing.name == cookie.name
            }) {
                Some(existing_cookie) => *existing_cookie = cookie,
                None => cookies.push(cookie),
            }
        }
        CookieJar { cookies }
    }

    pub fn for_request(&self, request_url: &reqwest::Url, unix_seconds: u64) -> CookieJar {
        let mut cookies: Vec<Cookie> = self
            .cookies
            .iter()
            .filter(|cookie| cookie.matches(request_url, unix_seconds))
            .cloned()
            .collect();
        // Browsers send the most specific path first and preserve creation order on ties.
        cookies.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        CookieJar { cookies }
    }

    pub fn header(&self) -> String {
        self.cookies
            .iter()
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn nonempty(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|cookie| cookie.name == name && !cookie.value.is_empty())
            .map(|cookie| cookie.value.as_str())
    }

    pub fn authenticated(&self) -> bool {
        self.nonempty("LOGIN_INFO").is_some()
            && signing_values(|name| self.nonempty(name))
                .into_iter()
                .any(|(_, cookie_value)| cookie_value.is_some())
    }

    /// Report credential capability without exposing names or values.
    pub fn summary(&self) -> CookieSummary {
        let unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let music_origin_url =
            reqwest::Url::parse(crate::MUSIC_ORIGIN).expect("the music origin is a fixed HTTPS URL");
        let music_cookies = self.for_request(&music_origin_url, unix_seconds);
        CookieSummary {
            count: music_cookies.cookies.len(),
            has_login: music_cookies.nonempty("LOGIN_INFO").is_some(),
            has_signing_cookie: signing_values(|name| music_cookies.nonempty(name))
                .into_iter()
                .any(|(_, cookie_value)| cookie_value.is_some()),
        }
    }

    pub fn authorization(
        &self,
        origin: &str,
        unix_seconds: u64,
        user_session_id: Option<&str>,
    ) -> Option<String> {
        authorization(|name| self.nonempty(name), origin, unix_seconds, user_session_id)
    }
}

pub(crate) fn header_has_signing_cookie(cookie_header: &str) -> bool {
    signing_values(|name| header_cookie_value(cookie_header, name))
        .into_iter()
        .any(|(_, cookie_value)| cookie_value.is_some())
}

pub(crate) fn header_authorization(cookie_header: &str, origin: &str, unix_seconds: u64) -> Option<String> {
    authorization(
        |name| header_cookie_value(cookie_header, name),
        origin,
        unix_seconds,
        None,
    )
}

fn header_cookie_value<'a>(cookie_header: &'a str, wanted_name: &str) -> Option<&'a str> {
    cookie_header.split(';').find_map(|cookie_pair| {
        let (cookie_name, cookie_value) = cookie_pair.split_once('=')?;
        let cookie_value = cookie_value.trim().trim_matches('"');
        (cookie_name.trim() == wanted_name && !cookie_value.is_empty()).then_some(cookie_value)
    })
}

fn signing_values<'a>(find_cookie: impl Fn(&str) -> Option<&'a str>) -> [(&'static str, Option<&'a str>); 3] {
    [
        (
            "SAPISIDHASH",
            find_cookie("SAPISID").or_else(|| find_cookie("__Secure-3PAPISID")),
        ),
        ("SAPISID1PHASH", find_cookie("__Secure-1PAPISID")),
        ("SAPISID3PHASH", find_cookie("__Secure-3PAPISID")),
    ]
}

fn authorization<'a>(
    find_cookie: impl Fn(&str) -> Option<&'a str>,
    origin: &str,
    unix_seconds: u64,
    user_session_id: Option<&str>,
) -> Option<String> {
    let authorization_headers: Vec<String> = signing_values(find_cookie)
        .into_iter()
        .filter_map(|(hash_scheme, signing_cookie_value)| {
            signing_cookie_value.map(|signing_cookie_value| {
                authorization_hash(
                    hash_scheme,
                    signing_cookie_value,
                    origin,
                    unix_seconds,
                    user_session_id,
                )
            })
        })
        .collect();
    (!authorization_headers.is_empty()).then(|| authorization_headers.join(" "))
}

fn authorization_hash(
    hash_scheme: &str,
    signing_cookie_value: &str,
    origin: &str,
    unix_seconds: u64,
    user_session_id: Option<&str>,
) -> String {
    let (hash_input, session_suffix) = match user_session_id.filter(|user| !user.is_empty()) {
        Some(user_session_id) => (
            format!("{user_session_id} {unix_seconds} {signing_cookie_value} {origin}"),
            "_u",
        ),
        None => (format!("{unix_seconds} {signing_cookie_value} {origin}"), ""),
    };
    format!(
        "{hash_scheme} {unix_seconds}_{}{session_suffix}",
        hex::encode(Sha1::digest(hash_input.as_bytes()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Netscape HTTP Cookie File\n\
        .youtube.com\tTRUE\t/\tTRUE\t1800000000\tSAPISID\tsecret-one\n\
        #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1800000000\tLOGIN_INFO\tsecret-two\n\
        .google.com\tTRUE\t/\tTRUE\t1800000000\tSAPISID\twrong-host\n\
        music.youtube.com\tFALSE\t/\tTRUE\t1800000000\tPREF\tf1=1\n\
        \n\
        broken line\n";

    #[test]
    fn netscape_cookies_are_read_for_youtube_hosts_only() {
        let jar = CookieJar::from_netscape(SAMPLE);
        assert_eq!(jar.nonempty("SAPISID"), Some("secret-one"));
        assert_eq!(jar.nonempty("LOGIN_INFO"), Some("secret-two"));
        assert_eq!(jar.nonempty("PREF"), Some("f1=1"));
        assert_eq!(
            jar.header(),
            "SAPISID=secret-one; LOGIN_INFO=secret-two; PREF=f1=1"
        );
    }

    #[test]
    fn sapisidhash_matches_the_known_vector() {
        assert_eq!(
            authorization_hash(
                "SAPISIDHASH",
                "AAAAAAAAAAAAAAAA",
                "https://music.youtube.com",
                1_700_000_000,
                None
            ),
            "SAPISIDHASH 1700000000_fff3f1c7f1891571ba9c0a66cf1432d4b8de3eb2"
        );
    }

    fn request(jar: &CookieJar, address: &str) -> CookieJar {
        jar.for_request(&reqwest::Url::parse(address).unwrap(), 1_700_000_000)
    }

    #[test]
    fn host_only_and_domain_cookies_keep_their_request_scope() {
        let jar = CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tDOMAIN\tshared\n\
             .www.youtube.com\tFALSE\t/\tTRUE\t0\tHOST\twww-only\n\
             .music.youtube.com\tTRUE\t/\tTRUE\t0\tMUSIC\tmusic-tree\n\
             .notyoutube.com\tTRUE\t/\tTRUE\t0\tWRONG\tignored\n",
        );
        assert_eq!(
            request(&jar, "https://music.youtube.com/").header(),
            "DOMAIN=shared; MUSIC=music-tree"
        );
        assert_eq!(
            request(&jar, "https://www.youtube.com/").header(),
            "DOMAIN=shared; HOST=www-only"
        );
        assert_eq!(
            request(&jar, "https://child.music.youtube.com/").header(),
            "DOMAIN=shared; MUSIC=music-tree"
        );
        assert_eq!(
            request(&jar, "https://child.www.youtube.com/").header(),
            "DOMAIN=shared"
        );
        assert!(
            request(&jar, "https://youtube.com.example.invalid/")
                .header()
                .is_empty()
        );
        assert!(request(&jar, "https://notyoutube.com/").header().is_empty());
    }

    #[test]
    fn request_excludes_expired_and_insecurely_sent_cookies() {
        let jar = CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tSESSION\talive\n\
             .youtube.com\tTRUE\t/\tFALSE\t1700000001\tFUTURE\talive\n\
             .youtube.com\tTRUE\t/\tFALSE\t1700000000\tEXPIRED\told\n",
        );
        assert_eq!(
            request(&jar, "https://music.youtube.com/").header(),
            "SESSION=alive; FUTURE=alive"
        );
        assert_eq!(
            request(&jar, "http://music.youtube.com/").header(),
            "FUTURE=alive"
        );
        let later = jar.for_request(
            &reqwest::Url::parse("https://music.youtube.com/").unwrap(),
            1_700_000_001,
        );
        assert_eq!(later.header(), "SESSION=alive");
        assert!(request(&jar, "ftp://music.youtube.com/").header().is_empty());
    }

    #[test]
    fn duplicate_cookie_names_survive_distinct_scopes_and_use_browser_path_order() {
        let jar = CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tSCOPE\troot\n\
             .youtube.com\tTRUE\t/watch\tTRUE\t0\tSCOPE\told\n\
             .youtube.com\tTRUE\t/watch\tTRUE\t0\tSCOPE\tupdated\n\
             music.youtube.com\tFALSE\t/\tTRUE\t0\tSCOPE\thost\n\
             .youtube.com\tTRUE\t/watch/\tTRUE\t0\tCHILD\tdeep\n",
        );
        let selected = request(&jar, "https://music.youtube.com/watch/next");
        assert_eq!(
            selected.header(),
            "CHILD=deep; SCOPE=updated; SCOPE=root; SCOPE=host"
        );
        assert_eq!(selected.nonempty("SCOPE"), Some("updated"));
        assert_eq!(
            request(&jar, "https://music.youtube.com/watch").header(),
            "SCOPE=updated; SCOPE=root; SCOPE=host"
        );
        assert_eq!(
            request(&jar, "https://music.youtube.com/watchlist").header(),
            "SCOPE=root; SCOPE=host"
        );
        assert_eq!(
            request(&jar, "https://www.youtube.com/watch").header(),
            "SCOPE=updated; SCOPE=root"
        );
    }

    #[test]
    fn unsafe_cookie_fields_cannot_inject_headers() {
        let jar = CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tGOOD\tsafe=1\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tBAD;NAME\tbad\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tBAD\tvalue;another=1\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tCONTROL\tvalue\x01\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tQUOTE\t\"value\"\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tTAB\tvalue\tother\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\t\tunnamed\n",
        );
        assert_eq!(
            request(&jar, "https://music.youtube.com/").header(),
            "GOOD=safe=1"
        );
    }

    #[test]
    fn authenticated_requires_login_info_and_a_nonempty_sid() {
        let login = ".youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\tfixture-login\n";
        assert!(!CookieJar::from_netscape(login).authenticated());
        for name in ["SAPISID", "__Secure-1PAPISID", "__Secure-3PAPISID"] {
            let sid = format!(".youtube.com\tTRUE\t/\tTRUE\t0\t{name}\tfixture-sid\n");
            assert!(!CookieJar::from_netscape(&sid).authenticated());
            assert!(CookieJar::from_netscape(&format!("{login}{sid}")).authenticated());
            let blank = format!(".youtube.com\tTRUE\t/\tTRUE\t0\t{name}\t\n");
            assert!(!CookieJar::from_netscape(&format!("{login}{blank}")).authenticated());
        }
        let empty_login = ".youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\t\n";
        let sid = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfixture-sid\n";
        assert!(!CookieJar::from_netscape(&format!("{empty_login}{sid}")).authenticated());
        let scoped_login = "www.youtube.com\tFALSE\t/\tTRUE\t0\tLOGIN_INFO\tfixture-login\n";
        let jar = CookieJar::from_netscape(&format!("{scoped_login}{sid}"));
        assert!(!request(&jar, "https://music.youtube.com/").authenticated());
    }

    #[test]
    fn authorization_emits_all_three_schemes_and_binds_the_user_session() {
        let jar = CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tAAAAAAAAAAAAAAAA\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-1PAPISID\tBBBBBBBBBBBBBBBB\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-3PAPISID\tCCCCCCCCCCCCCCCC\n",
        );
        let unbound = "SAPISIDHASH 1700000000_fff3f1c7f1891571ba9c0a66cf1432d4b8de3eb2 \
            SAPISID1PHASH 1700000000_59c1bdd52ba359a8150bd6535903a2870cd8a8e4 \
            SAPISID3PHASH 1700000000_5e2448d2438680741ddc5cb5023a91cacd06b7a8";
        let bound = "SAPISIDHASH 1700000000_6d9ba99351596c63d591fcbe89e83bf065809c0c_u \
            SAPISID1PHASH 1700000000_01af9442080adb0794424caa3639bcd0d36410e5_u \
            SAPISID3PHASH 1700000000_dc19465d68772f68d2200bb811adcc0a5cf60162_u";
        for (user_session_id, expected) in [
            (None, unbound),
            (Some(""), unbound),
            (Some("fixture-session"), bound),
        ] {
            assert_eq!(
                jar.authorization("https://music.youtube.com", 1_700_000_000, user_session_id)
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn secure_3p_fallback_skips_an_empty_sapisid() {
        let jar = CookieJar::from_netscape(
            ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\t\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-3PAPISID\tAAAAAAAAAAAAAAAA\n",
        );
        let expected = "1700000000_fff3f1c7f1891571ba9c0a66cf1432d4b8de3eb2";
        assert_eq!(
            jar.authorization("https://music.youtube.com", 1_700_000_000, None)
                .unwrap(),
            format!("SAPISIDHASH {expected} SAPISID3PHASH {expected}")
        );
        assert!(
            CookieJar::default()
                .authorization("https://music.youtube.com", 1_700_000_000, None)
                .is_none()
        );
    }
}
