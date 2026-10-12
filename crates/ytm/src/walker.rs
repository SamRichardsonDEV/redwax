//! Tolerant extraction from any YouTube Music response.
//!
//! YouTube changes layout, not data. Fixed pointer paths break when a wrapper
//! is inserted, an optional leaf is dropped, an unknown renderer appears in a
//! list, or an index moves. The walker never addresses by path. It walks the
//! whole document in order and recognises items by the keys that have not
//! changed in years: `videoId`, `browseId`, `playlistId`, `runs[].text`,
//! `thumbnails`. Mutation tests cover changes to the surrounding layout.

use serde_json::Value;

use crate::THUMBNAIL_SIZE;
use crate::models::{Album, Artist, Item, Playlist, Shelf, Track};

/// Renderer keys that carry one item. An object holding none of them is a
/// container, a decoration, or a renderer redwax does not show.
const ITEM_RENDERERS: [&str; 5] = [
    "musicResponsiveListItemRenderer",
    "musicTwoRowItemRenderer",
    "playlistPanelVideoRenderer",
    "musicMultiRowListItemRenderer",
    "musicCardShelfRenderer",
];

/// Renderer keys that open a shelf. The card shelf is both: its card is an item
/// and its contents are more items.
const SHELF_RENDERERS: [&str; 6] = [
    "musicShelfRenderer",
    "musicCarouselShelfRenderer",
    "musicCardShelfRenderer",
    "musicPlaylistShelfRenderer",
    "gridRenderer",
    "sectionListRenderer",
];

/// Subtrees that decorate an item rather than describe it. A menu repeats the
/// artist, the album and the video id, so a scan that reads menus labels a
/// playlist row as one of its own tracks.
const DECORATION: [&str; 14] = [
    "counterpart",
    "menu",
    "overlay",
    "thumbnailOverlay",
    "buttons",
    "badges",
    "subtitleBadges",
    "accessibility",
    "multiSelectCheckbox",
    "playButton",
    "startRadioButton",
    "moreContentButton",
    "shuffleToggleButton",
    "playlistEditParams",
];

/// Keys holding other items. An item is never described by its children.
const NESTED: [&str; 2] = ["contents", "items"];

/// Labels YouTube prints as the first subtitle run. They name the kind when no
/// id does.
const LABELS: [&str; 12] = [
    "Song", "Video", "Artist", "Playlist", "Album", "EP", "Single", "Episode", "Podcast", "Profile", "Show",
    "Channel",
];

/// Kinds redwax neither plays nor opens.
const DROPPED_LABELS: [&str; 4] = ["Episode", "Podcast", "Profile", "Show"];

/// Every shelf in `root`, in document order. A shelf is the nearest ancestor
/// of a run of items that carries a header with `runs`. Items with no titled
/// ancestor land in a shelf titled `fallback_title`.
pub fn shelves(root: &Value, fallback_title: &str) -> Vec<Shelf> {
    let mut parsed_shelves = Vec::new();
    let mut untitled_shelf_index = None;
    walk(
        root,
        None,
        &mut untitled_shelf_index,
        fallback_title,
        &mut parsed_shelves,
    );
    parsed_shelves
}

/// Every item under `node`, in document order, without shelf grouping.
pub fn items(node: &Value) -> Vec<Item> {
    let mut parsed_items = Vec::new();
    collect_items(node, &mut parsed_items);
    parsed_items
}

/// One item from an object holding an item renderer, else `None`.
///
/// A `videoId` makes a track. A `browseId` starting `MPRE` makes an album,
/// `UC` an artist, `VL` or a `playlistId` a playlist. Podcasts (`MPSP`) and
/// episodes are `None`. The title is the first `runs[].text` of the first flex
/// column or the `title` runs. Artists are runs whose navigation carries a `UC`
/// browse id, the album the run with an `MPRE` one. Duration is parsed from any
/// `h:mm:ss` or `m:ss` run. Thumbnails are taken from anywhere in the item and
/// resized to `THUMBNAIL_SIZE`. Like status comes from a `likeStatus` leaf.
pub fn item(node: &Value) -> Option<Item> {
    let object = node.as_object()?;
    let key = ITEM_RENDERERS.iter().find(|key| object.contains_key(**key))?;
    parse_item(object.get(*key)?)
}

