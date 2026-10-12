//! Reading the browser: the YouTube cookies and the tracks the web app marked
//! for download.
//!
//! Everything here is a read of SQLite files under the Firefox profile. The
//! files are copied (with their `-wal`) to a temp dir before opening, because
//! Firefox holds them locked. The encrypted audio chunks in the same database
//! are never read.

mod idb;

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no Firefox profile was found")]
    NoProfile,
    #[error("the selected Firefox profile has no cookie database")]
    NoCookies,
    #[error("the selected Firefox profile has no YouTube Music storage")]
    NoMarks,
    #[error("browser cookie export is not valid Netscape format")]
    InvalidCookieExport,
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirefoxProfile {
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileCapability {
    Cookies,
    Marks,
}

impl FirefoxProfile {
    /// The default-release profile under `$XDG_CONFIG_HOME/mozilla/firefox`
    /// or `~/.mozilla/firefox`, read from `profiles.ini`. `profile_name` picks by name.
    pub fn find(profile_name: Option<&str>) -> Result<FirefoxProfile> {
        for firefox_root in profile_roots() {
            let Ok(profiles_ini) = std::fs::read_to_string(firefox_root.join("profiles.ini")) else {
                continue;
            };
            for profile_directory in profile_dirs(&profiles_ini, &firefox_root, profile_name) {
                let candidate_profile = FirefoxProfile {
                    dir: profile_directory,
                };
                if !candidate_profile.dir.is_dir() {
                    continue;
                }
                return Ok(candidate_profile);
            }
        }
        Err(Error::NoProfile)
    }

    /// Find the first configured profile that can serve one browser capability.
    pub fn find_for(profile_name: Option<&str>, capability: ProfileCapability) -> Result<FirefoxProfile> {
        Self::find_all_for(profile_name, capability).map(|mut profiles| profiles.remove(0))
    }

    /// Configured profiles that can serve one browser capability, in Firefox's
    /// preferred order. A requested profile still narrows the result to one.
    pub fn find_all_for(
        profile_name: Option<&str>,
        capability: ProfileCapability,
    ) -> Result<Vec<FirefoxProfile>> {
        let mut profiles = Vec::new();
        let mut has_existing_profile = false;
        for firefox_root in profile_roots() {
            let Ok(profiles_ini) = std::fs::read_to_string(firefox_root.join("profiles.ini")) else {
                continue;
            };
            for profile_directory in profile_dirs(&profiles_ini, &firefox_root, profile_name) {
                let candidate_profile = FirefoxProfile {
                    dir: profile_directory,
                };
                if !candidate_profile.dir.is_dir() {
                    continue;
                }
                has_existing_profile = true;
                if candidate_profile.supports(capability)
                    && !profiles
                        .iter()
                        .any(|profile: &FirefoxProfile| profile.dir == candidate_profile.dir)
                {
                    profiles.push(candidate_profile);
                }
            }
        }
        if !profiles.is_empty() {
            return Ok(profiles);
        }
        match (has_existing_profile, capability) {
            (true, ProfileCapability::Cookies) => Err(Error::NoCookies),
            (true, ProfileCapability::Marks) => Err(Error::NoMarks),
            _ => Err(Error::NoProfile),
        }
    }

    pub fn idb_dir(&self) -> PathBuf {
        self.dir.join("storage/default/https+++music.youtube.com/idb")
    }

    pub fn has_cookies(&self) -> bool {
        self.dir.join("cookies.sqlite").is_file()
    }

    pub fn has_marks(&self) -> bool {
        self.idb_dir().is_dir()
    }

    fn supports(&self, capability: ProfileCapability) -> bool {
        match capability {
            ProfileCapability::Cookies => self.has_cookies(),
            ProfileCapability::Marks => self.has_marks(),
        }
    }
}

fn profile_roots() -> Vec<PathBuf> {
    let Some(base) = directories::BaseDirs::new() else {
        return Vec::new();
    };
    vec![
        base.config_dir().join("mozilla/firefox"),
        base.home_dir().join(".mozilla/firefox"),
    ]
}

/// Profile directories from `profiles.ini`, best candidate first.
///
/// A named `profile_name` narrows to that one. Otherwise the install's own default
/// leads, because `Default=1` on a `[Profile]` section marks the profile
/// manager's choice rather than the one the running Firefox opened.
fn profile_dirs(profiles_ini: &str, firefox_root: &Path, profile_name: Option<&str>) -> Vec<PathBuf> {
    let sections = parse_ini(profiles_ini);
    let section_value = |section_entries: &Vec<(String, String)>, key: &str| {
        section_entries
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, entry_value)| entry_value.clone())
    };
    let resolve_profile_path = |path: &str, is_relative: bool| {
        if is_relative || Path::new(path).is_relative() {
            firefox_root.join(path)
        } else {
            PathBuf::from(path)
        }
    };

    let profiles: Vec<(String, String, PathBuf, bool)> = sections
        .iter()
        .filter(|(name, _)| name.starts_with("Profile"))
        .filter_map(|(_, section_entries)| {
            let path = section_value(section_entries, "Path")?;
            let is_relative = section_value(section_entries, "IsRelative").as_deref() != Some("0");
            let profile_directory = resolve_profile_path(&path, is_relative);
            let is_default = section_value(section_entries, "Default").as_deref() == Some("1");
            Some((
                section_value(section_entries, "Name").unwrap_or_default(),
                path,
                profile_directory,
                is_default,
            ))
        })
        .collect();

    if let Some(requested_profile) = profile_name {
        return profiles
            .iter()
            .filter(|(name, path, profile_directory, _)| {
                name == requested_profile
                    || path == requested_profile
                    || profile_directory.as_path() == Path::new(requested_profile)
            })
            .map(|(_, _, profile_directory, _)| profile_directory.clone())
            .collect();
    }

    let mut ordered_directories: Vec<PathBuf> = Vec::new();
    let mut append_unique_directory = |profile_directory: PathBuf| {
        if !ordered_directories.contains(&profile_directory) {
            ordered_directories.push(profile_directory);
        }
    };
    for (name, section_entries) in &sections {
        if name.starts_with("Install")
            && let Some(path) = section_value(section_entries, "Default")
        {
            append_unique_directory(resolve_profile_path(&path, true));
        }
    }
    for (_, _, profile_directory, is_default) in &profiles {
        if *is_default {
            append_unique_directory(profile_directory.clone());
        }
    }
    for (_, _, profile_directory, _) in &profiles {
        append_unique_directory(profile_directory.clone());
    }
    ordered_directories
}

