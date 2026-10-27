use std::io;
use std::time::{Duration, Instant};

use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::{Frame, Terminal};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::document::Document;
use crate::editor::{Editor, EditorInput, EditorMode, Selection, SelectionKind};

const TAB_WIDTH: usize = 4;
const WELCOME_TITLE: &str = "Postvim";
const WELCOME_QUOTE: &str = "“Writing has nothing to do with signifying. It has to do with surveying, mapping, even realms that are yet to come.”";
const WELCOME_ATTRIBUTION: &str =
    "— Gilles Deleuze and Félix Guattari, A Thousand Plateaus, translated by Brian Massumi";
const WELCOME_HINTS: [&str; 4] = [
    "i — edit",
    "Esc — normal mode",
    ":w <path> — save",
    ":q — quit",
];
/// Widest welcome text column, so the block stays readable on wide terminals.
const WELCOME_MAX_WIDTH: usize = 76;
/// Marker drawn on logical rows past the end of the buffer.
const END_OF_BUFFER_MARKER: &str = "~";
/// Background of a selected Visual span. The block cursor keeps its reverse
/// video on top, so a cursor inside a selection stays visible.
const SELECTION_BACKGROUND: Color = Color::DarkGray;

/// Draw the complete editor view into a Ratatui frame. This function is
/// terminal-independent and is also used by TestBackend render tests.
pub fn render(frame: &mut Frame<'_>, editor: &Editor) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let content_height = area.height.saturating_sub(2);
    let content = Rect::new(area.x, area.y, area.width, content_height);
    if content.height > 0 {
        // The buffer background, including end-of-buffer markers, is always
        // drawn first; the welcome screen is an overlay on top of it.
        render_document(frame, content, editor);
        if editor.show_welcome() {
            render_welcome(frame, content);
        }
    }

    if area.height >= 2 {
        let status_y = area.y + area.height - 2;
        render_status(frame, Rect::new(area.x, status_y, area.width, 1), editor);
        render_prompt(
            frame,
            Rect::new(area.x, status_y + 1, area.width, 1),
            editor,
        );
    } else {
        render_status(
            frame,
            Rect::new(area.x, area.y + area.height - 1, area.width, 1),
            editor,
        );
    }

    if let Some((x, y)) = cursor_position(area, content, editor) {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// Run the synchronous terminal event loop. Bracketed paste is forwarded only
/// while Insert mode is active; all other paste events are ignored.
pub fn run(
    terminal: &mut Terminal<impl ratatui::backend::Backend<Error = io::Error>>,
    editor: &mut Editor,
) -> io::Result<()> {
    let interval = Duration::from_millis(500);
    let mut deadline = Instant::now() + interval;
    let mut redraw = true;
    loop {
        if redraw {
            execute!(io::stdout(), cursor_style(editor.mode()))?;
            terminal.draw(|frame| render(frame, editor))?;
            redraw = false;
        }
        if editor.should_quit() {
            return Ok(());
        }

        let now = Instant::now();
        if now >= deadline {
            redraw |= editor.poll_disk(now);
            deadline = now + interval;
            continue;
        }
        if event::poll(deadline.saturating_duration_since(now))? {
            let event = event::read()?;
            if matches!(event, Event::Resize(_, _)) {
                redraw = true;
            }
            if let Some(input) = map_event(event, editor.mode()) {
                editor.handle_input(input);
                redraw = true;
            }
        }
    }
}

/// Cursor shape for the active mode: a block over document graphemes, a bar at
/// the Insert boundary, and an underline for the `:`, `/` and `?` prompts.
fn cursor_style(mode: EditorMode) -> SetCursorStyle {
    match mode {
        EditorMode::Insert => SetCursorStyle::SteadyBar,
        EditorMode::Command | EditorMode::SearchForward | EditorMode::SearchBackward => {
            SetCursorStyle::SteadyUnderScore
        }
        EditorMode::Normal | EditorMode::Visual | EditorMode::VisualLine => {
            SetCursorStyle::SteadyBlock
        }
    }
}

/// Translate one terminal event into editor input for the active mode.
///
/// Everything is dispatched through this pure helper so the terminal mapping is
/// unit-testable without a terminal: keys go through [`map_key`], and bracketed
/// paste is forwarded only while Insert mode is active.
pub fn map_event(event: Event, mode: EditorMode) -> Option<EditorInput> {
    match event {
        Event::Key(key) => map_key(key, mode),
        Event::Paste(text) if mode == EditorMode::Insert => Some(EditorInput::Paste(text)),
        _ => None,
    }
}

/// Translate one key event. Release events are ignored, Ctrl-r is redo and only
/// in Normal mode, and every other Control/Alt combination stays rejected.
pub fn map_key(key: KeyEvent, mode: EditorMode) -> Option<EditorInput> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('r' | 'R') if mode == EditorMode::Normal => Some(EditorInput::Redo),
            _ => None,
        };
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char(ch) => Some(EditorInput::Character(ch)),
        KeyCode::Enter => Some(EditorInput::Enter),
        KeyCode::Esc => Some(EditorInput::Escape),
        KeyCode::Backspace => Some(EditorInput::Backspace),
        KeyCode::Delete => Some(EditorInput::Delete),
        KeyCode::Left => Some(EditorInput::Left),
        KeyCode::Right => Some(EditorInput::Right),
        KeyCode::Up => Some(EditorInput::Up),
        KeyCode::Down => Some(EditorInput::Down),
        KeyCode::Tab => Some(EditorInput::Tab),
        _ => None,
    }
}

