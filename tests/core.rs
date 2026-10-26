use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use postvim::{Cursor, Document, Editor, EditorInput as I, EditorMode, Register, SelectionKind};
use tempfile::tempdir;
use unicode_segmentation::UnicodeSegmentation;

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

fn token(name: &str) -> Option<I> {
    match name {
        "Esc" => Some(I::Escape),
        "CR" => Some(I::Enter),
        "Tab" => Some(I::Tab),
        "BS" => Some(I::Backspace),
        "Del" => Some(I::Delete),
        "Left" => Some(I::Left),
        "Right" => Some(I::Right),
        "Up" => Some(I::Up),
        "Down" => Some(I::Down),
        "C-r" => Some(I::Redo),
        _ => None,
    }
}

/// Longest name first so `\Right` is not read as `\R` and `\C-r` stays one token.
const TOKEN_NAMES: [&str; 10] = [
    "Right", "Left", "Down", "C-r", "Tab", "Del", "Esc", "CR", "BS", "Up",
];

fn take_token(rest: &str) -> Option<(&'static str, I)> {
    TOKEN_NAMES.iter().find_map(|name| {
        rest.strip_prefix(name)
            .and_then(|_| token(name).map(|input| (*name, input)))
    })
}

/// Feeds a key script. `\Name` tokens map to non-printable keys; every other
/// character is fed literally, so Insert sessions and prompts receive text.
fn feed(editor: &mut Editor, script: &str) {
    let mut rest = script;
    while let Some(ch) = rest.chars().next() {
        if ch == '\\'
            && let Some((name, input)) = take_token(&rest[1..])
        {
            editor.handle_input(input);
            rest = &rest[1 + name.len()..];
            continue;
        }
        editor.handle_input(I::Character(ch));
        rest = &rest[ch.len_utf8()..];
    }
}

/// Splits `text` at the `|` cursor marker (a grapheme boundary).
fn marked(text: &str) -> (String, Cursor) {
    let index = text.find('|').expect("cursor marker");
    let plain = format!("{}{}", &text[..index], &text[index + 1..]);
    let prefix = &text[..index];
    let row = prefix.matches('\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |offset| offset + 1);
    (plain, Cursor::new(row, prefix[line_start..].len()))
}

fn editor_at(marked_text: &str) -> Editor {
    let (plain, cursor) = marked(marked_text);
    let mut editor = Editor::from_document(Document::from_text(&plain));
    editor.set_cursor(cursor);
    editor
}

#[track_caller]
fn assert_text(editor: &Editor, expected: &str) {
    assert_eq!(
        editor.document().serialize(),
        expected,
        "document bytes differ"
    );
}

#[track_caller]
fn assert_cursor(editor: &Editor, row: usize, grapheme: usize) {
    let cursor = editor.cursor();
    assert_eq!(cursor.row, row, "cursor row differs");
    assert_eq!(
        editor.document().grapheme_index(cursor.row, cursor.column),
        grapheme,
        "cursor grapheme differs"
    );
}

/// Asserts document bytes and cursor against a marked expectation.
#[track_caller]
fn assert_state(editor: &Editor, expected_marked: &str, label: &str) {
    let (text, _) = marked(expected_marked);
    assert_eq!(
        editor.document().serialize(),
        text,
        "document bytes differ for {label}"
    );
    let index = expected_marked.find('|').expect("cursor marker");
    let prefix = &expected_marked[..index];
    let row = prefix.matches('\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |offset| offset + 1);
    let grapheme = prefix[line_start..].graphemes(true).count();
    let cursor = editor.cursor();
    assert_eq!(cursor.row, row, "cursor row differs for {label}");
    assert_eq!(
        editor.document().grapheme_index(cursor.row, cursor.column),
        grapheme,
        "cursor grapheme differs for {label}"
    );
    assert_invariants(editor);
}

/// Runs an `(initial, keys, expected)` table through the shared input path and
/// reports the failing case in every assertion message.
fn check_table(cases: &[(&str, &str, &str)]) {
    for (initial, script, expected) in cases {
        let mut editor = editor_at(initial);
        feed(&mut editor, script);
        assert_state(&editor, expected, &format!("{initial:?} + {script:?}"));
    }
}

fn assert_invariants(editor: &Editor) {
    let document = editor.document();
    assert!(document.line_count() >= 1, "document lost its last line");
    let cursor = editor.cursor();
    assert!(
        cursor.row < document.line_count(),
        "cursor row out of range"
    );
    assert!(
        cursor.column <= document.line_len_bytes(cursor.row),
        "cursor column past end of line"
    );
    assert!(
        document
            .grapheme_boundaries(cursor.row)
            .contains(&cursor.column),
        "cursor not on a grapheme boundary"
    );
    if !editor.mode().is_visual() && editor.mode() != EditorMode::Insert {
        let len = document.line_len_bytes(cursor.row);
        if len > 0 {
            assert!(
                document
                    .line(cursor.row)
                    .expect("line")
                    .grapheme_indices(true)
                    .any(|(index, _)| index == cursor.column),
                "normal cursor is not on a grapheme start"
            );
        }
    }
    if let Some(selection) = editor.selection() {
        assert!(selection.start <= selection.end);
        assert!(selection.start.row < document.line_count());
        assert!(selection.end.row < document.line_count());
    }
}

// ---------------------------------------------------------------- fixtures

#[test]
fn open_existing_and_missing_path() {
    let dir = tempdir().unwrap();
    let existing = dir.path().join("hello world.txt");
    fs::write(&existing, "hello\r\nworld").unwrap();
    let editor = Editor::new(Some(existing.clone())).unwrap();
    assert_eq!(editor.document().serialize(), "hello\r\nworld");
    let missing = dir.path().join("not yet.txt");
    let editor = Editor::new(Some(missing.clone())).unwrap();
    assert!(!missing.exists());
    assert!(editor.document().serialize().is_empty());
}

#[test]
fn reject_invalid_utf8_and_nonregular_paths() {
    let dir = tempdir().unwrap();
    let bad = dir.path().join("bad");
    fs::write(&bad, [0xff, 0xfe]).unwrap();
    assert!(Editor::new(Some(bad)).is_err());
    assert!(Editor::new(Some(dir.path().to_path_buf())).is_err());
}

#[test]
fn byte_exact_roundtrip_fixtures() {
    let dir = tempdir().unwrap();
    for (index, bytes) in [
        "",
        "x",
        "x\n",
        "\n",
        "\r\n",
        "\r",
        "a\r\nb\nc\rd",
        "\u{feff}a\n\r\nb",
        "\u{feff}e\u{301}👩‍👩‍👧‍👦界\r\n\t\0\u{1b}終",
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(format!("fixture-{index}"));
        fs::write(&path, bytes.as_bytes()).unwrap();
        let mut editor = Editor::new(Some(path.clone())).unwrap();
        assert_eq!(editor.document().serialize().as_bytes(), bytes.as_bytes());
        assert!(!editor.is_dirty());
        command(&mut editor, "w");
        assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
        assert!(!editor.is_dirty());
    }
    let bom = Document::from_text("\u{feff}hi");
    assert!(bom.has_bom());
    assert_eq!(bom.line(0), Some("hi"));
}

#[test]
fn inserted_newlines_follow_document_policy() {
    let mut editor = Editor::from_document(Document::from_text("a\r\nb"));
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Enter);
    editor.handle_input(I::Escape);
    assert_eq!(editor.document().serialize(), "\r\na\r\nb");
    let mut fresh = Editor::from_document(Document::new());
    insert(&mut fresh, "x\ny");
    assert_eq!(fresh.document().serialize(), "x\ny");
    let mut pasted = Editor::from_document(Document::from_text("a\r\nb"));
    pasted.handle_input(I::Character('i'));
    pasted.handle_input(I::Paste("X\nY\nZ".into()));
    assert_eq!(pasted.document().serialize(), "X\r\nY\r\nZa\r\nb");
}

#[test]
fn modal_editing_scenario() {
    let mut editor = Editor::new(None).unwrap();
    assert_eq!(editor.mode(), EditorMode::Normal);
    insert(&mut editor, "héllo\n世界");
    assert_eq!(editor.document().serialize(), "héllo\n世界");
    assert!(editor.is_dirty());
    editor.handle_input(I::Character('k'));
    editor.handle_input(I::Character('h'));
    assert_eq!(editor.mode(), EditorMode::Normal);
}

#[test]
fn deterministic_input_sequences_keep_cursor_on_grapheme_boundaries() {
    let mut editor = Editor::from_document(Document::from_text("a\u{301}界\n👩‍👩‍👧‍👦\tend"));
    let inputs = [
        I::Character('i'),
        I::Escape,
        I::Character('h'),
        I::Character('j'),
        I::Character('k'),
        I::Character('l'),
        I::Backspace,
        I::Delete,
        I::Enter,
        I::Text("e\u{301}界".into()),
        I::Left,
        I::Right,
        I::Up,
        I::Down,
    ];
    let mut state = 0x5eed_u32;
    for _ in 0..512 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        editor.handle_input(inputs[state as usize % inputs.len()].clone());
        let cursor = editor.cursor();
        assert!(cursor.row < editor.document().line_count());
        assert!(cursor.column <= editor.document().line_len_bytes(cursor.row));
        assert!(
            editor
                .document()
                .grapheme_boundaries(cursor.row)
                .contains(&cursor.column)
        );
    }
}

#[test]
fn grapheme_editing_and_movement() {
    let mut editor = Editor::from_document(Document::from_text("a\u{301}👩‍👩‍👧‍👦界"));
    editor.handle_input(I::Character('l'));
    assert_eq!(editor.cursor().column, "a\u{301}".len());
    editor.handle_input(I::Character('l'));
    assert_eq!(editor.cursor().column, "a\u{301}👩‍👩‍👧‍👦".len());
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Backspace);
    assert!(editor.is_dirty());
    assert_eq!(editor.document().serialize(), "a\u{301}界");
}

#[test]
fn vertical_motion_preserves_visual_column() {
    let mut editor = Editor::from_document(Document::from_text("12345\nx\n12345"));
    for _ in 0..3 {
        editor.handle_input(I::Character('l'));
    }
    editor.handle_input(I::Character('j'));
    assert_eq!(editor.cursor().row, 1);
    editor.handle_input(I::Character('j'));
    assert_eq!(editor.cursor().row, 2);
    assert_eq!(editor.cursor().column, 3);
}