fn parse_ini(ini_contents: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in ini_contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            sections.push((name.to_owned(), Vec::new()));
        } else if let Some((key, entry_value)) = line.split_once('=')
            && let Some((_, section_entries)) = sections.last_mut()
        {
            section_entries.push((key.trim().to_owned(), entry_value.trim().to_owned()));
        }
    }
    sections
}

#[derive(Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub host: String,
    pub include_subdomains: bool,
    pub path: String,
    pub secure: bool,
    pub expiry: i64,
}

impl std::fmt::Debug for Cookie {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Cookie")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .field("host", &self.host)
            .field("include_subdomains", &self.include_subdomains)
            .field("path", &self.path)
            .field("secure", &self.secure)
            .field("expiry", &self.expiry)
            .finish()
    }
}

/// Current YouTube cookies, excluding browser session tracking rows.
pub fn read_cookies(profile: &FirefoxProfile) -> Result<Vec<Cookie>> {
    if !profile.has_cookies() {
        return Err(Error::NoCookies);
    }
    let snapshot_directory = TempDir::new("cookies")?;
    let snapshot_path = snapshot(&profile.dir.join("cookies.sqlite"), snapshot_directory.path())?;
    let connection = open_snapshot(&snapshot_path)?;
    let mut statement =
        connection.prepare("select name, value, host, path, isSecure, expiry from moz_cookies")?;
    let rows = statement.query_map([], |row| {
        Ok(Cookie {
            name: row.get(0)?,
            value: row.get(1)?,
            host: row.get(2)?,
            include_subdomains: row.get::<_, String>(2)?.starts_with('.'),
            path: row.get(3)?,
            secure: row.get::<_, i64>(4)? != 0,
            expiry: row.get(5)?,
        })
    })?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut cookies: Vec<Cookie> = rows
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|cookie| is_youtube_cookie(cookie) && cookie_is_current(cookie, now))
        .collect();
    cookies.sort_by(|left_cookie, right_cookie| {
        left_cookie
            .name
            .cmp(&right_cookie.name)
            .then(left_cookie.host.cmp(&right_cookie.host))
    });
    Ok(cookies)
}

