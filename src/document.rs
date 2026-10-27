use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// A single line terminator as it appeared in the original file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineEnding {
    Lf,
    Crlf,
    Cr,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

/// A row plus a UTF-8 byte offset into the visible line text, excluding the BOM
/// and the line terminator. Offsets are kept on grapheme boundaries by every
/// editing operation; `column == line_len` is the insertion boundary at the end
/// of the line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
}

impl Cursor {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Line {
    text: String,
    ending: Option<LineEnding>,
}

/// One captured logical line of a linewise register or put payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedLine {
    pub text: String,
    pub ending: Option<LineEnding>,
}

impl CapturedLine {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ending: None,
        }
    }
}

/// The typed unnamed register.
///
/// Characterwise contents keep the exact source bytes, including internal line
/// terminators. Linewise contents keep the ordered logical lines with their
/// original internal terminators; the final captured line may have none when the
/// source's last line had no terminator. An empty register is distinct from a
/// register holding empty text.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Register {
    #[default]
    Empty,
    Characterwise(String),
    Linewise(Vec<CapturedLine>),
}

impl Register {
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    pub fn is_linewise(&self) -> bool {
        matches!(self, Self::Linewise(_))
    }

    /// Characterwise payload text, for tests and status displays.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Characterwise(text) => Some(text),
            _ => None,
        }
    }

    /// Linewise payload lines, for tests and status displays.
    pub fn lines(&self) -> Option<&[CapturedLine]> {
        match self {
            Self::Linewise(lines) => Some(lines),
            _ => None,
        }
    }
}

