use crate::term::TermModes;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Turn a winit key event into the byte sequence the shell expects on its
/// pty stdin. Returns `None` for key releases and keys with no terminal
/// meaning (e.g. bare modifier keys).
///
/// Takes the individual fields off `winit::event::KeyEvent` rather than the
/// event itself, since `KeyEvent` has private fields and can't be
/// constructed outside winit -- this keeps the mapping logic unit-testable.
pub fn encode_key(
    logical_key: &Key,
    text: Option<&str>,
    pressed: bool,
    mods: ModifiersState,
    modes: &TermModes,
) -> Option<Vec<u8>> {
    if !pressed {
        return None;
    }

    if let Key::Named(named) = logical_key {
        if let Some(seq) = encode_modified_named_key(*named, mods) {
            return Some(seq);
        }
        if let Some(seq) = encode_named_key(*named, modes.app_cursor_keys) {
            return Some(seq);
        }
    }

    // Ctrl+letter (and a few punctuation keys) map to C0 control codes.
    // This must run before the plain-text fallback below, since the OS
    // usually reports no `text` at all while Ctrl is held.
    if mods.control_key() {
        if let Key::Character(s) = logical_key {
            if let Some(byte) = encode_control_char(s) {
                return Some(vec![byte]);
            }
        }
    }

    if let Some(text) = text {
        if !text.is_empty() {
            return Some(text.as_bytes().to_vec());
        }
    }

    None
}

/// macOS-style line navigation, translated to the readline/zle bindings
/// every shell understands out of the box (emacs keymap, the default in
/// zsh and bash) -- the same translations iTerm2's default profile ships:
///
///   Cmd+Left/Right      -> Ctrl+A / Ctrl+E   (line start / line end)
///   Cmd+Backspace       -> Ctrl+U            (kill to line start)
///   Option+Left/Right   -> ESC b / ESC f     (word back / word forward)
///   Option+Backspace    -> ESC DEL           (kill word backward)
///
/// Emitting the emacs bindings rather than private CSI sequences means no
/// shell configuration is needed for these to work.
fn encode_modified_named_key(named: NamedKey, mods: ModifiersState) -> Option<Vec<u8>> {
    if mods.super_key() {
        return match named {
            NamedKey::ArrowLeft => Some(vec![0x01]),
            NamedKey::ArrowRight => Some(vec![0x05]),
            NamedKey::Backspace => Some(vec![0x15]),
            _ => None,
        };
    }
    if mods.alt_key() {
        return match named {
            NamedKey::ArrowLeft => Some(b"\x1bb".to_vec()),
            NamedKey::ArrowRight => Some(b"\x1bf".to_vec()),
            NamedKey::Backspace => Some(vec![0x1b, 0x7f]),
            _ => None,
        };
    }
    None
}

fn encode_named_key(named: NamedKey, app_cursor_keys: bool) -> Option<Vec<u8>> {
    Some(match named {
        NamedKey::Enter => b"\r".to_vec(),
        NamedKey::Backspace => vec![0x7f],
        NamedKey::Tab => b"\t".to_vec(),
        NamedKey::Escape => vec![0x1b],
        NamedKey::ArrowUp => cursor_key_seq(b'A', app_cursor_keys),
        NamedKey::ArrowDown => cursor_key_seq(b'B', app_cursor_keys),
        NamedKey::ArrowRight => cursor_key_seq(b'C', app_cursor_keys),
        NamedKey::ArrowLeft => cursor_key_seq(b'D', app_cursor_keys),
        // Home/End follow DECCKM like the arrows do (terminfo khome/kend
        // are SS3 H / SS3 F for xterm-256color's application mode).
        NamedKey::Home => cursor_key_seq(b'H', app_cursor_keys),
        NamedKey::End => cursor_key_seq(b'F', app_cursor_keys),
        NamedKey::PageUp => b"\x1b[5~".to_vec(),
        NamedKey::PageDown => b"\x1b[6~".to_vec(),
        NamedKey::Delete => b"\x1b[3~".to_vec(),
        NamedKey::Insert => b"\x1b[2~".to_vec(),
        NamedKey::F1 => b"\x1bOP".to_vec(),
        NamedKey::F2 => b"\x1bOQ".to_vec(),
        NamedKey::F3 => b"\x1bOR".to_vec(),
        NamedKey::F4 => b"\x1bOS".to_vec(),
        NamedKey::F5 => b"\x1b[15~".to_vec(),
        NamedKey::F6 => b"\x1b[17~".to_vec(),
        NamedKey::F7 => b"\x1b[18~".to_vec(),
        NamedKey::F8 => b"\x1b[19~".to_vec(),
        NamedKey::F9 => b"\x1b[20~".to_vec(),
        NamedKey::F10 => b"\x1b[21~".to_vec(),
        NamedKey::F11 => b"\x1b[23~".to_vec(),
        NamedKey::F12 => b"\x1b[24~".to_vec(),
        _ => return None,
    })
}

