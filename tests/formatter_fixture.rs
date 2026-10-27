//! A deterministic stand-in for an external formatter.
//!
//! This binary is a test artifact, not part of the shipping editor. It exists
//! so formatter behavior can be exercised without depending on a real tool,
//! a network, or timing luck.
//!
//! Three entry styles, all dispatched from this `main`:
//!
//! * `formatter_fixture` with no arguments runs the named scenarios below as
//!   ordinary Rust functions with assertions, then exits. `cargo test` runs the
//!   target this way, so a failing assertion fails the target. Nothing here
//!   needs a terminal, a nested Cargo, or Python.
//! * `formatter_fixture fixture MODE` is one formatter run: bytes arrive on
//!   stdin and results leave on stdout/stderr with a fixed exit status. Every
//!   mode is deterministic and finishes on its own.
//! * `formatter_fixture scenario NAME` holds the long or deadlock-sensitive
//!   behaviors, which a parent is expected to bound with a deadline and kill as
//!   a process group. They deliberately do not run in the no-argument set.
//!
//! The `fixture modes` listing is part of the contract with
//! `tests/terminal_smoke.py`: both sides name the same modes.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::time::Duration;

use postvim::document::{Cursor, Document};
use postvim::editor::{Editor, EditorInput, EditorMode};
use postvim::transform::{
    self, ByteRange, DIAGNOSTIC_SUMMARY_LIMIT, DIAGNOSTIC_TRUNCATION_MARKER, FormatterSpec,
    TransformError, TransformRequest, TransformResult, TransformSource,
};

/// What `fixture mutate-file PATH` writes over PATH, standing in for another
/// program editing a file while Postvim is busy formatting it.
const EXTERNAL_EDIT: &[u8] = b"external edit\n";

/// Exit statuses are distinct so tests can prove a formatter's status is
/// reported rather than collapsed into a generic failure.
const STATUS_ERROR: u8 = 3;
const STATUS_DIAGNOSTICS: u8 = 5;

const STDOUT_CHUNK: usize = 16 * 1024;
const STDERR_CHUNK: usize = 4 * 1024;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some((first, rest)) = args.split_first() else {
        return run_default_scenarios();
    };

    let result = match first.to_str() {
        Some("fixture") => run_fixture(rest),
        Some("scenario") => run_scenario(rest),
        Some("--help") | Some("-h") => {
            print!("{}", USAGE);
            Ok(())
        }
        _ => Err(format!(
            "unknown fixture command: {}\n{}",
            first.to_string_lossy(),
            USAGE
        )),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // Diagnostics never share stdout, which belongs to the formatter's
            // replacement text in this process.
            let _ = writeln!(io::stderr().lock(), "formatter_fixture: {message}");
            if is_usage_error(&message) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

const USAGE: &str = "\
usage: formatter_fixture                    run the built-in scenarios
       formatter_fixture fixture MODE       act as a formatter, one run
       formatter_fixture scenario NAME      run a long or blocking scenario
       formatter_fixture fixture modes      list the formatter modes
";

fn is_usage_error(message: &str) -> bool {
    message.starts_with("unknown fixture command")
        || message.starts_with("unknown formatter mode")
        || message.starts_with("unknown scenario")
        || message.starts_with("missing")
}

// ---------------------------------------------------------------------------
// Formatter modes: one run in, one result out.
// ---------------------------------------------------------------------------

const MODE_USAGE: &str = "'fixture MODE' where MODE is one of: identity, rewrite, \
empty, warning, error, invalid-utf8, large, early-close, mutate-file PATH, \
args, diagnostics, modes";

fn mode_names() -> &'static [&'static str] {
    &[
        "args",
        "diagnostics",
        "early-close",
        "empty",
        "error",
        "identity",
        "invalid-utf8",
        "large",
        "mutate-file",
        "rewrite",
        "warning",
    ]
}

