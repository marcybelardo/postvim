//! Deterministic tests for the interactive `:format` adapter.
//!
//! Every formatter is an ordinary installed program (`tr`, `cat`, `printf`, or
//! `sh -c` with an explicit program string). Tests deliberately never execute a
//! file they wrote themselves: a concurrent thread forking while such a file is
//! open for writing makes `execve` fail with `ETXTBSY`, which would make these
//! tests flaky for reasons unrelated to the editor.
#![cfg(unix)]

use std::fs;

use postvim::{Document, Editor, EditorInput as I, EditorMode, Register};
use tempfile::tempdir;

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
fn format_editor_whole_and_visual() {
    let mut editor = Editor::from_document(Document::from_text("one two\nthree\n"));

    command(&mut editor, "format tr a-z A-Z");
    assert_eq!(
        editor.document().serialize(),
        "ONE TWO\nTHREE\n",
        "message={:?}",
        editor.message()
    );
    assert!(editor.is_dirty(), "formatting is an unsaved buffer change");
    assert_eq!(editor.mode(), EditorMode::Normal);

    // The Visual selection is the input range, restored through the prompt.
    editor.set_cursor(postvim::Cursor::default());
    editor.handle_input(I::Character('y'));
    editor.handle_input(I::Character('y'));
    let register: Register = editor.register().clone();
    editor.handle_input(I::Character('v'));
    editor.handle_input(I::Character('l'));
    editor.handle_input(I::Character('l'));
    assert_eq!(editor.mode(), EditorMode::Visual);
    command(&mut editor, "format tr A-Z a-z");
    assert_eq!(
        editor.document().serialize(),
        "one TWO\nTHREE\n",
        "message={:?}",
        editor.message()
    );
    assert_eq!(editor.mode(), EditorMode::Normal);
    assert!(editor.selection().is_none());
    assert_eq!(editor.cursor(), postvim::Cursor::default());
    assert_eq!(editor.register(), &register, "formatting never yanks");

    // Each run is exactly one undo step.
    editor.handle_input(I::Character('u'));
    assert_eq!(editor.document().serialize(), "ONE TWO\nTHREE\n");
    editor.handle_input(I::Redo);
    assert_eq!(editor.document().serialize(), "one TWO\nTHREE\n");
}

#[test]
fn format_visual_ranges_are_half_open() {
    // A characterwise selection includes its last grapheme but not the line's
    // terminator, so removing newlines changes nothing here.
    let mut chars = Editor::from_document(Document::from_text("aa\nbb\ncc\n"));
    chars.handle_input(I::Character('v'));
    chars.handle_input(I::Character('l'));
    command(&mut chars, "format tr -d '\\n'");
    assert_eq!(
        chars.document().serialize(),
        "aa\nbb\ncc\n",
        "characterwise selection excludes the terminator; message={:?}",
        chars.message()
    );

    // A linewise selection includes the selected line's terminator.
    let mut lines = Editor::from_document(Document::from_text("aa\nbb\ncc\n"));
    lines.handle_input(I::Character('V'));
    command(&mut lines, "format tr -d '\\n'");
    assert_eq!(
        lines.document().serialize(),
        "aabb\ncc\n",
        "linewise selection includes the terminator; message={:?}",
        lines.message()
    );

    // A final line without a terminator stays exact.
    let mut last = Editor::from_document(Document::from_text("aa\nbb"));
    last.set_cursor(postvim::Cursor::new(1, 0));
    last.handle_input(I::Character('V'));
    command(&mut last, "format tr -d '\\n'");
    assert_eq!(last.document().serialize(), "aa\nbb");
}

#[test]
fn format_editor_history_and_failure_contract() {
    // A real change clears redo and the dot recipe, so `.` cannot re-run it.
    let mut editor = Editor::from_document(Document::from_text("abc\n"));
    insert(&mut editor, "!");
    command(&mut editor, "format tr a-z A-Z");
    assert_eq!(
        editor.document().serialize(),
        "!ABC\n",
        "message={:?}",
        editor.message()
    );
    editor.handle_input(I::Character('.'));
    assert_eq!(
        editor.message(),
        Some("No previous change"),
        "an external program is never replayed by `.`"
    );
    assert_eq!(editor.document().serialize(), "!ABC\n");

    // A no-op replacement adds no history and keeps the dot recipe.
    let mut noop = Editor::from_document(Document::from_text("abcd\n"));
    noop.handle_input(I::Character('x'));
    assert_eq!(noop.document().serialize(), "bcd\n");
    command(&mut noop, "format cat");
    assert_eq!(
        noop.document().serialize(),
        "bcd\n",
        "message={:?}",
        noop.message()
    );
    noop.handle_input(I::Character('.'));
    assert_eq!(
        noop.document().serialize(),
        "cd\n",
        "a no-op format keeps the previous change repeatable"
    );
    for _ in 0..3 {
        noop.handle_input(I::Character('u'));
    }
    assert_eq!(noop.document().serialize(), "abcd\n");

    // A formatter warning is reported without becoming file text.
    let mut warned = Editor::from_document(Document::from_text("keep\n"));
    command(&mut warned, "format sh -c \"cat; echo careful >&2\"");
    assert_eq!(
        warned.document().serialize(),
        "keep\n",
        "message={:?}",
        warned.message()
    );
    let message = warned.message().expect("message");
    assert!(message.contains("careful"), "{message}");

    // A failure leaves text, cursor, mode, and history alone.
    let mut failed = Editor::from_document(Document::from_text("stable\n"));
    failed.set_cursor(postvim::Cursor::new(0, 2));
    command(&mut failed, "format sh -c \"echo boom >&2; exit 3\"");
    assert_eq!(failed.document().serialize(), "stable\n");
    assert_eq!(failed.cursor(), postvim::Cursor::new(0, 2));
    assert_eq!(failed.mode(), EditorMode::Normal);
    assert!(!failed.is_dirty());
    let message = failed.message().expect("message");
    assert!(message.contains("Format failed"), "{message}");
    assert!(message.contains("boom"), "{message}");
    failed.handle_input(I::Character('u'));
    assert_eq!(
        failed.message(),
        Some("Already at oldest change"),
        "a failed run adds no history"
    );

    // An empty successful output is a valid deletion.
    let mut deleted = Editor::from_document(Document::from_text("gone\n"));
    command(&mut deleted, "format sh -c 'cat >/dev/null'");
    assert_eq!(
        deleted.document().serialize(),
        "",
        "message={:?}",
        deleted.message()
    );
    deleted.handle_input(I::Character('u'));
    assert_eq!(deleted.document().serialize(), "gone\n");

    // A Visual-origin failure keeps the selection and its mode.
    let mut visual = Editor::from_document(Document::from_text("abcdef\n"));
    visual.handle_input(I::Character('v'));
    visual.handle_input(I::Character('l'));
    command(&mut visual, "format sh -c \"echo nope >&2; exit 1\"");
    assert_eq!(visual.document().serialize(), "abcdef\n");
    assert_eq!(visual.mode(), EditorMode::Visual);
    assert!(visual.selection().is_some());
}

