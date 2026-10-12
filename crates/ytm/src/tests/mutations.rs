//! The five layout changes that broke ytmusicapi in August 2026, applied to a
//! captured response in memory.
//!
//! Each one leaves the data where it was and moves the shape around it. The
//! walker must read the same items out of both, which is the whole point of
//! not addressing by path.

use crate::test_common::fixture;
use crate::{Item, walker};
use serde_json::{Map, Value, json};

/// Item count and first item, which together catch both a dropped run of items
/// and a shifted one.
fn reading(root: &Value) -> (usize, Option<Item>) {
    let items = walker::items(root);
    (items.len(), items.first().cloned())
}

#[test]
fn a_new_wrapper_around_every_shelf_changes_nothing() {
    let original = fixture("search_songs.json");
    let mutated = map_objects(&original, &|object| {
        let shelf = object.get("musicShelfRenderer")?;
        let inner = json!({ "musicShelfRenderer": shelf.clone() });
        Some(json!({ "somethingNewRenderer": { "content": inner } }))
    });
    assert_ne!(mutated, original, "the mutation did not apply");
    assert_eq!(reading(&mutated), reading(&original));
    assert_eq!(shelf_titles(&mutated), shelf_titles(&original));
}

#[test]
fn dropping_every_thumbnail_leaves_the_items() {
    let original = fixture("search_songs.json");
    let mutated = map_objects(&original, &|object| {
        if !object.contains_key("thumbnail") {
            return None;
        }
        let mut copy = object.clone();
        copy.remove("thumbnail");
        Some(Value::Object(copy))
    });
    let (count, first) = reading(&mutated);
    let (was_count, was_first) = reading(&original);
    assert_eq!(count, was_count);

    let mut expected = was_first.expect("the fixture has items");
    if let Item::Track(track) = &mut expected {
        track.thumbnail_url = None;
    }
    assert_eq!(first, Some(expected));
}

#[test]
fn an_unknown_renderer_between_items_is_skipped() {
    let original = fixture("search_songs.json");
    let mutated = map_arrays(&original, &|entries| {
        if !entries.iter().any(is_item) {
            return None;
        }
        let mut out = Vec::with_capacity(entries.len() * 2);
        for entry in entries {
            if is_item(entry) {
                out.push(json!({ "unknownRenderer": {} }));
            }
            out.push(entry.clone());
        }
        Some(out)
    });
    assert_ne!(mutated, original, "the mutation did not apply");
    assert_eq!(reading(&mutated), reading(&original));
}

#[test]
fn reordered_flex_columns_still_find_the_title() {
    let original = fixture("playlist.json");
    let mutated = map_objects(&original, &|object| {
        let columns = object.get("flexColumns")?.as_array()?;
        let mut copy = object.clone();
        let mut reversed = columns.clone();
        reversed.reverse();
        copy.insert("flexColumns".to_string(), Value::Array(reversed));
        Some(Value::Object(copy))
    });
    assert_ne!(mutated, original, "the mutation did not apply");
    assert_eq!(reading(&mutated), reading(&original));
}

#[test]
fn a_renamed_header_key_keeps_the_shelf_titles() {
    let original = fixture("home.json");
    let mutated = map_objects(&original, &|object| {
        let header = object.get("header")?;
        let mut copy = object.clone();
        copy.remove("header");
        copy.insert("headerRenamed".to_string(), header.clone());
        Some(Value::Object(copy))
    });
    assert_ne!(mutated, original, "the mutation did not apply");
    assert_eq!(reading(&mutated), reading(&original));
    assert_eq!(shelf_titles(&mutated), shelf_titles(&original));
}

fn shelf_titles(root: &Value) -> Vec<String> {
    walker::shelves(root, "For you")
        .into_iter()
        .filter(|shelf| !shelf.items.is_empty())
        .map(|shelf| shelf.title)
        .collect()
}

fn is_item(entry: &Value) -> bool {
    walker::item(entry).is_some()
}

/// Rewrite every object bottom up, so a replacement is built from children that
/// have already been rewritten.
fn map_objects(node: &Value, change: &dyn Fn(&Map<String, Value>) -> Option<Value>) -> Value {
    match node {
        Value::Array(entries) => {
            Value::Array(entries.iter().map(|entry| map_objects(entry, change)).collect())
        }
        Value::Object(object) => {
            let mapped: Map<String, Value> = object
                .iter()
                .map(|(key, value)| (key.clone(), map_objects(value, change)))
                .collect();
            change(&mapped).unwrap_or(Value::Object(mapped))
        }
        other => other.clone(),
    }
}

fn map_arrays(node: &Value, change: &dyn Fn(&[Value]) -> Option<Vec<Value>>) -> Value {
    match node {
        Value::Array(entries) => {
            let mapped: Vec<Value> = entries.iter().map(|entry| map_arrays(entry, change)).collect();
            Value::Array(change(&mapped).unwrap_or(mapped))
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), map_arrays(value, change)))
                .collect(),
        ),
        other => other.clone(),
    }
}
