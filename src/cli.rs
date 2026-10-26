use std::ffi::OsString;
use std::path::PathBuf;

/// The command-line actions supported by Postvim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    Help,
    Version,
    Edit(Option<PathBuf>),
}

/// Help text for the bounded single-file editor workflow.
pub const HELP: &str = "Postvim — a small modal text editor\n\
Usage: postvim [--help | --version] [--] [PATH]\n\
\n\
Open PATH, or show the welcome screen when no path is supplied. A missing PATH\n\
will not be created until it is saved with :w. Use -- before a path beginning\n\
with '-'.\n\
\n\
Normal keys: h/j/k/l move; w/W/b/B/e/E move by word; 0/^/$ go to the line\n\
start, first non-blank, and last character; gg/G go to the buffer start/end;\n\
f/F/t/T find a character, with ;/, repeating it. Counts work and multiply\n\
(2d3w). i/I/a/A insert; o/O open a line; x/X delete; r replaces; s/S\n\
substitute; D/C delete or change to the end of the line; d/c/y take a motion or\n\
a doubled key (dd/cc/yy); p/P put. u undoes, Ctrl-r redoes, . repeats the last\n\
change. v/V start Visual mode, / and ? search, n/N repeat a search, and : opens\n\
an Ex command.\n\
\n\
Insert keys: text, Enter, Tab, Backspace, Delete, and the arrow keys. Esc\n\
returns to Normal mode; paste is inserted as text only in Insert mode.\n\
\n\
Commands: :w saves; :w PATH saves as PATH (adopted only after success); :q\n\
quits when the buffer matches its saved bytes; :q! quits and discards changes;\n\
:wq saves the active path and then quits; :x writes only when modified and then\n\
quits. :wq, :x, :q, and :q! take no path argument. A modified buffer refuses :q\n\
and reports how to save or discard it.\n\
\n\
This is a bounded Vim subset, not Vim compatibility. docs/keymaps.md lists the\n\
complete key inventory and the unsupported commands.\n";

/// Parse process arguments after the executable name.
///
/// Paths remain OS-native so non-UTF-8 filenames are preserved where supported.
pub fn parse_args_os<I>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(CliAction::Edit(None));
    };

    match first.to_str() {
        Some("--help") => {
            if args.next().is_some() {
                Err("--help must not be combined with other arguments".to_owned())
            } else {
                Ok(CliAction::Help)
            }
        }
        Some("--version") => {
            if args.next().is_some() {
                Err("--version must not be combined with other arguments".to_owned())
            } else {
                Ok(CliAction::Version)
            }
        }
        Some("--") => parse_path(args.next(), args.next()),
        Some(value) if value.starts_with('-') => Err(format!(
            "unknown option: {value}; use -- before a path beginning with '-'"
        )),
        _ => parse_path(Some(first), args.next()),
    }
}

fn parse_path(path: Option<OsString>, extra: Option<OsString>) -> Result<CliAction, String> {
    if extra.is_some() {
        return Err("expected at most one path".to_owned());
    }

    Ok(CliAction::Edit(path.map(PathBuf::from)))
}
