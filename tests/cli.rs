use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use postvim::cli::{
    ByteRange, CliAction, FormatInvocation, FormatStreams, parse_args_os, run_format,
};

fn parse(args: &[&str]) -> Result<CliAction, String> {
    parse_args_os(args.iter().map(OsString::from))
}

#[test]
fn context_argument_grammar() {
    use postvim::cli::{ContextInvocation, LineRange};
    for (options, expected) in [
        (vec!["--lines", "1:2"], 3),
        (vec!["--lines", "01:2", "--surround", "0"], 0),
        (vec!["--surround", "20", "--lines", "1:2"], 20),
    ] {
        let mut args = vec!["--context"];
        args.extend(options);
        args.extend(["--", "-"]);
        assert_eq!(parse(&args).unwrap(), CliAction::Context(ContextInvocation {
            path: PathBuf::from("-"), lines: LineRange { start: 1, end: 2 }, surrounding_lines: expected,
        }));
    }
    for options in [
        vec![], vec!["--", "p"], vec!["--lines"], vec!["--lines", "1:1"],
        vec!["--lines", "1:1", "--"], vec!["--lines", "1:1", "--", ""],
        vec!["--lines", "1:1", "--", "p", "q"],
        vec!["--lines", "1:1", "--lines", "1:1", "--", "p"],
        vec!["--lines", "1:1", "--surround", "0", "--surround", "1", "--", "p"],
        vec!["--unknown", "--", "p"], vec!["--surround"],
    ] {
        let mut args = vec!["--context"];
        args.extend(options);
        assert!(parse(&args).is_err(), "{args:?}");
    }
    for value in ["0:1", "2:1", "1:0", "1", "1:2:3", ":1", "1:", "-1:2", "+1:2", " 1:2", "١:2", "1:２", "184467440737095516160:2"] {
        assert!(parse(&["--context", "--lines", value, "--", "p"]).is_err(), "{value}");
    }
    for value in ["21", "-1", "+1", "", "١", " 1", "184467440737095516160"] {
        assert!(parse(&["--context", "--lines", "1:1", "--surround", value, "--", "p"]).is_err());
    }
    let maximum = format!("{}:{}", usize::MAX, usize::MAX);
    assert!(parse(&["--context", "--lines", &maximum, "--", "p"]).is_ok());
}

#[test]
fn context_preserves_existing_launch_grammar() {
    assert_eq!(parse(&["context"]).unwrap(), CliAction::Edit(Some("context".into())));
    assert_eq!(parse(&["--", "--context"]).unwrap(), CliAction::Edit(Some("--context".into())));
    assert!(parse(&["p", "--context"]).is_err());
    assert!(matches!(parse(&["--format", "--", "--context"]).unwrap(), CliAction::Format(_)));
    #[cfg(unix)] {
        use std::os::unix::ffi::OsStringExt;
        let native = OsString::from_vec(vec![0xff]);
        for option in ["--lines", "--surround"] {
            assert!(parse_os(vec!["--context".into(), option.into(), native.clone(), "--".into(), "p".into()]).is_err());
        }
        let action = parse_os(vec!["--context".into(), "--lines".into(), "1:1".into(), "--".into(), native.clone()]).unwrap();
        let CliAction::Context(invocation) = action else { panic!() };
        assert_eq!(invocation.path.as_os_str(), native);
    }
}

fn parse_os(args: Vec<OsString>) -> Result<CliAction, String> {
    parse_args_os(args)
}

fn format(
    input: Option<&str>,
    range: Option<(usize, usize)>,
    write: bool,
    stdout: bool,
    program: &str,
    args: &[&str],
) -> CliAction {
    CliAction::Format(FormatInvocation {
        input: input.map(PathBuf::from),
        range: range.map(|(start, end)| ByteRange { start, end }),
        write,
        stdout,
        program: OsString::from(program),
        args: args.iter().map(OsString::from).collect(),
    })
}

