//! Deterministic tests for external-file reconciliation, reload, and the
//! save paths that must keep the active file's baseline.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use postvim::{Document, Editor, EditorInput as I, EditorMode};
use tempfile::{TempDir, tempdir};

fn command(editor: &mut Editor, text: &str) {
    editor.handle_input(I::Character(':'));
    editor.handle_input(I::Text(text.to_owned()));
    editor.handle_input(I::Enter);
}

fn insert(editor: &mut Editor, text: &str) {
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Text(text.to_owned()));
    editor.handle_input(I::Escape);
}

#[test]
fn missing_file_with_symlink_parent_dotdot_keeps_native_destination() {
    let directory = tempdir().unwrap();
    let work = directory.path().join("work");
    let elsewhere = directory.path().join("elsewhere");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&elsewhere).unwrap();
    fs::create_dir(elsewhere.join("child")).unwrap();
    symlink(elsewhere.join("child"), work.join("link")).unwrap();
    let unrelated = work.join("new");
    fs::write(&unrelated, "unrelated bytes").unwrap();
    let native_path = work.join("link/../new");
    let mut editor = Editor::new(Some(native_path.clone())).unwrap();
    assert_eq!(editor.document().serialize(), "");
    assert_eq!(editor.path(), Some(native_path.as_path()));
    insert(&mut editor, "new document");
    command(&mut editor, "w");
    assert!(!editor.is_dirty(), "{:?}", editor.message());
    assert_eq!(fs::read(elsewhere.join("new")).unwrap(), b"new document");
    assert_eq!(fs::read(unrelated).unwrap(), b"unrelated bytes");

    let invalid = work.join("nonexistent/../new");
    assert!(Editor::new(Some(invalid)).is_err());
    assert_eq!(fs::read(work.join("new")).unwrap(), b"unrelated bytes");
}

/// A monotonic clock that moves one poll interval per sample, so tests never
/// sleep and every poll is far enough from the previous one to confirm.
struct Clock(Instant);

impl Clock {
    fn new() -> Self {
        Self(Instant::now())
    }

    fn sample(&mut self, editor: &mut Editor) -> bool {
        self.0 += Duration::from_millis(600);
        let now = self.0;
        editor.poll_disk(now)
    }

    fn samples(&mut self, editor: &mut Editor) -> bool {
        let first = self.sample(editor);
        let second = self.sample(editor);
        first || second
    }
}

struct Fixture {
    directory: TempDir,
    path: PathBuf,
    editor: Editor,
    clock: Clock,
}

impl Fixture {
    fn new(text: &str) -> Self {
        let directory = tempdir().unwrap();
        let path = directory.path().join("note.txt");
        fs::write(&path, text).unwrap();
        let mut editor = Editor::new(Some(path.clone())).unwrap();
        editor.handle_input(I::Character('l'));
        editor.set_cursor(postvim::Cursor::default());
        Self {
            directory,
            path,
            editor,
            clock: Clock::new(),
        }
    }

    fn directory(&self) -> &Path {
        self.directory.path()
    }

    fn text(&self) -> String {
        self.editor.document().serialize()
    }

    fn replace_disk(&self, text: &str) {
        fs::write(&self.path, text).unwrap();
    }

    fn replace_disk_bytes(&self, bytes: &[u8]) {
        fs::write(&self.path, bytes).unwrap();
    }

    fn disk(&self) -> String {
        fs::read_to_string(&self.path).unwrap()
    }

    /// Polls twice: the first sample is cached, the second confirms it.
    fn confirm(&mut self) -> bool {
        self.clock.samples(&mut self.editor)
    }

    fn status(&self) -> Option<String> {
        self.editor.external_status().map(str::to_owned)
    }
}

// ------------------------------------------------------------ clean reloads

