//! Terminal-independent text transformation.
//!
//! A transformation is four separate, concrete things:
//!
//! - [`TransformSource`] captures the whole BOM-free editable text of a document
//!   plus a half-open UTF-8 byte range into it.
//! - [`FormatterSpec`] names an existing program with native literal arguments
//!   and an optional working directory.
//! - [`run_formatter`] runs that program synchronously and returns a
//!   [`TransformResult`] carrying the replacement text, the raw stderr bytes, and
//!   the process status.
//! - [`apply_replacement`] (or [`TransformResult::apply`]) splices the replacement
//!   into a document only when that document's text and BOM flag still match the
//!   request's source.
//!
//! Nothing here reads or writes a document path, touches a terminal, or applies an
//! automatic formatter choice: callers own presentation and persistence.
//!
//! # Trust limits
//!
//! The formatter is an explicitly chosen, trusted program run synchronously.
//! Capturing is not bounded in time or memory, the process is not sandboxed, and
//! neither cancellation nor process-tree containment is provided: a formatter that
//! hangs, emits without limit, or leaves descendants holding the pipes can delay
//! completion indefinitely. Prefer finite stdin-to-stdout tools. The direct child
//! is always killed/reaped and its I/O workers always joined on every failure path
//! this module can observe, including a panicking worker.
//!
//! Diagnostics are presented through [`diagnostic_summary`]: lossy, escaped, and
//! capped at [`DIAGNOSTIC_SUMMARY_LIMIT`] Unicode scalar values before an explicit
//! truncation marker. The raw stderr bytes stay available to callers.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation;

use crate::document::Document;

/// Interval between direct-child status checks while the pipes are drained.
///
/// This is a polling interval, not an operation timeout.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Maximum number of Unicode scalar values in a [`diagnostic_summary`].
pub const DIAGNOSTIC_SUMMARY_LIMIT: usize = 2048;

/// Appended to a [`diagnostic_summary`] that reached [`DIAGNOSTIC_SUMMARY_LIMIT`].
pub const DIAGNOSTIC_TRUNCATION_MARKER: &str = " … [diagnostic truncated]";

/// An existing formatter invoked with native, literal arguments.
///
/// The program is executed directly: no shell is involved, no expansion happens,
/// and no input file path is appended. A caller may explicitly name a shell as the
/// program, but Postvim never supplies one. Arguments are `OsString`-compatible so
/// native (possibly non-UTF-8) argv survives; the working directory is the only
/// execution context carried, and `None` inherits the caller's directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatterSpec {
    program: OsString,
    arguments: Vec<OsString>,
    working_directory: Option<PathBuf>,
}

