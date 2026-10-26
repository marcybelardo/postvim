#!/usr/bin/env python3
"""Unix PTY smoke tests for Postvim's real terminal lifecycle.

Run from the repository root with the built editor path, for example:
    python3 tests/terminal_smoke.py target/debug/postvim

The lifecycle fixture is built and located from Cargo's JSON messages. This
harness deliberately uses only Python's standard library and a real PTY.
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
import termios
import tempfile
import time
import unittest
from pathlib import Path
from typing import Callable


ROOT = Path(__file__).resolve().parent.parent
FIXTURE_TARGET = "terminal_lifecycle_fixture"
FIXTURE_READY = "TERMINAL_FIXTURE_READY"
DEFAULT_TIMEOUT = 8.0
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


def displayed_text(data: bytes) -> str:
    """Strip styling escapes from one fresh output slice.

    Cursor cells and prompt characters are written with their own style
    sequences, so screen text is never contiguous in the raw stream. Removing
    only the escape sequences recovers what a reader would see in that slice
    without modelling the screen, cursor, or redraw order.
    """
    return ANSI_SEQUENCE.sub(b"", data).decode("utf-8", errors="replace")


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

    @classmethod
    def setUpClass(cls) -> None:
        cargo = subprocess.run(
            [
                "cargo",
                "test",
                "--test",
                FIXTURE_TARGET,
                "--no-run",
                "--message-format=json",
            ],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=180,
        )
        if cargo.returncode != 0:
            raise RuntimeError(
                "failed to build lifecycle fixture with Cargo:\n"
                + cargo.stdout[-5000:]
                + cargo.stderr[-5000:]
            )
        artifact: str | None = None
        for line in cargo.stdout.splitlines():
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            target = message.get("target", {})
            if (
                message.get("reason") == "compiler-artifact"
                and target.get("name") == FIXTURE_TARGET
                and message.get("executable")
            ):
                artifact = message["executable"]
        if artifact is None:
            raise RuntimeError(
                f"Cargo JSON output did not include executable for {FIXTURE_TARGET}; "
                f"stdout tail:\n{cargo.stdout[-5000:]}"
            )
        cls.fixture = Path(artifact).resolve()
        if not cls.fixture.is_file():
            raise RuntimeError(f"Cargo reported a missing fixture executable: {cls.fixture}")

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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="path to the built postvim executable")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error(f"not an executable file: {binary}")
    TerminalSmokeTests.binary = binary
    unittest.main(argv=[sys.argv[0]], verbosity=2)


if __name__ == "__main__":
    main()