#[test]
fn external_clean_reload_installs_one_undo_step() {
    let mut fixture = Fixture::new("one\n");
    // A single sample can never change anything, and an unchanged file never
    // adds history.
    assert!(!fixture.clock.sample(&mut fixture.editor));
    assert_eq!(fixture.text(), "one\n");
    assert_eq!(fixture.status(), None);

    fixture.replace_disk("two\n");
    assert!(
        !fixture.clock.sample(&mut fixture.editor),
        "one sample only"
    );
    assert_eq!(fixture.text(), "one\n");
    assert!(
        fixture.clock.sample(&mut fixture.editor),
        "confirmed change"
    );

    assert_eq!(fixture.text(), "two\n");
    assert!(!fixture.editor.is_dirty());
    assert_eq!(fixture.status(), None);
    assert_eq!(fixture.disk(), "two\n", "polling never writes");

    fixture.editor.handle_input(I::Character('u'));
    assert_eq!(fixture.text(), "one\n");
    assert!(fixture.editor.is_dirty(), "undone text is unsaved text");
    fixture.editor.handle_input(I::Redo);
    assert_eq!(fixture.text(), "two\n");
    assert!(!fixture.editor.is_dirty());
    assert_eq!(fixture.disk(), "two\n", "undo and redo never write");
}

#[test]
fn external_reload_needs_two_equal_samples() {
    let mut fixture = Fixture::new("one\n");
    // A truncate-then-complete pair must not be shown as an intermediate file.
    fixture.replace_disk("");
    assert!(!fixture.clock.sample(&mut fixture.editor));
    assert_eq!(fixture.text(), "one\n");
    fixture.replace_disk("two\n");
    assert!(
        !fixture.clock.sample(&mut fixture.editor),
        "samples disagree"
    );
    assert_eq!(fixture.text(), "one\n");
    // The settled contents are still reported at the next confirmation.
    assert!(fixture.clock.sample(&mut fixture.editor));
    assert_eq!(fixture.text(), "two\n");
}

#[test]
fn external_reload_is_deferred_at_unsafe_boundaries() {
    // Each case is a state a reload must not interrupt, plus the keys that
    // return the editor to an idle Normal buffer.
    let cases: [(&str, Vec<I>, Vec<I>); 4] = [
        ("pending count", vec![I::Character('2')], vec![I::Escape]),
        ("visual selection", vec![I::Character('v')], vec![I::Escape]),
        ("open prompt", vec![I::Character(':')], vec![I::Escape]),
        (
            "open insert session",
            vec![I::Character('i')],
            vec![I::Escape],
        ),
    ];
    for (label, prefix, release) in cases {
        let mut fixture = Fixture::new("one\n");
        for input in prefix {
            fixture.editor.handle_input(input);
        }
        fixture.replace_disk("two\n");
        // The status may appear at once, but the text must not change.
        fixture.confirm();
        assert_eq!(fixture.text(), "one\n", "{label}: text must survive");
        assert!(
            fixture.editor.external_status().is_some(),
            "{label}: status"
        );

        for input in release {
            fixture.editor.handle_input(input);
        }
        fixture.confirm();
        assert_eq!(fixture.text(), "two\n", "{label}: reconciled when idle");
        assert_eq!(fixture.status(), None, "{label}: status cleared");
    }
}

#[test]
fn dirty_buffer_keeps_local_text_and_reports_conflict() {
    let mut fixture = Fixture::new("one\n");
    insert(&mut fixture.editor, "local ");
    fixture.replace_disk("two\n");
    fixture.confirm();

    assert_eq!(fixture.text(), "local one\n");
    assert!(fixture.editor.is_dirty());
    assert_eq!(fixture.disk(), "two\n", "foreign bytes are preserved");
    let status = fixture.status().expect("conflict status");
    assert!(status.contains(":reload!"), "{status}");
}

#[test]
fn foreign_edit_that_matches_local_text_stays_dirty() {
    let mut fixture = Fixture::new("orig");
    insert(&mut fixture.editor, "x");
    // The foreign writer produced exactly the local text, which still must not
    // be adopted as saved.
    fixture.replace_disk("xorig");
    fixture.confirm();

    assert_eq!(fixture.text(), "xorig");
    assert!(fixture.editor.is_dirty());
    assert!(fixture.status().expect("conflict").contains(":reload!"));
}

