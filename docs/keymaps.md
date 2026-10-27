# Postvim keymap

Postvim implements a deliberately small, everyday slice of Vim's modal editing.
This page is the compatibility contract: it lists exactly the keys that work,
the choices Postvim makes where Vim's rules are ambiguous, and the features
that are intentionally absent. It is not a claim of Vim compatibility.

Semantics Postvim follows, for reference (Vim's own documentation):

- [word motions](https://vimhelp.org/motion.txt.html#word-motions), [exclusive motions](https://vimhelp.org/motion.txt.html#exclusive), [motion counts multiply](https://vimhelp.org/motion.txt.html#motion-count-multiplied)
- [delete and insert](https://vimhelp.org/change.txt.html#delete-insert)
- [Visual mode](https://vimhelp.org/visual.txt.html#v), [put from Visual mode](https://vimhelp.org/change.txt.html#put-Visual-mode)
- [undo blocks](https://vimhelp.org/undo.txt.html#undo-blocks), [single repeat](https://vimhelp.org/repeat.txt.html#single-repeat)
- [search commands](https://vimhelp.org/pattern.txt.html#search-commands)
- [write and quit](https://vimhelp.org/editing.txt.html#write-quit)

## Supported keys

The block below is the complete key inventory. Every entry is implemented and
no other key is. `Ctrl-r` means Control+R.

```text postvim-inventory
{count} | optional count prefix on most commands; a leading 1-9 starts it and 0 is then a digit
h j k l | move by grapheme or line; h/l stay on the line and j/k keep the preferred column
w W b B e E | move by word or WORD; blank lines are word stops and e/E skip empty lines
0 ^ $ | first column, first non-blank column, last grapheme of the line
gg G | first or numbered line; last or numbered line, at its first non-blank
f F t T ; , | find or till a character on the line, repeat it, and reverse it
i I a A | insert before the cursor, at the first non-blank, after the cursor, at the end of the line
o O | open a new line below or above the current line
x X | delete graphemes after or before the cursor, without consuming a line break
r s S | replace graphemes in place; substitute the cursor grapheme; substitute the whole line
D C | delete or change to the end of the line
d c y | operator prefix taking a motion, or a doubled key for whole lines
dd cc yy | whole-line delete, change, and yank
p P | put after the cursor or below the line; put before the cursor or above the line
u | undo one completed change; counts undo that many
Ctrl-r | redo one undone change; counts redo that many
. | repeat the last change, reading a new count when one is supplied
v V | Visual mode by character or by line
/ ? | open the forward or backward search prompt
n N | go to the next or previous match of the last search
: | open the Ex command prompt
Enter Tab Backspace Delete | Insert-mode line break, literal tab, and grapheme deletion
Left Right Up Down Esc | Insert-mode arrow movement, and return to Normal mode
```

Counts multiply where Vim multiplies them: `2d3w` deletes six words and `2y3y`
yanks six lines. Counts saturate at 1,000,000; traversal and deletion always
clamp to the available text, so a huge count never allocates or overflows.

Pending input is cancelled, never reinterpreted: `Esc` clears an incomplete
prefix, and a key that cannot continue one cancels it without touching the
text (so `dq` in Normal mode does nothing at all).

## Ex commands

The grammar is bounded: `:w`, `:w <path>`, `:q`, `:q!`, `:wq`, `:x`,
`:format <program> [<arg> ...]`, `:reload`, and `:reload!`. Surrounding
whitespace is allowed. In the block below `PATH` and `PROGRAM [ARG...]` are the
placeholders for those arguments; everything else is literal.

```text postvim-ex-commands
:w
:w PATH
:q
:q!
:wq
:x
:format PROGRAM [ARG...]
:reload
:reload!
```

- `:w` writes the active path; `:w <path>` writes there and adopts that path
  only after the write succeeds. Paths are literal text: no shell quoting,
  escaping, or globbing, and parent directories are never created.
- `:q` quits only when the buffer matches its saved bytes. Otherwise it keeps
  the editor open with `Unsaved changes; use :w [path] before :q, or :q! to discard`.
- `:q!` quits and discards unsaved changes without writing.
- `:wq` always writes the active path and quits only after that write succeeds.
  An unnamed buffer or a failed write stays open with the buffer, path, and
  dirty state intact.
- `:x` writes only when the buffer is modified, then quits. A clean buffer quits
  without writing, and no file is created for a clean unnamed buffer.
- `:format <program> [<arg> ...]` runs that program on the Visual selection or
  the whole buffer and applies its output as one undoable change without
  writing the file. The arguments are split with shell-style quoting; no shell
  runs, so operators and variables stay literal text.
- `:reload` replaces a clean buffer with its file's current contents. `:reload!`
  discards local text for those contents. Both are one undoable change, take no
  argument, and refuse an unnamed buffer, a missing file, or a file that is not
  readable regular UTF-8.
- Any other command, an empty command, and any of `:wq <path>`, `:x <path>`,
  `:reload <path>`, `:format` with an empty program or unmatched quotes, `:w!`,
  or `:q!!` are rejected without side effects.

## Formatting

`:format` runs one existing program that you name; Postvim bundles no formatter,
guesses none, and formats nothing automatically. The documented real example is
rustfmt, which must be on `PATH`:

```sh
# In a terminal reading from standard input:
printf 'fn main(){println!("hi");}\n' | rustfmt --emit stdout --edition 2024 --color never
```

Inside Postvim you type `:format rustfmt --emit stdout --edition 2024 --color never`.
The flags are explicit: `--emit stdout` makes rustfmt read standard input and
print the result, `--edition 2024` matches the edition Postvim is built with, and
`--color never` keeps ANSI escapes out of the text. Without those flags rustfmt
may rewrite files or refuse to format, so they are part of the documented
invocation rather than optional decoration. A Visual fragment is often not
valid Rust, and a program that rejects its input is reported as a failure.

- Input is the Visual selection when the prompt was opened from Visual mode, and
  the whole buffer otherwise. A characterwise selection includes its last
  grapheme; a linewise selection includes the selected line endings where the
  file has them, and a last line without a final newline stays exact.
- The program receives only the selected text on standard input. Its output
  replaces exactly that slice, so text outside the selection keeps its bytes:
  mixed line endings, an absent final newline, and an initial UTF-8 BOM survive
  untouched. The formatted slice itself is whatever the program emitted; there is
  no trimming, newline repair, or re-indentation.
- Success is one undoable change and never a write: the file on disk, the saved
  baseline, and the dirty state are only what the edit itself implies. An output
  identical to the input adds no undo step and clears nothing. The yank register
  is untouched, and the cursor returns to the start of the changed range clamped
  to a grapheme boundary, or to its approximate old position for a whole-buffer
  run.
- `.` never re-runs an external program, so a stale repeat is discarded by a
  successful format.
- A failure — missing program, nonzero exit, output that is not UTF-8, or an
  invalid range — keeps the text, cursor, mode, path, dirty state, undo history,
  and register exactly as they were, and shows the program's diagnostics. A
  program that succeeds but writes to standard error keeps its warnings visible
  in the message line; warnings never become text in the buffer.
- The program is trusted and synchronous. Postvim runs it directly, with no
  implicit shell and no sandbox. Argument text typed after `:format` is split
  with shell-style quoting, so quotes group one argument and a backslash escapes
  the next character, but nothing is expanded: `$HOME`, `|`, `>`, `*`, and `;`
  are ordinary characters. The program may modify files, use the network, or run
  for as long as it likes; those side effects are outside Postvim's control and
  cannot be undone by undo.
- There is no timeout, no cancellation, no memory limit, and no output limit. The
  editor accepts no input while the program runs and buffers its output in
  memory, and a program that leaves children holding its pipes can delay
  completion indefinitely. Use bounded, finite, trusted
  standard-input/standard-output tools.

## External changes and recovery

A buffered file can change while Postvim is open, for example because an agent
or an independent `postvim --format ... --write` run replaced it.

- While a buffer has a path, Postvim re-reads that file about twice a second
  (every 500 ms) and confirms a change only after two equivalent observations at
  least one interval apart. This delay is a heuristic that avoids displaying a
  brief truncated-then-written file; it is not a complete-write guarantee, and no
  universal end-of-write signal exists.
- New readable regular UTF-8 contents are installed automatically only while the
  buffer is clean and in idle Normal mode: no pending count or operator, no
  Visual selection or open prompt, no Insert session. Otherwise reconciliation is
  deferred and retried at the next safe boundary; a cached observation is never
  applied without re-checking the current file.
- A persistent status field reports an external change, a deferred change, or an
  unreadable file, and it stays visible until the situation is resolved; it is
  separate from the transient message line.
- An automatic or explicit reload is one undoable change: the reloaded bytes
  become the saved baseline and the expected on-disk contents, undo and redo
  still step through older changes, and undo never writes the file. Repeating a
  check that finds identical contents adds no history entry and clears no redo.
- Deletion, invalid UTF-8, a nonregular replacement, an unreadable destination, or
  a dangling link keeps the last good buffer and baseline even when the buffer is
  clean, and reports the problem instead of showing an empty buffer. A named file
  that was missing and has appeared with valid text is a real change and may
  reload.
- A dirty buffer keeps its text, cursor, mode, selection, Insert session, undo
  history, register, and saved baseline. The status field reports the conflict
  and the ways out: `:reload` after discarding local edits, `:reload!` to discard
  them explicitly, or a save elsewhere with `:w <other-path>`.
- Every route that writes the active file — `:w`, `:wq`, `:x` on a modified
  buffer, and `:w <path>` when the path resolves to the same file — first
  compares the destination against the contents Postvim last read or wrote, and
  compares again immediately before the atomic replacement. A difference refuses
  the write and leaves the foreign bytes, the buffer, the path, and the dirty
  state intact. Lexical aliases (`./notes.txt`), absolute paths, and symlinks
  pointing at the same target do not bypass that check.
- There is no force-overwrite flag and no merge: `:w!` stays unsupported, and
  recovery is an explicit reload, an explicit discard, or a save elsewhere. A
  save to a genuinely different destination keeps its documented behavior of
  replacing that destination; it is a save-as, not a claim that the old file was
  unchanged.
- These checks are optimistic: they reduce lost updates but are not file locking
  or an atomic compare-and-swap, and an arbitrary writer can still change the
  destination between the last comparison and the rename. Ownership, ACLs,
  extended attributes, hard-link identity, power-loss durability, and network
  filesystem consistency are not promised.
- Never redirect the editor onto the file it is editing. The shell truncates
  `postvim notes.txt > notes.txt` before Postvim reads it, so the buffer is empty
  before polling or save checks can help. Have agents and tools replace files
  atomically, or write a different path.

## Deliberately unsupported

Nothing outside the inventory above is implemented, including these keys and
commands people often expect from Vim:

```text postvim-unsupported
di da ci ca yi ya | text objects
Ctrl-v | Visual block mode
" | named, numbered, system, and clipboard registers
@ | macros and macro execution
q | Normal-mode macro recording
m | marks
R | replace mode
Y | yank to the end of the line
J | joining lines
~ | case swapping
ZZ ZQ | save-and-quit and quit-without-saving
gv | reselecting the previous Visual selection
:s :%s | Ex substitution
:! | Ex shell commands
:1,$d | Ex ranges and line numbers
:wq<path> :x<path> :w! :q!! | rejected command grammar
:format! :reload<path> | rejected command grammar
```

There is also no autoindent, no multiple buffers, no configuration, no plugin
host, no bundled or auto-detected formatter, no format-on-save, no asynchronous
formatting job or cancellation, no automatic merge, and no Windows terminal
claim.

## Interpretation rules and deviations

These are Postvim's explicit choices. Each one resolves an ambiguity rather
than copying Vim exactly:

- **Unnamed register only.** Deletes, changes, yanks, and Visual puts use one
  internal unnamed register that records characterwise text or ordered
  linewise contents with their original internal line endings. There are no
  named, numbered, system, or clipboard registers.
- **Put cursor rules.** A characterwise put of one line leaves the cursor on
  the last inserted grapheme; a multi-line characterwise put leaves it on the
  first inserted grapheme; a linewise put leaves it on the first non-blank of
  the first inserted line.
- **Snapshot history.** Undo and redo restore whole-document snapshots with the
  cursor, so memory use grows with file size times history length. There is no
  undo tree, no persistence, and no configurable limit.
- **Byte-exact saved baseline.** Dirty means the serialized bytes differ from
  the bytes last opened or last written successfully. Undoing back to those
  bytes makes the buffer clean again, and a failed write never moves the
  baseline. A reload moves it to the bytes just read, and `:format` never moves
  it because formatting does not write.
- **Repeat is semantic.** `.` re-runs the last change at the current cursor
  rather than replaying recorded keys at an absolute position, and a Visual
  change repeats the selection's shape (line count, or inclusive grapheme
  count) from the current cursor. Counts on a Visual repeat are ignored, and
  Visual `p` is not repeatable because it would reuse a swapped register.
- **Search is literal and line-scoped.** The query is case-sensitive literal
  text matched inside one line, with no regular expressions, no escapes, and no
  delimiter syntax. Matches must start and end on grapheme boundaries, so
  combining marks and emoji are never split. There is no incremental search,
  no mandatory highlight-all, and no search history.
- **`$` beyond the end.** A count that runs past the last line clamps to the
  last line instead of reporting Vim's error.
- **`cw` and `dw` exceptions.** `cw` on a non-blank behaves as `ce`, keeping the
  following blanks; `dw` on the last word of a line stops at that line end.
- **Atomic `r`.** `r` replaces the counted graphemes or changes nothing at all;
  an insufficient count is reported and the text is untouched.
- **Failed finds change nothing.** A failed `f`/`F`/`t`/`T` leaves the cursor,
  text, register, and history alone and reports it in the message line.
- **No virtual end-of-line cell.** In Visual mode `$` selects through the last
  grapheme of the line rather than a column past the text.
- **Visual entry counts.** A count before `v`/`V` selects that many graphemes or
  lines. Reselecting a previous selection with a count or `gv` is not supported.
- **Reversed selections normalize.** A backwards selection is recorded forward
  before it is applied or remembered for `.`.
- **External changes are observations, not locks.** An external edit is detected
  by comparing bytes at a resolved path, not by watching filesystem objects or
  holding them. Identical bytes count as unchanged even when the file was
  replaced, and a foreign edit reverted to the expected contents clears the
  conflict. Seeing bytes that happen to equal local unsaved edits does not mark
  the buffer saved.
- **Formatting is explicit and trusted.** `:format` starts exactly the program
  you named with the arguments you typed and no shell in between. Postvim adds no
  filename, reads no project configuration, and cannot bound the program's time,
  memory, or side effects. It also never runs a formatter on its own.

## Newly inserted line breaks

Text typed or pasted in Insert mode uses the document's existing insertion
ending: the first line ending found in the file, or LF when the file has none.
Mixed existing endings, absent final newlines, and a leading UTF-8 BOM are
preserved byte-for-byte, and undo/redo restores that metadata exactly.

## Tested examples

The table below is executed by `tests/documented_keymaps.rs`, so these examples
cannot drift from the implementation. Fields are separated by ` | `:
starting buffer, keys, resulting buffer, resulting mode, and the resulting
cursor as `row:byte-offset`. Buffers are quoted with `\n` for a line feed.
Keys are literal characters plus the tokens `<Esc>`, `<CR>`, `<Tab>`, and
`<BS>`. Redo uses the terminal key Ctrl-r and is covered by the PTY harness
instead of this table.

```text postvim-examples
"" | ihello<Esc> | "hello" | normal | 0:4
"abc\n" | xu | "abc\n" | normal | 0:0
"one two three four\n" | 2dw | "three four\n" | normal | 0:0
"alpha beta\n" | cwABC<Esc> | "ABC beta\n" | normal | 0:2
"a\nb\n" | ddp | "b\na\n" | normal | 1:0
"abcdef\n" | vlld | "def\n" | normal | 0:0
"one\ntwo\n" | Vjd | "" | normal | 0:0
"foo bar foo\n" | /foo<CR> | "foo bar foo\n" | normal | 0:8
"a\nb\nc" | G | "a\nb\nc" | normal | 2:0
"héllo\n" | lx | "hllo\n" | normal | 0:1
"a👩‍👩‍👦b\n" | lx | "ab\n" | normal | 0:1
"abc\n" | dq | "abc\n" | normal | 0:0
```

A typical session reaches the same result from the terminal:

```sh
printf 'alpha beta\n' > notes.txt
cargo run -- notes.txt   # then: cworld<Esc>, u, then :wq
```

## Verification

```sh
cargo test --test documented_keymaps
cargo test --test documented_transformations
python3 tests/terminal_smoke.py target/debug/postvim
```

`cargo test` runs the inventory, help, and example checks above plus the editor,
render, and filesystem suites. The transformation examples in
[docs/transformations.md](transformations.md) are executed by
`tests/documented_transformations.rs`, so the command grammar and the recovery
advice cannot drift either. The PTY harness drives real keystrokes, including CR
for Enter and Ctrl-r for redo, and asserts the exact bytes on disk.
