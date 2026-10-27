use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;

use crate::document::{CapturedLine, Cursor, Document, Register};
use crate::keymap::{self, InsertEntry, Operator, Parser, ReplaceTarget, Step};
use crate::motion::{self, FindKind, MotionRange, MotionSpec};
use crate::storage::{self, DiskContents, Observation, SaveError};
use crate::transform::{
    ByteRange, FormatterSpec, TransformError, TransformRequest, TransformSource,
};

/// How long two equivalent disk samples must be apart before a change is shown
/// or applied. This is a heuristic that hides ordinary truncate-then-write
/// gaps; it is not a proof that a foreign writer has finished.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Persistent status while a foreign edit and local text disagree.
const CONFLICT_STATUS: &str = "Disk changed externally; :reload! discards local edits";

/// Persistent status while a clean buffer waits for an idle Normal boundary.
const PENDING_STATUS: &str = "External change on disk; waiting for idle to reload";

/// Interactive mode. Rendering and cursor style follow this value; the editor
/// owns exactly one of these at a time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorMode {
    Normal,
    Insert,
    /// `:` command prompt.
    Command,
    /// Characterwise Visual selection.
    Visual,
    /// Linewise Visual selection.
    VisualLine,
    /// `/` search prompt.
    SearchForward,
    /// `?` search prompt.
    SearchBackward,
}

impl EditorMode {
    pub fn is_visual(self) -> bool {
        matches!(self, Self::Visual | Self::VisualLine)
    }

    pub fn is_prompt(self) -> bool {
        matches!(
            self,
            Self::Command | Self::SearchForward | Self::SearchBackward
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Command => "COMMAND",
            Self::Visual => "VISUAL",
            Self::VisualLine => "VISUAL LINE",
            Self::SearchForward => "/",
            Self::SearchBackward => "?",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorInput {
    Character(char),
    Text(String),
    Paste(String),
    Enter,
    Escape,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Tab,
    /// Ctrl-r: redo, honoured only in Normal mode.
    Redo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionKind {
    Characterwise,
    Linewise,
}

/// Normalized selection exposed to rendering and operators.
///
/// `start <= end` and both endpoints are inclusive Normal-style grapheme
/// positions. A linewise selection starts at column zero and ends on the last
/// grapheme of its final row, so highlighting `start.row..=end.row` is exact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub start: Cursor,
    pub end: Cursor,
    pub kind: SelectionKind,
}

#[derive(Clone, Debug)]
struct Snapshot {
    document: Document,
    cursor: Cursor,
}

#[derive(Clone, Copy, Debug)]
struct VisualState {
    anchor: Cursor,
    kind: SelectionKind,
}

#[derive(Clone, Debug)]
struct SearchMemory {
    query: String,
    forward: bool,
}

#[derive(Clone, Copy, Debug)]
struct FindMemory {
    kind: FindKind,
    target: char,
}

#[derive(Clone, Copy, Debug)]
struct PromptReturn {
    mode: EditorMode,
    visual: Option<VisualState>,
    cursor: Cursor,
}

#[derive(Clone, Copy, Debug)]
struct SearchPrompt {
    forward: bool,
    count: usize,
    restore: PromptReturn,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InsertAction {
    Text(String),
    Enter,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
}

/// How a completed Insert session is repeated by `.`.
#[derive(Clone, Debug)]
enum InsertDot {
    Keys {
        keys: Vec<char>,
        /// `Some` only when a count was actually typed, so `1` survives replay.
        count: Option<usize>,
    },
    Visual {
        op: Operator,
        shape: VisualShape,
    },
}

#[derive(Clone, Debug)]
struct InsertSession {
    before: Option<Snapshot>,
    entry: InsertEntry,
    count: usize,
    actions: Vec<InsertAction>,
    dot: InsertDot,
}

/// One sample of the active file, reduced to the facts reconciliation needs.
///
/// Samples are compared only with other samples, so equal-looking but distinct
/// reads of the same state confirm each other.
#[derive(Clone, Debug)]
enum DiskProbe {
    /// A readable destination holding UTF-8-compatible bytes, or nothing at all.
    Readable(Observation),
    /// A readable destination whose bytes are not valid UTF-8.
    NotUtf8(Observation),
    /// The destination could not be inspected.
    Failed(io::ErrorKind),
}

impl DiskProbe {
    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Readable(this), Self::Readable(that))
            | (Self::NotUtf8(this), Self::NotUtf8(that)) => {
                this.destination == that.destination && this.contents == that.contents
            }
            (Self::Failed(this), Self::Failed(that)) => this == that,
            _ => false,
        }
    }
}

/// Position in time of the first of two equivalent disk samples.
#[derive(Clone, Debug)]
struct StableSample {
    probe: DiskProbe,
    at: Instant,
}

/// A normalized, position-independent Visual selection shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisualShape {
    /// Linewise selection of N logical lines.
    Lines(usize),
    /// Same-line characterwise selection of N graphemes.
    Chars(usize),
    /// Multiline characterwise selection: row span plus the final endpoint's
    /// zero-based grapheme ordinal in its line.
    Rows { rows: usize, last_ordinal: usize },
}

