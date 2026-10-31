//! Read-only, terminal-independent reports of saved document lines.
//!
//! Errors remain unescaped until the caller's diagnostic boundary. Report
//! metadata and content are escaped here, exactly once.
use std::fmt::Write as _;
use std::io::Write;

use crate::cli::ContextInvocation;
use crate::document::Document;
use crate::storage::{self, DiskContents};

/// Escape report text or one complete raw context diagnostic.
pub fn escape(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.extend(ch.escape_default()),
            ch => escaped.push(ch),
        }
    }
    escaped
}

/// Construct the complete report before emitting any output.
///
/// This validates direct library invocations as well as parsed arguments, reads
/// one disk observation, and uses the editor's document line-ending semantics.
pub fn build_report(invocation: &ContextInvocation) -> Result<String, String> {
    let range = invocation.lines;
    if invocation.path.as_os_str().is_empty()
        || range.start == 0
        || range.start > range.end
        || invocation.surrounding_lines > 20
    {
        return Err("invalid context invocation: nonempty path, positive START <= END, and surround 0..20 required".into());
    }
    let path = invocation.path.display();
    let observation = storage::observe(&invocation.path)
        .map_err(|error| format!("{path}: {error}"))?;
    let DiskContents::Present(bytes) = observation.contents else {
        return Err(format!("{path}: no such file; --context requires an existing regular UTF-8 file"));
    };
    let text = String::from_utf8(bytes).map_err(|_| format!("{path}: not valid UTF-8"))?;
    let document = Document::from_text(&text);
    let total = document.line_count();
    if range.start > total || range.end > total {
        return Err(format!("--lines {}:{} exceeds document line count {total}", range.start, range.end));
    }
    let mut report = format!(
        "File: {}\nSource: saved file on disk (not live editor state)\nLines: {}:{} of {total}\nSurrounding lines: {}\n",
        escape(&path.to_string()), range.start, range.end, invocation.surrounding_lines,
    );
    // Use half-open zero-based indices internally to avoid inclusive-end
    // arithmetic at usize::MAX and keep the three sections disjoint.
    let start = range.start - 1;
    let end = range.end;
    let before = start.saturating_sub(invocation.surrounding_lines);
    let after = end.saturating_add(invocation.surrounding_lines).min(total);
    for (heading, rows) in [
        ("Context before:", before..start),
        ("Selected text:", start..end),
        ("Context after:", end..after),
    ] {
        if rows.is_empty() {
            continue;
        }
        writeln!(report, "{heading}").expect("writing to a String cannot fail");
        for row in rows {
            writeln!(report, "{} | {}", row + 1, escape(document.line(row).expect("validated row")))
                .expect("writing to a String cannot fail");
        }
    }
    Ok(report)
}

/// Write and flush a validated report. Only output errors can leave partial output.
pub fn run_context(invocation: &ContextInvocation, stdout: &mut dyn Write) -> Result<(), String> {
    let report = build_report(invocation)?;
    stdout.write_all(report.as_bytes()).and_then(|()| stdout.flush())
        .map_err(|error| format!("cannot write context report to standard output: {error}"))
}
