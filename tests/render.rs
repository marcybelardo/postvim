use postvim::document::Document;
use postvim::editor::{Editor, EditorInput, EditorMode, SelectionKind};
use postvim::ui;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier};

/// Background the renderer uses for Visual selection spans.
const VISUAL_BACKGROUND: Color = Color::DarkGray;

#[test]
fn external_change_status_render() {
    use std::time::{Duration, Instant};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("document");
    std::fs::write(&path, "original").unwrap();
    let mut editor = Editor::new(Some(path.clone())).unwrap();
    editor.handle_input(press('i'));
    editor.handle_input(press('x'));
    editor.handle_input(EditorInput::Escape);
    std::fs::write(&path, "external").unwrap();
    let now = Instant::now();
    editor.poll_disk(now);
    editor.poll_disk(now + Duration::from_millis(500));
    let status = editor
        .external_status()
        .expect("persistent conflict")
        .to_owned();
    editor.handle_input(press('h'));
    assert_eq!(editor.external_status(), Some(status.as_str()));
    let terminal = render_to(&editor, 160, 3);
    assert!(row(&terminal, 1).contains(&status));
    for width in [1, 2, 8, 20] {
        for height in [1, 2, 3] {
            let terminal = render_to(&editor, width, height);
            for y in 0..height {
                assert!(!row(&terminal, y).contains('\u{1b}'));
            }
        }
    }
}

const ALL_MODES: [EditorMode; 7] = [
    EditorMode::Normal,
    EditorMode::Insert,
    EditorMode::Command,
    EditorMode::Visual,
    EditorMode::VisualLine,
    EditorMode::SearchForward,
    EditorMode::SearchBackward,
];

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn press(ch: char) -> EditorInput {
    EditorInput::Character(ch)
}

fn editor_with_text(text: &str) -> Editor {
    Editor::from_document(Document::from_text(text))
}

fn render_to(editor: &Editor, width: u16, height: u16) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    terminal
        .draw(|frame| ui::render(frame, editor))
        .expect("render");
    terminal
}

fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect::<String>()
}

fn cursor(terminal: &mut Terminal<TestBackend>) -> Position {
    terminal.get_cursor_position().unwrap()
}

fn text_at(terminal: &Terminal<TestBackend>, y: u16) -> String {
    row(terminal, y).trim().to_owned()
}