#[test]
fn line_join_and_cursor_boundaries() {
    let mut editor = Editor::from_document(Document::from_text("\ncd"));
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Delete);
    assert_eq!(editor.document().serialize(), "cd");
    assert!(editor.cursor().column <= editor.document().line_len_bytes(editor.cursor().row));

    let mut backspace = Editor::from_document(Document::from_text("ab\ncd"));
    backspace.handle_input(I::Character('i'));
    backspace.handle_input(I::Down);
    backspace.handle_input(I::Backspace);
    assert_eq!(backspace.document().serialize(), "abcd");
    assert!(
        backspace.cursor().column <= backspace.document().line_len_bytes(backspace.cursor().row)
    );
}

#[test]
fn paste_is_text_not_commands() {
    let mut editor = Editor::new(None).unwrap();
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Paste(":q\u{1b} text\r\nnext".to_owned()));
    assert_eq!(editor.document().serialize(), ":q\u{1b} text\nnext");
    assert_eq!(editor.mode(), EditorMode::Insert);
}

#[test]
fn command_language_is_bounded() {
    let mut editor = Editor::new(None).unwrap();
    command(&mut editor, "wq");
    assert!(!editor.should_quit());
    assert_eq!(editor.message(), Some("No filename; use :w <path>"));
    command(&mut editor, "");
    assert_eq!(editor.message(), Some("Empty command"));
    command(&mut editor, "w");
    assert_eq!(editor.message(), Some("No filename; use :w <path>"));
    for rejected in [
        "wq /tmp/x",
        "x /tmp/x",
        "w!",
        "q!!",
        "wq!",
        " e",
        "write",
        "qa",
    ] {
        command(&mut editor, rejected);
        assert!(
            editor
                .message()
                .is_some_and(|message| message.starts_with("Unknown command")),
            "expected {rejected:?} to be rejected, got {:?}",
            editor.message()
        );
    }
}

#[test]
fn unnamed_save_then_quit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("with spaces.txt");
    let mut editor = Editor::new(None).unwrap();
    insert(&mut editor, "saved");
    command(&mut editor, &format!("w {}", path.display()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "saved");
    assert!(!editor.is_dirty());
    command(&mut editor, "q");
    assert!(editor.should_quit());
}

#[test]
fn dirty_quit_is_refused() {
    let mut editor = Editor::new(None).unwrap();
    insert(&mut editor, "x");
    command(&mut editor, "q");
    assert!(!editor.should_quit());
    assert_eq!(
        editor.message(),
        Some("Unsaved changes; use :w [path] before :q, or :q! to discard")
    );
}

#[test]
fn failed_save_preserves_buffer_path_and_dirty() {
    let dir = tempdir().unwrap();
    let original = dir.path().join("original");
    fs::write(&original, "old").unwrap();
    let mut editor = Editor::new(Some(original.clone())).unwrap();
    insert(&mut editor, "x");
    let prior = editor.path().unwrap().to_path_buf();
    command(
        &mut editor,
        &format!("w {}/missing/file", dir.path().display()),
    );
    assert_eq!(editor.path(), Some(prior.as_path()));
    assert!(editor.is_dirty());
    assert_eq!(editor.document().serialize(), "xold");
    assert_eq!(fs::read_to_string(original).unwrap(), "old");
}

#[test]
fn edits_reset_preferred_visual_column() {
    for (edit, expected_row, expected_column) in
        [(I::Enter, 3, 0), (I::Backspace, 2, 2), (I::Delete, 2, 3)]
    {
        let mut editor = Editor::from_document(Document::from_text("abcd\nwxyz\n1234"));
        editor.handle_input(I::Character('i'));
        for _ in 0..3 {
            editor.handle_input(I::Right);
        }
        editor.handle_input(I::Down);
        editor.handle_input(edit);
        editor.handle_input(I::Down);
        assert_eq!(editor.cursor().row, expected_row);
        assert_eq!(editor.cursor().column, expected_column);
    }

    // A line join changes both row and visual column after the cached motion.
    let mut editor = Editor::from_document(Document::from_text("abcd\nwxyz\n1234"));
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Down);
    editor.handle_input(I::Backspace);
    editor.handle_input(I::Down);
    assert_eq!(editor.cursor().column, 4);
}

#[test]
fn save_replaces_complete_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, "long old contents").unwrap();
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    editor.handle_input(I::Character('i'));
    for _ in 0.."long old contents".len() {
        editor.handle_input(I::Delete);
    }
    editor.handle_input(I::Text("short".into()));
    editor.handle_input(I::Escape);
    command(&mut editor, "w");
    assert!(!editor.is_dirty());
    assert_eq!(fs::read(&path).unwrap(), b"short");
}

#[test]
fn failed_save_original_intact() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("absent-parent/file");
    assert!(postvim::storage::save_document(&path, b"new").is_err());
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn save_preserves_permissions_and_symlink() {
    use std::os::unix::fs::symlink;
    let dir = tempdir().unwrap();
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    fs::write(&target, "before").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&target, &link).unwrap();
    postvim::storage::save_document(&link, b"after").unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).unwrap(), b"after");
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let readonly = dir.path().join("readonly");
    fs::write(&readonly, "stay").unwrap();
    let mut permissions = fs::metadata(&readonly).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&readonly, permissions).unwrap();
    assert!(postvim::storage::save_document(&readonly, b"bad").is_err());
    assert_eq!(fs::read(readonly).unwrap(), b"stay");
}

#[test]
fn display_helpers_sanitize_controls_and_share_width() {
    assert_eq!(Document::display_grapheme("\u{1b}", 0), "^[");
    assert_eq!(Document::display_grapheme("\t", 1), "   ");
    assert_eq!(Document::display_grapheme_width("界", 0), 2);
}

#[test]
fn cli_startup_contract() {
    let dir = tempdir().unwrap();
    let path = PathBuf::from(dir.path()).join("new file");
    let editor = Editor::new(Some(path.clone())).unwrap();
    assert_eq!(editor.path(), Some(path.as_path()));
    assert!(!path.exists());
}

// ---------------------------------------------------------- AC.2 motions

#[test]
fn everyday_motion_table() {
    let cases = [
        ("|abc", "l", "a|bc"),
        ("ab|c", "h", "a|bc"),
        ("ab|c", "0", "|abc"),
        ("   ab|c", "^", "   |abc"),
        ("ab|c\nde", "$", "ab|c\nde"),
        ("|abc\ndef", "j", "abc\n|def"),
        ("abc\n|def", "k", "|abc\ndef"),
        ("|abc\ndef\nghi", "G", "abc\ndef\n|ghi"),
        ("abc\ndef\n|ghi", "gg", "|abc\ndef\nghi"),
        ("abc\ndef\n|ghi", "2gg", "abc\n|def\nghi"),
        ("abc\ndef\n|ghi", "1G", "|abc\ndef\nghi"),
        ("|abc\ndef", "2l", "ab|c\ndef"),
        ("|abcdef", "99l", "abcde|f"),
        ("|foo bar", "w", "foo |bar"),
        ("foo ba|r\nbaz", "w", "foo bar\n|baz"),
        ("foo|\n  bar", "w", "foo\n  |bar"),
        ("|foo\n\nbar", "w", "foo\n|\nbar"),
        ("foo bar| baz", "e", "foo bar ba|z"),
        ("|foo bar", "e", "fo|o bar"),
        ("|foo.bar baz", "W", "foo.bar |baz"),
        ("foo |bar", "b", "|foo bar"),
        ("foo\n  |bar", "b", "|foo\n  bar"),
        ("  fo|o", "b", "  |foo"),
        ("|a.b", "w", "a|.b"),
        ("|foo bar baz", "2w", "foo bar |baz"),
        ("foo bar |baz", "2b", "|foo bar baz"),
    ];
    check_table(&cases);
}

#[test]
fn count_and_pending_key_state() {
    let mut editor = Editor::from_document(Document::from_text("a b c d e f g h"));
    feed(&mut editor, "2");
    assert_eq!(editor.pending_text(), "2");
    feed(&mut editor, "d");
    assert_eq!(editor.pending_text(), "2d");
    feed(&mut editor, "3");
    assert_eq!(editor.pending_text(), "2d3");
    feed(&mut editor, "w");
    assert_eq!(editor.pending_text(), "");
    assert_text(&editor, "g h");
    assert_cursor(&editor, 0, 0);

    // Unsupported continuations consume the prefix without executing the key.
    let mut editor = editor_at("a|bc");
    feed(&mut editor, "2q");
    assert_text(&editor, "abc");
    assert_eq!(editor.pending_text(), "");
    feed(&mut editor, "3g");
    assert_eq!(editor.pending_text(), "3g");
    feed(&mut editor, "x");
    assert_text(&editor, "abc");
    feed(&mut editor, "df");
    assert_eq!(editor.pending_text(), "df");
    feed(&mut editor, "\\Esc");
    assert_text(&editor, "abc");
    assert_eq!(editor.pending_text(), "");

    // Counts multiply, and a standalone zero is a motion.
    let mut editor = editor_at("|a b c d e f g h");
    feed(&mut editor, "2d3w");
    assert_text(&editor, "g h");

    let mut editor = editor_at("abc|def");
    feed(&mut editor, "0");
    assert_cursor(&editor, 0, 0);
    feed(&mut editor, "20l");
    assert_cursor(&editor, 0, 5);

    // Count accumulation saturates instead of overflowing.
    let mut editor = editor_at("|abcdef");
    feed(&mut editor, "99999999999999999999l");
    assert_cursor(&editor, 0, 5);
    assert_eq!(editor.pending_text(), "");

    // Esc clears an incomplete prefix.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "2d\\Esc");
    assert_eq!(editor.pending_text(), "");
    assert_text(&editor, "abc");

    // `0` after an operator is the line-start motion, even with a leading count.
    let mut editor = editor_at("abcd|efgh");
    feed(&mut editor, "2d0");
    assert_text(&editor, "efgh");
    assert_cursor(&editor, 0, 0);

    // A motion count still accepts zeros once it has started: 2d30w = 60 words.
    let mut editor = editor_at("|a b c d e f g");
    feed(&mut editor, "2d30");
    assert_eq!(editor.pending_text(), "2d30");
    feed(&mut editor, "w");
    assert_text(&editor, "");

    // `2d3w` and `d6w` are the same six words.
    let mut editor = editor_at("|a b c d e f g h");
    feed(&mut editor, "d6w");
    assert_text(&editor, "g h");
    let mut editor = editor_at("|a b c d e f g h");
    feed(&mut editor, "2d3w");
    assert_text(&editor, "g h");
}