#[derive(Clone, Debug)]
enum DotRecipe {
    Keys {
        keys: Vec<char>,
        /// `Some` only when a count was actually typed. `1dG` and `dG` differ, so
        /// an explicit one must be replayed as a count.
        count: Option<usize>,
        insert: Option<Vec<InsertAction>>,
    },
    Visual {
        op: Operator,
        shape: VisualShape,
        insert: Option<Vec<InsertAction>>,
    },
    VisualPut {
        shape: VisualShape,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisualCommand {
    Delete,
    Change,
    Yank,
    Put {
        before: bool,
    },
    Toggle(SelectionKind),
    Search {
        forward: bool,
    },
    SearchNext {
        reverse: bool,
    },
    /// `:` from a Visual selection; the prompt keeps the selection for range
    /// commands such as `:format`.
    CommandPrompt,
}

fn visual_command(ch: char) -> Option<VisualCommand> {
    match ch {
        'd' | 'x' => Some(VisualCommand::Delete),
        'c' => Some(VisualCommand::Change),
        'y' => Some(VisualCommand::Yank),
        'p' => Some(VisualCommand::Put { before: false }),
        'P' => Some(VisualCommand::Put { before: true }),
        'v' => Some(VisualCommand::Toggle(SelectionKind::Characterwise)),
        'V' => Some(VisualCommand::Toggle(SelectionKind::Linewise)),
        '/' => Some(VisualCommand::Search { forward: true }),
        '?' => Some(VisualCommand::Search { forward: false }),
        'n' => Some(VisualCommand::SearchNext { reverse: false }),
        'N' => Some(VisualCommand::SearchNext { reverse: true }),
        ':' => Some(VisualCommand::CommandPrompt),
        _ => None,
    }
}

/// Modal editor state: document, cursor, pending keys, history, repeat recipe,
/// unnamed register, search memory and save baseline.
#[derive(Clone, Debug)]
pub struct Editor {
    document: Document,
    path: Option<PathBuf>,
    cursor: Cursor,
    mode: EditorMode,
    parser: Parser,
    visual: Option<VisualState>,
    prompt: String,
    search_prompt: Option<SearchPrompt>,
    command_return: Option<PromptReturn>,
    message: Option<String>,
    saved: String,
    /// Expected disk state for the active path: the bytes and resolved
    /// destination captured when the file was opened, last written
    /// successfully, or last reloaded.
    ///
    /// Kept separate from `saved` on purpose: a foreign edit must never advance
    /// this baseline, or it would authorize the very overwrite it warns about.
    /// `Some` exactly when `path` is `Some`.
    disk: Option<Observation>,
    /// Persistent external-change status for the status line. Unlike `message`
    /// it survives input, so a conflict stays visible while the user types.
    external: Option<String>,
    /// When the most recent disk sample was taken.
    last_sample: Option<Instant>,
    /// First of two equivalent samples awaiting confirmation.
    stable_candidate: Option<StableSample>,
    should_quit: bool,
    preferred_visual_column: Option<usize>,
    welcome: bool,
    register: Register,
    find_memory: Option<FindMemory>,
    search_memory: Option<SearchMemory>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    transaction_open: bool,
    insert: Option<InsertSession>,
    dot: Option<DotRecipe>,
    replaying: bool,
}

impl Editor {
    pub fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let Some(path) = path else {
            let document = Document::new();
            let saved = document.serialize();
            return Ok(Self::from_state(document, None, saved));
        };
        // One logical read supplies both the text and the expectation, so the
        // baseline can never describe a moment the text did not come from.
        let observation = storage::observe(&path)?;
        let content = match &observation.contents {
            DiskContents::Missing => String::new(),
            DiskContents::Present(bytes) => String::from_utf8(bytes.clone())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        };
        let document = Document::from_text(&content);
        let saved = document.serialize();
        let mut editor = Self::from_state(document, Some(path), saved);
        editor.disk = Some(observation);
        Ok(editor)
    }

    pub fn from_document(document: Document) -> Self {
        let saved = document.serialize();
        Self::from_state(document, None, saved)
    }

    fn from_state(document: Document, path: Option<PathBuf>, saved: String) -> Self {
        Self {
            document,
            path,
            cursor: Cursor::default(),
            mode: EditorMode::Normal,
            parser: Parser::new(),
            visual: None,
            prompt: String::new(),
            search_prompt: None,
            command_return: None,
            message: None,
            saved,
            disk: None,
            external: None,
            last_sample: None,
            stable_candidate: None,
            should_quit: false,
            preferred_visual_column: None,
            welcome: true,
            register: Register::Empty,
            find_memory: None,
            search_memory: None,
            undo: Vec::new(),
            redo: Vec::new(),
            transaction_open: false,
            insert: None,
            dot: None,
            replaying: false,
        }
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn mode(&self) -> EditorMode {
        self.mode
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Persistent external-change status for the active file.
    ///
    /// Unlike [`Editor::message`], which input clears, this stays visible while
    /// the file and the buffer disagree, and while a clean buffer waits for an
    /// idle boundary. `None` means the last observation matched the baseline.
    pub fn external_status(&self) -> Option<&str> {
        self.external.as_deref()
    }

    /// When the next [`Editor::poll_disk`] call can do work, if the editor polls
    /// at all. Advisory: the event loop may call `poll_disk` earlier, which
    /// simply reports no change.
    pub fn next_poll_at(&self) -> Option<Instant> {
        if self.path.is_none() || self.disk.is_none() || self.should_quit {
            return None;
        }
        Some(match self.last_sample {
            Some(last) => last + POLL_INTERVAL,
            None => Instant::now(),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn register(&self) -> &Register {
        &self.register
    }

    /// Places the cursor at a validated Normal-mode position. Intended for tests
    /// and embedding; the position is clamped onto a grapheme boundary.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = cursor;
        self.clamp_cursor_normal();
        self.preferred_visual_column = None;
    }

    /// Dirty state is derived from bytes: it is true exactly when the serialized
    /// document differs from the baseline captured at open or at the last
    /// successful write.
    pub fn is_dirty(&self) -> bool {
        self.document.serialize() != self.saved
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn show_welcome(&self) -> bool {
        self.welcome
            && self.path.is_none()
            && self.document.line_count() == 1
            && self.document.line(0) == Some("")
    }

    /// The normalized selection, available only while a Visual mode is active.
    pub fn selection(&self) -> Option<Selection> {
        let state = self.visual?;
        if !self.mode.is_visual() {
            return None;
        }
        let (start, end) = if self.cursor <= state.anchor {
            (self.cursor, state.anchor)
        } else {
            (state.anchor, self.cursor)
        };
        Some(normalize_selection(start, end, state.kind, &self.document))
    }

    /// `:` for the command prompt, `/` or `?` for search prompts.
    pub fn prompt_prefix(&self) -> Option<&'static str> {
        match self.mode {
            EditorMode::Command => Some(":"),
            EditorMode::SearchForward => Some("/"),
            EditorMode::SearchBackward => Some("?"),
            _ => None,
        }
    }

    /// Prompt-owned text for the active `:`/`/`/`?` prompt.
    pub fn prompt_text(&self) -> &str {
        &self.prompt
    }

    /// Compact pending key prefix such as `2d`, `df`, or an empty string.
    pub fn pending_text(&self) -> String {
        self.parser.display()
    }

    pub fn handle_input(&mut self, input: EditorInput) {
        if self.should_quit {
            return;
        }
        self.message = None;
        match self.mode {
            EditorMode::Normal => self.handle_normal(input),
            EditorMode::Insert => self.handle_insert(input),
            EditorMode::Command | EditorMode::SearchForward | EditorMode::SearchBackward => {
                self.handle_prompt(input)
            }
            EditorMode::Visual | EditorMode::VisualLine => self.handle_visual(input),
        }
    }

    // ---------------------------------------------------------------- Normal

    fn handle_normal(&mut self, input: EditorInput) {
        match input {
            EditorInput::Redo => {
                // A pending count applies to redo; any other pending prefix is an
                // unsupported continuation and is consumed without redoing.
                let count = self.parser.take_count();
                let other_prefix = !self.parser.is_idle();
                self.parser.reset();
                if !other_prefix {
                    self.redo_steps(count.unwrap_or(1));
                }
            }
            EditorInput::Escape => {
                self.parser.reset();
                self.preferred_visual_column = None;
            }
            EditorInput::Character(ch) => {
                let step = self.parser.push(ch);
                self.normal_step(step);
            }
            EditorInput::Text(text) => {
                for ch in text.chars() {
                    if self.should_quit || self.mode != EditorMode::Normal {
                        break;
                    }
                    let step = self.parser.push(ch);
                    self.normal_step(step);
                }
            }
            EditorInput::Enter => {
                let step = self.parser.push_enter();
                self.normal_step(step);
            }
            EditorInput::Tab => {
                let step = self.parser.push_tab();
                self.normal_step(step);
            }
            EditorInput::Left => self.arrow_motion(MotionSpec::Left),
            EditorInput::Right => self.arrow_motion(MotionSpec::Right),
            EditorInput::Up => self.arrow_motion(MotionSpec::Up),
            EditorInput::Down => self.arrow_motion(MotionSpec::Down),
            EditorInput::Backspace | EditorInput::Delete | EditorInput::Paste(_) => {
                self.parser.reset();
            }
        }
    }

    fn arrow_motion(&mut self, spec: MotionSpec) {
        // A pending count applies to the arrow. Any other pending prefix (an
        // operator, `g`, an awaited find or replace) is an unsupported
        // continuation: consume it and ignore the arrow, never executing it.
        let count = self.parser.take_count();
        if count.is_none() && !self.parser.is_idle() {
            self.parser.reset();
            return;
        }
        self.parser.reset();
        self.move_cursor(spec, count);
    }

    fn normal_step(&mut self, step: Step) {
        match step {
            Step::Waiting | Step::Cancel => {}
            Step::Command { key, count } => self.normal_command(key, count),
            Step::Motion { spec, count } => self.move_cursor(spec, count),
            Step::Operator { op, spec, count } => self.operator_motion(op, spec, count),
            Step::OperatorLinewise { op, count } => {
                let keys = vec![op.key(), op.key()];
                self.operator_lines(op, count, keys);
            }
            Step::OperatorFind {
                op,
                kind,
                target,
                count,
            } => self.operator_find(op, kind, target, count),
            Step::Find {
                kind,
                target,
                count,
            } => self.find_move(kind, target, count, false),
            Step::Replace { target, count } => self.replace_chars(target, count),
        }
    }

    fn normal_command(&mut self, key: char, count: Option<usize>) {
        match key {
            'i' | 'I' | 'a' | 'A' | 'o' | 'O' => {
                let entry = InsertEntry::from_key(key).expect("insert entry key");
                self.start_insert_entry(entry, count);
            }
            'x' => self.delete_chars(count, true),
            'X' => self.delete_chars(count, false),
            'D' => {
                let keys = vec!['D'];
                self.operator_with_keys(
                    Operator::Delete,
                    MotionSpec::LineEnd(count.unwrap_or(1)),
                    count,
                    keys,
                );
            }
            'C' => {
                let keys = vec!['C'];
                self.operator_with_keys(
                    Operator::Change,
                    MotionSpec::LineEnd(count.unwrap_or(1)),
                    count,
                    keys,
                );
            }
            's' => {
                let range =
                    motion::grapheme_range(self.document(), self.cursor, count.unwrap_or(1));
                self.apply_operator(Operator::Change, range, vec!['s'], count);
            }
            'S' => self.operator_lines(Operator::Change, count.unwrap_or(1), vec!['S']),
            'p' => self.put(count, false),
            'P' => self.put(count, true),
            'u' => self.undo_steps(count.unwrap_or(1)),
            'v' => self.enter_visual(SelectionKind::Characterwise, count),
            'V' => self.enter_visual(SelectionKind::Linewise, count),
            '/' => self.open_search(true, count),
            '?' => self.open_search(false, count),
            'n' => self.repeat_search(false, count),
            'N' => self.repeat_search(true, count),
            ':' => self.open_command(),
            '.' => self.repeat_dot(count),
            _ => {}
        }
    }

    fn move_cursor(&mut self, spec: MotionSpec, count: Option<usize>) {
        let count = count.unwrap_or(1);
        let Some(spec) = self.expand_motion(spec) else {
            return;
        };
        if matches!(spec, MotionSpec::Down | MotionSpec::Up) {
            let delta = if spec == MotionSpec::Down { 1 } else { -1 };
            self.move_vertical(delta * count as isize);
            return;
        }
        let Some(outcome) = motion::resolve(self.document(), self.cursor, spec, count) else {
            self.report_motion_failure(spec);
            return;
        };
        self.cursor = outcome.target;
        self.clamp_cursor_normal();
        self.preferred_visual_column = None;
    }

    fn expand_motion(&mut self, spec: MotionSpec) -> Option<MotionSpec> {
        match spec {
            MotionSpec::RepeatFind { reverse } => {
                let Some(memory) = self.find_memory else {
                    self.message = Some("No previous f/F/t/T".to_owned());
                    return None;
                };
                let kind = if reverse {
                    reverse_find(memory.kind)
                } else {
                    memory.kind
                };
                Some(MotionSpec::Find {
                    kind,
                    target: memory.target,
                    repeat: true,
                })
            }
            other => Some(other),
        }
    }

    fn report_motion_failure(&mut self, spec: MotionSpec) {
        if let MotionSpec::Find { .. } = spec {
            self.message = Some("Character not found".to_owned());
        }
    }

    fn find_move(&mut self, kind: FindKind, target: char, count: usize, repeat: bool) {
        let spec = MotionSpec::Find {
            kind,
            target,
            repeat,
        };
        let Some(outcome) = motion::resolve(self.document(), self.cursor, spec, count) else {
            self.message = Some("Character not found".to_owned());
            return;
        };
        self.find_memory = Some(FindMemory { kind, target });
        self.cursor = outcome.target;
        self.clamp_cursor_normal();
        self.preferred_visual_column = None;
    }

    fn move_vertical(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        let target_row = self.cursor.row as isize + delta;
        if target_row < 0 || target_row >= self.document.line_count() as isize {
            return;
        }
        let visual = *self.preferred_visual_column.get_or_insert_with(|| {
            self.document
                .visual_column(self.cursor.row, self.cursor.column)
        });
        self.cursor.row = target_row as usize;
        let byte = self.document.byte_at_visual_column(self.cursor.row, visual);
        self.cursor.column = if self.mode == EditorMode::Insert {
            self.document.clamp_boundary(self.cursor.row, byte)
        } else {
            self.document.normal_cursor_col(self.cursor.row, byte)
        };
    }

    // ------------------------------------------------------------- operators

    fn operator_motion(&mut self, op: Operator, spec: MotionSpec, count: Option<usize>) {
        let keys = {
            let mut keys = vec![op.key()];
            keys.extend(spec.keys());
            keys
        };
        self.operator_with_keys(op, spec, count, keys);
    }

    fn operator_with_keys(
        &mut self,
        op: Operator,
        spec: MotionSpec,
        count: Option<usize>,
        keys: Vec<char>,
    ) {
        let explicit_find = matches!(spec, MotionSpec::Find { .. });
        let Some(spec) = self.expand_motion(spec) else {
            return;
        };
        let effective = count.unwrap_or(1);
        let spec = match spec {
            MotionSpec::LineEnd(_) => MotionSpec::LineEnd(effective),
            MotionSpec::GotoLastLine(None) if count.is_some() => MotionSpec::GotoLastLine(count),
            other => other,
        };
        let spec = if op == Operator::Change {
            self.change_word_exception(spec)
        } else {
            spec
        };
        let Some(outcome) = motion::resolve(self.document(), self.cursor, spec, effective) else {
            self.report_motion_failure(spec);
            return;
        };
        // An explicitly typed `f`/`F`/`t`/`T` updates the remembered find on
        // success; a failed find and a repeated `;`/`,` leave it unchanged so the
        // recorded direction survives.
        if explicit_find && let MotionSpec::Find { kind, target, .. } = spec {
            self.find_memory = Some(FindMemory { kind, target });
        }
        let range = motion::range_for(self.document(), self.cursor, outcome);
        self.apply_operator(op, range, keys, count);
    }

    fn operator_find(&mut self, op: Operator, kind: FindKind, target: char, count: usize) {
        let keys = vec![op.key(), kind.key(), target];
        self.operator_with_keys(
            op,
            MotionSpec::Find {
                kind,
                target,
                repeat: false,
            },
            Some(count),
            keys,
        );
    }

    fn operator_lines(&mut self, op: Operator, count: usize, keys: Vec<char>) {
        let last = self
            .cursor
            .row
            .saturating_add(count.max(1) - 1)
            .min(self.document.line_count().saturating_sub(1));
        let range = MotionRange::Line {
            first: self.cursor.row,
            last,
        };
        self.apply_operator(op, range, keys, Some(count.max(1)));
    }

    /// `cw`/`cW` on a non-blank stop at the end of the word under the cursor
    /// without consuming following blanks; on whitespace they use `w`/`W`.
    fn change_word_exception(&self, spec: MotionSpec) -> MotionSpec {
        let target = match spec {
            MotionSpec::WordForward => MotionSpec::ChangeWordEnd,
            MotionSpec::BigWordForward => MotionSpec::ChangeBigWordEnd,
            other => return other,
        };
        let on_blank = self
            .document
            .line(self.cursor.row)
            .and_then(|line| {
                line.grapheme_indices(true)
                    .find(|(index, _)| *index >= self.cursor.column)
                    .map(|(_, grapheme)| grapheme)
            })
            .is_none_or(|grapheme| grapheme.chars().next().is_none_or(char::is_whitespace));
        if on_blank { spec } else { target }
    }

    fn apply_operator(
        &mut self,
        op: Operator,
        range: MotionRange,
        keys: Vec<char>,
        count: Option<usize>,
    ) {
        match op {
            Operator::Yank => {
                if range.is_empty() {
                    return;
                }
                self.yank_range(range);
                self.cursor = self.yank_cursor(range);
                self.clamp_cursor_normal();
                self.preferred_visual_column = None;
            }
            Operator::Delete => {
                if range.is_empty() {
                    return;
                }
                let before = self.begin_change();
                self.yank_range(range);
                self.delete_range(range);
                self.commit_change(
                    before,
                    Some(DotRecipe::Keys {
                        keys,
                        count,
                        insert: None,
                    }),
                );
                self.preferred_visual_column = None;
            }
            Operator::Change => {
                let before = self.begin_change();
                self.yank_range(range);
                let anchor = self.change_range(range);
                self.cursor = anchor;
                self.begin_insert_session(
                    before,
                    InsertEntry::Change,
                    1,
                    InsertDot::Keys { keys, count },
                );
            }
        }
    }

    fn yank_range(&mut self, range: MotionRange) {
        self.register = match range {
            MotionRange::Char { start, end } => {
                Register::Characterwise(self.document.text_between(start, end))
            }
            MotionRange::Line { first, last } => {
                Register::Linewise(self.document.captured_lines(first, last))
            }
        };
    }

    fn yank_cursor(&self, range: MotionRange) -> Cursor {
        match range {
            MotionRange::Char { start, .. } => start,
            MotionRange::Line { first, .. } => Cursor::new(first, self.cursor.column),
        }
    }

    fn delete_range(&mut self, range: MotionRange) {
        match range {
            MotionRange::Char { start, end } => {
                if start == end {
                    return;
                }
                self.document.replace_text_between(start, end, "");
                self.cursor = start;
            }
            MotionRange::Line { first, last } => {
                let row = self.document.replace_line_range(first, last, &[]);
                self.cursor = Cursor::new(row, self.document.first_nonblank_col(row));
            }
        }
        self.clamp_cursor_normal();
    }

    fn change_range(&mut self, range: MotionRange) -> Cursor {
        match range {
            MotionRange::Char { start, end } => {
                if start != end {
                    self.document.replace_text_between(start, end, "");
                }
                Cursor::new(start.row, start.column)
            }
            MotionRange::Line { first, last } => {
                let payload = [CapturedLine::plain("")];
                let row = self.document.replace_line_range(first, last, &payload);
                Cursor::new(row, 0)
            }
        }
    }

    fn delete_chars(&mut self, count: Option<usize>, forward: bool) {
        let amount = count.unwrap_or(1).max(1);
        let range = if forward {
            motion::grapheme_range(self.document(), self.cursor, amount)
        } else {
            let mut start = self.cursor.column;
            for _ in 0..amount {
                match self.document.previous_grapheme(self.cursor.row, start) {
                    Some(range) => start = range.start,
                    None => break,
                }
            }
            MotionRange::Char {
                start: Cursor::new(self.cursor.row, start),
                end: self.cursor,
            }
        };
        if range.is_empty() {
            return;
        }
        let before = self.begin_change();
        self.yank_range(range);
        self.delete_range(range);
        self.commit_change(
            before,
            Some(DotRecipe::Keys {
                keys: vec![if forward { 'x' } else { 'X' }],
                count,
                insert: None,
            }),
        );
        self.preferred_visual_column = None;
    }

    fn replace_chars(&mut self, target: ReplaceTarget, count: Option<usize>) {
        let amount = count.unwrap_or(1).max(1);
        let available = motion::graphemes_remaining(self.document(), self.cursor);
        if available < amount {
            self.message = Some("Not enough characters".to_owned());
            return;
        }
        let MotionRange::Char { start, end } =
            motion::grapheme_range(self.document(), self.cursor, amount)
        else {
            return;
        };
        let replacement = match target {
            ReplaceTarget::Char(ch) => ch.to_string().repeat(amount),
            ReplaceTarget::Tab => "\t".repeat(amount),
            ReplaceTarget::LineBreak => self.document.insertion_line_ending().to_owned(),
        };
        let before = self.begin_change();
        let end_position = self.document.replace_text_between(start, end, &replacement);
        self.cursor = match target {
            ReplaceTarget::LineBreak => Cursor::new(end_position.row, 0),
            _ => start,
        };
        self.clamp_cursor_normal();
        self.preferred_visual_column = None;
        let key = match target {
            ReplaceTarget::Char(ch) => ch,
            ReplaceTarget::Tab => '\t',
            ReplaceTarget::LineBreak => '\n',
        };
        self.commit_change(
            before,
            Some(DotRecipe::Keys {
                keys: vec!['r', key],
                count,
                insert: None,
            }),
        );
    }

    // ------------------------------------------------------------------- put

    fn put(&mut self, count: Option<usize>, before: bool) {
        let amount = count.unwrap_or(1).max(1);
        let payload = self.register.clone();
        if payload.is_empty() {
            self.message = Some("Nothing in register".to_owned());
            return;
        }
        let snapshot = self.begin_change();
        let cursor = match &payload {
            Register::Characterwise(text) => {
                let text = text.repeat(amount);
                let anchor = if before {
                    self.cursor
                } else {
                    self.after_cursor_grapheme()
                };
                let end = self.document.replace_text_between(anchor, anchor, &text);
                if text.contains(['\n', '\r']) {
                    anchor
                } else {
                    Cursor::new(
                        end.row,
                        self.document
                            .grapheme_start(end.row, end.column.saturating_sub(1)),
                    )
                }
            }
            Register::Linewise(lines) => {
                let mut payload_lines: Vec<CapturedLine> = Vec::new();
                for _ in 0..amount {
                    payload_lines.extend(lines.iter().cloned());
                }
                let row = if before {
                    self.document
                        .insert_lines_before(self.cursor.row, &payload_lines)
                } else {
                    self.document
                        .insert_lines_after(self.cursor.row, &payload_lines)
                };
                Cursor::new(row, self.document.first_nonblank_col(row))
            }
            Register::Empty => self.cursor,
        };
        self.cursor = cursor;
        self.clamp_cursor_normal();
        self.preferred_visual_column = None;
        self.commit_change(
            snapshot,
            Some(DotRecipe::Keys {
                keys: vec![if before { 'P' } else { 'p' }],
                count,
                insert: None,
            }),
        );
    }

    fn after_cursor_grapheme(&self) -> Cursor {
        let row = self.cursor.row;
        if self.document.grapheme_count(row) == 0 {
            return Cursor::new(row, 0);
        }
        Cursor::new(row, self.document.grapheme_end(row, self.cursor.column))
    }

    // ---------------------------------------------------------------- insert

    fn start_insert_entry(&mut self, entry: InsertEntry, count: Option<usize>) {
        let amount = count.unwrap_or(1).max(1);
        let snapshot = self.begin_change();
        self.enter_insert_position(entry);
        self.begin_insert_session(
            snapshot,
            entry,
            amount,
            InsertDot::Keys {
                keys: vec![entry.key()],
                count,
            },
        );
    }

    fn begin_insert_session(
        &mut self,
        before: Option<Snapshot>,
        entry: InsertEntry,
        count: usize,
        dot: InsertDot,
    ) {
        self.insert = Some(InsertSession {
            before,
            entry,
            count,
            actions: Vec::new(),
            dot,
        });
        self.mode = EditorMode::Insert;
        self.parser.reset();
        self.visual = None;
        self.preferred_visual_column = None;
        self.welcome = false;
    }

    fn enter_insert_position(&mut self, entry: InsertEntry) {
        match entry {
            InsertEntry::Before => {
                self.cursor.column = self
                    .document
                    .clamp_boundary(self.cursor.row, self.cursor.column);
            }
            InsertEntry::FirstNonBlank => {
                self.cursor.column = self.document.first_nonblank_col(self.cursor.row);
            }
            InsertEntry::After => {
                self.cursor.column = if self.document.grapheme_count(self.cursor.row) == 0 {
                    0
                } else {
                    self.document
                        .grapheme_end(self.cursor.row, self.cursor.column)
                };
            }
            InsertEntry::LineEnd => {
                self.cursor.column = self.document.line_len_bytes(self.cursor.row);
            }
            InsertEntry::OpenBelow | InsertEntry::OpenAbove => self.open_line(entry),
            InsertEntry::Change => {}
        }
    }

    fn open_line(&mut self, entry: InsertEntry) {
        match entry {
            InsertEntry::OpenBelow => {
                let row = self.cursor.row;
                let mut target_row = row;
                let mut target_column = self.document.line_len_bytes(row);
                self.document
                    .insert_newline(&mut target_row, &mut target_column);
                self.cursor = Cursor::new(target_row, target_column);
            }
            InsertEntry::OpenAbove => {
                let row = self.cursor.row;
                let mut target_row = row;
                let mut target_column = 0;
                self.document
                    .insert_newline(&mut target_row, &mut target_column);
                self.cursor = Cursor::new(row, 0);
            }
            _ => {}
        }
    }

    fn handle_insert(&mut self, input: EditorInput) {
        match input {
            EditorInput::Escape => self.finish_insert(),
            EditorInput::Redo => {}
            EditorInput::Text(text) | EditorInput::Paste(text) => {
                self.apply_insert_action(&InsertAction::Text(text), true);
            }
            EditorInput::Character(ch) => {
                if !ch.is_control() {
                    self.apply_insert_action(&InsertAction::Text(ch.to_string()), true);
                }
            }
            EditorInput::Enter => self.apply_insert_action(&InsertAction::Enter, true),
            EditorInput::Tab => self.apply_insert_action(&InsertAction::Tab, true),
            EditorInput::Backspace => self.apply_insert_action(&InsertAction::Backspace, true),
            EditorInput::Delete => self.apply_insert_action(&InsertAction::Delete, true),
            EditorInput::Left => self.apply_insert_action(&InsertAction::Left, true),
            EditorInput::Right => self.apply_insert_action(&InsertAction::Right, true),
            EditorInput::Up => self.apply_insert_action(&InsertAction::Up, true),
            EditorInput::Down => self.apply_insert_action(&InsertAction::Down, true),
        }
    }

    fn apply_insert_action(&mut self, action: &InsertAction, record: bool) {
        let mut changed = false;
        match action {
            InsertAction::Text(text) => {
                changed =
                    self.document
                        .insert_text(&mut self.cursor.row, &mut self.cursor.column, text);
            }
            InsertAction::Enter => {
                changed = self
                    .document
                    .insert_newline(&mut self.cursor.row, &mut self.cursor.column);
            }
            InsertAction::Tab => {
                changed =
                    self.document
                        .insert_text(&mut self.cursor.row, &mut self.cursor.column, "\t");
            }
            InsertAction::Backspace => {
                changed = self
                    .document
                    .backspace(&mut self.cursor.row, &mut self.cursor.column);
            }
            InsertAction::Delete => {
                changed = self
                    .document
                    .delete(&mut self.cursor.row, &mut self.cursor.column);
            }
            InsertAction::Left => self.insert_move_horizontal(false),
            InsertAction::Right => self.insert_move_horizontal(true),
            InsertAction::Up => self.move_vertical(-1),
            InsertAction::Down => self.move_vertical(1),
        }
        if changed {
            self.clamp_cursor_insert();
            self.preferred_visual_column = None;
            self.welcome = false;
        }
        if record && let Some(session) = self.insert.as_mut() {
            session.actions.push(action.clone());
        }
    }

    fn insert_move_horizontal(&mut self, right: bool) {
        if right {
            if self.cursor.column < self.document.line_len_bytes(self.cursor.row) {
                if let Some(range) = self
                    .document
                    .current_grapheme(self.cursor.row, self.cursor.column)
                {
                    self.cursor.column = range.end;
                }
            } else if self.cursor.row + 1 < self.document.line_count() {
                self.cursor.row += 1;
                self.cursor.column = 0;
            }
        } else if self.cursor.column > 0 {
            if let Some(range) = self
                .document
                .previous_grapheme(self.cursor.row, self.cursor.column)
            {
                self.cursor.column = range.start;
            }
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
            self.cursor.column = self.document.line_len_bytes(self.cursor.row);
        }
        self.preferred_visual_column = None;
    }

    fn finish_insert(&mut self) {
        let Some(mut session) = self.insert.take() else {
            self.mode = EditorMode::Normal;
            self.clamp_cursor_normal();
            return;
        };
        while session.count > 1 {
            session.count -= 1;
            if session.entry.is_open() {
                self.open_line(session.entry);
            }
            for action in session.actions.clone() {
                self.apply_insert_action(&action, false);
            }
        }
        if let Some(range) = self
            .document
            .previous_grapheme(self.cursor.row, self.cursor.column)
        {
            self.cursor.column = range.start;
        }
        self.clamp_cursor_normal();
        self.mode = EditorMode::Normal;
        self.preferred_visual_column = None;
        let dot = match &session.dot {
            InsertDot::Keys { keys, count } => DotRecipe::Keys {
                keys: keys.clone(),
                count: *count,
                insert: Some(session.actions.clone()),
            },
            InsertDot::Visual { op, shape } => DotRecipe::Visual {
                op: *op,
                shape: *shape,
                insert: Some(session.actions.clone()),
            },
        };
        self.commit_change(session.before, Some(dot));
    }

    // ---------------------------------------------------------------- visual

    fn handle_visual(&mut self, input: EditorInput) {
        match input {
            EditorInput::Escape => {
                self.cancel_visual();
            }
            EditorInput::Redo => {
                self.parser.reset();
            }
            EditorInput::Character(ch) => self.visual_char(ch),
            EditorInput::Text(text) => {
                for ch in text.chars() {
                    if !self.mode.is_visual() {
                        break;
                    }
                    self.visual_char(ch);
                }
            }
            EditorInput::Enter | EditorInput::Tab | EditorInput::Backspace => {
                self.parser.reset();
            }
            EditorInput::Delete | EditorInput::Paste(_) => {
                self.parser.reset();
            }
            EditorInput::Left => self.visual_arrow(MotionSpec::Left),
            EditorInput::Right => self.visual_arrow(MotionSpec::Right),
            EditorInput::Up => self.visual_arrow(MotionSpec::Up),
            EditorInput::Down => self.visual_arrow(MotionSpec::Down),
        }
    }

    fn visual_arrow(&mut self, spec: MotionSpec) {
        // Same contract as Normal mode: a pending count applies, any other
        // pending prefix cancels and the arrow does not extend the selection.
        let count = self.parser.take_count();
        if count.is_none() && !self.parser.is_idle() {
            self.parser.reset();
            return;
        }
        self.parser.reset();
        self.visual_motion(spec, count);
    }

    fn visual_char(&mut self, ch: char) {
        if let Some(command) = visual_command(ch) {
            self.parser.reset();
            self.visual_command(command);
            return;
        }
        let step = self.parser.push(ch);
        match step {
            Step::Motion { spec, count } => self.visual_motion(spec, count),
            Step::Find {
                kind,
                target,
                count,
            } => self.find_move(kind, target, count, false),
            _ => {}
        }
    }

    fn visual_motion(&mut self, spec: MotionSpec, count: Option<usize>) {
        let count = count.unwrap_or(1);
        let Some(spec) = self.expand_motion(spec) else {
            return;
        };
        if matches!(spec, MotionSpec::Down | MotionSpec::Up) {
            let delta = if spec == MotionSpec::Down { 1 } else { -1 };
            self.move_vertical(delta * count as isize);
            return;
        }
        let Some(outcome) = motion::resolve(self.document(), self.cursor, spec, count) else {
            self.report_motion_failure(spec);
            return;
        };
        self.cursor = outcome.target;
        self.clamp_cursor_normal();
        self.preferred_visual_column = None;
    }

    fn visual_command(&mut self, command: VisualCommand) {
        match command {
            VisualCommand::Delete => self.visual_operator(Operator::Delete),
            VisualCommand::Change => self.visual_operator(Operator::Change),
            VisualCommand::Yank => self.visual_operator(Operator::Yank),
            VisualCommand::Put { before } => self.visual_put(before),
            VisualCommand::Toggle(kind) => self.toggle_visual(kind),
            VisualCommand::Search { forward } => self.open_search(forward, None),
            VisualCommand::SearchNext { reverse } => self.repeat_search(reverse, None),
            VisualCommand::CommandPrompt => self.open_command(),
        }
    }

    fn visual_operator(&mut self, op: Operator) {
        let Some(selection) = self.selection() else {
            return;
        };
        let shape = self.visual_shape(selection);
        let range = self.selection_range(selection);
        match op {
            Operator::Yank => {
                self.yank_range(range);
                self.cursor = selection.start;
                self.clamp_cursor_normal();
                self.preferred_visual_column = None;
                self.cancel_visual_silently();
            }
            Operator::Delete => {
                let snapshot = self.begin_change();
                self.yank_range(range);
                self.delete_range(range);
                self.cancel_visual_silently();
                self.commit_change(
                    snapshot,
                    Some(DotRecipe::Visual {
                        op,
                        shape,
                        insert: None,
                    }),
                );
            }
            Operator::Change => {
                let snapshot = self.begin_change();
                self.yank_range(range);
                let anchor = self.change_range(range);
                self.cursor = anchor;
                self.begin_insert_session(
                    snapshot,
                    InsertEntry::Change,
                    1,
                    InsertDot::Visual { op, shape },
                );
            }
        }
    }

    fn visual_put(&mut self, before: bool) {
        let Some(selection) = self.selection() else {
            return;
        };
        let payload = self.register.clone();
        if payload.is_empty() {
            self.message = Some("Nothing in register".to_owned());
            return;
        }
        let shape = self.visual_shape(selection);
        let snapshot = self.begin_change();
        self.cancel_visual_silently();
        let cursor = self.replace_selection(selection, &payload, !before);
        self.cursor = cursor;
        self.clamp_cursor_normal();
        self.preferred_visual_column = None;
        // Visual `p` swaps the register with the removed selection, so it cannot
        // be repeated faithfully; only `P` refreshes the dot recipe.
        let dot = before.then_some(DotRecipe::VisualPut { shape });
        self.commit_change(snapshot, dot);
    }

    fn replace_selection(
        &mut self,
        selection: Selection,
        payload: &Register,
        swap_register: bool,
    ) -> Cursor {
        let removed = match self.selection_range(selection) {
            MotionRange::Char { start, end } => {
                Register::Characterwise(self.document.text_between(start, end))
            }
            MotionRange::Line { first, last } => {
                Register::Linewise(self.document.captured_lines(first, last))
            }
        };
        let cursor = self.replace_selection_text(selection, payload);
        if swap_register {
            self.register = removed;
        }
        cursor
    }

    fn replace_selection_text(&mut self, selection: Selection, payload: &Register) -> Cursor {
        match (payload, selection.kind) {
            (Register::Characterwise(text), SelectionKind::Characterwise) => {
                let end = Cursor::new(
                    selection.end.row,
                    self.document
                        .grapheme_end(selection.end.row, selection.end.column),
                );
                self.document
                    .replace_text_between(selection.start, end, text);
                selection.start
            }
            (Register::Characterwise(text), SelectionKind::Linewise) => {
                let lines = split_lines(text);
                let row = self.document.replace_line_range(
                    selection.start.row,
                    selection.end.row,
                    &lines,
                );
                Cursor::new(row, self.document.first_nonblank_col(row))
            }
            (Register::Linewise(lines), SelectionKind::Linewise) => {
                let row =
                    self.document
                        .replace_line_range(selection.start.row, selection.end.row, lines);
                Cursor::new(row, self.document.first_nonblank_col(row))
            }
            (Register::Linewise(lines), SelectionKind::Characterwise) => {
                let end = Cursor::new(
                    selection.end.row,
                    self.document
                        .grapheme_end(selection.end.row, selection.end.column),
                );
                let prefix = self
                    .document
                    .line(selection.start.row)
                    .map_or_else(String::new, |line| {
                        line[..selection.start.column.min(line.len())].to_owned()
                    });
                let suffix = self
                    .document
                    .line(end.row)
                    .map_or_else(String::new, |line| {
                        line[end.column.min(line.len())..].to_owned()
                    });
                let mut replacement: Vec<CapturedLine> = Vec::with_capacity(lines.len() + 2);
                replacement.push(CapturedLine {
                    text: prefix,
                    ending: Some(self.document.insertion_ending()),
                });
                replacement.extend(lines.iter().cloned());
                replacement.push(CapturedLine {
                    text: suffix,
                    ending: None,
                });
                let row = self.document.replace_line_range(
                    selection.start.row,
                    selection.end.row,
                    &replacement,
                );
                Cursor::new(row, self.document.first_nonblank_col(row))
            }
            (Register::Empty, _) => self.cursor,
        }
    }

    fn selection_range(&self, selection: Selection) -> MotionRange {
        match selection.kind {
            SelectionKind::Characterwise => MotionRange::Char {
                start: selection.start,
                end: Cursor::new(
                    selection.end.row,
                    self.document
                        .grapheme_end(selection.end.row, selection.end.column),
                ),
            },
            SelectionKind::Linewise => MotionRange::Line {
                first: selection.start.row,
                last: selection.end.row,
            },
        }
    }

    fn visual_shape(&self, selection: Selection) -> VisualShape {
        match selection.kind {
            SelectionKind::Linewise => {
                VisualShape::Lines(selection.end.row.saturating_sub(selection.start.row) + 1)
            }
            SelectionKind::Characterwise => {
                if selection.start.row == selection.end.row {
                    let start = self
                        .document
                        .grapheme_index(selection.start.row, selection.start.column);
                    let end = self
                        .document
                        .grapheme_index(selection.end.row, selection.end.column);
                    VisualShape::Chars(end.saturating_sub(start) + 1)
                } else {
                    VisualShape::Rows {
                        rows: selection.end.row.saturating_sub(selection.start.row),
                        last_ordinal: self
                            .document
                            .grapheme_index(selection.end.row, selection.end.column),
                    }
                }
            }
        }
    }

    fn shape_selection(&self, shape: VisualShape, at: Cursor) -> Selection {
        let last_row = self.document.line_count().saturating_sub(1);
        let selection = match shape {
            VisualShape::Lines(lines) => {
                let end_row = at.row.saturating_add(lines.max(1) - 1).min(last_row);
                Selection {
                    start: Cursor::new(at.row, 0),
                    end: Cursor::new(end_row, self.document.last_grapheme_col(end_row)),
                    kind: SelectionKind::Linewise,
                }
            }
            VisualShape::Chars(chars) => Selection {
                start: at,
                end: self.advance_graphemes(at, chars.max(1) - 1),
                kind: SelectionKind::Characterwise,
            },
            VisualShape::Rows { rows, last_ordinal } => {
                let end_row = at.row.saturating_add(rows).min(last_row);
                let ordinal =
                    last_ordinal.min(self.document.grapheme_count(end_row).saturating_sub(1));
                let end_column = self.document.byte_at_grapheme(end_row, ordinal);
                Selection {
                    start: at,
                    end: Cursor::new(end_row, end_column),
                    kind: SelectionKind::Characterwise,
                }
            }
        };
        // The contract is a normalized forward shape replayed from the cursor as
        // the new selection start: the recorded endpoint is clamped to available
        // text but never placed before that start, so the final cursor grapheme
        // stays included even when the row span collapses at the end of the file.
        let end = if selection.end < selection.start {
            selection.start
        } else {
            selection.end
        };
        Selection {
            start: selection.start,
            end,
            kind: selection.kind,
        }
    }

    fn advance_graphemes(&self, from: Cursor, count: usize) -> Cursor {
        let mut cursor = from;
        for _ in 0..count {
            match self.step_grapheme_forward(cursor) {
                Some(next) => cursor = next,
                None => break,
            }
        }
        cursor
    }

    fn step_grapheme_forward(&self, cursor: Cursor) -> Option<Cursor> {
        if let Some(range) = self.document.current_grapheme(cursor.row, cursor.column) {
            let next = range.end;
            if next < self.document.line_len_bytes(cursor.row) {
                return Some(Cursor::new(cursor.row, next));
            }
        }
        if cursor.row + 1 < self.document.line_count() {
            return Some(Cursor::new(cursor.row + 1, 0));
        }
        None
    }

    fn enter_visual(&mut self, kind: SelectionKind, count: Option<usize>) {
        let anchor = self.cursor;
        let count = count.unwrap_or(1).max(1);
        let cursor = if count > 1 {
            match kind {
                SelectionKind::Characterwise => self.advance_graphemes(anchor, count - 1),
                SelectionKind::Linewise => {
                    let last_row = self.document.line_count().saturating_sub(1);
                    let row = anchor.row.saturating_add(count - 1).min(last_row);
                    Cursor::new(row, self.document.last_grapheme_col(row))
                }
            }
        } else {
            anchor
        };
        self.visual = Some(VisualState { anchor, kind });
        self.cursor = cursor;
        self.mode = match kind {
            SelectionKind::Characterwise => EditorMode::Visual,
            SelectionKind::Linewise => EditorMode::VisualLine,
        };
        self.parser.reset();
        self.preferred_visual_column = None;
    }

    fn toggle_visual(&mut self, kind: SelectionKind) {
        match self.visual {
            Some(state) if state.kind == kind => {
                self.cancel_visual_silently();
            }
            Some(state) => {
                let anchor = state.anchor;
                self.visual = Some(VisualState { anchor, kind });
                self.mode = match kind {
                    SelectionKind::Characterwise => EditorMode::Visual,
                    SelectionKind::Linewise => EditorMode::VisualLine,
                };
            }
            None => self.enter_visual(kind, None),
        }
    }

    fn cancel_visual(&mut self) {
        self.cancel_visual_silently();
        self.preferred_visual_column = None;
    }

    fn cancel_visual_silently(&mut self) {
        self.visual = None;
        self.mode = EditorMode::Normal;
        self.parser.reset();
    }

    // ---------------------------------------------------------------- search

    fn open_search(&mut self, forward: bool, count: Option<usize>) {
        let restore = PromptReturn {
            mode: self.mode,
            visual: self.visual,
            cursor: self.cursor,
        };
        self.search_prompt = Some(SearchPrompt {
            forward,
            count: count.unwrap_or(1).max(1),
            restore,
        });
        self.prompt.clear();
        self.mode = if forward {
            EditorMode::SearchForward
        } else {
            EditorMode::SearchBackward
        };
        self.parser.reset();
    }

    fn repeat_search(&mut self, reverse: bool, count: Option<usize>) {
        let Some(memory) = self.search_memory.clone() else {
            self.message = Some("No previous search".to_owned());
            return;
        };
        let forward = if reverse {
            !memory.forward
        } else {
            memory.forward
        };
        self.search_to(&memory.query, forward, count.unwrap_or(1).max(1));
    }

    fn run_search(&mut self, query: &str, forward: bool, count: usize) {
        let query = if query.is_empty() {
            match &self.search_memory {
                Some(memory) if !memory.query.is_empty() => memory.query.clone(),
                _ => {
                    self.message = Some("No previous search".to_owned());
                    return;
                }
            }
        } else {
            query.to_owned()
        };
        self.search_memory = Some(SearchMemory {
            query: query.clone(),
            forward,
        });
        self.search_to(&query, forward, count);
    }

    fn search_to(&mut self, query: &str, forward: bool, count: usize) {
        let matches = self.search_matches(query);
        if matches.is_empty() {
            self.message = Some("Pattern not found".to_owned());
            return;
        }
        let cursor = self.cursor;
        let mut order: Vec<Cursor> = Vec::with_capacity(matches.len());
        if forward {
            order.extend(matches.iter().copied().filter(|found| *found > cursor));
            order.extend(matches.iter().copied().filter(|found| *found <= cursor));
        } else {
            order.extend(
                matches
                    .iter()
                    .rev()
                    .copied()
                    .filter(|found| *found < cursor),
            );
            order.extend(
                matches
                    .iter()
                    .rev()
                    .copied()
                    .filter(|found| *found >= cursor),
            );
        }
        let index = (count.max(1) - 1) % order.len();
        self.cursor = order[index];
        if !self.mode.is_visual() {
            self.clamp_cursor_normal();
        }
        self.preferred_visual_column = None;
    }

    /// All literal, case-sensitive single-line matches whose start and end are
    /// grapheme boundaries, in buffer order.
    fn search_matches(&self, query: &str) -> Vec<Cursor> {
        let mut matches = Vec::new();
        if query.is_empty() {
            return matches;
        }
        for row in 0..self.document.line_count() {
            let Some(line) = self.document.line(row) else {
                continue;
            };
            let boundaries: Vec<usize> = line
                .grapheme_indices(true)
                .map(|(index, _)| index)
                .chain(std::iter::once(line.len()))
                .collect();
            for &start in &boundaries {
                let Some(candidate) = line.get(start..) else {
                    continue;
                };
                if !candidate.starts_with(query) {
                    continue;
                }
                let end = start + query.len();
                if boundaries.binary_search(&end).is_ok() {
                    matches.push(Cursor::new(row, start));
                }
            }
        }
        matches
    }

    // --------------------------------------------------------------- prompts

    fn open_command(&mut self) {
        self.command_return = Some(PromptReturn {
            mode: self.mode,
            visual: self.visual,
            cursor: self.cursor,
        });
        self.prompt.clear();
        self.mode = EditorMode::Command;
        self.parser.reset();
    }

    fn handle_prompt(&mut self, input: EditorInput) {
        match input {
            EditorInput::Escape => self.cancel_prompt(),
            EditorInput::Enter => self.submit_prompt(),
            EditorInput::Backspace => {
                self.prompt.pop();
            }
            EditorInput::Character(ch) if !ch.is_control() => self.prompt.push(ch),
            EditorInput::Text(text) => self
                .prompt
                .extend(text.chars().filter(|ch| !ch.is_control())),
            _ => {}
        }
    }

    fn prompt_return(&self) -> Option<PromptReturn> {
        match self.mode {
            EditorMode::Command => self.command_return,
            EditorMode::SearchForward | EditorMode::SearchBackward => {
                self.search_prompt.map(|prompt| prompt.restore)
            }
            _ => None,
        }
    }

    fn cancel_prompt(&mut self) {
        let restore = self.prompt_return();
        self.prompt.clear();
        self.search_prompt = None;
        self.command_return = None;
        match restore {
            Some(restore) => {
                self.mode = restore.mode;
                self.visual = restore.visual;
                self.cursor = restore.cursor;
            }
            None => self.mode = EditorMode::Normal,
        }
        self.parser.reset();
    }

    fn submit_prompt(&mut self) {
        let text = std::mem::take(&mut self.prompt);
        let search = self.search_prompt.take();
        let restore = if self.mode == EditorMode::Command {
            self.command_return.take()
        } else {
            search.map(|prompt| prompt.restore)
        };
        match restore {
            Some(restore) => {
                self.mode = restore.mode;
                self.visual = restore.visual;
                self.cursor = restore.cursor;
            }
            None => {
                self.mode = EditorMode::Normal;
                self.visual = None;
            }
        }
        if let Some(prompt) = search {
            self.run_search(&text, prompt.forward, prompt.count);
        } else {
            self.execute_command(&text);
        }
    }

    // ------------------------------------------------------ disk reconciliation

    /// Samples the active file and reconciles a confirmed external change.
    ///
    /// Returns `true` when anything observable changed, so the event loop can
    /// skip a redraw when nothing happened. Reads happen at most once per
    /// [`POLL_INTERVAL`], and a change is reported or applied only after two
    /// equivalent samples at least one interval apart, which hides ordinary
    /// truncate-then-write gaps without claiming to detect every partial write.
    ///
    /// Reconciliation never edits a dirty buffer, never advances the expected
    /// disk state from a foreign observation, and installs a reload only for a
    /// clean buffer at an idle Normal boundary.
    pub fn poll_disk(&mut self, now: Instant) -> bool {
        if self.should_quit || self.disk.is_none() {
            return false;
        }
        let Some(path) = self.path.clone() else {
            return false;
        };
        if let Some(last) = self.last_sample
            && now.saturating_duration_since(last) < POLL_INTERVAL
        {
            return false;
        }
        self.last_sample = Some(now);
        let probe = sample_disk(&path);
        // The disk is only trusted once it has looked the same twice, one
        // interval apart; a single sample is cached for the next comparison.
        let confirmed = matches!(
            &self.stable_candidate,
            Some(candidate)
                if now.saturating_duration_since(candidate.at) >= POLL_INTERVAL
                    && candidate.probe.same_as(&probe)
        );
        self.stable_candidate = None;
        if !confirmed {
            self.stable_candidate = Some(StableSample { probe, at: now });
            return false;
        }
        self.reconcile_sample(probe)
    }

    fn reconcile_sample(&mut self, probe: DiskProbe) -> bool {
        let prior_status = self.external.clone();
        let text_changed = match probe {
            DiskProbe::Readable(observation) => self.reconcile_contents(observation),
            DiskProbe::NotUtf8(_) => {
                // Bytes we cannot parse are reported, never installed, so last
                // good text and baseline survive.
                self.external =
                    Some("Disk changed externally; contents are not valid UTF-8".to_owned());
                false
            }
            DiskProbe::Failed(kind) => {
                self.external = Some(format!("Disk unavailable: {}", failure_reason(kind)));
                false
            }
        };
        text_changed || self.external != prior_status
    }

    /// Reconciles a readable observation with the expected disk state.
    ///
    /// Returns whether the buffer text or history changed.
    fn reconcile_contents(&mut self, observation: Observation) -> bool {
        let Some(expected) = self.disk.as_ref() else {
            return false;
        };
        let same_bytes = expected.contents == observation.contents;
        let same_target = expected.destination == observation.destination;
        if same_bytes && same_target {
            // The common case: nothing happened. Clearing a stale status is the
            // only work, and no history or baseline moves.
            self.external = None;
            return false;
        }
        if same_bytes {
            // Same bytes at a new destination: an alias was retargeted, or an
            // identical file replaced the old one. The text is unaffected, so
            // only the destination a later save must match may be refreshed, and
            // only from an idle clean buffer. Anything else is reported like any
            // other external change instead of passing silently: a dirty buffer
            // is a conflict, and a clean buffer mid-sequence is deferred.
            if self.is_dirty() {
                self.external = Some(CONFLICT_STATUS.to_owned());
            } else if self.at_reload_boundary() {
                self.disk = Some(observation);
                self.external = None;
                self.stable_candidate = None;
            } else {
                self.external = Some(PENDING_STATUS.to_owned());
            }
            return false;
        }
        match &observation.contents {
            DiskContents::Missing => {
                // Deletion is an unavailable file, not an empty document: keep
                // the last good text and baseline.
                self.external = Some("Disk unavailable: file was removed".to_owned());
                false
            }
            DiskContents::Present(bytes) => {
                let text = std::str::from_utf8(bytes)
                    .expect("probe validated UTF-8")
                    .to_owned();
                if self.is_dirty() {
                    // Foreign bytes never authorize an overwrite and never
                    // replace local work.
                    self.external = Some(CONFLICT_STATUS.to_owned());
                    return false;
                }
                if !self.at_reload_boundary() {
                    // A clean buffer that is mid-sequence, selecting, prompting
                    // or inserting is reconciled again at the next boundary.
                    self.external = Some(PENDING_STATUS.to_owned());
                    return false;
                }
                let changed = self.install_reload(observation, &text);
                self.message = Some("Reloaded changed file from disk".to_owned());
                changed
            }
        }
    }

    /// Whether the buffer is at an idle Normal boundary where installing new
    /// text cannot discard parser state, a selection, a prompt, or an Insert
    /// session that is still open.
    fn at_reload_boundary(&self) -> bool {
        self.mode == EditorMode::Normal
            && self.parser.is_idle()
            && self.visual.is_none()
            && self.insert.is_none()
            && self.command_return.is_none()
            && self.search_prompt.is_none()
            && !self.transaction_open
            && !self.replaying
            && self.prompt.is_empty()
    }

    /// Replaces the buffer with `text` and makes it the saved baseline and the
    /// expected disk state.
    ///
    /// Returns whether the text changed; only a real change records one undo
    /// step, clears redo and drops the stale dot recipe. Undo restores the older
    /// text while the disk baseline stays at the reloaded bytes, so undo never
    /// writes the file and the undone text reads as unsaved.
    fn install_reload(&mut self, observation: Observation, text: &str) -> bool {
        let document = Document::from_text(text);
        let changed = document != self.document;
        if changed {
            self.undo.push(Snapshot {
                document: std::mem::replace(&mut self.document, document),
                cursor: self.cursor,
            });
            self.redo.clear();
            self.dot = None;
        } else {
            self.document = document;
        }
        self.saved = self.document.serialize();
        self.disk = Some(observation);
        self.external = None;
        self.stable_candidate = None;
        self.last_sample = None;
        self.mode = EditorMode::Normal;
        self.visual = None;
        self.prompt.clear();
        self.command_return = None;
        self.search_prompt = None;
        self.insert = None;
        self.transaction_open = false;
        self.preferred_visual_column = None;
        self.parser.reset();
        self.clamp_cursor_normal();
        changed
    }

    // ------------------------------------------------------------- save paths

    /// Whether `candidate` names the file this buffer is already editing.
    ///
    /// The comparison is lexical and target-based rather than an exact string
    /// match, so `./file`, an absolute path, and a symlink alias all keep the
    /// active baseline. A candidate is an alias when any of these hold:
    ///
    /// 1. its normalized spelling equals the active spelling;
    /// 2. its normalized spelling equals the destination stored in the active
    ///    expectation, which is the literal target that was opened or written;
    /// 3. it resolves to that stored destination, so another name for the old
    ///    target still uses the old baseline even if the active path moved;
    /// 4. it resolves to the destination the active path resolves to right now,
    ///    which covers a sibling alias of a retargeted active path; or
    /// 5. the active path's current destination is the candidate's own spelling,
    ///    which covers a candidate that has since disappeared.
    ///
    /// Rules 4 and 5 are what stop a retargeted active path from handing another
    /// name a fresh save-as expectation: every name that points where the active
    /// path points is written through the baseline captured at open. Deliberately
    /// conservative: a retargeted or deleted active path still matches its own
    /// spelling, so a conflict cannot be escaped by respelling the destination.
    fn is_active_alias(&self, candidate: &Path) -> bool {
        let (Some(active), Some(expected)) = (self.path.as_deref(), self.disk.as_ref()) else {
            return false;
        };
        let Ok(candidate_key) = storage::normalize_path(candidate) else {
            return false;
        };
        if storage::normalize_path(active).is_ok_and(|key| key == candidate_key) {
            return true;
        }
        if storage::normalize_path(&expected.destination).is_ok_and(|key| key == candidate_key) {
            return true;
        }
        let candidate_target = storage::resolve_destination(candidate).ok();
        let active_target = storage::resolve_destination(active).ok();
        if let (Some(candidate_destination), Some(active_destination)) =
            (&candidate_target, &active_target)
            && candidate_destination == active_destination
        {
            return true;
        }
        if candidate_target.is_some_and(|resolved| resolved == expected.destination) {
            return true;
        }
        active_target.is_some_and(|resolved| resolved == candidate_key)
    }

    fn save_to(&mut self, path: PathBuf) -> bool {
        let bytes = self.document.serialize();
        // The alias decision is made before anything is adopted or mutated.
        let expectation = if self.is_active_alias(&path) {
            match self.disk.clone() {
                Some(expectation) => expectation,
                None => match storage::observe(&path) {
                    Ok(observation) => observation,
                    Err(error) => {
                        self.message = Some(format!("Write failed: {error}"));
                        return false;
                    }
                },
            }
        } else {
            // A save-as target carries its own expectation, captured as the save
            // begins; unrelated destination bytes are never compared with the
            // active file's baseline.
            match storage::observe(&path) {
                Ok(observation) => observation,
                Err(error) => {
                    self.message = Some(format!("Write failed: {error}"));
                    return false;
                }
            }
        };
        match storage::save_document_guarded(&path, bytes.as_bytes(), &expectation) {
            Ok(committed) => {
                self.path = Some(path);
                self.saved = bytes;
                self.disk = Some(committed);
                self.external = None;
                self.stable_candidate = None;
                self.last_sample = None;
                self.message = Some("Written".to_owned());
                true
            }
            Err(error) => {
                // The buffer, path, dirty state and old expectation survive.
                self.message = Some(save_failure_message(&error));
                false
            }
        }
    }

    // -------------------------------------------------------------------- Ex

    fn execute_command(&mut self, text: &str) {
        let command = text.trim();
        if command.is_empty() {
            self.message = Some("Empty command".to_owned());
            return;
        }
        match command {
            "w" => self.save_active(),
            "q" => self.quit_without_force(),
            "q!" => self.should_quit = true,
            "reload" => self.reload_file(false),
            "reload!" => self.reload_file(true),
            "wq" => {
                if self.save_active_ok() {
                    self.should_quit = true;
                }
            }
            "x" => {
                if !self.is_dirty() || self.save_active_ok() {
                    self.should_quit = true;
                }
            }
            _ => {
                if let Some(rest) = format_command_text(command) {
                    self.format_text(rest);
                } else {
                    match write_path_argument(command) {
                        Some(path) if !path.is_empty() => {
                            self.save_to(PathBuf::from(path));
                        }
                        Some(_) => self.message = Some("No filename; use :w <path>".to_owned()),
                        None => self.message = Some(format!("Unknown command: {command}")),
                    }
                }
            }
        }
    }

    /// `:reload` and `:reload!`: install the current disk contents.
    ///
    /// Both take no arguments and fail without touching text, path, history or
    /// the expected baseline when the file is unnamed, missing, unreadable,
    /// nonregular or not valid UTF-8. Without `!` a dirty buffer is refused;
    /// with `!` the local text is replaced as one recoverable undo step. A
    /// Visual-origin prompt normalizes to Normal only on success.
    fn reload_file(&mut self, force: bool) {
        let Some(path) = self.path.clone() else {
            self.message = Some("No filename; :reload needs a file path".to_owned());
            return;
        };
        if !force && self.is_dirty() {
            self.message = Some("Unsaved changes; use :reload! to discard them".to_owned());
            return;
        }
        match sample_disk(&path) {
            DiskProbe::Readable(observation) => {
                let text = match &observation.contents {
                    DiskContents::Present(bytes) => {
                        String::from_utf8(bytes.clone()).expect("probe validated UTF-8")
                    }
                    DiskContents::Missing => {
                        self.message = Some("Reload failed: file was removed from disk".to_owned());
                        return;
                    }
                };
                let changed = self.install_reload(observation, &text);
                self.message = Some(if changed {
                    "Reloaded from disk".to_owned()
                } else {
                    "Already up to date".to_owned()
                });
            }
            DiskProbe::NotUtf8(_) => {
                self.message = Some("Reload failed: contents are not valid UTF-8".to_owned());
            }
            DiskProbe::Failed(kind) => {
                self.message = Some(format!("Reload failed: {}", failure_reason(kind)));
            }
        }
    }

    /// `:format PROGRAM [ARG...]`: run an explicitly chosen formatter over the
    /// whole editable text or the active Visual selection.
    ///
    /// The arguments are split with shell-style quoting but never expanded or
    /// interpreted, so `|`, `$HOME` and `>` stay literal. Nothing is written to
    /// disk and the saved baseline is untouched: the replacement is one undoable
    /// buffer change. A no-op replacement adds no history and keeps the dot
    /// recipe; any failure leaves text, mode, selection, cursor and history
    /// exactly as they were.
    fn format_text(&mut self, arguments: &str) {
        let Some((program, program_arguments)) = parse_formatter(arguments, &mut self.message)
        else {
            return;
        };
        let mut formatter =
            FormatterSpec::new(program.as_str()).args(program_arguments.iter().map(String::as_str));
        // The formatter runs in the editor's own working directory, declared
        // explicitly rather than left to whatever the child would inherit.
        if let Ok(directory) = std::env::current_dir() {
            formatter = formatter.working_directory(directory);
        }
        let (source, selection_start) = match self.selection() {
            Some(selection) => (
                TransformSource::from_document(
                    &self.document,
                    self.selection_byte_range(selection),
                ),
                Some(selection.start),
            ),
            None => (
                TransformSource::whole(self.document.editable_text(), self.document.has_bom()),
                None,
            ),
        };
        let result = match TransformRequest::new(source, formatter).execute() {
            Ok(result) => result,
            Err(error) => {
                self.message = Some(transform_failure_message(&error));
                return;
            }
        };
        // The library refuses a result whose source no longer matches, so a
        // stale application can never splice into the wrong text.
        let document = match result.apply(&self.document) {
            Ok(document) => document,
            Err(error) => {
                self.message = Some(transform_failure_message(&error));
                return;
            }
        };
        if result.changed() {
            self.undo.push(Snapshot {
                document: std::mem::replace(&mut self.document, document),
                cursor: self.cursor,
            });
            self.redo.clear();
            // `.` replays key recipes; it must not re-run an external program.
            self.dot = None;
        } else {
            self.document = document;
        }
        if let Some(start) = selection_start {
            self.cursor = start;
        }
        self.clamp_cursor_normal();
        self.mode = EditorMode::Normal;
        self.visual = None;
        self.prompt.clear();
        self.command_return = None;
        self.search_prompt = None;
        self.transaction_open = false;
        self.preferred_visual_column = None;
        self.parser.reset();
        // Formatting may take a while, so the file can move underneath it. The
        // new local text is kept and the difference is surfaced; a later save is
        // refused by the guard even if polling has not noticed yet.
        self.mark_possible_external_change();
        self.message = Some(match result.warnings() {
            Some(warnings) => format!("Formatted; formatter stderr: {warnings}"),
            None => "Formatted".to_owned(),
        });
    }

    /// Half-open byte range of `selection` in the document's editable text.
    ///
    /// A characterwise selection includes its last grapheme, a linewise
    /// selection includes each selected line's terminator where the file has
    /// one, and a final line without a terminator stays exact.
    fn selection_byte_range(&self, selection: Selection) -> ByteRange {
        let start = self.document.byte_offset(selection.start);
        let end = match selection.kind {
            SelectionKind::Characterwise => {
                let column = self
                    .document
                    .grapheme_end(selection.end.row, selection.end.column);
                self.document
                    .byte_offset(Cursor::new(selection.end.row, column))
            }
            SelectionKind::Linewise => {
                let next = selection.end.row + 1;
                if next < self.document.line_count() {
                    self.document.byte_offset(Cursor::new(next, 0))
                } else {
                    self.document.editable_text().len()
                }
            }
        };
        ByteRange::new(start, end)
    }

    /// Re-looks at the active file after an external program ran.
    ///
    /// This is not a poll: it takes no sample series and applies nothing. It only
    /// raises the persistent status when the file already differs from the
    /// baseline, so a formatter run that raced a foreign writer is visible at
    /// once instead of at the next confirmation.
    fn mark_possible_external_change(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some(expected) = self.disk.as_ref() else {
            return;
        };
        let unchanged =
            matches!(&sample_disk(&path), DiskProbe::Readable(observed) if observed == expected);
        self.stable_candidate = None;
        self.last_sample = None;
        if unchanged {
            self.external = None;
        } else {
            self.external = Some(
                if self.is_dirty() {
                    CONFLICT_STATUS
                } else {
                    PENDING_STATUS
                }
                .to_owned(),
            );
        }
    }

    fn quit_without_force(&mut self) {
        if self.is_dirty() {
            self.message =
                Some("Unsaved changes; use :w [path] before :q, or :q! to discard".to_owned());
        } else {
            self.should_quit = true;
        }
    }

    fn save_active(&mut self) {
        let _ = self.save_active_ok();
    }

    fn save_active_ok(&mut self) -> bool {
        match self.path.clone() {
            Some(path) => self.save_to(path),
            None => {
                self.message = Some("No filename; use :w <path>".to_owned());
                false
            }
        }
    }

    // --------------------------------------------------------- history, dot

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            document: self.document.clone(),
            cursor: self.cursor,
        }
    }

    fn begin_change(&mut self) -> Option<Snapshot> {
        if self.transaction_open {
            return None;
        }
        self.transaction_open = true;
        Some(self.snapshot())
    }

    fn commit_change(&mut self, before: Option<Snapshot>, dot: Option<DotRecipe>) {
        let Some(before) = before else {
            return;
        };
        self.transaction_open = false;
        if self.document != before.document {
            self.undo.push(before);
            self.redo.clear();
            if !self.replaying
                && let Some(dot) = dot
            {
                self.dot = Some(dot);
            }
        }
    }

    fn undo_steps(&mut self, count: usize) {
        self.parser.reset();
        for _ in 0..count.max(1) {
            let Some(snapshot) = self.undo.pop() else {
                self.message = Some("Already at oldest change".to_owned());
                return;
            };
            self.redo.push(self.snapshot());
            self.restore(snapshot);
        }
        self.message = None;
    }

    fn redo_steps(&mut self, count: usize) {
        self.parser.reset();
        for _ in 0..count.max(1) {
            let Some(snapshot) = self.redo.pop() else {
                self.message = Some("Already at newest change".to_owned());
                return;
            };
            self.undo.push(self.snapshot());
            self.restore(snapshot);
        }
        self.message = None;
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.document = snapshot.document;
        self.cursor = snapshot.cursor;
        self.mode = EditorMode::Normal;
        self.visual = None;
        self.prompt.clear();
        self.search_prompt = None;
        self.command_return = None;
        self.insert = None;
        self.parser.reset();
        self.transaction_open = false;
        self.preferred_visual_column = None;
        self.clamp_cursor_normal();
    }

    fn repeat_dot(&mut self, count: Option<usize>) {
        let Some(recipe) = self.dot.clone() else {
            self.message = Some("No previous change".to_owned());
            return;
        };
        let snapshot = self.begin_change();
        self.replaying = true;
        match recipe {
            DotRecipe::Keys {
                keys,
                count: stored,
                insert,
            } => {
                // A count typed before `.` replaces the stored one; an explicit
                // count (even 1) is replayed as digits so `1dG` and `dG` differ.
                if let Some(effective) = count.or(stored) {
                    for digit in keymap::count_digits(effective) {
                        let step = self.parser.push(digit);
                        self.normal_step(step);
                    }
                }
                for key in keys {
                    let step = self.parser.push(key);
                    self.normal_step(step);
                }
                if self.mode == EditorMode::Insert {
                    self.replay_insert_actions(insert.as_deref());
                }
            }
            DotRecipe::Visual { op, shape, insert } => {
                let selection = self.shape_selection(shape, self.cursor);
                let range = self.selection_range(selection);
                self.yank_range(range);
                match op {
                    Operator::Yank => {}
                    Operator::Delete => self.delete_range(range),
                    Operator::Change => {
                        let anchor = self.change_range(range);
                        self.cursor = anchor;
                        self.begin_insert_session(
                            None,
                            InsertEntry::Change,
                            1,
                            InsertDot::Visual { op, shape },
                        );
                        self.replay_insert_actions(insert.as_deref());
                    }
                }
            }
            DotRecipe::VisualPut { shape } => {
                let selection = self.shape_selection(shape, self.cursor);
                let payload = self.register.clone();
                if payload.is_empty() {
                    self.message = Some("Nothing in register".to_owned());
                } else {
                    let cursor = self.replace_selection_text(selection, &payload);
                    self.cursor = cursor;
                    self.clamp_cursor_normal();
                }
            }
        }
        self.replaying = false;
        self.commit_change(snapshot, None);
    }

    fn replay_insert_actions(&mut self, actions: Option<&[InsertAction]>) {
        if let Some(actions) = actions {
            for action in actions {
                self.apply_insert_action(action, true);
            }
        }
        self.finish_insert();
    }

    // --------------------------------------------------------------- helpers

    fn clamp_cursor_normal(&mut self) {
        let row = self
            .cursor
            .row
            .min(self.document.line_count().saturating_sub(1));
        self.cursor = Cursor::new(
            row,
            self.document.normal_cursor_col(row, self.cursor.column),
        );
    }

    fn clamp_cursor_insert(&mut self) {
        let row = self
            .cursor
            .row
            .min(self.document.line_count().saturating_sub(1));
        self.cursor = Cursor::new(row, self.document.clamp_boundary(row, self.cursor.column));
    }
}

fn reverse_find(kind: FindKind) -> FindKind {
    match kind {
        FindKind::FindForward => FindKind::FindBackward,
        FindKind::FindBackward => FindKind::FindForward,
        FindKind::TillForward => FindKind::TillBackward,
        FindKind::TillBackward => FindKind::TillForward,
    }
}

fn normalize_selection(
    start: Cursor,
    end: Cursor,
    kind: SelectionKind,
    document: &Document,
) -> Selection {
    match kind {
        SelectionKind::Characterwise => Selection { start, end, kind },
        SelectionKind::Linewise => Selection {
            start: Cursor::new(start.row, 0),
            end: Cursor::new(end.row, document.last_grapheme_col(end.row)),
            kind,
        },
    }
}

/// A trailing `w` argument: `w <path>` allows surrounding whitespace and a path
/// containing spaces. Any other `w` prefix (`wq`, `w!`) is not a path form.
fn write_path_argument(command: &str) -> Option<&str> {
    let rest = command.strip_prefix('w')?;
    if rest.starts_with(|ch: char| ch.is_whitespace()) {
        Some(rest.trim())
    } else {
        None
    }
}

/// The argument text of `:format PROGRAM [ARG...]`, without the leading blank.
///
/// Only a bare `format` or `format` followed by whitespace is the command, so
/// `format!` and `formatx` stay unknown commands. `:w PATH` keeps its literal
/// path grammar and never comes through here.
fn format_command_text(command: &str) -> Option<&str> {
    let rest = command.strip_prefix("format")?;
    if rest.is_empty() {
        return Some(rest);
    }
    rest.starts_with(|ch: char| ch.is_whitespace())
        .then(|| rest.trim_start())
}

/// Reads one disk sample without changing anything.
fn sample_disk(path: &Path) -> DiskProbe {
    match storage::observe(path) {
        Ok(observation) => {
            let valid_utf8 = match &observation.contents {
                DiskContents::Missing => true,
                DiskContents::Present(bytes) => std::str::from_utf8(bytes).is_ok(),
            };
            if valid_utf8 {
                DiskProbe::Readable(observation)
            } else {
                DiskProbe::NotUtf8(observation)
            }
        }
        Err(error) => DiskProbe::Failed(error.kind()),
    }
}

/// Compact reason text for a status line, chosen so the message names the
/// situation rather than a bare error number.
fn failure_reason(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "file was removed",
        io::ErrorKind::PermissionDenied => "permission denied",
        io::ErrorKind::InvalidInput => "not a regular file",
        _ => "cannot read the file",
    }
}

/// User-facing text for a refused or failed write.
fn save_failure_message(error: &SaveError) -> String {
    match error {
        SaveError::Conflict { .. } => {
            "Write refused: file changed on disk; use :reload! or :w <path>".to_owned()
        }
        SaveError::Io(error) => format!("Write failed: {error}"),
    }
}

/// Splits `:format` argument text into a program and its literal arguments.
///
/// Shell-style quoting groups words; nothing is expanded, globbed, or handed to
/// a shell, so `|`, `>`, and `$HOME` reach the program as written. A malformed
/// quote or an empty invocation leaves a usage message and returns `None`.
fn parse_formatter(arguments: &str, message: &mut Option<String>) -> Option<(String, Vec<String>)> {
    if arguments.trim().is_empty() {
        *message = Some("Usage: :format PROGRAM [ARG...]".to_owned());
        return None;
    }
    let argv = match shell_words::split(arguments) {
        Ok(argv) => argv,
        Err(error) => {
            *message = Some(format!("Invalid formatter arguments: {error}"));
            return None;
        }
    };
    let Some((program, arguments)) = argv.split_first() else {
        *message = Some("Usage: :format PROGRAM [ARG...]".to_owned());
        return None;
    };
    if program.is_empty() {
        *message = Some("Usage: :format PROGRAM [ARG...]".to_owned());
        return None;
    }
    Some((program.clone(), arguments.to_vec()))
}

/// User-facing text for a transformation that produced nothing applicable,
/// keeping the useful category before the sanitized formatter diagnostics.
fn transform_failure_message(error: &TransformError) -> String {
    let summary = match error {
        TransformError::InvalidRange { .. } => "Format failed: selection is not a valid byte range",
        TransformError::Spawn(_) => "Format failed: formatter could not be started",
        TransformError::Process { .. } => "Format failed: formatter streams failed",
        TransformError::Exit { .. } => "Format failed: formatter exited unsuccessfully",
        TransformError::InvalidUtf8Output { .. } => {
            "Format failed: formatter output was not valid UTF-8"
        }
        TransformError::StaleSource => "Format failed: the buffer changed during formatting",
    };
    match error.stderr_summary() {
        Some(diagnostics) => format!("{summary}; {diagnostics}"),
        None => summary.to_owned(),
    }
}

fn split_lines(text: &str) -> Vec<CapturedLine> {
    let mut lines = Vec::new();
    let mut segments: Vec<String> = Vec::new();
    let mut endings: Vec<crate::document::LineEnding> = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let ending = match bytes[index] {
            b'\n' => Some((crate::document::LineEnding::Lf, 1)),
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                Some((crate::document::LineEnding::Crlf, 2))
            }
            b'\r' => Some((crate::document::LineEnding::Cr, 1)),
            _ => None,
        };
        match ending {
            Some((ending, width)) => {
                segments.push(text[start..index].to_owned());
                endings.push(ending);
                index += width;
                start = index;
            }
            None => {
                index += text[index..]
                    .chars()
                    .next()
                    .expect("valid UTF-8 boundary")
                    .len_utf8();
            }
        }
    }
    segments.push(text[start..].to_owned());
    for (position, segment) in segments.into_iter().enumerate() {
        lines.push(CapturedLine {
            text: segment,
            ending: endings.get(position).copied(),
        });
    }
    lines
}
