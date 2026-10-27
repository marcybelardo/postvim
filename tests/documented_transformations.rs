//! Contract tests for the documented transformation workflow.
//!
//! `docs/transformations.md` is the contract for running one external formatter,
//! and `README.md`, `AGENTS.md`, and `docs/keymaps.md` restate parts of it. These
//! tests keep those pages, `--help`, the command-line parser, the library, and the
//! editor from drifting apart: every documented command line is parsed and
//! compared field by field, every documented byte-range example runs through the
//! library request/execute/apply steps, the documented editor example runs through
//! the editor, and the rejected commands stay rejected.

use std::ffi::OsString;
use std::path::PathBuf;

use postvim::cli::{ByteRange, CliAction, HELP, parse_args_os};
use postvim::transform::{
    ByteRange as SourceRange, FormatterSpec, TransformRequest, TransformSource,
};
use postvim::{Document, Editor, EditorInput};

const TRANSFORMATIONS: &str = include_str!("../docs/transformations.md");
const KEYMAP: &str = include_str!("../docs/keymaps.md");
const AGENTS: &str = include_str!("../AGENTS.md");
const README: &str = include_str!("../README.md");

/// The `--format` grammar every page and `--help` spell out.
const FORMAT_GRAMMAR: &str = "postvim --format [--input PATH] [--range START:END] \
                              [--write] [--stdout] -- PROGRAM [ARG...]";

/// The rustfmt invocation the documentation treats as the real example.
const RUSTFMT_FLAGS: &str = "--emit stdout --edition 2024 --color never";

fn block_lines(document: &str, name: &str) -> Vec<String> {
    let marker = format!("```text {name}");
    let mut lines = Vec::new();
    let mut collecting = false;
    for line in document.lines() {
        let trimmed = line.trim();
        if !collecting {
            collecting = trimmed == marker;
            continue;
        }
        if trimmed == "```" {
            return lines;
        }
        if !trimmed.is_empty() {
            lines.push(trimmed.to_owned());
        }
    }
    panic!("{name:?} block is missing or unterminated");
}

fn quoted(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("expected a quoted buffer, found {text:?}"));
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            other => panic!("unsupported escape in {text:?}: {other:?}"),
        }
    }
    out
}

fn documented_range(text: &str) -> (usize, usize) {
    let (start, end) = text
        .split_once(':')
        .unwrap_or_else(|| panic!("expected START:END, found {text:?}"));
    (
        start.trim().parse().expect("range start"),
        end.trim().parse().expect("range end"),
    )
}

fn documented_flag(value: &str, line: &str) -> bool {
    match value {
        "yes" => true,
        "no" => false,
        other => panic!("expected yes or no, found {other:?} in {line:?}"),
    }
}

fn tokens(text: &str) -> Vec<OsString> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    shell_words::split(text)
        .unwrap_or_else(|error| panic!("cannot tokenize {text:?}: {error}"))
        .into_iter()
        .map(OsString::from)
        .collect()
}

/// One documented `--format` command line and the invocation it must produce.
struct CliExample {
    argv: Vec<OsString>,
    input: Option<PathBuf>,
    range: Option<(usize, usize)>,
    write: bool,
    stdout: bool,
    program: OsString,
    args: Vec<OsString>,
}

fn cli_examples() -> Vec<CliExample> {
    block_lines(TRANSFORMATIONS, "postvim-cli-examples")
        .iter()
        .map(|line| {
            let (argv_text, fields_text) = line
                .split_once(" | ")
                .unwrap_or_else(|| panic!("documented command line needs fields: {line:?}"));
            let mut argv = tokens(argv_text);
            assert_eq!(
                argv.first().map(OsString::as_os_str),
                Some(std::ffi::OsStr::new("postvim")),
                "documented command lines start with the executable name: {line:?}"
            );
            argv.remove(0);
            let mut example = CliExample {
                argv,
                input: None,
                range: None,
                write: false,
                stdout: false,
                program: OsString::new(),
                args: Vec::new(),
            };
            for field in fields_text.split("; ") {
                let (key, value) = field
                    .split_once('=')
                    .unwrap_or_else(|| panic!("malformed documented field {field:?}"));
                match key {
                    "input" => {
                        example.input = match value {
                            "stdin" => None,
                            path => Some(PathBuf::from(path)),
                        };
                    }
                    "range" => {
                        example.range = match value {
                            "none" => None,
                            range => Some(documented_range(range)),
                        };
                    }
                    "write" => example.write = documented_flag(value, line),
                    "stdout" => example.stdout = documented_flag(value, line),
                    "program" => example.program = OsString::from(value),
                    "args" => example.args = tokens(value),
                    other => panic!("unknown documented field {other:?} in {line:?}"),
                }
            }
            example
        })
        .collect()
}