fn run_fixture(rest: &[OsString]) -> Result<(), String> {
    let Some(mode) = rest.first() else {
        return Err(MODE_USAGE.to_owned());
    };
    let Some(mode) = mode.to_str() else {
        return Err(format!("formatter mode must be UTF-8; {MODE_USAGE}"));
    };
    let arguments = &rest[1..];

    match mode {
        "modes" => {
            let mut stdout = io::stdout().lock();
            for name in mode_names() {
                writeln!(stdout, "{name}").map_err(|error| error.to_string())?;
            }
            stdout.flush().map_err(|error| error.to_string())
        }
        "identity" => replacement(|input| input.to_vec()),
        "rewrite" => replacement(|input| {
            input
                .iter()
                .map(|byte| byte.to_ascii_uppercase())
                .collect::<Vec<u8>>()
        }),
        "empty" => replacement(|_| Vec::new()),
        "warning" => {
            let input = read_stdin()?;
            write_stderr(b"fixture warning: the formatter kept the input\n")?;
            write_stdout(&input)
        }
        "error" => {
            let mut stderr = io::stderr().lock();
            stderr
                .write_all(b"fixture error: the formatter rejected this input\n")
                .map_err(|error| error.to_string())?;
            stderr.flush().map_err(|error| error.to_string())?;
            // Read stdin so a caller that writes a large document is not
            // rejected with a broken pipe instead of this deliberate failure.
            read_stdin()?;
            std::process::exit(i32::from(STATUS_ERROR));
        }
        "invalid-utf8" => {
            read_stdin()?;
            write_stdout(b"replacement \xff\xfe bytes\n")
        }
        "large" => {
            read_stdin()?;
            write_interleaved(64)
        }
        "early-close" => {
            // Deliberately leaves stdin unread and exits successfully: the
            // caller's writer sees a broken pipe while the process status is
            // still a success.
            write_stdout(b"partial\n")
        }
        "mutate-file" => {
            let Some(path) = arguments.first() else {
                return Err(format!("mutate-file needs a path; {MODE_USAGE}"));
            };
            let input = read_stdin()?;
            fs::write(Path::new(path), EXTERNAL_EDIT).map_err(|error| {
                format!(
                    "mutate-file could not write {}: {error}",
                    path.to_string_lossy()
                )
            })?;
            write_stdout(&input)
        }
        "args" => {
            // Literal-argument and working-directory context, hex encoded so a
            // non-UTF-8 argument or path survives the round trip exactly.
            let input = read_stdin()?;
            let working_directory = std::env::current_dir().map_err(|error| error.to_string())?;
            let mut report = Vec::new();
            report.extend_from_slice(b"CWD ");
            report.extend_from_slice(
                hex(working_directory.into_os_string().as_encoded_bytes()).as_bytes(),
            );
            report.push(b'\n');
            for argument in arguments {
                report.extend_from_slice(b"ARG ");
                report.extend_from_slice(hex(argument.as_encoded_bytes()).as_bytes());
                report.push(b'\n');
            }
            report.extend_from_slice(b"STDIN ");
            report.extend_from_slice(hex(&input).as_bytes());
            report.push(b'\n');
            write_stdout(&report)
        }
        "diagnostics" => {
            read_stdin()?;
            let mut stderr = io::stderr().lock();
            // Control characters, a long middle, and invalid UTF-8: a
            // presentation-layer summary has to survive all three.
            stderr
                .write_all(b"fixture diagnostics: \x1b[31mred\x07 bell \x00 nul ")
                .map_err(|error| error.to_string())?;
            stderr
                .write_all(&vec![b'x'; 3000])
                .map_err(|error| error.to_string())?;
            stderr
                .write_all(b" \xff\xfe raw\n")
                .map_err(|error| error.to_string())?;
            stderr.flush().map_err(|error| error.to_string())?;
            std::process::exit(i32::from(STATUS_DIAGNOSTICS));
        }
        other => Err(format!("unknown formatter mode {other:?}; {MODE_USAGE}")),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
        text.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('?'));
    }
    text
}

fn read_stdin() -> Result<Vec<u8>, String> {
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut input)
        .map_err(|error| format!("could not read stdin: {error}"))?;
    Ok(input)
}

fn replacement(transform: impl FnOnce(&[u8]) -> Vec<u8>) -> Result<(), String> {
    let input = read_stdin()?;
    write_stdout(&transform(&input))
}

fn write_stdout(bytes: &[u8]) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(bytes)
        .map_err(|error| format!("could not write stdout: {error}"))?;
    stdout.flush().map_err(|error| error.to_string())
}

fn write_stderr(bytes: &[u8]) -> Result<(), String> {
    let mut stderr = io::stderr().lock();
    stderr
        .write_all(bytes)
        .map_err(|error| format!("could not write stderr: {error}"))?;
    stderr.flush().map_err(|error| error.to_string())
}