/// Read the current Google and YouTube rows from a yt-dlp Netscape export.
pub fn read_netscape(cookie_export_path: &Path) -> Result<Vec<Cookie>> {
    let cookie_export = std::fs::read_to_string(cookie_export_path)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut cookies = Vec::new();
    for line in cookie_export.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 7 {
            return Err(Error::InvalidCookieExport);
        }
        let include_subdomains = match fields[1] {
            "TRUE" => true,
            "FALSE" => false,
            _ => return Err(Error::InvalidCookieExport),
        };
        let secure = match fields[3] {
            "TRUE" => true,
            "FALSE" => false,
            _ => return Err(Error::InvalidCookieExport),
        };
        let expiry = fields[4].parse().map_err(|_| Error::InvalidCookieExport)?;
        let cookie = Cookie {
            host: fields[0].to_owned(),
            include_subdomains,
            path: fields[2].to_owned(),
            secure,
            expiry,
            name: fields[5].to_owned(),
            value: fields[6].to_owned(),
        };
        if is_google_or_youtube_cookie(&cookie)
            && !cookie.name.starts_with("ST-")
            && cookie_is_current(&cookie, now)
        {
            cookies.push(cookie);
        }
    }
    cookies.sort_by(|left_cookie, right_cookie| {
        left_cookie
            .name
            .cmp(&right_cookie.name)
            .then(left_cookie.host.cmp(&right_cookie.host))
    });
    Ok(cookies)
}

/// YouTube's own cookies, minus the hundreds of `ST-` session tracking ones.
fn is_youtube_cookie(cookie: &Cookie) -> bool {
    let host = cookie.host.trim_start_matches('.');
    (host == "youtube.com" || host.ends_with(".youtube.com")) && !cookie.name.starts_with("ST-")
}

fn is_google_or_youtube_cookie(cookie: &Cookie) -> bool {
    let host = cookie.host.trim_start_matches('.');
    host == "google.com"
        || host.ends_with(".google.com")
        || host == "youtube.com"
        || host.ends_with(".youtube.com")
}

fn cookie_is_current(cookie: &Cookie, now: u64) -> bool {
    cookie.expiry == 0 || u64::try_from(cookie.expiry).is_ok_and(|expiry| expiry > now)
}

