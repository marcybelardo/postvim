use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

// Keep stdin open until the child exits: waiting for EOF would hang without
// the deadline. These bounded reports fit comfortably in the captured pipes.
fn run(directory: &Path, args: &[OsString]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_postvim"))
        .current_dir(directory).args(args).stdin(Stdio::piped())
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("context child did not exit while stdin remained open: {result:?}");
            }
        }
    }
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    output
}
fn args(values: &[&str]) -> Vec<OsString> { values.iter().map(OsString::from).collect() }

#[test]
fn context_cli_non_tty_documented_example() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = "alpha\nβeta\ngamma\ndelta\nomega\n".as_bytes();
    fs::write(dir.path().join("notes.txt"), bytes).unwrap();
    let output = run(dir.path(), &args(&["--context", "--lines", "2:3", "--surround", "1", "--", "notes.txt"]));
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "File: notes.txt\nSource: saved file on disk (not live editor state)\nLines: 2:3 of 6\nSurrounding lines: 1\nContext before:\n1 | alpha\nSelected text:\n2 | βeta\n3 | gamma\nContext after:\n4 | delta\n");
    assert_eq!(fs::read(dir.path().join("notes.txt")).unwrap(), bytes);
    fs::write(dir.path().join("-"), "literal").unwrap();
    let output = run(dir.path(), &args(&["--context", "--lines", "1:1", "--", "-"]));
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().ends_with("Selected text:\n1 | literal\n"));
}

#[test]
fn context_cli_escapes_control_content_and_path() {
    let dir = tempfile::tempdir().unwrap();
    // Backslash is a path separator on Windows, so path controls are Unix-only.
    let name = if cfg!(unix) { "notes\\\t\u{1b}\n" } else { "notes.txt" };
    let bytes = "β\\\t\0\u{1b}\u{7f}\u{85}\r\n".as_bytes();
    fs::write(dir.path().join(name), bytes).unwrap();
    let output = run(dir.path(), &args(&["--context", "--lines", "1:2", "--", name]));
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let escaped_name = if cfg!(unix) { "notes\\\\\\t\\u{1b}\\n" } else { "notes.txt" };
    assert_eq!(stdout, format!("File: {escaped_name}\nSource: saved file on disk (not live editor state)\nLines: 1:2 of 2\nSurrounding lines: 3\nSelected text:\n1 | β\\\\\\t\\u{{0}}\\u{{1b}}\\u{{7f}}\\u{{85}}\n2 | \n"));
    assert_eq!(fs::read(dir.path().join(name)).unwrap(), bytes);
}

#[test]
fn context_cli_input_errors() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("invalid"), [0xff]).unwrap();
    fs::write(dir.path().join("short"), "one").unwrap();
    fs::create_dir(dir.path().join("directory")).unwrap();
    for values in [
        vec!["--context", "--lines", "1:1", "--", "missing"],
        vec!["--context", "--lines", "1:1", "--", "invalid"],
        vec!["--context", "--lines", "1:1", "--", "directory"],
        vec!["--context", "--lines", "2:2", "--", "short"],
        vec!["--context", "--lines", "1:2", "--", "short"],
        vec!["--context", "--lines", "0:1", "--", "missing"],
        vec!["--context", "--lines", "1:1", "--", "missing", "extra"],
    ] {
        let output = run(dir.path(), &args(&values));
        assert!(!output.status.success(), "{values:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        assert!(!output.stderr.contains(&0x1b));
    }
    assert!(!dir.path().join("missing").exists());
    let output = run(dir.path(), &args(&["--context", "--bad\\\t\u{1b}"]));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"postvim: unknown --context option: --bad\\\\\\t\\u{1b}\n");
    #[cfg(unix)] {
        let output = run(dir.path(), &args(&["--context", "--lines", "1:1", "--", "missing\\\t\u{1b}"]));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"postvim: missing\\\\\\t\\u{1b}: no such file; --context requires an existing regular UTF-8 file\n");
        assert!(!dir.path().join("missing\\\t\u{1b}").exists());
        fs::create_dir(dir.path().join("directory\\\t\u{1b}")).unwrap();
        let output = run(dir.path(), &args(&["--context", "--lines", "1:1", "--", "directory\\\t\u{1b}"]));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"postvim: directory\\\\\\t\\u{1b}: path is not a regular file\n");
    }
}
