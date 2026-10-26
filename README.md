# Postvim

A small, single-buffer UTF-8 terminal editor. It implements a deliberately
bounded, everyday subset of Vim's modal editing—motions, operators, undo and
redo, Visual selection, literal search, and explicit save/quit commands. It is
not Vim compatibility, and not the tentative extension ideas in
[the original concept note](docs/Concept.md). The exact key inventory and the
places where Postvim deliberately differs are in
[docs/keymaps.md](docs/keymaps.md).

## Build and run

Requires Rust 1.88 or newer (Rust 2024).

```sh
cargo build
cargo run
cargo run -- 'notes with spaces.txt'
# A filename beginning with '-' needs the option separator:
cargo run -- -- -notes.txt
```

`postvim [--] [path]` opens one existing regular UTF-8 file, or an empty named buffer for a missing path. A missing file is not created until a successful write. Without a path it opens a pristine unnamed buffer and welcome screen. Filesystem paths are native OS paths; command-entered save paths are Unicode text. `--help` and `--version` run without a terminal. Extra arguments, unreadable/nonregular files, and invalid UTF-8 fail before terminal initialization. Editing requires interactive stdin and stdout.

## Controls

The complete key inventory, Ex grammar, and deliberate deviations are in
[docs/keymaps.md](docs/keymaps.md); this section is the short version.

- Normal mode: `h`, `j`, `k`, `l` move; `w`/`W`, `b`/`B`, `e`/`E` move by word;
  `0`, `^`, `$` go to the line start, first non-blank, and last character;
  `gg`/`G` go to the buffer start/end; `f`/`F`/`t`/`T` find a character on the
  line, with `;`/`,` repeating and reversing it.
- Counts work as in Vim and multiply where Vim multiplies them (`2d3w` deletes
  six words); they saturate at 1,000,000 rather than overflowing.
- `i`, `I`, `a`, `A` insert; `o`/`O` open a line; `x`/`X` delete; `r` replaces;
  `s`/`S` substitute; `D`/`C` delete or change to the end of the line. `d`,
  `c`, `y` take a motion or a doubled key (`dd`, `cc`, `yy`), and `p`/`P` put.
  Text objects, `Y`, `J`, `~`, and replace mode are not implemented.
- `u` undoes, `Ctrl-r` redoes, and `.` repeats the last change at the current
  cursor. One complete change is one undo step, and an Insert session is a
  single step.
- `v`/`V` start characterwise/linewise Visual mode; `d`, `c`, `y`, `x`, `p`,
  `P` act on the selection and `Esc` cancels. Visual block mode is not
  implemented.
- `/` and `?` search literally within one line, and `n`/`N` step through matches,
  wrapping once. There are no regular expressions and no incremental search.
- Insert mode: type text; Enter inserts a line break; Tab inserts a literal tab; Backspace/Delete remove a grapheme or join lines. Arrow keys move. `Esc` returns to Normal mode.
- Paste is inserted as text only in Insert mode, never interpreted as commands.
- Command prompt: printable characters, Backspace, Enter to submit, Esc to cancel.
- `:w` writes the current file; `:w <path>` writes to that path, including paths with internal spaces, and adopts it only after success. Surrounding command whitespace is ignored. There is no shell quoting or escaping.
- `:q` quits only when the buffer matches its saved bytes. Otherwise the editor
  stays open with `Unsaved changes; use :w [path] before :q, or :q! to discard`.
- `:q!` quits and discards unsaved changes without writing. `:wq` saves the
  active path and then quits, and `:x` writes only when modified and then quits
  (a clean buffer quits without writing). `:wq`, `:x`, `:q`, and `:q!` take no
  path argument; a failed save-and-quit keeps the editor open with the buffer,
  active path, and unsaved changes intact.

`Esc` cancels an incomplete pending sequence, and a key that cannot continue a
pending command cancels it without touching the text. Other Ex commands and
other Normal-mode keys are rejected or ignored without side effects. Control and
Alt shortcuts other than `Ctrl-r` redo are ignored; none of them bypass the save
checks. [docs/keymaps.md](docs/keymaps.md) lists what is unsupported.

## Text and save behavior

The document is an in-memory UTF-8 string; extremely large files are not optimized. Cursor movement and deletion use extended graphemes. Tabs have fixed four-cell stops. Terminal emoji widths may vary. Content scrolls without wrapping, and control characters are displayed safely rather than emitted to the terminal.

Unedited saves preserve exact bytes: mixed LF/CRLF/lone-CR endings, empty lines, absent final newlines, and an initial UTF-8 BOM. The BOM is not shown or editable as text. Newly inserted/pasted line breaks use the first encountered line ending, or LF if none exists. Existing mixed endings are otherwise left alone.

A buffer counts as unsaved only while its serialized bytes differ from the bytes
it was opened with or last written successfully. Undoing back to that content
therefore makes it clean again, and a failed write never moves the saved bytes.
Undo and redo restore whole-document snapshots, so their memory cost grows with
file size and history length.

An explicit write may replace an existing regular destination. No parent directories are created. Existing symlinks are resolved to regular-file targets and preserved; readonly destinations are rejected. Saving writes a temporary file in the destination directory, flushes/syncs it, copies existing permissions, and atomically replaces the destination. Errors before replacement retain the original file, buffer, dirty state, and active path.

Atomic replacement is **not a guarantee of power-loss durability**. Ownership, ACLs, extended attributes, and hard-link identity are not promised. External concurrent modifications and TOCTOU races are not guarded beyond ordinary safe replacement; network filesystem semantics can differ. SIGKILL, abort, and power failure are outside terminal-cleanup guarantees.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
python3 tests/terminal_smoke.py target/debug/postvim
```

The Python runner requires Python 3 and macOS/Linux Unix PTYs; no third-party Python packages are needed. It builds and locates the separate lifecycle fixture using Cargo JSON and exercises normal exit, editing/saving, dirty-quit refusal, undo/redo, Visual selection with search, `:wq`, `:q!`, clean `:x`, resize, returned errors, and panics. The fixture is not part of the shipping editor. Rust tests cover document/application transitions, filesystem behavior, Ratatui TestBackend rendering, and the documented key inventory: `tests/documented_keymaps.rs` executes the examples in [docs/keymaps.md](docs/keymaps.md) and checks that the README, `--help`, and that page agree on the supported keys and commands. These checks do not claim verified Windows terminal behavior.

For a human-visible quickstart, run `cargo run`, inspect the welcome, press `i`, type accented/CJK/emoji text and tabs, resize the terminal, press Esc, try motions and `u`/`Ctrl-r`, try `v`/`V` and `.`, search with `/`, use `:w <path>` and then `:wq` (or `:q!` to discard). Check cursor shape/readability and verify saved bytes. Automated PTY checks do not replace this visual check.

## Welcome quotation

The replaceable welcome quotation is attributed to Gilles Deleuze and Félix Guattari, *A Thousand Plateaus*, translated by Brian Massumi: “Writing has nothing to do with signifying. It has to do with surveying, mapping, even realms that are yet to come.” Sources: [full text](https://theanarchistlibrary.org/library/gilles-deleuze-felix-guattari-a-thousand-plateaus) and [Columbia commentary](https://blogs.law.columbia.edu/nietzsche1313/daniela-gandorfer-deleuze-guatarris-a-thousand-plateaus-law-and-synesthesia/). No print page number or product/philosophical commitment is implied.
