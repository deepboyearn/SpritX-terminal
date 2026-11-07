// Regression tests for the shifted-digit key mapping (terminal-keys-1).
//
// `printable` in src/keys.rs previously mapped Shift+9 to '(' instead of
// ')', so typing ')' in a terminal produced '('. The tests below pin the
// full US shifted-digit row and the Shift+= mapping. `key_char` is left
// `None` so the key-name fallback path (the one the bug lived in) is what
// runs; GPUI normally fills `key_char` with the already-shifted char, which
// would sidestep this code entirely.

use gpui::{Keystroke, Modifiers};
use spritx_terminal::keys::encode_keystroke;

fn encoded(key: &str, modifiers: Modifiers) -> Option<Vec<u8>> {
    encode_keystroke(&Keystroke {
        modifiers,
        key: key.to_string(),
        key_char: None,
    })
}

fn shift() -> Modifiers {
    Modifiers { shift: true, ..Default::default() }
}

#[test]
fn shift_9_is_close_paren() {
    // The actual bug: pre-fix this returned b"(".
    assert_eq!(encoded("9", shift()), Some(b")".to_vec()));
}

#[test]
fn shift_0_is_close_paren() {
    assert_eq!(encoded("0", shift()), Some(b")".to_vec()));
}

#[test]
fn shift_1_is_bang() {
    assert_eq!(encoded("1", shift()), Some(b"!".to_vec()));
}

#[test]
fn shift_equal_is_plus() {
    assert_eq!(encoded("equal", shift()), Some(b"+".to_vec()));
}

#[test]
fn every_shifted_digit_yields_exactly_one_byte_with_the_us_mapping() {
    // The whole shifted digit row, so a single wrong entry anywhere fails.
    let expected: [(&str, u8); 10] = [
        ("0", b')'),
        ("1", b'!'),
        ("2", b'@'),
        ("3", b'#'),
        ("4", b'$'),
        ("5", b'%'),
        ("6", b'^'),
        ("7", b'&'),
        ("8", b'*'),
        ("9", b')'),
    ];
    for (digit, shifted) in expected {
        let out = encoded(digit, shift()).expect("digit must encode");
        assert_eq!(out.len(), 1, "shift+{digit} must be one byte, got {out:?}");
        assert_eq!(out, vec![shifted], "shift+{digit}");
    }
}

#[test]
fn unshifted_digits_are_unchanged() {
    for digit in ["0", "1", "5", "9"] {
        assert_eq!(encoded(digit, Modifiers::none()), Some(digit.as_bytes().to_vec()));
    }
}

#[test]
fn shift_9_via_named_key_path_is_close_paren() {
    // Same lookup through the digit branch as shift_9_is_close_paren, but
    // pinned alongside its neighbours so the ("9" => ")") arm cannot silently
    // drift back to "(" while the rest of the row stays correct.
    assert_eq!(encoded("8", shift()), Some(b"*".to_vec()));
    assert_eq!(encoded("9", shift()), Some(b")".to_vec()));
    assert_eq!(encoded("0", shift()), Some(b")".to_vec()));
}
#[test]
fn process_control_chords_include_named_punctuation_keys() {
    for (key, byte) in [("c", 3), ("d", 4), ("z", 26), ("backslash", 28),
        ("leftbracket", 27), ("rightbracket", 29)] {
        assert_eq!(encoded(key, Modifiers::control()), Some(vec![byte]), "Ctrl+{key}");
    }
}
