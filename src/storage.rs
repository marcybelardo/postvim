//! Filesystem storage: what a destination currently holds, and atomic
//! replacement that is refused when the destination moved underneath us.
//!
//! [`observe`] describes a path without changing it, and
//! [`save_document_guarded`] writes only while the destination still matches
//! the observation its caller began with. [`read_document`] and
//! [`save_document`] stay available as the unguarded wrappers.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use tempfile::NamedTempFile;

/// The state of one destination at a moment in time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The resolved, normalized absolute path that a save would replace.
    ///
    /// Symlinks are followed, so every name reaching the same file shares one
    /// destination, and parent-directory aliases collapse as well. A
    /// destination that does not exist yet keeps its final component
    /// unresolved and is normalized lexically.
    pub destination: PathBuf,
    /// The bytes found there, or the fact that nothing is there.
    pub contents: DiskContents,
}

/// Raw file contents, keeping "absent" distinct from "empty".
///
/// Bytes are unvalidated here: UTF-8 is a document concern, not a storage
/// fact, so invalid text is still observable byte for byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiskContents {
    Missing,
    Present(Vec<u8>),
}

/// Why a guarded save did not commit.
#[derive(Debug)]
pub enum SaveError {
    /// The destination could not be inspected or replaced.
    Io(io::Error),
    /// The destination no longer matched the caller's observation, so nothing
    /// was written and the observation the caller holds is out of date.
    Conflict {
        destination: PathBuf,
        found: DiskContents,
    },
}

impl SaveError {
    /// Whether the save was refused because the destination changed, as
    /// opposed to failing to reach the destination at all.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Conflict { destination, .. } => write!(
                formatter,
                "{} changed on disk since it was read; re-read it before saving, or save elsewhere",
                destination.display()
            ),
        }
    }
}

impl Error for SaveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Conflict { .. } => None,
        }
    }
}

impl From<io::Error> for SaveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads a document as text, treating a missing file as an empty document.
///
/// A path that exists but is not a regular file, or that holds invalid UTF-8,
/// is an error rather than an empty buffer.
pub fn read_document(path: &Path) -> io::Result<String> {
    match observe(path)?.contents {
        DiskContents::Missing => Ok(String::new()),
        DiskContents::Present(bytes) => String::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
    }
}

/// Describes what `path` currently resolves to without modifying it.
///
/// Invalid UTF-8 contents are reported as raw bytes; callers decide whether
/// that is acceptable. Symlinks are followed, so the returned destination is
/// the regular file that a save would replace.
pub fn observe(path: &Path) -> io::Result<Observation> {
    let destination = resolve_destination(path)?;
    let contents = match fs::metadata(&destination) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path is not a regular file",
                ));
            }
            DiskContents::Present(fs::read(&destination)?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => DiskContents::Missing,
        Err(error) => return Err(error),
    };
    Ok(Observation {
        destination,
        contents,
    })
}

/// Resolves `path` to the normalized absolute destination a save would replace.
///
/// Existing paths are canonicalized, so final symlinks and symlinked parent
/// directories are followed. A path whose final component does not exist keeps
/// that component, and its parent is canonicalized in the spelling the caller
/// used, so traversal resolves the way the operating system resolves it. No
/// directory is ever created.
pub fn resolve_destination(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        // A broken symlink fails here, as it did before: it names nothing to
        // observe and nothing to replace.
        Ok(_) => fs::canonicalize(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => resolve_missing_destination(path),
        Err(error) => Err(error),
    }
}

/// Resolves a destination whose final component does not exist.
///
/// The parent keeps the caller's spelling and is canonicalized as written.
/// Folding the path lexically first would name a different file: with
/// `work/link -> elsewhere/child`, `work/link/../new` is `elsewhere/new`, while
/// collapsing `link/..` would wrongly pick the unrelated `work/new`.
fn resolve_missing_destination(path: &Path) -> io::Result<PathBuf> {
    let Some(leaf) = path.file_name() else {
        // `.`, `..`, and the root name no new file.
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} does not name a file", path.display()),
        ));
    };
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    match fs::canonicalize(parent) {
        Ok(parent) => Ok(parent.join(leaf)),
        // A parent directory that does not exist cannot be resolved, but a path
        // that only walks through `.` components names nothing another spelling
        // could also reach, so it stays a missing leaf that no save will create
        // directories for. A `..` through a directory this process cannot
        // resolve names something it cannot determine, so it is rejected rather
        // than guessed: choosing a lexical answer there could name an unrelated
        // file that happens to be reachable.
        Err(error) if error.kind() == io::ErrorKind::NotFound && !walks_through_parent(path) => {
            absolute_path(path)
        }
        Err(error) => Err(error),
    }
}

