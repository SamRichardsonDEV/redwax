use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const VIDEO_ID_LEN: usize = 11;
const AUDIO_EXTENSIONS: [&str; 8] = ["webm", "m4a", "mp4", "opus", "ogg", "mka", "mp3", "flac"];

/// Audio files indexed by the YouTube id in their yt-dlp filename.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryIndex {
    files: HashMap<String, PathBuf>,
}

impl LibraryIndex {
    /// Dotted directories are excluded. Directory aliases are followed only once.
    pub fn scan(directory: &Path) -> LibraryIndex {
        let mut index = LibraryIndex::default();
        index.collect(directory, &mut HashSet::new());
        index
    }

    pub fn get(&self, video_id: &str) -> Option<&Path> {
        self.files.get(video_id).map(PathBuf::as_path)
    }

    pub fn contains(&self, video_id: &str) -> bool {
        self.files.contains_key(video_id)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.files
            .iter()
            .map(|(video_id, audio_path)| (video_id.as_str(), audio_path.as_path()))
    }

    fn collect(&mut self, directory: &Path, visited_directories: &mut HashSet<PathBuf>) {
        let Ok(canonical_directory) = directory.canonicalize() else {
            return;
        };
        if !visited_directories.insert(canonical_directory) {
            return;
        }
        let Ok(directory_entries) = std::fs::read_dir(directory) else {
            return;
        };
        for directory_entry in directory_entries.flatten() {
            let entry_path = directory_entry.path();
            let Some(filename) = entry_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if filename.starts_with('.') {
                continue;
            }
            if entry_path.is_dir() {
                self.collect(&entry_path, visited_directories);
            } else if is_audio(&entry_path)
                && let Some(video_id) = video_id_in(filename)
            {
                self.files.insert(video_id.to_owned(), entry_path);
            }
        }
    }
}

fn is_audio(file_path: &Path) -> bool {
    file_path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| AUDIO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Read the last bracket pair so brackets in a title cannot win.
fn video_id_in(filename: &str) -> Option<&str> {
    let closing_bracket_index = filename.rfind(']')?;
    let opening_bracket_index = filename[..closing_bracket_index].rfind('[')?;
    let candidate_video_id = &filename[opening_bracket_index + 1..closing_bracket_index];
    let is_plausible_video_id = candidate_video_id.len() == VIDEO_ID_LEN
        && candidate_video_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    is_plausible_video_id.then_some(candidate_video_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn scan_follows_directory_links_without_looping_back_into_the_library() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("redwax-library-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let library = root.join("library");
        let album = root.join("album");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("Track [Abc_123-xYz].opus"), []).unwrap();
        symlink(&album, library.join("linked-album")).unwrap();
        symlink(&library, album.join("back-to-library")).unwrap();

        let index = LibraryIndex::scan(&library);

        assert_eq!(index.len(), 1);
        assert!(index.get("Abc_123-xYz").unwrap().is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_uses_the_last_exact_video_marker_and_skips_dotted_directories() {
        let root = std::env::temp_dir().join(format!("redwax-library-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("album")).expect("create visible library");
        std::fs::create_dir_all(root.join(".evicted")).expect("create hidden library");
        std::fs::write(root.join("album/Title [words] [Abc_123-xYz].OPUS"), []).expect("write visible track");
        std::fs::write(root.join("album/Second [Mka_123-xYz].MkA"), [])
            .expect("write mixed case Matroska track");
        std::fs::write(root.join("album/Third [Mp4_123-xYz].mP4"), []).expect("write mixed case MP4 track");
        std::fs::write(root.join("album/short [abc].opus"), []).expect("write invalid marker");
        std::fs::write(root.join(".evicted/Hidden [Zyx-987_AbC].opus"), []).expect("write hidden track");

        let index = LibraryIndex::scan(&root);

        assert_eq!(index.len(), 3);
        assert!(index.contains("Abc_123-xYz"));
        assert!(index.contains("Mka_123-xYz"));
        assert!(index.contains("Mp4_123-xYz"));
        assert!(!index.contains("words"));
        assert!(!index.contains("Zyx-987_AbC"));
        let _ = std::fs::remove_dir_all(root);
    }
}