#[test]
fn find_and_till_repeat() {
    let cases = [
        ("|a,b,c,d", "f,", "a|,b,c,d"),
        ("|a,b,c,d", "2f,", "a,b|,c,d"),
        ("|a,b,c,d", "t,", "|a,b,c,d"),
        ("|a,b,c,d", "t,;", "a,|b,c,d"),
        ("|a,b,c,d", "f,;;", "a,b,c|,d"),
        ("|a,b,c,d", "f,,", "a|,b,c,d"),
        ("|a,b,c,d", "2t,", "a,|b,c,d"),
        ("abc|def", "Fc", "ab|cdef"),
        ("abcab|c", "Tb", "abcab|c"),
        ("|abcdef", "fd", "abc|def"),
        ("|abcdef", "td", "ab|cdef"),
        ("abcde|f", "Fb", "a|bcdef"),
    ];
    check_table(&cases);

    // Failed finds report visibly and leave state untouched.
    let mut editor = editor_at("|abcabc");
    feed(&mut editor, "fz");
    assert_eq!(editor.message(), Some("Character not found"));
    assert_cursor(&editor, 0, 0);
    assert_text(&editor, "abcabc");

    // A target scalar matches a grapheme that contains it; movement stays whole.
    let mut editor = editor_at("|ae\u{301}b");
    feed(&mut editor, "f\u{301}");
    assert_cursor(&editor, 0, 1);

    // `;` without a previous find reports and does not move.
    let mut editor = editor_at("|abc");
    feed(&mut editor, ";");
    assert_eq!(editor.message(), Some("No previous f/F/t/T"));
    assert_cursor(&editor, 0, 0);

    // An operator find records the target for `;`, and a failed one keeps it.
    let mut editor = editor_at("|axbxc");
    feed(&mut editor, "yfx");
    assert_text(&editor, "axbxc");
    assert_eq!(editor.register().text(), Some("ax"));
    feed(&mut editor, ";");
    assert_cursor(&editor, 0, 1);
    feed(&mut editor, ";");
    assert_cursor(&editor, 0, 3);
    feed(&mut editor, "dFz");
    assert_eq!(editor.message(), Some("Character not found"));
    assert_text(&editor, "axbxc");
    feed(&mut editor, ",");
    assert_cursor(&editor, 0, 1);

    // A repeated find keeps the recorded direction; `,` reverses without changing it.
    let mut editor = editor_at("|a,b,c");
    feed(&mut editor, "f,;");
    assert_cursor(&editor, 0, 3);
    feed(&mut editor, ";");
    assert_cursor(&editor, 0, 3);
    feed(&mut editor, ",");
    assert_cursor(&editor, 0, 1);
    feed(&mut editor, ";");
    assert_cursor(&editor, 0, 3);

    // An operator used with `,` must not flip the remembered direction.
    let mut editor = editor_at("|a x b x c x d");
    feed(&mut editor, "fxl");
    assert_cursor(&editor, 0, 3);
    feed(&mut editor, "d,");
    assert_text(&editor, "a  b x c x d");
    assert_cursor(&editor, 0, 2);
    feed(&mut editor, ";");
    assert_cursor(&editor, 0, 5);

    // The same holds for a reversed yank and a reversed change.
    let mut editor = editor_at("|a x b x c x d");
    feed(&mut editor, "2fx");
    assert_cursor(&editor, 0, 6);
    feed(&mut editor, "y,");
    assert_eq!(editor.register().text(), Some("x b "));
    assert_cursor(&editor, 0, 2);
    feed(&mut editor, ";");
    assert_cursor(&editor, 0, 6);

    let mut editor = editor_at("|a x b x c x d");
    feed(&mut editor, "2fx");
    feed(&mut editor, "c,Z\\Esc");
    assert_text(&editor, "a Zx c x d");
    feed(&mut editor, ";");
    assert_cursor(&editor, 0, 3);

    // A till repeat also keeps the forward direction.
    let mut editor = editor_at("|a x b x c x d");
    feed(&mut editor, "txd;");
    assert_text(&editor, "ax c x d");
    assert_cursor(&editor, 0, 1);
}

#[test]
fn word_motions_at_end_of_text() {
    // A forward word motion with no further stop ends at the line end, so the
    // operator consumes the trailing word instead of failing on it.
    let cases: [(&str, &str, &str, &str); 8] = [
        ("|foo", "dw", "|", "foo"),
        ("fo|o", "dw", "f|o", "o"),
        ("|foo ", "dw", "|", "foo "),
        ("|one two", "3dw", "|", "one two"),
        ("|a.b", "dW", "|", "a.b"),
        ("|a b c", "d99w", "|", "a b c"),
        ("foo |", "dw", "fo|o", " "),
        ("foo\n|bar", "yw", "foo\n|bar", "bar"),
    ];
    for (initial, script, expected, register) in cases {
        let mut editor = editor_at(initial);
        feed(&mut editor, script);
        assert_state(&editor, expected, &format!("{initial:?} + {script:?}"));
        assert_eq!(
            editor.register().text(),
            Some(register),
            "{initial:?} + {script:?}: register"
        );
    }

    // cw on trailing whitespace at the end of the text removes the blank run.
    let mut editor = editor_at("foo |");
    feed(&mut editor, "cwX\\Esc");
    assert_text(&editor, "fooX");

    // A bare w at the end still clamps to the last grapheme and edits nothing.
    let mut editor = editor_at("|a b");
    feed(&mut editor, "9w");
    assert_text(&editor, "a b");
    assert_cursor(&editor, 0, 2);
}

#[test]
fn incomplete_prefix_is_consumed() {
    // An arrow with a pending non-count prefix cancels it without executing: the
    // cursor stays put and the next key never completes the abandoned operator.
    for prefix in ["g", "d", "d2", "dg", "df", "r", "f", "2d"] {
        for arrow in ["\\Left", "\\Right", "\\Up", "\\Down"] {
            let mut editor = editor_at("|abc\ndef");
            feed(&mut editor, prefix);
            feed(&mut editor, arrow);
            let label = format!("{prefix:?} + {arrow:?}");
            assert_eq!(editor.pending_text(), "", "{label}: pending");
            assert_text(&editor, "abc\ndef");
            assert_cursor(&editor, 0, 0);
            feed(&mut editor, "w");
            assert_text(&editor, "abc\ndef");
        }
    }

    // A pending count still applies to an arrow (counted arrows keep working).
    let mut editor = editor_at("|abc\ndef\nghi");
    feed(&mut editor, "2\\Right");
    assert_eq!(editor.pending_text(), "");
    assert_cursor(&editor, 0, 2);
    feed(&mut editor, "2\\Down");
    assert_cursor(&editor, 2, 2);

    // Other unsupported non-characters consume a pending prefix too.
    for input in [
        I::Enter,
        I::Tab,
        I::Backspace,
        I::Delete,
        I::Paste("xy".to_owned()),
    ] {
        let mut editor = editor_at("|abc");
        feed(&mut editor, "d");
        editor.handle_input(input);
        assert_eq!(editor.pending_text(), "");
        assert_text(&editor, "abc");
        feed(&mut editor, "w");
        assert_text(&editor, "abc");
    }

    // Ctrl-r with a pending operator cancels the operator without redoing.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "x");
    feed(&mut editor, "u");
    assert_text(&editor, "abc");
    feed(&mut editor, "d\\C-r");
    assert_text(&editor, "abc");
    assert_eq!(editor.pending_text(), "");
    feed(&mut editor, "\\C-r");
    assert_text(&editor, "bc");

    // Visual: a pending count applies to the arrow, any other prefix cancels it.
    let mut editor = editor_at("|abc\ndef");
    feed(&mut editor, "v2\\Right");
    assert_eq!(editor.pending_text(), "");
    assert_eq!(
        editor.selection().map(|selection| selection.end),
        Some(Cursor::new(0, 2))
    );
    let mut editor = editor_at("|abc\ndef");
    feed(&mut editor, "vf\\Right");
    assert_eq!(editor.pending_text(), "");
    assert_eq!(editor.mode(), EditorMode::Visual);
    assert_eq!(
        editor.selection(),
        Some(postvim::Selection {
            start: Cursor::new(0, 0),
            end: Cursor::new(0, 0),
            kind: SelectionKind::Characterwise,
        })
    );
    assert_text(&editor, "abc\ndef");
}

#[test]
fn unicode_word_classes() {
    // Combining sequences and ZWJ emoji stay indivisible.
    let mut editor = editor_at("|a\u{301}b c\u{301}d");
    feed(&mut editor, "w");
    assert_cursor(&editor, 0, 3);
    feed(&mut editor, "b");
    assert_cursor(&editor, 0, 0);

    // CJK alphanumerics group as keywords, emoji as non-keyword non-blank.
    let mut editor = editor_at("|世界👍👍x");
    feed(&mut editor, "w");
    assert_cursor(&editor, 0, 2);
    feed(&mut editor, "w");
    assert_cursor(&editor, 0, 4);
    feed(&mut editor, "e");
    assert_cursor(&editor, 0, 4);

    // Underscores join keyword runs; punctuation forms separate runs.
    let mut editor = editor_at("|a_b.c");
    feed(&mut editor, "w");
    assert_cursor(&editor, 0, 3);
    feed(&mut editor, "w");
    assert_cursor(&editor, 0, 4);
    let mut editor = editor_at("|a_b.c x");
    feed(&mut editor, "W");
    assert_cursor(&editor, 0, 6);

    // Blank lines are word stops for w and are skipped by e.
    let mut editor = editor_at("|ab\n\ncd");
    feed(&mut editor, "w");
    assert_cursor(&editor, 1, 0);
    feed(&mut editor, "w");
    assert_cursor(&editor, 2, 0);
    let mut editor = editor_at("|ab\n\ncd");
    feed(&mut editor, "e");
    assert_cursor(&editor, 0, 1);
    feed(&mut editor, "e");
    assert_cursor(&editor, 2, 1);

    // Grapheme containing a keyword scalar is keyword-classed as a whole.
    let mut editor = editor_at("|e\u{301}👍");
    feed(&mut editor, "w");
    assert_cursor(&editor, 0, 1);
}