impl FormatterSpec {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            working_directory: None,
        }
    }

    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    pub fn working_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }

    pub fn program(&self) -> &OsStr {
        &self.program
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    pub fn cwd(&self) -> Option<&PathBuf> {
        self.working_directory.as_ref()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments);
        if let Some(directory) = &self.working_directory {
            command.current_dir(directory);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

/// Half-open UTF-8 byte range into a document's BOM-free editable text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl ByteRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// The whole of `text`.
    pub fn whole(text: &str) -> Self {
        Self::new(0, text.len())
    }

    /// Byte length of the range, saturating for a reversed range.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// The text a transformation operates on plus the selected half-open range.
///
/// `text` is the entire editable text, excluding the file's initial BOM; only the
/// selected slice is ever passed to the formatter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransformSource {
    text: String,
    has_bom: bool,
    range: ByteRange,
}

impl TransformSource {
    pub fn new(text: impl Into<String>, has_bom: bool, range: ByteRange) -> Self {
        Self {
            text: text.into(),
            has_bom,
            range,
        }
    }

    /// The whole editable text of a document.
    pub fn whole(text: impl Into<String>, has_bom: bool) -> Self {
        let text = text.into();
        let range = ByteRange::whole(&text);
        Self::new(text, has_bom, range)
    }

    pub fn from_document(document: &Document, range: ByteRange) -> Self {
        Self::new(document.editable_text(), document.has_bom(), range)
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn has_bom(&self) -> bool {
        self.has_bom
    }

    pub fn range(&self) -> ByteRange {
        self.range
    }

    /// The selected slice, or an empty slice for a range this source's own
    /// [`TransformSource::validate`] would reject.
    pub fn selected(&self) -> &str {
        self.text
            .get(self.range.start..self.range.end)
            .unwrap_or("")
    }

    /// Checks the range against this source's editable text.
    pub fn validate(&self) -> Result<(), TransformError> {
        validate_range(&self.text, self.range)
    }
}

/// A validated text range plus the formatter meant to transform it.
#[derive(Clone, Debug)]
pub struct TransformRequest {
    source: TransformSource,
    formatter: FormatterSpec,
}

impl TransformRequest {
    pub fn new(source: TransformSource, formatter: FormatterSpec) -> Self {
        Self { source, formatter }
    }

    pub fn source(&self) -> &TransformSource {
        &self.source
    }

    pub fn formatter(&self) -> &FormatterSpec {
        &self.formatter
    }

    /// Runs the formatter synchronously; see [`run_formatter`].
    pub fn execute(&self) -> Result<TransformResult, TransformError> {
        run_formatter(self)
    }
}

/// The outcome of one successful formatter run.
///
/// The source is carried so application can prove the document has not changed
/// since the request was captured.
#[derive(Clone, Debug)]
pub struct TransformResult {
    replacement: String,
    stderr: Vec<u8>,
    status: ExitStatus,
    source: TransformSource,
}

impl TransformResult {
    pub fn replacement(&self) -> &str {
        &self.replacement
    }

    /// Raw formatter standard error, arbitrary bytes, never mixed into the text.
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    pub fn status(&self) -> ExitStatus {
        self.status
    }

    pub fn succeeded(&self) -> bool {
        self.status.success()
    }

    pub fn source(&self) -> &TransformSource {
        &self.source
    }

    /// Whether the replacement differs from the selected source text.
    pub fn changed(&self) -> bool {
        self.replacement != self.source.selected()
    }

    /// Sanitized formatter warnings, when a successful run wrote to stderr.
    pub fn warnings(&self) -> Option<String> {
        if self.stderr.is_empty() {
            return None;
        }
        Some(diagnostic_summary(&self.stderr))
    }

    /// Pure application of this result to `current`; see [`apply_replacement`].
    pub fn apply(&self, current: &Document) -> Result<Document, TransformError> {
        apply_replacement(current, &self.source, &self.replacement)
    }
}

/// Why a transformation did not produce an applicable result.
#[derive(Debug)]
pub enum TransformError {
    /// The range is reversed, out of bounds, splits a grapheme, or splits a CRLF pair.
    InvalidRange {
        start: usize,
        end: usize,
        source_len: usize,
    },
    /// The program could not be started.
    Spawn(io::Error),
    /// A stream, worker, coordinator, or delivery failure; the child was reaped.
    Process { message: String, stderr: Vec<u8> },
    /// The formatter exited unsuccessfully.
    Exit { status: ExitStatus, stderr: Vec<u8> },
    /// The formatter wrote standard output bytes that are not UTF-8.
    InvalidUtf8Output { stderr: Vec<u8> },
    /// The document no longer matches the source the request was built from.
    StaleSource,
}

impl TransformError {
    /// Raw captured stderr, when this failure carries any.
    pub fn stderr(&self) -> &[u8] {
        match self {
            Self::Process { stderr, .. }
            | Self::Exit { stderr, .. }
            | Self::InvalidUtf8Output { stderr } => stderr,
            _ => &[],
        }
    }

    /// Sanitized stderr for a status line or CLI diagnostic, when present.
    pub fn stderr_summary(&self) -> Option<String> {
        let stderr = self.stderr();
        if stderr.is_empty() {
            return None;
        }
        Some(diagnostic_summary(stderr))
    }

    fn with_stderr(mut self, stderr: Vec<u8>) -> Self {
        let target = match &mut self {
            Self::Process {
                stderr: existing, ..
            }
            | Self::Exit {
                stderr: existing, ..
            }
            | Self::InvalidUtf8Output { stderr: existing } => Some(existing),
            _ => None,
        };
        if let Some(existing) = target
            && existing.is_empty()
        {
            *existing = stderr;
        }
        self
    }
}

impl fmt::Display for TransformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRange {
                start,
                end,
                source_len,
            } => write!(
                formatter,
                "invalid range {start}..{end} for {source_len} bytes of editable text"
            ),
            Self::Spawn(error) => write!(formatter, "could not start formatter: {error}"),
            Self::Process { message, .. } => write!(formatter, "{message}"),
            Self::Exit { status, .. } => write!(formatter, "formatter exited with {status}"),
            Self::InvalidUtf8Output { .. } => {
                write!(formatter, "formatter standard output was not valid UTF-8")
            }
            Self::StaleSource => write!(
                formatter,
                "the document changed after the transformation request was created"
            ),
        }
    }
}

impl std::error::Error for TransformError {}