/// Reports whether `path` climbs with a `..` component.
fn walks_through_parent(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

/// Makes `path` absolute, joining the current directory for relative paths.
fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Normalizes `path` lexically into an absolute path without touching the
/// filesystem.
///
/// Current-directory and parent-directory components are folded away, so two
/// spellings of one location compare equal even when it does not exist yet.
/// Symlinks are not resolved: this is a spelling canonicalization for comparing
/// aliases, not a destination resolution. Folding `..` ignores symlinks and
/// missing directories, so a lexical answer can name a different file than the
/// operating system would; use [`resolve_destination`] for the destination.
/// Climbing above the root stays at the root.
pub fn normalize_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = absolute_path(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            // Popping a root is a no-op, which is the usual lexical answer for
            // parent components above the filesystem root.
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    Ok(normalized)
}

/// Atomically replaces a regular file while preserving its permission bits.
/// The temporary file is created beside the destination so persistence stays on
/// one filesystem.
pub fn save_document(path: &Path, contents: &[u8]) -> io::Result<()> {
    save_document_with_writer(path, contents, |temporary, contents| {
        temporary.write_all(contents)?;
        temporary.flush()
    })
}

/// Atomically replaces a regular file only while it still matches `expected`.
///
/// The destination is observed on entry and again immediately before the
/// replacement; either mismatch returns [`SaveError::Conflict`] and leaves the
/// destination exactly as it was found. Permission bits of an existing file are
/// copied to the replacement, symlinks are resolved and preserved, read-only
/// destinations are refused, and missing parent directories are never created.
///
/// On success the returned observation describes what was written; the file is
/// not read back, so the caller can adopt it as its new baseline.
pub fn save_document_guarded(
    path: &Path,
    contents: &[u8],
    expected: &Observation,
) -> Result<Observation, SaveError> {
    save_guarded_with_writer(path, contents, expected, |temporary, contents| {
        temporary.write_all(contents)?;
        temporary.flush()
    })
}

fn save_document_with_writer(
    path: &Path,
    contents: &[u8],
    write_contents: impl FnOnce(&mut NamedTempFile, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let destination = resolve_destination(path)?;
    let permissions = validate_save_target(&destination)?;
    let temporary = write_temporary(&destination, contents, permissions, write_contents)?;
    temporary
        .persist(&destination)
        .map(|_| ())
        .map_err(|error| error.error)
}

/// Performs a guarded save with an injectable write step, so tests can retarget
/// the destination between the entry observation and the replacement.
fn save_guarded_with_writer(
    path: &Path,
    contents: &[u8],
    expected: &Observation,
    write_contents: impl FnOnce(&mut NamedTempFile, &[u8]) -> io::Result<()>,
) -> Result<Observation, SaveError> {
    let entry = observe(path)?;
    if entry != *expected {
        return Err(SaveError::Conflict {
            destination: entry.destination,
            found: entry.contents,
        });
    }
    let destination = entry.destination.clone();
    let permissions = validate_save_target(&destination)?;
    let temporary = write_temporary(&destination, contents, permissions, write_contents)?;
    // The write may have taken a while; replace only if the destination is
    // still what the caller was told about.
    let before_persist = observe(path)?;
    if before_persist != entry {
        return Err(SaveError::Conflict {
            destination: before_persist.destination,
            found: before_persist.contents,
        });
    }
    temporary
        .persist(&destination)
        .map_err(|error| SaveError::Io(error.error))?;
    Ok(Observation {
        destination,
        contents: DiskContents::Present(contents.to_vec()),
    })
}

/// Checks that a resolved destination may be replaced, returning the permission
/// bits to copy onto its replacement.
fn validate_save_target(destination: &Path) -> io::Result<Option<fs::Permissions>> {
    let metadata = match fs::metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination is not a regular file",
        ));
    }
    if metadata.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "destination is read-only",
        ));
    }
    Ok(Some(metadata.permissions()))
}

/// Writes `contents` into a fresh temporary file beside the destination,
/// applying `permissions` and syncing it before it can replace anything.
fn write_temporary(
    destination: &Path,
    contents: &[u8],
    permissions: Option<fs::Permissions>,
    write_contents: impl FnOnce(&mut NamedTempFile, &[u8]) -> io::Result<()>,
) -> io::Result<NamedTempFile> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    write_contents(&mut temporary, contents)?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.as_file().sync_all()?;
    Ok(temporary)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        DiskContents, SaveError, normalize_path, observe, save_document_with_writer,
        save_guarded_with_writer,
    };
    use std::fs;
    use std::io::{self, Write};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn partial_write_failure_keeps_original_and_cleans_temporary_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("document");
        fs::write(&path, b"original bytes").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let result = save_document_with_writer(&path, b"replacement", |temporary, bytes| {
            temporary.write_all(&bytes[..4])?;
            Err(io::Error::other("injected partial write failure"))
        });

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"original bytes");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn guarded_partial_write_failure_keeps_original_and_cleans_temporary_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("document");
        fs::write(&path, b"original bytes").unwrap();
        let expected = observe(&path).unwrap();

        let result =
            save_guarded_with_writer(&path, b"replacement", &expected, |temporary, bytes| {
                temporary.write_all(&bytes[..4])?;
                Err(io::Error::other("injected partial write failure"))
            });

        let error = result.unwrap_err();
        assert!(!error.is_conflict());
        assert_eq!(fs::read(&path).unwrap(), b"original bytes");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn guarded_save_refuses_a_destination_changed_midwrite() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("document");
        fs::write(&path, b"original bytes").unwrap();
        let expected = observe(&path).unwrap();

        // A concurrent writer changes the file after the entry observation and
        // before the replacement, which is the last moment a save can notice.
        let result =
            save_guarded_with_writer(&path, b"replacement", &expected, |temporary, bytes| {
                temporary.write_all(bytes)?;
                fs::write(&path, b"external edit")
            });

        let error = result.unwrap_err();
        assert!(error.is_conflict());
        assert!(matches!(
            error,
            SaveError::Conflict {
                found: DiskContents::Present(ref bytes),
                ..
            } if bytes == b"external edit"
        ));
        assert_eq!(fs::read(&path).unwrap(), b"external edit");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn normalize_path_folds_directory_components_lexically() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("real")).unwrap();

        let normalized = normalize_path(&directory.path().join("real/../real/./file")).unwrap();
        assert_eq!(normalized, directory.path().join("real/file"));
        assert!(normalized.is_absolute());
    }
}