#[test]
fn input_sequences_keep_cursor_invariants() {
    let mut editor = Editor::from_document(Document::from_text(
        "one two\n\nthree\te\u{301}👩‍👩‍👧‍👦\n  世界\nlast",
    ));
    let scripts = [
        "w", "b", "e", "W", "B", "E", "j", "k", "h", "l", "0", "^", "$", "G", "gg", "2j", "3w",
        "x", "X", "dd", "cc", "yy", "p", "P", "u", "\\C-r", "v", "V", "\\Esc", "f", "o", "w", "i",
        "\\Esc", "a", "\\CR", "\\BS", "\\Del", "\\Left", "\\Right", "\\Up", "\\Down", ".", "r",
        "s", "S", "D", "C", "\\C-r", "/", "n", "N", "V", "d", "y", "c",
    ];
    let mut state = 0x1234_5678_u32;
    for _ in 0..800 {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let script = scripts[state as usize % scripts.len()];
        feed(&mut editor, script);
        assert_invariants(&editor);
    }
}

// ---------------------------------------------------------- AC.3 editing

#[test]
fn insertion_entry_and_counted_sessions() {
    let cases = [
        ("ab|cdef", "iXY\\Esc", "abX|Ycdef"),
        ("ab|cdef", "aXY\\Esc", "abcX|Ydef"),
        ("   ab|c", "IX\\Esc", "   |Xabc"),
        ("ab|c", "AX\\Esc", "abc|X"),
        ("ab|c", "oX\\Esc", "abc\n|X"),
        ("ab|c", "OX\\Esc", "|X\nabc"),
        ("|x", "3iZ\\Esc", "ZZ|Zx"),
        ("|x", "3oZ\\Esc", "x\nZ\nZ\n|Z"),
        ("|x", "2OZ\\Esc", "|Z\nZ\nx"),
        ("|abcdef", "i\\Right\\RightX\\Esc", "ab|Xcdef"),
        ("ab|c", "2aX\\Esc", "abcX|X"),
        ("|abc", "i\\CR\\Esc", "\n|abc"),
    ];
    check_table(&cases);

    // Returning from Insert moves left one grapheme without crossing the line.
    let mut editor = editor_at("|a\u{301}b");
    feed(&mut editor, "Ae\u{301}\\Esc");
    assert_text(&editor, "a\u{301}be\u{301}");
    assert_cursor(&editor, 0, 2);

    // Arrows inside an Insert session move the insertion boundary.
    let mut editor = editor_at("ab|cdef");
    feed(&mut editor, "i\\Left\\LeftZ\\Esc");
    assert_text(&editor, "Zabcdef");
    assert_cursor(&editor, 0, 0);

    // Insert leaves no undo entry for a no-op session.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "i\\Esc");
    feed(&mut editor, "u");
    assert_eq!(editor.message(), Some("Already at oldest change"));
}

#[test]
fn operator_motion_range_table() {
    let cases: [(&str, &str, &str, &str); 23] = [
        ("ab|c", "dh", "a|c", "char:b"),
        ("ab|c", "dl", "a|b", "char:c"),
        ("|foo bar", "dw", "|bar", "char:foo "),
        ("foo |bar\nbaz", "dw", "foo| \nbaz", "char:bar"),
        ("foo b|ar", "de", "foo |b", "char:ar"),
        ("foo bar| baz", "de", "foo ba|r", "char: baz"),
        ("foo |bar", "db", "|bar", "char:foo "),
        ("foo |bar", "d0", "|bar", "char:foo "),
        ("foo |bar", "d$", "foo| ", "char:bar"),
        ("foo |bar", "D", "foo| ", "char:bar"),
        ("|a.b c", "dW", "|c", "char:a.b "),
        ("foo.b|ar baz", "dB", "|ar baz", "char:foo.b"),
        ("a\nb|c\nd", "dj", "|a", "line"),
        ("a\nb|c\nd", "dk", "|d", "line"),
        ("a\nb\n|c", "dd", "a\n|b", "line"),
        ("|a\nb\nc", "dj", "|c", "line"),
        ("a\n|b", "2dd", "|a", "line"),
        ("a\nb\n|c", "dgg", "|", "line"),
        ("|a\nb\nc", "dG", "|", "line"),
        ("|abcdef", "dfd", "|ef", "char:abcd"),
        ("|abcdef", "dtd", "|def", "char:abc"),
        ("abcd|ef", "dFa", "|ef", "char:abcd"),
        ("abcd|ef", "dTa", "a|ef", "char:bcd"),
    ];
    for (initial, script, expected, register) in cases {
        let mut editor = editor_at(initial);
        feed(&mut editor, script);
        assert_state(&editor, expected, &format!("{initial:?} + {script:?}"));
        match register {
            "line" => assert!(
                editor.register().is_linewise(),
                "{script}: expected a linewise register, got {:?}",
                editor.register()
            ),
            other => assert_eq!(
                editor.register().text(),
                Some(other.strip_prefix("char:").expect("register text")),
                "{script}: characterwise register"
            ),
        }
        assert_invariants(&editor);
    }

    // No movement means no edit: `dj` on the last line does nothing.
    let mut editor = editor_at("a\n|b");
    feed(&mut editor, "dj");
    assert_text(&editor, "a\nb");
    assert!(editor.register().is_empty());
    let mut editor = editor_at("|a");
    feed(&mut editor, "dk");
    assert_text(&editor, "a");

    // A failed find cancels the operator without touching text or register.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "dfz");
    assert_text(&editor, "abc");
    assert_eq!(editor.message(), Some("Character not found"));
    assert!(editor.register().is_empty());

    // Exclusive l may end at the insertion boundary after the final grapheme.
    let mut editor = editor_at("fo|o");
    feed(&mut editor, "dl");
    assert_text(&editor, "fo");
    // Inclusive motions yield a one-grapheme range at the last character.
    let mut editor = editor_at("fo|o");
    feed(&mut editor, "de");
    assert_text(&editor, "fo");
    // An adjacent till target is an inclusive one-grapheme range.
    let mut editor = editor_at("|a,b");
    feed(&mut editor, "dt,");
    assert_text(&editor, ",b");
}

#[test]
fn word_change_and_line_end_exceptions() {
    let cases = [
        ("|foo bar", "cwX\\Esc", "|X bar"),
        ("foo |bar", "cwX\\Esc", "foo |X"),
        ("foo| bar", "cwX\\Esc", "foo|Xbar"),
        ("|foo bar", "cWX\\Esc", "|X bar"),
        ("foo |bar baz", "ceX\\Esc", "foo |X baz"),
        ("|foo", "cwX\\Esc", "|X"),
        ("foo |bar\nbaz", "cwX\\Esc", "foo |X\nbaz"),
        ("foo |bar", "CX\\Esc", "foo |X"),
        ("foo |bar\nbaz", "c$X\\Esc", "foo |X\nbaz"),
        ("ab|c", "sX\\Esc", "ab|X"),
        ("ab|cdef", "3sX\\Esc", "ab|Xf"),
        ("ab|cd", "SX\\Esc", "|X"),
        ("   |abc", "ccX\\Esc", "|X"),
    ];
    check_table(&cases);

    // cw on whitespace uses the ordinary w range and keeps following blanks.
    let mut editor = editor_at("foo|  bar");
    feed(&mut editor, "cwX\\Esc");
    assert_text(&editor, "fooXbar");
    assert_cursor(&editor, 0, 3);

    // An empty line change is a valid empty range that enters Insert.
    let mut editor = editor_at("|");
    feed(&mut editor, "ccX\\Esc");
    assert_text(&editor, "X");
    assert_eq!(editor.mode(), EditorMode::Normal);

    // `s` on an empty line also enters Insert without deleting anything.
    let mut editor = editor_at("|");
    feed(&mut editor, "sX\\Esc");
    assert_text(&editor, "X");

    // A failed find must not enter Insert.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "cfz");
    assert_eq!(editor.mode(), EditorMode::Normal);
    assert_text(&editor, "abc");

    // `cw` stops at the end of the word under the cursor: at a word end it never
    // runs on into the next word or the next line.
    let mut editor = editor_at("|a\nb\nc");
    feed(&mut editor, "cwX\\Esc");
    assert_text(&editor, "X\nb\nc");
    let mut editor = editor_at("|a\nb\nc");
    feed(&mut editor, "c2wX\\Esc");
    assert_text(&editor, "X\nc");

    // Counted `cw` covers whole words and preserves trailing blanks.
    let mut editor = editor_at("|aa bb  \ncc");
    feed(&mut editor, "c2wX\\Esc");
    assert_text(&editor, "X  \ncc");
    let mut editor = editor_at("|aa.bb cc  \ndd");
    feed(&mut editor, "c2WX\\Esc");
    assert_text(&editor, "X  \ndd");
}

#[test]
fn linewise_register_boundary_cases() {
    // Yanking the final line of a file without a terminator drops it cleanly.
    let mut editor = editor_at("a\n|b");
    feed(&mut editor, "yy");
    assert_eq!(
        editor.register(),
        &Register::Linewise(vec![postvim::CapturedLine::plain("b")])
    );
    // A non-final line captures its original terminator.
    let mut editor = editor_at("|a\nb");
    feed(&mut editor, "yy");
    assert_eq!(
        editor.register(),
        &Register::Linewise(vec![postvim::CapturedLine {
            text: "a".to_owned(),
            ending: Some(postvim::LineEnding::Lf),
        }])
    );

    // Linewise delete of the final line drops the separating terminator.
    let mut editor = editor_at("a\n|b");
    feed(&mut editor, "dd");
    assert_text(&editor, "a");
    assert_cursor(&editor, 0, 0);

    // Deleting the sole line leaves one empty editable line.
    let mut editor = editor_at("|a");
    feed(&mut editor, "dd");
    assert_text(&editor, "");
    assert_eq!(editor.document().line_count(), 1);

    // Deleting a real terminal empty line removes its preceding separator.
    let mut editor = editor_at("a\n|");
    feed(&mut editor, "dd");
    assert_text(&editor, "a");

    // Linewise put below the last line synthesizes a boundary.
    let mut editor = editor_at("|a\nb");
    feed(&mut editor, "yyjp");
    assert_text(&editor, "a\nb\na\n");
    assert_cursor(&editor, 2, 0);

    // Linewise put above the first line keeps the captured ending.
    let mut editor = editor_at("a\n|b");
    feed(&mut editor, "yyggP");
    assert_text(&editor, "b\na\nb");

    // Mixed endings: captured internal endings survive, new boundaries use the
    // document's insertion ending.
    let mut editor = editor_at("|a\r\nb\nc");
    feed(&mut editor, "yy2jp");
    assert_text(&editor, "a\r\nb\nc\r\na\r\n");

    // Linewise change keeps the separator needed to connect a following line.
    let mut editor = editor_at("a\n|b\nc");
    feed(&mut editor, "ccX\\Esc");
    assert_text(&editor, "a\nX\nc");
    let mut editor = editor_at("a\n|b");
    feed(&mut editor, "ccX\\Esc");
    assert_text(&editor, "a\nX");

    // Characterwise delete spanning a line break keeps the later terminator.
    let mut editor = editor_at("ab|c\r\ndef");
    feed(&mut editor, "d$");
    assert_text(&editor, "ab\r\ndef");

    // Linewise yank moves to the first selected row and clamps the column.
    let mut editor = editor_at("abc\nde|f\nghi");
    feed(&mut editor, "yj");
    assert_cursor(&editor, 1, 2);
}