/// Write cookies in Netscape format, what yt-dlp and mpv read.
pub fn write_netscape(cookies: &[Cookie], cookie_export_path: &Path) -> Result<()> {
    if let Some(parent) = cookie_export_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cookie_export_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(cookie_export_path)?;
    cookie_export_file.write_all(&encode_netscape(cookies))?;
    // A rerun over an existing file keeps that file's mode, so set it either way.
    std::fs::set_permissions(cookie_export_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn encode_netscape(cookies: &[Cookie]) -> Vec<u8> {
    let mut cookie_export =
        String::from("# Netscape HTTP Cookie File\n# This file was generated by redwax. Do not edit.\n\n");
    for cookie in cookies {
        let subdomain_flag = if cookie.include_subdomains {
            "TRUE"
        } else {
            "FALSE"
        };
        let secure_flag = if cookie.secure { "TRUE" } else { "FALSE" };
        cookie_export.push_str(&format!(
            "{}\t{subdomain_flag}\t{}\t{secure_flag}\t{}\t{}\t{}\n",
            cookie.host, cookie.path, cookie.expiry, cookie.name, cookie.value,
        ));
    }
    cookie_export.into_bytes()
}

/// What the web app has marked for download.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Marks {
    /// Video ids with a `downloadStatusEntity`.
    pub tracks: Vec<String>,
    /// Playlist ids with a `musicPlaylistDownloadMetadataEntity`.
    pub playlists: Vec<String>,
    /// Album browse ids with the album equivalent.
    pub albums: Vec<String>,
    /// Video ids present in `yt-player-local-media`'s `index` store, keyed `<id>|a`.
    pub stored_locally: Vec<String>,
}

// YouTube renumbers entity types. Recognisable album ids take precedence.
const DOWNLOAD_STATUS_ENTITY: u64 = 141;
const MUSIC_TRACK_DOWNLOAD_METADATA_ENTITY: u64 = 234;
const MUSIC_PLAYLIST_DOWNLOAD_METADATA_ENTITY: u64 = 248;

const ALBUM_ID_PREFIX: &str = "MPRE";

/// The `index` store rows are `<videoId>|a` for audio and `<videoId>|v` for video.
const LOCAL_AUDIO_SUFFIX: &str = "|a";

/// Read download marks and stored audio ids from Firefox IndexedDB snapshots.
pub fn read_marks(profile: &FirefoxProfile) -> Result<Marks> {
    if !profile.has_marks() {
        return Err(Error::NoMarks);
    }
    let snapshot_directory = TempDir::new("marks")?;
    let indexeddb_directory = profile.idb_dir();
    let mut marked_video_ids = BTreeSet::new();
    let mut marked_playlist_ids = BTreeSet::new();
    let mut marked_album_browse_ids = BTreeSet::new();
    let mut stored_audio_video_ids = BTreeSet::new();

    if let Some(database_path) = find_database(&indexeddb_directory, "PersistentEntityStoreDb") {
        let connection = open_snapshot(&snapshot(&database_path, snapshot_directory.path())?)?;
        for key in store_keys(&connection, "EntityStore")? {
            let Some(entity) = idb::parse_entity_key(&key) else {
                continue;
            };
            if entity.id.starts_with(ALBUM_ID_PREFIX) {
                marked_album_browse_ids.insert(entity.id);
                continue;
            }
            match entity.entity_type {
                DOWNLOAD_STATUS_ENTITY | MUSIC_TRACK_DOWNLOAD_METADATA_ENTITY => {
                    marked_video_ids.insert(entity.id);
                }
                MUSIC_PLAYLIST_DOWNLOAD_METADATA_ENTITY => {
                    marked_playlist_ids.insert(entity.id);
                }
                _ => {}
            }
        }
    }

    if let Some(database_path) = find_database(&indexeddb_directory, "yt-player-local-media") {
        let connection = open_snapshot(&snapshot(&database_path, snapshot_directory.path())?)?;
        for key in store_keys(&connection, "index")? {
            if let Some(video_id) = key.strip_suffix(LOCAL_AUDIO_SUFFIX) {
                stored_audio_video_ids.insert(video_id.to_owned());
            }
        }
    }

    Ok(Marks {
        tracks: marked_video_ids.into_iter().collect(),
        playlists: marked_playlist_ids.into_iter().collect(),
        albums: marked_album_browse_ids.into_iter().collect(),
        stored_locally: stored_audio_video_ids.into_iter().collect(),
    })
}

/// Every decoded key of one object store.
fn store_keys(connection: &Connection, object_store_name: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "select object_data.key from object_data
         join object_store on object_store.id = object_data.object_store_id
         where object_store.name = ?1",
    )?;
    let rows = statement.query_map([object_store_name], |row| row.get::<_, Vec<u8>>(0))?;
    Ok(rows
        .filter_map(std::result::Result::ok)
        .filter_map(|encoded_key| idb::decode_key(&encoded_key))
        .collect())
}