/// One laid-out line of the welcome block. Blank lines only add vertical space.
struct WelcomeLine {
    text: String,
    style: Style,
}

fn render_welcome(frame: &mut Frame<'_>, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // One column stays reserved for the end-of-buffer markers whenever the
    // editing area is wide enough to keep a text column beside them, so the
    // markers stay visible next to the overlay.
    let marker_column: u16 = if area.width >= 2 { 1 } else { 0 };
    let region = Rect::new(
        area.x.saturating_add(marker_column),
        area.y,
        area.width.saturating_sub(marker_column),
        area.height,
    );
    let region_width = usize::from(region.width);
    let wrap_width = WELCOME_MAX_WIDTH.min(region_width);
    if wrap_width == 0 {
        return;
    }

    let height = usize::from(area.height);
    let mut lines = welcome_block(wrap_width, false);
    if lines.len() > height {
        lines = welcome_block(wrap_width, true);
    }
    if lines.len() > height {
        // Last resort at a couple of rows: the blank separator goes first, then
        // the trailing hints, so the title never disappears.
        lines.retain(|line| !line.text.is_empty());
        lines.truncate(height);
    }

    let start = (height - lines.len()) / 2;
    for (offset, line) in lines.iter().enumerate() {
        if line.text.is_empty() {
            continue;
        }
        let slack = region_width.saturating_sub(display_width(&line.text));
        let x = region
            .x
            .saturating_add(u16::try_from(slack / 2).unwrap_or(0));
        let y = area
            .y
            .saturating_add(u16::try_from(start + offset).unwrap_or(0));
        put_text_line_at(frame, area, x, y, &line.text, line.style);
    }
}

