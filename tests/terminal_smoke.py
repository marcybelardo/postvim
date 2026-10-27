#!/usr/bin/env python3
"""Unix PTY and subprocess smoke tests for Postvim.

Run from the repository root with the built editor path, for example:
    python3 tests/terminal_smoke.py target/debug/postvim
    python3 tests/terminal_smoke.py            # discover it through Cargo

The editor, the terminal-lifecycle fixture, and the formatter fixture are all
located from Cargo's JSON messages, so any configured target directory, profile,
or target triple works without hardcoded artifact paths. This harness
deliberately uses only Python's standard library and a real PTY.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest
from pathlib import Path
from typing import Callable, Sequence


ROOT = Path(__file__).resolve().parent.parent
FIXTURE_TARGET = "terminal_lifecycle_fixture"
FORMATTER_FIXTURE_TARGET = "formatter_fixture"
SHIPPING_TARGET = "postvim"
FIXTURE_READY = "TERMINAL_FIXTURE_READY"
DEFAULT_TIMEOUT = 8.0
# Formatter runs, large streams, and startup of a freshly built binary are
# slower than a keystroke round trip, but a hung child must not stall the suite.
DEFAULT_DEADLINE = 120.0
REDRAW_TIMEOUT = 15.0
# Escape sequences that only a program which took over the terminal emits.
TERMINAL_TAKEOVER = (
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?2004h",
    b"\x1b[?2004l",
    b"\x1b[?25l",
)
# A redraw always starts with a cursor-position escape sequence, so waiting for
# one is how these tests tell old screen bytes from a fresh frame.
CURSOR_POSITION = re.compile(rb"\x1b\[[0-9;]*H")
# The editor writes the cursor style of the current mode before every frame, so
# these sequences identify the mode a frame was drawn in. Waiting for the style
# the *next* mode emits is stronger than waiting for any redraw: a redraw queued
# for an earlier keystroke cannot satisfy it.
NORMAL_CURSOR = b"\x1b[2 q"
INSERT_CURSOR = b"\x1b[6 q"
PROMPT_CURSOR = b"\x1b[4 q"
# SGR/CSI sequences only; this is not a screen model, just a way to read the
# text of a fresh output slice without the style bytes that split cells.
ANSI_SEQUENCE = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]")
# The dirty-quit refusal, as documented in AGENTS.md and docs/keymaps.md.
DOCUMENTED_DIRTY_MESSAGE = (
    "Unsaved changes; use :w [path] before :q, or :q! to discard"
)
# Older editors only said this much; asserting the shared prefix keeps this
# test about refusal behavior rather than about the trailing :q! advice.
DIRTY_REFUSAL_PREFIX = "Unsaved changes; use :w [path] before :q"

# The formatter fixture's modes, in the order its `fixture modes` listing emits
# them. Both sides name the same set: a renamed mode fails loudly here instead of
# silently weakening a test.
FIXTURE_MODES = [
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
FIXTURE_MODE_LINES = "".join(f"{mode}\n" for mode in FIXTURE_MODES).encode()
# What `fixture mutate-file PATH` writes, standing in for an outside editor.
EXTERNAL_EDIT = b"external edit\n"
FIXTURE_SCENARIOS = [
    "document_preserves_exact_bytes",
    "editor_format_whole_and_visual_parity",
    "fixture_protocol_matrix",
    "guarded_save_commits_and_reports_observation",
    "guarded_save_refuses_external_mutation",
    "library_identity_rewrite_and_empty",
    "library_literal_arguments_and_working_directory",
    "library_range_bom_stale_and_validation",
    "library_warnings_and_diagnostics",
]


def displayed_text(data: bytes) -> str:
    """Strip styling escapes from one fresh output slice.

    Cursor cells and prompt characters are written with their own style
    sequences, so screen text is never contiguous in the raw stream. Removing
    only the escape sequences recovers what a reader would see in that slice
    without modelling the screen, cursor, or redraw order.
    """
    return ANSI_SEQUENCE.sub(b"", data).decode("utf-8", errors="replace")


def visible_text(data: bytes) -> str:
    """The non-space characters a reader would see in one fresh output slice.

    The editor moves the cursor between text runs and leaves blank cells
    unpainted, so words are not contiguous in the raw stream even after styling
    is removed. Dropping whitespace from both sides of a comparison keeps an
    assertion about content without building a screen model.
    """
    return "".join(displayed_text(data).split())


def unhex_bytes(text: str) -> bytes:
    return bytes.fromhex(text)


def quote_for_command(word: str) -> str:
    """Quote one word for an interactive `:format` command line.

    `:format` splits its argument with shell-word rules but never expands or
    interprets anything, so this only has to survive quoting.
    """
    if word and not any(character in word for character in " \t'\"\\|$`>&<*?[](){};#!~\n"):
        return word
    return "'" + word.replace("'", "'\\''") + "'"


def atomic_replace(path: Path, contents: bytes) -> None:
    """Replace `path` the way an agent or tool would: write beside, then rename."""
    temporary = path.with_name(path.name + ".external")
    temporary.write_bytes(contents)
    os.replace(temporary, path)


# ---------------------------------------------------------------------------
# Cargo artifact discovery
# ---------------------------------------------------------------------------


class CargoArtifactError(RuntimeError):
    pass


def cargo_executables(
    cargo_arguments: Sequence[str], *, timeout: float = 600.0
) -> dict[str, Path]:
    """Build with Cargo and map target names to the executables it reported.

    Cargo decides where artifacts live, including `CARGO_TARGET_DIR`, a
    configured `target-dir`, `--profile`, and `--target`. Reading its JSON
    messages therefore supports every layout without guessing any path.
    """
    command = ["cargo", *cargo_arguments, "--message-format=json"]
    completed = subprocess.run(
        command,
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if completed.returncode != 0:
        raise CargoArtifactError(
            f"failed to build {cargo_arguments} with Cargo:\n"
            + completed.stdout[-5000:]
            + completed.stderr[-5000:]
        )

    executables: dict[str, Path] = {}
    for line in completed.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        name = message.get("target", {}).get("name", "")
        executables[name] = Path(message["executable"]).resolve()
    return executables


def require_executable(executables: dict[str, Path], target: str, description: str) -> Path:
    executable = executables.get(target)
    if executable is None:
        raise CargoArtifactError(
            f"Cargo JSON output did not include an executable for {target} ({description})"
        )
    if not executable.is_file():
        raise CargoArtifactError(f"Cargo reported a missing {description}: {executable}")
    return executable


def discover_test_binary(target: str, description: str) -> Path:
    return require_executable(
        cargo_executables(["test", "--test", target, "--no-run"]), target, description
    )


def discover_shipping_binary() -> Path:
    executable = require_executable(
        cargo_executables(["build", "--bin", SHIPPING_TARGET]),
        SHIPPING_TARGET,
        "shipping editor",
    )
    if not os.access(executable, os.X_OK):
        raise CargoArtifactError(f"shipping editor is not executable: {executable}")
    return executable


# ---------------------------------------------------------------------------
# Deadline containment for subprocess work
# ---------------------------------------------------------------------------


class DeadlineExceeded(AssertionError):
    """A bounded subprocess did not finish, and its process group was killed."""

    def __init__(
        self,
        command: Sequence[str | os.PathLike[str]],
        timeout: float,
        pid: int,
        returncode: int | None,
        output: bytes = b"",
    ) -> None:
        self.command = [os.fspath(arg) for arg in command]
        self.timeout = timeout
        self.pid = pid
        self.returncode = returncode
        self.output = output
        super().__init__(
            f"command did not finish within {timeout}s and was killed: {self.command}; "
            f"pid {pid} returned {returncode}; output tail:\n"
            f"{output[-4000:]!r}"
        )


def contain_process_group(process: subprocess.Popen[bytes], grace: float = 2.0) -> int | None:
    """Terminate a whole process group, then reap the leader.

    The process was started in its own session, so signalling the group reaches
    children it spawned. This is test containment: it stops a hung scenario from
    stalling the suite and does not claim Postvim contains formatter
    descendants in production.
    """
    if process.poll() is None:
        try:
            os.killpg(os.getpgid(process.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass
        try:
            process.communicate(timeout=grace)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(os.getpgid(process.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            try:
                process.communicate(timeout=grace)
            except subprocess.TimeoutExpired:
                # Last resort: at least reap the direct child.
                try:
                    process.kill()
                except ProcessLookupError:
                    pass
                process.wait(timeout=grace)
    else:
        try:
            process.communicate(timeout=grace)
        except subprocess.TimeoutExpired:
            pass
    return process.returncode


def run_with_deadline(
    command: Sequence[str | os.PathLike[str]],
    *,
    stdin: bytes = b"",
    timeout: float = DEFAULT_DEADLINE,
    cwd: str | os.PathLike[str] | None = None,
) -> subprocess.CompletedProcess[bytes]:
    """Run a command with piped streams under a hard deadline.

    Every run gets its own session so a timeout can kill whatever it started.
    """
    arguments = [os.fspath(argument) for argument in command]
    process = subprocess.Popen(
        arguments,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
        cwd=None if cwd is None else os.fspath(cwd),
    )
    try:
        stdout, stderr = process.communicate(input=stdin, timeout=timeout)
    except subprocess.TimeoutExpired as expired:
        returncode = contain_process_group(process)
        raise DeadlineExceeded(
            arguments,
            timeout,
            process.pid,
            returncode,
            output=(expired.stdout or b"") + (expired.stderr or b""),
        ) from expired
    return subprocess.CompletedProcess(arguments, process.returncode, stdout, stderr)


class PtySession:
    """Run one process on a sized PTY while retaining its slave for inspection."""

    def __init__(
        self,
        command: list[str | os.PathLike[str]],
        *,
        rows: int = 24,
        columns: int = 80,
        timeout: float = DEFAULT_TIMEOUT,
        cwd: str | os.PathLike[str] | None = None,
    ) -> None:
        self.timeout = timeout
        self.master_fd, self.slave_fd = pty.openpty()
        self._set_size(rows, columns)
        self.initial_attrs = termios.tcgetattr(self.slave_fd)
        self.output = bytearray()
        self._master_closed = False
        try:
            self.process = subprocess.Popen(
                [os.fspath(arg) for arg in command],
                stdin=self.slave_fd,
                stdout=self.slave_fd,
                stderr=self.slave_fd,
                close_fds=True,
                start_new_session=True,
                cwd=None if cwd is None else os.fspath(cwd),
            )
        except BaseException:
            os.close(self.master_fd)
            os.close(self.slave_fd)
            raise

    def _set_size(self, rows: int, columns: int) -> None:
        winsize = struct.pack("HHHH", rows, columns, 0, 0)
        fcntl.ioctl(self.slave_fd, termios.TIOCSWINSZ, winsize)

    @property
    def text(self) -> str:
        return self.output.decode("utf-8", errors="replace")

    def _read_once(self, timeout: float) -> bool:
        if self._master_closed:
            return False
        readable, _, _ = select.select([self.master_fd], [], [], max(timeout, 0.0))
        if not readable:
            return False
        try:
            chunk = os.read(self.master_fd, 65536)
        except OSError:
            self._master_closed = True
            return False
        if not chunk:
            self._master_closed = True
            return False
        self.output.extend(chunk)
        return True

    def drain_available(self) -> None:
        while self._read_once(0):
            pass

    def wait_for(self, predicate: Callable[[bytes], bool], description: str, timeout: float | None = None) -> None:
        deadline = time.monotonic() + (self.timeout if timeout is None else timeout)
        while not predicate(bytes(self.output)):
            remaining = deadline - time.monotonic()
            if remaining <= 0 or (self.process.poll() is not None and not self._read_once(0)):
                self.fail(f"timed out waiting for {description}")
            self._read_once(min(remaining, 0.1))

    def wait_for_text(self, needle: str, timeout: float | None = None) -> None:
        encoded = needle.encode("utf-8")
        self.wait_for(lambda data: encoded in data, repr(needle), timeout)

    def wait_for_bytes_after(
        self, offset: int, needle: bytes, description: str, timeout: float | None = None
    ) -> None:
        self.wait_for(
            lambda data: needle in data[offset:],
            description,
            timeout,
        )

    def wait_for_text_casefold(self, needle: str, timeout: float | None = None) -> None:
        encoded = needle.casefold().encode("utf-8")
        self.wait_for(lambda data: encoded in data.lower(), repr(needle), timeout)

    def fail(self, reason: str) -> None:
        excerpt = self.text[-5000:]
        raise AssertionError(f"{reason}; captured PTY output tail:\n{excerpt!r}")

    def send(self, data: bytes) -> None:
        view = memoryview(data)
        while view:
            try:
                written = os.write(self.master_fd, view)
            except OSError as exc:
                raise AssertionError(f"failed to write keystrokes to PTY: {exc}") from exc
            view = view[written:]

    def draw_mark(self) -> int:
        """Drain pending output and return an offset for fresh-frame detection."""
        self.drain_available()
        return len(self.output)

    def wait_for_frame_after(self, offset: int, timeout: float | None = None) -> None:
        """Wait until a redraw starts after `offset`, ignoring earlier bytes."""
        self.wait_for(
            lambda data: CURSOR_POSITION.search(data[offset:]) is not None,
            "a fresh frame after the previous keystrokes",
            timeout,
        )

    def send_keys(self, data: bytes) -> None:
        """Send keystrokes, then wait for the frame they trigger."""
        offset = self.draw_mark()
        self.send(data)
        self.wait_for_frame_after(offset)

    def wait_for_first_frame(self) -> None:
        """Wait for the first painted frame, without matching any screen text."""

        def painted(data: bytes) -> bool:
            alternate_screen = data.find(b"\x1b[?1049h")
            return (
                alternate_screen >= 0
                and CURSOR_POSITION.search(data[alternate_screen:]) is not None
            )

        self.wait_for(painted, "the first painted frame")

    def send_and_await(
        self,
        data: bytes,
        expected: bytes,
        description: str,
        timeout: float | None = None,
    ) -> int:
        """Send keys, then wait for a marker that only the new state emits.

        `expected` is normally a mode cursor style. Because a frame carries the
        style of the mode it is drawn in, this both proves the editor handled
        these keys before anything else is written and refuses to be satisfied
        by output already queued for an earlier keystroke. That ordering is what
        keeps `Esc` a key of its own: a terminal that receives `Esc` and a
        following `:` together reports one Alt-`:` key event, which the editor
        ignores, so the command would silently become Insert text instead.
        """
        offset = self.draw_mark()
        self.send(data)
        self.wait_for_bytes_after(offset, expected, description, timeout)
        return offset

    def wait_for_displayed_text_after(
        self, offset: int, needle: str, description: str, timeout: float | None = None
    ) -> None:
        """Wait until `needle` is readable in the output after `offset`.

        The comparison ignores styling escapes so cursor cells and prompt
        characters that are written with their own style still read as text.
        """
        self.wait_for(
            lambda data: needle in displayed_text(bytes(data[offset:])),
            description,
            timeout,
        )

    def wait_for_visible_text_after(
        self, offset: int, needle: str, description: str, timeout: float | None = None
    ) -> None:
        """Wait until `needle` is readable after `offset`, ignoring whitespace.

        Buffer text arrives split by cursor moves and unpainted blank cells, so
        a whole sentence only reads as contiguous when whitespace is dropped.
        Use a freshly repainted slice (a resize) when asserting about text that
        may not have changed cells in an incremental frame.
        """
        expected = "".join(needle.split())
        self.wait_for(
            lambda data: expected in visible_text(bytes(data[offset:])),
            description,
            timeout,
        )

    def resize(self, rows: int, columns: int) -> int:
        self.drain_available()
        previous_length = len(self.output)
        self._set_size(rows, columns)
        os.kill(self.process.pid, signal.SIGWINCH)
        return previous_length

    def resize_and_wait(
        self, rows: int, columns: int, attempts: int = 12, attempt_timeout: float = 0.5
    ) -> int:
        """Resize the terminal and wait for the redraw it must trigger.

        A window-size signal can reach the editor before it has installed its
        handler, and such a signal is simply lost, so it is re-sent until a
        fresh frame appears. Repeating the same size is idempotent.
        """
        previous_length = self.resize(rows, columns)
        for _ in range(attempts):
            try:
                self.wait_for_frame_after(previous_length, timeout=attempt_timeout)
            except AssertionError:
                if self.process.poll() is not None:
                    break
                os.kill(self.process.pid, signal.SIGWINCH)
                continue
            return previous_length
        self.fail("no redraw after SIGWINCH")

    def wait_for_exit(self, timeout: float | None = None) -> int:
        deadline = time.monotonic() + (self.timeout if timeout is None else timeout)
        while self.process.poll() is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self.fail("process did not exit before timeout")
            self._read_once(min(remaining, 0.1))
        self.drain_available()
        return self.process.wait()

    def assert_terminal_restored(self, case: unittest.TestCase) -> None:
        actual = termios.tcgetattr(self.slave_fd)
        # macOS sets PENDIN when restored canonical input is reprocessed; it is
        # transient kernel bookkeeping, not a raw-mode setting controlled by us.
        expected = list(self.initial_attrs)
        pending = getattr(termios, "PENDIN", 0)
        actual[3] &= ~pending
        expected[3] &= ~pending
        case.assertEqual(actual, expected, "terminal attributes were not restored")
        output = bytes(self.output)
        for sequence, label in (
            (b"\x1b[?2004h", "bracketed-paste enable"),
            (b"\x1b[?2004l", "bracketed-paste disable"),
            (b"\x1b[?25h", "cursor show"),
            (b"\x1b[?1049h", "alternate-screen enter"),
            (b"\x1b[?1049l", "alternate-screen leave"),
        ):
            case.assertIn(sequence, output, f"missing {label} escape sequence")
        case.assertTrue(
            b"\x1b[0 q" in output or b"\x1b[ q" in output,
            "missing cursor-style reset escape sequence",
        )

    def assert_cleanup_before(self, case: unittest.TestCase, diagnostic: str) -> None:
        output = bytes(self.output)
        diagnostic_at = output.lower().find(diagnostic.encode("utf-8").lower())
        case.assertGreaterEqual(diagnostic_at, 0, f"missing {diagnostic!r} diagnostic")
        for sequence, label in (
            (b"\x1b[?2004l", "bracketed-paste disable"),
            (b"\x1b[?25h", "cursor show"),
            (b"\x1b[0 q", "cursor-style reset"),
            (b"\x1b[?1049l", "alternate-screen leave"),
        ):
            at = output.rfind(sequence, 0, diagnostic_at)
            case.assertGreaterEqual(at, 0, f"{label} did not occur before the diagnostic")

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=1)
        try:
            # On a failed/timed-out test, do not leave this PTY in raw mode.
            termios.tcsetattr(self.slave_fd, termios.TCSANOW, self.initial_attrs)
        except OSError:
            pass
        for fd in (self.master_fd, self.slave_fd):
            try:
                os.close(fd)
            except OSError:
                pass

    def __enter__(self) -> PtySession:
        return self

    def __exit__(self, exc_type: object, exc_value: object, traceback: object) -> None:
        self.close()


class TerminalSmokeTests(unittest.TestCase):
    binary: Path
    fixture: Path
    formatter_fixture: Path

    @classmethod
    def setUpClass(cls) -> None:
        cls.fixture = discover_test_binary(FIXTURE_TARGET, "lifecycle fixture")
        cls.formatter_fixture = discover_test_binary(
            FORMATTER_FIXTURE_TARGET, "formatter fixture"
        )

    def formatter_command(self, *arguments: str) -> str:
        """:format words that run the fixture formatter in one of its modes.

        `:format` splits this text with shell-word rules and never expands it, so
        quoting only has to survive that split.
        """
        return " ".join(
            quote_for_command(word)
            for word in [os.fspath(self.formatter_fixture), "fixture", *arguments]
        )

    def run_editor(
        self,
        *args: str,
        rows: int = 24,
        columns: int = 80,
        cwd: str | None = None,
    ) -> PtySession:
        return PtySession([self.binary, *args], rows=rows, columns=columns, cwd=cwd)

    def assert_successful_editor_exit(self, session: PtySession) -> None:
        self.assertEqual(session.wait_for_exit(), 0, session.text[-5000:])
        session.assert_terminal_restored(self)

    def test_pty_welcome_quit(self) -> None:
        with self.run_editor() as session:
            session.wait_for_text("Postvim")
            self.assertIn("Esc", session.text)
            self.assertIn(":q", session.text)
            session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
            session.send_keys(b"q")
            session.send(b"\r")
            self.assert_successful_editor_exit(session)

    def test_pty_edit_save_quit(self) -> None:
        with self.subTest("insert Unicode text and line break, save exact bytes"):
            with tempfile.TemporaryDirectory(prefix="postvim pty ") as directory:
                destination = Path(directory) / "saved text.txt"
                inserted = "héllo\n世界"
                with self.run_editor() as session:
                    session.wait_for_text("Postvim")
                    session.send_and_await(b"i", INSERT_CURSOR, "insert-mode bar cursor")
                    session.send("héllo".encode("utf-8") + b"\r" + "世界".encode("utf-8"))
                    session.wait_for_text("界")
                    session.send_and_await(
                        b"\x1b", NORMAL_CURSOR, "normal-mode block cursor after Escape"
                    )
                    session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                    session.send(b"w " + os.fsencode(destination) + b"\r")
                    session.wait_for_text("Written")
                    session.send(b":q\r")
                    self.assert_successful_editor_exit(session)
                self.assertEqual(destination.read_bytes(), inserted.encode("utf-8"))

    def test_pty_dirty_quit_then_save(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim dirty ") as directory:
            destination = Path(directory) / "after refusal.txt"
            with self.run_editor() as session:
                session.wait_for_text("Postvim")
                session.send_and_await(b"i", INSERT_CURSOR, "insert-mode bar cursor")
                typed_start = len(session.output)
                session.send(b"unsaved")
                session.wait_for(
                    lambda data: re.search(
                        rb"\x1b\[1;\d+H\x1b\[6 q\x1b\[1;\d+Hd", data[typed_start:]
                    )
                    is not None,
                    "last inserted character",
                )
                session.send_and_await(
                    b"\x1b", NORMAL_CURSOR, "normal-mode block cursor after Escape"
                )
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                session.send_keys(b"q")
                refusal_start = session.draw_mark()
                session.send(b"\r")
                refusal = DIRTY_REFUSAL_PREFIX
                session.wait_for_displayed_text_after(
                    refusal_start, refusal, "the dirty-quit refusal"
                )
                self.assertIsNone(session.process.poll(), "dirty :q must leave the editor running")
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                session.send(b"w " + os.fsencode(destination) + b"\r")
                session.wait_for_text("Written")
                session.send(b":q\r")
                self.assert_successful_editor_exit(session)
                self.assertIn(refusal, session.text)
            self.assertEqual(destination.read_bytes(), b"unsaved")

    def test_pty_resize(self) -> None:
        with self.run_editor(rows=24, columns=80) as session:
            session.wait_for_text("Postvim")
            session.resize_and_wait(12, 40)
            session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
            session.send_keys(b"q")
            session.send(b"\r")
            self.assert_successful_editor_exit(session)

    def assert_only_files(self, directory: Path, expected: list[str]) -> None:
        self.assertEqual(
            sorted(entry.name for entry in directory.iterdir()),
            sorted(expected),
            "the directory must contain only the expected files",
        )

    def test_pty_everyday_edit_undo_redo(self) -> None:
        with self.subTest("undo restores saved bytes, so :q accepts"):
            with tempfile.TemporaryDirectory(prefix="postvim undo ") as directory:
                destination = Path(directory) / "undo clean.txt"
                original = b"alpha beta\n"
                destination.write_bytes(original)
                with self.run_editor(os.fspath(destination)) as session:
                    session.wait_for_first_frame()
                    session.send_keys(b"x")
                    session.send_keys(b"u")
                    session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                    session.send_keys(b"q")
                    session.send(b"\r")
                    self.assert_successful_editor_exit(session)
                self.assertEqual(
                    destination.read_bytes(),
                    original,
                    "undo must restore the saved bytes, not just the screen",
                )
                self.assert_only_files(Path(directory), ["undo clean.txt"])
        with self.subTest("redo re-applies the deletion and :wq saves exact bytes"):
            with tempfile.TemporaryDirectory(prefix="postvim redo ") as directory:
                destination = Path(directory) / "redo saved.txt"
                destination.write_bytes(b"alpha beta\n")
                with self.run_editor(os.fspath(destination)) as session:
                    session.wait_for_first_frame()
                    session.send_keys(b"x")
                    session.send_keys(b"u")
                    # Ctrl-r is sent as 0x12.
                    session.send_keys(b"\x12")
                    session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                    session.send_keys(b"wq")
                    session.send(b"\r")
                    self.assert_successful_editor_exit(session)
                self.assertEqual(destination.read_bytes(), b"lpha beta\n")
                self.assert_only_files(Path(directory), ["redo saved.txt"])

    def test_pty_visual_search_and_wq(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim visual ") as directory:
            destination = Path(directory) / "visual search.txt"
            destination.write_bytes(b"alpha beta gamma\n")
            with self.run_editor(os.fspath(destination)) as session:
                session.wait_for_first_frame()
                session.send_and_await(b"/", PROMPT_CURSOR, "search prompt cursor")
                session.send_keys(b"gamma")
                session.send_and_await(
                    b"\r",
                    NORMAL_CURSOR,
                    "normal-mode block cursor after submitting the search",
                )
                session.send_keys(b"v")
                session.send_keys(b"$")
                session.send_keys(b"d")
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                session.send_keys(b"wq")
                session.send(b"\r")
                self.assert_successful_editor_exit(session)
            self.assertEqual(destination.read_bytes(), b"alpha beta \n")
            self.assert_only_files(Path(directory), ["visual search.txt"])

    def test_pty_force_quit_and_clean_x(self) -> None:
        with self.subTest("dirty :q reports the documented message, :q! discards"):
            with tempfile.TemporaryDirectory(prefix="postvim force quit ") as directory:
                destination = Path(directory) / "force quit.txt"
                destination.write_bytes(b"orig\n")
                with self.run_editor(
                    os.fspath(destination), rows=30, columns=100
                ) as session:
                    session.wait_for_first_frame()
                    session.send_and_await(b"i", INSERT_CURSOR, "insert-mode bar cursor")
                    session.send_keys(b"X")
                    # Escape is sent and awaited on its own. Writing it together
                    # with the following `:` makes the terminal deliver a single
                    # Alt-`:` key event, which the editor ignores, so the command
                    # would be typed into the buffer instead of run.
                    session.send_and_await(
                        b"\x1b", NORMAL_CURSOR, "normal-mode block cursor after Escape"
                    )
                    session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                    session.send_keys(b"q")
                    refusal_start = session.draw_mark()
                    session.send(b"\r")
                    session.wait_for_displayed_text_after(
                        refusal_start,
                        DOCUMENTED_DIRTY_MESSAGE,
                        "the documented dirty-quit refusal",
                    )
                    self.assertIsNone(
                        session.process.poll(),
                        "a dirty :q must leave the editor running",
                    )
                    session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                    session.send_keys(b"q!")
                    session.send(b"\r")
                    self.assert_successful_editor_exit(session)
                self.assertEqual(
                    destination.read_bytes(),
                    b"orig\n",
                    ":q! must discard the buffer without writing",
                )
                self.assert_only_files(Path(directory), ["force quit.txt"])
        with self.subTest("clean :x exits without writing"):
            with tempfile.TemporaryDirectory(prefix="postvim clean x ") as directory:
                destination = Path(directory) / "clean x.txt"
                destination.write_bytes(b"clean\n")
                before = destination.stat()
                with self.run_editor(os.fspath(destination)) as session:
                    session.wait_for_first_frame()
                    session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                    session.send_keys(b"x")
                    session.send(b"\r")
                    self.assert_successful_editor_exit(session)
                after = destination.stat()
                self.assertEqual(destination.read_bytes(), b"clean\n")
                self.assertEqual(
                    (after.st_ino, after.st_mtime_ns),
                    (before.st_ino, before.st_mtime_ns),
                    "a clean :x must not rewrite the file",
                )
                self.assert_only_files(Path(directory), ["clean x.txt"])
        with self.subTest("clean :x on an unnamed buffer creates nothing"):
            with tempfile.TemporaryDirectory(prefix="postvim unnamed x ") as directory:
                with self.run_editor(cwd=directory) as session:
                    session.wait_for_first_frame()
                    session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                    session.send_keys(b"x")
                    session.send(b"\r")
                    self.assert_successful_editor_exit(session)
                self.assertEqual(
                    list(Path(directory).iterdir()),
                    [],
                    "a clean :x must not create a file",
                )

    # -----------------------------------------------------------------
    # External reconciliation over a real terminal
    # -----------------------------------------------------------------

    def test_pty_external_clean_reload(self) -> None:
        """A clean idle editor shows an external write without any keystroke."""
        with tempfile.TemporaryDirectory(prefix="postvim external reload ") as directory:
            destination = Path(directory) / "reload.txt"
            # The two versions differ in every cell, so the incremental frame
            # that reports the change also repaints the whole new line and can be
            # read directly, without a screen model or a forced redraw.
            destination.write_bytes(b"old draft line\n")
            with self.run_editor(os.fspath(destination)) as session:
                session.wait_for_visible_text_after(
                    0, "old draft line", "the originally opened text"
                )
                settled = session.draw_mark()
                atomic_replace(destination, b"NEW: 9th pass\n")
                # Deliberately no keystroke: reconciliation is what must notice.
                session.wait_for_visible_text_after(
                    settled,
                    "NEW: 9th pass",
                    "the externally written text, with no key pressed",
                    timeout=REDRAW_TIMEOUT,
                )
                self.assertIsNone(
                    session.process.poll(),
                    "noticing an external change must not exit the editor",
                )
                # A resize repaints every cell, independently confirming that the
                # buffer holds the new text and none of the old.
                repainted = session.resize_and_wait(24, 90)
                session.wait_for_visible_text_after(
                    repainted,
                    "NEW: 9th pass",
                    "the reloaded buffer text",
                    timeout=REDRAW_TIMEOUT,
                )
                self.assertNotIn(
                    "old",
                    visible_text(bytes(session.output[repainted:])),
                    "the reloaded buffer must no longer show the replaced text",
                )
                # The reloaded buffer matches the disk, so :q accepts.
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                session.send_keys(b"q")
                session.send(b"\r")
                self.assert_successful_editor_exit(session)
            self.assertEqual(destination.read_bytes(), b"NEW: 9th pass\n")
            self.assert_only_files(Path(directory), ["reload.txt"])

    def test_pty_dirty_external_conflict(self) -> None:
        """Local edits survive an external write, and :w refuses to overwrite it."""
        with tempfile.TemporaryDirectory(prefix="postvim external conflict ") as directory:
            destination = Path(directory) / "conflict.txt"
            destination.write_bytes(b"original\n")
            with self.run_editor(os.fspath(destination), rows=30, columns=100) as session:
                session.wait_for_first_frame()
                session.send_and_await(b"i", INSERT_CURSOR, "insert-mode bar cursor")
                session.send_keys(b"local")
                session.send_and_await(
                    b"\x1b", NORMAL_CURSOR, "normal-mode block cursor after Escape"
                )
                settled = session.draw_mark()
                atomic_replace(destination, b"foreign edit\n")
                # Give reconciliation a frame to report the conflict; the
                # buffer itself must not change.
                try:
                    session.wait_for_frame_after(settled, timeout=REDRAW_TIMEOUT)
                except AssertionError:
                    pass
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                session.send_keys(b"w")
                refusal_start = session.draw_mark()
                session.send(b"\r")
                session.wait_for_frame_after(refusal_start, timeout=REDRAW_TIMEOUT)
                self.assertIsNone(
                    session.process.poll(),
                    "a refused save must leave the editor running",
                )
                # The status line still marks the buffer modified, which is how a
                # reader sees that the local text was kept rather than replaced.
                session.wait_for_displayed_text_after(
                    refusal_start,
                    "[+]",
                    "the buffer still marked modified after the refused save",
                )
                self.assertEqual(
                    destination.read_bytes(),
                    b"foreign edit\n",
                    "a refused save must not overwrite the external bytes",
                )
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                session.send_keys(b"q!")
                session.send(b"\r")
                self.assert_successful_editor_exit(session)
            self.assertEqual(destination.read_bytes(), b"foreign edit\n")
            self.assert_only_files(Path(directory), ["conflict.txt"])

    def test_pty_format_selection_undo(self) -> None:
        """Format a Visual selection, undo it, then format and save."""
        with tempfile.TemporaryDirectory(prefix="postvim format selection ") as directory:
            destination = Path(directory) / "format selection.txt"
            destination.write_bytes(b"alpha beta gamma\n")
            command = self.formatter_command("rewrite")
            with self.run_editor(os.fspath(destination), rows=30, columns=100) as session:
                session.wait_for_first_frame()
                # `w` reaches "beta"; the characterwise selection covers it.
                session.send_keys(b"w")
                session.send_keys(b"v")
                session.send_keys(b"e")
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                formatted_start = session.draw_mark()
                session.send(f"format {command} rewrite\r".encode("utf-8"))
                session.wait_for_visible_text_after(
                    formatted_start,
                    "BETA",
                    "the formatted selection",
                    timeout=REDRAW_TIMEOUT,
                )
                self.assertIsNone(
                    session.process.poll(), "formatting must not exit the editor"
                )
                # One undo returns to the saved bytes, so :q would accept.
                undone_start = session.draw_mark()
                session.send_keys(b"u")
                session.wait_for_visible_text_after(
                    undone_start,
                    "beta",
                    "the text restored by a single undo",
                    timeout=REDRAW_TIMEOUT,
                )
                self.assertEqual(
                    destination.read_bytes(),
                    b"alpha beta gamma\n",
                    "formatting must not write to disk",
                )
                # A whole-document format is one unsaved change; :wq saves it.
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                session.send(f"format {command} rewrite\r".encode("utf-8"))
                session.send_and_await(b":", PROMPT_CURSOR, "command prompt cursor")
                session.send_keys(b"wq")
                session.send(b"\r")
                self.assert_successful_editor_exit(session)
            self.assertEqual(destination.read_bytes(), b"ALPHA BETA GAMMA\n")
            self.assert_only_files(Path(directory), ["format selection.txt"])

    def run_fixture_failure(self, mode: str) -> None:
        with PtySession([self.fixture, mode]) as session:
            session.wait_for_text(FIXTURE_READY)
            status = session.wait_for_exit()
            self.assertNotEqual(status, 0, f"fixture {mode!r} unexpectedly succeeded")
            self.assertIn("injected", session.text.lower())
            session.assert_terminal_restored(self)
            session.assert_cleanup_before(self, "injected")

    def test_pty_error_restores_terminal(self) -> None:
        self.run_fixture_failure("error")

    def test_pty_panic_restores_terminal(self) -> None:
        self.run_fixture_failure("panic")


class FormatterFixtureTests(unittest.TestCase):
    """The formatter fixture itself, driven without a PTY and with deadlines."""

    fixture: Path

    @classmethod
    def setUpClass(cls) -> None:
        cls.fixture = discover_test_binary(
            FORMATTER_FIXTURE_TARGET, "formatter fixture"
        )

    def run_fixture(
        self,
        *args: str,
        stdin: bytes = b"",
        timeout: float = DEFAULT_DEADLINE,
        cwd: str | os.PathLike[str] | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        return run_with_deadline(
            [self.fixture, *args], stdin=stdin, timeout=timeout, cwd=cwd
        )

    def test_fixture_protocol_streams(self) -> None:
        listing = self.run_fixture("fixture", "modes")
        self.assertEqual(listing.returncode, 0, listing.stderr)
        self.assertEqual(listing.stdout, FIXTURE_MODE_LINES)

        identity = self.run_fixture("fixture", "identity", stdin="héllo\n".encode())
        self.assertEqual(identity.returncode, 0)
        self.assertEqual(identity.stdout, "héllo\n".encode())
        self.assertEqual(identity.stderr, b"")

        rewrite = self.run_fixture("fixture", "rewrite", stdin=b"aBc\xff\n")
        self.assertEqual(rewrite.returncode, 0)
        self.assertEqual(rewrite.stdout, b"ABC\xff\n")

        empty = self.run_fixture("fixture", "empty", stdin=b"gone\n")
        self.assertEqual((empty.returncode, empty.stdout), (0, b""))

        warning = self.run_fixture("fixture", "warning", stdin=b"kept\n")
        self.assertEqual(warning.returncode, 0)
        self.assertEqual(warning.stdout, b"kept\n")
        self.assertIn(b"warning", warning.stderr)

        error = self.run_fixture("fixture", "error", stdin=b"rejected\n")
        self.assertEqual(error.returncode, 3)
        self.assertEqual(error.stdout, b"")
        self.assertIn(b"rejected", error.stderr)

        invalid = self.run_fixture("fixture", "invalid-utf8", stdin=b"input\n")
        self.assertEqual(invalid.returncode, 0)
        with self.assertRaises(UnicodeDecodeError):
            invalid.stdout.decode("utf-8")

        large = self.run_fixture("fixture", "large", stdin=b"input\n")
        self.assertEqual(large.returncode, 0)
        self.assertEqual(len(large.stdout), 64 * 16384)
        self.assertEqual(len(large.stderr), 64 * 4096)

        early = self.run_fixture("fixture", "early-close", stdin=b"unread\n")
        self.assertEqual(early.returncode, 0)
        self.assertEqual(early.stdout, b"partial\n")

        diagnostics = self.run_fixture("fixture", "diagnostics", stdin=b"input\n")
        self.assertEqual(diagnostics.returncode, 5)
        self.assertEqual(diagnostics.stdout, b"")
        with self.assertRaises(UnicodeDecodeError):
            diagnostics.stderr.decode("utf-8")

        unknown = self.run_fixture("fixture", "no-such-mode")
        self.assertEqual(unknown.returncode, 2)
        self.assertEqual(unknown.stdout, b"")

    def test_fixture_literal_arguments_and_stdin_context(self) -> None:
        weird = ["|", "$HOME", ">", "hé llo.txt", "-x", "a b\tc"]
        with tempfile.TemporaryDirectory(prefix="postvim fixture args ") as directory:
            expected_cwd = Path(directory).resolve()
            report = self.run_fixture(
                "fixture", "args", *weird, stdin=b"stdin bytes\n", cwd=directory
            )
            self.assertEqual(report.returncode, 0, report.stderr)
        lines = report.stdout.decode("utf-8").splitlines()
        arguments = [unhex_bytes(line[4:]) for line in lines if line.startswith("ARG ")]
        self.assertEqual(arguments, [argument.encode() for argument in weird])
        cwd_line = next(line for line in lines if line.startswith("CWD "))
        self.assertEqual(
            Path(unhex_bytes(cwd_line[4:]).decode()).resolve(),
            expected_cwd,
        )
        stdin_line = next(line for line in lines if line.startswith("STDIN "))
        self.assertEqual(unhex_bytes(stdin_line[6:]), b"stdin bytes\n")

    def test_fixture_mutate_file_is_visible_to_another_process(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim fixture mutation ") as directory:
            destination = Path(directory) / "document.txt"
            destination.write_bytes(b"original\n")
            mutation = self.run_fixture(
                "fixture", "mutate-file", os.fspath(destination), stdin=b"stdin\n"
            )
            self.assertEqual(mutation.returncode, 0, mutation.stderr)
            self.assertEqual(mutation.stdout, b"stdin\n")
            self.assertEqual(destination.read_bytes(), EXTERNAL_EDIT)

    def test_fixture_default_scenarios_pass(self) -> None:
        """The no-argument run is what `cargo test` executes for this target."""
        completed = self.run_fixture(timeout=DEFAULT_DEADLINE)
        self.assertEqual(
            completed.returncode,
            0,
            "fixture scenarios failed:\n" + completed.stdout.decode("utf-8", "replace"),
        )
        reported = [
            line[len("scenario ok: ") :]
            for line in completed.stdout.decode("utf-8").splitlines()
            if line.startswith("scenario ok: ")
        ]
        self.assertEqual(sorted(reported), FIXTURE_SCENARIOS)

    def test_fixture_flood_completes_without_deadlock(self) -> None:
        flood = self.run_fixture("scenario", "flood", stdin=b"input\n", timeout=60.0)
        self.assertEqual(flood.returncode, 0, flood.stderr)
        self.assertEqual(len(flood.stdout), 256 * 16384)
        self.assertEqual(len(flood.stderr), 256 * 4096)

    def test_fixture_hang_is_contained_by_the_deadline(self) -> None:
        with self.assertRaises(DeadlineExceeded) as raised:
            self.run_fixture("scenario", "hang", timeout=1.0)
        self.assertIn(b"SCENARIO_HANGING", raised.exception.output)
        self.assertIsNotNone(
            raised.exception.returncode,
            "the contained process group must be waited for",
        )
        self.assertNotEqual(raised.exception.returncode, 0)


class FormatCliTests(unittest.TestCase):
    """The independent `postvim --format` adapter, parity-checked through the fixture."""

    binary: Path
    fixture: Path

    @classmethod
    def setUpClass(cls) -> None:
        cls.fixture = discover_test_binary(
            FORMATTER_FIXTURE_TARGET, "formatter fixture"
        )

    def run_format(
        self,
        *arguments: str,
        stdin: bytes = b"",
        cwd: str | os.PathLike[str] | None = None,
        timeout: float = DEFAULT_DEADLINE,
    ) -> subprocess.CompletedProcess[bytes]:
        return run_with_deadline(
            [self.binary, "--format", *arguments],
            stdin=stdin,
            timeout=timeout,
            cwd=cwd,
        )

    def formatter_argv(self, *arguments: str) -> list[str]:
        """The fixture invoked as a formatter: `fixture MODE [ARG...]`."""
        return [os.fspath(self.fixture), "fixture", *arguments]

    def scenario_argv(self, *arguments: str) -> list[str]:
        """The fixture invoked as a long-running scenario: `scenario NAME`."""
        return [os.fspath(self.fixture), "scenario", *arguments]

    def assert_no_terminal_takeover(self, completed: subprocess.CompletedProcess[bytes]) -> None:
        for stream in (completed.stdout, completed.stderr):
            for sequence in TERMINAL_TAKEOVER:
                self.assertNotIn(
                    sequence,
                    stream,
                    "the noninteractive CLI must not take over a terminal",
                )

    def test_format_cli_stdout_default_does_not_write(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim cli stdout ") as directory:
            destination = Path(directory) / "document.txt"
            destination.write_bytes(b"abc\n")
            before = destination.stat()
            completed = self.run_format(
                "--input", os.fspath(destination), "--",
                *self.formatter_argv("identity"),
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(completed.stdout, b"abc\n")
            self.assertEqual(completed.stderr, b"")
            self.assert_no_terminal_takeover(completed)
            after = destination.stat()
            self.assertEqual(destination.read_bytes(), b"abc\n")
            self.assertEqual(
                (after.st_ino, after.st_mtime_ns),
                (before.st_ino, before.st_mtime_ns),
                "the default mode must not touch the file",
            )

    def test_format_cli_transforms_through_a_range(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim cli range ") as directory:
            destination = Path(directory) / "document.txt"
            destination.write_bytes(b"abc\n")
            completed = self.run_format(
                "--input", os.fspath(destination), "--range", "0:3", "--",
                *self.formatter_argv("rewrite"),
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(completed.stdout, b"ABC\n")
            self.assertEqual(destination.read_bytes(), b"abc\n")

    def test_format_cli_reads_stdin_without_an_input_path(self) -> None:
        completed = self.run_format("--", *self.formatter_argv("rewrite"), stdin=b"abc\n")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, b"ABC\n")

    def test_format_cli_empty_output_deletes_the_document(self) -> None:
        completed = self.run_format("--", *self.formatter_argv("empty"), stdin=b"gone\n")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, b"")

    def test_format_cli_write_modes(self) -> None:
        with self.subTest("--write saves and prints nothing"):
            with tempfile.TemporaryDirectory(prefix="postvim cli write ") as directory:
                destination = Path(directory) / "document.txt"
                destination.write_bytes(b"abc\n")
                completed = self.run_format(
                    "--input", os.fspath(destination), "--write", "--",
                    *self.formatter_argv("rewrite"),
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(completed.stdout, b"")
                self.assertEqual(destination.read_bytes(), b"ABC\n")
                self.assertEqual(
                    sorted(entry.name for entry in Path(directory).iterdir()),
                    ["document.txt"],
                    "a guarded write leaves no temporary file behind",
                )
        with self.subTest("--write --stdout saves first, then prints the document"):
            with tempfile.TemporaryDirectory(prefix="postvim cli write stdout ") as directory:
                destination = Path(directory) / "document.txt"
                destination.write_bytes(b"abc\n")
                completed = self.run_format(
                    "--input", os.fspath(destination), "--write", "--stdout", "--",
                    *self.formatter_argv("rewrite"),
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(completed.stdout, b"ABC\n")
                self.assertEqual(destination.read_bytes(), b"ABC\n")
        with self.subTest("--stdout alone is the explicit default"):
            with tempfile.TemporaryDirectory(prefix="postvim cli stdout flag ") as directory:
                destination = Path(directory) / "document.txt"
                destination.write_bytes(b"abc\n")
                completed = self.run_format(
                    "--input", os.fspath(destination), "--stdout", "--",
                    *self.formatter_argv("rewrite"),
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(completed.stdout, b"ABC\n")
                self.assertEqual(destination.read_bytes(), b"abc\n")

    def test_format_cli_warnings_do_not_become_document_text(self) -> None:
        completed = self.run_format(
            "--", *self.formatter_argv("warning"), stdin=b"kept\n"
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, b"kept\n")
        self.assertIn(b"warning", completed.stderr)

    def test_format_cli_error_stream_contract(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim cli errors ") as directory:
            destination = Path(directory) / "document.txt"
            destination.write_bytes(b"original\n")

            failure = self.run_format(
                "--input", os.fspath(destination), "--write", "--",
                *self.formatter_argv("error"),
            )
            self.assertNotEqual(failure.returncode, 0)
            self.assertEqual(failure.stdout, b"")
            self.assertIn(b"rejected", failure.stderr)
            self.assertEqual(
                destination.read_bytes(),
                b"original\n",
                "a failed formatter must not write",
            )
            self.assert_no_terminal_takeover(failure)

            invalid = self.run_format(
                "--input", os.fspath(destination), "--write", "--",
                *self.formatter_argv("invalid-utf8"),
            )
            self.assertNotEqual(invalid.returncode, 0)
            self.assertEqual(invalid.stdout, b"")
            self.assertEqual(destination.read_bytes(), b"original\n")

            diagnostics = self.run_format(
                "--", *self.formatter_argv("diagnostics"), stdin=b"input\n"
            )
            self.assertNotEqual(diagnostics.returncode, 0)
            self.assertEqual(diagnostics.stdout, b"")
            self.assertTrue(diagnostics.stderr, "diagnostics must be reported")
            self.assertNotIn(
                b"\x1b",
                diagnostics.stderr,
                "diagnostics must be escaped, not emitted as terminal control",
            )
            self.assertLess(
                len(diagnostics.stderr),
                8000,
                "diagnostics must be summarized rather than copied wholesale",
            )

    def test_format_cli_rejects_bad_usage_without_side_effects(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim cli usage ") as directory:
            destination = Path(directory) / "document.txt"
            destination.write_bytes(b"abc\n")
            program = self.formatter_argv("identity")
            document = os.fspath(destination)
            cases: list[tuple[str, list[str]]] = [
                ("missing option terminator", ["--input", document, *program]),
                ("empty program", ["--input", document, "--"]),
                ("unknown option", ["--input", document, "--nope", "--", *program]),
                (
                    "duplicate input",
                    ["--input", document, "--input", document, "--", *program],
                ),
                ("reversed range", ["--input", document, "--range", "3:1", "--", *program]),
                ("non-numeric range", ["--input", document, "--range", "a:b", "--", *program]),
                ("missing range value", ["--input", document, "--range", "--", *program]),
                ("range past the document", ["--input", document, "--range", "0:99", "--", *program]),
                ("write from stdin", ["--write", "--", *program]),
                ("missing input file", ["--input", os.path.join(directory, "absent.txt"), "--", *program]),
            ]
            for description, arguments in cases:
                with self.subTest(description):
                    completed = self.run_format(*arguments, stdin=b"abc\n")
                    self.assertNotEqual(
                        completed.returncode, 0, f"{description} must be rejected"
                    )
                    self.assertEqual(completed.stdout, b"", f"{description} wrote stdout")
                    self.assertEqual(destination.read_bytes(), b"abc\n")
            self.assertEqual(
                sorted(entry.name for entry in Path(directory).iterdir()),
                ["document.txt"],
                "rejected invocations must not create files",
            )

    def test_format_cli_dash_input_is_a_literal_file(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim cli dash ") as directory:
            dash = Path(directory) / "-"
            dash.write_bytes(b"dash\n")
            completed = self.run_format(
                "--input", "-", "--", *self.formatter_argv("identity"), cwd=directory
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(completed.stdout, b"dash\n")
            self.assertEqual(dash.read_bytes(), b"dash\n")

    def test_format_cli_passes_literal_arguments_and_inherits_working_directory(self) -> None:
        weird = ["|", "$HOME", ">", "hé llo.txt", "-x"]
        with tempfile.TemporaryDirectory(prefix="postvim cli args ") as directory:
            completed = self.run_format(
                "--", *self.formatter_argv("args", *weird),
                stdin=b"stdin bytes\n", cwd=directory,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            lines = completed.stdout.decode("utf-8").splitlines()
            arguments = [unhex_bytes(line[4:]) for line in lines if line.startswith("ARG ")]
            self.assertEqual(
                arguments,
                [argument.encode() for argument in weird],
                "formatter arguments must arrive literally, with no shell expansion",
            )
            cwd_line = next(line for line in lines if line.startswith("CWD "))
            self.assertEqual(
                Path(unhex_bytes(cwd_line[4:]).decode()).resolve(),
                Path(directory).resolve(),
            )

    def test_format_cli_conflict_during_formatter(self) -> None:
        with tempfile.TemporaryDirectory(prefix="postvim cli conflict ") as directory:
            destination = Path(directory) / "document.txt"
            destination.write_bytes(b"before\n")
            completed = self.run_format(
                "--input", os.fspath(destination), "--write", "--",
                *self.formatter_argv("mutate-file", os.fspath(destination)),
            )
            self.assertNotEqual(
                completed.returncode, 0, "a conflicted save must be refused"
            )
            self.assertEqual(completed.stdout, b"", "a conflict must not emit a document")
            self.assertEqual(
                destination.read_bytes(),
                EXTERNAL_EDIT,
                "the external bytes must survive the refused save",
            )
            self.assertEqual(
                sorted(entry.name for entry in Path(directory).iterdir()),
                ["document.txt"],
                "a conflicted save must not leave a temporary file",
            )

    def test_format_cli_large_bidirectional_streams_do_not_deadlock(self) -> None:
        payload = b"0123456789abcdef" * 65536  # 1 MiB of stdin
        completed = self.run_format(
            "--", *self.scenario_argv("flood"), stdin=payload, timeout=120.0
        )
        self.assertEqual(completed.returncode, 0, completed.stderr[:2000])
        self.assertEqual(
            len(completed.stdout),
            256 * 16384,
            "the whole replacement must arrive on stdout",
        )
        self.assertTrue(
            completed.stderr,
            "a successful run still reports the formatter's diagnostics",
        )
        self.assertLess(
            len(completed.stderr),
            8000,
            "diagnostics are summarized, not copied wholesale",
        )

    def test_format_cli_delivers_the_whole_selection_on_stdin(self) -> None:
        payload = b"abc\ndef\n"
        completed = self.run_format("--", *self.scenario_argv("drain"), stdin=payload)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout,
            f"DRAINED {len(payload)}\n".encode(),
            "the formatter must receive every selected byte, then see stdin close",
        )

    def test_format_cli_hanging_formatter_is_contained(self) -> None:
        """A formatter that never exits must not hang the suite."""
        with self.assertRaises(DeadlineExceeded) as raised:
            self.run_format(
                "--", *self.scenario_argv("hang"), stdin=b"abc\n", timeout=2.0
            )
        self.assertIsNotNone(
            raised.exception.returncode,
            "the contained process group must be waited for",
        )

    def test_format_cli_early_stdin_close_does_not_hang(self) -> None:
        payload = b"x" * (256 * 1024)
        completed = self.run_format(
            "--", *self.formatter_argv("early-close"), stdin=payload, timeout=60.0
        )
        self.assertEqual(
            completed.stdout,
            b"",
            "an incomplete delivery must not yield a replacement document",
        )
        self.assertNotEqual(
            completed.returncode,
            0,
            "successful status alone must not hide an incomplete stdin delivery",
        )

    def test_format_cli_leaves_interactive_launch_grammar_alone(self) -> None:
        for arguments, expected in (
            (["--help"], b"Postvim"),
            (["--version"], b"postvim "),
        ):
            with self.subTest(arguments[0]):
                completed = run_with_deadline([self.binary, *arguments], timeout=60.0)
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertIn(expected, completed.stdout)
                self.assert_no_terminal_takeover(completed)


class TransformationCleanupTests(unittest.TestCase):
    """The library's formatter process seams, run under a hard deadline.

    `cargo test --lib` covers these as ordinary unit tests, but they spawn real
    children and deliberately fail reads, writes, and worker starts. A seam that
    stopped cleaning up would hang that run with no containment, so the same
    library test binary is located through Cargo's JSON messages and executed
    here inside the deadline.
    """

    library_tests: Path
    TRANSFORM_PREFIX = "transform::tests::"
    # Names that describe a cleanup or deadlock guarantee from the plan's AC.5.
    REQUIRED_TOPICS = ("reaps", "cleans_up", "worker_panic", "broken_pipe", "incomplete_delivery", "deadlock")

    @classmethod
    def setUpClass(cls) -> None:
        cls.library_tests = require_executable(
            cargo_executables(["test", "--lib", "--no-run"]),
            SHIPPING_TARGET,
            "library test binary",
        )

    def list_library_tests(self) -> list[str]:
        listed = run_with_deadline([self.library_tests, "--list"], timeout=120.0)
        self.assertEqual(listed.returncode, 0, listed.stderr[-2000:])
        names = []
        for line in listed.stdout.decode("utf-8", "replace").splitlines():
            # libtest prints "module::path::test: test", and the module path
            # itself contains colons, so split from the right.
            name = line.rsplit(": ", 1)[0].strip()
            if name:
                names.append(name)
        return names

    def test_transform_cleanup_seams_run_under_a_deadline(self) -> None:
        transform_tests = [
            name for name in self.list_library_tests() if name.startswith(self.TRANSFORM_PREFIX)
        ]
        self.assertTrue(
            transform_tests,
            f"the library test binary lists no {self.TRANSFORM_PREFIX} tests",
        )
        covered = {
            topic
            for topic in self.REQUIRED_TOPICS
            if any(topic in name for name in transform_tests)
        }
        self.assertEqual(
            covered,
            set(self.REQUIRED_TOPICS),
            "the injected formatter-process seams must be covered by library tests; "
            f"missing {sorted(set(self.REQUIRED_TOPICS) - covered)}",
        )

        completed = run_with_deadline(
            [self.library_tests, self.TRANSFORM_PREFIX, "--test-threads=1"],
            timeout=600.0,
        )
        output = completed.stdout.decode("utf-8", "replace")
        self.assertEqual(
            completed.returncode,
            0,
            output[-4000:] + completed.stderr.decode("utf-8", "replace")[-2000:],
        )
        self.assertIn("0 failed", output)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "binary",
        type=Path,
        nargs="?",
        help="path to the built postvim executable (discovered through Cargo if omitted)",
    )
    args = parser.parse_args()
    if args.binary is None:
        binary = discover_shipping_binary()
    else:
        binary = args.binary.resolve()
        if not binary.is_file() or not os.access(binary, os.X_OK):
            parser.error(f"not an executable file: {binary}")
    TerminalSmokeTests.binary = binary
    FormatCliTests.binary = binary
    unittest.main(argv=[sys.argv[0]], verbosity=2)


if __name__ == "__main__":
    main()