#[test]
fn reverting_a_foreign_edit_clears_the_conflict() {
    let mut fixture = Fixture::new("orig");
    insert(&mut fixture.editor, "x");
    fixture.replace_disk("foreign");
    fixture.confirm();
    assert!(fixture.editor.external_status().is_some());

    fixture.replace_disk("orig");
    fixture.confirm();
    assert_eq!(fixture.status(), None);
    assert_eq!(fixture.text(), "xorig");
    assert!(fixture.editor.is_dirty());
}

#[test]
fn unchanged_polls_are_noops() {
    let mut fixture = Fixture::new("steady\n");
    for _ in 0..4 {
        assert!(!fixture.confirm());
    }
    assert!(fixture.editor.message().is_none());
    fixture.editor.handle_input(I::Character('u'));
    assert_eq!(
        fixture.editor.message(),
        Some("Already at oldest change"),
        "polling must not add history"
    );
}

#[test]
fn atomic_replacement_with_equal_bytes_is_not_a_change() {
    let mut fixture = Fixture::new("same\n");
    // Replace the file atomically with the same bytes.
    let replacement = fixture.directory().join("replacement");
    fs::write(&replacement, "same\n").unwrap();
    fs::rename(&replacement, &fixture.path).unwrap();

    assert!(!fixture.confirm());
    assert_eq!(fixture.text(), "same\n");
    assert_eq!(fixture.status(), None);
    fixture.editor.handle_input(I::Character('u'));
    assert_eq!(fixture.editor.message(), Some("Already at oldest change"));
}

#[test]
fn deletion_and_unreadable_states_preserve_text() {
    let mut deleted = Fixture::new("keep\n");
    fs::remove_file(&deleted.path).unwrap();
    deleted.confirm();
    assert_eq!(deleted.text(), "keep\n");
    assert!(!deleted.editor.is_dirty(), "unchanged text stays clean");
    let status = deleted.status().expect("unavailable status");
    assert!(status.contains("removed"), "{status}");

    let mut invalid = Fixture::new("keep\n");
    invalid.replace_disk_bytes(&[0xff, 0xfe, 0x00]);
    invalid.confirm();
    assert_eq!(invalid.text(), "keep\n");
    let status = invalid.status().expect("invalid UTF-8 status");
    assert!(status.to_lowercase().contains("utf-8"), "{status}");

    let mut nonregular = Fixture::new("keep\n");
    fs::remove_file(&nonregular.path).unwrap();
    fs::create_dir(&nonregular.path).unwrap();
    nonregular.confirm();
    assert_eq!(nonregular.text(), "keep\n");
    let status = nonregular.status().expect("nonregular status");
    assert!(status.contains("regular file"), "{status}");
}

#[test]
fn unnamed_buffers_never_poll() {
    let mut editor = Editor::new(None).unwrap();
    let now = Instant::now() + Duration::from_secs(5);
    assert!(!editor.poll_disk(now));
    assert!(editor.next_poll_at().is_none());
}

#[test]
fn retargeted_alias_with_equal_bytes_only_moves_the_expectation() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("first.txt");
    let second = directory.path().join("second.txt");
    let link = directory.path().join("link.txt");
    fs::write(&first, "same\n").unwrap();
    fs::write(&second, "same\n").unwrap();
    symlink(&first, &link).unwrap();
    let mut editor = Editor::new(Some(link.clone())).unwrap();
    let mut clock = Clock::new();

    fs::remove_file(&link).unwrap();
    symlink(&second, &link).unwrap();
    clock.samples(&mut editor);

    assert_eq!(editor.document().serialize(), "same\n");
    assert_eq!(editor.external_status(), None);
    editor.handle_input(I::Character('u'));
    assert_eq!(
        editor.message(),
        Some("Already at oldest change"),
        "a target-only change adds no text history"
    );

    insert(&mut editor, "new ");
    command(&mut editor, "w");
    assert!(!editor.is_dirty());
    assert_eq!(fs::read_to_string(&second).unwrap(), "new same\n");
    assert_eq!(fs::read_to_string(&first).unwrap(), "same\n");
}