/// Firefox database filenames do not reveal the stored database name.
///
/// Firefox mangles the file name, so the name inside the file is the only
/// reliable handle. `immutable=1` reads that one row without a lock and
/// without writing an index file beside the running browser's data.
fn find_database(indexeddb_directory: &Path, database_name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(indexeddb_directory).ok()?;
    for entry in entries.filter_map(std::result::Result::ok) {
        let database_path = entry.path();
        if database_path.extension().and_then(|extension| extension.to_str()) != Some("sqlite") {
            continue;
        }
        let database_uri = format!("file:{}?immutable=1", uri_escape(&database_path));
        let open_flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
        let Ok(connection) = Connection::open_with_flags(&database_uri, open_flags) else {
            continue;
        };
        let stored_database_name: std::result::Result<String, _> =
            connection.query_row("select name from database", [], |row| row.get(0));
        // The name carries the signed-in account ids after a colon.
        if let Ok(stored_database_name) = stored_database_name
            && stored_database_name.split(':').next() == Some(database_name)
        {
            return Some(database_path);
        }
    }
    None
}

fn uri_escape(path: &Path) -> String {
    path.to_string_lossy()
        .bytes()
        .map(|byte| match byte {
            b'?' | b'#' | b'%' => format!("%{byte:02X}"),
            _ => char::from(byte).to_string(),
        })
        .collect()
}

/// The WAL is copied with its database so committed browser writes stay visible.
fn snapshot(database_path: &Path, snapshot_directory: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(snapshot_directory)?;
    let database_filename = database_path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            database_path.display().to_string(),
        )
    })?;
    let snapshot_path = snapshot_directory.join(database_filename);
    std::fs::copy(database_path, &snapshot_path)?;
    let write_ahead_log_path = sibling(database_path, "-wal");
    if write_ahead_log_path.exists() {
        std::fs::copy(&write_ahead_log_path, sibling(&snapshot_path, "-wal"))?;
    }
    Ok(snapshot_path)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut sibling_filename = path.as_os_str().to_owned();
    sibling_filename.push(suffix);
    PathBuf::from(sibling_filename)
}

/// Open a snapshot read-only, falling back to read-write on the same copy.
///
/// SQLite refuses a read-only open of a database that has a WAL and no `-shm`,
/// and Firefox leaves no `-shm` behind. The fallback writes only that index
/// file, and only inside our own copy.
fn open_snapshot(snapshot_path: &Path) -> Result<Connection> {
    match Connection::open_with_flags(snapshot_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => Ok(connection),
        Err(_) => Ok(Connection::open(snapshot_path)?),
    }
}

/// A directory that removes itself, so a failed read leaves no copy of the
/// profile behind.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(snapshot_kind: &str) -> Result<TempDir> {
        let timestamp_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "redwax-marks-{snapshot_kind}-{}-{timestamp_nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        Ok(TempDir { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "\
[General]
StartWithLastProfile=1

[Profile0]
Name=default-release
IsRelative=1
Path=w90pxhq9.default-release

[Profile1]
Name=default
IsRelative=1
Path=7eqztxmy.default
Default=1

