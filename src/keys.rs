//! Keystrokes back into the bytes a terminal would have sent.
//!
//! The harness reads the real terminal through crossterm, which hands it
//! `KeyEvent`s — a decoded form. A child on a pseudo-terminal expects the
//! undecoded thing: bytes. So for a session whose agent is an embedded program
//! rather than this harness, every key has to be encoded back.
//!
//! What this is not: a full terminfo. It covers what a text interface is driven
//! by, in the encodings every terminal of the last forty years agrees on —
//! xterm's. Keys outside that return `None` and are dropped, which is what a
//! terminal without them would have done anyway.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const ESC: u8 = 0x1b;

/// The bytes for `key`, or `None` for a key a terminal would not have sent.
pub fn encode(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let body: Vec<u8> = match key.code {
        KeyCode::Char(c) if ctrl => {
            // The control block: Ctrl+A is 0x01 through Ctrl+Z is 0x1a, then
            // the four punctuation controls. This is arithmetic on the ASCII
            // code rather than a table because that is what it is — the
            // terminal clears the top three bits.
            let byte = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                '@' | ' ' => 0,
                '[' => 0x1b,
                '\\' => 0x1c,
                ']' => 0x1d,
                '^' => 0x1e,
                '_' | '?' => 0x1f,
                // Ctrl with anything else is not a control code; the character
                // alone is what a terminal sends.
                other => return printable(other, alt),
            };
            vec![byte]
        }
        KeyCode::Char(c) => return printable(c, alt),
        // Meta+Return is the newline a text interface takes without submitting.
        // Ctrl+J would be the other one, but the harness keeps that key.
        KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
            vec![ESC, b'\r']
        }
        KeyCode::Enter => vec![b'\r'],
        // Not `\t`: Ctrl+I *is* tab, and this arm is reached without Ctrl.
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        // DEL rather than BS. Terminals have disagreed about this for decades;
        // DEL is what xterm sends and so what readline and every TUI expects.
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![ESC],
        KeyCode::Up => cursor(b'A', key.modifiers),
        KeyCode::Down => cursor(b'B', key.modifiers),
        KeyCode::Right => cursor(b'C', key.modifiers),
        KeyCode::Left => cursor(b'D', key.modifiers),
        // Home and End have two encodings; the `[H`/`[F` pair is what xterm
        // sends in its normal keypad mode, which is the mode we are in.
        KeyCode::Home => cursor(b'H', key.modifiers),
        KeyCode::End => cursor(b'F', key.modifiers),
        KeyCode::Insert => tilde(2, key.modifiers),
        KeyCode::Delete => tilde(3, key.modifiers),
        KeyCode::PageUp => tilde(5, key.modifiers),
        KeyCode::PageDown => tilde(6, key.modifiers),
        KeyCode::F(n @ 1..=4) => {
            // F1-F4 are SS3, not CSI, and have no `~`.
            vec![ESC, b'O', b'P' + (n - 1)]
        }
        KeyCode::F(n @ 5..=12) => {
            // The numbers skip 16, 22, 27 and 30 for historical reasons; this
            // is the table rather than a formula because there isn't one.
            let code = match n {
                5 => 15,
                6..=10 => 17 + (n - 6),
                11 => 23,
                _ => 24,
            };
            return tilde(code, key.modifiers).into();
        }
        _ => return None,
    };
    Some(body)
}

/// A character, with Alt as the `ESC` prefix every terminal uses for it.
fn printable(c: char, alt: bool) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(5);
    if alt {
        bytes.push(ESC);
    }
    let mut buf = [0u8; 4];
    bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    Some(bytes)
}

/// `CSI <mods> <final>` for the arrows and Home/End.
fn cursor(final_byte: u8, mods: KeyModifiers) -> Vec<u8> {
    match modifier_code(mods) {
        // Plain: `CSI A`. A modifier needs the `1;n` parameter form, since
        // there is nowhere else to put it.
        None => vec![ESC, b'[', final_byte],
        Some(n) => {
            let mut bytes = vec![ESC, b'['];
            bytes.extend_from_slice(format!("1;{n}").as_bytes());
            bytes.push(final_byte);
            bytes
        }
    }
}

/// `CSI <n> [;<mods>] ~` for the editing and function keys.
fn tilde(code: u8, mods: KeyModifiers) -> Vec<u8> {
    let mut bytes = vec![ESC, b'['];
    match modifier_code(mods) {
        None => bytes.extend_from_slice(code.to_string().as_bytes()),
        Some(n) => bytes.extend_from_slice(format!("{code};{n}").as_bytes()),
    }
    bytes.push(b'~');
    bytes
}