// --------------------------------------------------------------- save paths

#[test]
fn alias_to_the_retargeted_destination_cannot_bypass_conflict() {
    // Open through a symlink to A, edit locally, retarget that symlink onto B,
    // then save through two *other* names for B: a sibling symlink and a
    // symlinked parent directory. Neither is the stored target and neither
    // matches the active spelling, so both must still be judged against the
    // baseline captured at open instead of a fresh save-as expectation.
    let directory = tempdir().unwrap();
    let root = directory.path();
    let target = root.join("a.txt");
    let parent_b = root.join("b");
    fs::create_dir(&parent_b).unwrap();
    let other_target = parent_b.join("file.txt");
    let link = root.join("link.txt");
    let sibling = root.join("sibling.txt");
    let parent_alias = root.join("parent-b");
    fs::write(&target, "opened").unwrap();
    fs::write(&other_target, "foreign").unwrap();
    symlink(&target, &link).unwrap();
    symlink(&other_target, &sibling).unwrap();
    symlink(&parent_b, &parent_alias).unwrap();

    let mut editor = Editor::new(Some(link.clone())).unwrap();
    insert(&mut editor, "local ");
    fs::remove_file(&link).unwrap();
    symlink(&other_target, &link).unwrap();

    for candidate in [
        sibling.clone(),
        parent_alias.join("file.txt"),
        other_target.clone(),
    ] {
        command(&mut editor, &format!("w {}", candidate.display()));
        assert_eq!(
            fs::read_to_string(&other_target).unwrap(),
            "foreign",
            "{candidate:?} must not replace foreign bytes"
        );
        assert_eq!(
            editor.document().serialize(),
            "local opened",
            "{candidate:?}"
        );
        assert!(editor.is_dirty(), "{candidate:?}");
        assert_eq!(editor.path(), Some(link.as_path()), "{candidate:?}");
        let message = editor.message().expect("message");
        assert!(message.contains("refused"), "{candidate:?}: {message}");
    }
    assert_eq!(fs::read_to_string(&target).unwrap(), "opened");
}

#[test]
fn same_byte_retarget_reports_conflict_when_dirty() {
    let directory = tempdir().unwrap();
    let target = directory.path().join("a.txt");
    let other = directory.path().join("b.txt");
    let link = directory.path().join("link.txt");
    fs::write(&target, "same").unwrap();
    fs::write(&other, "same").unwrap();
    symlink(&target, &link).unwrap();
    let mut editor = Editor::new(Some(link.clone())).unwrap();
    let mut clock = Clock::new();
    insert(&mut editor, "local ");

    fs::remove_file(&link).unwrap();
    symlink(&other, &link).unwrap();
    clock.samples(&mut editor);

    // Identical bytes, different destination, dirty buffer: a conflict.
    assert_eq!(editor.document().serialize(), "local same");
    assert!(editor.is_dirty());
    let status = editor.external_status().expect("conflict status");
    assert!(status.contains(":reload!"), "{status}");

    // The baseline did not move, so a write through the active path refuses
    // rather than silently replacing the newly named file.
    command(&mut editor, "w");
    assert_eq!(fs::read_to_string(&other).unwrap(), "same");
    assert_eq!(fs::read_to_string(&target).unwrap(), "same");
    assert_eq!(editor.document().serialize(), "local same");
    assert!(editor.is_dirty());
    let message = editor.message().expect("message");
    assert!(message.contains("refused"), "{message}");

    // Retargeting back to the expected destination clears the status without
    // touching the text or the history.
    fs::remove_file(&link).unwrap();
    symlink(&target, &link).unwrap();
    clock.samples(&mut editor);
    assert_eq!(editor.external_status(), None);
    assert_eq!(editor.document().serialize(), "local same");
    assert!(editor.is_dirty());
    editor.handle_input(I::Character('u'));
    assert_eq!(editor.document().serialize(), "same");
    editor.handle_input(I::Redo);

    // A matching baseline saves again, and the other file is untouched.
    command(&mut editor, "w");
    assert_eq!(fs::read_to_string(&target).unwrap(), "local same");
    assert_eq!(fs::read_to_string(&other).unwrap(), "same");
    assert!(!editor.is_dirty());
}

