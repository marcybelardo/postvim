# Text transformations

Postvim can run one existing standard-input/standard-output program over part of
a document and apply its output as text. The same operation is available three
ways: the interactive editor (`:format`), an independent command line
(`postvim --format`), and the Rust library. Postvim bundles no formatter, guesses
none, and formats nothing automatically; you name the program and its arguments.

The three clients share the operation, not the session. The library does not
read or write the editor's buffer, the command line never needs a running editor,
and the editor never observes an independent run. There is no daemon, there is
no shared unsaved state, and there is no implicit file protocol between them:
they exchange text and diagnostics, and each caller decides what to do with the
result.

## The command line

```
postvim --format [--input PATH] [--range START:END] [--write] [--stdout] -- PROGRAM [ARG...]
```

`--format` is recognized only as the first argument. `postvim format` still opens
a file named `format`, and the existing `postvim [--] [path]` grammar is
unchanged.

- `--input PATH` reads that path, which must exist and be a regular UTF-8 file.
  This differs from interactive editing, where a missing path opens an empty
  named buffer. Without `--input`, the document is read from standard input
  through end of file, and `--input -` means a file literally named `-`, not a
  stream alias.
- `--range START:END` formats only that part of the text. Offsets are decimal
  zero-based **bytes**, not characters or screen columns, and `END` is exclusive.
  They count the editable text, so an initial UTF-8 BOM is not part of the range
  and never shifts an offset. The default is the whole document. A range that is
  reversed, out of bounds, splits a grapheme, or splits a CRLF pair is rejected
  before the formatter runs.
- Standard output is the **full transformed document**, including the original
  BOM and every byte outside the range, not just the replacement. Preview,
  pipeline, and write modes therefore share one artifact.
- Nothing is written by default. `--write` requires `--input` and atomically
  replaces that file, and then prints no document. `--write --stdout` saves first
  and then prints the same full document. `--stdout` alone is accepted as the
  explicit spelling of the default. Writing standard input back to a file is not
  supported; there is no destination guessing.
- Standard output carries document bytes and nothing else: no success message, no
  progress, and no injected escape sequences. Formatter warnings and failures go
  to standard error, escaped so that control characters cannot drive the
  terminal.
- No terminal is needed. The command runs without initializing the UI, without
  escape sequences, and without requiring a TTY on either stream.
- Exit status is zero on success and nonzero for usage errors, formatter
  failures, invalid range or output, and refused writes. A failure emits no
  document on standard output and performs no write. If `--write --stdout` saved
  the file and then failed to write standard output, the error says the save
  already happened; nothing is rolled back.

### Arguments are literal

Everything after `--` is the program and its arguments, passed as native
operating-system strings. Postvim never inserts a shell, so `|`, `>`, `$HOME`,
`*`, `;`, and backslashes are ordinary characters. Your own shell still expands
them before Postvim starts, exactly as it would for any other command; quote them
when you want them literal.

```sh
# The shell expands nothing here; rustfmt sees four literal arguments.
postvim --format --input notes.rs -- rustfmt --emit stdout --edition 2024 --color never

# Ask the shell for a literal dollar sign:
postvim --format -- my-formatter '$HOME' '|'
```

The same rule applies to `--input`: it takes one path, including a path that
begins with `-`, and Postvim performs no globbing.

### The real example

rustfmt is the documented real formatter; it must be on `PATH`. The flags are
explicit and each one matters:

- `--emit stdout` reads standard input and writes the formatted text to standard
  output instead of rewriting files.
- `--edition 2024` matches the edition Postvim is built with.
- `--color never` keeps ANSI escapes out of the text that becomes document bytes.

```sh
printf 'fn main(){println!("hi");}\n' > /tmp/sample.rs
postvim --format --input /tmp/sample.rs -- rustfmt --emit stdout --edition 2024 --color never
# The file itself is still unchanged:
cmp /tmp/sample.rs <(printf 'fn main(){println!("hi");}\n')
```

A short fragment or an incomplete expression is often invalid Rust. rustfmt then
exits with an error, Postvim reports it on standard error, and no output is
printed.

## The interactive command

`:format PROGRAM [ARG...]` runs the named program on the Visual selection or, when
the prompt was not opened from Visual mode, on the whole buffer.

- The command text is split with shell-style quoting, so `"notes file"` is one
  argument and `\$` keeps a literal dollar sign, but nothing is expanded and no
  shell operator is interpreted. `:w <path>` is unchanged: it takes the rest of
  the line verbatim, with no quoting at all.
- The program receives only the selected text and its output replaces exactly that
  slice. Untouched text keeps its bytes: mixed line endings, an absent final
  newline, and the initial BOM survive. The formatted slice is verbatim program
  output, with no trimming or newline repair.
- A successful run is one undoable change, clears redo for a real change, and
  never writes the file or moves the saved baseline. Output identical to the
  input adds no undo step and clears nothing. `.` never re-runs an external
  program.