/// xterm's modifier parameter: a bitfield, offset by one. `None` when no
/// modifier is held, which selects the shorter encoding.
fn modifier_code(mods: KeyModifiers) -> Option<u8> {
    let mut bits = 0;
    if mods.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    (bits != 0).then_some(bits + 1)
}

/// Pasted text, in the brackets that tell the child it is a paste rather than
/// someone typing very fast.
///
/// The harness turns bracketed paste on for its own terminal (see
/// [`crate::tui::init`]), so a paste arrives as one event; the child has asked
/// for the same thing if it is a text interface, and gets it back in the same
/// form.
pub fn encode_paste(text: &str) -> Vec<u8> {
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// The whole encoding, as a table. Every one of these is a byte sequence a
    /// real terminal sends, so a wrong entry is a key that silently does the
    /// wrong thing inside an embedded program — the kind of bug that is found
    /// by someone typing, not by a compiler.
    #[test]
    fn keys_encode_the_way_a_terminal_sends_them() {
        let cases: &[(KeyEvent, &[u8])] = &[
            // Ordinary typing, including past ASCII.
            (key(KeyCode::Char('a')), b"a"),
            (key(KeyCode::Char('Z')), b"Z"),
            (key(KeyCode::Char('é')), "é".as_bytes()),
            (key(KeyCode::Char(' ')), b" "),
            // The control block.
            (with(KeyCode::Char('c'), KeyModifiers::CONTROL), &[0x03]),
            (with(KeyCode::Char('a'), KeyModifiers::CONTROL), &[0x01]),
            (with(KeyCode::Char('z'), KeyModifiers::CONTROL), &[0x1a]),
            (with(KeyCode::Char('j'), KeyModifiers::CONTROL), &[0x0a]),
            // Case does not matter to a control code.
            (with(KeyCode::Char('D'), KeyModifiers::CONTROL), &[0x04]),
            (with(KeyCode::Char(' '), KeyModifiers::CONTROL), &[0x00]),
            // Alt is the ESC prefix.
            (with(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb"),
            // The basics.
            (key(KeyCode::Enter), b"\r"),
            (with(KeyCode::Enter, KeyModifiers::ALT), b"\x1b\r"),
            (with(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b\r"),
            (key(KeyCode::Tab), b"\t"),
            (key(KeyCode::BackTab), b"\x1b[Z"),
            (key(KeyCode::Backspace), &[0x7f]),
            (key(KeyCode::Esc), &[0x1b]),
            // Arrows, bare and modified.
            (key(KeyCode::Up), b"\x1b[A"),
            (key(KeyCode::Down), b"\x1b[B"),
            (key(KeyCode::Right), b"\x1b[C"),
            (key(KeyCode::Left), b"\x1b[D"),
            (with(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D"),
            (with(KeyCode::Right, KeyModifiers::ALT), b"\x1b[1;3C"),
            (with(KeyCode::Up, KeyModifiers::SHIFT), b"\x1b[1;2A"),
            // Home and End in normal keypad mode.
            (key(KeyCode::Home), b"\x1b[H"),
            (key(KeyCode::End), b"\x1b[F"),
            // The tilde family.
            (key(KeyCode::Delete), b"\x1b[3~"),
            (key(KeyCode::PageUp), b"\x1b[5~"),
            (key(KeyCode::PageDown), b"\x1b[6~"),
            (with(KeyCode::Delete, KeyModifiers::CONTROL), b"\x1b[3;5~"),
            // Function keys: SS3 below five, CSI from five up.
            (key(KeyCode::F(1)), b"\x1bOP"),
            (key(KeyCode::F(4)), b"\x1bOS"),
            (key(KeyCode::F(5)), b"\x1b[15~"),
            (key(KeyCode::F(12)), b"\x1b[24~"),
        ];

        for (event, expected) in cases {
            assert_eq!(
                encode(event).as_deref(),
                Some(*expected),
                "{:?} with {:?}",
                event.code,
                event.modifiers
            );
        }
    }

    /// A key a terminal has no sequence for is dropped rather than guessed at.
    #[test]
    fn a_key_with_no_encoding_is_dropped() {
        for code in [KeyCode::CapsLock, KeyCode::ScrollLock, KeyCode::Menu] {
            assert_eq!(encode(&key(code)), None, "{code:?}");
        }
    }

    /// A paste arrives as one event and leaves in the brackets that say so, or
    /// a program that treats a newline as "send" would send half of it.
    #[test]
    fn a_paste_keeps_its_brackets() {
        assert_eq!(
            encode_paste("two\nlines"),
            b"\x1b[200~two\nlines\x1b[201~".to_vec()
        );
    }
}
