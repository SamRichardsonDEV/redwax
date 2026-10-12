//! Fixture loading shared by the fixture and mutation tests.

#![allow(dead_code)]

use crate::{Track, walker};
use serde_json::Value;

/// One captured response, read from the repository's `fixtures` directory.
pub fn fixture(name: &str) -> Value {
    let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The tracks of the first shelf that holds any, which is how a page of tracks
/// is read.
pub fn tracks_in(name: &str) -> Vec<Track> {
    walker::shelves(&fixture(name), "Tracks")
        .into_iter()
        .find(|shelf| shelf.tracks().next().is_some())
        .map(|shelf| shelf.tracks().cloned().collect())
        .unwrap_or_default()
}
