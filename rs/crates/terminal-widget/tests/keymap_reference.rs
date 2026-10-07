//! Keymap reference tests — assert [`encode_key`] turns representative key presses +
//! modifiers into the exact PTY byte sequence a VT/xterm shell expects.
//!
//! Modeled on Alacritty's keymap reference: a small, explicit table of (key, modifiers)
//! → bytes. Special (non-printable) keys are addressed through `slint::platform::Key`
//! rather than hardcoded private-use codepoints, so the table tracks Slint exactly the
//! way the encoder itself does.

use avada_terminal_widget::encode_key;
use slint::platform::Key;

/// The `KeyEvent.text` Slint delivers for a special key (a private-use codepoint).
fn special(k: Key) -> String {
    let s: slint::SharedString = k.into();
    s.to_string()
}

#[test]
fn printable_text_passes_through_unchanged() {
    assert_eq!(encode_key("a", false, false, false), Some(b"a".to_vec()));
    assert_eq!(encode_key("Z", false, false, true), Some(b"Z".to_vec()));
    assert_eq!(encode_key("7", false, false, false), Some(b"7".to_vec()));
}

#[test]
fn arrow_keys_emit_csi_sequences() {
    assert_eq!(
        encode_key(&special(Key::UpArrow), false, false, false),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::DownArrow), false, false, false),
        Some(b"\x1b[B".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::RightArrow), false, false, false),
        Some(b"\x1b[C".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::LeftArrow), false, false, false),
        Some(b"\x1b[D".to_vec())
    );
}

#[test]
fn page_and_home_end_keys_emit_their_sequences() {
    assert_eq!(
        encode_key(&special(Key::PageUp), false, false, false),
        Some(b"\x1b[5~".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::PageDown), false, false, false),
        Some(b"\x1b[6~".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::Home), false, false, false),
        Some(b"\x1b[H".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::End), false, false, false),
        Some(b"\x1b[F".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::Delete), false, false, false),
        Some(b"\x1b[3~".to_vec())
    );
}

#[test]
fn enter_tab_backspace_escape() {
    assert_eq!(
        encode_key(&special(Key::Return), false, false, false),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::Tab), false, false, false),
        Some(b"\t".to_vec())
    );
    // Terminals conventionally map Backspace to DEL (0x7f).
    assert_eq!(
        encode_key(&special(Key::Backspace), false, false, false),
        Some(vec![0x7f])
    );
    assert_eq!(
        encode_key(&special(Key::Escape), false, false, false),
        Some(vec![0x1b])
    );
}

#[test]
fn ctrl_letters_map_to_control_bytes() {
    // Ctrl-C -> ETX (0x03), Ctrl-D -> EOT (0x04), case-insensitive.
    assert_eq!(encode_key("c", true, false, false), Some(vec![0x03]));
    assert_eq!(encode_key("C", true, false, false), Some(vec![0x03]));
    assert_eq!(encode_key("d", true, false, false), Some(vec![0x04]));
    assert_eq!(encode_key("a", true, false, false), Some(vec![0x01]));
}

#[test]
fn ctrl_punctuation_maps_to_low_control_bytes() {
    assert_eq!(encode_key(" ", true, false, false), Some(vec![0x00])); // Ctrl-Space -> NUL
    assert_eq!(encode_key("[", true, false, false), Some(vec![0x1b])); // Ctrl-[ -> ESC
    assert_eq!(encode_key("\\", true, false, false), Some(vec![0x1c]));
    assert_eq!(encode_key("]", true, false, false), Some(vec![0x1d]));
}

#[test]
fn alt_prefixes_an_escape() {
    // Alt/Meta sends ESC then the text (e.g. Alt-b for word-back in readline).
    assert_eq!(encode_key("b", false, true, false), Some(vec![0x1b, b'b']));
}

#[test]
fn empty_text_sends_nothing() {
    // A bare modifier press (no text) must not emit bytes.
    assert_eq!(encode_key("", false, false, false), None);
}

#[test]
fn option_delete_kills_words() {
    // Option-Delete is backward-kill-word (ESC DEL); fn-Option-Delete is forward (ESC d) —
    // the bytes iTerm2 sends, which zsh, bash and Claude Code all bind.
    assert_eq!(
        encode_key(&special(Key::Backspace), false, true, false),
        Some(vec![0x1b, 0x7f])
    );
    assert_eq!(
        encode_key(&special(Key::Delete), false, true, false),
        Some(b"\x1bd".to_vec())
    );
}

#[test]
fn mac_chords_edit_the_line() {
    use avada_terminal_widget::keys::mac_line_edit_key;
    let cmd = |k: Key| mac_line_edit_key(&special(k), true, false, false);
    assert_eq!(cmd(Key::Backspace), Some(vec![0x15])); // Ctrl-U: delete to line start
    assert_eq!(cmd(Key::Delete), Some(vec![0x0b])); // Ctrl-K: delete to line end
    assert_eq!(cmd(Key::LeftArrow), Some(vec![0x01])); // Ctrl-A: line start
    assert_eq!(cmd(Key::RightArrow), Some(vec![0x05])); // Ctrl-E: line end
    let opt = |k: Key| mac_line_edit_key(&special(k), false, true, false);
    assert_eq!(opt(Key::LeftArrow), Some(b"\x1bb".to_vec())); // word back
    assert_eq!(opt(Key::RightArrow), Some(b"\x1bf".to_vec())); // word forward
                                                               // Option+Backspace is encode_key's job (ESC DEL), not a Mac special case.
    assert_eq!(opt(Key::Backspace), None);
    // No modifier, both, or Shift added: not a line edit.
    assert_eq!(
        mac_line_edit_key(&special(Key::Backspace), false, false, false),
        None
    );
    assert_eq!(
        mac_line_edit_key(&special(Key::LeftArrow), true, true, false),
        None
    );
    assert_eq!(
        mac_line_edit_key(&special(Key::Backspace), true, false, true),
        None
    );
    assert_eq!(mac_line_edit_key("a", true, false, false), None);
}