/// A UTF-8 document preserving each original line terminator and initial BOM.
///
/// Structure invariant after every edit: at least one line exists, every line
/// except the last has a terminator, and the last line has none. A file that ends
/// with a terminator therefore has a real empty final line, never a phantom
/// newline. Editing is grapheme-aware; this in-memory representation is not
/// optimized for very large files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    lines: Vec<Line>,
    bom: bool,
    insertion_ending: LineEnding,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            lines: vec![Line {
                text: String::new(),
                ending: None,
            }],
            bom: false,
            insertion_ending: LineEnding::Lf,
        }
    }

    /// Parses loaded file text, recognizing an initial BOM and removing it from
    /// the editable text.
    ///
    /// Use [`Document::from_editable_text`] for text whose BOM status is already
    /// known, so a formatter-emitted leading U+FEFF stays editable text.
    pub fn from_text(text: &str) -> Self {
        let bom = text.starts_with('\u{feff}');
        let text = if bom {
            &text['\u{feff}'.len_utf8()..]
        } else {
            text
        };
        Self::from_editable_text(text, bom)
    }

    /// Builds a document from BOM-free editable text plus an explicit BOM flag.
    ///
    /// Never strips or infers an initial U+FEFF: a leading U+FEFF in `text` is
    /// editable text at offset zero, even when `has_bom` is also true.
    pub fn from_editable_text(text: &str, has_bom: bool) -> Self {
        let mut document = Self::parse_editable_text(text);
        document.bom = has_bom;
        document
    }

    fn parse_editable_text(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        let mut i = 0;
        let mut first_ending = None;
        while i < text.len() {
            let b = text.as_bytes()[i];
            let ending = match b {
                b'\n' => Some((LineEnding::Lf, 1)),
                b'\r' if text.as_bytes().get(i + 1) == Some(&b'\n') => Some((LineEnding::Crlf, 2)),
                b'\r' => Some((LineEnding::Cr, 1)),
                _ => None,
            };
            if let Some((ending, width)) = ending {
                lines.push(Line {
                    text: text[start..i].to_owned(),
                    ending: Some(ending),
                });
                first_ending.get_or_insert(ending);
                i += width;
                start = i;
            } else {
                let ch = text[i..].chars().next().expect("valid UTF-8 boundary");
                i += ch.len_utf8();
            }
        }
        lines.push(Line {
            text: text[start..].to_owned(),
            ending: None,
        });
        Self {
            lines,
            bom: false,
            insertion_ending: first_ending.unwrap_or(LineEnding::Lf),
        }
    }

    /// The document's text without the initial file BOM, byte for byte as it
    /// would be serialized after a processed write.
    pub fn editable_text(&self) -> String {
        let mut text = String::new();
        self.write_editable_text(&mut text);
        text
    }

    fn write_editable_text(&self, text: &mut String) {
        for line in &self.lines {
            text.push_str(&line.text);
            if let Some(ending) = line.ending {
                text.push_str(ending.as_str());
            }
        }
    }

    /// Serialized file bytes: the initial BOM when present, then editable text.
    pub fn serialize(&self) -> String {
        let mut result = String::new();
        if self.bom {
            result.push('\u{feff}');
        }
        self.write_editable_text(&mut result);
        result
    }

    /// Byte offset of `cursor` into [`Document::editable_text`], counting the
    /// exact stored terminator of every preceding line.
    ///
    /// Out-of-range rows and columns clamp to the nearest position, so the result
    /// is always a valid offset into the editable text.
    pub fn byte_offset(&self, cursor: Cursor) -> usize {
        let row = cursor.row.min(self.lines.len().saturating_sub(1));
        let mut offset = 0;
        for line in self.lines.iter().take(row) {
            offset += line.text.len() + line.ending.map_or(0, |ending| ending.as_str().len());
        }
        offset + self.clamp_boundary(row, cursor.column)
    }

    /// Cursor at or before a byte offset into [`Document::editable_text`].
    ///
    /// The column is always on a grapheme boundary and may be the insertion
    /// boundary at the end of a line. An offset inside a line terminator maps to
    /// the end of that line, before the terminator; an offset past the end maps to
    /// the final line's end.
    pub fn cursor_at_byte_offset(&self, offset: usize) -> Cursor {
        let mut remaining = offset;
        for (row, line) in self.lines.iter().enumerate() {
            if remaining <= line.text.len() {
                return Cursor::new(row, self.clamp_boundary(row, remaining));
            }
            let terminator = line.ending.map_or(0, |ending| ending.as_str().len());
            if remaining < line.text.len() + terminator {
                return Cursor::new(row, line.text.len());
            }
            remaining -= line.text.len() + terminator;
        }
        let last = self.lines.len().saturating_sub(1);
        Cursor::new(last, self.line_len_bytes(last))
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
    pub fn line(&self, row: usize) -> Option<&str> {
        self.lines.get(row).map(|line| line.text.as_str())
    }
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|line| line.text.as_str())
    }
    pub fn has_bom(&self) -> bool {
        self.bom
    }
    pub fn insertion_line_ending(&self) -> &'static str {
        self.insertion_ending.as_str()
    }
    pub fn insertion_ending(&self) -> LineEnding {
        self.insertion_ending
    }
    pub fn line_len_bytes(&self, row: usize) -> usize {
        self.line(row).map_or(0, str::len)
    }
    pub fn line_ending(&self, row: usize) -> Option<LineEnding> {
        self.lines.get(row).and_then(|line| line.ending)
    }

    pub fn grapheme_boundaries(&self, row: usize) -> Vec<usize> {
        let Some(line) = self.line(row) else {
            return Vec::new();
        };
        let mut bounds: Vec<_> = line.grapheme_indices(true).map(|(i, _)| i).collect();
        bounds.push(line.len());
        bounds
    }

    pub fn clamp_boundary(&self, row: usize, byte: usize) -> usize {
        if self.line(row).is_none() {
            return 0;
        }
        self.grapheme_boundaries(row)
            .into_iter()
            .take_while(|&i| i <= byte)
            .last()
            .unwrap_or(0)
    }

    /// Moves a byte offset to the next grapheme boundary, or leaves it at the line end.
    pub fn next_boundary(&self, row: usize, byte: usize) -> usize {
        self.grapheme_boundaries(row)
            .into_iter()
            .find(|&i| i >= byte)
            .unwrap_or_else(|| self.line_len_bytes(row))
    }

    /// Renders one grapheme without sending file control characters to the terminal.
    /// Tabs expand to four-cell stops; ASCII controls use caret notation.
    pub fn display_grapheme(grapheme: &str, visual_column: usize) -> String {
        if grapheme == "\t" {
            return " ".repeat(4 - visual_column % 4);
        }
        let mut output = String::new();
        for ch in grapheme.chars() {
            let value = ch as u32;
            if value < 0x20 {
                output.push('^');
                output.push(char::from_u32(value + 0x40).unwrap_or('?'));
            } else if value == 0x7f {
                output.push_str("^?");
            } else if ch.is_control() {
                output.push('�');
            } else {
                output.push(ch);
            }
        }
        output
    }

    /// Display-cell width of [`Document::display_grapheme`] at this visual column.
    pub fn display_grapheme_width(grapheme: &str, visual_column: usize) -> usize {
        Self::display_grapheme(grapheme, visual_column).width()
    }

    pub fn previous_grapheme(&self, row: usize, byte: usize) -> Option<std::ops::Range<usize>> {
        let line = self.line(row)?;
        line.grapheme_indices(true)
            .rfind(|(i, _)| *i < byte)
            .map(|(i, g)| i..i + g.len())
    }

    pub fn current_grapheme(&self, row: usize, byte: usize) -> Option<std::ops::Range<usize>> {
        let line = self.line(row)?;
        line.grapheme_indices(true)
            .find(|(i, _)| *i >= byte)
            .map(|(i, g)| i..i + g.len())
    }

    /// Byte offset of the grapheme that starts at or before `byte`.
    pub fn grapheme_start(&self, row: usize, byte: usize) -> usize {
        self.clamp_boundary(row, byte)
    }

    /// Byte offset just past the grapheme starting at `byte`, clamped to the line end.
    pub fn grapheme_end(&self, row: usize, byte: usize) -> usize {
        self.current_grapheme(row, byte)
            .map_or_else(|| self.line_len_bytes(row), |range| range.end)
    }

    /// Zero-based grapheme ordinal of the grapheme starting at `byte`.
    pub fn grapheme_index(&self, row: usize, byte: usize) -> usize {
        let Some(line) = self.line(row) else {
            return 0;
        };
        line.grapheme_indices(true)
            .take_while(|(i, _)| *i < byte)
            .count()
    }

    /// Byte offset of the grapheme at `index`, clamped to the last grapheme.
    pub fn byte_at_grapheme(&self, row: usize, index: usize) -> usize {
        let Some(line) = self.line(row) else {
            return 0;
        };
        line.grapheme_indices(true)
            .nth(index)
            .map_or_else(|| self.normal_cursor_col(row, line.len()), |(i, _)| i)
    }

    pub fn grapheme_count(&self, row: usize) -> usize {
        self.line(row)
            .map_or(0, |line| line.graphemes(true).count())
    }

    pub fn visual_column(&self, row: usize, byte: usize) -> usize {
        let Some(line) = self.line(row) else {
            return 0;
        };
        let mut col = 0;
        for (i, grapheme) in line.grapheme_indices(true) {
            if i >= byte {
                break;
            }
            col += Self::display_grapheme_width(grapheme, col);
        }
        col
    }

    pub fn byte_at_visual_column(&self, row: usize, target: usize) -> usize {
        let Some(line) = self.line(row) else {
            return 0;
        };
        let mut col = 0;
        for (i, grapheme) in line.grapheme_indices(true) {
            let width = Self::display_grapheme_width(grapheme, col);
            if col + width > target {
                return i;
            }
            col += width;
            if col >= target {
                return i + grapheme.len();
            }
        }
        line.len()
    }

    /// The Normal/Visual cursor position for a byte offset: an existing grapheme
    /// start, never the insertion boundary at the end of a non-empty line.
    pub fn normal_cursor_col(&self, row: usize, byte: usize) -> usize {
        let Some(line) = self.line(row) else {
            return 0;
        };
        if line.is_empty() {
            return 0;
        }
        line.grapheme_indices(true)
            .take_while(|(i, _)| *i <= byte)
            .last()
            .map_or(0, |(i, _)| i)
    }

    /// Byte offset of the last grapheme of the row, or zero for an empty line.
    pub fn last_grapheme_col(&self, row: usize) -> usize {
        self.normal_cursor_col(row, self.line_len_bytes(row).saturating_sub(1))
    }

    /// Byte offset of the first non-blank grapheme; a blank-only line yields its
    /// last grapheme (Vim's `^` behaviour).
    pub fn first_nonblank_col(&self, row: usize) -> usize {
        let Some(line) = self.line(row) else {
            return 0;
        };
        line.grapheme_indices(true)
            .find(|(_, grapheme)| !grapheme.starts_with([' ', '\t']))
            .map_or_else(|| self.last_grapheme_col(row), |(i, _)| i)
    }

    /// Normalizes user-typed or pasted line breaks to the document's insertion ending.
    ///
    /// Break bytes are canonicalized to LF first and then replaced once, so a
    /// CRLF policy is never applied to its own output and CR/CRLF input cannot
    /// grow extra break bytes.
    pub fn normalize_breaks(&self, text: &str) -> String {
        if !text.contains(['\r', '\n']) {
            return text.to_owned();
        }
        let canonical = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.insertion_ending {
            LineEnding::Lf => canonical,
            ending => canonical.replace('\n', ending.as_str()),
        }
    }

    /// Raw text between two normalized half-open positions, terminators included.
    pub fn text_between(&self, start: Cursor, end: Cursor) -> String {
        let (start, end) = ordered(start, end);
        let start_col = self.clamp_boundary(start.row, start.column);
        let end_col = self.clamp_boundary(end.row, end.column);
        if start.row == end.row {
            return self.lines[start.row].text[start_col..end_col].to_owned();
        }
        let mut out = String::new();
        out.push_str(&self.lines[start.row].text[start_col..]);
        out.push_str(
            self.lines[start.row]
                .ending
                .unwrap_or(self.insertion_ending)
                .as_str(),
        );
        for row in start.row + 1..end.row {
            out.push_str(&self.lines[row].text);
            out.push_str(
                self.lines[row]
                    .ending
                    .unwrap_or(self.insertion_ending)
                    .as_str(),
            );
        }
        out.push_str(&self.lines[end.row].text[..end_col]);
        out
    }

    /// Captured logical lines for a linewise range, terminators preserved.
    pub fn captured_lines(&self, first: usize, last: usize) -> Vec<CapturedLine> {
        if self.lines.is_empty() {
            return Vec::new();
        }
        let first = first.min(self.lines.len() - 1);
        let last = last.min(self.lines.len() - 1).max(first);
        (first..=last)
            .map(|row| CapturedLine {
                text: self.lines[row].text.clone(),
                ending: self.lines[row].ending,
            })
            .collect()
    }

    /// Replaces the half-open character range between `start` and `end` with raw
    /// `replacement` text. Break bytes in `replacement` are preserved verbatim;
    /// callers normalize user input first. Returns the position at the end of the
    /// inserted text, which is an insertion boundary.
    pub fn replace_text_between(
        &mut self,
        start: Cursor,
        end: Cursor,
        replacement: &str,
    ) -> Cursor {
        let (start, end) = ordered(start, end);
        let start_col = self.clamp_boundary(start.row, start.column);
        let end_col = self.clamp_boundary(end.row, end.column);
        let prefix = self.lines[start.row].text[..start_col].to_owned();
        let suffix = self.lines[end.row].text[end_col..].to_owned();
        let end_ending = self.lines[end.row].ending;
        let mut segments: Vec<String> = Vec::new();
        let mut terminators: Vec<LineEnding> = Vec::new();
        split_breaks(replacement, &mut segments, &mut terminators);

        let mut new_lines: Vec<Line> = Vec::with_capacity(segments.len());
        for (index, segment) in segments.iter().enumerate() {
            let text = if index == 0 {
                format!("{prefix}{segment}")
            } else {
                segment.clone()
            };
            let ending = if index + 1 < segments.len() {
                Some(terminators[index])
            } else {
                end_ending
            };
            new_lines.push(Line { text, ending });
        }
        let last_segment_len = segments.last().map_or(0, String::len);
        let first_offset = if segments.len() == 1 { prefix.len() } else { 0 };
        if let Some(last) = new_lines.last_mut() {
            last.text.push_str(&suffix);
        }
        let cursor = Cursor {
            row: start.row + segments.len() - 1,
            column: first_offset + last_segment_len,
        };
        self.lines.splice(start.row..=end.row, new_lines);
        self.repair();
        Cursor {
            row: cursor.row.min(self.lines.len() - 1),
            column: self.clamp_boundary(cursor.row.min(self.lines.len() - 1), cursor.column),
        }
    }

    /// Replaces the inclusive logical row range with `payload` lines. Missing
    /// internal terminators are synthesized from the document's insertion ending.
    pub fn replace_line_range(
        &mut self,
        first: usize,
        last: usize,
        payload: &[CapturedLine],
    ) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let first = first.min(self.lines.len() - 1);
        let last = last.min(self.lines.len() - 1).max(first);
        if payload.is_empty() {
            self.lines.drain(first..=last);
            if self.lines.is_empty() {
                self.lines.push(Line {
                    text: String::new(),
                    ending: None,
                });
                return 0;
            }
            if first >= self.lines.len()
                && let Some(line) = self.lines.last_mut()
            {
                line.ending = None;
            }
            self.repair();
            return first.min(self.lines.len() - 1);
        }
        let lines = payload_lines(payload);
        let at_end = last + 1 >= self.lines.len();
        self.lines.splice(first..=last, lines);
        if at_end && let Some(line) = self.lines.last_mut() {
            line.ending = None;
        }
        self.repair();
        first.min(self.lines.len() - 1)
    }

    /// Inserts `payload` lines directly below `row`; returns the first inserted row.
    pub fn insert_lines_after(&mut self, row: usize, payload: &[CapturedLine]) -> usize {
        if payload.is_empty() || self.lines.is_empty() {
            return row.min(self.lines.len().saturating_sub(1));
        }
        let at = row.min(self.lines.len() - 1) + 1;
        self.lines.splice(at..at, payload_lines(payload));
        self.repair();
        at.min(self.lines.len() - 1)
    }

    /// Inserts `payload` lines directly above `row`; returns the first inserted row.
    pub fn insert_lines_before(&mut self, row: usize, payload: &[CapturedLine]) -> usize {
        if payload.is_empty() || self.lines.is_empty() {
            return row.min(self.lines.len().saturating_sub(1));
        }
        let at = row.min(self.lines.len() - 1);
        self.lines.splice(at..at, payload_lines(payload));
        self.repair();
        at
    }

    /// Inserts text at an insertion boundary, normalizing break bytes to the
    /// document policy. Used by interactive typing and paste.
    pub fn insert_text(&mut self, row: &mut usize, col: &mut usize, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let text = self.normalize_breaks(text);
        let start = Cursor {
            row: *row,
            column: *col,
        };
        let end = self.replace_text_between(start, start, &text);
        *row = end.row;
        *col = end.column;
        true
    }

    pub fn insert_newline(&mut self, row: &mut usize, col: &mut usize) -> bool {
        self.insert_text(row, col, "\n")
    }

    pub fn backspace(&mut self, row: &mut usize, col: &mut usize) -> bool {
        if self.lines.get(*row).is_none() {
            return false;
        }
        if let Some(range) = self.previous_grapheme(*row, *col) {
            self.lines[*row].text.replace_range(range.clone(), "");
            *col = self.clamp_boundary(*row, range.start);
            return true;
        }
        if *row == 0 {
            return false;
        }
        let prior_len = self.lines[*row - 1].text.len();
        let current = self.lines.remove(*row);
        self.lines[*row - 1].text.push_str(&current.text);
        self.lines[*row - 1].ending = current.ending;
        *row -= 1;
        *col = self.clamp_boundary(*row, prior_len);
        true
    }

    pub fn delete(&mut self, row: &mut usize, col: &mut usize) -> bool {
        if let Some(range) = self.current_grapheme(*row, *col) {
            self.lines[*row].text.replace_range(range, "");
            *col = self.clamp_boundary(*row, *col);
            return true;
        }
        if *row + 1 >= self.lines.len() {
            return false;
        }
        let next = self.lines.remove(*row + 1);
        self.lines[*row].text.push_str(&next.text);
        self.lines[*row].ending = next.ending;
        *col = self.clamp_boundary(*row, *col);
        true
    }

    /// Restores the structural invariant after a structural splice.
    fn repair(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(Line {
                text: String::new(),
                ending: None,
            });
            return;
        }
        let count = self.lines.len();
        for line in self.lines.iter_mut().take(count - 1) {
            if line.ending.is_none() {
                line.ending = Some(self.insertion_ending);
            }
        }
        if self.lines[count - 1].ending.is_some() {
            self.lines.push(Line {
                text: String::new(),
                ending: None,
            });
        }
    }
}