fn parsed(args: &[&str]) -> FormatInvocation {
    match parse(args) {
        Ok(CliAction::Format(invocation)) => invocation,
        other => panic!("expected a format invocation from {args:?}, found {other:?}"),
    }
}

#[test]
fn cli_startup_contract() {
    assert_eq!(parse(&[]), Ok(CliAction::Edit(None)));
    assert_eq!(
        parse(&["notes.txt"]),
        Ok(CliAction::Edit(Some(PathBuf::from("notes.txt"))))
    );
    assert_eq!(
        parse(&["--", "-draft"]),
        Ok(CliAction::Edit(Some(PathBuf::from("-draft"))))
    );
    assert_eq!(parse(&["--help"]), Ok(CliAction::Help));
    assert_eq!(parse(&["--version"]), Ok(CliAction::Version));
    assert!(parse(&["--unknown"]).is_err());
    assert!(parse(&["a", "b"]).is_err());
    assert!(parse(&["--", "a", "b"]).is_err());
    assert!(parse(&["--help", "file"]).is_err());
}

#[test]
fn format_is_recognized_only_in_the_first_option_position() {
    // A file named `format` keeps opening as a positional path.
    assert_eq!(
        parse(&["format"]),
        Ok(CliAction::Edit(Some(PathBuf::from("format"))))
    );
    assert_eq!(
        parse(&["--", "format"]),
        Ok(CliAction::Edit(Some(PathBuf::from("format"))))
    );
    // Later occurrences stay positional arguments, so the path grammar rejects
    // the extra argument instead of switching modes.
    assert!(parse(&["notes.txt", "--format"]).is_err());
    assert!(parse(&["--", "a", "--format"]).is_err());
}

#[test]
fn format_cli_parse_matrix() {
    // Defaults: stdin input, whole-document range, stdout artifact.
    let invocation = parsed(&["--format", "--", "rustfmt"]);
    assert_eq!(invocation.input, None);
    assert_eq!(invocation.range, None);
    assert!(!invocation.writes_document());
    assert!(invocation.emits_document());
    assert_eq!(invocation.program, OsString::from("rustfmt"));
    assert!(invocation.args.is_empty());
    assert_eq!(
        parse(&["--format", "--", "rustfmt"]),
        Ok(format(None, None, false, false, "rustfmt", &[]))
    );

    // Every formatter argument after `--` is preserved verbatim.
    let invocation = parsed(&[
        "--format",
        "--",
        "rustfmt",
        "--emit",
        "stdout",
        "--edition",
        "2024",
        "--color",
        "never",
    ]);
    assert_eq!(
        invocation.args,
        ["--emit", "stdout", "--edition", "2024", "--color", "never"]
            .map(OsString::from)
            .to_vec()
    );
    assert_eq!(
        parse(&["--format", "--", "fmt", "-w", "--", "literal"]),
        Ok(format(
            None,
            None,
            false,
            false,
            "fmt",
            &["-w", "--", "literal"]
        ))
    );

    // `--input` consumes one native path, including one that begins with `-`.
    assert_eq!(
        parsed(&["--format", "--input", "notes.txt", "--", "fmt"]).input,
        Some(PathBuf::from("notes.txt"))
    );
    assert_eq!(
        parsed(&["--format", "--input", "-", "--", "fmt"]).input,
        Some(PathBuf::from("-")),
        "`--input -` names a literal file, not an implicit stream alias"
    );
    assert_eq!(
        parsed(&["--format", "--input", "--write", "--", "fmt"]).input,
        Some(PathBuf::from("--write")),
        "--input takes the next argument verbatim even when it looks like an option"
    );

    // Ranges are decimal, zero-based, half-open, and byte-oriented.
    for (text, expected) in [
        ("0:0", ByteRange { start: 0, end: 0 }),
        ("2:2", ByteRange { start: 2, end: 2 }),
        ("007:010", ByteRange { start: 7, end: 10 }),
    ] {
        assert_eq!(
            parsed(&["--format", "--range", text, "--", "fmt"]).range,
            Some(expected),
            "range {text:?}"
        );
    }
    let largest = format!("0:{}", usize::MAX);
    assert_eq!(
        parsed(&["--format", "--range", &largest, "--", "fmt"]).range,
        Some(ByteRange {
            start: 0,
            end: usize::MAX,
        })
    );
    assert_eq!(
        parse(&["--format", "--range", "4:9", "--", "fmt"]),
        Ok(format(None, Some((4, 9)), false, false, "fmt", &[]))
    );

    // Write and stdout combinations.
    assert_eq!(
        parse(&["--format", "--input", "f", "--write", "--", "fmt"]),
        Ok(format(Some("f"), None, true, false, "fmt", &[]))
    );
    assert_eq!(
        parse(&["--format", "--stdout", "--", "fmt"]),
        Ok(format(None, None, false, true, "fmt", &[]))
    );
    assert_eq!(
        parse(&[
            "--format", "--input", "f", "--write", "--stdout", "--", "fmt"
        ]),
        Ok(format(Some("f"), None, true, true, "fmt", &[]))
    );
    assert_eq!(
        parse(&[
            "--format", "--stdout", "--write", "--input", "f", "--", "fmt"
        ]),
        Ok(format(Some("f"), None, true, true, "fmt", &[]))
    );
}