- A failure keeps the text, cursor, mode, path, dirty state, history, and register
  intact and shows escaped diagnostics. Warnings from a successful run stay
  visible in the message line without entering the buffer.

See [the key reference](keymaps.md) for selection details and
[external changes and recovery](keymaps.md#external-changes-and-recovery) for
polling, reload, and conflict behavior.

## The library contract

The library exposes one operation: a typed request describing the text, a
half-open byte range, the program, its arguments, and the working directory; a
typed result carrying the replacement text plus diagnostic bytes and exit status;
and a pure apply step. The runner and the apply step never touch the document
file, the editor state, or the terminal.

- Program and arguments are native strings, so non-UTF-8 program names and
  arguments pass through unchanged.
- The request's range is validated by the same rules the command line uses:
  ordered, in bounds, on extended grapheme boundaries, never splitting a CRLF
  pair, and allowed to be empty or to end at the document end.
- The result records the source text and range it was produced from. Applying
  checks the current editable text and BOM flag against that source and rejects a
  mismatch as stale, even when the selected fragment happens to look identical.
- Applying splices the replacement verbatim into the source, keeping all bytes
  outside the range and the original BOM. Empty output is a valid deletion. A
  leading U+FEFF in formatter output is ordinary editable text, even at offset
  zero; it does not become a file BOM.
- Errors are structured and distinguish an invalid range, a spawn or I/O failure,
  a nonzero exit with diagnostics, output that is not UTF-8, and a stale source.
- A result requires complete delivery of the input and of the captured streams. A
  program that exits before reading all of its standard input therefore fails as
  a premature close of the formatter's input, instead of being reported as an
  empty replacement. An empty-output tool must be one that drains what it is
  given: `sed -n ''`, not `true`.
- Diagnostic bytes from standard error are preserved raw for library callers, who
  may log them, parse them, or show them differently.

Because the request carries text rather than a live buffer, callers decide how a
result reaches a particular document, and a caller that has moved on can detect
that its result is stale instead of clobbering newer edits.

## Design direction (not yet implemented)

The long-term direction is for Postvim to be agnostic to an operation's task.
Formatting is the first adapter, not the permanent conceptual category. An
operation would request text or explicitly supported context; Postvim would
supply it and receive an edit, information, or failure. Postvim would own
document/context access, stale-source validation, edit application, undo,
presentation, and explicit persistence. The operation would own task-specific
meaning. Context provision must be explicit, not unrestricted ambient access;
this boundary would not sandbox a trusted external program. Independent CLI,
library, and editor clients do not share unsaved session state. The current
formatter API and commands remain as documented above, and the current trust
and synchronous-process limits remain unchanged.

The recommended next bounded experiment—not an approved feature implementation
plan—is to run an explicitly selected stdin/stdout tool on a whole document or
selection in an information-only mode, using `wc -w` as a concrete example. Its
successful output would be presented as information, not replacement text. The
buffer, dirty baseline, undo/redo history, and file would remain unchanged by
Postvim. This would reuse direct execution and sanitized presentation, not add
word-count-specific logic; a trusted program's own side effects would still be
outside Postvim's control.

A caller-declared result intent would be enough for that experiment. The useful
result distinction is edit versus information, not format versus explain, count,
or translate. No new command or API spelling is specified here, nor dynamic
context negotiation, a plugin manifest, registry, discovery, loading, panels,
asynchronous scheduling, or a general protocol. Today's tools do not negotiate
context or return typed information results through the formatter API.

## Diagnostics for people

The editor and the command line present formatter diagnostics the same way: they
decode standard error lossily, escape terminal control characters, and cap the
summary at 2,048 Unicode scalar values after escaping, without splitting a
UTF-8 sequence, appending an explicit truncation marker when they cut. A useful
status or error category stays in front of the summary. The cap keeps one
misbehaving program from flooding a terminal line; it is not a limit on the
formatter's output on standard output, which becomes text.

## Trust and limits

A transformation runs an arbitrary program that you chose, synchronously, in
Postvim's own process tree. That is deliberate and bounded by documentation
rather than by enforcement:

- There is no sandbox, no privilege drop, and no filesystem or network isolation.
  A formatter can edit files, read secrets, and talk to the network. Postvim
  cannot undo those effects, and undo only covers text.
- There is no time limit, no memory limit, and no output limit. While it runs, the
  editor accepts no input. A formatter that never finishes, or that leaves
  children holding its pipes open, delays completion indefinitely.
- Postvim does wait for and reap the direct child it started, and drains all three
  streams concurrently, so ordinary large input and output does not deadlock.
  Distinguish that from arbitrary descendants, which are outside any process-tree
  guarantee.
- Use finite, trusted standard-input/standard-output tools. Format-on-save,
  formatter autodetection, asynchronous jobs, and cancellation are not
  implemented.

## Never redirect onto the input file

```sh
postvim notes.txt > notes.txt    # wrong: notes.txt is empty before Postvim runs
```

The shell truncates the destination before Postvim starts, so the editor opens an
empty file and there is nothing left to protect. Write a different path, or use
`postvim --format --input notes.txt --write -- ...` when you want a guarded
in-place replacement. `--write` compares the file against the bytes it read and
refuses if another process changed it in the meantime; that check is optimistic
and not a lock.

## Executed examples

Each line below is a command line followed by the interpretation
`tests/documented_transformations.rs` requires it to have, so the grammar in this
page cannot drift from the parser. The command column is tokenized with shell
quoting (quotes are the shell's, not Postvim's) and the field column records the
expected input path (or `stdin`), byte range (or `none`), write and standard
output behavior, program, and arguments.

```text postvim-cli-examples
postvim --format -- rustfmt --emit stdout --edition 2024 --color never | input=stdin; range=none; write=no; stdout=yes; program=rustfmt; args=--emit stdout --edition 2024 --color never
postvim --format --input notes.rs --range 0:11 -- rustfmt --emit stdout --edition 2024 --color never | input=notes.rs; range=0:11; write=no; stdout=yes; program=rustfmt; args=--emit stdout --edition 2024 --color never
postvim --format --input notes.rs --write -- rustfmt --emit stdout --edition 2024 --color never | input=notes.rs; range=none; write=yes; stdout=no; program=rustfmt; args=--emit stdout --edition 2024 --color never
postvim --format --input notes.rs --write --stdout -- rustfmt --emit stdout --edition 2024 --color never | input=notes.rs; range=none; write=yes; stdout=yes; program=rustfmt; args=--emit stdout --edition 2024 --color never
postvim --format --stdout -- rustfmt --emit stdout --edition 2024 --color never | input=stdin; range=none; write=no; stdout=yes; program=rustfmt; args=--emit stdout --edition 2024 --color never
postvim --format -- my-formatter '|' '$HOME' '>' | input=stdin; range=none; write=no; stdout=yes; program=my-formatter; args=| $HOME >
```

The rejected forms stay rejected: `--write` without `--input`, a range that is
reversed or malformed, a missing `--` before the program, an empty program, and
duplicate or unknown options.

## Executed transformation examples

The programs below are the POSIX `cat`, which copies its input, and `sed -n ''`,
which reads all of standard input and writes nothing. Both are installed where
these tests run, and both *drain* the text Postvim sends them. That matters: a
program that exits without reading its input (`true`, for instance) can make the
run fail with `the formatter closed its standard input before receiving the whole
selection`. That is a legitimate formatter failure, not a deterministic empty
replacement, so an empty-output tool must consume what it is given. This page
therefore never uses `true`.

The program column is a whole command line, tokenized with the same shell-word
quoting an interactive `:format` uses, so `sed -n ''` means the program `sed`
with two arguments: `-n` and an empty script. Postvim still inserts no shell:
if you want one you have to name it yourself, for example
`:format sh -c 'cat >/dev/null'`, and that is your explicit choice rather than
something Postvim does on its own.

Fields are separated by ` | `: editable starting text, half-open byte range,
formatter command line, BOM flag, and the editable text after the result is
applied. Buffers are quoted with `\n`, `\r`, `\t`, `\\`, and `\"` escapes. `cat`
shows that text outside the range, the line endings, and the BOM survive;
`sed -n ''` shows that empty output is a valid deletion of exactly the selected
bytes.

```text postvim-transform-examples
"alpha beta\n" | 0:5 | cat | bom=no | "alpha beta\n"
"alpha beta\n" | 0:5 | sed -n '' | bom=no | " beta\n"
"a\r\nb\r\n" | 0:3 | sed -n '' | bom=no | "b\r\n"
"αβ\n" | 0:2 | cat | bom=no | "αβ\n"
"alpha\n" | 0:5 | cat | bom=yes | "alpha\n"
"a👩‍👩‍👦b\n" | 0:1 | sed -n '' | bom=no | "👩‍👩‍👦b\n"
"abc" | 0:3 | sed -n '' | bom=no | ""
```

`tests/documented_transformations.rs` runs each row through the library request,
execute, and apply steps and also checks the documented `:format`, `:reload`,
and `--format` behavior it can reach without a formatter installation. The
editor-side example is `:format sed -n ''` on a whole buffer: the text becomes
empty as one undo step that `u` restores, without writing the file.

## Verification

```sh
cargo test --test documented_transformations
# A real rustfmt run, when rustfmt is on PATH:
printf 'fn main(){println!("hi");}\n' > /tmp/sample.rs
postvim --format --input /tmp/sample.rs -- rustfmt --emit stdout --edition 2024 --color never
```

The documented examples are executed by `tests/documented_transformations.rs`;
the rustfmt smoke test needs the real tool and is part of the development-shell
verification, not of ordinary `cargo test`.