fn symbol_at(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> String {
    terminal.backend().buffer()[(x, y)].symbol().to_owned()
}

fn background_at(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> Color {
    terminal.backend().buffer()[(x, y)].bg
}

/// Whether the cell carries the reverse video the renderer uses for a block cursor.
fn is_cursor_cell(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> bool {
    terminal.backend().buffer()[(x, y)]
        .modifier
        .contains(Modifier::REVERSED)
}

/// Columns `0..=end` of one row that carry the selection background.
fn selected_span(terminal: &Terminal<TestBackend>, y: u16, end: u16) -> bool {
    (0..=end).all(|x| background_at(terminal, x, y) == VISUAL_BACKGROUND)
}

/// Row text with the reserved marker column excluded from the comparison.
fn text_after(terminal: &Terminal<TestBackend>, y: u16, left: u16) -> String {
    let buffer = terminal.backend().buffer();
    (left..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect::<String>()
        .trim()
        .to_owned()
}

/// First and last drawn columns at or after `left`, so the reserved
/// end-of-buffer marker column can be excluded from layout assertions.
fn ink_span(terminal: &Terminal<TestBackend>, y: u16, left: u16) -> Option<(u16, u16)> {
    let buffer = terminal.backend().buffer();
    let mut first: Option<u16> = None;
    let mut last: Option<u16> = None;
    for x in left..buffer.area.width {
        if buffer[(x, y)].symbol() != " " {
            first.get_or_insert(x);
            last = Some(x);
        }
    }
    first.zip(last)
}

/// Assert one rendered row is floor-centered inside `left..right`.
fn assert_line_centered(terminal: &Terminal<TestBackend>, y: u16, left: u16, right: u16) {
    let width = usize::from(right - left);
    let (first, last) =
        ink_span(terminal, y, left).unwrap_or_else(|| panic!("row {y} has no drawn text"));
    let ink = usize::from(last - first) + 1;
    let left_slack = usize::from(first - left);
    let right_slack = usize::from(right - last) - 1;
    assert_eq!(
        left_slack,
        (width - ink) / 2,
        "row {y} is not floor-centered: {first}..={last}"
    );
    assert!(
        right_slack >= left_slack && right_slack - left_slack <= 1,
        "row {y} is not centered: left slack {left_slack}, right slack {right_slack}"
    );
}

/// Rows of the content area that carry welcome text, ignoring the marker column.
fn block_rows(terminal: &Terminal<TestBackend>, content_height: u16) -> Vec<u16> {
    (0..content_height)
        .filter(|y| ink_span(terminal, *y, 1).is_some())
        .collect()
}

const WELCOME_HINTS: [&str; 4] = [
    "i — edit",
    "Esc — normal mode",
    ":w <path> — save",
    ":q — quit",
];

#[test]
fn render_welcome_and_modes() {
    let mut editor = Editor::new(None).expect("unnamed editor");
    let mut terminal = render_to(&editor, 64, 16);
    let screen = (0..16).map(|y| row(&terminal, y)).collect::<String>();
    assert!(screen.contains("Postvim"));
    assert!(screen.contains("Writing has nothing to do"));
    assert!(screen.contains("i — edit"));
    assert!(screen.contains("NORMAL  [No Name]"));

    let attribution = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .find(|cell| cell.symbol() == "—")
        .expect("welcome quote attribution is visible");
    assert_eq!(attribution.fg, Color::DarkGray);

    editor.handle_input(EditorInput::Character('i'));
    assert_eq!(editor.mode(), EditorMode::Insert);
    terminal
        .draw(|frame| ui::render(frame, &editor))
        .expect("render insert");
    assert!(!row(&terminal, 0).contains("Postvim"));
    assert!(row(&terminal, 14).contains("INSERT"));
    assert_eq!(cursor(&mut terminal), Position::new(0, 0));

    editor.handle_input(EditorInput::Character('x'));
    assert!(editor.is_dirty());
    terminal
        .draw(|frame| ui::render(frame, &editor))
        .expect("render dirty insert");
    assert!(row(&terminal, 14).contains("INSERT"));
    assert!(row(&terminal, 14).contains("[+]"));
    assert_eq!(cursor(&mut terminal), Position::new(1, 0));

    editor.handle_input(EditorInput::Escape);
    assert_eq!(editor.mode(), EditorMode::Normal);
    terminal
        .draw(|frame| ui::render(frame, &editor))
        .expect("render normal");
    assert!(row(&terminal, 14).contains("NORMAL"));
    assert!(row(&terminal, 14).contains("[+]"));
    assert_eq!(cursor(&mut terminal), Position::new(0, 0));
    assert_eq!(terminal.backend().buffer()[(0, 14)].symbol(), " ");
}

#[test]
fn render_command_and_errors() {
    let mut editor = editor_with_text("abc");
    editor.handle_input(EditorInput::Character(':'));
    editor.handle_input(EditorInput::Character('w'));
    editor.handle_input(EditorInput::Character(' '));
    editor.handle_input(EditorInput::Character('o'));
    editor.handle_input(EditorInput::Character('k'));
    let mut terminal = render_to(&editor, 24, 5);
    assert_eq!(editor.mode(), EditorMode::Command);
    assert!(row(&terminal, 4).starts_with(":w ok"));
    assert!(row(&terminal, 3).contains("COMMAND"));
    assert_eq!(cursor(&mut terminal), Position::new(5, 4));

    // Executing :w without an existing path reports the filename error without
    // creating a file in the test working directory.
    let mut unnamed = editor_with_text("abc");
    unnamed.handle_input(EditorInput::Character(':'));
    unnamed.handle_input(EditorInput::Character('w'));
    let prompt = render_to(&unnamed, 40, 5);
    assert!(row(&prompt, 4).starts_with(":w"));
    unnamed.handle_input(EditorInput::Enter);
    let terminal = render_to(&unnamed, 40, 5);
    assert_eq!(unnamed.mode(), EditorMode::Normal);
    assert!(row(&terminal, 4).contains("No filename; use :w <path>"));
    assert!(row(&terminal, 3).contains("NORMAL"));
}

#[test]
fn render_unicode_tabs_and_scrolling() {
    let mut editor = editor_with_text("a\t界e\u{301}🙂XYZ");
    for _ in 0..8 {
        editor.handle_input(EditorInput::Character('l'));
    }
    let mut terminal = render_to(&editor, 7, 5);
    let content = row(&terminal, 0);
    assert!(content.contains("🙂"));
    assert_eq!(cursor(&mut terminal), Position::new(5, 0));
    assert_eq!(terminal.backend().buffer()[(5, 0)].symbol(), "Z");
    assert!(!content.contains('�'));

    let mut tab_cursor = editor_with_text("a\tb");
    tab_cursor.handle_input(EditorInput::Character('l'));
    let mut terminal = render_to(&tab_cursor, 12, 5);
    assert_eq!(cursor(&mut terminal), Position::new(1, 0));
    assert_eq!(terminal.backend().buffer()[(1, 0)].symbol(), " ");
    assert_eq!(terminal.backend().buffer()[(2, 0)].symbol(), " ");
    assert_eq!(terminal.backend().buffer()[(3, 0)].symbol(), " ");

    let mut tabs = editor_with_text("a\tb");
    for _ in 0..4 {
        tabs.handle_input(EditorInput::Character('l'));
    }
    let mut terminal = render_to(&tabs, 12, 5);
    assert_eq!(terminal.backend().buffer()[(4, 0)].symbol(), "b");
    assert_eq!(cursor(&mut terminal), Position::new(4, 0));

    // A wide grapheme is omitted rather than partially rendered or assigned a
    // cursor cell when only one column remains in the viewport.
    let mut clipped = editor_with_text("a界x");
    clipped.handle_input(EditorInput::Character('l'));
    let terminal = render_to(&clipped, 1, 5);
    assert_eq!(row(&terminal, 0), " ");
    assert!(!row(&terminal, 0).contains('界'));

    let mut vertical = editor_with_text("one\ntwo\nthree\nfour\nfive");
    for _ in 0..4 {
        vertical.handle_input(EditorInput::Character('j'));
    }
    let mut terminal = render_to(&vertical, 12, 4);
    assert!(row(&terminal, 1).contains("five"));
    assert_eq!(cursor(&mut terminal), Position::new(0, 1));
}

#[test]
fn render_tiny_and_resized_terminals() {
    let mut editor = editor_with_text("wide界line\nsecond");
    for _ in 0..3 {
        editor.handle_input(EditorInput::Character('l'));
    }
    let backend = TestBackend::new(1, 1);
    let mut terminal = Terminal::new(backend).expect("tiny terminal");
    terminal
        .draw(|frame| ui::render(frame, &editor))
        .expect("tiny render");
    let tiny_cursor = cursor(&mut terminal);
    assert!(tiny_cursor.x < 1);
    assert!(tiny_cursor.y < 1);

    terminal.backend_mut().resize(0, 1);
    terminal
        .draw(|frame| ui::render(frame, &editor))
        .expect("zero-width render");
    assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, 0, 1));

    terminal.backend_mut().resize(1, 0);
    terminal
        .draw(|frame| ui::render(frame, &editor))
        .expect("zero-height render");
    assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, 1, 0));

    terminal.backend_mut().resize(0, 0);
    terminal
        .draw(|frame| ui::render(frame, &editor))
        .expect("zero render");
    assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, 0, 0));

    terminal.backend_mut().resize(30, 8);
    terminal
        .draw(|frame| ui::render(frame, &editor))
        .expect("resized render");
    assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, 30, 8));
    let resized_cursor = cursor(&mut terminal);
    assert!(resized_cursor.x < 30);
    assert!(resized_cursor.y < 8);

    // The welcome overlay survives the smallest drawable areas and resize
    // sequences: nothing is drawn outside the frame and no grapheme is split.
    let welcome = Editor::new(None).expect("unnamed welcome editor");
    assert!(welcome.show_welcome());
    for (width, height) in [
        (0u16, 0u16),
        (1, 1),
        (1, 2),
        (2, 1),
        (2, 2),
        (3, 3),
        (4, 6),
        (76, 5),
        (200, 40),
    ] {
        terminal.backend_mut().resize(width, height);
        terminal
            .draw(|frame| ui::render(frame, &welcome))
            .expect("welcome render");
        assert_eq!(
            terminal.backend().buffer().area,
            Rect::new(0, 0, width, height),
            "{width}x{height} welcome render"
        );
        for y in 0..height {
            assert!(
                !row(&terminal, y).contains('\u{fffd}'),
                "{width}x{height} row {y} kept every grapheme whole"
            );
        }
        if height >= 3 && width >= 12 {
            assert!(
                row(&terminal, height - 2).contains("NORMAL"),
                "{width}x{height} keeps the status row"
            );
        }
    }
}