/// The concatenated `text` of a `runs` array, or of `simpleText`.
pub fn text(node: &Value) -> Option<String> {
    if let Some(simple) = node.get("simpleText").and_then(Value::as_str) {
        return non_empty(simple.to_string());
    }
    let runs = node.get("runs")?.as_array()?;
    let joined: String = runs
        .iter()
        .filter_map(|run| run.get("text").and_then(Value::as_str))
        .collect();
    non_empty(joined)
}

/// The largest thumbnail URL under `node`, rewritten to `THUMBNAIL_SIZE`.
pub fn thumbnail(node: &Value) -> Option<String> {
    let mut largest_thumbnail_candidate: Option<(u64, &str)> = None;
    largest_thumbnail(node, &mut largest_thumbnail_candidate);
    largest_thumbnail_candidate.map(|(_, url)| resize(url))
}

/// `1:02:03` or `4:19` to seconds.
pub fn parse_length(text: &str) -> Option<u32> {
    let text = text.trim();
    let mut duration_parts = text.split(':').rev();
    let seconds = two_digits(duration_parts.next()?)?;
    let minutes: u32 = duration_parts.next()?.parse().ok()?;
    let hours: u32 = match duration_parts.next() {
        Some(hours_text) => hours_text.parse().ok()?,
        None => 0,
    };
    if duration_parts.next().is_some() || seconds >= 60 || minutes >= 60 {
        return None;
    }
    Some(hours * 3600 + minutes * 60 + seconds)
}

// ---------------------------------------------------------------------------
// Walking
// ---------------------------------------------------------------------------

fn walk(
    node: &Value,
    parent_shelf_index: Option<usize>,
    untitled_shelf_index: &mut Option<usize>,
    fallback_title: &str,
    parsed_shelves: &mut Vec<Shelf>,
) {
    match node {
        Value::Array(entries) => {
            for entry in entries {
                walk(
                    entry,
                    parent_shelf_index,
                    untitled_shelf_index,
                    fallback_title,
                    parsed_shelves,
                );
            }
        }
        Value::Object(object) => {
            if let Some(key) = SHELF_RENDERERS.iter().find(|key| object.contains_key(**key)) {
                let shelf_renderer = &object[*key];
                parsed_shelves.push(Shelf::new(
                    shelf_title(shelf_renderer).unwrap_or_default(),
                    Vec::new(),
                ));
                let opened_shelf_index = parsed_shelves.len() - 1;
                if ITEM_RENDERERS.contains(key)
                    && let Some(parsed_item) = parse_item(shelf_renderer)
                {
                    parsed_shelves[opened_shelf_index].items.push(parsed_item);
                }
                walk(
                    shelf_renderer,
                    Some(opened_shelf_index),
                    untitled_shelf_index,
                    fallback_title,
                    parsed_shelves,
                );
                return;
            }
            if let Some(parsed_item) = item(node) {
                let destination_shelf_index = match parent_shelf_index {
                    Some(destination_shelf_index) => destination_shelf_index,
                    None => *untitled_shelf_index.get_or_insert_with(|| {
                        parsed_shelves.push(Shelf::new(fallback_title.to_string(), Vec::new()));
                        parsed_shelves.len() - 1
                    }),
                };
                parsed_shelves[destination_shelf_index].items.push(parsed_item);
                return;
            }
            for (key, value) in object {
                if DECORATION.contains(&key.as_str()) {
                    continue;
                }
                walk(
                    value,
                    parent_shelf_index,
                    untitled_shelf_index,
                    fallback_title,
                    parsed_shelves,
                );
            }
        }
        _ => {}
    }
}

fn collect_items(node: &Value, parsed_items: &mut Vec<Item>) {
    match node {
        Value::Array(entries) => {
            for entry in entries {
                collect_items(entry, parsed_items);
            }
        }
        Value::Object(object) => {
            if let Some(parsed_item) = item(node) {
                parsed_items.push(parsed_item);
                for body in object.values() {
                    for key in NESTED {
                        if let Some(nested) = body.get(key) {
                            collect_items(nested, parsed_items);
                        }
                    }
                }
                return;
            }
            for (key, value) in object {
                if DECORATION.contains(&key.as_str()) {
                    continue;
                }
                collect_items(value, parsed_items);
            }
        }
        _ => {}
    }
}

