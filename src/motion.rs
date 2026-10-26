use crate::document::{Cursor, Document};
use unicode_segmentation::UnicodeSegmentation;

/// How a `f`/`F`/`t`/`T` motion locates its target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FindKind {
    FindForward,
    FindBackward,
    TillForward,
    TillBackward,
}

impl FindKind {
    pub fn key(self) -> char {
        match self {
            Self::FindForward => 'f',
            Self::FindBackward => 'F',
            Self::TillForward => 't',
            Self::TillBackward => 'T',
        }
    }

    pub fn from_key(ch: char) -> Option<Self> {
        match ch {
            'f' => Some(Self::FindForward),
            'F' => Some(Self::FindBackward),
            't' => Some(Self::TillForward),
            'T' => Some(Self::TillBackward),
            _ => None,
        }
    }

    pub fn forward(self) -> bool {
        matches!(self, Self::FindForward | Self::TillForward)
    }

    pub fn till(self) -> bool {
        matches!(self, Self::TillForward | Self::TillBackward)
    }

    /// Vim treats `f`/`t` as inclusive motions and `F`/`T` as exclusive ones.
    pub fn kind(self) -> MotionKind {
        if self.forward() {
            MotionKind::Inclusive
        } else {
            MotionKind::Exclusive
        }
    }
}

/// A supported motion, independent of the key sequence that produced it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionSpec {
    Left,
    Right,
    Down,
    Up,
    WordForward,
    BigWordForward,
    WordBackward,
    BigWordBackward,
    WordEnd,
    BigWordEnd,
    /// `cw`/`cW` on a non-blank: the end of the word under the cursor, then
    /// ordinary `e`/`E` steps for additional counts. Unlike `e`, the first step
    /// never moves past a word that already ends under the cursor.
    ChangeWordEnd,
    ChangeBigWordEnd,
    LineStart,
    FirstNonBlank,
    /// `$`: the count-th line including the current one.
    LineEnd(usize),
    /// `gg`: first line, or the explicit one-based line number.
    GotoFirstLine(Option<usize>),
    /// `G`: last line, or the explicit one-based line number.
    GotoLastLine(Option<usize>),
    Find {
        kind: FindKind,
        target: char,
        /// Repeat motions skip an immediately adjacent occurrence so they progress.
        repeat: bool,
    },
    /// `;`/`,`, expanded by the editor into a concrete [`MotionSpec::Find`].
    RepeatFind {
        reverse: bool,
    },
    /// The grapheme under the cursor, used by `s`.
    CurrentChar,
}

impl MotionSpec {
    /// The characters originally typed for this motion, used by dot repeat.
    pub fn keys(self) -> Vec<char> {
        match self {
            Self::Left => vec!['h'],
            Self::Right => vec!['l'],
            Self::Down => vec!['j'],
            Self::Up => vec!['k'],
            Self::WordForward => vec!['w'],
            Self::BigWordForward => vec!['W'],
            Self::WordBackward => vec!['b'],
            Self::BigWordBackward => vec!['B'],
            Self::WordEnd => vec!['e'],
            Self::BigWordEnd => vec!['E'],
            // The change-word motions replay as `cw`/`cW` so the exception is
            // re-evaluated at the new cursor.
            Self::ChangeWordEnd => vec!['w'],
            Self::ChangeBigWordEnd => vec!['W'],
            Self::LineStart => vec!['0'],
            Self::FirstNonBlank => vec!['^'],
            Self::LineEnd(_) => vec!['$'],
            Self::GotoFirstLine(_) => vec!['g', 'g'],
            Self::GotoLastLine(_) => vec!['G'],
            Self::Find { kind, target, .. } => vec![kind.key(), target],
            Self::RepeatFind { reverse } => vec![if reverse { ',' } else { ';' }],
            Self::CurrentChar => vec![],
        }
    }