#[test]
fn same_byte_retarget_pends_while_clean_but_busy() {
    let directory = tempdir().unwrap();
    let target = directory.path().join("a.txt");
    let other = directory.path().join("b.txt");
    let link = directory.path().join("link.txt");
    fs::write(&target, "same").unwrap();
    fs::write(&other, "same").unwrap();
    symlink(&target, &link).unwrap();
    let mut editor = Editor::new(Some(link.clone())).unwrap();
    let mut clock = Clock::new();

    // The buffer is clean, but an open prompt is not a reload boundary.
    editor.handle_input(I::Character(':'));
    fs::remove_file(&link).unwrap();
    symlink(&other, &link).unwrap();
    clock.samples(&mut editor);

    assert_eq!(editor.document().serialize(), "same");
    let status = editor.external_status().expect("pending status");
    assert!(status.contains("waiting"), "{status}");

    // Closing the prompt reaches an idle boundary: the destination is
    // refreshed with no text history and no undo step.
    editor.handle_input(I::Escape);
    clock.samples(&mut editor);
    assert_eq!(editor.external_status(), None);
    assert_eq!(editor.document().serialize(), "same");
    editor.handle_input(I::Character('u'));
    assert_eq!(
        editor.message(),
        Some("Already at oldest change"),
        "refreshing the destination adds no history"
    );

    // The refreshed destination is where a save now goes.
    insert(&mut editor, "new ");
    command(&mut editor, "w");
    assert_eq!(fs::read_to_string(&other).unwrap(), "new same");
    assert_eq!(fs::read_to_string(&target).unwrap(), "same");
    assert!(!editor.is_dirty());
}

#[test]
fn guarded_save_rejects_external_changes() {
    let mut fixture = Fixture::new("orig");
    insert(&mut fixture.editor, "local ");
    // No poll ever ran: the save-time check alone must refuse.
    fixture.replace_disk("foreign");
    let active = fixture.editor.path().unwrap().to_path_buf();

    command(&mut fixture.editor, "w");

    assert_eq!(fixture.disk(), "foreign");
    assert_eq!(fixture.text(), "local orig");
    assert!(fixture.editor.is_dirty());
    assert_eq!(fixture.editor.path(), Some(active.as_path()));
    assert!(!fixture.editor.should_quit());
    let message = fixture.editor.message().expect("message");
    assert!(message.contains("refused"), "{message}");
}