/// The shelf's own title. A child naming a title wins over one naming a header,
/// so a `bottomText` sitting beside both is never mistaken for the title.
fn shelf_title(body: &Value) -> Option<String> {
    let object = body.as_object()?;
    let mut candidates: Vec<(u8, &Value)> = Vec::new();
    for (key, value) in object {
        if NESTED.contains(&key.as_str()) || !value.is_object() {
            continue;
        }
        let normalized_key = key.to_lowercase();
        let title_priority = if normalized_key == "title" {
            0
        } else if normalized_key.contains("header") || normalized_key.contains("strapline") {
            1
        } else if normalized_key.contains("title") {
            2
        } else {
            3
        };
        candidates.push((title_priority, value));
    }
    candidates.sort_by_key(|(title_priority, _)| *title_priority);
    candidates.into_iter().find_map(|(_, value)| titled(value, 3))
}

fn titled(node: &Value, remaining_depth: u8) -> Option<String> {
    if remaining_depth == 0 {
        return None;
    }
    let object = node.as_object()?;
    if let Some(title_text) = text(node) {
        return Some(title_text);
    }
    if let Some(title) = object.get("title")
        && let Some(title_text) = titled(title, remaining_depth - 1)
    {
        return Some(title_text);
    }
    object
        .iter()
        .filter(|(key, _)| !NESTED.contains(&key.as_str()))
        .find_map(|(_, value)| titled(value, remaining_depth - 1))
}

// ---------------------------------------------------------------------------
// One item
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Run {
    text: String,
    browse_id: Option<String>,
    video_id: Option<String>,
}

impl Run {
    fn from(node: &Value) -> Run {
        let endpoint = node.get("navigationEndpoint");
        Run {
            text: node.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            browse_id: endpoint
                .and_then(|node| find_str(node, "browseId"))
                .map(str::to_string),
            video_id: endpoint
                .and_then(|node| find_str(node, "videoId"))
                .map(str::to_string),
        }
    }

    fn is_separator(&self) -> bool {
        let trimmed = self.text.trim();
        trimmed.is_empty() || trimmed == "\u{2022}" || trimmed == "\u{b7}" || trimmed == "-"
    }
}

/// The item's own words: its title, and every run group that follows it.
struct ItemText {
    title: Option<String>,
    detail: Vec<Vec<Run>>,
}

fn parse_item(item_renderer: &Value) -> Option<Item> {
    let parsed_text = item_text(item_renderer);
    if let Some(label) = kind_label(&parsed_text.detail)
        && DROPPED_LABELS.contains(&label)
    {
        return None;
    }

    let browse_id = primary_browse_id(item_renderer);
    if let Some(page_browse_id) = &browse_id
        && (page_browse_id.starts_with("MPED") || page_browse_id.starts_with("MPSP"))
    {
        return None;
    }

    let title = parsed_text
        .title
        .clone()
        .unwrap_or_else(|| "Untitled".to_string());
    let thumbnail_url = thumbnail_of(item_renderer);

    if let Some(video_id) = find_str(item_renderer, "videoId") {
        let (album, album_browse_id) = album_of(&parsed_text);
        return Some(Item::Track(Track {
            video_id: video_id.to_string(),
            title,
            artists: artists_of(&parsed_text),
            album,
            album_browse_id,
            duration_seconds: duration_of(item_renderer, &parsed_text),
            thumbnail_url,
            liked: liked(item_renderer),
            playlist_entry: find_str(item_renderer, "playlistSetVideoId")
                .map(str::to_string)
                .map(crate::PlaylistEntry::new),
        }));
    }

    let page_browse_id = browse_id.unwrap_or_default();
    if page_browse_id.starts_with("MPRE") {
        return Some(Item::Album(Album {
            browse_id: page_browse_id,
            title,
            artists: artists_of(&parsed_text),
            year: year_of(&parsed_text),
            thumbnail_url,
        }));
    }
    // A library artist is the channel id behind an `MPLA` page prefix, and the
    // channel id is what opens the artist page.
    let artist_channel_id = page_browse_id.strip_prefix("MPLA").unwrap_or(&page_browse_id);
    if artist_channel_id.starts_with("UC") {
        return Some(Item::Artist(Artist {
            channel_id: artist_channel_id.to_string(),
            name: title,
            thumbnail_url,
        }));
    }
    let playlist_id = page_browse_id
        .strip_prefix("VL")
        .map(str::to_string)
        .or_else(|| find_str(item_renderer, "playlistId").map(str::to_string))?;
    Some(Item::Playlist(Playlist {
        playlist_id,
        title,
        subtitle: parsed_text.detail.first().map(|runs| join(runs)),
        thumbnail_url,
    }))
}

