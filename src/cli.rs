use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::document::Document;
use crate::storage::{self, DiskContents, Observation};
use crate::transform::{FormatterSpec, TransformSource};

/// The command-line actions supported by Postvim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    Help,
    Version,
    Edit(Option<PathBuf>),
    Format(FormatInvocation),
}

/// A half-open UTF-8 byte range into BOM-free editable document text.
///
/// Offsets count bytes, not display columns or characters, and the end offset is
/// exclusive. This is the same range type the transformation library validates,
/// so a parsed `--range` obeys exactly the rules a Visual selection conversion
/// does.
pub use crate::transform::ByteRange;

/// An independent `--format` run.
///
/// Paths and formatter argv stay OS-native. `input` is `None` when the document
/// comes from standard input; the program and its arguments are passed to the
/// formatter literally, with no shell interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatInvocation {
    pub input: Option<PathBuf>,
    pub range: Option<ByteRange>,
    pub write: bool,
    pub stdout: bool,
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl FormatInvocation {
    /// Whether this run replaces the input file instead of only reporting text.
    pub fn writes_document(&self) -> bool {
        self.write
    }

    /// Whether the full transformed document is written to standard output.
    ///
    /// Standard output is the default; `--write` drops it unless `--stdout`
    /// asks for both.
    pub fn emits_document(&self) -> bool {
        !self.write || self.stdout
    }
}

/// Help text for the bounded single-file editor workflow.
pub const HELP: &str = "Postvim — a small modal text editor\n\
Usage: postvim [--help | --version] [--] [PATH]\n\
       postvim --format [--input PATH] [--range START:END] [--write] [--stdout] -- PROGRAM [ARG...]\n\
\n\
Open PATH, or show the welcome screen when no path is supplied. A missing PATH\n\
will not be created until it is saved with :w. Use -- before a path beginning\n\
with '-'.\n\
\n\
Format mode: postvim --format runs one existing formatter and prints the full\n\
transformed document to stdout; it never edits a file unless --write is given.\n\
--format is recognized only as the first argument, so 'postvim format' still\n\
opens a file named 'format'. --input PATH reads PATH, which must exist and be a\n\
regular UTF-8 file; without --input the document is read from stdin through EOF,\n\
and '--input -' means a file literally named '-'. --range START:END selects a\n\
half-open byte range of the text, excluding any initial BOM; the default is the\n\
whole document. Offsets are bytes, not characters or screen columns. -- ends\n\
Postvim's options and is required before PROGRAM; everything after it is passed\n\
to the formatter verbatim as native arguments. PROGRAM must be nonempty. --write\n\
requires --input and atomically replaces that file only if it still matches the\n\
bytes read before the formatter ran; --write --stdout saves first and then\n\
prints the same full document. Formatter warnings and failures go to stderr.\n\
No shell is involved, so pipes, globs, variables, and redirections are literal\n\
arguments. The formatter is trusted: it runs synchronously with no timeout,\n\
sandbox, or output limit, and its own side effects cannot be undone by Postvim.\n\
\n\
Normal keys: h/j/k/l move; w/W/b/B/e/E move by word; 0/^/$ go to the line\n\
start, first non-blank, and last character; gg/G go to the buffer start/end;\n\
f/F/t/T find a character, with ;/, repeating it. Counts work and multiply\n\
(2d3w). i/I/a/A insert; o/O open a line; x/X delete; r replaces; s/S\n\
substitute; D/C delete or change to the end of the line; d/c/y take a motion or\n\
a doubled key (dd/cc/yy); p/P put. u undoes, Ctrl-r redoes, . repeats the last\n\
change. v/V start Visual mode, / and ? search, n/N repeat a search, and : opens\n\
an Ex command.\n\
\n\
Insert keys: text, Enter, Tab, Backspace, Delete, and the arrow keys. Esc\n\
returns to Normal mode; paste is inserted as text only in Insert mode.\n\
\n\
Commands: :w saves; :w PATH saves as PATH (adopted only after success); :q\n\
quits when the buffer matches its saved bytes; :q! quits and discards changes;\n\
:wq saves the active path and then quits; :x writes only when modified and then\n\
quits. :wq, :x, :q, and :q! take no path argument. A modified buffer refuses :q\n\
and reports how to save or discard it. :format PROGRAM [ARG...] runs PROGRAM on\n\
the Visual selection or the whole buffer as one undoable unsaved change; quotes\n\
group arguments but no shell expansion happens. :reload replaces a clean buffer\n\
with its file's current contents, and :reload! discards local text for the\n\
file's current contents, both as one undoable change. Neither reload command\n\
takes an argument, and neither works for an unnamed buffer.\n\
\n\
This is a bounded Vim subset, not Vim compatibility. docs/keymaps.md lists the\n\
complete key inventory and the unsupported commands.\n";