/// DECCKM: cursor keys send SS3 (ESC O) instead of CSI (ESC [) in
/// application mode, which full-screen programs like vim rely on to tell
/// cursor movement apart from other CSI sequences.
fn cursor_key_seq(final_byte: u8, app_cursor_keys: bool) -> Vec<u8> {
    if app_cursor_keys {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

fn encode_control_char(s: &str) -> Option<u8> {
    let ch = s.chars().next()?;
    if ch.is_ascii_alphabetic() {
        return Some((ch.to_ascii_uppercase() as u8) & 0x1f);
    }
    match ch {
        // Ctrl+Space / Ctrl+@ -> NUL: emacs set-mark, a common tmux
        // prefix -- without this the fallback sent a plain space.
        ' ' | '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(app_cursor_keys: bool) -> TermModes {
        TermModes {
            app_cursor_keys,
            ..TermModes::default()
        }
    }

    #[test]
    fn release_is_ignored() {
        let key = Key::Named(NamedKey::Enter);
        assert_eq!(
            encode_key(&key, None, false, ModifiersState::empty(), &modes(false)),
            None
        );
    }

    #[test]
    fn enter_and_backspace() {
        let m = modes(false);
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Enter), None, true, ModifiersState::empty(), &m),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Backspace), None, true, ModifiersState::empty(), &m),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn arrow_keys_respect_decckm() {
        let key = Key::Named(NamedKey::ArrowUp);
        assert_eq!(
            encode_key(&key, None, true, ModifiersState::empty(), &modes(false)),
            Some(vec![0x1b, b'[', b'A'])
        );
        assert_eq!(
            encode_key(&key, None, true, ModifiersState::empty(), &modes(true)),
            Some(vec![0x1b, b'O', b'A'])
        );
    }

    #[test]
    fn ctrl_letter_maps_to_control_code() {
        let key = Key::Character("c".into());
        let bytes = encode_key(&key, None, true, ModifiersState::CONTROL, &modes(false));
        assert_eq!(bytes, Some(vec![0x03])); // Ctrl+C
    }

    #[test]
    fn ctrl_space_sends_nul() {
        let key = Key::Character(" ".into());
        let bytes = encode_key(&key, Some(" "), true, ModifiersState::CONTROL, &modes(false));
        assert_eq!(bytes, Some(vec![0x00]));
    }

    #[test]
    fn function_keys_send_xterm_sequences() {
        let m = modes(false);
        assert_eq!(
            encode_key(&Key::Named(NamedKey::F1), None, true, ModifiersState::empty(), &m),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::F5), None, true, ModifiersState::empty(), &m),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::F12), None, true, ModifiersState::empty(), &m),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn cmd_arrows_jump_to_line_start_and_end() {
        let m = modes(false);
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowLeft), None, true, ModifiersState::SUPER, &m),
            Some(vec![0x01]) // Ctrl+A
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowRight), None, true, ModifiersState::SUPER, &m),
            Some(vec![0x05]) // Ctrl+E
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Backspace), None, true, ModifiersState::SUPER, &m),
            Some(vec![0x15]) // Ctrl+U
        );
    }

    #[test]
    fn option_arrows_jump_by_word() {
        let m = modes(false);
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowLeft), None, true, ModifiersState::ALT, &m),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowRight), None, true, ModifiersState::ALT, &m),
            Some(b"\x1bf".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Backspace), None, true, ModifiersState::ALT, &m),
            Some(vec![0x1b, 0x7f])
        );
    }

    #[test]
    fn plain_arrows_are_unaffected_by_the_modifier_arms() {
        // A bare arrow must still produce its CSI sequence, not fall into
        // the Cmd/Option translations.
        let m = modes(false);
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowLeft), None, true, ModifiersState::empty(), &m),
            Some(vec![0x1b, b'[', b'D'])
        );
    }

    #[test]
    fn home_end_respect_decckm() {
        let key = Key::Named(NamedKey::Home);
        assert_eq!(
            encode_key(&key, None, true, ModifiersState::empty(), &modes(false)),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            encode_key(&key, None, true, ModifiersState::empty(), &modes(true)),
            Some(b"\x1bOH".to_vec())
        );
    }

    #[test]
    fn plain_text_passes_through() {
        let key = Key::Character("a".into());
        let bytes = encode_key(&key, Some("a"), true, ModifiersState::empty(), &modes(false));
        assert_eq!(bytes, Some(b"a".to_vec()));
    }

    #[test]
    fn unicode_text_passes_through() {
        let key = Key::Character("あ".into());
        let bytes = encode_key(&key, Some("あ"), true, ModifiersState::empty(), &modes(false));
        assert_eq!(bytes, Some("あ".as_bytes().to_vec()));
    }
}