fn item_text(body: &Value) -> ItemText {
    let flex_columns = column_runs(body, "flexColumns");
    if !flex_columns.is_empty() {
        // The title column is the one linking to the track, so a reordering of
        // the columns still finds it.
        let title_column_index = flex_columns
            .iter()
            .position(|runs| runs.iter().any(|run| run.video_id.is_some()))
            .unwrap_or(0);
        let mut detail: Vec<Vec<Run>> = flex_columns
            .iter()
            .enumerate()
            .filter(|(column_index, _)| *column_index != title_column_index)
            .map(|(_, runs)| runs.clone())
            .collect();
        detail.extend(column_runs(body, "fixedColumns"));
        detail.retain(|runs| !runs.is_empty());
        return ItemText {
            title: non_empty(join(&flex_columns[title_column_index])),
            detail,
        };
    }

    let mut text_groups = Vec::new();
    collect_groups(body, "", &mut text_groups);
    let title_group_index = text_groups
        .iter()
        .position(|(key, _)| key == "title")
        .unwrap_or(0);
    let title = text_groups
        .get(title_group_index)
        .and_then(|(_, runs)| non_empty(join(runs)));
    let detail = text_groups
        .into_iter()
        .enumerate()
        .filter(|(group_index, _)| *group_index != title_group_index)
        .map(|(_, (_, runs))| runs)
        .collect();
    ItemText { title, detail }
}

/// Runs of every column under `key`, one entry per column, empty columns kept
/// so the caller can still tell which column is which.
fn column_runs(body: &Value, key: &str) -> Vec<Vec<Run>> {
    let Some(columns) = find_array(body, key) else {
        return Vec::new();
    };
    columns
        .iter()
        .map(|column| {
            let mut text_groups = Vec::new();
            collect_groups(column, "", &mut text_groups);
            text_groups
                .into_iter()
                .next()
                .map(|(_, runs)| runs)
                .unwrap_or_default()
        })
        .collect()
}

fn collect_groups(node: &Value, parent_key: &str, text_groups: &mut Vec<(String, Vec<Run>)>) {
    match node {
        Value::Array(entries) => {
            for entry in entries {
                collect_groups(entry, parent_key, text_groups);
            }
        }
        Value::Object(object) => {
            if let Some(runs) = object.get("runs").and_then(Value::as_array) {
                text_groups.push((parent_key.to_string(), runs.iter().map(Run::from).collect()));
                return;
            }
            if let Some(simple_text) = object.get("simpleText").and_then(Value::as_str) {
                let run = Run {
                    text: simple_text.to_string(),
                    browse_id: None,
                    video_id: None,
                };
                text_groups.push((parent_key.to_string(), vec![run]));
                return;
            }
            for (key, value) in object {
                if DECORATION.contains(&key.as_str()) || NESTED.contains(&key.as_str()) {
                    continue;
                }
                let child_key = if value.is_object() {
                    key.as_str()
                } else {
                    parent_key
                };
                collect_groups(value, child_key, text_groups);
            }
        }
        _ => {}
    }
}

fn join(runs: &[Run]) -> String {
    runs.iter().map(|run| run.text.as_str()).collect()
}

fn kind_label(detail: &[Vec<Run>]) -> Option<&'static str> {
    let first = detail.iter().find(|runs| !runs.is_empty())?;
    let text = first.first()?.text.trim();
    LABELS.iter().copied().find(|label| *label == text)
}

fn primary_browse_id(body: &Value) -> Option<String> {
    for key in ["navigationEndpoint", "title", "onTap"] {
        if let Some(node) = body.get(key)
            && let Some(id) = find_str(node, "browseId")
        {
            return Some(id.to_string());
        }
    }
    None
}