/// Validates `range` against BOM-free editable `text`.
///
/// Endpoints must be ordered, in bounds, at extended grapheme boundaries, and
/// never between the `\r` and `\n` of one CRLF terminator. The document end is a
/// valid endpoint and zero-length ranges are valid. This is the one validator the
/// library, the CLI, and the editor share, so a Visual selection converted to byte
/// offsets obeys exactly the same rules as `--range START:END`.
pub fn validate_range(text: &str, range: ByteRange) -> Result<(), TransformError> {
    let source_len = text.len();
    let invalid = || TransformError::InvalidRange {
        start: range.start,
        end: range.end,
        source_len,
    };
    if range.start > range.end || range.end > source_len {
        return Err(invalid());
    }
    for index in [range.start, range.end] {
        if !is_grapheme_boundary(text, index) || splits_crlf(text, index) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn is_grapheme_boundary(text: &str, index: usize) -> bool {
    if index == 0 || index == text.len() {
        return index <= text.len();
    }
    if !text.is_char_boundary(index) {
        return false;
    }
    text.grapheme_indices(true)
        .take_while(|&(start, _)| start <= index)
        .last()
        .is_some_and(|(start, _)| start == index)
}

fn splits_crlf(text: &str, index: usize) -> bool {
    let bytes = text.as_bytes();
    index > 0 && index < bytes.len() && bytes[index - 1] == b'\r' && bytes[index] == b'\n'
}

/// Splices `replacement` into `current` at `source`'s range, purely.
///
/// The whole current editable text and its explicit BOM flag must match the
/// request source, so a stale result cannot apply even when the selected fragment
/// still coincides. Bytes outside the range are preserved verbatim and the
/// replacement is inserted without normalizing endings, trimming, or adding a
/// final newline; empty output is a valid deletion. The returned document keeps the
/// source's BOM flag, and any leading U+FEFF in the replacement stays editable text.
pub fn apply_replacement(
    current: &Document,
    source: &TransformSource,
    replacement: &str,
) -> Result<Document, TransformError> {
    let current_text = current.editable_text();
    if current_text != source.text || current.has_bom() != source.has_bom {
        return Err(TransformError::StaleSource);
    }
    let range = source.range;
    validate_range(&current_text, range)?;
    let mut text = String::with_capacity(current_text.len() - range.len() + replacement.len());
    text.push_str(&current_text[..range.start]);
    text.push_str(replacement);
    text.push_str(&current_text[range.end..]);
    Ok(Document::from_editable_text(&text, source.has_bom))
}

/// Escapes and caps formatter diagnostics for a status line or CLI message.
///
/// The bytes are decoded lossily, terminal control characters are escaped, and at
/// most [`DIAGNOSTIC_SUMMARY_LIMIT`] Unicode scalar values are kept before
/// [`DIAGNOSTIC_TRUNCATION_MARKER`] is appended. Escaping is applied before the cap
/// so no escape sequence is split, and the result is always valid UTF-8.
pub fn diagnostic_summary(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut summary = String::new();
    let mut scalars = 0;
    let mut truncated = false;
    for character in text.chars() {
        if character.is_control() {
            let mut escape = String::new();
            escape.extend(character.escape_debug());
            let width = escape.chars().count();
            if scalars + width > DIAGNOSTIC_SUMMARY_LIMIT {
                truncated = true;
                break;
            }
            scalars += width;
            summary.push_str(&escape);
        } else {
            if scalars + 1 > DIAGNOSTIC_SUMMARY_LIMIT {
                truncated = true;
                break;
            }
            scalars += 1;
            summary.push(character);
        }
    }
    if truncated {
        summary.push_str(DIAGNOSTIC_TRUNCATION_MARKER);
    }
    summary
}

/// Runs `request`'s formatter synchronously and returns its result.
///
/// The direct child is spawned with all three standard streams piped, the selected
/// text is written to its standard input, and three workers drain input, output,
/// and diagnostics concurrently so large streams in either direction cannot
/// deadlock. A success requires a successful status, complete delivery of the
/// selection, and strict UTF-8 standard output. A child that stops reading input
/// early is only reported after its own nonzero status and diagnostics have had
/// precedence. Every failure path kills and reaps a still-running direct child and
/// joins every started worker before returning; a worker panic or a worker that
/// cannot start is reported as an operation error.
pub fn run_formatter(request: &TransformRequest) -> Result<TransformResult, TransformError> {
    run_inner(request, None).0
}

/// How the selection transfer ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delivery {
    /// The whole selection was written and the pipe was closed.
    Complete,
    /// The formatter closed its standard input before reading everything.
    BrokenPipe,
}

/// Which internal failure to inject; production runs always pass `None`.
#[derive(Clone, Copy, Debug, Default)]
struct Faults {
    stdin_write_error: bool,
    stdout_read_error: bool,
    stderr_read_error: bool,
    panic_stdout_worker: bool,
    /// A worker start that fails the way thread resource exhaustion would.
    fail_worker_start: Option<WorkerSlot>,
}

/// Which worker a [`Faults`] start failure applies to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerSlot {
    Input,
    Output,
    Error,
}

/// What the coordinator observably did, for cleanup assertions in tests.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
struct RunTrace {
    joined_workers: usize,
    kill_sent: bool,
    reaped: bool,
}

enum MonitorOutcome {
    Exited(ExitStatus),
    WorkerFailed,
    WaitFailed(io::Error),
}