#[test]
fn replace_and_put_table() {
    // r replaces count graphemes and fails atomically when short.
    let mut editor = editor_at("ab|cd");
    feed(&mut editor, "rZ");
    assert_text(&editor, "abZd");
    assert_cursor(&editor, 0, 2);

    let mut editor = editor_at("ab|cd");
    feed(&mut editor, "2rZ");
    assert_text(&editor, "abZZ");

    let mut editor = editor_at("a|b");
    feed(&mut editor, "3rZ");
    assert_eq!(editor.message(), Some("Not enough characters"));
    assert_text(&editor, "ab");
    assert_cursor(&editor, 0, 1);
    feed(&mut editor, "u");
    assert_eq!(editor.message(), Some("Already at oldest change"));

    let mut editor = editor_at("ab|cd");
    feed(&mut editor, "2r\\Tab");
    assert_text(&editor, "ab\t\t");

    let mut editor = editor_at("a\r\nb|cd");
    feed(&mut editor, "r\\CR");
    assert_text(&editor, "a\r\nb\r\nd");
    assert_cursor(&editor, 2, 0);

    // x and X never consume a line break.
    let mut editor = editor_at("a|b");
    feed(&mut editor, "3x");
    assert_text(&editor, "a");
    assert_cursor(&editor, 0, 0);
    let mut editor = editor_at("abcde|f");
    feed(&mut editor, "3X");
    assert_text(&editor, "abf");
    assert_cursor(&editor, 0, 2);

    // Put with an empty register reports and does nothing.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "p");
    assert_eq!(editor.message(), Some("Nothing in register"));
    assert_text(&editor, "abc");

    // Characterwise single-line put lands on the last inserted grapheme.
    let mut editor = editor_at("a|bc");
    feed(&mut editor, "ylp");
    assert_text(&editor, "abbc");
    assert_cursor(&editor, 0, 2);

    // Counts repeat the payload.
    let mut editor = editor_at("a|bc");
    feed(&mut editor, "yl3p");
    assert_text(&editor, "abbbbc");

    // P inserts before the cursor.
    let mut editor = editor_at("ab|c");
    feed(&mut editor, "ylP");
    assert_text(&editor, "abcc");

    // Multiline characterwise put lands on the first inserted grapheme.
    let mut editor = editor_at("|abc\ndef");
    feed(&mut editor, "vjyp");
    assert_text(&editor, "aabc\ndbc\ndef");
    assert_cursor(&editor, 0, 1);

    // Linewise put lands on the first non-blank of the first inserted line.
    let mut editor = editor_at("|abc\ndef");
    feed(&mut editor, "yyjp");
    assert_text(&editor, "abc\ndef\nabc\n");
    assert_cursor(&editor, 2, 0);

    // Registers keep exact source terminators rather than assuming LF.
    let mut editor = editor_at("|a\r\nb");
    feed(&mut editor, "vjy");
    assert_eq!(
        editor.register(),
        &Register::Characterwise("a\r\nb".to_owned())
    );
    let mut editor = editor_at("|a\r\nb");
    feed(&mut editor, "yy");
    assert_eq!(
        editor.register(),
        &Register::Linewise(vec![postvim::CapturedLine {
            text: "a".to_owned(),
            ending: Some(postvim::LineEnding::Crlf),
        }])
    );
}

#[test]
fn break_normalization_uses_policy_once() {
    // CRLF, lone CR and LF input each normalize to exactly one CRLF; a CRLF
    // policy is never applied twice to its own output.
    let document = Document::from_text("a\r\nb");
    assert_eq!(document.normalize_breaks("x\r\ny"), "x\r\ny");
    assert_eq!(document.normalize_breaks("x\ry"), "x\r\ny");
    assert_eq!(document.normalize_breaks("x\ny"), "x\r\ny");
    assert_eq!(document.normalize_breaks("x\r\ry"), "x\r\n\r\ny");

    let mut editor = Editor::from_document(Document::from_text("a\r\nb"));
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Paste("X\r\nY\rZ\nW".to_owned()));
    assert_text(&editor, "X\r\nY\r\nZ\r\nWa\r\nb");
    assert_eq!(editor.document().insertion_line_ending(), "\r\n");

    // Enter still inserts a single chosen line break.
    let mut editor = Editor::from_document(Document::from_text("a\r\nb"));
    feed(&mut editor, "i\\CR\\Esc");
    assert_text(&editor, "\r\na\r\nb");

    // A CR policy folds CRLF and LF into CR.
    let mut editor = Editor::from_document(Document::from_text("a\rb"));
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Paste("X\r\nY\nZ\rW".to_owned()));
    assert_text(&editor, "X\rY\rZ\rWa\rb");
    assert_eq!(editor.document().insertion_line_ending(), "\r");

    // An LF policy never grows CR bytes.
    let mut editor = Editor::from_document(Document::from_text("a\nb"));
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Paste("X\r\nY\rZ\nW".to_owned()));
    assert_text(&editor, "X\nY\nZ\nWa\nb");
}

#[test]
fn exclusive_motion_linewise_conversion() {
    // A cross-line exclusive motion ending in column 0 becomes linewise when the
    // start is at or before the first non-blank and more than one line is crossed.
    let cases: [(&str, &str, &str, bool); 11] = [
        ("|a\nb\nc", "2dw", "|c", true),
        ("|a\nb\nc", "dw", "|\nb\nc", false),
        ("|a\nb\nc\nd", "3dw", "|d", true),
        ("  |a\nb\nc", "2dw", "|c", true),
        ("|a\n\nb\nc", "2dw", "|b\nc", true),
        ("|a.b\nc\nd", "2dW", "|d", true),
        ("|a b\nc\nd", "2dw", "|\nc\nd", false),
        ("foo |bar\nbaz\nqux", "2dw", "foo| \nqux", false),
        ("a\n\n|b", "db", "a\n|b", true),
        ("a\n\n  |b", "db", "a\n|b", false),
        ("aa\n|bb", "yb", "|aa\nbb", true),
    ];
    for (initial, script, expected, linewise) in cases {
        let mut editor = editor_at(initial);
        feed(&mut editor, script);
        assert_state(&editor, expected, &format!("{initial:?} + {script:?}"));
        assert_eq!(
            editor.register().is_linewise(),
            linewise,
            "{initial:?} + {script:?}: register type"
        );
    }

    // The conversion applies to change as well, and never eats following blanks.
    let mut editor = editor_at("|a\nb\nc");
    feed(&mut editor, "c2wX\\Esc");
    assert_text(&editor, "X\nc");
}

#[test]
fn blank_line_word_stops() {
    // `b` lands on a blank line and further counts step through blank lines.
    let mut editor = editor_at("a\n\n|b");
    feed(&mut editor, "b");
    assert_cursor(&editor, 1, 0);
    feed(&mut editor, "b");
    assert_cursor(&editor, 0, 0);

    let mut editor = editor_at("a\n\n\n|b");
    feed(&mut editor, "b");
    assert_cursor(&editor, 2, 0);
    feed(&mut editor, "b");
    assert_cursor(&editor, 1, 0);
    feed(&mut editor, "b");
    assert_cursor(&editor, 0, 0);

    let mut editor = editor_at("a\n\n|b");
    feed(&mut editor, "2b");
    assert_cursor(&editor, 0, 0);

    // From a blank line `b` continues to the previous word.
    let mut editor = editor_at("a\n|\nb");
    feed(&mut editor, "b");
    assert_cursor(&editor, 0, 0);

    // `db` from a word after a blank line joins onto the blank line.
    let mut editor = editor_at("a\n\n|b");
    feed(&mut editor, "db");
    assert_text(&editor, "a\nb");
    assert!(editor.register().is_linewise());

    // `w` also stops on the blank line, and `e` skips it.
    let mut editor = editor_at("|a\n\nb");
    feed(&mut editor, "w");
    assert_cursor(&editor, 1, 0);
    let mut editor = editor_at("|a\n\nb");
    feed(&mut editor, "e");
    assert_cursor(&editor, 2, 0);
}

#[test]
fn editing_preserves_bom_and_mixed_endings() {
    let original = "\u{feff}a\r\nb\nc\rd";
    let mut editor = Editor::from_document(Document::from_text(original));
    assert!(editor.document().has_bom());

    feed(&mut editor, "x");
    assert_text(&editor, "\u{feff}\r\nb\nc\rd");
    feed(&mut editor, "u");
    assert_text(&editor, original);

    // New breaks use the insertion ending and never rewrite existing ones.
    feed(&mut editor, "A!");
    feed(&mut editor, "\\CRnew\\Esc");
    assert_text(&editor, "\u{feff}a!\r\nnew\r\nb\nc\rd");

    // A pasted leading U+FEFF is text, not a new document BOM.
    let mut editor = Editor::from_document(Document::from_text("x"));
    editor.handle_input(I::Character('i'));
    editor.handle_input(I::Paste("\u{feff}y\nz".to_owned()));
    assert_text(&editor, "\u{feff}y\nzx");
    assert!(!editor.document().has_bom());
}

// ---------------------------------------------------------- AC.4 history

