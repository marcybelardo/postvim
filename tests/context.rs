use std::fs;
use std::io::{self, Write};
use std::path::Path;

use postvim::cli::{ContextInvocation, LineRange};
use postvim::context::{build_report, escape, run_context};

fn invocation(path: &Path, start: usize, end: usize, surrounding_lines: usize) -> ContextInvocation {
    ContextInvocation { path: path.into(), lines: LineRange { start, end }, surrounding_lines }
}

#[test]
fn context_selects_disjoint_numbered_sections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample");
    fs::write(&path, "a\nb\nc\nd\ne").unwrap();
    for (start, end, surround, suffix) in [
        (2, 3, 1, "Context before:\n1 | a\nSelected text:\n2 | b\n3 | c\nContext after:\n4 | d\n"),
        (1, 1, 20, "Selected text:\n1 | a\nContext after:\n2 | b\n3 | c\n4 | d\n5 | e\n"),
        (5, 5, 20, "Context before:\n1 | a\n2 | b\n3 | c\n4 | d\nSelected text:\n5 | e\n"),
        (2, 3, 0, "Selected text:\n2 | b\n3 | c\n"),
        (1, 5, 3, "Selected text:\n1 | a\n2 | b\n3 | c\n4 | d\n5 | e\n"),
    ] {
        let report = build_report(&invocation(&path, start, end, surround)).unwrap();
        assert!(report.ends_with(suffix), "{report}");
    }
    for (start, end, surround) in [(0, 1, 3), (2, 1, 3), (1, 1, 21), (1, 6, 3), (6, 6, 3), (usize::MAX, usize::MAX, 20), (1, usize::MAX, 20)] {
        let mut output = Vec::new();
        assert!(run_context(&invocation(&path, start, end, surround), &mut output).is_err());
        assert!(output.is_empty());
    }
    assert!(build_report(&invocation(Path::new(""), 1, 1, 0)).is_err());
}

#[test]
fn context_line_endings_bom_and_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample");
    for (text, count, suffix) in [
        ("", 1, "1 | \n"),
        ("\u{feff}", 1, "1 | \n"),
        ("\u{feff}α\r\nβ\rc\n", 4, "1 | α\n2 | β\n3 | c\n4 | \n"),
        ("a\n\nb", 3, "1 | a\n2 | \n3 | b\n"),
        ("a\n", 2, "1 | a\n2 | \n"),
    ] {
        fs::write(&path, text).unwrap();
        let report = build_report(&invocation(&path, 1, count, 0)).unwrap();
        assert!(report.contains(&format!("Lines: 1:{count} of {count}\n")));
        assert!(report.ends_with(suffix), "{report}");
        assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
    }
}

#[test]
fn context_report_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    fs::write(&path, "alpha\nβeta\ngamma\ndelta\nomega\n").unwrap();
    assert_eq!(build_report(&invocation(&path, 2, 3, 1)).unwrap(), format!(
        "File: {}\nSource: saved file on disk (not live editor state)\nLines: 2:3 of 6\nSurrounding lines: 1\nContext before:\n1 | alpha\nSelected text:\n2 | βeta\n3 | gamma\nContext after:\n4 | delta\n", escape(&path.display().to_string())));
    assert_eq!(escape("\\\t\0\r\n\u{1b}\u{7f}\u{85} β中🙂"), "\\\\\\t\\u{0}\\r\\n\\u{1b}\\u{7f}\\u{85} β中🙂");
}

#[test]
fn context_writer_failure() {
    struct Failing { flush: bool, bytes: Vec<u8> }
    impl Write for Failing {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.flush { return Err(io::Error::other("write\\\t\u{1b}")); }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Err(io::Error::other("flush\\\t\u{1b}")) }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample");
    fs::write(&path, "saved").unwrap();
    for flush in [false, true] {
        let mut writer = Failing { flush, bytes: Vec::new() };
        let error = run_context(&invocation(&path, 1, 1, 0), &mut writer).unwrap_err();
        assert_eq!(error, format!("cannot write context report to standard output: {}\\\t\u{1b}", if flush { "flush" } else { "write" }));
        assert!(escape(&error).ends_with(if flush { "flush\\\\\\t\\u{1b}" } else { "write\\\\\\t\\u{1b}" }));
        assert_eq!(!writer.bytes.is_empty(), flush);
        assert_eq!(fs::read(&path).unwrap(), b"saved");
    }
}

#[cfg(unix)]
#[test]
fn context_native_paths_and_symlinks() {
    use std::ffi::OsString;
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(OsString::from_vec(b"native\xff".to_vec()));
    fs::write(&path, "native").unwrap();
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    for path in [&path, &link] {
        let report = build_report(&invocation(path, 1, 1, 0)).unwrap();
        assert!(report.starts_with(&format!("File: {}\n", escape(&path.display().to_string()))));
        assert!(report.ends_with("Selected text:\n1 | native\n"));
    }
    fs::remove_file(&path).unwrap();
    assert!(build_report(&invocation(&link, 1, 1, 0)).is_err());
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
}