/// Parse process arguments after the executable name.
///
/// Paths remain OS-native so non-UTF-8 filenames are preserved where supported.
pub fn parse_args_os<I>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(CliAction::Edit(None));
    };

    if first == OsStr::new("--format") {
        return parse_format(args);
    }

    match first.to_str() {
        Some("--help") => {
            if args.next().is_some() {
                Err("--help must not be combined with other arguments".to_owned())
            } else {
                Ok(CliAction::Help)
            }
        }
        Some("--version") => {
            if args.next().is_some() {
                Err("--version must not be combined with other arguments".to_owned())
            } else {
                Ok(CliAction::Version)
            }
        }
        Some("--") => parse_path(args.next(), args.next()),
        Some(value) if value.starts_with('-') => Err(format!(
            "unknown option: {value}; use -- before a path beginning with '-'"
        )),
        _ => parse_path(Some(first), args.next()),
    }
}

/// Parse the options that follow a leading `--format`.
fn parse_format<I>(mut args: I) -> Result<CliAction, String>
where
    I: Iterator<Item = OsString>,
{
    let mut input = None;
    let mut range = None;
    let mut write = false;
    let mut stdout = false;

    loop {
        let Some(option) = args.next() else {
            return Err("--format needs -- before the formatter program".to_owned());
        };
        if option == OsStr::new("--") {
            break;
        }
        match option.to_str() {
            Some("--input") => {
                if input.is_some() {
                    return Err("--input may be given only once".to_owned());
                }
                let path = args
                    .next()
                    .ok_or_else(|| "--input requires a path".to_owned())?;
                input = Some(PathBuf::from(path));
            }
            Some("--range") => {
                if range.is_some() {
                    return Err("--range may be given only once".to_owned());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "--range requires START:END".to_owned())?;
                range = Some(parse_byte_range(&value)?);
            }
            Some("--write") => {
                if write {
                    return Err("--write may be given only once".to_owned());
                }
                write = true;
            }
            Some("--stdout") => {
                if stdout {
                    return Err("--stdout may be given only once".to_owned());
                }
                stdout = true;
            }
            Some(unknown) => {
                return Err(format!(
                    "unknown --format option: {unknown}; use -- before the formatter program"
                ));
            }
            None => {
                return Err(
                    "--format options must be valid Unicode; use -- before the formatter program"
                        .to_owned(),
                );
            }
        }
    }

    let mut rest = args;
    let Some(program) = rest.next() else {
        return Err("--format needs a formatter program after --".to_owned());
    };
    if program.is_empty() {
        return Err("formatter program must not be empty".to_owned());
    }
    if write && input.is_none() {
        return Err(
            "--write requires --input PATH; stdin cannot name a destination to save".to_owned(),
        );
    }

    Ok(CliAction::Format(FormatInvocation {
        input,
        range,
        write,
        stdout,
        program,
        args: rest.collect(),
    }))
}

/// Parse a decimal `START:END` byte range.
pub fn parse_byte_range(value: &OsStr) -> Result<ByteRange, String> {
    let text = value
        .to_str()
        .ok_or_else(|| "--range must be valid Unicode".to_owned())?;
    let (start, end) = text
        .split_once(':')
        .ok_or_else(|| format!("invalid --range {text:?}: expected START:END"))?;
    let start = parse_byte_offset(start, text)?;
    let end = parse_byte_offset(end, text)?;
    if start > end {
        return Err(format!(
            "invalid --range {text:?}: START must not exceed END"
        ));
    }
    Ok(ByteRange { start, end })
}

fn parse_byte_offset(part: &str, whole: &str) -> Result<usize, String> {
    if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "invalid --range {whole:?}: offsets must be decimal byte counts"
        ));
    }
    part.parse::<usize>()
        .map_err(|_| format!("invalid --range {whole:?}: offset is too large"))
}

fn parse_path(path: Option<OsString>, extra: Option<OsString>) -> Result<CliAction, String> {
    if extra.is_some() {
        return Err("expected at most one path".to_owned());
    }

    Ok(CliAction::Edit(path.map(PathBuf::from)))
}

/// Standard streams for one independent `--format` run.
///
/// They are trait objects so a caller can supply in-memory capture or a
/// deliberately failing writer instead of a terminal. The adapter never
/// initializes a terminal and needs no TTY.
pub struct FormatStreams<'a> {
    pub stdin: &'a mut dyn Read,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}

/// An operational `--format` failure.
///
/// Grammar problems are reported by [`parse_args_os`] before this type exists.
/// A `FormatError` means the run started but could not produce its artifact; the
/// message keeps the failure category ahead of any sanitized formatter
/// diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
    message: String,
}

