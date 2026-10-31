# Saved-file context

`postvim --context` prints a plain-text report around a selected range of lines in
an existing file. It is a separate, read-only command; it does not open the
editor or connect to a running editor.

```text
postvim --context --lines START:END [--surround N] -- PATH
```

`--context` must be the first argument. Both `--lines START:END` and `--` are
required. `--lines` and `--surround N` may appear in either order before `--`.
Pass exactly one nonempty native filesystem path after `--`; `-` is an ordinary
filename, not standard input. No terminal is required.

`START:END` selects 1-based, inclusive line numbers. This is not the formatter's
zero-based, half-open byte range. `--surround N` requests up to N lines of
context on each side, defaults to 3, and accepts values from 0 through 20.

The command reads the file's currently readable, saved contents from disk. It
cannot see unsaved editor changes, highlights, or live editor state; save first
if the context should include unsaved changes. It does not consult a previous
saved version retained by an editor, and it cannot automatically refuse to run
when an editor has unsaved changes. This is a read-only report, not an editing,
saving, or insertion interface. It does not provide a plugin system or a shared
editor session; a person, agent, or plugin can invoke the same process command
using its own process-launch mechanism.

The report preserves Unicode text and escapes backslashes as `\\`, tabs as
`\t`, and other control characters using Rust's `escape_default` representation.
The displayed path uses the native path for file access and may be lossy when
converted for display. Report rows are separated with LF; line endings in the
file are structural input, so the report is not a lossless export or insertion
payload.

Missing, unreadable, non-regular, or invalid-UTF-8 files, invalid line bounds,
and invalid options produce a nonzero exit status and diagnostics on standard
error without report output on standard output. If writing the report itself to
standard output fails, some output may already have been written. The command
observes one file snapshot; it does not lock the file or promise consistency if
another process changes it while being read. Like the editor, it reads the whole
file into memory, so extremely large files are not optimized.

## Example

Create this fixture as `notes.txt` (the last line is followed by a newline):

```text
alpha
βeta
gamma
delta
omega
```

That final newline means the file has six logical lines, the sixth empty. Run:

```sh
postvim --context --lines 2:3 --surround 1 -- notes.txt
```

The report has no blank lines between its entries:

```text
File: notes.txt
Source: saved file on disk (not live editor state)
Lines: 2:3 of 6
Surrounding lines: 1
Context before:
1 | alpha
Selected text:
2 | βeta
3 | gamma
Context after:
4 | delta
```
