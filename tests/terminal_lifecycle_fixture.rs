use std::io::{self, Write};

use postvim::terminal::with_terminal;
use ratatui::crossterm::cursor::Hide;
use ratatui::crossterm::event::EnableBracketedPaste;
use ratatui::crossterm::execute;

fn main() -> std::process::ExitCode {
    let mode = std::env::args_os().nth(1);
    if mode.is_none() {
        return std::process::ExitCode::SUCCESS;
    }

    let result: io::Result<()> = with_terminal(|terminal| {
        execute!(io::stdout(), Hide, EnableBracketedPaste)?;
        terminal.draw(|_| {})?;
        writeln!(io::stdout(), "TERMINAL_FIXTURE_READY")?;
        io::stdout().flush()?;

        match mode.as_deref().and_then(|arg| arg.to_str()) {
            Some("error") => Err(io::Error::other("injected fixture error")),
            Some("panic") => panic!("injected fixture panic"),
            Some(other) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown fixture mode: {other}"),
            )),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture mode must be UTF-8",
            )),
        }
    });

    if let Err(error) = result {
        // with_terminal has restored the terminal before this diagnostic.
        eprintln!("fixture failed: {error}");
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}
