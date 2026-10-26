use std::io::{self, IsTerminal, Write};
use std::panic;
use std::sync::Once;

use ratatui::crossterm::cursor::{SetCursorStyle, Show};
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;

static CLEANUP_PANIC_HOOK: Once = Once::new();

/// Run an interactive session with terminal setup and best-effort restoration.
///
/// Both standard input and output must be terminals. The callback's error is
/// returned only after the terminal cleanup guard has run.
pub fn with_terminal<T>(
    run: impl FnOnce(&mut ratatui::DefaultTerminal) -> io::Result<T>,
) -> io::Result<T> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Postvim requires interactive stdin and stdout",
        ));
    }

    install_cleanup_panic_hook();
    let _guard = TerminalCleanupGuard;
    let mut terminal = ratatui::try_init()?;
    execute!(io::stdout(), EnableBracketedPaste)?;
    run(&mut terminal)
}

fn install_cleanup_panic_hook() {
    CLEANUP_PANIC_HOOK.call_once(|| {
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            cleanup_paste_and_cursor();
            previous_hook(panic_info);
        }));
    });
}

struct TerminalCleanupGuard;

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        cleanup_paste_and_cursor();
        ratatui::restore();
    }
}

fn cleanup_paste_and_cursor() {
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        DisableBracketedPaste,
        Show,
        SetCursorStyle::DefaultUserShape
    );
    let _ = stdout.flush();
}
