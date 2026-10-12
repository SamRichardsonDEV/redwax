//! Response shape is useful for parser failures without retaining personal values.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// Keeps object keys and array shape. Values cannot safely be classified as public.
pub fn redact_response(response: &Value) -> Value {
    match response {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), redact_response(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_response).collect()),
        Value::String(_) => Value::String("[redacted]".to_owned()),
        Value::Number(_) => Value::from(0),
        other => other.clone(),
    }
}

/// A private temporary inode replaces existing destinations without exposing partial output.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty());
    if let Some(directory) = parent {
        fs::create_dir_all(directory)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let (temporary, mut file) = loop {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = std::path::PathBuf::from(name);
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_identity_and_stream_values_keep_only_their_shape() {
        let response = json!({"items": [{"accountName": "private", "url": "https://example.invalid/?sig=private", "id": 123456789}], "empty": null});
        assert_eq!(
            redact_response(&response),
            json!({"items": [{"accountName": "[redacted]", "url": "[redacted]", "id": 0}], "empty": null})
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacement_of_existing_public_file_has_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("redwax-diagnostic-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("response.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