#[test]
fn format_command_grammar_and_quoting() {
    // `printf` repeats its format for each argument, so the output shows exactly
    // how many arguments arrived and what they contained.
    let mut editor = Editor::from_document(Document::new());
    command(
        &mut editor,
        "format printf '%s\\n' \"two words\" '$HOME' '>'",
    );
    assert_eq!(
        editor.document().serialize(),
        "two words\n$HOME\n>\n",
        "quoting groups words, and nothing is expanded or interpreted; message={:?}",
        editor.message()
    );

    // A bare `:format` explains the usage, and a bad quote is rejected.
    let mut grammar = Editor::from_document(Document::from_text("text\n"));
    command(&mut grammar, "format");
    assert_eq!(grammar.message(), Some("Usage: :format PROGRAM [ARG...]"));
    command(&mut grammar, "format \"unterminated");
    let message = grammar.message().expect("message");
    assert!(message.contains("Invalid formatter arguments"), "{message}");
    assert_eq!(grammar.document().serialize(), "text\n");

    // `format` only matches as a whole command word.
    for rejected in ["formatx", "format!"] {
        command(&mut grammar, rejected);
        let message = grammar.message().expect("message");
        assert!(
            message.starts_with("Unknown command"),
            "{rejected}: {message}"
        );
    }

    // A formatter that cannot start is an error, not a panic.
    command(&mut grammar, "format /nonexistent/postvim-formatter");
    let message = grammar.message().expect("message");
    assert!(message.contains("could not be started"), "{message}");
    assert_eq!(grammar.document().serialize(), "text\n");
}

#[test]
fn format_does_not_write_or_move_the_saved_baseline() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "one\n").unwrap();
    let mut editor = Editor::new(Some(path.clone())).unwrap();

    command(&mut editor, "format tr a-z A-Z");
    assert_eq!(
        editor.document().serialize(),
        "ONE\n",
        "message={:?}",
        editor.message()
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\n", "never saves");
    assert!(editor.is_dirty());
    command(&mut editor, "q");
    assert!(!editor.should_quit(), "formatted text is still unsaved");

    command(&mut editor, "w");
    assert_eq!(fs::read_to_string(&path).unwrap(), "ONE\n");
    assert!(!editor.is_dirty());
}

#[test]
fn format_keeps_an_explicit_bom_and_formatter_text() {
    let mut editor = Editor::from_document(Document::from_text("\u{feff}ab\n"));
    command(&mut editor, "format tr a-z A-Z");
    assert_eq!(
        editor.document().serialize(),
        "\u{feff}AB\n",
        "message={:?}",
        editor.message()
    );
    assert!(editor.document().has_bom());

    // A leading U+FEFF in formatter output is editable text, not a new BOM.
    let mut plain = Editor::from_document(Document::new());
    command(&mut plain, "format printf '\\357\\273\\277AB'");
    assert!(!plain.document().has_bom());
    assert_eq!(
        plain.document().serialize(),
        "\u{feff}AB",
        "message={:?}",
        plain.message()
    );
    assert_eq!(plain.document().line(0), Some("\u{feff}AB"));

    editor.handle_input(I::Character('u'));
    assert_eq!(editor.document().serialize(), "\u{feff}ab\n");
}

#[test]
fn format_that_races_a_foreign_writer_marks_a_conflict() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "one\n").unwrap();
    let mut editor = Editor::new(Some(path.clone())).unwrap();

    // The formatter itself rewrites the file while it runs.
    command(
        &mut editor,
        &format!(
            "format sh -c 'printf \"foreign\\n\" > \"$1\"; tr a-z A-Z' sh {}",
            path.display()
        ),
    );
    assert_eq!(
        editor.document().serialize(),
        "ONE\n",
        "new local text is kept; message={:?}",
        editor.message()
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "foreign\n",
        "the formatter's own side effect is out of Postvim's control"
    );
    assert!(
        editor.external_status().is_some(),
        "a raced foreign write is surfaced at once"
    );
    command(&mut editor, "w");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "foreign\n",
        "the guarded save refuses to overwrite it"
    );
    assert!(editor.is_dirty());
}