#[test]
fn render_control_characters_safely() {
    let editor = editor_with_text("A\u{1b}B\0C\u{7f}D\u{85}E");
    let terminal = render_to(&editor, 32, 5);
    let content = row(&terminal, 0);
    assert!(content.contains("A^[B^@C^?D�E"));
    assert!(!content.contains('\u{1b}'));
}

#[test]
fn welcome_block_is_centered() {
    for (width, height) in [(80u16, 24u16), (120, 30), (200, 40)] {
        let editor = Editor::new(None).expect("unnamed editor");
        assert!(editor.show_welcome());
        let terminal = render_to(&editor, width, height);
        let content_height = height - 2;

        // End-of-buffer markers own the leftmost content column and stay
        // visible behind and beside the overlay.
        for y in 1..content_height {
            assert_eq!(
                terminal.backend().buffer()[(0, y)].symbol(),
                "~",
                "{width}x{height} row {y} marker"
            );
            assert_eq!(terminal.backend().buffer()[(0, y)].fg, Color::DarkGray);
        }

        // The whole block is floor-centered vertically in the editing area.
        let rows = block_rows(&terminal, content_height);
        let first = *rows.first().expect("welcome block is visible");
        let last = *rows.last().expect("welcome block ends");
        let block_height = usize::from(last - first + 1);
        let slack = usize::from(content_height) - block_height;
        assert_eq!(
            usize::from(first),
            slack / 2,
            "{width}x{height} block is not floor-centered vertically"
        );
        let bottom_slack = usize::from(content_height - 1 - last);
        assert!(
            bottom_slack == slack / 2 || bottom_slack == slack / 2 + 1,
            "{width}x{height} vertical slack {slack} split as {first} top / {bottom_slack} bottom"
        );

        // Each drawn line is centered in the region right of the marker column
        // and never grows past the documented wrap width.
        for y in first..=last {
            if let Some((start, end)) = ink_span(&terminal, y, 1) {
                assert!(
                    usize::from(end - start) < 76,
                    "{width}x{height} row {y} is wider than the welcome wrap width"
                );
                assert_line_centered(&terminal, y, 1, width);
            }
        }

        // Wording, styles and controls are unchanged from the previous welcome.
        let lines: Vec<String> = (0..content_height)
            .map(|y| text_after(&terminal, y, 1))
            .collect();
        assert_eq!(lines[usize::from(first)], "Postvim");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Writing has nothing to do with"))
        );
        assert!(lines.iter().any(|line| line.contains("Gilles Deleuze")));
        for hint in WELCOME_HINTS {
            assert!(
                lines.iter().any(|line| line.contains(hint)),
                "{width}x{height} shows hint {hint}"
            );
        }

        let (title_x, _) = ink_span(&terminal, first, 1).expect("title row");
        assert!(
            terminal.backend().buffer()[(title_x, first)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let attribution_y = (first..=last)
            .find(|y| text_after(&terminal, *y, 1).contains("Gilles Deleuze"))
            .expect("attribution is visible");
        let (attribution_x, _) = ink_span(&terminal, attribution_y, 1).expect("attribution text");
        assert_eq!(
            terminal.backend().buffer()[(attribution_x, attribution_y)].fg,
            Color::DarkGray
        );
    }
}

#[test]
fn eof_tildes_distinguish_blank_lines() {
    // Four logical lines, two of them genuinely empty: only rows past the
    // fourth logical line may show a marker.
    let editor = editor_with_text("first\n\nthird\n");
    assert_eq!(editor.document().line_count(), 4);
    let terminal = render_to(&editor, 12, 8);
    let content_height = 6;
    assert_eq!(content_height, 8 - 2);
    for y in 0..content_height {
        let symbol = terminal.backend().buffer()[(0, y)].symbol().to_owned();
        if usize::from(y) < 4 {
            assert_ne!(symbol, "~", "row {y} is a real line, blank or not");
        } else {
            assert_eq!(symbol, "~", "row {y} is past the final line");
            assert_eq!(terminal.backend().buffer()[(0, y)].fg, Color::DarkGray);
        }
    }
    assert_eq!(text_at(&terminal, 0), "first");
    assert_eq!(text_at(&terminal, 1), "");
    assert_eq!(text_at(&terminal, 2), "third");
    assert_eq!(text_at(&terminal, 3), "");
    assert_eq!(text_at(&terminal, 4), "~");
    assert_eq!(text_at(&terminal, 5), "~");

    // Status and prompt rows never carry markers.
    assert!(row(&terminal, 6).contains("NORMAL"));
    assert!(!row(&terminal, 6).contains('~'));
    assert!(!row(&terminal, 7).contains('~'));

    // Markers ignore horizontal scrolling: the long first line is scrolled
    // sideways while the row past it keeps its marker in column zero.
    let mut scrolled = editor_with_text(&format!("{}tail\nsecond", "0123456789".repeat(4)));
    assert_eq!(scrolled.document().line_count(), 2);
    for _ in 0..30 {
        scrolled.handle_input(EditorInput::Character('l'));
    }
    let terminal = render_to(&scrolled, 10, 6);
    assert_eq!(row(&terminal, 0), "1234567890");
    // The second line is scrolled out of view, but it is a real line, so its
    // leftmost column stays blank instead of turning into a marker.
    assert_eq!(row(&terminal, 1), " ".repeat(10));
    for y in 2..4 {
        assert_eq!(terminal.backend().buffer()[(0, y)].symbol(), "~");
        assert_eq!(terminal.backend().buffer()[(0, y)].fg, Color::DarkGray);
    }
    assert!(row(&terminal, 4).contains("NORMAL"));
}

#[test]
fn welcome_compact_layout() {
    let editor = Editor::new(None).expect("unnamed editor");

    // Short terminal: the quote cannot fit, so the compact fallback keeps the
    // title and every control hint instead.
    let terminal = render_to(&editor, 40, 10);
    let content: Vec<String> = (0..8).map(|y| text_after(&terminal, y, 1)).collect();
    assert_eq!(content[1], "Postvim");
    assert_eq!(content[0], "");
    for hint in WELCOME_HINTS {
        assert!(
            content.iter().any(|line| line.contains(hint)),
            "compact layout keeps {hint}"
        );
    }
    assert!(
        !content
            .iter()
            .any(|line| line.contains("Writing has nothing"))
    );
    assert!(!content.iter().any(|line| line.contains("Deleuze")));
    for y in 0..8 {
        if ink_span(&terminal, y, 1).is_some() {
            assert_line_centered(&terminal, y, 1, 40);
        }
    }
    for y in 1..8 {
        assert_eq!(terminal.backend().buffer()[(0, y)].symbol(), "~");
    }
    assert!(row(&terminal, 8).contains("NORMAL"));
    assert_eq!(text_at(&terminal, 9), "");

    // Narrow but tall: the full block wraps into the narrow column and never
    // spills into the marker column, the status bar or the prompt row.
    let terminal = render_to(&editor, 20, 30);
    let content: Vec<String> = (0..28).map(|y| text_after(&terminal, y, 1)).collect();
    assert!(content.iter().any(|line| line.contains("Writing has")));
    assert!(content.iter().any(|line| line.contains("Deleuze")));
    for hint in WELCOME_HINTS {
        assert!(content.iter().any(|line| line.contains(hint)));
    }
    for y in 0..28 {
        if let Some((start, end)) = ink_span(&terminal, y, 1) {
            assert!(
                usize::from(end - start) < 19,
                "narrow welcome row {y} overflows the 19-cell region"
            );
            assert_line_centered(&terminal, y, 1, 20);
        }
    }
    assert!(row(&terminal, 28).contains("NORMAL"));
    assert_eq!(text_at(&terminal, 29), "");

    // Very short: the title survives and the leading hints clip first.
    let terminal = render_to(&editor, 80, 6);
    let content: Vec<String> = (0..4).map(|y| text_after(&terminal, y, 1)).collect();
    assert_eq!(content[0], "Postvim");
    assert_eq!(content[1], "i — edit");
    assert!(content[2].contains("Esc — normal mode"));
    assert!(!content.iter().any(|line| line.contains(":q — quit")));

    // A single editing row keeps the title alone.
    let terminal = render_to(&editor, 80, 3);
    assert_eq!(text_at(&terminal, 0), "Postvim");
    assert_line_centered(&terminal, 0, 1, 80);
    assert!(row(&terminal, 1).contains("NORMAL"));
}

#[test]
fn render_visual_selection_and_modes() {
    // Characterwise selection: the anchor and the cursor are both inclusive, so
    // `v` plus four `l` presses selects exactly "alpha".
    let mut editor = editor_with_text("alpha beta\nsecond line\nthird");
    editor.handle_input(press('v'));
    for _ in 0..4 {
        editor.handle_input(press('l'));
    }
    assert_eq!(editor.mode(), EditorMode::Visual);
    assert_eq!(
        editor.selection().expect("visual selection").kind,
        SelectionKind::Characterwise
    );
    let mut terminal = render_to(&editor, 20, 6);
    assert!(row(&terminal, 4).contains("VISUAL"));
    assert!(!row(&terminal, 4).contains("VISUAL LINE"));

    // Every selected grapheme span carries the selection background; the cell
    // after the selection and the next row stay unselected.
    assert!(selected_span(&terminal, 0, 4), "alpha is highlighted");
    assert_eq!(background_at(&terminal, 5, 0), Color::Reset);
    assert_eq!(background_at(&terminal, 0, 1), Color::Reset);

    // The cursor stays a block on top of the selection background.
    assert!(is_cursor_cell(&terminal, 4, 0), "cursor kept its block");
    assert!(!is_cursor_cell(&terminal, 0, 0));
    assert_eq!(cursor(&mut terminal), Position::new(4, 0));

    // Extending over a line break selects the rest of the first row and the
    // start of the second one.
    editor.handle_input(press('j'));
    let terminal = render_to(&editor, 20, 6);
    assert!(
        selected_span(&terminal, 0, 9),
        "row 0 is selected to its end"
    );
    assert!(
        selected_span(&terminal, 1, 4),
        "row 1 is selected up to the cursor"
    );
    assert_eq!(background_at(&terminal, 5, 1), Color::Reset);
    assert_eq!(background_at(&terminal, 0, 2), Color::Reset);

    // `V` switches to linewise selection and reports VISUAL LINE.
    editor.handle_input(press('V'));
    assert_eq!(editor.mode(), EditorMode::VisualLine);
    assert_eq!(
        editor.selection().expect("linewise selection").kind,
        SelectionKind::Linewise
    );
    let terminal = render_to(&editor, 20, 6);
    assert!(row(&terminal, 4).contains("VISUAL LINE"));
    assert!(
        selected_span(&terminal, 0, 9),
        "linewise row 0 is fully selected"
    );
    assert!(
        selected_span(&terminal, 1, 10),
        "linewise row 1 is fully selected"
    );
    assert_eq!(background_at(&terminal, 0, 2), Color::Reset);

    // Esc cancels without touching the text and clears every highlight.
    editor.handle_input(EditorInput::Escape);
    assert_eq!(editor.mode(), EditorMode::Normal);
    assert_eq!(editor.document().line(0), Some("alpha beta"));
    let terminal = render_to(&editor, 20, 6);
    assert!(row(&terminal, 4).contains("NORMAL"));
    assert_eq!(background_at(&terminal, 0, 0), Color::Reset);
    assert_eq!(background_at(&terminal, 4, 0), Color::Reset);

    // Tabs, wide cells, combining marks and safe controls keep their complete
    // rendered span and stay selected as one span each.
    let mut unicode = editor_with_text("a\t界e\u{301}🙂\u{1b}0");
    unicode.handle_input(press('v'));
    for _ in 0..6 {
        unicode.handle_input(press('l'));
    }
    let terminal = render_to(&unicode, 24, 4);
    for x in 1..=3u16 {
        assert_eq!(
            background_at(&terminal, x, 0),
            VISUAL_BACKGROUND,
            "tab expansion cell {x} is selected"
        );
    }
    assert_eq!(symbol_at(&terminal, 4, 0), "界");
    assert_eq!(
        background_at(&terminal, 4, 0),
        VISUAL_BACKGROUND,
        "the wide grapheme's leading cell carries the whole highlighted span"
    );
    assert_eq!(symbol_at(&terminal, 6, 0), "e\u{301}");
    assert_eq!(background_at(&terminal, 6, 0), VISUAL_BACKGROUND);
    assert_eq!(symbol_at(&terminal, 7, 0), "🙂");
    assert_eq!(symbol_at(&terminal, 9, 0), "^");
    assert_eq!(symbol_at(&terminal, 10, 0), "[");
    assert_eq!(background_at(&terminal, 10, 0), VISUAL_BACKGROUND);
    assert!(!row(&terminal, 0).contains('\u{1b}'));
    assert!(!row(&terminal, 0).contains('\u{fffd}'));

    // A ZWJ emoji stays one grapheme and one highlighted span.
    let mut zwj = editor_with_text("👨‍👩‍👧x");
    zwj.handle_input(press('v'));
    zwj.handle_input(press('l'));
    let terminal = render_to(&zwj, 16, 4);
    let buffer = terminal.backend().buffer();
    let (x, y, cell) = buffer
        .content
        .iter()
        .enumerate()
        .find(|(_, cell)| cell.symbol() == "👨‍👩‍👧")
        .map(|(index, cell)| {
            let index = index as u16;
            (index % buffer.area.width, index / buffer.area.width, cell)
        })
        .expect("the ZWJ emoji renders as one grapheme");
    assert_eq!(cell.bg, VISUAL_BACKGROUND, "ZWJ emoji span is selected");
    assert_eq!(y, 0);
    assert!(!row(&terminal, 0).contains('\u{fffd}'));
    assert_ne!(symbol_at(&terminal, x + 2, 0), "");

    // The highlight follows horizontal scrolling: every visible cell of the
    // selected line is selected and the block cursor stays in view.
    let mut scrolled = editor_with_text(&format!("{}tail", "0123456789".repeat(4)));
    scrolled.handle_input(press('v'));
    for _ in 0..42 {
        scrolled.handle_input(press('l'));
    }
    let mut terminal = render_to(&scrolled, 10, 4);
    assert!(selected_span(&terminal, 0, 9), "visible window is selected");
    assert!(is_cursor_cell(&terminal, 9, 0));
    assert_eq!(symbol_at(&terminal, 9, 0), "i");
    assert_eq!(cursor(&mut terminal), Position::new(9, 0));

    // Resizing keeps the selection inside the drawable frame.
    for (width, height) in [(1u16, 1u16), (3, 2), (40, 10)] {
        terminal.backend_mut().resize(width, height);
        terminal
            .draw(|frame| ui::render(frame, &scrolled))
            .expect("visual render after resize");
        for y in 0..height {
            let drawn = row(&terminal, y);
            assert!(!drawn.contains('\u{fffd}'), "{width}x{height} row {y}");
        }
        if width >= 2 && height >= 3 {
            assert!(row(&terminal, height - 2).contains("VISUAL"));
        }
    }
}

#[test]
fn render_search_and_pending_prompts() {
    // Forward and backward search prompts show their own prefix and text, and
    // the status row keeps a distinct mode label.
    let mut editor = editor_with_text("alpha beta gamma\nalpha again");
    editor.handle_input(press('/'));
    assert_eq!(editor.mode(), EditorMode::SearchForward);
    for ch in "al".chars() {
        editor.handle_input(press(ch));
    }
    let mut terminal = render_to(&editor, 24, 5);
    assert!(row(&terminal, 4).starts_with("/al"));
    assert!(row(&terminal, 3).starts_with(" / "));
    assert_eq!(cursor(&mut terminal), Position::new(3, 4));

    editor.handle_input(EditorInput::Escape);
    editor.handle_input(press('?'));
    assert_eq!(editor.mode(), EditorMode::SearchBackward);
    for ch in "al".chars() {
        editor.handle_input(press(ch));
    }
    let mut terminal = render_to(&editor, 24, 5);
    assert!(row(&terminal, 4).starts_with("?al"));
    assert!(row(&terminal, 3).starts_with(" ? "));
    assert_eq!(cursor(&mut terminal), Position::new(3, 4));

    // The command prompt keeps its ':' prefix.
    editor.handle_input(EditorInput::Escape);
    editor.handle_input(press(':'));
    editor.handle_input(press('w'));
    let mut terminal = render_to(&editor, 24, 5);
    assert!(row(&terminal, 4).starts_with(":w"));
    assert!(row(&terminal, 3).starts_with(" COMMAND"));
    assert_eq!(cursor(&mut terminal), Position::new(2, 4));

    // A long Unicode prompt is clipped from the left on grapheme boundaries so
    // the prompt cursor always stays inside the row.
    let mut long = editor_with_text("x");
    long.handle_input(press('/'));
    let query = "界".repeat(20);
    for ch in query.chars() {
        long.handle_input(press(ch));
    }
    let mut terminal = render_to(&long, 12, 4);
    let prompt = row(&terminal, 3);
    assert!(prompt.starts_with('/'));
    assert!(!prompt.contains('\u{fffd}'));
    // Wide graphemes leave their continuation cell blank, so drop those cells
    // before comparing against the typed query.
    let shown: String = prompt
        .trim_start_matches('/')
        .chars()
        .filter(|ch| *ch != ' ')
        .collect();
    assert!(
        query.ends_with(&shown),
        "clipped prompt {shown:?} is a suffix of the query"
    );
    assert_eq!(
        shown.chars().count(),
        5,
        "ten cells of width-2 graphemes fit"
    );
    let position = cursor(&mut terminal);
    assert!(position.x < 12, "prompt cursor stays in bounds");
    assert_eq!(position.x, 11);
    assert_eq!(position.y, 3);

    // An odd available width cannot split a wide grapheme: it drops it instead.
    let mut terminal = render_to(&long, 11, 4);
    let prompt = row(&terminal, 3);
    let shown: String = prompt
        .trim_start_matches('/')
        .chars()
        .filter(|ch| *ch != ' ')
        .collect();
    assert_eq!(
        shown.chars().count(),
        4,
        "an odd cell count keeps whole graphemes"
    );
    let position = cursor(&mut terminal);
    assert_eq!(position.x, 9);
    assert!(position.x < 11);

    // Errors stay on the prompt row and never displace the mode label.
    let mut failing = editor_with_text("abc");
    failing.handle_input(press('/'));
    for ch in "zzz".chars() {
        failing.handle_input(press(ch));
    }
    failing.handle_input(EditorInput::Enter);
    assert_eq!(failing.mode(), EditorMode::Normal);
    let terminal = render_to(&failing, 40, 5);
    assert!(row(&terminal, 4).contains("Pattern not found"));
    assert_eq!(terminal.backend().buffer()[(0, 4)].fg, Color::LightRed);
    assert!(row(&terminal, 3).contains("NORMAL"));

    // The pending count/operator prefix is a separate status field, so it never
    // overwrites the message row.
    let mut pending = editor_with_text("one two three");
    pending.handle_input(press('2'));
    pending.handle_input(press('d'));
    assert_eq!(pending.pending_text(), "2d");
    let terminal = render_to(&pending, 40, 5);
    assert!(row(&terminal, 3).contains("NORMAL"));
    assert!(text_at(&terminal, 3).ends_with("2d"));
    assert_eq!(text_at(&terminal, 4), "", "pending input is not an error");

    pending.handle_input(press('f'));
    assert_eq!(pending.pending_text(), "2df");
    let terminal = render_to(&pending, 40, 5);
    assert!(text_at(&terminal, 3).ends_with("2df"));
    assert_eq!(text_at(&terminal, 4), "");

    // Esc clears the prefix and the status row drops it again.
    pending.handle_input(EditorInput::Escape);
    assert_eq!(pending.pending_text(), "");
    let terminal = render_to(&pending, 40, 5);
    assert!(!row(&terminal, 3).contains("2df"));
    assert!(row(&terminal, 3).contains("NORMAL"));

    // A prompt row and a pending prefix never claim the same row: while the
    // search prompt is open the message row shows the query, not the error.
    let mut both = editor_with_text("abc");
    both.handle_input(press('/'));
    both.handle_input(press('b'));
    let terminal = render_to(&both, 40, 5);
    assert!(row(&terminal, 4).starts_with("/b"));
    assert!(!row(&terminal, 3).contains("/b"));
}

#[test]
fn terminal_key_modifier_contract() {
    // Ctrl-r is redo, and only in Normal mode.
    let ctrl_r = key_event(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(
        ui::map_key(ctrl_r, EditorMode::Normal),
        Some(EditorInput::Redo)
    );
    for mode in ALL_MODES {
        if mode != EditorMode::Normal {
            assert_eq!(ui::map_key(ctrl_r, mode), None, "Ctrl-r in {mode:?}");
        }
    }

    // Repeat presses keep working; release events are ignored in every mode.
    let ctrl_r_repeat = KeyEvent::new_with_kind(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
        KeyEventKind::Repeat,
    );
    assert_eq!(
        ui::map_key(ctrl_r_repeat, EditorMode::Normal),
        Some(EditorInput::Redo)
    );
    for code in [
        KeyCode::Char('r'),
        KeyCode::Char('a'),
        KeyCode::Char('v'),
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Tab,
        KeyCode::Left,
    ] {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT] {
            let release = KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Release);
            for mode in ALL_MODES {
                assert_eq!(
                    ui::map_key(release, mode),
                    None,
                    "{code:?} release with {modifiers:?} in {mode:?}"
                );
            }
        }
    }

    // Every other Control or Alt combination stays rejected in every mode.
    for code in [
        KeyCode::Char('c'),
        KeyCode::Char('d'),
        KeyCode::Char('q'),
        KeyCode::Char('['),
        KeyCode::Char('A'),
        KeyCode::Enter,
        KeyCode::Left,
        KeyCode::Tab,
    ] {
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            for mode in ALL_MODES {
                assert_eq!(
                    ui::map_key(key_event(code, modifiers), mode),
                    None,
                    "{code:?} with {modifiers:?} in {mode:?}"
                );
            }
        }
    }

    // Plain keys, including Shift-produced upper case, keep mapping everywhere.
    for mode in ALL_MODES {
        assert_eq!(
            ui::map_key(key_event(KeyCode::Char('v'), KeyModifiers::NONE), mode),
            Some(press('v'))
        );
        assert_eq!(
            ui::map_key(key_event(KeyCode::Char('V'), KeyModifiers::SHIFT), mode),
            Some(press('V'))
        );
        assert_eq!(
            ui::map_key(key_event(KeyCode::Char('∫'), KeyModifiers::NONE), mode),
            Some(press('∫'))
        );
        for (code, expected) in [
            (KeyCode::Enter, EditorInput::Enter),
            (KeyCode::Esc, EditorInput::Escape),
            (KeyCode::Backspace, EditorInput::Backspace),
            (KeyCode::Delete, EditorInput::Delete),
            (KeyCode::Left, EditorInput::Left),
            (KeyCode::Right, EditorInput::Right),
            (KeyCode::Up, EditorInput::Up),
            (KeyCode::Down, EditorInput::Down),
            (KeyCode::Tab, EditorInput::Tab),
        ] {
            assert_eq!(
                ui::map_key(key_event(code, KeyModifiers::NONE), mode),
                Some(expected),
                "{code:?} in {mode:?}"
            );
        }
        assert_eq!(
            ui::map_key(key_event(KeyCode::F(1), KeyModifiers::NONE), mode),
            None
        );
        assert_eq!(
            ui::map_key(key_event(KeyCode::Null, KeyModifiers::NONE), mode),
            None
        );
    }

    // Bracketed paste is Insert-only, so a pasted ':q!' is never executed.
    let paste = Event::Paste(":q!".to_owned());
    assert_eq!(
        ui::map_event(paste.clone(), EditorMode::Insert),
        Some(EditorInput::Paste(":q!".to_owned()))
    );
    for mode in ALL_MODES {
        if mode != EditorMode::Insert {
            assert_eq!(
                ui::map_event(paste.clone(), mode),
                None,
                "paste in {mode:?}"
            );
        }
    }

    // Key events route through the same mapping, and unrelated events are ignored.
    assert_eq!(
        ui::map_event(Event::Key(ctrl_r), EditorMode::Normal),
        Some(EditorInput::Redo)
    );
    assert_eq!(ui::map_event(Event::Key(ctrl_r), EditorMode::Insert), None);
    assert_eq!(
        ui::map_event(Event::Resize(80, 24), EditorMode::Normal),
        None
    );
    assert_eq!(ui::map_event(Event::FocusGained, EditorMode::Normal), None);
    assert_eq!(ui::map_event(Event::FocusLost, EditorMode::Insert), None);
}