#[test]
fn undo_redo_transaction_boundaries() {
    let mut editor = editor_at("|hello world");
    feed(&mut editor, "cwX\\Esc");
    assert_text(&editor, "X world");
    feed(&mut editor, "u");
    assert_text(&editor, "hello world");
    assert_eq!(editor.mode(), EditorMode::Normal);
    feed(&mut editor, "\\C-r");
    assert_text(&editor, "X world");

    // A whole Insert session is one transaction.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "ihello\\CRworld\\Esc");
    assert_text(&editor, "hello\nworldabc");
    feed(&mut editor, "u");
    assert_text(&editor, "abc");
    feed(&mut editor, "u");
    assert_eq!(editor.message(), Some("Already at oldest change"));

    // Counted insertion is a single transaction too.
    let mut editor = editor_at("|x");
    feed(&mut editor, "3iZ\\Esc");
    assert_text(&editor, "ZZZx");
    feed(&mut editor, "u");
    assert_text(&editor, "x");

    // No-op and canceled keys create no history and keep redo alive.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "x");
    feed(&mut editor, "u");
    assert_text(&editor, "abc");
    feed(&mut editor, "dh");
    feed(&mut editor, "d0");
    feed(&mut editor, "dfz");
    feed(&mut editor, "i\\Esc");
    feed(&mut editor, "\\C-r");
    assert_text(&editor, "bc");

    // Branching after undo clears redo.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "x");
    feed(&mut editor, "u");
    feed(&mut editor, "l");
    feed(&mut editor, "x");
    assert_text(&editor, "ac");
    feed(&mut editor, "\\C-r");
    assert_eq!(editor.message(), Some("Already at newest change"));

    // Counts undo and redo whole transactions.
    let mut editor = editor_at("|abcdef");
    feed(&mut editor, "x");
    feed(&mut editor, "x");
    feed(&mut editor, "x");
    assert_text(&editor, "def");
    feed(&mut editor, "2u");
    assert_text(&editor, "bcdef");
    feed(&mut editor, "2\\C-r");
    assert_text(&editor, "def");
}

#[test]
fn history_is_byte_exact() {
    let original = "\u{feff}a\r\nb\nc\rdef";
    let mut editor = Editor::from_document(Document::from_text(original));
    editor.set_cursor(Cursor::new(1, 0));
    feed(&mut editor, "dd");
    let edited = editor.document().serialize();
    assert_text(&editor, "\u{feff}a\r\nc\rdef");
    feed(&mut editor, "u");
    assert_text(&editor, original);
    assert!(editor.document().has_bom());
    assert_eq!(editor.document().line(0), Some("a"));
    feed(&mut editor, "\\C-r");
    assert_text(&editor, &edited);

    // Metadata survives a deep history cycle.
    let mut editor = Editor::from_document(Document::from_text("a\r\nb"));
    editor.set_cursor(Cursor::new(0, 0));
    feed(&mut editor, "iX\\Esc");
    feed(&mut editor, "oY\\Esc");
    feed(&mut editor, "u");
    feed(&mut editor, "u");
    assert_text(&editor, "a\r\nb");
    feed(&mut editor, "\\C-r");
    feed(&mut editor, "\\C-r");
    assert_text(&editor, "Xa\r\nY\r\nb");
    assert_eq!(editor.document().insertion_line_ending(), "\r\n");
}

#[test]
fn dot_repeats_semantic_changes() {
    // The motion is re-evaluated at the new cursor instead of reusing a range.
    let mut editor = editor_at("|foo bar baz");
    feed(&mut editor, "cwX\\Esc");
    assert_text(&editor, "X bar baz");
    feed(&mut editor, "w");
    feed(&mut editor, ".");
    assert_text(&editor, "X X baz");

    // Recorded Insert actions replay with their semantics, arrows included.
    let mut editor = editor_at("|abcd");
    feed(&mut editor, "iX\\RightY\\Esc");
    assert_text(&editor, "XaYbcd");
    assert_cursor(&editor, 0, 2);
    feed(&mut editor, ".");
    assert_text(&editor, "XaXYYbcd");
    assert_cursor(&editor, 0, 4);

    // Delete commands repeat with their motion.
    let mut editor = editor_at("|a b c d");
    feed(&mut editor, "dw");
    assert_text(&editor, "b c d");
    feed(&mut editor, ".");
    assert_text(&editor, "c d");

    // Put replay uses the current unnamed register, not a frozen payload.
    let mut editor = editor_at("|ab\ncd");
    feed(&mut editor, "yyp");
    assert_text(&editor, "ab\nab\ncd");
    feed(&mut editor, "j");
    feed(&mut editor, "yy");
    feed(&mut editor, "k");
    feed(&mut editor, ".");
    assert_text(&editor, "ab\nab\ncd\ncd");

    // Undo and failed edits do not replace the recipe.
    let mut editor = editor_at("|foo bar baz qux");
    feed(&mut editor, "cwX\\Esc");
    feed(&mut editor, "u");
    assert_text(&editor, "foo bar baz qux");
    feed(&mut editor, ".");
    assert_text(&editor, "X bar baz qux");
    feed(&mut editor, "r");
    feed(&mut editor, "\\Esc");
    feed(&mut editor, "w");
    feed(&mut editor, ".");
    assert_text(&editor, "X X baz qux");
    feed(&mut editor, "/zzz\\CR");
    assert_eq!(editor.message(), Some("Pattern not found"));
    feed(&mut editor, "w");
    feed(&mut editor, ".");
    assert_text(&editor, "X X X qux");
}

#[test]
fn counted_dot_replaces_count() {
    let mut editor = editor_at("|abcdefgh");
    feed(&mut editor, "2x");
    assert_text(&editor, "cdefgh");
    feed(&mut editor, ".");
    assert_text(&editor, "efgh");

    let mut editor = editor_at("|abcdefgh");
    feed(&mut editor, "2x");
    feed(&mut editor, "3.");
    assert_text(&editor, "fgh");

    let mut editor = editor_at("|abcdefgh");
    feed(&mut editor, "x");
    feed(&mut editor, "4.");
    assert_text(&editor, "fgh");

    // A multiplied count is recorded as one effective count.
    let mut editor = editor_at("|a b c d e f g h");
    feed(&mut editor, "2d2w");
    assert_text(&editor, "e f g h");
    // Replaying `4dw` on four remaining words consumes the trailing word too.
    feed(&mut editor, ".");
    assert_text(&editor, "");

    // Counted insert sessions replay their unit count through dot.
    let mut editor = editor_at("|x");
    feed(&mut editor, "3iZ\\Esc");
    assert_text(&editor, "ZZZx");
    feed(&mut editor, ".");
    assert_text(&editor, "ZZZZZZx");

    // An explicit 1 is a line number for G and must survive dot replay.
    let mut editor = editor_at("a\nb\n|c\nd\ne");
    feed(&mut editor, "1dG");
    assert_text(&editor, "d\ne");
    feed(&mut editor, ".");
    assert_text(&editor, "e");

    // A count supplied to dot turns an uncounted dG into 1dG.
    let mut editor = editor_at("a\nb\n|c\nd\ne");
    feed(&mut editor, "dG");
    assert_text(&editor, "a\nb");
    feed(&mut editor, "1.");
    assert_text(&editor, "");
}

#[test]
fn visual_dot_uses_selection_shape() {
    // Same-line characterwise shape.
    let mut editor = editor_at("|abcdefgh");
    feed(&mut editor, "vllcX\\Esc");
    assert_text(&editor, "Xdefgh");
    feed(&mut editor, "lll");
    feed(&mut editor, ".");
    assert_text(&editor, "XdeX");

    // A reversed source selection is normalized before recording.
    let mut editor = editor_at("abc|def");
    feed(&mut editor, "vhhcX\\Esc");
    assert_text(&editor, "aXef");
    editor.set_cursor(Cursor::new(0, 2));
    feed(&mut editor, ".");
    assert_text(&editor, "aXX");

    // Multiline characterwise shape records a row span and the final ordinal.
    let mut editor = editor_at("|abcdef\nghijkl\nmnopqr");
    feed(&mut editor, "vlljd");
    assert_text(&editor, "jkl\nmnopqr");
    feed(&mut editor, ".");
    assert_text(&editor, "pqr");

    // Short destination and EOF clamp to the available text.
    let mut editor = editor_at("|abcdefgh");
    feed(&mut editor, "vlllld");
    assert_text(&editor, "fgh");
    feed(&mut editor, ".");
    assert_text(&editor, "");

    let mut editor = editor_at("|a\nb\nc");
    feed(&mut editor, "Vjd");
    assert_text(&editor, "c");
    feed(&mut editor, ".");
    assert_text(&editor, "");

    // Visual P repeats with its shape and the current register.
    let mut editor = editor_at("|xyz\nabc\ndef");
    feed(&mut editor, "yyjVP");
    assert_text(&editor, "xyz\nxyz\ndef");
    feed(&mut editor, "j");
    feed(&mut editor, ".");
    assert_text(&editor, "xyz\nxyz\nxyz");

    // Counts on Visual dot are ignored.
    let mut editor = editor_at("|abcdef");
    feed(&mut editor, "vlld");
    assert_text(&editor, "def");
    editor.set_cursor(Cursor::new(0, 0));
    feed(&mut editor, "3.");
    assert_text(&editor, "");

    // A row span that collapses at the end of the buffer is clamped to the
    // replay start, so only the final cursor grapheme is affected.
    let mut editor = editor_at("|abc\ndefghij");
    feed(&mut editor, "vjd");
    assert_text(&editor, "efghij");
    feed(&mut editor, "G$");
    feed(&mut editor, ".");
    assert_text(&editor, "efghi");

    let mut editor = editor_at("|abc\ndef");
    feed(&mut editor, "vjdG$.");
    assert_text(&editor, "e");

    // A reversed Unicode multiline source records the normalized forward shape,
    // and a shorter destination clamps to a valid selection.
    let mut editor = editor_at("|a👩‍👩‍👧‍👦b\n界cd");
    editor.set_cursor(Cursor::new(1, 0));
    feed(&mut editor, "vkd");
    assert_text(&editor, "cd");
    editor.set_cursor(Cursor::new(0, 0));
    feed(&mut editor, ".");
    assert_text(&editor, "d");
}

// ------------------------------------------------- AC.5 baseline / exit

#[test]
fn undo_redo_crosses_saved_baseline() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("baseline");
    fs::write(&path, "one\ntwo\n").unwrap();
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    assert!(!editor.is_dirty());
    feed(&mut editor, "x");
    assert!(editor.is_dirty());
    command(&mut editor, "w");
    assert!(!editor.is_dirty());
    assert_eq!(fs::read(&path).unwrap(), b"ne\ntwo\n");

    feed(&mut editor, "x");
    assert!(editor.is_dirty());
    feed(&mut editor, "u");
    assert!(
        !editor.is_dirty(),
        "undo back to saved bytes is clean again"
    );
    feed(&mut editor, "\\C-r");
    assert!(editor.is_dirty());
    feed(&mut editor, "u");
    assert!(!editor.is_dirty());

    // A successful `:w <other>` moves the path and the baseline.
    let other = dir.path().join("other");
    command(&mut editor, &format!("w {}", other.display()));
    assert_eq!(editor.path(), Some(other.as_path()));
    assert!(!editor.is_dirty());
    feed(&mut editor, "u");
    assert!(editor.is_dirty());
}