fn artists_of(parsed_text: &ItemText) -> Vec<String> {
    let linked_artist_names: Vec<String> = parsed_text
        .detail
        .iter()
        .flatten()
        .filter(|run| run.browse_id.as_deref().is_some_and(|id| id.starts_with("UC")) && !run.is_separator())
        .map(|run| run.text.trim().to_string())
        .collect();
    if !linked_artist_names.is_empty() {
        return dedupe(linked_artist_names);
    }
    parsed_text
        .detail
        .iter()
        .find_map(|runs| unlinked_artists(runs))
        .unwrap_or_default()
}

/// The first subtitle segment that is a name rather than a count, a year or a
/// duration, split the way YouTube joins several artists.
fn unlinked_artists(runs: &[Run]) -> Option<Vec<String>> {
    for run in runs {
        if run.is_separator() {
            continue;
        }
        let text = run.text.trim();
        if is_metadata(text) {
            continue;
        }
        let artist_names: Vec<String> = text
            .split([',', '&'])
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        if !artist_names.is_empty() {
            return Some(artist_names);
        }
    }
    None
}

/// True for the counts, years and durations YouTube prints beside a name.
fn is_metadata(text: &str) -> bool {
    if LABELS.contains(&text) || parse_length(text).is_some() || is_year(text) {
        return true;
    }
    is_count(text)
}

/// `1.2M views`, `24 songs`, `3 plays`: a number then a count word. A name
/// that starts with a digit, like `21 Savage`, is not one.
fn is_count(text: &str) -> bool {
    const COUNT_WORDS: [&str; 12] = [
        "view",
        "views",
        "play",
        "plays",
        "song",
        "songs",
        "track",
        "tracks",
        "subscriber",
        "subscribers",
        "listener",
        "listeners",
    ];
    let mut words = text.split_whitespace();
    let (Some(number), Some(unit), None) = (words.next(), words.next(), words.next()) else {
        return false;
    };
    let is_numeric_count = number
        .chars()
        .all(|character| character.is_ascii_digit() || matches!(character, '.' | ',' | 'K' | 'M' | 'B'))
        && number.starts_with(|character: char| character.is_ascii_digit());
    is_numeric_count && COUNT_WORDS.contains(&unit.to_ascii_lowercase().as_str())
}

fn is_year(text: &str) -> bool {
    text.len() == 4 && text.chars().all(|character| character.is_ascii_digit())
}

fn year_of(parsed_text: &ItemText) -> Option<String> {
    parsed_text
        .detail
        .iter()
        .flatten()
        .map(|run| run.text.trim())
        .find(|text| is_year(text))
        .map(str::to_string)
}

fn album_of(parsed_text: &ItemText) -> (Option<String>, Option<String>) {
    for run in parsed_text.detail.iter().flatten() {
        if let Some(album_browse_id) = run.browse_id.as_deref()
            && album_browse_id.starts_with("MPRE")
        {
            return (
                non_empty(run.text.trim().to_string()),
                Some(album_browse_id.to_string()),
            );
        }
    }
    (None, None)
}

fn duration_of(body: &Value, parsed_text: &ItemText) -> Option<u32> {
    let duration_from_runs = parsed_text
        .detail
        .iter()
        .flatten()
        .find_map(|run| parse_length(&run.text));
    if duration_from_runs.is_some() {
        return duration_from_runs;
    }
    body.get("lengthText")
        .and_then(text)
        .as_deref()
        .and_then(parse_length)
}

fn liked(body: &Value) -> bool {
    let mut is_liked = false;
    like_status(body, &mut is_liked);
    is_liked
}

fn like_status(node: &Value, is_liked: &mut bool) {
    if *is_liked {
        return;
    }
    match node {
        Value::Array(entries) => {
            for entry in entries {
                like_status(entry, is_liked);
            }
        }
        Value::Object(object) => {
            if object.get("likeStatus").and_then(Value::as_str) == Some("LIKE") {
                *is_liked = true;
                return;
            }
            let is_toggled = object.get("isToggled").and_then(Value::as_bool) == Some(true);
            let targets_like_action = object
                .get("likeEndpoint")
                .and_then(|node| node.get("status"))
                .and_then(Value::as_str)
                == Some("LIKE");
            if is_toggled && targets_like_action {
                *is_liked = true;
                return;
            }
            for (key, value) in object {
                if NESTED.contains(&key.as_str()) {
                    continue;
                }
                like_status(value, is_liked);
            }
        }
        _ => {}
    }
}