#[test]
fn save_aliases_cannot_bypass_conflict() {
    // A symlink is retargeted after a foreign write, and every spelling of the
    // files behind it must still use the baseline captured at open.
    let symlink_case = || {
        let directory = tempdir().unwrap();
        let target = directory.path().join("a.txt");
        let other = directory.path().join("b.txt");
        let link = directory.path().join("link.txt");
        fs::write(&target, "opened").unwrap();
        fs::write(&other, "other").unwrap();
        symlink(&target, &link).unwrap();
        let mut editor = Editor::new(Some(link.clone())).unwrap();
        insert(&mut editor, "local ");
        fs::write(&target, "foreign").unwrap();
        fs::remove_file(&link).unwrap();
        symlink(&other, &link).unwrap();
        let dotted = directory.path().join("./a.txt");
        (directory, target, other, link, editor, dotted)
    };

    for spelling in 0..4 {
        let (directory, target, other, link, mut editor, dotted) = symlink_case();
        let candidate = match spelling {
            0 => target.clone(),
            1 => link.clone(),
            2 => other.clone(),
            _ => dotted.clone(),
        };
        command(&mut editor, &format!("w {}", candidate.display()));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "foreign",
            "spelling {candidate:?} must not replace the foreign edit"
        );
        assert_eq!(fs::read_to_string(&other).unwrap(), "other");
        assert_eq!(editor.document().serialize(), "local opened");
        assert!(editor.is_dirty());
        let message = editor.message().expect("message");
        assert!(message.contains("refused"), "{candidate:?}: {message}");
        drop(directory);
    }

    // A retargeted parent directory is equally covered.
    let directory = tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let parent = directory.path().join("current");
    symlink(&first, &parent).unwrap();
    let in_first = first.join("file.txt");
    fs::write(&in_first, "opened").unwrap();
    let opened = parent.join("file.txt");
    let mut editor = Editor::new(Some(opened.clone())).unwrap();
    insert(&mut editor, "local ");
    fs::write(&in_first, "foreign").unwrap();
    fs::remove_file(&parent).unwrap();
    symlink(&second, &parent).unwrap();
    command(&mut editor, "w");
    assert_eq!(fs::read_to_string(&in_first).unwrap(), "foreign");
    assert!(!second.join("file.txt").exists());
    assert_eq!(editor.document().serialize(), "local opened");
    assert!(editor.is_dirty());

    // A deleted target still resolves lexically, so the old baseline applies.
    let directory = tempdir().unwrap();
    let target = directory.path().join("a.txt");
    let link = directory.path().join("link.txt");
    fs::write(&target, "opened").unwrap();
    symlink(&target, &link).unwrap();
    let mut editor = Editor::new(Some(link.clone())).unwrap();
    insert(&mut editor, "local ");
    fs::write(&target, "foreign").unwrap();
    fs::remove_file(&target).unwrap();
    command(&mut editor, &format!("w {}", target.display()));
    assert!(!target.exists(), "a refused save creates nothing");
    let message = editor.message().expect("message");
    assert!(message.contains("refused"), "{message}");
    command(&mut editor, &format!("w {}", link.display()));
    assert!(!target.exists());
    assert_eq!(editor.document().serialize(), "local opened");
    assert!(editor.is_dirty());
}

#[test]
fn save_as_uses_own_expectation() {
    let mut fixture = Fixture::new("orig");
    insert(&mut fixture.editor, "local ");
    let original = fixture.path.clone();
    let copy = fixture.directory().join("copy.txt");

    command(&mut fixture.editor, &format!("w {}", copy.display()));
    assert_eq!(fs::read_to_string(&copy).unwrap(), "local orig");
    assert_eq!(fs::read_to_string(&original).unwrap(), "orig");
    assert!(!fixture.editor.is_dirty());
    assert_eq!(fixture.editor.path(), Some(copy.as_path()));

    // The unrelated old file no longer constrains this buffer.
    fs::write(&original, "foreign").unwrap();
    fixture.editor.set_cursor(postvim::Cursor::default());
    insert(&mut fixture.editor, "more ");
    command(&mut fixture.editor, "w");
    assert_eq!(fs::read_to_string(&copy).unwrap(), "more local orig");
    assert_eq!(fs::read_to_string(&original).unwrap(), "foreign");
    assert!(!fixture.editor.is_dirty());

    // An explicit save-as may still replace an unrelated regular file.
    let existing = fixture.directory().join("existing.txt");
    fs::write(&existing, "old contents").unwrap();
    command(&mut fixture.editor, &format!("w {}", existing.display()));
    assert_eq!(fs::read_to_string(&existing).unwrap(), "more local orig");

    // A failed save-as keeps path, baseline, and dirty state.
    let readonly = fixture.directory().join("readonly.txt");
    fs::write(&readonly, "stay").unwrap();
    let mut permissions = fs::metadata(&readonly).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&readonly, permissions).unwrap();
    insert(&mut fixture.editor, "edit ");
    command(&mut fixture.editor, &format!("w {}", readonly.display()));
    assert_eq!(fs::read_to_string(&readonly).unwrap(), "stay");
    assert_eq!(fixture.editor.path(), Some(existing.as_path()));
    assert!(fixture.editor.is_dirty());
    let message = fixture.editor.message().expect("message");
    assert!(message.starts_with("Write failed"), "{message}");
}