#[test]
fn save_exit_command_contract() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("save-exit");
    fs::write(&path, "keep\n").unwrap();

    // :wq saves through the active path and exits only on success.
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    feed(&mut editor, "ihello \\Esc");
    feed(&mut editor, ":wq\\CR");
    assert!(editor.should_quit());
    assert_eq!(fs::read(&path).unwrap(), b"hello keep\n");

    // :x on a dirty buffer writes and quits.
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    feed(&mut editor, "x");
    feed(&mut editor, ":x\\CR");
    assert!(editor.should_quit());
    assert_eq!(fs::read(&path).unwrap(), b"ello keep\n");

    // :x on a clean buffer exits without writing.
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    let before = fs::read(&path).unwrap();
    feed(&mut editor, ":x\\CR");
    assert!(editor.should_quit());
    assert_eq!(fs::read(&path).unwrap(), before);

    // :q refuses dirty bytes with the exact message.
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    feed(&mut editor, "x");
    feed(&mut editor, ":q\\CR");
    assert!(!editor.should_quit());
    assert_eq!(
        editor.message(),
        Some("Unsaved changes; use :w [path] before :q, or :q! to discard")
    );

    // :q! discards without writing.
    let before = fs::read(&path).unwrap();
    feed(&mut editor, ":q!\\CR");
    assert!(editor.should_quit());
    assert_eq!(fs::read(&path).unwrap(), before);

    // Bounded grammar rejects the rest without side effects.
    for rejected in [
        "wq /tmp/postvim-should-not-exist",
        "x /tmp/postvim-should-not-exist",
        "w!",
        "q!!",
    ] {
        let mut editor = Editor::new(Some(path.clone())).unwrap();
        feed(&mut editor, "x");
        command(&mut editor, rejected);
        assert!(!editor.should_quit());
        assert!(
            editor
                .message()
                .is_some_and(|message| message.starts_with("Unknown command")),
            "{rejected}: {:?}",
            editor.message()
        );
        assert!(editor.is_dirty());
        assert!(!std::path::Path::new("/tmp/postvim-should-not-exist").exists());
    }
}

#[test]
fn failed_save_exit_preserves_session() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("readonly");
    fs::write(&path, "locked").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();

    let mut editor = Editor::new(Some(path.clone())).unwrap();
    feed(&mut editor, "inew \\Esc");
    feed(&mut editor, ":wq\\CR");
    assert!(
        !editor.should_quit(),
        ":wq must not exit after a failed save"
    );
    assert!(editor.is_dirty());
    assert_eq!(editor.path(), Some(path.as_path()));
    assert_text(&editor, "new locked");
    assert!(
        editor
            .message()
            .is_some_and(|message| message.starts_with("Write failed")),
        "{:?}",
        editor.message()
    );
    assert_eq!(fs::read(&path).unwrap(), b"locked");

    // :x behaves the same way when its write fails.
    feed(&mut editor, ":x\\CR");
    assert!(!editor.should_quit());
    assert!(editor.is_dirty());
    assert_eq!(fs::read(&path).unwrap(), b"locked");

    // An unnamed buffer refuses save-and-exit without exiting.
    let mut editor = Editor::new(None).unwrap();
    feed(&mut editor, "idata\\Esc");
    feed(&mut editor, ":wq\\CR");
    assert!(!editor.should_quit());
    assert_eq!(editor.message(), Some("No filename; use :w <path>"));
    assert!(editor.is_dirty());

    let mut editor = Editor::new(None).unwrap();
    feed(&mut editor, "idata\\Esc");
    feed(&mut editor, ":x\\CR");
    assert!(!editor.should_quit());
    assert_eq!(editor.message(), Some("No filename; use :w <path>"));
    assert!(editor.is_dirty());
}

#[test]
fn clean_x_does_not_write() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("never-created");
    let mut editor = Editor::new(Some(missing.clone())).unwrap();
    assert!(!editor.is_dirty());
    feed(&mut editor, ":x\\CR");
    assert!(editor.should_quit());
    assert!(!missing.exists(), "clean :x must not create the file");

    let second = dir.path().join("also-missing");
    let mut editor = Editor::new(Some(second.clone())).unwrap();
    feed(&mut editor, ":x\\CR");
    assert!(editor.should_quit());
    assert!(!second.exists());

    // A clean unnamed buffer exits without a path.
    let mut editor = Editor::new(None).unwrap();
    feed(&mut editor, ":x\\CR");
    assert!(editor.should_quit());
}

#[test]
fn force_quit_does_not_save() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("force-quit");
    fs::write(&path, "keep").unwrap();
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    feed(&mut editor, "ddiZZZ\\Esc");
    assert!(editor.is_dirty());
    feed(&mut editor, ":q!\\CR");
    assert!(editor.should_quit());
    assert_eq!(fs::read(&path).unwrap(), b"keep");

    let mut editor = Editor::new(None).unwrap();
    feed(&mut editor, "idata\\Esc");
    feed(&mut editor, ":q!\\CR");
    assert!(editor.should_quit());
}

// ----------------------------------------------------------- AC.6 Visual

#[test]
fn visual_selection_and_operators() {
    // Mode labels and normalized selection geometry for rendering.
    let mut editor = editor_at("|abcdef");
    feed(&mut editor, "vll");
    assert_eq!(editor.mode(), EditorMode::Visual);
    assert_eq!(editor.mode().label(), "VISUAL");
    assert_eq!(
        editor.selection(),
        Some(postvim::Selection {
            start: Cursor::new(0, 0),
            end: Cursor::new(0, 2),
            kind: SelectionKind::Characterwise,
        })
    );
    feed(&mut editor, "V");
    assert_eq!(editor.mode(), EditorMode::VisualLine);
    assert_eq!(editor.mode().label(), "VISUAL LINE");
    assert_eq!(
        editor.selection(),
        Some(postvim::Selection {
            start: Cursor::new(0, 0),
            end: Cursor::new(0, 5),
            kind: SelectionKind::Linewise,
        })
    );
    feed(&mut editor, "v");
    assert_eq!(editor.mode(), EditorMode::Visual);
    feed(&mut editor, "v");
    assert_eq!(editor.mode(), EditorMode::Normal);
    assert_eq!(editor.selection(), None);

    // Reversed selections normalize and delete the same text.
    let mut editor = editor_at("abcde|f");
    feed(&mut editor, "vhh");
    assert_eq!(
        editor.selection(),
        Some(postvim::Selection {
            start: Cursor::new(0, 3),
            end: Cursor::new(0, 5),
            kind: SelectionKind::Characterwise,
        })
    );
    feed(&mut editor, "d");
    assert_text(&editor, "abc");
    assert_eq!(editor.register().text(), Some("def"));
    assert_cursor(&editor, 0, 2);
    assert_eq!(editor.mode(), EditorMode::Normal);

    // Counts on entry select N graphemes or N lines.
    let mut editor = editor_at("|abcdef");
    feed(&mut editor, "3v");
    assert_eq!(
        editor.selection().map(|selection| selection.end),
        Some(Cursor::new(0, 2))
    );
    feed(&mut editor, "y");
    assert_eq!(editor.register().text(), Some("abc"));

    let mut editor = editor_at("|a\nb\nc\nd");
    feed(&mut editor, "3V");
    assert_eq!(
        editor
            .selection()
            .map(|selection| (selection.start.row, selection.end.row)),
        Some((0, 2))
    );
    feed(&mut editor, "d");
    assert_text(&editor, "d");

    // Motions extend the selection and `$` stops at the last real grapheme.
    let mut editor = editor_at("ab|cdef");
    feed(&mut editor, "v$");
    assert_eq!(
        editor.selection().map(|selection| selection.end),
        Some(Cursor::new(0, 5))
    );
    feed(&mut editor, "d");
    assert_text(&editor, "ab");

    let mut editor = editor_at("|abc\ndef");
    feed(&mut editor, "vj");
    assert_eq!(
        editor
            .selection()
            .map(|selection| (selection.start, selection.end)),
        Some((Cursor::new(0, 0), Cursor::new(1, 0)))
    );
    feed(&mut editor, "y");
    assert_eq!(
        editor.register(),
        &Register::Characterwise("abc\nd".to_owned())
    );

    // x behaves like d; y does not mutate; Esc cancels without mutation.
    let mut editor = editor_at("|abcdef");
    feed(&mut editor, "vlx");
    assert_text(&editor, "cdef");
    let mut editor = editor_at("|abcdef");
    feed(&mut editor, "vly");
    assert_text(&editor, "abcdef");
    assert_eq!(editor.register().text(), Some("ab"));
    let mut editor = editor_at("|abcdef");
    feed(&mut editor, "vll\\Esc");
    assert_text(&editor, "abcdef");
    assert_cursor(&editor, 0, 2);
    assert_eq!(editor.mode(), EditorMode::Normal);

    // Visual change enters Insert at the range start.
    let mut editor = editor_at("ab|cdef");
    feed(&mut editor, "vlcX\\Esc");
    assert_text(&editor, "abXef");
    assert_cursor(&editor, 0, 2);

    // Visual operators share register and history behavior with Normal mode.
    let mut editor = editor_at("|abc\ndef\nghi");
    feed(&mut editor, "Vjd");
    assert_text(&editor, "ghi");
    assert!(editor.register().is_linewise());
    feed(&mut editor, "u");
    assert_text(&editor, "abc\ndef\nghi");
}

