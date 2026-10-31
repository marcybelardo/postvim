use std::io::{self, Write};
use std::process::ExitCode;

use postvim::cli::{self, CliAction, FormatStreams};
use postvim::editor::Editor;
use postvim::terminal::with_terminal;
use postvim::ui;

fn main() -> ExitCode {
    // Context owns a separate diagnostic boundary: its raw errors are escaped
    // once, while existing editor and formatter diagnostics retain their style.
    let context = std::env::args_os().nth(1).is_some_and(|arg| arg == "--context");
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let message = if context {
                postvim::context::escape(&message)
            } else {
                sanitize(&message)
            };
            let _ = writeln!(io::stderr().lock(), "postvim: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let action = cli::parse_args_os(std::env::args_os().skip(1))?;
    match action {
        CliAction::Help => {
            print!("{}", cli::HELP);
            Ok(())
        }
        CliAction::Version => {
            println!("postvim {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CliAction::Context(invocation) => {
            postvim::context::run_context(&invocation, &mut io::stdout().lock())
        }
        CliAction::Edit(path) => {
            let mut editor = Editor::new(path.clone()).map_err(|error| match path {
                Some(path) => format!("{}: {error}", path.to_string_lossy()),
                None => error.to_string(),
            })?;
            with_terminal(|terminal| ui::run(terminal, &mut editor))
                .map_err(|error| error.to_string())
        }
        CliAction::Format(invocation) => {
            // Format mode is independent of the editor and the terminal: no
            // terminal setup, no TTY requirement, and no document file is
            // touched unless --write requested it.
            let working_directory = std::env::current_dir()
                .map_err(|error| format!("cannot determine the working directory: {error}"))?;
            let mut stdin = io::stdin().lock();
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            cli::run_format(
                &invocation,
                &working_directory,
                FormatStreams {
                    stdin: &mut stdin,
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
            )
            .map_err(|error| error.to_string())
        }
    }
}

fn sanitize(text: &str) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        if ch.is_control() {
            let value = ch as u32;
            if value < 0x20 {
                result.push('^');
                result.push(char::from_u32(value + 0x40).unwrap_or('?'));
            } else if value == 0x7f {
                result.push_str("^?");
            } else {
                result.push('�');
            }
        } else {
            result.push(ch);
        }
    }
    result
}