    pub fn from_key(ch: char) -> Option<Self> {
        match ch {
            'h' => Some(Self::Left),
            'l' => Some(Self::Right),
            'j' => Some(Self::Down),
            'k' => Some(Self::Up),
            'w' => Some(Self::WordForward),
            'W' => Some(Self::BigWordForward),
            'b' => Some(Self::WordBackward),
            'B' => Some(Self::BigWordBackward),
            'e' => Some(Self::WordEnd),
            'E' => Some(Self::BigWordEnd),
            '^' => Some(Self::FirstNonBlank),
            '$' => Some(Self::LineEnd(1)),
            '0' => Some(Self::LineStart),
            'G' => Some(Self::GotoLastLine(None)),
            ';' => Some(Self::RepeatFind { reverse: false }),
            ',' => Some(Self::RepeatFind { reverse: true }),
            _ => None,
        }
    }
}

/// Range classification produced by a motion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MotionOutcome {
    /// Endpoint before Normal-cursor clamping; it may be the insertion boundary
    /// after the last grapheme, or the destination row of a vertical motion.
    pub target: Cursor,
    pub kind: MotionKind,
}

/// Ordered operator range: characterwise half-open positions or logical rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionRange {
    Char { start: Cursor, end: Cursor },
    Line { first: usize, last: usize },
}