fn run_inner(
    request: &TransformRequest,
    faults: Option<Faults>,
) -> (Result<TransformResult, TransformError>, RunTrace) {
    let faults = faults.unwrap_or_default();

    if let Err(error) = request.source.validate() {
        return (Err(error), RunTrace::default());
    }

    let mut child = match request.formatter.command().spawn() {
        Ok(child) => child,
        Err(error) => return (Err(TransformError::Spawn(error)), RunTrace::default()),
    };
    // The piped handles are taken before any worker exists; a handle a worker never
    // receives is dropped at that point, closing that pipe against the child.
    let streams = Streams {
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
    };
    let mut coordinator = Coordinator::new(child);

    let (worker_failure, failures) = mpsc::channel();
    if let Err(error) = start_workers(
        &mut coordinator,
        streams,
        request.source.selected(),
        faults,
        worker_failure,
    ) {
        // Partial startup: the started workers are joined and the direct child is
        // killed and reaped before the error leaves this function.
        coordinator.shutdown();
        return (
            Err(TransformError::Process {
                message: format!("could not start a formatter worker: {error}"),
                stderr: Vec::new(),
            }),
            coordinator.trace(),
        );
    }

    let outcome = coordinator.monitor(&failures);
    if matches!(outcome, MonitorOutcome::Exited(_)) {
        // A successful `try_wait` has already reaped the direct child.
        coordinator.mark_reaped();
    } else {
        // Kill/reap first: joins must never wait on a live child's pipes.
        coordinator.reap();
    }
    let joined = coordinator.collect();
    let trace = coordinator.trace();

    let mut failure = None;
    let output_bytes = match joined.output {
        Some(Ok(bytes)) => bytes,
        Some(Err(error)) => {
            failure.get_or_insert(error);
            Vec::new()
        }
        None => Vec::new(),
    };
    let stderr_bytes = match joined.error {
        Some(Ok(bytes)) => bytes,
        Some(Err(error)) => {
            failure.get_or_insert(error);
            Vec::new()
        }
        None => Vec::new(),
    };
    let mut delivery = None;
    if let Some(result) = joined.input {
        match result {
            Ok(value) => delivery = Some(value),
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }

    if let Some(error) = failure {
        return (Err(error.with_stderr(stderr_bytes)), trace);
    }

    let MonitorOutcome::Exited(status) = outcome else {
        let message = match outcome {
            MonitorOutcome::WaitFailed(error) => {
                format!("waiting for the formatter failed: {error}")
            }
            _ => "the formatter run ended without a process status".to_owned(),
        };
        return (
            Err(TransformError::Process {
                message,
                stderr: stderr_bytes,
            }),
            trace,
        );
    };

    if !status.success() {
        return (
            Err(TransformError::Exit {
                status,
                stderr: stderr_bytes,
            }),
            trace,
        );
    }
    if delivery == Some(Delivery::BrokenPipe) {
        return (
            Err(TransformError::Process {
                message: "the formatter closed its standard input before receiving the whole \
                          selection"
                    .to_owned(),
                stderr: stderr_bytes,
            }),
            trace,
        );
    }
    let replacement = match String::from_utf8(output_bytes) {
        Ok(text) => text,
        Err(_) => {
            return (
                Err(TransformError::InvalidUtf8Output {
                    stderr: stderr_bytes,
                }),
                trace,
            );
        }
    };
    (
        Ok(TransformResult {
            replacement,
            stderr: stderr_bytes,
            status,
            source: request.source.clone(),
        }),
        trace,
    )
}

/// The piped child handles, held until a worker takes each one.
struct Streams {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

/// Starts the three I/O workers one at a time on `coordinator`.
///
/// A failure leaves the handles it never transferred closed and every worker it
/// already started still owned by the coordinator, so the caller's cleanup can join
/// them. Readers start first, which is why a failing third start still has workers
/// to join.
fn start_workers(
    coordinator: &mut Coordinator,
    streams: Streams,
    payload: &str,
    faults: Faults,
    worker_failure: Sender<()>,
) -> io::Result<()> {
    let Streams {
        stdin,
        stdout,
        stderr,
    } = streams;
    coordinator.start_output(stdout, faults, worker_failure.clone())?;
    coordinator.start_error(stderr, faults, worker_failure.clone())?;
    coordinator.start_input(stdin, payload, faults, worker_failure)
}

/// Owns the direct child and every worker that was started for it.
///
/// Each worker handle is recorded by [`spawn_worker`] the instant its start
/// succeeds, and [`Coordinator::shutdown`] kills and reaps a still-running child
/// before joining all remaining workers. `Drop` runs that same cleanup, so neither
/// a failed worker start nor unwinding out of this module can detach a thread or
/// leak the direct child.
struct Coordinator {
    child: Option<Child>,
    workers: Workers,
    trace: RunTrace,
}

impl Coordinator {
    fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            workers: Workers::default(),
            trace: RunTrace::default(),
        }
    }

    fn trace(&self) -> RunTrace {
        self.trace
    }

    fn start_output(
        &mut self,
        stream: Option<ChildStdout>,
        faults: Faults,
        worker_failure: Sender<()>,
    ) -> io::Result<()> {
        spawn_worker(
            &mut self.workers.output,
            worker_failure,
            faults.fail_worker_start == Some(WorkerSlot::Output),
            move || {
                if faults.stdout_read_error {
                    return Err(TransformError::Process {
                        message: "injected failure while reading standard output".to_owned(),
                        stderr: Vec::new(),
                    });
                }
                if faults.panic_stdout_worker {
                    panic!("injected formatter worker panic");
                }
                read_stream(stream, "standard output")
            },
        )
    }

    fn start_error(
        &mut self,
        stream: Option<ChildStderr>,
        faults: Faults,
        worker_failure: Sender<()>,
    ) -> io::Result<()> {
        spawn_worker(
            &mut self.workers.error,
            worker_failure,
            faults.fail_worker_start == Some(WorkerSlot::Error),
            move || {
                if faults.stderr_read_error {
                    return Err(TransformError::Process {
                        message: "injected failure while reading standard error".to_owned(),
                        stderr: Vec::new(),
                    });
                }
                read_stream(stream, "standard error")
            },
        )
    }

    fn start_input(
        &mut self,
        stream: Option<ChildStdin>,
        payload: &str,
        faults: Faults,
        worker_failure: Sender<()>,
    ) -> io::Result<()> {
        let payload = payload.to_owned();
        spawn_worker(
            &mut self.workers.input,
            worker_failure,
            faults.fail_worker_start == Some(WorkerSlot::Input),
            move || write_payload(stream, &payload, faults.stdin_write_error),
        )
    }

    /// Waits for the direct child's status, abandoning the wait when a worker
    /// reports a failure so a stuck child can be killed instead of awaited.
    fn monitor(&mut self, worker_failure: &Receiver<()>) -> MonitorOutcome {
        loop {
            if worker_failure.try_recv().is_ok() {
                return MonitorOutcome::WorkerFailed;
            }
            let Some(child) = self.child.as_mut() else {
                return MonitorOutcome::WaitFailed(io::Error::other(
                    "the formatter child handle is gone",
                ));
            };
            match child.try_wait() {
                Ok(Some(status)) => return MonitorOutcome::Exited(status),
                Ok(None) => thread::sleep(CHILD_POLL_INTERVAL),
                Err(error) => return MonitorOutcome::WaitFailed(error),
            }
        }
    }

    /// Records that `try_wait` already reaped the finished direct child.
    fn mark_reaped(&mut self) {
        self.child = None;
        self.trace.reaped = true;
    }

    /// Kills a still-running direct child and reaps it.
    fn reap(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(None)) {
            self.trace.kill_sent = true;
            let _ = child.kill();
        }
        let _ = child.wait();
        self.trace.reaped = true;
    }

    /// Joins every started worker and returns their results.
    fn collect(&mut self) -> Joined {
        std::mem::take(&mut self.workers).join(&mut self.trace)
    }

    /// Explicit cleanup: reap the child, then join every remaining worker.
    fn shutdown(&mut self) {
        self.reap();
        let _ = std::mem::take(&mut self.workers).join(&mut self.trace);
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        // Unwinding backstop: the direct child is killed and reaped and every
        // started worker is joined, so no path can detach a worker or leak a child.
        self.shutdown();
    }
}

#[derive(Default)]
struct Workers {
    input: Option<JoinHandle<Result<Delivery, TransformError>>>,
    output: Option<JoinHandle<Result<Vec<u8>, TransformError>>>,
    error: Option<JoinHandle<Result<Vec<u8>, TransformError>>>,
}

