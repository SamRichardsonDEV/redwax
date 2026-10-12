use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_directory(directory: &Path) -> io::Result<()> {
    ensure_parent_preserving_existing(directory)?;
    set_mode(directory, 0o700)?;

    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            set_mode(&entry.path(), 0o600)?;
        }
    }
    Ok(())
}

pub(crate) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    StagedWrite::new(path, contents)?.publish()
}

pub(crate) struct StagedWrite {
    destination_path: PathBuf,
    temporary_path: PathBuf,
    destination_directory: PathBuf,
}

impl StagedWrite {
    pub(crate) fn new(destination_path: &Path, contents: &[u8]) -> io::Result<Self> {
        let parent = parent_directory(destination_path)?;
        let directory = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ensure_parent_preserving_existing(parent)?;
            parent
        };
        let (temporary_path, mut temporary_file) = create_temporary(destination_path)?;
        let staged = Self {
            destination_path: destination_path.to_path_buf(),
            temporary_path,
            destination_directory: directory.to_path_buf(),
        };
        temporary_file.write_all(contents)?;
        temporary_file.sync_all()?;
        Ok(staged)
    }

    pub(crate) fn publish(self) -> io::Result<()> {
        fs::rename(&self.temporary_path, &self.destination_path)?;
        sync_directory(&self.destination_directory)
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary_path);
    }
}

fn ensure_parent_preserving_existing(directory: &Path) -> io::Result<()> {
    let mut missing_directories = Vec::new();
    let mut ancestor_directory = directory;
    while !ancestor_directory.as_os_str().is_empty() && !ancestor_directory.exists() {
        missing_directories.push(ancestor_directory.to_path_buf());
        let Some(parent) = ancestor_directory.parent() else {
            break;
        };
        ancestor_directory = parent;
    }
    for path in missing_directories.into_iter().rev() {
        match create_private_directory(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn create_temporary(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    loop {
        let temporary_path = temporary_path(path);
        match private_options().open(&temporary_path) {
            Ok(temporary_file) => return Ok((temporary_path, temporary_file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn parent_directory(path: &Path) -> io::Result<&Path> {
    match path.parent() {
        Some(parent) => Ok(parent),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private file has no parent directory",
        )),
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let temporary_sequence = NEXT_TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut temporary_filename = path.as_os_str().to_owned();
    temporary_filename.push(format!(".{}.{}.new", std::process::id(), temporary_sequence));
    PathBuf::from(temporary_filename)
}

fn private_options() -> OpenOptions {
    let mut open_options = OpenOptions::new();
    open_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_options.mode(0o600);
    }
    open_options
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut directory_builder = fs::DirBuilder::new();
    directory_builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_preparation_makes_existing_cache_storage_private() {
        let directory = std::env::temp_dir().join(format!("redwax-private-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("old.json"), "old").unwrap();

        prepare_directory(&directory).unwrap();
        write(&directory.join("new.json"), b"new").unwrap();

        assert_eq!(fs::read(directory.join("new.json")).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(directory.join("old.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(directory.join("new.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_preserves_an_existing_generic_parent_and_its_siblings() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!("redwax-private-parent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let sibling = directory.join("unrelated");
        fs::write(&sibling, "held").unwrap();
        fs::set_permissions(&sibling, fs::Permissions::from_mode(0o644)).unwrap();

        write(&directory.join("state.json"), b"state").unwrap();

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&sibling).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(directory.join("state.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
