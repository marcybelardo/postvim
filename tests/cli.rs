use std::ffi::OsString;
use std::path::PathBuf;

use postvim::cli::{CliAction, parse_args_os};

fn parse(args: &[&str]) -> Result<CliAction, String> {
    parse_args_os(args.iter().map(OsString::from))
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
}

#[test]
fn help_documents_bounded_commands() {
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
