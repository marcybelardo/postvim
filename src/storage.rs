use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

pub fn read_document(path: &Path) -> io::Result<String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    let resolved = if metadata.file_type().is_symlink() {
        fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    let metadata = fs::metadata(&resolved)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a regular file",
        ));
    }
    let bytes = fs::read(resolved)?;
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Atomically replaces a regular file while preserving its permission bits.
/// The temporary file is created beside the destination so persistence stays on one filesystem.
pub fn save_document(path: &Path, contents: &[u8]) -> io::Result<()> {
    save_document_with_writer(path, contents, |temporary, contents| {
        temporary.write_all(contents)?;
        temporary.flush()
    })
}

fn save_document_with_writer(
    path: &Path,
    contents: &[u8],
    write_contents: impl FnOnce(&mut NamedTempFile, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let (destination, old_permissions) = resolve_save_destination(path)?;
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    write_contents(&mut temporary, contents)?;
    if let Some(permissions) = old_permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(&destination)
        .map(|_| ())
        .map_err(|error| error.error)
}

fn resolve_save_destination(path: &Path) -> io::Result<(PathBuf, Option<fs::Permissions>)> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::canonicalize(path)?;
            let target_metadata = fs::metadata(&target)?;
            if !target_metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "symlink target is not a regular file",
                ));
            }
            if target_metadata.permissions().readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "destination is read-only",
                ));
            }
            Ok((target, Some(target_metadata.permissions())))
        }
        Ok(metadata) => {
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
            Ok((path.to_path_buf(), Some(metadata.permissions())))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((path.to_path_buf(), None)),
        Err(error) => Err(error),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::save_document_with_writer;
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
}
