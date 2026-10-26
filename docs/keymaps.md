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

The grammar is bounded: `:w`, `:w <path>`, `:q`, `:q!`, `:wq`, `:x`.
Surrounding whitespace is allowed and only `:w` takes a path. In the block
below `PATH` is the placeholder for that argument; everything else is literal.

```text postvim-ex-commands
:w
:w PATH
:q
:q!
:wq
:x
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
- Any other command, an empty command, and any of `:wq <path>`, `:x <path>`,
  `:w!`, or `:q!!` are rejected without side effects.

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
```

There is also no autoindent, no multiple buffers, no configuration, no plugin
host, and no Windows terminal claim.

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
  baseline.
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
python3 tests/terminal_smoke.py target/debug/postvim
```

`cargo test` runs the inventory, help, and example checks above plus the editor,
render, and filesystem suites. The PTY harness drives real keystrokes, including
CR for Enter and Ctrl-r for redo, and asserts the exact bytes on disk.