/// Writes both output streams past a pipe buffer while alternating between
/// them, so a caller that drains only one stream stalls instead of passing.
fn write_interleaved(chunks: usize) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let pattern: Vec<u8> = std::iter::repeat_n(*b"0123456789abcdef", 1024)
        .flatten()
        .collect();
    let stdout_piece = &pattern[..STDOUT_CHUNK];
    let stderr_piece = &pattern[..STDERR_CHUNK];
    for _ in 0..chunks {
        stdout
            .write_all(stdout_piece)
            .map_err(|error| format!("could not write stdout: {error}"))?;
        stderr
            .write_all(stderr_piece)
            .map_err(|error| format!("could not write stderr: {error}"))?;
    }
    stdout.flush().map_err(|error| error.to_string())?;
    stderr.flush().map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// Scenarios: the long or blocking behaviors a parent bounds with a deadline.
// ---------------------------------------------------------------------------

const SCENARIO_USAGE: &str = "'scenario NAME' where NAME is one of: flood, drain, hang, hang-open";

fn run_scenario(rest: &[OsString]) -> Result<(), String> {
    let Some(name) = rest.first().and_then(|name| name.to_str()) else {
        return Err(SCENARIO_USAGE.to_owned());
    };

    match name {
        // Well past the pipe buffer on both streams, with interleaved writes.
        "flood" => {
            read_stdin()?;
            write_interleaved(256)
        }
        // Reports how many stdin bytes arrived, so a caller can prove stdin was
        // closed rather than left open.
        "drain" => {
            let input = read_stdin()?;
            write_stdout(format!("DRAINED {}\n", input.len()).as_bytes())
        }
        // Announces itself, then never finishes: containment tests kill this.
        "hang" => {
            write_stdout(b"SCENARIO_HANGING\n")?;
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        // Consumes stdin forever and never exits or closes its streams.
        "hang-open" => {
            write_stdout(b"SCENARIO_HANGING_OPEN\n")?;
            let mut stdin = io::stdin().lock();
            let mut scratch = [0u8; 4096];
            loop {
                match stdin.read(&mut scratch) {
                    Ok(0) => std::thread::sleep(Duration::from_secs(3600)),
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => std::thread::sleep(Duration::from_secs(3600)),
                }
            }
        }
        other => Err(format!("unknown scenario {other:?}; {SCENARIO_USAGE}")),
    }
}

// ---------------------------------------------------------------------------
// No-argument scenarios: shared-library behavior that needs no terminal.
// ---------------------------------------------------------------------------

struct Scenario {
    name: &'static str,
    run: fn() -> Result<(), String>,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "fixture_protocol_matrix",
            run: fixture_protocol_matrix,
        },
        Scenario {
            name: "guarded_save_commits_and_reports_observation",
            run: guarded_save_commits_and_reports_observation,
        },
        Scenario {
            name: "guarded_save_refuses_external_mutation",
            run: guarded_save_refuses_external_mutation,
        },
        Scenario {
            name: "document_preserves_exact_bytes",
            run: document_preserves_exact_bytes,
        },
        Scenario {
            name: "library_identity_rewrite_and_empty",
            run: library_identity_rewrite_and_empty,
        },
        Scenario {
            name: "library_range_bom_stale_and_validation",
            run: library_range_bom_stale_and_validation,
        },
        Scenario {
            name: "library_warnings_and_diagnostics",
            run: library_warnings_and_diagnostics,
        },
        Scenario {
            name: "library_literal_arguments_and_working_directory",
            run: library_literal_arguments_and_working_directory,
        },
        Scenario {
            name: "editor_format_whole_and_visual_parity",
            run: editor_format_whole_and_visual_parity,
        },
    ]
}

fn run_default_scenarios() -> ExitCode {
    let mut failures = 0;
    for scenario in scenarios() {
        match (scenario.run)() {
            Ok(()) => println!("scenario ok: {}", scenario.name),
            Err(message) => {
                failures += 1;
                println!("scenario FAILED: {}: {message}", scenario.name);
            }
        }
    }
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        println!("{failures} scenario(s) failed");
        ExitCode::FAILURE
    }
}

fn check(condition: bool, message: impl Into<String>) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn check_eq<T: PartialEq + std::fmt::Debug>(
    actual: T,
    expected: T,
    what: impl Into<String>,
) -> Result<(), String> {
    let what = what.into();
    check(
        actual == expected,
        format!("{what}: expected {expected:?}, found {actual:?}"),
    )
}

fn fixture_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("could not locate this fixture: {error}"))
}