// ------------------------------------------------------------ reload command

#[test]
fn reload_commands_and_quit_contract() {
    // An unnamed buffer explains that a path is required.
    let mut unnamed = Editor::new(None).unwrap();
    command(&mut unnamed, "reload");
    let message = unnamed.message().expect("message");
    assert!(message.contains("file path"), "{message}");
    assert_eq!(unnamed.document().serialize(), "");

    // `:reload` refuses dirty text; `:reload!` replaces it as one undo step.
    let mut fixture = Fixture::new("disk\n");
    insert(&mut fixture.editor, "local ");
    command(&mut fixture.editor, "reload");
    assert_eq!(fixture.text(), "local disk\n");
    assert!(fixture.editor.is_dirty());
    assert!(
        fixture
            .editor
            .message()
            .expect("message")
            .contains(":reload!")
    );
    fixture.replace_disk("fresh\n");
    command(&mut fixture.editor, "reload!");
    assert_eq!(fixture.text(), "fresh\n");
    assert!(!fixture.editor.is_dirty());
    fixture.editor.handle_input(I::Character('u'));
    assert_eq!(fixture.text(), "local disk\n", "the discard is recoverable");
    assert!(fixture.editor.is_dirty());

    // A failed reload keeps local work: deletion and invalid bytes.
    let mut deleted = Fixture::new("here\n");
    insert(&mut deleted.editor, "local ");
    fs::remove_file(&deleted.path).unwrap();
    command(&mut deleted.editor, "reload!");
    assert_eq!(deleted.text(), "local here\n");
    assert!(deleted.editor.is_dirty());
    assert!(
        deleted
            .editor
            .message()
            .expect("message")
            .contains("removed")
    );

    let mut invalid = Fixture::new("here\n");
    insert(&mut invalid.editor, "local ");
    invalid.replace_disk_bytes(&[0x80]);
    command(&mut invalid.editor, "reload!");
    assert_eq!(invalid.text(), "local here\n");
    assert!(invalid.editor.is_dirty());
    assert!(
        invalid
            .editor
            .message()
            .expect("message")
            .to_lowercase()
            .contains("utf-8")
    );

    // Quit contracts are unchanged by reload support.
    let mut quit = Fixture::new("saved\n");
    insert(&mut quit.editor, "local ");
    command(&mut quit.editor, "q");
    assert!(!quit.editor.should_quit());
    command(&mut quit.editor, "q!");
    assert!(quit.editor.should_quit());

    let mut clean = Fixture::new("saved\n");
    command(&mut clean.editor, "x");
    assert!(clean.editor.should_quit());
    assert_eq!(clean.disk(), "saved\n", "clean :x does not write");
}

#[test]
fn reload_equal_observation_is_noop() {
    let mut fixture = Fixture::new("same\n");
    insert(&mut fixture.editor, "x");
    fixture.editor.handle_input(I::Character('u'));
    // One redo entry is available before the reload.
    command(&mut fixture.editor, "reload");
    assert_eq!(fixture.text(), "same\n");
    fixture.editor.handle_input(I::Character('u'));
    assert_eq!(
        fixture.editor.message(),
        Some("Already at oldest change"),
        "an equal reload adds no undo step"
    );
    fixture.editor.handle_input(I::Redo);
    assert_eq!(fixture.text(), "xsame\n", "redo survives an equal reload");
}