fn thumbnail_of(body: &Value) -> Option<String> {
    let mut largest_thumbnail_candidate: Option<(u64, &str)> = None;
    largest_thumbnail(body, &mut largest_thumbnail_candidate);
    largest_thumbnail_candidate.map(|(_, url)| resize(url))
}

fn largest_thumbnail<'a>(node: &'a Value, largest_thumbnail_candidate: &mut Option<(u64, &'a str)>) {
    match node {
        Value::Array(entries) => {
            for entry in entries {
                largest_thumbnail(entry, largest_thumbnail_candidate);
            }
        }
        Value::Object(object) => {
            if let Some(url) = object.get("url").and_then(Value::as_str)
                && object.contains_key("width")
            {
                let width = object.get("width").and_then(Value::as_u64).unwrap_or(0);
                let height = object.get("height").and_then(Value::as_u64).unwrap_or(0);
                let area_pixels = width * height;
                if largest_thumbnail_candidate
                    .is_none_or(|(previous_area_pixels, _)| area_pixels > previous_area_pixels)
                {
                    *largest_thumbnail_candidate = Some((area_pixels, url));
                }
            }
            for (key, value) in object {
                if NESTED.contains(&key.as_str()) || DECORATION.contains(&key.as_str()) {
                    continue;
                }
                largest_thumbnail(value, largest_thumbnail_candidate);
            }
        }
        _ => {}
    }
}

/// Google's image host serves any square from the same URL, so a 60 pixel row
/// thumbnail becomes the sidebar art without a second request.
fn resize(url: &str) -> String {
    let Some(size_parameter_offset) = url.find("=w") else {
        return url.to_string();
    };
    let width_parameter = &url[size_parameter_offset + 2..];
    let width_digit_count = digits(width_parameter);
    if width_digit_count == 0 {
        return url.to_string();
    }
    let Some(height_parameter) = width_parameter[width_digit_count..].strip_prefix("-h") else {
        return url.to_string();
    };
    let height_digit_count = digits(height_parameter);
    if height_digit_count == 0 {
        return url.to_string();
    }
    let thumbnail_size_pixels = THUMBNAIL_SIZE;
    format!(
        "{}=w{thumbnail_size_pixels}-h{thumbnail_size_pixels}{}",
        &url[..size_parameter_offset],
        &height_parameter[height_digit_count..]
    )
}

fn digits(text: &str) -> usize {
    text.chars().take_while(char::is_ascii_digit).count()
}

fn find_str<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    match node {
        Value::Array(entries) => entries.iter().find_map(|entry| find_str(entry, key)),
        Value::Object(object) => {
            if let Some(matching_text) = object.get(key).and_then(Value::as_str) {
                return Some(matching_text);
            }
            object
                .iter()
                .filter(|(name, _)| plain(name))
                .find_map(|(_, value)| find_str(value, key))
        }
        _ => None,
    }
}

fn find_array<'a>(node: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    match node {
        Value::Array(entries) => entries.iter().find_map(|entry| find_array(entry, key)),
        Value::Object(object) => {
            if let Some(matching_array) = object.get(key).and_then(Value::as_array) {
                return Some(matching_array);
            }
            object
                .iter()
                .filter(|(name, _)| plain(name))
                .find_map(|(_, value)| find_array(value, key))
        }
        _ => None,
    }
}

fn plain(key: &str) -> bool {
    !DECORATION.contains(&key) && !NESTED.contains(&key)
}

fn two_digits(part: &str) -> Option<u32> {
    if part.len() != 2 {
        return None;
    }
    part.parse().ok()
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn dedupe(names: Vec<String>) -> Vec<String> {
    let mut unique_names: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        if !unique_names.contains(&name) {
            unique_names.push(name);
        }
    }
    unique_names
}

#[cfg(test)]
mod count_tests {
    use super::is_metadata;

    #[test]
    fn counts_are_metadata_but_numeric_names_are_not() {
        assert!(is_metadata("1.2M views"));
        assert!(is_metadata("24 songs"));
        assert!(is_metadata("3:45"));
        assert!(is_metadata("2019"));
        assert!(!is_metadata("21 Savage"));
        assert!(!is_metadata("3 Doors Down"));
    }
}