#[test]
fn format_cli_rejects_bad_grammar_before_running_anything() {
    let cases: &[&[&str]] = &[
        // `--` is mandatory and a program is mandatory after it.
        &["--format"],
        &["--format", "--input", "f"],
        &["--format", "--write"],
        &["--format", "--", ""],
        // Missing option values.
        &["--format", "--input"],
        &["--format", "--range"],
        // Unknown options.
        &["--format", "--bogus", "--", "fmt"],
        &["--format", "-x", "--", "fmt"],
        // Repeated options.
        &["--format", "--input", "a", "--input", "b", "--", "fmt"],
        &["--format", "--range", "0:1", "--range", "0:1", "--", "fmt"],
        &[
            "--format", "--write", "--write", "--input", "f", "--", "fmt",
        ],
        &["--format", "--stdout", "--stdout", "--", "fmt"],
        // `--write` has no destination without `--input`.
        &["--format", "--write", "--", "fmt"],
        &["--format", "--write", "--stdout", "--", "fmt"],
        &["--format", "--range", "", "--", "fmt"],
        &["--format", "--range", "1", "--", "fmt"],
        &["--format", "--range", "a:b", "--", "fmt"],
        &["--format", "--range", "-1:2", "--", "fmt"],
        &["--format", "--range", "1:-2", "--", "fmt"],
        &["--format", "--range", "1:2:3", "--", "fmt"],
        &["--format", "--range", "3:2", "--", "fmt"],
        &["--format", "--range", "1: 2", "--", "fmt"],
        &["--format", "--range", "1..2", "--", "fmt"],
        &[
            "--format",
            "--range",
            "99999999999999999999999:1",
            "--",
            "fmt",
        ],
    ];
    for args in cases {
        assert!(
            parse(args).is_err(),
            "expected {args:?} to be rejected before a formatter runs"
        );
    }

    // The usage diagnostics name the problem specifically enough to act on.
    let missing_separator = parse(&["--format", "fmt"]).unwrap_err();
    assert!(missing_separator.contains("--"), "{missing_separator}");
    let write_without_input = parse(&["--format", "--write", "--", "fmt"]).unwrap_err();
    assert!(
        write_without_input.contains("--input"),
        "{write_without_input}"
    );
    let reversed = parse(&["--format", "--range", "3:2", "--", "fmt"]).unwrap_err();
    assert!(reversed.contains("3:2"), "{reversed}");
}