struct Joined {
    input: Option<Result<Delivery, TransformError>>,
    output: Option<Result<Vec<u8>, TransformError>>,
    error: Option<Result<Vec<u8>, TransformError>>,
}

impl Workers {
    fn join(mut self, trace: &mut RunTrace) -> Joined {
        let input = join_worker(self.input.take(), trace);
        let output = join_worker(self.output.take(), trace);
        let error = join_worker(self.error.take(), trace);
        Joined {
            input,
            output,
            error,
        }
    }
}

fn join_worker<T>(
    worker: Option<JoinHandle<Result<T, TransformError>>>,
    trace: &mut RunTrace,
) -> Option<Result<T, TransformError>> {
    let worker = worker?;
    trace.joined_workers += 1;
    Some(worker.join().unwrap_or_else(|_| {
        Err(TransformError::Process {
            message: "a formatter worker thread panicked".to_owned(),
            stderr: Vec::new(),
        })
    }))
}

/// Starts one worker into `slot`, which the coordinator already owns.
///
/// `thread::Builder::spawn` reports resource exhaustion as an `io::Result` instead
/// of panicking, and the handle is stored in `slot` before this returns, so a
/// failed or abandoned start can never leave an untracked running thread.
fn spawn_worker<T, F>(
    slot: &mut Option<JoinHandle<Result<T, TransformError>>>,
    worker_failure: Sender<()>,
    inject_failure: bool,
    work: F,
) -> io::Result<()>
where
    F: FnOnce() -> Result<T, TransformError> + Send + 'static,
    T: Send + 'static,
{
    if inject_failure {
        return Err(io::Error::other("injected formatter worker start failure"));
    }
    let handle = thread::Builder::new()
        .name("postvim-formatter-worker".to_owned())
        .spawn(move || worker_body(worker_failure, work))?;
    *slot = Some(handle);
    Ok(())
}

/// Runs one worker, notifying the coordinator before any error escapes so a stuck
/// child is killed instead of awaited forever. A panic becomes an operation error.
fn worker_body<T>(
    worker_failure: Sender<()>,
    work: impl FnOnce() -> Result<T, TransformError>,
) -> Result<T, TransformError> {
    match std::panic::catch_unwind(AssertUnwindSafe(work)) {
        Ok(result) => {
            if result.is_err() {
                let _ = worker_failure.send(());
            }
            result
        }
        Err(_) => {
            let _ = worker_failure.send(());
            Err(TransformError::Process {
                message: "a formatter worker thread panicked".to_owned(),
                stderr: Vec::new(),
            })
        }
    }
}

fn read_stream(reader: Option<impl Read>, stream: &str) -> Result<Vec<u8>, TransformError> {
    let mut bytes = Vec::new();
    if let Some(mut reader) = reader {
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| TransformError::Process {
                message: format!("reading formatter {stream} failed: {error}"),
                stderr: Vec::new(),
            })?;
    }
    Ok(bytes)
}