/// One documented library example: text, range, formatter command, BOM, expected
/// text.
///
/// The formatter column is a whole command line rather than one executable name,
/// because the deterministic empty-output example needs arguments (`sed -n ''`).
/// It is tokenized with the same shell-word rules an interactive `:format` uses,
/// so the page can spell an empty argument without inventing a new syntax.
struct TransformExample {
    start: String,
    range: (usize, usize),
    program: OsString,
    args: Vec<OsString>,
    has_bom: bool,
    expected: String,
}

impl TransformExample {
    fn command(&self) -> Vec<String> {
        std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|token| token.to_string_lossy().into_owned())
            .collect()
    }
}

fn transform_examples() -> Vec<TransformExample> {
    block_lines(TRANSFORMATIONS, "postvim-transform-examples")
        .iter()
        .map(|line| {
            let fields: Vec<&str> = line.split(" | ").map(str::trim).collect();
            assert_eq!(
                fields.len(),
                5,
                "malformed documented transformation example: {line:?}"
            );
            let bom = fields[3]
                .strip_prefix("bom=")
                .unwrap_or_else(|| panic!("expected bom=yes or bom=no: {line:?}"));
            let mut command = tokens(fields[2]);
            assert!(
                !command.is_empty(),
                "documented example needs a formatter command: {line:?}"
            );
            let program = command.remove(0);
            TransformExample {
                start: quoted(fields[0]),
                range: documented_range(fields[1]),
                program,
                args: command,
                has_bom: documented_flag(bom, line),
                expected: quoted(fields[4]),
            }
        })
        .collect()
}