#[test]
fn format_cli_mode_predicates_match_the_documented_artifact() {
    let stdout_only = parsed(&["--format", "--", "fmt"]);
    assert!(stdout_only.emits_document());
    assert!(!stdout_only.writes_document());

    let explicit_stdout = parsed(&["--format", "--stdout", "--", "fmt"]);
    assert!(explicit_stdout.emits_document());
    assert!(!explicit_stdout.writes_document());

    let write_only = parsed(&["--format", "--input", "f", "--write", "--", "fmt"]);
    assert!(!write_only.emits_document());
    assert!(write_only.writes_document());

    let both = parsed(&[
        "--format", "--input", "f", "--write", "--stdout", "--", "fmt",
    ]);
    assert!(both.emits_document());
    assert!(both.writes_document());
}

#[test]
fn format_cli_help_documents_the_grammar() {
    let help = postvim::cli::HELP;
    for expected in [
        "postvim --format",
        "--input PATH",
        "--range START:END",
        "--write",
        "--stdout",
        "PROGRAM [ARG...]",
        "half-open",
        "bytes",
        "no shell",
        "no timeout",
        "stderr",
        ":format PROGRAM [ARG...]",
        ":reload",
        ":reload!",
    ] {
        assert!(help.contains(expected), "--help must mention {expected:?}");
    }
    // Legacy documentation stays intact.
    for command in [":w", ":w PATH", ":q", ":q!", ":wq", ":x"] {
        assert!(help.contains(command), "help must document {command:?}");
    }
}

#[cfg(unix)]
#[test]
fn cli_preserves_non_utf8_paths() {
    use std::os::unix::ffi::OsStringExt;

    let path = OsString::from_vec(vec![b'f', b'i', 0x80]);
    assert_eq!(
        parse_args_os([path.clone()]),
        Ok(CliAction::Edit(Some(PathBuf::from(path))))
    );
}

#[cfg(unix)]
#[test]
fn format_cli_preserves_native_paths_and_argv() {
    use std::os::unix::ffi::OsStringExt;

    // Input paths and formatter argv keep raw bytes instead of lossy Unicode.
    let input = OsString::from_vec(vec![b'i', b'n', b'p', b'u', b't', 0x80]);
    let program = OsString::from_vec(vec![b'p', b'r', b'o', b'g', 0x80]);
    let arg = OsString::from_vec(vec![b'-', b'a', 0x80]);
    let invocation = parse_os(vec![
        OsString::from("--format"),
        OsString::from("--input"),
        input.clone(),
        OsString::from("--"),
        program.clone(),
        arg.clone(),
    ])
    .expect("native argv parses");
    let CliAction::Format(invocation) = invocation else {
        panic!("expected a format invocation");
    };
    assert_eq!(invocation.input, Some(PathBuf::from(input)));
    assert_eq!(invocation.program, program);
    assert_eq!(invocation.args, vec![arg]);
}

#[test]
fn non_tty_exits_cleanly() {
    use std::process::{Command, Stdio};

    let output = Command::new(env!("CARGO_BIN_EXE_postvim"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive stdin and stdout"));
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));

    for flag in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_postvim"))
            .arg(flag)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.contains(&0x1b));
    }
}

#[test]
fn help_output_is_the_shared_text() {
    use std::process::{Command, Stdio};

    let output = Command::new(env!("CARGO_BIN_EXE_postvim"))
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = output.stdout;
    assert_eq!(
        String::from_utf8(stdout.clone()).unwrap(),
        postvim::cli::HELP,
        "--help must print the shared help text unchanged"
    );
    assert!(!stdout.contains(&0x1b));
    assert!(String::from_utf8(stdout).unwrap().contains("postvim --context --lines START:END"));
}

