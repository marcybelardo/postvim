//! Guarded storage: observations, guarded atomic replacement, and the
//! filesystem guarantees the unguarded compatibility helpers keep.

use std::fs;
use std::path::Path;

use postvim::storage::{
    self, DiskContents, SaveError, normalize_path, observe, resolve_destination,
};
use tempfile::tempdir;

fn present(bytes: &[u8]) -> DiskContents {
    DiskContents::Present(bytes.to_vec())
}

#[test]
fn missing_contents_differ_from_empty_contents() {
    let directory = tempdir().unwrap();
    let absent = directory.path().join("absent");
    let missing = observe(&absent).unwrap();
    assert_eq!(missing.contents, DiskContents::Missing);
    assert!(missing.destination.is_absolute());
    assert!(!absent.exists());

    let empty = directory.path().join("empty");
    fs::write(&empty, b"").unwrap();
    assert_eq!(observe(&empty).unwrap().contents, present(b""));
    assert_ne!(observe(&empty).unwrap().contents, DiskContents::Missing);

    // `..` through a directory that exists resolves to the same absent
    // destination, because the parent is canonicalized as written.
    fs::create_dir(directory.path().join("sub")).unwrap();
    let dotted = directory.path().join("sub/../absent");
    assert_eq!(observe(&dotted).unwrap(), missing);

    assert_eq!(storage::read_document(&absent).unwrap(), "");
    assert_eq!(storage::read_document(&empty).unwrap(), "");
}

