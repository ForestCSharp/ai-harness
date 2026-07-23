//! Terminal setup and teardown.
//!
//! `ratatui::init` only enables raw mode and the alternate screen. We also want
//! mouse capture (scrollback), bracketed paste (multi-line pastes arrive as one
//! event), and — where supported — the kitty keyboard protocol, which is what
//! makes Shift+Enter distinguishable from Enter.

use std::io::{Stdout, stdout};

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Whether the kitty keyboard protocol was pushed, so teardown knows to pop it.
static ENHANCED_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn init() -> Result<Tui> {
    enable_raw_mode().context("enabling raw mode")?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .context("initialising terminal")?;

    // Optional: without it Shift+Enter is indistinguishable from Enter, which
    // is why Alt+Enter is the documented way to insert a newline.
    if supports_keyboard_enhancement().unwrap_or(false)
        && execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok()
    {
        ENHANCED_KEYS.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    set_panic_hook();

    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
    .context("creating terminal")?;
    Ok(terminal)
}

/// Undo everything [`init`] did. Safe to call more than once.
pub fn restore() {
    if ENHANCED_KEYS.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

/// Restore the terminal before a panic prints, so the message is readable and
/// the shell is not left in raw mode with the mouse captured.
fn set_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}