#[test]
fn help_documents_bounded_commands() {
    for text in ["--context", "--surround", "saved file on disk", "not live editor state", "dirty buffers or highlights", "not lossless", "1-based inclusive", "byte offsets"] {
        assert!(postvim::cli::HELP.contains(text), "help must document {text}");
    }
    for command in [":w", ":w PATH", ":q", ":q!", ":wq", ":x"] {
        assert!(
            postvim::cli::HELP.contains(command),
            "help must document {command:?}"
        );
    }
    assert!(
        !postvim::cli::HELP.contains("There is no force-quit"),
        "help still claims there is no force quit"
    );
}

fn run_format_with(
    invocation: &FormatInvocation,
    directory: &Path,
    input: &[u8],
    stdout: &mut dyn Write,
) -> (Vec<u8>, Option<String>) {
    let mut stdin = Cursor::new(input.to_vec());
    let mut stderr = Vec::new();
    let result = run_format(
        invocation,
        directory,
        FormatStreams {
            stdin: &mut stdin,
            stdout,
            stderr: &mut stderr,
        },
    );
    (stderr, result.err().map(|error| error.to_string()))
}

fn run_format_capture(
    invocation: &FormatInvocation,
    directory: &Path,
    input: &[u8],
) -> (Vec<u8>, Vec<u8>, Option<String>) {
    let mut stdout = Vec::new();
    let (stderr, error) = run_format_with(invocation, directory, input, &mut stdout);
    (stdout, stderr, error)
}

#[cfg(unix)]
fn run_binary(args: &[&str], stdin: &[u8]) -> std::process::Output {
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_postvim"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn format_cli_input_and_range_errors_emit_no_document() {
    let directory = tempfile::tempdir().unwrap();

    let missing = directory.path().join("missing.txt");
    let invocation = parsed(&[
        "--format",
        "--input",
        missing.to_str().unwrap(),
        "--",
        "cat",
    ]);
    let (stdout, _, error) = run_format_capture(&invocation, directory.path(), b"");
    assert!(error.unwrap().contains("no such file"));
    assert!(stdout.is_empty());

    let invalid = directory.path().join("invalid.txt");
    fs::write(&invalid, [0xff, 0xfe]).unwrap();
    let invocation = parsed(&[
        "--format",
        "--input",
        invalid.to_str().unwrap(),
        "--",
        "cat",
    ]);
    let (stdout, _, error) = run_format_capture(&invocation, directory.path(), b"");
    assert!(error.unwrap().contains("not valid UTF-8"));
    assert!(stdout.is_empty());
    assert_eq!(fs::read(&invalid).unwrap(), [0xff, 0xfe]);

    // Standard input is validated the same way.
    let invocation = parsed(&["--format", "--", "cat"]);
    let (stdout, _, error) = run_format_capture(&invocation, directory.path(), &[0xff]);
    assert!(error.unwrap().contains("standard input is not valid UTF-8"));
    assert!(stdout.is_empty());

    // Out-of-bounds and CRLF-splitting ranges are rejected before the program
    // could run: an unresolvable program name still yields the range error.
    let document = directory.path().join("document.txt");
    fs::write(&document, "alpha\r\nbeta").unwrap();
    let input = document.to_str().unwrap().to_owned();
    for range in ["0:999", "0:6"] {
        let invocation = parsed(&[
            "--format",
            "--input",
            input.as_str(),
            "--range",
            range,
            "--",
            "postvim-test-missing-formatter",
        ]);
        let (stdout, _, error) = run_format_capture(&invocation, directory.path(), b"");
        let error = error.expect("an invalid range must fail");
        assert!(error.contains("invalid range"), "range {range}: {error}");
        assert!(stdout.is_empty());
    }

    // A range may not split an extended grapheme.
    let invocation = parsed(&[
        "--format",
        "--range",
        "0:1",
        "--",
        "postvim-test-missing-formatter",
    ]);
    let (_, _, error) = run_format_capture(&invocation, directory.path(), "é".as_bytes());
    assert!(error.unwrap().contains("invalid range"));
}