#[test]
fn observation_keeps_raw_bytes_and_rejects_nonregular_paths() {
    let directory = tempdir().unwrap();
    let raw = directory.path().join("raw");
    fs::write(&raw, [0xff, 0xfe, 0x00]).unwrap();
    assert_eq!(
        observe(&raw).unwrap().contents,
        present(&[0xff, 0xfe, 0x00])
    );
    assert_eq!(
        storage::read_document(&raw).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );

    // Observation is byte exact: BOM, mixed line endings, and no final newline.
    let exact = directory.path().join("exact");
    let bytes = b"\xef\xbb\xbfa\r\nb\nc\rd".as_slice();
    fs::write(&exact, bytes).unwrap();
    assert_eq!(observe(&exact).unwrap().contents, present(bytes));

    assert_eq!(
        observe(directory.path()).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[cfg(unix)]
#[test]
fn guarded_save_commits_and_returns_the_written_observation() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().unwrap();
    let path = directory.path().join("document");
    fs::write(&path, b"before").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let before = observe(&path).unwrap();

    let committed = storage::save_document_guarded(&path, b"after", &before).unwrap();

    assert_eq!(committed.destination, before.destination);
    assert_eq!(committed.contents, present(b"after"));
    assert_eq!(fs::read(&path).unwrap(), b"after");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(observe(&path).unwrap(), committed);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn guarded_save_refuses_a_stale_observation() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("document");
    fs::write(&path, b"before").unwrap();
    let stale = observe(&path).unwrap();
    fs::write(&path, b"external edit").unwrap();

    let error = storage::save_document_guarded(&path, b"mine", &stale).unwrap_err();

    assert!(error.is_conflict());
    let message = error.to_string();
    assert!(message.contains("changed on disk"), "{message}");
    assert!(message.contains("re-read it before saving"), "{message}");
    assert!(message.contains("save elsewhere"), "{message}");
    assert!(
        !message.contains("save again"),
        "a retry with the same observation cannot succeed: {message}"
    );
    assert!(!message.contains("force"), "{message}");
    let _: &dyn std::error::Error = &error;
    assert_eq!(fs::read(&path).unwrap(), b"external edit");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn guarded_save_notices_appearing_and_disappearing_files() {
    let directory = tempdir().unwrap();
    let appeared = directory.path().join("appeared");
    let missing = observe(&appeared).unwrap();
    fs::write(&appeared, b"someone else").unwrap();

    let error = storage::save_document_guarded(&appeared, b"mine", &missing).unwrap_err();
    assert!(matches!(
        error,
        SaveError::Conflict {
            found: DiskContents::Present(ref bytes),
            ..
        } if bytes == b"someone else"
    ));
    assert_eq!(fs::read(&appeared).unwrap(), b"someone else");

    let vanished = directory.path().join("vanished");
    fs::write(&vanished, b"gone soon").unwrap();
    let observed = observe(&vanished).unwrap();
    fs::remove_file(&vanished).unwrap();

    let error = storage::save_document_guarded(&vanished, b"mine", &observed).unwrap_err();
    assert!(matches!(
        error,
        SaveError::Conflict {
            found: DiskContents::Missing,
            ..
        }
    ));
    assert!(!vanished.exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn guarded_save_never_creates_parent_directories() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("absent-parent/file");
    let expected = observe(&path).unwrap();
    assert_eq!(expected.contents, DiskContents::Missing);

    let error = storage::save_document_guarded(&path, b"new", &expected).unwrap_err();

    assert!(!error.is_conflict());
    assert!(matches!(
        error,
        SaveError::Io(ref error) if error.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(!directory.path().join("absent-parent").exists());
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn guarded_save_refuses_readonly_destinations() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().unwrap();
    let readonly = directory.path().join("readonly");
    fs::write(&readonly, b"stay").unwrap();
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o444)).unwrap();
    let expected = observe(&readonly).unwrap();

    let error = storage::save_document_guarded(&readonly, b"bad", &expected).unwrap_err();

    assert!(!error.is_conflict());
    assert!(matches!(
        error,
        SaveError::Io(ref error) if error.kind() == std::io::ErrorKind::PermissionDenied
    ));
    assert_eq!(fs::read(&readonly).unwrap(), b"stay");
}

#[cfg(unix)]
#[test]
fn guarded_save_preserves_symlinks_and_refuses_a_retargeted_one() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let link = directory.path().join("link");
    fs::write(&first, b"first").unwrap();
    fs::set_permissions(&first, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&second, b"second").unwrap();
    symlink(&first, &link).unwrap();

    let observed = observe(&link).unwrap();
    assert_eq!(observed.destination, fs::canonicalize(&first).unwrap());

    let committed = storage::save_document_guarded(&link, b"written", &observed).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&first).unwrap(), b"written");
    assert_eq!(fs::read(&second).unwrap(), b"second");
    assert_eq!(committed.destination, observed.destination);
    assert_eq!(
        fs::metadata(&first).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Retargeting the link changes which file a save would replace, so the old
    // observation no longer describes the destination.
    fs::remove_file(&link).unwrap();
    symlink(&second, &link).unwrap();
    let error = storage::save_document_guarded(&link, b"mine", &committed).unwrap_err();
    assert!(error.is_conflict());
    assert_eq!(fs::read(&second).unwrap(), b"second");
    assert_eq!(fs::read(&first).unwrap(), b"written");
}

#[cfg(unix)]
#[test]
fn guarded_save_notices_a_retargeted_parent_directory() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let parent = directory.path().join("parent");
    let moved = directory.path().join("moved");
    fs::create_dir(&parent).unwrap();
    let path = parent.join("document");
    fs::write(&path, b"content").unwrap();
    let observed = observe(&path).unwrap();

    // The bytes stay reachable and unchanged, but only through a new name, so
    // the observed destination is gone.
    fs::rename(&parent, &moved).unwrap();
    symlink(&moved, &parent).unwrap();

    let error = storage::save_document_guarded(&path, b"mine", &observed).unwrap_err();

    assert!(error.is_conflict());
    assert_eq!(fs::read(&path).unwrap(), b"content");
    assert_eq!(fs::read(moved.join("document")).unwrap(), b"content");
}