#[test]
fn reload_undo_uses_new_saved_baseline() {
    let mut fixture = Fixture::new("one\n");
    fixture.replace_disk("two\n");
    command(&mut fixture.editor, "reload");
    assert_eq!(fixture.text(), "two\n");
    assert!(!fixture.editor.is_dirty());

    fixture.editor.handle_input(I::Character('u'));
    assert_eq!(fixture.text(), "one\n");
    assert!(fixture.editor.is_dirty(), "the older text is unsaved");
    fixture.editor.handle_input(I::Redo);
    assert_eq!(fixture.text(), "two\n");
    assert!(!fixture.editor.is_dirty());

    // Undoing again and saving writes the older text on purpose.
    fixture.editor.handle_input(I::Character('u'));
    command(&mut fixture.editor, "w");
    assert_eq!(fixture.disk(), "one\n");
    assert!(!fixture.editor.is_dirty());
}

#[test]
fn reload_preserves_exact_bytes() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("mixed.txt");
    let opened = "\u{feff}one\r\ntwo\nthree\rfour";
    fs::write(&path, opened).unwrap();
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    assert_eq!(editor.document().serialize(), opened);

    let reloaded = "\u{feff}alpha\r\nbeta\rgamma\n";
    fs::write(&path, reloaded).unwrap();
    command(&mut editor, "reload");
    assert_eq!(editor.document().serialize(), reloaded);
    assert!(editor.document().has_bom());
    assert!(!editor.is_dirty());

    command(&mut editor, "w");
    assert_eq!(fs::read(&path).unwrap(), reloaded.as_bytes());
}

#[test]
fn reload_from_visual_prompt_normalizes_only_on_success() {
    // Success: the prompt returns to Visual first, then the reload settles on
    // Normal with no selection.
    let mut fixture = Fixture::new("one two\n");
    fixture.editor.handle_input(I::Character('v'));
    fixture.editor.handle_input(I::Character('l'));
    assert_eq!(fixture.editor.mode(), EditorMode::Visual);
    fixture.replace_disk("three four\n");
    command(&mut fixture.editor, "reload");
    assert_eq!(fixture.text(), "three four\n");
    assert_eq!(fixture.editor.mode(), EditorMode::Normal);
    assert!(fixture.editor.selection().is_none());

    // Failure: a dirty buffer keeps the Visual selection and the local text.
    let mut dirty = Fixture::new("one two\n");
    insert(&mut dirty.editor, "local ");
    dirty.editor.handle_input(I::Character('v'));
    dirty.editor.handle_input(I::Character('l'));
    dirty.replace_disk("three four\n");
    command(&mut dirty.editor, "reload");
    assert_eq!(dirty.text(), "local one two\n");
    assert_eq!(dirty.editor.mode(), EditorMode::Visual);
    assert!(dirty.editor.selection().is_some());
    assert!(
        dirty
            .editor
            .message()
            .expect("message")
            .contains(":reload!")
    );
}

#[test]
fn cursor_is_clamped_after_a_shorter_reload() {
    let mut fixture = Fixture::new("first\nsecond\nthird\n");
    fixture
        .editor
        .set_cursor(postvim::Cursor::new(2, "third".len()));
    fixture.replace_disk("only\n");
    command(&mut fixture.editor, "reload");
    assert_eq!(fixture.text(), "only\n");
    // The approximate row is clamped into the shorter document, and the column
    // stays on a grapheme-safe Normal position.
    let cursor = fixture.editor.cursor();
    assert!(cursor.row < fixture.editor.document().line_count());
    assert_eq!(
        cursor.column,
        fixture
            .editor
            .document()
            .normal_cursor_col(cursor.row, cursor.column)
    );
}

#[test]
fn documents_without_a_path_still_edit() {
    // A baseline-free editor keeps working exactly as before.
    let mut editor = Editor::from_document(Document::from_text("text\n"));
    insert(&mut editor, "more ");
    assert!(editor.is_dirty());
    assert!(!editor.poll_disk(Instant::now() + Duration::from_secs(1)));
}