#[cfg(unix)]
#[test]
fn format_cli_stdout_write_modes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("document.txt");
    let original = b"\xEF\xBB\xBFalpha\r\nbeta".to_vec();
    fs::write(&path, &original).unwrap();
    let input = path.to_str().unwrap().to_owned();

    // Default: the full artifact with the BOM and CRLF endings preserved, and
    // the file is never touched.
    let invocation = parsed(&["--format", "--input", input.as_str(), "--", "cat"]);
    let (stdout, stderr, error) = run_format_capture(&invocation, directory.path(), b"");
    assert_eq!(error, None);
    assert_eq!(stdout, original);
    assert!(stderr.is_empty());
    assert_eq!(fs::read(&path).unwrap(), original);

    // A selected range changes only the formatter's input, not the artifact.
    let invocation = parsed(&[
        "--format",
        "--input",
        input.as_str(),
        "--range",
        "0:5",
        "--",
        "cat",
    ]);
    let (stdout, _, error) = run_format_capture(&invocation, directory.path(), b"");
    assert_eq!(error, None);
    assert_eq!(stdout, original);

    // Without --input, the document is read from standard input through EOF.
    let invocation = parsed(&["--format", "--", "cat"]);
    let (stdout, _, error) = run_format_capture(&invocation, directory.path(), b"from stdin");
    assert_eq!(error, None);
    assert_eq!(stdout, b"from stdin");

    // --write replaces the input and emits no document unless --stdout asks.
    let plain = directory.path().join("plain.txt");
    fs::write(&plain, b"hello\r\nworld").unwrap();
    let plain_input = plain.to_str().unwrap().to_owned();
    let invocation = parsed(&[
        "--format",
        "--input",
        plain_input.as_str(),
        "--write",
        "--",
        "tr",
        "a-z",
        "A-Z",
    ]);
    let (stdout, _, error) = run_format_capture(&invocation, directory.path(), b"");
    assert_eq!(error, None);
    assert!(stdout.is_empty(), "--write alone emits no document");
    assert_eq!(fs::read(&plain).unwrap(), b"HELLO\r\nWORLD");

    // --write --stdout saves first, then emits the same full document.
    let both = directory.path().join("both.txt");
    fs::write(&both, b"hello\r\nworld").unwrap();
    let both_input = both.to_str().unwrap().to_owned();
    let invocation = parsed(&[
        "--format",
        "--input",
        both_input.as_str(),
        "--write",
        "--stdout",
        "--",
        "tr",
        "a-z",
        "A-Z",
    ]);
    let (stdout, _, error) = run_format_capture(&invocation, directory.path(), b"");
    assert_eq!(error, None);
    assert_eq!(stdout, b"HELLO\r\nWORLD");
    assert_eq!(fs::read(&both).unwrap(), b"HELLO\r\nWORLD");
}

#[cfg(unix)]
#[test]
fn format_cli_error_stream_contract() {
    let directory = tempfile::tempdir().unwrap();

    let invocation = parsed(&["--format", "--", "postvim-test-missing-formatter"]);
    let (stdout, stderr, error) = run_format_capture(&invocation, directory.path(), b"text");
    let error = error.expect("a missing program is an error");
    assert!(error.contains("could not start formatter"), "{error}");
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());

    // A failing formatter keeps its diagnostics sanitized, never as document text.
    // The tests name `sh` explicitly, which Postvim permits; it never supplies a
    // shell itself. Using an installed binary also avoids exec'ing a file this
    // process just wrote, whose inherited write descriptor can race with another
    // test thread's fork and surface as ETXTBSY.
    let invocation = parsed(&[
        "--format",
        "--",
        "sh",
        "-c",
        "cat >/dev/null; printf '\\033[31mbad\\033[0m' >&2; exit 3",
    ]);
    let (stdout, stderr, error) = run_format_capture(&invocation, directory.path(), b"text");
    let error = error.expect("a nonzero formatter is an error");
    assert!(error.contains("formatter exited"), "{error}");
    assert!(
        error.contains("\\u{1b}[31mbad"),
        "diagnostics stay escaped in the message: {error}"
    );
    assert!(
        !error.contains('\u{1b}'),
        "no raw escape reaches the stream"
    );
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn format_cli_successful_warnings_go_to_stderr_sanitized() {
    let directory = tempfile::tempdir().unwrap();
    let invocation = parsed(&[
        "--format",
        "--",
        "sh",
        "-c",
        "printf '\\033[33mwarn\\033[0m' >&2; cat",
    ]);
    let (stdout, stderr, error) = run_format_capture(&invocation, directory.path(), b"document");
    assert_eq!(error, None);
    assert_eq!(stdout, b"document", "warnings never become document text");
    let stderr = String::from_utf8(stderr).unwrap();
    assert!(stderr.contains("formatter:"), "{stderr}");
    assert!(stderr.contains("\\u{1b}[33mwarn"), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "stderr carries no raw escape");
}