impl FormatError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The complete, already-sanitized diagnostic for the caller to present.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FormatError {}

/// Run one independent `--format` request.
///
/// The input bytes and, for `--input PATH`, the disk observation are captured
/// together before the formatter starts. The full transformed document is the
/// artifact: standard output by default, or a guarded atomic replacement of the
/// input when `--write` asks for one. Without `--write` nothing is written to
/// disk, and no failure path emits document bytes to standard output.
pub fn run_format(
    invocation: &FormatInvocation,
    working_directory: &Path,
    streams: FormatStreams<'_>,
) -> Result<(), FormatError> {
    let FormatStreams {
        stdin,
        stdout,
        stderr,
    } = streams;
    let (raw, observation) = read_format_input(invocation, stdin)?;
    let source_name = invocation.input.as_ref().map_or_else(
        || "standard input".to_owned(),
        |path| path.display().to_string(),
    );
    let loaded = String::from_utf8(raw)
        .map_err(|_| FormatError::new(format!("{source_name} is not valid UTF-8")))?;

    // The file-loading constructor recognizes an initial BOM, so the range and
    // formatter input never include it while the artifact keeps it.
    let document = Document::from_text(&loaded);
    let source_text = document.editable_text();
    let range = invocation
        .range
        .unwrap_or_else(|| ByteRange::whole(&source_text));
    let source = TransformSource::new(source_text, document.has_bom(), range);
    // Reject a range the shared validator refuses before any process starts.
    source
        .validate()
        .map_err(|error| FormatError::new(error.to_string()))?;

    let formatter = FormatterSpec::new(invocation.program.clone())
        .args(invocation.args.clone())
        .working_directory(working_directory.to_path_buf());
    let request = crate::transform::TransformRequest::new(source, formatter);
    let result = request.execute().map_err(format_transform_error)?;

    // Successful formatter warnings are diagnostics, never document text.
    if let Some(warnings) = result.warnings() {
        let _ = writeln!(stderr, "postvim: formatter: {warnings}");
        let _ = stderr.flush();
    }

    let transformed = result.apply(&document).map_err(format_transform_error)?;
    let bytes = transformed.serialize();

    if invocation.write {
        let path = invocation
            .input
            .as_deref()
            .ok_or_else(|| FormatError::new("--write requires --input PATH"))?;
        let expected = observation.as_ref().ok_or_else(|| {
            FormatError::new("--write requires the observation of an existing input file")
        })?;
        match storage::save_document_guarded(path, bytes.as_bytes(), expected) {
            Ok(_) => {}
            Err(error) if error.is_conflict() => {
                return Err(FormatError::new(format!("not saved: {error}")));
            }
            Err(error) => {
                return Err(FormatError::new(format!(
                    "cannot save {}: {error}",
                    path.display()
                )));
            }
        }
    }

    if invocation.emits_document()
        && let Err(error) = stdout
            .write_all(bytes.as_bytes())
            .and_then(|()| stdout.flush())
    {
        return Err(if invocation.write {
            let path = invocation
                .input
                .as_ref()
                .expect("--write was validated above");
            FormatError::new(format!(
                "saved {}, but writing the transformed document to standard output failed: {error}",
                path.display()
            ))
        } else {
            FormatError::new(format!(
                "cannot write the transformed document to standard output: {error}"
            ))
        });
    }

    Ok(())
}

fn format_transform_error(error: crate::transform::TransformError) -> FormatError {
    match error.stderr_summary() {
        Some(summary) => FormatError::new(format!("{error}: {summary}")),
        None => FormatError::new(error.to_string()),
    }
}

/// Reads the request input, keeping a file's observation alongside its bytes.
///
/// A requested input path must already exist: unlike an interactive buffer,
/// `--format` will not format an empty stand-in for a missing file.
fn read_format_input(
    invocation: &FormatInvocation,
    stdin: &mut dyn Read,
) -> Result<(Vec<u8>, Option<Observation>), FormatError> {
    let Some(path) = invocation.input.as_deref() else {
        let mut bytes = Vec::new();
        stdin
            .read_to_end(&mut bytes)
            .map_err(|error| FormatError::new(format!("cannot read standard input: {error}")))?;
        return Ok((bytes, None));
    };

    let observation = storage::observe(path)
        .map_err(|error| FormatError::new(format!("{}: {error}", path.display())))?;
    let bytes = match &observation.contents {
        DiskContents::Missing => {
            return Err(FormatError::new(format!(
                "{}: no such file; --input requires an existing regular UTF-8 file",
                path.display()
            )));
        }
        DiskContents::Present(bytes) => bytes.clone(),
    };
    Ok((bytes, Some(observation)))
}