/// Runs this same binary with `args`, feeding `input` on stdin.
///
/// A broken pipe while writing is not an error here: a mode may deliberately
/// exit without reading its stdin, and that is one of the behaviors under test.
/// Small inputs only: this writes stdin before reading either output stream, so
/// large-stream behavior belongs to the Python deadline harness.
fn run_self<S: AsRef<OsStr>>(args: &[S], input: &[u8]) -> Result<Output, String> {
    let mut child = Command::new(fixture_path()?)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start the fixture: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(input) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
            Err(error) => return Err(format!("could not write fixture stdin: {error}")),
        }
    }
    child
        .wait_with_output()
        .map_err(|error| format!("could not collect fixture output: {error}"))
}

fn run_self_fixture(mode: &str, input: &[u8]) -> Result<Output, String> {
    run_self(&["fixture", mode], input)
}

fn expect_success(output: &Output, what: &str) -> Result<(), String> {
    check(
        output.status.success(),
        format!(
            "{what} failed with {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// The modes `tests/terminal_smoke.py` and the PTY/CLI tests rely on, checked
/// byte for byte so a fixture change cannot silently weaken them.
fn fixture_protocol_matrix() -> Result<(), String> {
    let listing = run_self_fixture("modes", b"")?;
    expect_success(&listing, "listing formatter modes")?;
    let names: Vec<&str> = std::str::from_utf8(&listing.stdout)
        .map_err(|error| format!("mode listing is not text: {error}"))?
        .lines()
        .collect();
    check_eq(names, mode_names().to_vec(), "formatter mode listing")?;

    let identity = run_self_fixture("identity", "héllo\nwörld\n".as_bytes())?;
    expect_success(&identity, "identity")?;
    check_eq(
        identity.stdout.as_slice(),
        "héllo\nwörld\n".as_bytes(),
        "identity stdout",
    )?;
    check_eq(
        identity.stderr.as_slice(),
        b"".as_slice(),
        "identity stderr",
    )?;

    let rewrite = run_self_fixture("rewrite", b"aBc\xff\n")?;
    expect_success(&rewrite, "rewrite")?;
    check_eq(
        rewrite.stdout.as_slice(),
        b"ABC\xff\n".as_slice(),
        "rewrite stdout",
    )?;

    let empty = run_self_fixture("empty", b"gone\n")?;
    expect_success(&empty, "empty")?;
    check_eq(empty.stdout.as_slice(), b"".as_slice(), "empty stdout")?;

    let warning = run_self_fixture("warning", b"kept\n")?;
    expect_success(&warning, "warning")?;
    check_eq(
        warning.stdout.as_slice(),
        b"kept\n".as_slice(),
        "warning stdout",
    )?;
    check(
        String::from_utf8_lossy(&warning.stderr).contains("warning"),
        "warning mode must explain itself on stderr",
    )?;

    let error = run_self_fixture("error", b"rejected\n")?;
    check_eq(
        error.status.code(),
        Some(i32::from(STATUS_ERROR)),
        "error mode status",
    )?;
    check_eq(error.stdout.as_slice(), b"".as_slice(), "error stdout")?;
    check(
        String::from_utf8_lossy(&error.stderr).contains("rejected"),
        "error mode must describe the failure on stderr",
    )?;

    let invalid = run_self_fixture("invalid-utf8", b"input\n")?;
    expect_success(&invalid, "invalid-utf8")?;
    check(
        std::str::from_utf8(&invalid.stdout).is_err(),
        "invalid-utf8 stdout must not be valid UTF-8",
    )?;

    let large = run_self_fixture("large", b"input\n")?;
    expect_success(&large, "large")?;
    check_eq(large.stdout.len(), 64 * STDOUT_CHUNK, "large stdout length")?;
    check_eq(large.stderr.len(), 64 * STDERR_CHUNK, "large stderr length")?;

    let early = run_self_fixture("early-close", b"never read\n")?;
    expect_success(&early, "early-close")?;
    check_eq(
        early.stdout.as_slice(),
        b"partial\n".as_slice(),
        "early-close stdout",
    )?;

    let diagnostics = run_self_fixture("diagnostics", b"input\n")?;
    check_eq(
        diagnostics.status.code(),
        Some(i32::from(STATUS_DIAGNOSTICS)),
        "diagnostics mode status",
    )?;
    check_eq(
        diagnostics.stdout.as_slice(),
        b"".as_slice(),
        "diagnostics stdout",
    )?;
    check(
        std::str::from_utf8(&diagnostics.stderr).is_err(),
        "diagnostics stderr must include invalid UTF-8",
    )?;

    let arguments = run_self_fixture("args", b"stdin bytes\n")?;
    expect_success(&arguments, "args")?;
    let report = String::from_utf8_lossy(&arguments.stdout);
    check(
        report.contains("CWD "),
        "args mode must report its working directory",
    )?;
    check(
        report.contains(&format!("STDIN {}", hex(b"stdin bytes\n"))),
        "args mode must report the stdin it received",
    )?;

    let unknown = run_self_fixture("no-such-mode", b"")?;
    check_eq(unknown.status.code(), Some(2), "unknown mode status")?;
    check_eq(
        unknown.stdout.as_slice(),
        b"".as_slice(),
        "unknown mode stdout",
    )?;

    Ok(())
}

fn temporary_directory() -> Result<tempfile::TempDir, String> {
    tempfile::tempdir().map_err(|error| format!("could not create a temporary directory: {error}"))
}

fn directory_entries(directory: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn guarded_save_commits_and_reports_observation() -> Result<(), String> {
    let directory = temporary_directory()?;
    let path = directory.path().join("document.txt");
    fs::write(&path, b"original\n").map_err(|error| error.to_string())?;

    let expected = postvim::storage::observe(&path).map_err(|error| error.to_string())?;
    let committed = postvim::storage::save_document_guarded(&path, b"replacement\n", &expected)
        .map_err(|error| format!("guarded save should have succeeded: {error}"))?;

    check_eq(
        fs::read(&path).map_err(|error| error.to_string())?,
        b"replacement\n".to_vec(),
        "saved bytes",
    )?;
    let reopened = postvim::storage::observe(&path).map_err(|error| error.to_string())?;
    check_eq(
        committed,
        reopened,
        "committed observation matches a fresh read",
    )?;
    check_eq(
        directory_entries(directory.path())?,
        vec!["document.txt".to_owned()],
        "no temporary file is left behind",
    )?;
    Ok(())
}

/// A second process changes the destination while this one holds an older
/// observation, which is exactly the external-edit case a guarded save must
/// refuse. The change is made by the fixture's own `mutate-file` mode, so no
/// sleep or timing guess is involved.
fn guarded_save_refuses_external_mutation() -> Result<(), String> {
    let directory = temporary_directory()?;
    let path = directory.path().join("document.txt");
    fs::write(&path, b"original\n").map_err(|error| error.to_string())?;
    let expected = postvim::storage::observe(&path).map_err(|error| error.to_string())?;

    let mutation = run_self(
        &[
            "fixture",
            "mutate-file",
            path.to_str().ok_or("path is not UTF-8")?,
        ],
        b"",
    )?;
    expect_success(&mutation, "external mutation")?;

    match postvim::storage::save_document_guarded(&path, b"local edit\n", &expected) {
        Err(error) => check(
            error.is_conflict(),
            format!("expected a conflict, found {error}"),
        )?,
        Ok(_) => return Err("a guarded save overwrote an externally changed file".to_owned()),
    }

    check_eq(
        fs::read(&path).map_err(|error| error.to_string())?,
        EXTERNAL_EDIT.to_vec(),
        "the external bytes survived the refused save",
    )?;
    check_eq(
        directory_entries(directory.path())?,
        vec!["document.txt".to_owned()],
        "a refused save leaves no temporary file behind",
    )?;
    Ok(())
}

/// Exact-byte document behavior the formatter contract depends on: a leading
/// U+FEFF from file loading is a BOM, while the same code point arriving as
/// editable text stays text.
fn document_preserves_exact_bytes() -> Result<(), String> {
    let mixed = "\u{feff}alpha\r\nbeta\ngamma\rdelta";
    let document = postvim::document::Document::from_text(mixed);
    check(document.has_bom(), "an initial U+FEFF must load as a BOM")?;
    check_eq(
        document.editable_text(),
        "alpha\r\nbeta\ngamma\rdelta".to_owned(),
        "editable text excludes the BOM",
    )?;
    check_eq(document.serialize(), mixed.to_owned(), "serialized bytes")?;

    let formatted = postvim::document::Document::from_editable_text("\u{feff}alpha\n", false);
    check(
        !formatted.has_bom(),
        "editable text starting with U+FEFF is not a BOM",
    )?;
    check_eq(
        formatted.editable_text(),
        "\u{feff}alpha\n".to_owned(),
        "the formatter's U+FEFF stays editable",
    )?;
    check_eq(
        formatted.serialize(),
        "\u{feff}alpha\n".to_owned(),
        "no BOM is invented around formatter output",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared-library and editor scenarios.
//
// These run the same requests the interactive editor and the CLI run, so a
// library-only regression shows up here as an ordinary `cargo test` failure.
// ---------------------------------------------------------------------------

fn self_formatter() -> FormatterSpec {
    FormatterSpec::new(std::env::current_exe().expect("the fixture can locate itself"))
}

fn attempt_transform(
    source: TransformSource,
    arguments: &[&str],
) -> Result<TransformResult, TransformError> {
    // This fixture doubles as the formatter, so each run starts with the
    // `fixture` subcommand and then the mode the scenario asked for.
    TransformRequest::new(
        source,
        self_formatter()
            .args(["fixture"])
            .args(arguments.iter().copied()),
    )
    .execute()
}

fn transform_or_fail(
    source: TransformSource,
    arguments: &[&str],
) -> Result<TransformResult, String> {
    attempt_transform(source, arguments).map_err(|error| format!("transform failed: {error}"))
}

fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err(format!("odd-length hexadecimal text: {text:?}"));
    }
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let digits = std::str::from_utf8(pair).map_err(|error| error.to_string())?;
        decoded.push(
            u8::from_str_radix(digits, 16)
                .map_err(|error| format!("invalid hexadecimal text {digits:?}: {error}"))?,
        );
    }
    Ok(decoded)
}

/// One word of an interactive `:format` command line, quoted only when needed.
fn interactive_word(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text
        .chars()
        .any(|character| character.is_whitespace() || character == '\'' || character == '"')
    {
        format!("'{}'", text.replace('\'', "'\\''"))
    } else {
        text.into_owned()
    }
}

fn run_ex_command(editor: &mut Editor, command: &str) -> Result<(), String> {
    editor.handle_input(EditorInput::Character(':'));
    check_eq(
        editor.mode(),
        EditorMode::Command,
        "the command prompt opens",
    )?;
    editor.handle_input(EditorInput::Text(command.to_owned()));
    editor.handle_input(EditorInput::Enter);
    check_eq(
        editor.mode(),
        EditorMode::Normal,
        format!("after :{command}: {:?}", editor.message()),
    )?;
    let message = editor.message().unwrap_or_default();
    check(
        message.starts_with("Formatted"),
        format!(":{command} should report success, found {message:?}"),
    )
}

fn library_identity_rewrite_and_empty() -> Result<(), String> {
    let text = "alpha\n";
    let document = Document::from_editable_text(text, false);

    let identity = transform_or_fail(TransformSource::whole(text, false), &["identity"])?;
    check_eq(
        identity.replacement().to_owned(),
        text.to_owned(),
        "identity replacement",
    )?;
    check(!identity.changed(), "identity output is not a change")?;
    let applied = identity
        .apply(&document)
        .map_err(|error| error.to_string())?;
    check_eq(
        applied.serialize(),
        text.to_owned(),
        "identity application keeps the bytes",
    )?;

    let rewrite = transform_or_fail(TransformSource::whole(text, false), &["rewrite"])?;
    check_eq(
        rewrite.replacement().to_owned(),
        "ALPHA\n".to_owned(),
        "rewrite replacement",
    )?;
    check(rewrite.changed(), "rewrite output is a change")?;
    let applied = rewrite
        .apply(&document)
        .map_err(|error| error.to_string())?;
    check_eq(
        applied.editable_text(),
        "ALPHA\n".to_owned(),
        "rewrite application",
    )?;

    let empty = transform_or_fail(TransformSource::whole(text, false), &["empty"])?;
    check_eq(
        empty.replacement().to_owned(),
        String::new(),
        "empty output is an empty replacement",
    )?;
    let applied = empty.apply(&document).map_err(|error| error.to_string())?;
    check_eq(
        applied.serialize(),
        String::new(),
        "empty output deletes the whole document",
    )?;
    Ok(())
}

fn library_range_bom_stale_and_validation() -> Result<(), String> {
    let text = "alpha beta\n";
    let result = transform_or_fail(
        TransformSource::new(text, true, ByteRange::new(6, 10)),
        &["rewrite"],
    )?;
    check_eq(
        result.replacement().to_owned(),
        "BETA".to_owned(),
        "range replacement",
    )?;

    let original = Document::from_editable_text(text, true);
    let applied = result.apply(&original).map_err(|error| error.to_string())?;
    check_eq(
        applied.editable_text(),
        "alpha BETA\n".to_owned(),
        "bytes outside the range are preserved",
    )?;
    check(applied.has_bom(), "the BOM survives a range replacement")?;
    check_eq(
        applied.serialize(),
        "\u{feff}alpha BETA\n".to_owned(),
        "serialized bytes keep one BOM",
    )?;

    match result.apply(&Document::from_editable_text("alpha gamma\n", true)) {
        Err(TransformError::StaleSource) => {}
        Err(other) => return Err(format!("expected a stale-source rejection, found {other}")),
        Ok(_) => return Err("a stale source was applied".to_owned()),
    }
    match result.apply(&Document::from_editable_text(text, false)) {
        Err(TransformError::StaleSource) => {}
        Err(other) => {
            return Err(format!(
                "a BOM-only change must also count as a stale source, found {other}"
            ));
        }
        Ok(_) => return Err("a BOM-only change was treated as the same source".to_owned()),
    }

    for (description, range, source) in [
        ("reversed", ByteRange::new(10, 6), text),
        ("past the end", ByteRange::new(0, 99), text),
        ("splitting a grapheme", ByteRange::new(0, 1), "é\n"),
        ("splitting a CRLF pair", ByteRange::new(0, 2), "a\r\nb"),
    ] {
        match TransformSource::new(source, false, range).validate() {
            Err(TransformError::InvalidRange { .. }) => {}
            other => return Err(format!("{description} range was accepted: {other:?}")),
        }
    }

    let zero_length = TransformSource::new("alpha\n", false, ByteRange::new(5, 5));
    check(
        zero_length.validate().is_ok(),
        "a zero-length range at a document end is valid",
    )?;
    let whole = TransformSource::new("alpha\n", false, ByteRange::new(0, 6));
    check(whole.validate().is_ok(), "a whole-document range is valid")?;
    Ok(())
}

fn library_warnings_and_diagnostics() -> Result<(), String> {
    let text = "kept\n";
    let warning = transform_or_fail(TransformSource::whole(text, false), &["warning"])?;
    check_eq(
        warning.replacement().to_owned(),
        text.to_owned(),
        "a successful warning leaves the replacement text alone",
    )?;
    let warnings = warning
        .warnings()
        .ok_or("successful stderr must surface as warnings")?;
    check(
        warnings.contains("warning"),
        format!("warnings should name themselves: {warnings}"),
    )?;
    let applied = warning
        .apply(&Document::from_editable_text(text, false))
        .map_err(|error| error.to_string())?;
    check_eq(
        applied.editable_text(),
        text.to_owned(),
        "stderr never becomes document text",
    )?;

    match attempt_transform(TransformSource::whole(text, false), &["error"]) {
        Err(TransformError::Exit { status, stderr }) => {
            check_eq(
                status.code(),
                Some(i32::from(STATUS_ERROR)),
                "formatter status",
            )?;
            check(
                String::from_utf8_lossy(&stderr).contains("rejected"),
                "raw stderr bytes stay available to callers",
            )?;
        }
        other => return Err(format!("expected a nonzero exit, found {other:?}")),
    }

    match attempt_transform(TransformSource::whole(text, false), &["invalid-utf8"]) {
        Err(TransformError::InvalidUtf8Output { .. }) => {}
        other => {
            return Err(format!(
                "expected an invalid-UTF-8 rejection, found {other:?}"
            ));
        }
    }

    let diagnostics = match attempt_transform(TransformSource::whole(text, false), &["diagnostics"])
    {
        Err(error) => error,
        Ok(_) => return Err("the diagnostics mode must fail".to_owned()),
    };
    check(
        diagnostics.stderr().len() > 3000,
        "raw diagnostics keep their full length",
    )?;
    let summary = diagnostics
        .stderr_summary()
        .ok_or("failed diagnostics must be summarized for presentation")?;
    check(
        !summary.contains('\u{1b}'),
        "the summary escapes control characters",
    )?;
    check(
        summary.chars().count()
            <= DIAGNOSTIC_SUMMARY_LIMIT + DIAGNOSTIC_TRUNCATION_MARKER.chars().count(),
        "the summary respects its scalar limit",
    )?;
    check(
        summary.ends_with(DIAGNOSTIC_TRUNCATION_MARKER),
        "long diagnostics end with the truncation marker",
    )?;

    check_eq(
        transform::diagnostic_summary(b"plain words\n"),
        "plain words\\n".to_owned(),
        "a short diagnostic is escaped, not copied byte for byte",
    )?;
    Ok(())
}

fn library_literal_arguments_and_working_directory() -> Result<(), String> {
    let directory = temporary_directory()?;
    let expected_cwd = fs::canonicalize(directory.path()).map_err(|error| error.to_string())?;
    let literal = ["args", "|", "$HOME", ">", "hé llo.txt"];
    let formatter = self_formatter()
        .args(["fixture"])
        .args(literal)
        .working_directory(directory.path());
    let report = TransformRequest::new(TransformSource::whole("stdin bytes\n", false), formatter)
        .execute()
        .map_err(|error| format!("transform failed: {error}"))?;
    let report = report.replacement().to_owned();

    let arguments: Vec<&str> = report
        .lines()
        .filter_map(|line| line.strip_prefix("ARG "))
        .collect();
    check_eq(arguments.len(), literal.len() - 1, "argument count")?;
    let decoded: Vec<Vec<u8>> = arguments
        .iter()
        .map(|line| unhex(line))
        .collect::<Result<_, _>>()?;
    check_eq(
        decoded,
        literal[1..]
            .iter()
            .map(|argument| argument.as_bytes().to_vec())
            .collect::<Vec<_>>(),
        "formatter arguments arrive literally, with no expansion",
    )?;

    let cwd_line = report
        .lines()
        .find_map(|line| line.strip_prefix("CWD "))
        .ok_or("the formatter should report its working directory")?;
    check_eq(
        unhex(cwd_line)?,
        expected_cwd.into_os_string().as_encoded_bytes().to_vec(),
        "the request's working directory is the one the formatter ran in",
    )?;
    let stdin_line = report
        .lines()
        .find_map(|line| line.strip_prefix("STDIN "))
        .ok_or("the formatter should report the stdin it received")?;
    check_eq(
        unhex(stdin_line)?,
        b"stdin bytes\n".to_vec(),
        "the selection, and only the selection, is sent on stdin",
    )?;
    Ok(())
}

/// The editor and the library must produce the same document for the same
/// request, whether the range came from a Visual selection or the whole buffer.
fn editor_format_whole_and_visual_parity() -> Result<(), String> {
    let text = "alpha beta gamma\n";
    let command = format!(
        "format {} fixture rewrite",
        interactive_word(&fixture_path()?)
    );

    let library_whole = transform_or_fail(TransformSource::whole(text, false), &["rewrite"])?;
    let library_whole_document = library_whole
        .apply(&Document::from_editable_text(text, false))
        .map_err(|error| error.to_string())?;

    let mut editor = Editor::from_document(Document::from_editable_text(text, false));
    run_ex_command(&mut editor, &command)?;
    check_eq(
        editor.document().editable_text(),
        library_whole_document.editable_text(),
        "the editor and the library apply the same whole-document request",
    )?;
    check_eq(
        editor.document().editable_text(),
        "ALPHA BETA GAMMA\n".to_owned(),
        "whole-document format",
    )?;
    check(editor.is_dirty(), "formatting is one unsaved change")?;
    check(
        editor.external_status().is_none(),
        "formatting does not change disk expectations",
    )?;

    editor.handle_input(EditorInput::Character('u'));
    check_eq(
        editor.document().editable_text(),
        text.to_owned(),
        "one undo restores the text",
    )?;
    check(
        !editor.is_dirty(),
        "undoing back to the saved bytes is clean again",
    )?;

    editor.handle_input(EditorInput::Character('.'));
    check_eq(
        editor.document().editable_text(),
        text.to_owned(),
        "dot repeats key recipes, never an external program",
    )?;

    let library_visual = transform_or_fail(
        TransformSource::new(text, false, ByteRange::new(6, 10)),
        &["rewrite"],
    )?;
    let library_visual_document = library_visual
        .apply(&Document::from_editable_text(text, false))
        .map_err(|error| error.to_string())?;

    let mut editor = Editor::from_document(Document::from_editable_text(text, false));
    editor.set_cursor(Cursor::new(0, 6));
    editor.handle_input(EditorInput::Character('v'));
    editor.handle_input(EditorInput::Character('e'));
    let selection = editor
        .selection()
        .ok_or("a characterwise Visual selection should be active")?;
    check_eq(selection.start, Cursor::new(0, 6), "selection start")?;
    check_eq(selection.end, Cursor::new(0, 9), "selection end")?;
    run_ex_command(&mut editor, &command)?;
    check_eq(
        editor.document().editable_text(),
        library_visual_document.editable_text(),
        "the editor and the library apply the same selected range",
    )?;
    check_eq(
        editor.document().editable_text(),
        "alpha BETA gamma\n".to_owned(),
        "only the selected text changes",
    )?;
    check_eq(
        editor.mode(),
        EditorMode::Normal,
        "formatting returns to Normal",
    )?;
    check_eq(
        editor.cursor(),
        Cursor::new(0, 6),
        "the selection start is the restored cursor",
    )?;
    Ok(())
}