#[cfg(unix)]
#[test]
fn format_cli_conflict_during_formatter_keeps_foreign_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("document.txt");
    fs::write(&path, b"local bytes").unwrap();
    let body = format!("printf 'foreign bytes' > '{}'; cat", path.display());
    let invocation = parsed(&[
        "--format",
        "--input",
        path.to_str().unwrap(),
        "--write",
        "--",
        "sh",
        "-c",
        body.as_str(),
    ]);
    let (stdout, _, error) = run_format_capture(&invocation, directory.path(), b"");
    let error = error.expect("an external change must refuse the save");
    assert!(error.contains("changed on disk"), "{error}");
    assert!(stdout.is_empty());
    assert_eq!(fs::read(&path).unwrap(), b"foreign bytes");
}

#[cfg(unix)]
#[test]
fn format_cli_stdout_failure_reports_a_committed_save() {
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected stdout failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("document.txt");
    fs::write(&path, b"hello").unwrap();
    let input = path.to_str().unwrap().to_owned();

    // Without --write, a stdout failure leaves the file alone.
    let invocation = parsed(&[
        "--format",
        "--input",
        input.as_str(),
        "--",
        "tr",
        "a-z",
        "A-Z",
    ]);
    let mut failing = FailingWriter;
    let (_, error) = run_format_with(&invocation, directory.path(), b"", &mut failing);
    let error = error.expect("a stdout failure is an error");
    assert!(
        error.contains("cannot write the transformed document"),
        "{error}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"hello");

    // After a committed save the same failure must say the save already happened.
    let invocation = parsed(&[
        "--format",
        "--input",
        input.as_str(),
        "--write",
        "--stdout",
        "--",
        "tr",
        "a-z",
        "A-Z",
    ]);
    let mut failing = FailingWriter;
    let (_, error) = run_format_with(&invocation, directory.path(), b"", &mut failing);
    let error = error.expect("a post-save stdout failure is still an error");
    assert!(error.contains("saved"), "{error}");
    assert!(error.contains("standard output"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), b"HELLO");
}

#[cfg(unix)]
#[test]
fn format_cli_runs_without_a_tty() {
    let output = run_binary(&["--format", "--", "cat"], b"hello\n");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"hello\n");
    assert!(output.stderr.is_empty());
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
}

#[cfg(unix)]
#[test]
fn format_cli_usage_errors_use_stderr_without_a_tty() {
    for args in [
        &["--format"][..],
        &["--format", "--write", "--", "cat"][..],
        &["--format", "--range", "3:2", "--", "cat"][..],
    ] {
        let output = run_binary(args, b"");
        assert!(!output.status.success(), "expected {args:?} to fail");
        assert!(
            output.stdout.is_empty(),
            "a usage failure must not emit document bytes"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("postvim:"), "{stderr}");
        assert!(!output.stderr.contains(&0x1b));
    }
}