#[test]
fn visual_put_type_matrix() {
    // Characterwise register into a characterwise selection.
    let mut editor = editor_at("|abcdef");
    editor.set_cursor(Cursor::new(0, 1));
    feed(&mut editor, "vly");
    assert_eq!(editor.register().text(), Some("bc"));
    editor.set_cursor(Cursor::new(0, 3));
    feed(&mut editor, "vlp");
    assert_text(&editor, "abcbcf");
    assert_eq!(editor.register().text(), Some("de"), "p swaps the register");

    // Characterwise register into a linewise selection replaces whole lines.
    let mut editor = editor_at("|abcdef");
    editor.set_cursor(Cursor::new(0, 1));
    feed(&mut editor, "vly");
    editor.set_cursor(Cursor::new(0, 3));
    feed(&mut editor, "Vp");
    assert_text(&editor, "bc");
    assert!(editor.register().is_linewise());

    // Linewise register into a linewise selection.
    let mut editor = editor_at("|abc\ndef\nghi");
    feed(&mut editor, "yyjVp");
    assert_text(&editor, "abc\nabc\nghi");
    assert_cursor(&editor, 1, 0);

    // Linewise register into a characterwise selection keeps prefix and suffix.
    let mut editor = editor_at("|abc\ndef");
    feed(&mut editor, "yy");
    editor.set_cursor(Cursor::new(1, 1));
    feed(&mut editor, "vlp");
    assert_text(&editor, "abc\nd\nabc\n");

    // An empty register reports and mutates nothing, staying in Visual.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "vlp");
    assert_eq!(editor.message(), Some("Nothing in register"));
    assert_text(&editor, "abc");
    assert_eq!(editor.mode(), EditorMode::Visual);

    // Selections at EOF and on empty buffers stay bounded.
    let mut editor = editor_at("|a\nb");
    editor.set_cursor(Cursor::new(1, 0));
    feed(&mut editor, "yyggVp");
    assert_text(&editor, "b\nb");
    let mut editor = editor_at("|");
    feed(&mut editor, "yyVp");
    assert_text(&editor, "");
    assert_eq!(editor.document().line_count(), 1);

    // Reversed selections behave the same as forward ones for every type pair.
    let mut editor = editor_at("|abcdef");
    editor.set_cursor(Cursor::new(0, 1));
    feed(&mut editor, "vlly");
    assert_eq!(editor.register().text(), Some("bcd"));
    editor.set_cursor(Cursor::new(0, 4));
    feed(&mut editor, "vhhp");
    assert_text(&editor, "abbcdf");
    assert_eq!(editor.register().text(), Some("cde"));

    let mut editor = editor_at("|abc\ndef\nghi");
    feed(&mut editor, "yy");
    editor.set_cursor(Cursor::new(2, 0));
    feed(&mut editor, "VkP");
    assert_text(&editor, "abc\nabc");
    assert_cursor(&editor, 1, 0);

    // An empty characterwise selection on an empty line receives the payload.
    let mut editor = editor_at("|abcdef\n");
    editor.set_cursor(Cursor::new(0, 1));
    feed(&mut editor, "vlly");
    feed(&mut editor, "j");
    feed(&mut editor, "v");
    feed(&mut editor, "p");
    assert_text(&editor, "abcdef\nbcd");

    // A linewise register into a characterwise selection at the end of the buffer.
    let mut editor = editor_at("|abc\ndef");
    editor.set_cursor(Cursor::new(1, 0));
    feed(&mut editor, "yy");
    editor.set_cursor(Cursor::new(0, 2));
    feed(&mut editor, "vlp");
    // Complete lines are inserted between the prefix and the (empty) suffix, so
    // the untouched following row remains after them.
    assert_text(&editor, "ab\ndef\n\ndef");
}

// ----------------------------------------------------------- AC.7 search

#[test]
fn literal_search_navigation() {
    let mut editor = editor_at("|foo bar foo baz foo");
    feed(&mut editor, "/foo\\CR");
    assert_cursor(&editor, 0, 8);
    feed(&mut editor, "n");
    assert_cursor(&editor, 0, 16);
    feed(&mut editor, "n");
    assert_cursor(&editor, 0, 0);
    feed(&mut editor, "N");
    assert_cursor(&editor, 0, 16);
    feed(&mut editor, "n");
    assert_cursor(&editor, 0, 0);

    // Backward search; `n` repeats the recorded direction.
    let mut editor = editor_at("foo bar foo baz |foo");
    feed(&mut editor, "?foo\\CR");
    assert_cursor(&editor, 0, 8);
    feed(&mut editor, "n");
    assert_cursor(&editor, 0, 0);
    feed(&mut editor, "N");
    assert_cursor(&editor, 0, 8);

    // Counts pick the Nth occurrence, cycling through wraps.
    let mut editor = editor_at("|foo foo foo");
    feed(&mut editor, "2/foo\\CR");
    assert_cursor(&editor, 0, 8);
    let mut editor = editor_at("|foo foo foo");
    feed(&mut editor, "5/foo\\CR");
    assert_cursor(&editor, 0, 8);
    let mut editor = editor_at("|foo foo foo");
    feed(&mut editor, "/foo\\CR");
    assert_cursor(&editor, 0, 4);
    feed(&mut editor, "2n");
    assert_cursor(&editor, 0, 0);

    // Query metacharacters are ordinary text.
    let mut editor = editor_at("|a.c abc a.c");
    feed(&mut editor, "/a.c\\CR");
    assert_cursor(&editor, 0, 8);
    feed(&mut editor, "n");
    assert_cursor(&editor, 0, 0);
    let mut editor = editor_at("|x(y)z x(y)z");
    feed(&mut editor, "/(y)\\CR");
    assert_cursor(&editor, 0, 1);

    // Searching never dirties the buffer or creates history.
    let mut editor = editor_at("|foo bar");
    feed(&mut editor, "/bar\\CR");
    assert!(!editor.is_dirty());
    feed(&mut editor, "u");
    assert_eq!(editor.message(), Some("Already at oldest change"));

    // Search inside Visual extends the selection.
    let mut editor = editor_at("|foo bar foo");
    feed(&mut editor, "v/foo\\CR");
    assert_eq!(editor.mode(), EditorMode::Visual);
    assert_eq!(
        editor
            .selection()
            .map(|selection| (selection.start, selection.end)),
        Some((Cursor::new(0, 0), Cursor::new(0, 8)))
    );
    feed(&mut editor, "d");
    assert_text(&editor, "oo");

    // n and N extend a Visual selection too.
    let mut editor = editor_at("|foo bar foo");
    feed(&mut editor, "v/foo\\CRn");
    assert_eq!(
        editor.selection().map(|selection| selection.end),
        Some(Cursor::new(0, 0))
    );
    assert_eq!(editor.mode(), EditorMode::Visual);
}

#[test]
fn search_cancel_and_failure() {
    // Failure keeps the cursor and reports visibly.
    let mut editor = editor_at("|abc def");
    feed(&mut editor, "/zzz\\CR");
    assert_eq!(editor.message(), Some("Pattern not found"));
    assert_cursor(&editor, 0, 0);
    assert_text(&editor, "abc def");
    assert!(!editor.is_dirty());

    // The last nonempty query is remembered even when it was not found.
    feed(&mut editor, "/abc\\CR");
    assert_cursor(&editor, 0, 0);
    feed(&mut editor, "/zzz\\CR");
    assert_eq!(editor.message(), Some("Pattern not found"));
    feed(&mut editor, "/\\CR");
    assert_eq!(editor.message(), Some("Pattern not found"));

    // An empty submission with no memory reports no previous search.
    let mut editor = editor_at("|abc");
    feed(&mut editor, "/\\CR");
    assert_eq!(editor.message(), Some("No previous search"));
    assert_eq!(editor.mode(), EditorMode::Normal);
    feed(&mut editor, "n");
    assert_eq!(editor.message(), Some("No previous search"));

    // Esc restores the pre-prompt mode and selection without replacing memory.
    let mut editor = editor_at("|foo bar foo");
    feed(&mut editor, "/foo\\CR");
    assert_cursor(&editor, 0, 8);
    feed(&mut editor, "/zz\\Esc");
    assert_eq!(editor.mode(), EditorMode::Normal);
    assert_eq!(editor.message(), None);
    assert_eq!(editor.prompt_text(), "");
    feed(&mut editor, "n");
    assert_cursor(&editor, 0, 0);

    let mut editor = editor_at("|foo bar foo");
    feed(&mut editor, "vll");
    let selection = editor.selection();
    feed(&mut editor, "/foo\\Esc");
    assert_eq!(editor.mode(), EditorMode::Visual);
    assert_eq!(editor.selection(), selection);
    assert_cursor(&editor, 0, 2);

    // The prompt owns its text; paste is Insert-only and never executed.
    let mut editor = editor_at("|foo");
    feed(&mut editor, "/fo");
    assert_eq!(editor.prompt_prefix(), Some("/"));
    assert_eq!(editor.prompt_text(), "fo");
    editor.handle_input(I::Paste(":q!\n".to_owned()));
    assert_eq!(editor.prompt_text(), "fo");
    feed(&mut editor, "\\BS");
    assert_eq!(editor.prompt_text(), "f");
    feed(&mut editor, "\\CR");
    assert_eq!(editor.message(), None);
    assert_cursor(&editor, 0, 0);
}

#[test]
fn search_grapheme_boundaries() {
    // A match must start and end on grapheme boundaries.
    let mut editor = editor_at("|e\u{301}x");
    feed(&mut editor, "/\u{301}\\CR");
    assert_eq!(editor.message(), Some("Pattern not found"));
    let mut editor = editor_at("|e\u{301}x");
    feed(&mut editor, "/e\\CR");
    assert_eq!(editor.message(), Some("Pattern not found"));
    let mut editor = editor_at("|e\u{301}x");
    feed(&mut editor, "/e\u{301}\\CR");
    assert_cursor(&editor, 0, 0);

    // ZWJ emoji sequences are indivisible.
    let mut editor = editor_at("|a👩‍👩‍👧‍👦b👩‍👩‍👧‍👦");
    feed(&mut editor, "/👩‍👩‍👧‍👦\\CR");
    assert_cursor(&editor, 0, 1);
    feed(&mut editor, "n");
    assert_cursor(&editor, 0, 3);
    feed(&mut editor, "/👩\\CR");
    assert_eq!(editor.message(), Some("Pattern not found"));

    // Multi-byte matches keep the cursor on a grapheme start.
    let mut editor = editor_at("|世界abc世界");
    feed(&mut editor, "/世界\\CR");
    assert_cursor(&editor, 0, 5);
    assert_invariants(&editor);

    // Searching forward never matches before the cursor without wrapping.
    let mut editor = editor_at("foo |bar abc");
    feed(&mut editor, "/abc\\CR");
    assert_cursor(&editor, 0, 8);
}

#[test]
fn search_prompt_state_is_renderable() {
    let mut editor = editor_at("|abc");
    feed(&mut editor, "2d");
    assert_eq!(editor.pending_text(), "2d");
    feed(&mut editor, "\\Esc");
    assert_eq!(editor.pending_text(), "");
    feed(&mut editor, ":");
    assert_eq!(editor.prompt_prefix(), Some(":"));
    assert_eq!(editor.mode().label(), "COMMAND");
    feed(&mut editor, "wq\\Esc");
    assert_eq!(editor.prompt_prefix(), None);
    feed(&mut editor, "?");
    assert_eq!(editor.prompt_prefix(), Some("?"));
    assert_eq!(editor.mode(), EditorMode::SearchBackward);
    feed(&mut editor, "\\Esc");
    assert_eq!(editor.mode(), EditorMode::Normal);
    assert_eq!(editor.prompt_text(), "");
    assert_eq!(editor.pending_text(), "");
}