[Install11457493C5A56847]
Default=w90pxhq9.default-release
Locked=1
";

    #[test]
    fn the_install_default_leads_and_a_name_narrows_to_one() {
        let root = Path::new("/profiles");
        assert_eq!(
            profile_dirs(INI, root, None),
            vec![
                root.join("w90pxhq9.default-release"),
                root.join("7eqztxmy.default"),
            ]
        );
        assert_eq!(
            profile_dirs(INI, root, Some("default")),
            vec![root.join("7eqztxmy.default")]
        );
        assert_eq!(
            profile_dirs(INI, root, Some("w90pxhq9.default-release")),
            vec![root.join("w90pxhq9.default-release")]
        );
        let absolute_selector = root.join("w90pxhq9.default-release");
        assert_eq!(
            profile_dirs(INI, root, absolute_selector.to_str()),
            vec![absolute_selector]
        );
        assert!(profile_dirs(INI, root, Some("nothing")).is_empty());
    }

    #[test]
    fn profile_capabilities_are_selected_independently() {
        let temp = TempDir::new("finder").unwrap();
        let root = temp.path();
        std::fs::write(root.join("profiles.ini"), INI).unwrap();
        let wanted = root.join("w90pxhq9.default-release");
        std::fs::create_dir_all(&wanted).unwrap();
        std::fs::write(wanted.join("cookies.sqlite"), []).unwrap();
        std::fs::create_dir_all(root.join("7eqztxmy.default")).unwrap();
        let found = profile_dirs(INI, root, None).into_iter().find(|dir| dir.is_dir());
        assert_eq!(found, Some(wanted));
        let cookie_profile = FirefoxProfile { dir: found.unwrap() };
        assert!(cookie_profile.supports(ProfileCapability::Cookies));
        assert!(!cookie_profile.supports(ProfileCapability::Marks));

        let marks = root.join("7eqztxmy.default");
        std::fs::create_dir_all(marks.join("storage/default/https+++music.youtube.com/idb")).unwrap();
        let profiles: Vec<_> = profile_dirs(INI, root, None)
            .into_iter()
            .filter(|dir| dir.is_dir())
            .map(|dir| FirefoxProfile { dir })
            .collect();
        assert_eq!(
            profiles
                .iter()
                .find(|profile| profile.supports(ProfileCapability::Cookies)),
            Some(&cookie_profile)
        );
        assert_eq!(
            profiles
                .iter()
                .find(|profile| profile.supports(ProfileCapability::Marks))
                .map(|profile| profile.dir.as_path()),
            Some(marks.as_path())
        );
    }

    fn cookie(name: &str, value: &str, host: &str) -> Cookie {
        Cookie {
            name: name.to_owned(),
            value: value.to_owned(),
            host: host.to_owned(),
            include_subdomains: host.starts_with('.'),
            path: "/".to_owned(),
            secure: true,
            expiry: 1_800_000_000,
        }
    }

    #[test]
    fn cookie_debug_redacts_the_browser_value() {
        let cookie = cookie("SAPISID", "private-value", ".youtube.com");
        let debug = format!("{cookie:?}");
        assert!(debug.contains("SAPISID"));
        assert!(!debug.contains("private-value"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn test_import_drops_session_tracking_cookies() {
        let all = [
            cookie("__Secure-3PAPISID", "s", ".youtube.com"),
            cookie("ST-abc", "noise", ".youtube.com"),
            cookie("ST-def", "noise", "music.youtube.com"),
            cookie("NID", "n", ".google.com"),
            cookie("SAPISID", "unrelated", "notyoutube.com"),
        ];
        let kept: Vec<&Cookie> = all.iter().filter(|found| is_youtube_cookie(found)).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "__Secure-3PAPISID");
    }

    #[test]
    fn expired_browser_cookies_do_not_enter_the_export() {
        let mut expired = cookie("SAPISID", "old", ".youtube.com");
        expired.expiry = 99;
        let mut session = cookie("PREF", "session", ".youtube.com");
        session.expiry = 0;

        assert!(!cookie_is_current(&expired, 100));
        assert!(cookie_is_current(&session, 100));
    }

    #[test]
    fn test_cookie_header_prefers_the_youtube_wide_value() {
        // The reader keeps both rows, and `Session` picks one value per name.
        // The wide host has to survive the filter before it can be preferred.
        let all = [
            cookie("SID", "yt", ".youtube.com"),
            cookie("SID", "host", "music.youtube.com"),
            cookie("HSID", "h", ".youtube.com"),
        ];
        let kept: Vec<&Cookie> = all.iter().filter(|found| is_youtube_cookie(found)).collect();
        assert_eq!(kept.len(), 3);
        assert!(
            kept.iter()
                .any(|found| found.name == "SID" && found.host == ".youtube.com")
        );
    }

    #[test]
    fn netscape_output_is_the_seven_column_form_at_mode_600() {
        let temp = TempDir::new("netscape").unwrap();
        let path = temp.path().join("cookies.txt");
        let cookies = [
            cookie("SAPISID", "secret", ".youtube.com"),
            Cookie {
                secure: false,
                ..cookie("PREF", "f1=x", "music.youtube.com")
            },
        ];
        write_netscape(&cookies, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# Netscape HTTP Cookie File\n\
             # This file was generated by redwax. Do not edit.\n\
             \n\
             .youtube.com\tTRUE\t/\tTRUE\t1800000000\tSAPISID\tsecret\n\
             music.youtube.com\tFALSE\t/\tFALSE\t1800000000\tPREF\tf1=x\n"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn browser_export_reader_keeps_only_current_google_and_youtube_rows() {
        let temp = TempDir::new("browser-export").unwrap();
        let path = temp.path().join("cookies.txt");
        std::fs::write(
            &path,
            "# Netscape HTTP Cookie File\n\
             .youtube.com\tTRUE\t/\tTRUE\t4102444800\tSAPISID\tsigning\n\
             .google.com\tTRUE\t/\tTRUE\t4102444800\tSID\taccount\n\
             .youtube.com\tTRUE\t/\tTRUE\t1\tOLD\texpired\n\
             .example.com\tTRUE\t/\tTRUE\t4102444800\tPRIVATE\tunrelated\n",
        )
        .unwrap();

        let cookies = read_netscape(&path).unwrap();

        assert_eq!(cookies.len(), 2);
        assert!(cookies.iter().any(|cookie| cookie.name == "SAPISID"));
        assert!(cookies.iter().any(|cookie| cookie.name == "SID"));
    }

    #[test]
    fn netscape_round_trip_preserves_the_explicit_domain_flag() {
        let temp = TempDir::new("domain-flag").unwrap();
        let path = temp.path().join("cookies.txt");
        std::fs::write(
            &path,
            ".youtube.com\tFALSE\t/\tTRUE\t4102444800\tSAPISID\tsigning\n\
             youtube.com\tTRUE\t/\tTRUE\t4102444800\tSID\taccount\n",
        )
        .unwrap();
        let cookies = read_netscape(&path).unwrap();
        assert!(
            !cookies
                .iter()
                .find(|cookie| cookie.name == "SAPISID")
                .unwrap()
                .include_subdomains
        );
        assert!(
            cookies
                .iter()
                .find(|cookie| cookie.name == "SID")
                .unwrap()
                .include_subdomains
        );
        write_netscape(&cookies, &path).unwrap();
        assert_eq!(read_netscape(&path).unwrap(), cookies);
    }

    #[test]
    fn a_snapshot_takes_the_wal_with_it() {
        let temp = TempDir::new("snapshot").unwrap();
        let source = temp.path().join("source");
        let into = temp.path().join("into");
        std::fs::create_dir_all(&source).unwrap();
        let db = source.join("store.sqlite");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(source.join("store.sqlite-wal"), b"wal").unwrap();
        let copy = snapshot(&db, &into).unwrap();
        assert_eq!(copy, into.join("store.sqlite"));
        assert_eq!(std::fs::read(&copy).unwrap(), b"main");
        assert_eq!(std::fs::read(into.join("store.sqlite-wal")).unwrap(), b"wal");
    }
}