fn write_payload(
    stdin: Option<ChildStdin>,
    payload: &str,
    inject_error: bool,
) -> Result<Delivery, TransformError> {
    let Some(mut stdin) = stdin else {
        return Ok(Delivery::Complete);
    };
    if inject_error {
        // Closing the pipe is what a real writer failure would leave behind.
        drop(stdin);
        return Err(TransformError::Process {
            message: "injected failure while writing standard input".to_owned(),
            stderr: Vec::new(),
        });
    }
    match stdin.write_all(payload.as_bytes()) {
        Ok(()) => {
            let _ = stdin.flush();
            Ok(Delivery::Complete)
        }
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(Delivery::BrokenPipe),
        Err(error) => Err(TransformError::Process {
            message: format!("writing formatter standard input failed: {error}"),
            stderr: Vec::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Instant;

    fn spec(program: &str, arguments: &[&str]) -> FormatterSpec {
        FormatterSpec::new(program).args(arguments.to_vec())
    }

    fn request(source: TransformSource, formatter: FormatterSpec) -> TransformRequest {
        TransformRequest::new(source, formatter)
    }

    fn whole_request(text: &str, program: &str, arguments: &[&str]) -> TransformRequest {
        request(
            TransformSource::whole(text, false),
            spec(program, arguments),
        )
    }

    #[test]
    fn formatter_spec_builds_a_direct_command_with_literal_arguments() {
        let spec = FormatterSpec::new("rustfmt")
            .arg("--emit")
            .arg("stdout")
            .arg("|")
            .arg("$HOME")
            .arg(">")
            .arg("naïve file.rs")
            .working_directory("/tmp");
        assert_eq!(spec.program(), OsStr::new("rustfmt"));
        assert_eq!(spec.arguments().len(), 6);
        assert_eq!(spec.cwd(), Some(&PathBuf::from("/tmp")));

        // The program stays the program: Postvim never wraps it in a shell, so
        // shell metacharacters are literal argv entries.
        let command = spec.command();
        assert_eq!(command.get_program(), OsStr::new("rustfmt"));
        assert_eq!(
            command.get_args().collect::<Vec<&OsStr>>(),
            spec.arguments()
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp")));
    }

    #[test]
    fn range_validator_accepts_grapheme_and_crlf_boundaries() {
        let text = "a👩‍👩‍👧‍👦b\r\ncd\n";
        assert_eq!(text.len(), 32);
        for range in [
            ByteRange::new(0, 0),
            ByteRange::new(0, 1),
            ByteRange::new(1, 26),
            ByteRange::new(26, 27),
            ByteRange::new(27, 29),
            ByteRange::new(29, 31),
            ByteRange::new(31, 32),
            ByteRange::new(32, 32),
            ByteRange::whole(text),
        ] {
            assert!(
                validate_range(text, range).is_ok(),
                "{range:?} should be valid"
            );
        }
    }

    #[test]
    fn range_validator_rejects_split_graphemes_crlf_and_bad_bounds() {
        let text = "a👩‍👩‍👧‍👦b\r\ncd\n";
        for range in [
            ByteRange::new(1, 0),   // reversed
            ByteRange::new(0, 33),  // out of bounds
            ByteRange::new(33, 33), // out of bounds
            ByteRange::new(0, 25),  // inside the emoji grapheme
            ByteRange::new(2, 26),  // starts inside the emoji grapheme
            ByteRange::new(27, 28), // ends between \r and \n
            ByteRange::new(28, 29), // starts between \r and \n
        ] {
            assert!(
                matches!(
                    validate_range(text, range),
                    Err(TransformError::InvalidRange { .. })
                ),
                "{range:?} should be invalid"
            );
        }
        // An empty file accepts only the empty range.
        assert!(validate_range("", ByteRange::new(0, 0)).is_ok());
        assert!(validate_range("", ByteRange::new(0, 1)).is_err());
    }

    #[test]
    fn apply_preserves_outside_bytes_and_the_explicit_bom() {
        let text = "one\r\ntwo\nthree";
        let source = TransformSource::new(text, true, ByteRange::new(5, 8));
        assert_eq!(source.selected(), "two");
        let document = Document::from_editable_text(text, true);
        let updated = apply_replacement(&document, &source, "TWO").unwrap();
        assert!(updated.has_bom());
        assert_eq!(updated.editable_text(), "one\r\nTWO\nthree");
        assert_eq!(updated.serialize(), "\u{feff}one\r\nTWO\nthree");

        // The untouched prefix/suffix and the missing final newline survive, and
        // the replacement's own ending bytes are inserted verbatim.
        let source = TransformSource::new(text, false, ByteRange::new(5, 8));
        let without_bom = Document::from_editable_text(text, false);
        let updated = apply_replacement(&without_bom, &source, "T\r\nW").unwrap();
        assert!(!updated.has_bom());
        assert_eq!(updated.editable_text(), "one\r\nT\r\nW\nthree");
        assert_eq!(updated.serialize(), "one\r\nT\r\nW\nthree");
    }

    #[test]
    fn apply_empty_replacement_deletes_the_selection() {
        let text = "one\ntwo\n";
        let source = TransformSource::new(text, false, ByteRange::new(4, 7));
        let document = Document::from_editable_text(text, false);
        let updated = apply_replacement(&document, &source, "").unwrap();
        assert_eq!(updated.editable_text(), "one\n\n");

        let whole = TransformSource::whole(text, false);
        let emptied = apply_replacement(&document, &whole, "").unwrap();
        assert_eq!(emptied.editable_text(), "");
        assert_eq!(emptied.line_count(), 1);
        assert_eq!(emptied.serialize(), "");
    }

    #[test]
    fn apply_rejects_stale_source_including_bom_only_changes() {
        let source = TransformSource::new("hello world", false, ByteRange::new(0, 5));
        let unchanged = Document::from_editable_text("hello world", false);
        assert!(apply_replacement(&unchanged, &source, "HI").is_ok());

        // The selected fragment still matches, but the unselected text does not.
        let outside = Document::from_editable_text("hello WORLD", false);
        assert!(matches!(
            apply_replacement(&outside, &source, "HI"),
            Err(TransformError::StaleSource)
        ));

        // Identical editable text, only the BOM flag differs.
        let bom = Document::from_editable_text("hello world", true);
        assert!(matches!(
            apply_replacement(&bom, &source, "HI"),
            Err(TransformError::StaleSource)
        ));

        // A leading U+FEFF in editable text is source text too.
        let feff = Document::from_editable_text("\u{feff}hello world", false);
        assert!(matches!(
            apply_replacement(&feff, &source, "HI"),
            Err(TransformError::StaleSource)
        ));
    }

    #[test]
    fn apply_keeps_a_formatter_emitted_leading_fe_ff_as_text() {
        // Original file without a BOM: the emitted U+FEFF is editable text at 0.
        let plain = Document::from_editable_text("body", false);
        let source = TransformSource::whole("body", false);
        let updated = apply_replacement(&plain, &source, "\u{feff}body").unwrap();
        assert!(!updated.has_bom());
        assert_eq!(updated.editable_text(), "\u{feff}body");
        assert_eq!(updated.serialize(), "\u{feff}body");
        // Offsets stay BOM-free: 3 is the start of "body", not past a BOM.
        assert_eq!(
            updated.cursor_at_byte_offset(3),
            crate::document::Cursor::new(0, 3)
        );
        assert_eq!(updated.byte_offset(updated.cursor_at_byte_offset(3)), 3);

        // Original file with a BOM: the emitted U+FEFF is a second, editable one.
        let with_bom = Document::from_editable_text("body", true);
        let source = TransformSource::whole("body", true);
        let updated = apply_replacement(&with_bom, &source, "\u{feff}body").unwrap();
        assert!(updated.has_bom());
        assert_eq!(updated.editable_text(), "\u{feff}body");
        assert_eq!(updated.serialize(), "\u{feff}\u{feff}body");
        assert_eq!(
            updated.cursor_at_byte_offset(3),
            crate::document::Cursor::new(0, 3)
        );
    }

    #[test]
    fn result_apply_and_changed_track_the_selection() {
        let source = TransformSource::new("keep\ntarget\nkeep", false, ByteRange::new(5, 11));
        let document = Document::from_editable_text("keep\ntarget\nkeep", false);
        let identical = synthetic_result(source.clone(), "target");
        assert!(!identical.changed());
        assert_eq!(identical.apply(&document).unwrap(), document);

        let changed = synthetic_result(source, "TARGET");
        assert!(changed.changed());
        assert_eq!(
            changed.apply(&document).unwrap().editable_text(),
            "keep\nTARGET\nkeep"
        );
    }

    #[test]
    fn diagnostic_summary_escapes_controls_and_decodes_lossily() {
        assert_eq!(
            diagnostic_summary(b"a\nb\tc\rd"),
            "a\\nb\\tc\\rd".to_owned()
        );
        let escaped = diagnostic_summary(b"error\x1b[31m red");
        assert!(!escaped.chars().any(char::is_control));
        assert!(escaped.contains("\\u{1b}"), "{escaped}");
        assert_eq!(diagnostic_summary(&[0x61, 0xff, 0x62]), "a\u{fffd}b");
        assert_eq!(diagnostic_summary(b""), "");
    }

    #[test]
    fn diagnostic_summary_truncates_without_splitting_utf8() {
        // Multi-byte text: exactly the limit fits.
        let exact = "é".repeat(DIAGNOSTIC_SUMMARY_LIMIT).into_bytes();
        let summary = diagnostic_summary(&exact);
        assert_eq!(summary.chars().count(), DIAGNOSTIC_SUMMARY_LIMIT);
        assert!(!summary.contains(DIAGNOSTIC_TRUNCATION_MARKER));

        let longer = "é".repeat(DIAGNOSTIC_SUMMARY_LIMIT + 1).into_bytes();
        let summary = diagnostic_summary(&longer);
        assert!(summary.ends_with(DIAGNOSTIC_TRUNCATION_MARKER));
        let body = summary
            .strip_suffix(DIAGNOSTIC_TRUNCATION_MARKER)
            .expect("marker present");
        assert_eq!(body.chars().count(), DIAGNOSTIC_SUMMARY_LIMIT);

        // Escaping happens before the cap, so no partial escape sequence is kept.
        let controls = vec![b'\x1b'; 4000];
        let summary = diagnostic_summary(&controls);
        assert!(summary.ends_with(DIAGNOSTIC_TRUNCATION_MARKER));
        let body = summary
            .strip_suffix(DIAGNOSTIC_TRUNCATION_MARKER)
            .expect("marker present");
        let scalars = body.chars().count();
        assert!(scalars <= DIAGNOSTIC_SUMMARY_LIMIT, "{scalars}");
        assert!(scalars + 6 > DIAGNOSTIC_SUMMARY_LIMIT, "{scalars}");
        assert!(!body.chars().any(char::is_control));
        assert!(body.ends_with("\\u{1b}"), "{body}");
    }

    #[test]
    fn invalid_range_is_rejected_before_starting_a_process() {
        let source = TransformSource::new("ab", false, ByteRange::new(1, 0));
        let request = request(source, FormatterSpec::new("postvim-missing-formatter"));
        // A spawn would report `Spawn`; the range check must come first.
        assert!(matches!(
            run_formatter(&request),
            Err(TransformError::InvalidRange { .. })
        ));
    }

    /// A result built without a process, for pure application tests.
    fn synthetic_result(source: TransformSource, replacement: &str) -> TransformResult {
        TransformResult {
            replacement: replacement.to_owned(),
            stderr: Vec::new(),
            status: Command::new("true")
                .status()
                .expect("`true` should be available to build a status"),
            source,
        }
    }

    #[cfg(unix)]
    #[test]
    fn identity_formatter_returns_the_selection_verbatim() {
        let text = "hello\tworld\r\nno final newline";
        let result = run_formatter(&whole_request(text, "cat", &[])).expect("cat should succeed");
        assert_eq!(result.replacement(), text);
        assert!(result.succeeded());
        assert!(result.stderr().is_empty());
        assert!(!result.changed());
        assert!(result.warnings().is_none());

        // Only the selected slice reaches the executable.
        let source = TransformSource::new(text, false, ByteRange::new(6, 11));
        let result = run_formatter(&request(source, spec("cat", &[]))).unwrap();
        assert_eq!(result.replacement(), "world");
    }

    #[cfg(unix)]
    #[test]
    fn successful_stderr_is_reported_as_warnings_not_text() {
        let text = "value\n";
        let result = run_formatter(&whole_request(
            text,
            "sh",
            &["-c", "cat; printf 'warning: check this\\n' >&2"],
        ))
        .expect("sh should succeed");
        assert_eq!(result.replacement(), text);
        assert_eq!(result.warnings().as_deref(), Some("warning: check this\\n"));
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_reports_raw_stderr_and_status() {
        let result = run_formatter(&whole_request(
            "x",
            "sh",
            &["-c", "printf 'fatal: broken\\n' >&2; exit 7"],
        ));
        match result {
            Err(TransformError::Exit { status, stderr }) => {
                assert_eq!(status.code(), Some(7));
                assert_eq!(stderr, b"fatal: broken\n");
            }
            other => panic!("unexpected result: {other:?}"),
        }
        assert_eq!(
            TransformError::Exit {
                status: Command::new("true").status().unwrap(),
                stderr: b"fatal: broken\n".to_vec(),
            }
            .stderr_summary()
            .as_deref(),
            Some("fatal: broken\\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn invalid_utf8_output_is_rejected_with_diagnostics_preserved() {
        let result = run_formatter(&whole_request(
            "x",
            "sh",
            &["-c", "cat >/dev/null; printf '\\303'; printf 'hint\\n' >&2"],
        ));
        match result {
            Err(TransformError::InvalidUtf8Output { stderr }) => {
                assert_eq!(stderr, b"hint\n");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_program_is_a_spawn_error() {
        let request = whole_request("x", "postvim-no-such-formatter-binary", &[]);
        assert!(matches!(
            run_formatter(&request),
            Err(TransformError::Spawn(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn empty_output_is_a_valid_deletion() {
        let text = "one\ntwo\n";
        let source = TransformSource::new(text, false, ByteRange::new(4, 7));
        let result = run_formatter(&request(source, spec("sh", &["-c", "cat >/dev/null"])))
            .expect("sh should succeed");
        assert_eq!(result.replacement(), "");
        let document = Document::from_editable_text(text, false);
        assert_eq!(result.apply(&document).unwrap().editable_text(), "one\n\n");
    }

    #[cfg(unix)]
    #[test]
    fn large_bidirectional_io_does_not_deadlock() {
        let text = "0123456789abcdef".repeat(64 * 1024);
        let result = run_formatter(&whole_request(&text, "cat", &[])).expect("cat should succeed");
        assert_eq!(result.replacement().len(), text.len());
        assert_eq!(result.replacement(), text);
    }

    #[cfg(unix)]
    #[test]
    fn broken_pipe_yields_to_a_nonzero_diagnostic() {
        // The child never reads its input, so the writer sees EPIPE while the
        // coordinator keeps draining and collecting the status and diagnostics.
        let text = "x".repeat(256 * 1024);
        let result = run_formatter(&whole_request(
            &text,
            "sh",
            &["-c", "printf 'fatal: no stdin\\n' >&2; exit 3"],
        ));
        match result {
            Err(TransformError::Exit { status, stderr }) => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stderr, b"fatal: no stdin\n");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_delivery_after_success_is_an_error() {
        let text = "x".repeat(256 * 1024);
        let result = run_formatter(&whole_request(&text, "sh", &["-c", "exit 0"]));
        match result {
            Err(TransformError::Process { message, .. }) => {
                assert!(message.contains("standard input"), "{message}");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[cfg(unix)]
    fn sleeping_request() -> TransformRequest {
        // A direct `sleep`, not `sh -c`, so no descendant can inherit the pipes and
        // stall the coordinator's joins.
        whole_request("x", "sleep", &["30"])
    }

    #[cfg(unix)]
    fn assert_cleaned_up(result: Result<TransformResult, TransformError>, trace: RunTrace) {
        assert!(result.is_err(), "expected a cleanup failure");
        assert!(trace.kill_sent, "the direct child should have been killed");
        assert!(trace.reaped, "the direct child should have been reaped");
    }

    #[cfg(unix)]
    #[test]
    fn reader_failure_kills_and_reaps_the_direct_child() {
        let started = Instant::now();
        let (result, trace) = run_inner(
            &sleeping_request(),
            Some(Faults {
                stdout_read_error: true,
                ..Faults::default()
            }),
        );
        assert_cleaned_up(result, trace);
        assert_eq!(trace.joined_workers, 3);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cleanup took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn stderr_reader_failure_kills_and_reaps_the_direct_child() {
        let (result, trace) = run_inner(
            &sleeping_request(),
            Some(Faults {
                stderr_read_error: true,
                ..Faults::default()
            }),
        );
        assert_cleaned_up(result, trace);
        assert_eq!(trace.joined_workers, 3);
    }

    #[cfg(unix)]
    #[test]
    fn writer_failure_kills_and_reaps_the_direct_child() {
        let (result, trace) = run_inner(
            &sleeping_request(),
            Some(Faults {
                stdin_write_error: true,
                ..Faults::default()
            }),
        );
        assert_cleaned_up(result, trace);
        assert_eq!(trace.joined_workers, 3);
    }

    #[cfg(unix)]
    #[test]
    fn worker_start_failure_cleans_up_started_workers() {
        // Readers start before the input worker, so failing the third start must
        // still join the two that are already running; failing the first must leave
        // nothing to join but still reap the child.
        for (slot, started_workers) in [
            (WorkerSlot::Input, 2),
            (WorkerSlot::Error, 1),
            (WorkerSlot::Output, 0),
        ] {
            let began = Instant::now();
            let (result, trace) = run_inner(
                &sleeping_request(),
                Some(Faults {
                    fail_worker_start: Some(slot),
                    ..Faults::default()
                }),
            );
            match &result {
                Err(TransformError::Process { message, .. }) => {
                    assert!(message.contains("worker"), "{slot:?}: {message}");
                }
                other => panic!("{slot:?}: unexpected result: {other:?}"),
            }
            assert_cleaned_up(result, trace);
            assert_eq!(trace.joined_workers, started_workers, "{slot:?}");
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "{slot:?}: cleanup took {:?}",
                began.elapsed()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn coordinator_drop_joins_workers_and_reaps_the_child() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut command = Command::new("sleep");
        command
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("sleep should start");
        let stdout = child.stdout.take();
        let finished = Arc::new(AtomicUsize::new(0));
        let mut coordinator = Coordinator::new(child);
        let (worker_failure, _failures) = mpsc::channel();
        let marker = Arc::clone(&finished);
        spawn_worker(
            &mut coordinator.workers.output,
            worker_failure,
            false,
            move || {
                let result = read_stream(stdout, "standard output");
                marker.fetch_add(1, Ordering::SeqCst);
                result
            },
        )
        .expect("worker should start");

        let began = Instant::now();
        drop(coordinator);

        // The guard joins, so the worker's completion is ordered before this point:
        // dropping must never detach a running worker.
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "drop took {:?}",
            began.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn worker_panic_is_an_operation_error_and_still_reaps() {
        let (result, trace) = run_inner(
            &sleeping_request(),
            Some(Faults {
                panic_stdout_worker: true,
                ..Faults::default()
            }),
        );
        match result {
            Err(TransformError::Process { message, .. }) => {
                assert!(message.contains("panicked"), "{message}");
            }
            other => panic!("unexpected result: {other:?}"),
        }
        assert!(trace.kill_sent);
        assert!(trace.reaped);
        assert_eq!(trace.joined_workers, 3);
    }
}