impl MotionRange {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Char { start, end } => start == end,
            Self::Line { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WordClass {
    Blank,
    Keyword,
    Punct,
    Empty,
}

fn classify(grapheme: &str) -> WordClass {
    let Some(first) = grapheme.chars().next() else {
        return WordClass::Empty;
    };
    if first.is_whitespace() {
        WordClass::Blank
    } else if first.is_alphanumeric() || first == '_' {
        WordClass::Keyword
    } else {
        WordClass::Punct
    }
}

/// Both keyword and punctuation graphemes are word cells. Small words separate
/// them into distinct runs; `W`/`B`/`E` treat a whole non-blank run as one WORD,
/// which `same_run` decides.
fn is_word(class: WordClass) -> bool {
    matches!(class, WordClass::Keyword | WordClass::Punct)
}

fn same_run(left: WordClass, right: WordClass, big: bool) -> bool {
    if is_word(left) && is_word(right) {
        big || left == right
    } else {
        false
    }
}

#[derive(Clone, Copy)]
struct Cell {
    pos: Cursor,
    class: WordClass,
}

/// Flat grapheme stream used by the word motions. Line breaks act as whitespace
/// and an empty line contributes a single stop cell at column zero.
fn cells(document: &Document) -> Vec<Cell> {
    let mut cells = Vec::new();
    for row in 0..document.line_count() {
        let Some(line) = document.line(row) else {
            continue;
        };
        if line.is_empty() {
            cells.push(Cell {
                pos: Cursor::new(row, 0),
                class: WordClass::Empty,
            });
            continue;
        }
        for (byte, grapheme) in line.grapheme_indices(true) {
            cells.push(Cell {
                pos: Cursor::new(row, byte),
                class: classify(grapheme),
            });
        }
    }
    cells
}

fn cell_index(cells: &[Cell], cursor: Cursor) -> Option<usize> {
    if cells.is_empty() {
        return None;
    }
    let exact = cells.iter().position(|cell| cell.pos == cursor);
    if exact.is_some() {
        return exact;
    }
    let before = cells
        .iter()
        .rposition(|cell| cell.pos.row == cursor.row && cell.pos.column <= cursor.column);
    before.or_else(|| {
        cells
            .iter()
            .position(|cell| cell.pos.row >= cursor.row)
            .or(Some(cells.len() - 1))
    })
}

fn word_forward(cells: &[Cell], index: usize, big: bool) -> Option<usize> {
    let mut next = index + 1;
    if next >= cells.len() {
        return None;
    }
    if is_word(cells[index].class) {
        while next < cells.len()
            && cells[next].pos.row == cells[index].pos.row
            && same_run(cells[next].class, cells[index].class, big)
        {
            next += 1;
        }
    }
    while next < cells.len() && cells[next].class == WordClass::Blank {
        next += 1;
    }
    (next < cells.len()).then_some(next)
}

fn word_end(cells: &[Cell], index: usize, big: bool) -> Option<usize> {
    let mut next = index + 1;
    if next >= cells.len() {
        return None;
    }
    if is_word(cells[index].class)
        && cells[next].pos.row == cells[index].pos.row
        && same_run(cells[next].class, cells[index].class, big)
    {
        while next + 1 < cells.len()
            && cells[next + 1].pos.row == cells[index].pos.row
            && same_run(cells[next + 1].class, cells[index].class, big)
        {
            next += 1;
        }
        return Some(next);
    }
    while next < cells.len() && !is_word(cells[next].class) {
        next += 1;
    }
    if next >= cells.len() {
        return None;
    }
    while next + 1 < cells.len()
        && cells[next + 1].pos.row == cells[next].pos.row
        && same_run(cells[next + 1].class, cells[next].class, big)
    {
        next += 1;
    }
    Some(next)
}

fn word_backward(cells: &[Cell], index: usize, big: bool) -> Option<usize> {
    if index == 0 {
        return None;
    }
    let mut next = index - 1;
    loop {
        if is_word(cells[next].class) {
            break;
        }
        // A blank line is a word stop: `b` lands on it, and further counts step
        // from there to the previous stop.
        if cells[next].class == WordClass::Empty {
            return Some(next);
        }
        if next == 0 {
            return None;
        }
        next -= 1;
    }
    while next > 0
        && cells[next - 1].pos.row == cells[next].pos.row
        && same_run(cells[next - 1].class, cells[next].class, big)
    {
        next -= 1;
    }
    Some(next)
}

/// `cw`/`cW` endpoint: the end of the word under the cursor, then ordinary
/// word-end steps for the remaining count.
fn word_change_end(
    document: &Document,
    from: Cursor,
    count: usize,
    big: bool,
) -> Option<MotionOutcome> {
    let cells = cells(document);
    let mut index = cell_index(&cells, from)?;
    if !is_word(cells[index].class) {
        return None;
    }
    while index + 1 < cells.len()
        && cells[index + 1].pos.row == cells[index].pos.row
        && same_run(cells[index + 1].class, cells[index].class, big)
    {
        index += 1;
    }
    for _ in 1..count.max(1) {
        match word_end(&cells, index, big) {
            Some(next) => index = next,
            None => break,
        }
    }
    Some(MotionOutcome {
        target: cells[index].pos,
        kind: MotionKind::Inclusive,
    })
}

type WordStep = fn(&[Cell], usize, bool) -> Option<usize>;

fn word_motion(
    document: &Document,
    from: Cursor,
    count: usize,
    spec: MotionSpec,
) -> Option<MotionOutcome> {
    let cells = cells(document);
    let mut index = cell_index(&cells, from)?;
    let (big, step): (bool, WordStep) = match spec {
        MotionSpec::WordForward => (false, word_forward),
        MotionSpec::BigWordForward => (true, word_forward),
        MotionSpec::WordBackward => (false, word_backward),
        MotionSpec::BigWordBackward => (true, word_backward),
        MotionSpec::WordEnd => (false, word_end),
        MotionSpec::BigWordEnd => (true, word_end),
        _ => return None,
    };
    let mut moved = false;
    let mut exhausted = false;
    for _ in 0..count.max(1) {
        match step(&cells, index, big) {
            Some(next) => {
                index = next;
                moved = true;
            }
            None => {
                exhausted = true;
                break;
            }
        }
    }
    let inclusive = matches!(spec, MotionSpec::WordEnd | MotionSpec::BigWordEnd);
    let forward_word = matches!(spec, MotionSpec::WordForward | MotionSpec::BigWordForward);
    if forward_word && exhausted {
        // With no further word stop, Vim's endpoint is the end of the last line
        // with an exclusive range, so `dw`/`d99w` consume the trailing word (and
        // its trailing blanks) instead of failing on it. A bare `w` still clamps
        // to the last grapheme.
        let row = cells[index].pos.row;
        return Some(MotionOutcome {
            target: Cursor::new(row, document.line_len_bytes(row)),
            kind: MotionKind::Exclusive,
        });
    }
    if !(moved || inclusive && is_word(cells[index].class)) {
        return None;
    }
    let kind = if inclusive {
        MotionKind::Inclusive
    } else {
        MotionKind::Exclusive
    };
    Some(MotionOutcome {
        target: cells[index].pos,
        kind,
    })
}

fn horizontal_forward(document: &Document, from: Cursor, count: usize) -> Option<MotionOutcome> {
    let line_len = document.line_len_bytes(from.row);
    let mut column = from.column;
    for _ in 0..count.max(1) {
        if column >= line_len {
            break;
        }
        column = document.grapheme_end(from.row, column);
    }
    if column == from.column {
        return None;
    }
    Some(MotionOutcome {
        target: Cursor::new(from.row, column),
        kind: MotionKind::Exclusive,
    })
}

fn horizontal_backward(document: &Document, from: Cursor, count: usize) -> Option<MotionOutcome> {
    let mut column = from.column;
    for _ in 0..count.max(1) {
        match document.previous_grapheme(from.row, column) {
            Some(range) => column = range.start,
            None => break,
        }
    }
    if column == from.column {
        return None;
    }
    Some(MotionOutcome {
        target: Cursor::new(from.row, column),
        kind: MotionKind::Exclusive,
    })
}

fn vertical(document: &Document, from: Cursor, delta: isize) -> Option<MotionOutcome> {
    let last = document.line_count().saturating_sub(1) as isize;
    let target_row = (from.row as isize + delta).clamp(0, last) as usize;
    if target_row == from.row {
        return None;
    }
    let visual = document.visual_column(from.row, from.column);
    let byte = document.byte_at_visual_column(target_row, visual);
    Some(MotionOutcome {
        target: Cursor::new(target_row, document.normal_cursor_col(target_row, byte)),
        kind: MotionKind::Linewise,
    })
}

fn line_target(document: &Document, row: Option<usize>, first: bool) -> Cursor {
    let last = document.line_count().saturating_sub(1);
    let row = match row {
        Some(number) => number.saturating_sub(1).min(last),
        None => {
            if first {
                0
            } else {
                last
            }
        }
    };
    Cursor::new(row, document.first_nonblank_col(row))
}

/// Resolves a motion to its endpoint and range classification.
///
/// Word and horizontal motions clamp at the end of the available text; a motion
/// that cannot make any progress at all returns `None` so operators cancel
/// without mutating the document.
pub fn resolve(
    document: &Document,
    from: Cursor,
    spec: MotionSpec,
    count: usize,
) -> Option<MotionOutcome> {
    let count = count.max(1);
    match spec {
        MotionSpec::Left => horizontal_backward(document, from, count),
        MotionSpec::Right => horizontal_forward(document, from, count),
        MotionSpec::Down => vertical(document, from, count as isize),
        MotionSpec::Up => vertical(document, from, -(count as isize)),
        MotionSpec::WordForward
        | MotionSpec::BigWordForward
        | MotionSpec::WordBackward
        | MotionSpec::BigWordBackward
        | MotionSpec::WordEnd
        | MotionSpec::BigWordEnd => word_motion(document, from, count, spec),
        MotionSpec::ChangeWordEnd => word_change_end(document, from, count, false),
        MotionSpec::ChangeBigWordEnd => word_change_end(document, from, count, true),
        MotionSpec::LineStart => Some(MotionOutcome {
            target: Cursor::new(from.row, 0),
            kind: MotionKind::Exclusive,
        }),
        MotionSpec::FirstNonBlank => Some(MotionOutcome {
            target: Cursor::new(from.row, document.first_nonblank_col(from.row)),
            kind: MotionKind::Exclusive,
        }),
        MotionSpec::LineEnd(lines) => {
            let last = document.line_count().saturating_sub(1);
            let row = from.row.saturating_add(lines.saturating_sub(1)).min(last);
            Some(MotionOutcome {
                target: Cursor::new(row, document.last_grapheme_col(row)),
                kind: MotionKind::Inclusive,
            })
        }
        MotionSpec::GotoFirstLine(number) => Some(MotionOutcome {
            target: line_target(document, number, true),
            kind: MotionKind::Linewise,
        }),
        MotionSpec::GotoLastLine(number) => Some(MotionOutcome {
            target: line_target(document, number, false),
            kind: MotionKind::Linewise,
        }),
        MotionSpec::Find {
            kind,
            target,
            repeat,
        } => {
            let line = document.line(from.row)?;
            let graphemes: Vec<(usize, &str)> = line.grapheme_indices(true).collect();
            let indexes: Vec<usize> = if kind.forward() {
                (0..graphemes.len())
                    .filter(|index| graphemes[*index].0 > from.column)
                    .collect()
            } else {
                (0..graphemes.len())
                    .rev()
                    .filter(|index| graphemes[*index].0 < from.column)
                    .collect()
            };
            let mut candidates: Vec<Cursor> = Vec::new();
            for index in indexes {
                if !graphemes[index].1.contains(target) {
                    continue;
                }
                let column = if kind.till() {
                    if kind.forward() {
                        if index == 0 {
                            continue;
                        }
                        graphemes[index - 1].0
                    } else {
                        match graphemes.get(index + 1) {
                            Some((byte, _)) => *byte,
                            None => continue,
                        }
                    }
                } else {
                    graphemes[index].0
                };
                let candidate = Cursor::new(from.row, column);
                // Repeats skip an adjacent occurrence that would not progress;
                // a first command may legitimately land on the current grapheme.
                if repeat && candidate == from {
                    continue;
                }
                candidates.push(candidate);
            }
            let cursor = *candidates.get(count.saturating_sub(1))?;
            Some(MotionOutcome {
                target: cursor,
                kind: kind.kind(),
            })
        }
        MotionSpec::RepeatFind { .. } | MotionSpec::CurrentChar => None,
    }
}

fn after_grapheme(document: &Document, pos: Cursor) -> Cursor {
    Cursor::new(pos.row, document.grapheme_end(pos.row, pos.column))
}

/// Converts a resolved motion into the ordered operator range.
///
/// Exclusive motions ending in column zero of a later row stop at the end of the
/// previous line instead of consuming the line break, which is Vim's documented
/// adjustment and is what makes `dw` on the last word of a line keep the break.
pub fn range_for(document: &Document, from: Cursor, outcome: MotionOutcome) -> MotionRange {
    match outcome.kind {
        MotionKind::Linewise => MotionRange::Line {
            first: from.row.min(outcome.target.row),
            last: from.row.max(outcome.target.row),
        },
        MotionKind::Inclusive => {
            let (start, end) = if outcome.target < from {
                (outcome.target, from)
            } else {
                (from, outcome.target)
            };
            MotionRange::Char {
                start,
                end: after_grapheme(document, end),
            }
        }
        MotionKind::Exclusive => {
            if outcome.target > from {
                if outcome.target.column == 0 && outcome.target.row > from.row {
                    // Vim's exclusive-to-linewise conversion: when the start is at
                    // or before the first non-blank and the destination is more
                    // than one row below, whole lines are affected. With a single
                    // crossed line the range instead stops at the previous line's
                    // end, which is what keeps `dw` on a last word from eating the
                    // line break.
                    let at_first_nonblank = from.column <= document.first_nonblank_col(from.row);
                    if at_first_nonblank && outcome.target.row > from.row + 1 {
                        return MotionRange::Line {
                            first: from.row,
                            last: outcome.target.row - 1,
                        };
                    }
                    let row = outcome.target.row - 1;
                    MotionRange::Char {
                        start: from,
                        end: Cursor::new(row, document.line_len_bytes(row)),
                    }
                } else {
                    MotionRange::Char {
                        start: from,
                        end: outcome.target,
                    }
                }
            } else {
                // A backward exclusive motion that ends in column zero of an
                // earlier row becomes linewise when the cursor itself sits in
                // column zero; Vim keeps it charwise for an indented cursor.
                if outcome.target.column == 0 && outcome.target.row < from.row && from.column == 0 {
                    return MotionRange::Line {
                        first: outcome.target.row,
                        last: from.row - 1,
                    };
                }
                MotionRange::Char {
                    start: outcome.target,
                    end: from,
                }
            }
        }
    }
}

/// Characterwise half-open range covering up to `count` graphemes from `at`,
/// clamped to the end of the line.
pub fn grapheme_range(document: &Document, at: Cursor, count: usize) -> MotionRange {
    let line_len = document.line_len_bytes(at.row);
    let mut column = at.column;
    for _ in 0..count.max(1) {
        if column >= line_len {
            break;
        }
        column = document.grapheme_end(at.row, column);
    }
    MotionRange::Char {
        start: at,
        end: Cursor::new(at.row, column),
    }
}

/// Graphemes available from `at` to the end of its line.
pub fn graphemes_remaining(document: &Document, at: Cursor) -> usize {
    document
        .grapheme_count(at.row)
        .saturating_sub(document.grapheme_index(at.row, at.column))
}