/// Build the welcome block for one wrap width. The compact variant keeps the
/// title and the control hints and drops the quote with its attribution, so the
/// supported controls stay readable when a short terminal cannot fit everything.
fn welcome_block(wrap_width: usize, compact: bool) -> Vec<WelcomeLine> {
    let mut lines = vec![
        WelcomeLine {
            text: WELCOME_TITLE.to_owned(),
            style: Style::default().add_modifier(Modifier::BOLD),
        },
        WelcomeLine {
            text: String::new(),
            style: Style::default(),
        },
    ];
    if !compact {
        lines.extend(wrapped_block_lines(
            WELCOME_QUOTE,
            wrap_width,
            Style::default(),
        ));
        lines.extend(wrapped_block_lines(
            WELCOME_ATTRIBUTION,
            wrap_width,
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(WelcomeLine {
            text: String::new(),
            style: Style::default(),
        });
    }
    for hint in WELCOME_HINTS {
        lines.extend(wrapped_block_lines(hint, wrap_width, Style::default()));
    }
    lines
}

fn wrapped_block_lines(text: &str, wrap_width: usize, style: Style) -> Vec<WelcomeLine> {
    wrap_text(text, wrap_width)
        .into_iter()
        .map(|text| WelcomeLine { text, style })
        .collect()
}

fn render_document(frame: &mut Frame<'_>, area: Rect, editor: &Editor) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let cursor = editor.cursor();
    let block_cursor = !editor.show_welcome() && document_cursor_is_block(editor.mode());
    let selection = editor.selection();
    let top = if cursor.row >= usize::from(area.height) {
        cursor.row + 1 - usize::from(area.height)
    } else {
        0
    };
    let left = horizontal_offset(editor, usize::from(area.width));
    let right = left.saturating_add(usize::from(area.width));

    for screen_row in 0..usize::from(area.height) {
        let Some(row) = top.checked_add(screen_row) else {
            break;
        };
        let y = area.y + screen_row as u16;
        let Some(line) = editor.document().line(row) else {
            // Rows past the final logical line are not part of the buffer, so
            // they carry a marker in the leftmost content column. It stays put
            // when the viewport is scrolled sideways.
            frame.buffer_mut().set_string(
                area.x,
                y,
                END_OF_BUFFER_MARKER,
                Style::default().fg(Color::DarkGray),
            );
            continue;
        };
        // Byte-column bounds of the selected span on this row, if any. Each
        // grapheme is highlighted as a whole rendered span, so tabs, wide cells
        // and control-character replacements stay intact.
        let selected = selection.and_then(|selection| selected_columns(selection, row, line));
        let mut visual = 0usize;
        for (byte, grapheme) in line.grapheme_indices(true) {
            let display = display_grapheme(grapheme, visual);
            let width = display_width(&display);
            let end = visual.saturating_add(width);
            let is_cursor = block_cursor && row == cursor.row && byte == cursor.column;
            let is_selected = selected.is_some_and(|(low, high)| byte >= low && byte <= high);
            if visual >= left && end <= right && width > 0 {
                let x = area.x + (visual - left) as u16;
                let mut style = Style::default();
                if is_selected {
                    style = style.bg(SELECTION_BACKGROUND);
                }
                if is_cursor {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                // One styled span per grapheme: a wide grapheme's style is
                // carried by its leading cell and the terminal paints the whole
                // glyph with it, so the highlighted span stays complete.
                frame.buffer_mut().set_string(x, y, &display, style);
            }
            visual = end;
        }

        // An empty line has no grapheme to carry the block cursor, so the
        // cursor cell keeps its own single space.
        if block_cursor && row == cursor.row && line.is_empty() && left == 0 && area.width > 0 {
            let mut style = Style::default().add_modifier(Modifier::REVERSED);
            if selected.is_some() {
                style = style.bg(SELECTION_BACKGROUND);
            }
            frame.buffer_mut().set_string(area.x, y, " ", style);
        }
    }
}

/// Inclusive byte-column bounds of a selection on one logical row, or `None`
/// when the row lies outside it. `high` is the byte offset of the last selected
/// grapheme, so callers compare grapheme start offsets against both bounds.
fn selected_columns(selection: Selection, row: usize, line: &str) -> Option<(usize, usize)> {
    if row < selection.start.row || row > selection.end.row {
        return None;
    }
    match selection.kind {
        // Linewise selections own whole rows and pad them to the line end.
        SelectionKind::Linewise => Some((0, line.len())),
        SelectionKind::Characterwise => {
            let low = if row == selection.start.row {
                selection.start.column
            } else {
                0
            };
            let high = if row == selection.end.row {
                selection.end.column
            } else {
                line.len()
            };
            Some((low, high))
        }
    }
}

/// Whether this mode puts the terminal cursor on a document grapheme, which is
/// drawn as a block: Normal, Visual and Visual Line. Insert shows a bar at the
/// insertion boundary, and the prompts own their own cursor row.
fn document_cursor_is_block(mode: EditorMode) -> bool {
    match mode {
        EditorMode::Normal | EditorMode::Visual | EditorMode::VisualLine => true,
        EditorMode::Insert
        | EditorMode::Command
        | EditorMode::SearchForward
        | EditorMode::SearchBackward => false,
    }
}

fn render_status(frame: &mut Frame<'_>, area: Rect, editor: &Editor) {
    let name = editor
        .path()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "[No Name]".to_owned());
    let name = sanitize(&name);
    let marker = if editor.is_dirty() { " [+]" } else { "" };
    let cursor = editor.cursor();
    let col = visual_column(editor.document(), cursor.row, cursor.column).saturating_add(1);
    let external = editor
        .external_status()
        .map(|text| format!(" [{}]", sanitize(text)))
        .unwrap_or_default();
    let mut status = format!(
        " {}{external}  {name}{marker}  {}:{}",
        editor.mode().label(),
        cursor.row + 1,
        col
    );
    // The pending count/operator prefix is reported as its own trailing field so
    // an error or prompt message on the row below is never overwritten.
    let pending = editor.pending_text();
    if !pending.is_empty() {
        status.push(' ');
        status.push_str(&pending);
    }
    put_text_line(
        frame,
        area,
        0,
        &status,
        Style::default().fg(Color::Black).bg(Color::Gray),
    );
}

fn render_prompt(frame: &mut Frame<'_>, area: Rect, editor: &Editor) {
    let (text, style) = match prompt_layout(editor, usize::from(area.width)) {
        Some(layout) => (
            format!("{}{}", layout.prefix, layout.tail),
            Style::default().fg(Color::Yellow),
        ),
        None => (
            editor.message().map(sanitize).unwrap_or_default(),
            Style::default().fg(Color::LightRed),
        ),
    };
    put_text_line(frame, area, 0, &text, style);
}

/// The visible part of an active `:`, `/` or `?` prompt.
struct PromptLayout {
    prefix: &'static str,
    tail: String,
    /// Column, relative to the prompt row, where the prompt cursor belongs.
    cursor_column: usize,
}

/// Lay out the active prompt so its cursor stays visible. One cell is kept for
/// the prefix and one beyond the text for the cursor, and the text is cut on
/// grapheme boundaries so long Unicode prompts never split a grapheme.
fn prompt_layout(editor: &Editor, width: usize) -> Option<PromptLayout> {
    let prefix = editor.prompt_prefix()?;
    if width == 0 {
        return None;
    }
    let prefix_width = display_width(prefix);
    let available = width.saturating_sub(prefix_width).saturating_sub(1);
    let tail = clipped_tail(editor.prompt_text(), available);
    let cursor_column = prefix_width
        .saturating_add(display_width(&tail))
        .min(width.saturating_sub(1));
    Some(PromptLayout {
        prefix,
        tail,
        cursor_column,
    })
}

/// The trailing part of `text` that fits in `available` cells, cut on grapheme
/// boundaries. Control characters count with their rendered replacement width.
fn clipped_tail(text: &str, available: usize) -> String {
    if available == 0 {
        return String::new();
    }
    let mut used = 0usize;
    let mut start = text.len();
    for (index, grapheme) in text.grapheme_indices(true).rev() {
        let width = display_width(&sanitize(grapheme));
        if used.saturating_add(width) > available {
            break;
        }
        used = used.saturating_add(width);
        start = index;
    }
    text[start..].to_owned()
}

fn cursor_position(area: Rect, content: Rect, editor: &Editor) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if editor.mode().is_prompt() {
        // The prompt row only exists when a status row fits above it.
        if area.height < 2 {
            return None;
        }
        let layout = prompt_layout(editor, usize::from(area.width))?;
        let x = area.x + u16::try_from(layout.cursor_column).ok()?;
        return Some((x, area.y + area.height - 1));
    }
    if editor.show_welcome() || content.width == 0 || content.height == 0 {
        return None;
    }

    let cursor = editor.cursor();
    let top = if cursor.row >= usize::from(content.height) {
        cursor.row + 1 - usize::from(content.height)
    } else {
        0
    };
    let screen_row = cursor.row.checked_sub(top)?;
    if screen_row >= usize::from(content.height) {
        return None;
    }
    let cursor_visual = visual_column(editor.document(), cursor.row, cursor.column);
    let left = horizontal_offset(editor, usize::from(content.width));
    let x = cursor_visual.checked_sub(left)?;
    if x >= usize::from(content.width) {
        return None;
    }
    if document_cursor_is_block(editor.mode()) {
        let line = editor.document().line(cursor.row)?;
        if let Some((_, grapheme)) = line
            .grapheme_indices(true)
            .find(|(byte, _)| *byte == cursor.column)
        {
            let width = display_width(&display_grapheme(grapheme, cursor_visual));
            if width > usize::from(content.width).saturating_sub(x) {
                return None;
            }
        }
    }
    Some((content.x + x as u16, content.y + screen_row as u16))
}

fn horizontal_offset(editor: &Editor, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let cursor = editor.cursor();
    let cursor_visual = visual_column(editor.document(), cursor.row, cursor.column);
    let cursor_width = if document_cursor_is_block(editor.mode()) {
        editor
            .document()
            .line(cursor.row)
            .and_then(|line| {
                line.grapheme_indices(true)
                    .find(|(byte, _)| *byte == cursor.column)
            })
            .map(|(_, grapheme)| display_width(&display_grapheme(grapheme, cursor_visual)))
            .unwrap_or(1)
            .max(1)
            .min(width)
    } else {
        1
    };
    let target = cursor_visual.saturating_sub(width.saturating_sub(cursor_width));
    if target == 0 {
        return 0;
    }
    let Some(line) = editor.document().line(cursor.row) else {
        return target;
    };
    let mut visual = 0usize;
    for grapheme in line.graphemes(true) {
        if visual >= target {
            return visual;
        }
        visual = visual.saturating_add(display_width(&display_grapheme(grapheme, visual)));
    }
    visual.min(target.max(visual))
}

fn visual_column(document: &Document, row: usize, byte_column: usize) -> usize {
    let Some(line) = document.line(row) else {
        return 0;
    };
    let mut visual = 0usize;
    for (byte, grapheme) in line.grapheme_indices(true) {
        if byte >= byte_column {
            break;
        }
        visual = visual.saturating_add(display_width(&display_grapheme(grapheme, visual)));
    }
    visual
}

fn display_grapheme(grapheme: &str, visual_column: usize) -> String {
    if grapheme == "\t" {
        return " ".repeat(TAB_WIDTH - visual_column % TAB_WIDTH);
    }
    Document::display_grapheme(grapheme, visual_column)
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn put_text_line(frame: &mut Frame<'_>, area: Rect, row: u16, text: &str, style: Style) {
    put_text_line_at(frame, area, area.x, row, text, style);
}

fn put_text_line_at(frame: &mut Frame<'_>, area: Rect, x: u16, row: u16, text: &str, style: Style) {
    if row >= area.height || area.width == 0 || x >= area.x.saturating_add(area.width) {
        return;
    }
    let available = usize::from(area.x.saturating_add(area.width) - x);
    let mut col = 0usize;
    for grapheme in text.graphemes(true) {
        let visible = sanitize(grapheme);
        let width = display_width(&visible);
        if width > 0 {
            if col.saturating_add(width) > available {
                break;
            }
            frame
                .buffer_mut()
                .set_string(x + col as u16, area.y + row, &visible, style);
            col += width;
        }
    }
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = display_width(word);
        let separator = usize::from(!current.is_empty());
        if !current.is_empty()
            && current_width
                .saturating_add(separator)
                .saturating_add(word_width)
                > max_width
        {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if word_width <= max_width {
            if !current.is_empty() {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
        } else {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            for grapheme in word.graphemes(true) {
                let width = display_width(grapheme);
                if current_width > 0 && current_width.saturating_add(width) > max_width {
                    lines.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                if width <= max_width {
                    current.push_str(grapheme);
                    current_width += width;
                }
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn sanitize(text: &str) -> String {
    let mut clean = String::new();
    for ch in text.chars() {
        let value = ch as u32;
        if value < 0x20 {
            clean.push('^');
            clean.push(char::from_u32(value + 0x40).unwrap_or('?'));
        } else if value == 0x7f {
            clean.push_str("^?");
        } else if ch.is_control() {
            clean.push('�');
        } else {
            clean.push(ch);
        }
    }
    clean
}