/// The key tokens the editor examples may use: literal characters plus `<CR>`,
/// `<Esc>`, `<Tab>`, and `<BS>`.
fn type_keys(editor: &mut Editor, keys: &str) {
    let mut rest = keys;
    while let Some(ch) = rest.chars().next() {
        if ch != '<' {
            editor.handle_input(EditorInput::Character(ch));
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        let end = rest
            .find('>')
            .unwrap_or_else(|| panic!("unterminated key token in {keys:?}"));
        editor.handle_input(match &rest[1..end] {
            "Esc" => EditorInput::Escape,
            "CR" => EditorInput::Enter,
            "Tab" => EditorInput::Tab,
            "BS" => EditorInput::Backspace,
            other => panic!("unknown key token <{other}>"),
        });
        rest = &rest[end + 1..];
    }
}

#[test]
fn documented_cli_examples() {
    let examples = cli_examples();
    assert!(
        examples.len() >= 6,
        "expected the documented command lines, found {}",
        examples.len()
    );
    let mut failures = Vec::new();
    for (index, example) in examples.iter().enumerate() {
        let argv: Vec<String> = example
            .argv
            .iter()
            .map(|token| token.to_string_lossy().into_owned())
            .collect();
        match parse_args_os(example.argv.clone()) {
            Ok(CliAction::Format(invocation)) => {
                let documented_input = example.input.clone();
                if invocation.input != documented_input {
                    failures.push(format!(
                        "example {index} {argv:?}: input is {:?}, documented {:?}",
                        invocation.input, documented_input
                    ));
                }
                let documented_range = example.range.map(|(start, end)| ByteRange { start, end });
                if invocation.range != documented_range {
                    failures.push(format!(
                        "example {index} {argv:?}: range is {:?}, documented {:?}",
                        invocation.range, documented_range
                    ));
                }
                if invocation.writes_document() != example.write {
                    failures.push(format!(
                        "example {index} {argv:?}: write is {}, documented {}",
                        invocation.writes_document(),
                        example.write
                    ));
                }
                if invocation.emits_document() != example.stdout {
                    failures.push(format!(
                        "example {index} {argv:?}: document output is {}, documented {}",
                        invocation.emits_document(),
                        example.stdout
                    ));
                }
                if invocation.program != example.program {
                    failures.push(format!(
                        "example {index} {argv:?}: program is {:?}, documented {:?}",
                        invocation.program, example.program
                    ));
                }
                if invocation.args != example.args {
                    failures.push(format!(
                        "example {index} {argv:?}: arguments are {:?}, documented {:?}",
                        invocation.args, example.args
                    ));
                }
            }
            other => failures.push(format!(
                "example {index} {argv:?}: expected a format invocation, found {other:?}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "docs/transformations.md command lines do not match the parser:\n{}",
        failures.join("\n")
    );
}

#[test]
fn documented_rejected_command_lines() {
    // Each row is a command line `docs/transformations.md` says is rejected.
    let rejected: [&[&str]; 7] = [
        &["--format", "--write", "--", "cat"],
        &["--format", "--input", "notes.rs"],
        &[
            "--format", "--input", "notes.rs", "--range", "5:1", "--", "cat",
        ],
        &[
            "--format", "--input", "notes.rs", "--range", "1", "--", "cat",
        ],
        &["--format", "--"],
        &["--format", "--input", "a", "--input", "b", "--", "cat"],
        &["--format", "--input", "a", "--output", "b", "--", "cat"],
    ];
    for argv in rejected {
        let parsed = parse_args_os(argv.iter().map(OsString::from));
        assert!(
            parsed.is_err(),
            "{argv:?} must stay rejected, but parsed as {parsed:?}"
        );
    }
    // The documented grammar is exactly what `--help` prints.
    assert!(
        HELP.contains(FORMAT_GRAMMAR),
        "--help must print the documented --format grammar"
    );
}

#[test]
fn documented_rejected_editor_commands() {
    // `:w!` and `:!` are the escape hatches Postvim does not have, and both
    // reload commands and `:format` reject their documented bad forms.
    for command in [":w!", ":!", ":reload PATH", ":reload! PATH", ":format"] {
        let mut editor = Editor::from_document(Document::from_editable_text("abc\n", false));
        type_keys(&mut editor, &format!("{command}<CR>"));
        assert_eq!(
            editor.document().editable_text(),
            "abc\n",
            "{command} must not touch the text"
        );
        assert!(
            editor.message().is_some(),
            "{command} must report why it was rejected"
        );
        assert!(!editor.should_quit(), "{command} must not quit");
    }
}

#[test]
fn documented_unnamed_reload_is_refused() {
    let mut editor = Editor::from_document(Document::from_editable_text("abc\n", false));
    for command in [":reload", ":reload!"] {
        type_keys(&mut editor, &format!("{command}<CR>"));
        assert_eq!(
            editor.document().editable_text(),
            "abc\n",
            "{command} must keep an unnamed buffer"
        );
        assert!(
            editor.message().is_some(),
            "{command} must explain that a file path is required"
        );
    }
}

#[test]
fn documented_empty_output_example_drains_its_input() {
    // Documented examples must not use a program that exits without reading its
    // standard input. Postvim requires complete delivery, so such a run is
    // reported as a premature close and the documented expectation would never
    // hold: `true` and friends would make this suite flaky rather than prove the
    // empty-output rule. The deterministic sink is `sed -n ''`.
    let examples = transform_examples();
    assert!(
        examples.iter().any(|example| example.expected.is_empty()),
        "docs/transformations.md must keep an empty-output deletion example"
    );
    for example in &examples {
        let program = example.program.to_string_lossy();
        assert!(
            !["true", "false", ":", "yes"].contains(&program.as_ref()),
            "{program} exits without draining standard input; use a consuming sink such as `sed -n ''`"
        );
    }
    assert!(
        TRANSFORMATIONS.contains(":format sed -n ''"),
        "the documented editor example must use the input-draining empty-output tool"
    );
}

#[cfg(unix)]
#[test]
fn documented_transform_examples() {
    let examples = transform_examples();
    assert!(
        examples.len() >= 6,
        "expected the documented transformation examples, found {}",
        examples.len()
    );
    let mut failures = Vec::new();
    for (index, example) in examples.iter().enumerate() {
        let command = example.command();
        let document = Document::from_editable_text(&example.start, example.has_bom);
        let source = TransformSource::from_document(
            &document,
            SourceRange::new(example.range.0, example.range.1),
        );
        let formatter = FormatterSpec::new(&example.program).args(example.args.clone());
        let request = TransformRequest::new(source, formatter);
        match request.execute() {
            Ok(result) => match result.apply(&document) {
                Ok(updated) => {
                    let text = updated.editable_text();
                    if text != example.expected {
                        failures.push(format!(
                            "example {index} ({command:?}): editable text is {text:?}, documented {:?}",
                            example.expected
                        ));
                    }
                    if updated.has_bom() != example.has_bom {
                        failures.push(format!(
                            "example {index} ({command:?}): BOM is {}, documented {}",
                            updated.has_bom(),
                            example.has_bom
                        ));
                    }
                }
                Err(error) => failures.push(format!(
                    "example {index} ({command:?}): apply failed: {error}"
                )),
            },
            Err(error) => failures.push(format!(
                "example {index} ({command:?}): formatter run failed: {error}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "docs/transformations.md examples do not match the transformation contract:\n{}",
        failures.join("\n")
    );
}

#[cfg(unix)]
#[test]
fn documented_editor_format_example() {
    // The documented editor example: `sed -n ''` reads all of standard input and
    // prints nothing, so the whole buffer becomes empty as one unsaved undo step,
    // and `u` restores it. `true` is deliberately not used here: exiting without
    // draining the input can fail with a broken pipe, which is a legitimate
    // formatter failure rather than a deterministic empty replacement.
    let mut editor = Editor::from_document(Document::from_editable_text("abc\n", false));
    type_keys(&mut editor, ":format sed -n ''<CR>");
    assert_eq!(
        editor.document().editable_text(),
        "",
        ":format sed -n '' must replace the whole buffer with empty output"
    );
    assert!(
        editor.is_dirty(),
        ":format must leave an unsaved change without writing"
    );
    type_keys(&mut editor, "u");
    assert_eq!(
        editor.document().editable_text(),
        "abc\n",
        "one undo must restore the bytes the format run replaced"
    );
    assert!(
        !editor.is_dirty(),
        "undoing back to the saved bytes must make the buffer clean again"
    );
}

#[test]
fn guides_and_readme_agree_on_transformations() {
    assert!(
        TRANSFORMATIONS.contains(FORMAT_GRAMMAR),
        "docs/transformations.md must document the exact --format grammar"
    );
    for guide in [TRANSFORMATIONS, README, AGENTS] {
        assert!(
            guide.contains(RUSTFMT_FLAGS),
            "the formatter flags are documented explicitly everywhere rustfmt is named"
        );
    }
    // The library/CLI/editor separation and the diagnostic cap are contract.
    for claim in ["2,048", "BOM", "no shared unsaved state"] {
        assert!(
            TRANSFORMATIONS.contains(claim),
            "docs/transformations.md must state {claim:?}"
        );
    }
    assert!(
        TRANSFORMATIONS.contains("Never redirect"),
        "docs/transformations.md must warn against redirecting onto the input file"
    );
    // Recovery stays explicit: no force-write, and the keymap keeps it that way.
    assert!(KEYMAP.contains(":w!"), "docs/keymaps.md must reject :w!");
    assert!(AGENTS.contains(":w!"), "AGENTS.md must reject :w!");
    assert!(
        TRANSFORMATIONS.contains("keymaps.md"),
        "docs/transformations.md must point at the key reference"
    );
}