fn payload_lines(payload: &[CapturedLine]) -> Vec<Line> {
    payload
        .iter()
        .map(|line| Line {
            text: line.text.clone(),
            ending: line.ending,
        })
        .collect()
}

fn ordered(first: Cursor, second: Cursor) -> (Cursor, Cursor) {
    if first <= second {
        (first, second)
    } else {
        (second, first)
    }
}

/// Splits raw text into segments and the exact terminators between them.
fn split_breaks(text: &str, segments: &mut Vec<String>, terminators: &mut Vec<LineEnding>) {
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let ending = match bytes[i] {
            b'\n' => Some((LineEnding::Lf, 1)),
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => Some((LineEnding::Crlf, 2)),
            b'\r' => Some((LineEnding::Cr, 1)),
            _ => None,
        };
        match ending {
            Some((ending, width)) => {
                segments.push(text[start..i].to_owned());
                terminators.push(ending);
                i += width;
                start = i;
            }
            None => {
                i += text[i..]
                    .chars()
                    .next()
                    .expect("valid UTF-8 boundary")
                    .len_utf8();
            }
        }
    }
    segments.push(text[start..].to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_text_still_recognizes_an_initial_bom() {
        let document = Document::from_text("\u{feff}hello\nworld");
        assert!(document.has_bom());
        assert_eq!(document.line(0), Some("hello"));
        assert_eq!(document.editable_text(), "hello\nworld");
        assert_eq!(document.serialize(), "\u{feff}hello\nworld");
    }

    #[test]
    fn from_editable_text_never_strips_a_leading_fe_ff() {
        let document = Document::from_editable_text("\u{feff}hello", false);
        assert!(!document.has_bom());
        assert_eq!(document.line(0), Some("\u{feff}hello"));
        assert_eq!(document.editable_text(), "\u{feff}hello");
        // The leading U+FEFF is editable text, so serialization adds none.
        assert_eq!(document.serialize(), "\u{feff}hello");
    }

    #[test]
    fn from_editable_text_keeps_the_explicit_bom_flag() {
        let document = Document::from_editable_text("\u{feff}hello", true);
        assert!(document.has_bom());
        assert_eq!(document.editable_text(), "\u{feff}hello");
        assert_eq!(document.serialize(), "\u{feff}\u{feff}hello");
    }

    #[test]
    fn editable_text_preserves_exact_terminators_and_missing_final_newline() {
        let mixed = "a\r\nb\nc\rd";
        let document = Document::from_editable_text(mixed, true);
        assert_eq!(document.editable_text(), mixed);
        assert_eq!(document.serialize(), format!("\u{feff}{mixed}"));
        assert_eq!(document.line_count(), 4);

        let trailing_break = Document::from_editable_text("a\n", false);
        assert_eq!(trailing_break.line_count(), 2);
        assert_eq!(trailing_break.line(1), Some(""));
        assert_eq!(trailing_break.editable_text(), "a\n");
    }

    #[test]
    fn byte_offset_counts_stored_terminators() {
        let document = Document::from_editable_text("ab\r\ncd\n\nef", false);
        assert_eq!(document.line_count(), 4);
        assert_eq!(document.byte_offset(Cursor::new(0, 0)), 0);
        assert_eq!(document.byte_offset(Cursor::new(0, 1)), 1);
        assert_eq!(document.byte_offset(Cursor::new(0, 2)), 2);
        assert_eq!(document.byte_offset(Cursor::new(1, 0)), 4);
        assert_eq!(document.byte_offset(Cursor::new(2, 0)), 7);
        assert_eq!(document.byte_offset(Cursor::new(3, 0)), 8);
        assert_eq!(document.byte_offset(Cursor::new(3, 2)), 10);
        // Out-of-range positions clamp to the nearest valid offset.
        assert_eq!(document.byte_offset(Cursor::new(0, 99)), 2);
        assert_eq!(document.byte_offset(Cursor::new(99, 0)), 8);
    }

    #[test]
    fn cursor_at_byte_offset_maps_graphemes_and_terminators() {
        let text = "a👩‍👩‍👧‍👦b\r\ncd";
        let document = Document::from_editable_text(text, false);
        assert_eq!(text.len(), 31);
        assert_eq!(document.cursor_at_byte_offset(0), Cursor::new(0, 0));
        assert_eq!(document.cursor_at_byte_offset(1), Cursor::new(0, 1));
        // Inside the emoji grapheme: clamped back to its start.
        assert_eq!(document.cursor_at_byte_offset(5), Cursor::new(0, 1));
        assert_eq!(document.cursor_at_byte_offset(26), Cursor::new(0, 26));
        // End of the first line's text, immediately before the CRLF terminator.
        assert_eq!(document.cursor_at_byte_offset(27), Cursor::new(0, 27));
        // Inside the CRLF terminator: the end of that line, never a split pair.
        assert_eq!(document.cursor_at_byte_offset(28), Cursor::new(0, 27));
        assert_eq!(document.cursor_at_byte_offset(29), Cursor::new(1, 0));
        assert_eq!(document.cursor_at_byte_offset(31), Cursor::new(1, 2));
        assert_eq!(document.cursor_at_byte_offset(4096), Cursor::new(1, 2));
    }

    #[test]
    fn cursor_and_offset_round_trip_on_editable_boundaries() {
        let text = "x\r\ny\n\nz";
        let document = Document::from_editable_text(text, false);
        for offset in [0, 1, 3, 4, 5, 6, 7] {
            assert_eq!(
                document.byte_offset(document.cursor_at_byte_offset(offset)),
                offset
            );
        }
        assert_eq!(text.len(), 7);
        let empty = Document::from_editable_text("", true);
        assert_eq!(empty.editable_text(), "");
        assert_eq!(empty.serialize(), "\u{feff}");
        assert_eq!(empty.byte_offset(empty.cursor_at_byte_offset(0)), 0);
    }
}