#[cfg(unix)]
#[test]
fn missing_leaf_after_symlinked_parent_traversal_stays_native() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let work = directory.path().join("work");
    let elsewhere = directory.path().join("elsewhere");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&elsewhere).unwrap();
    fs::create_dir(elsewhere.join("child")).unwrap();
    symlink(elsewhere.join("child"), work.join("link")).unwrap();
    // Only the lexical answer for the traversal below, never its target.
    let unrelated = work.join("new");
    fs::write(&unrelated, b"unrelated bytes").unwrap();

    let path = work.join("link/../new");
    let observed = observe(&path).unwrap();
    assert_eq!(observed.contents, DiskContents::Missing);
    assert_eq!(
        observed.destination,
        fs::canonicalize(&elsewhere).unwrap().join("new")
    );
    assert_ne!(
        observed.destination,
        fs::canonicalize(&work).unwrap().join("new"),
        "collapsing the traversal lexically would name the unrelated file"
    );

    let committed = storage::save_document_guarded(&path, b"native bytes", &observed).unwrap();
    assert_eq!(committed.destination, observed.destination);
    assert_eq!(fs::read(elsewhere.join("new")).unwrap(), b"native bytes");
    assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated bytes");

    // The lexical helper exists for comparing spellings, and would name the
    // unrelated file here, so it must not be used to resolve a destination.
    assert_eq!(normalize_path(&path).unwrap(), unrelated);
}

#[test]
fn traversal_through_a_missing_directory_is_rejected() {
    let directory = tempdir().unwrap();
    let victim = directory.path().join("victim");
    fs::write(&victim, b"untouched").unwrap();

    // Lexically this names `victim`, but the operating system cannot reach it:
    // `missing` does not exist.
    let doomed = directory.path().join("missing/../victim");
    for error in [
        observe(&doomed).unwrap_err(),
        storage::read_document(&doomed).unwrap_err(),
        resolve_destination(&doomed).unwrap_err(),
    ] {
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
    }

    // An observation of the unreachable path cannot be built, so a guard refuses
    // before it could choose another target.
    let expected = observe(&victim).unwrap();
    let error = storage::save_document_guarded(&doomed, b"mine", &expected).unwrap_err();
    assert!(matches!(
        error,
        SaveError::Io(ref error) if error.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(storage::save_document(&doomed, b"mine").is_err());
    assert_eq!(fs::read(&victim).unwrap(), b"untouched");
    assert!(!directory.path().join("missing").exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);

    // The same rule applies when the traversal is the whole final component.
    assert_eq!(
        observe(&directory.path().join("missing/.."))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

#[cfg(unix)]
#[test]
fn normalization_and_destination_resolution_support_alias_checks() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let real = directory.path().join("real");
    fs::create_dir(&real).unwrap();
    let file = real.join("file");
    fs::write(&file, b"x").unwrap();
    let alias = directory.path().join("alias");
    symlink(&real, &alias).unwrap();

    // Normalization folds spelling only, so an alias keeps its own name.
    assert_eq!(normalize_path(&real.join("./../real/file")).unwrap(), file);
    let alias_normalized = normalize_path(&alias.join("file")).unwrap();
    assert_eq!(alias_normalized, alias.join("file"));
    assert_ne!(alias_normalized, file);

    // Destination resolution follows symlinks, so aliases compare equal.
    let resolved = resolve_destination(&alias.join("file")).unwrap();
    assert_eq!(resolved, fs::canonicalize(&file).unwrap());
    assert_eq!(observe(&alias.join("file")).unwrap().destination, resolved);
    assert_eq!(
        observe(&alias.join("missing")).unwrap().destination,
        fs::canonicalize(&real).unwrap().join("missing")
    );

    // Relative paths become absolute, and climbing above the root stays there.
    let relative = normalize_path(Path::new("a/./b")).unwrap();
    assert!(relative.is_absolute());
    assert_eq!(relative.file_name().unwrap(), "b");
    assert_eq!(relative.parent().unwrap().file_name().unwrap(), "a");
    assert_eq!(normalize_path(Path::new("/../..")).unwrap(), Path::new("/"));
}

#[test]
fn compatibility_helpers_keep_their_contract() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("document");
    assert_eq!(storage::read_document(&path).unwrap(), "");
    assert!(!path.exists());

    storage::save_document(&path, b"written").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"written");
    assert_eq!(storage::read_document(&path).unwrap(), "written");

    storage::save_document(&path, b"").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"");
    assert_eq!(observe(&path).unwrap().contents, present(b""));

    let raw = directory.path().join("raw");
    fs::write(&raw, [0x80]).unwrap();
    assert_eq!(
        storage::read_document(&raw).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );

    let nested = directory.path().join("absent/file");
    assert!(storage::save_document(&nested, b"x").is_err());
    assert!(!nested.exists());

    assert_eq!(
        storage::save_document(directory.path(), b"x")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}
