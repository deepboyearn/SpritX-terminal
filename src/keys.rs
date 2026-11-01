use gpui::{KeyDownEvent, Keystroke, Modifiers};

/// Translate a GPUI key press into the bytes a terminal expects.
///
/// Returns `None` for pure modifier keys and for app shortcuts that should be
/// handled by the UI layer (Super/Cmd combinations).
pub fn encode_key(event: &KeyDownEvent) -> Option<Vec<u8>> {
    encode_keystroke(&event.keystroke)
}

pub fn encode_keystroke(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    let key = keystroke.key.as_str();

    if key == "shift" || key == "control" || key == "alt" || key == "super" || key == "meta" {
        return None;
    }
    if modifiers.platform {
        return None;
    }

    if modifiers.control {
        if let Some(byte) = control_byte(key) {
            return Some(vec![byte]);
        }
    }

    if let Some(sequence) = special_key(key, modifiers) {
        return Some(sequence.as_bytes().to_vec());
    }

    let mut text = printable(key, keystroke.key_char.as_deref(), modifiers.shift)?;
    if modifiers.alt {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.push(0x1b);
        bytes.append(&mut text);
        return Some(bytes);
    }
    Some(text)
}

fn control_byte(key: &str) -> Option<u8> {
    let byte = match key {
        "space" | "@" => 0x00,
        "[" => 0x1b,
        "\\" => 0x1c,
        "]" => 0x1d,
        "^" => 0x1e,
        "_" | "/" => 0x1f,
        "?" => 0x7f,
        key if key.len() == 1 => {
            let ch = key.as_bytes()[0].to_ascii_lowercase();
            if ch.is_ascii_lowercase() {
                ch - b'a' + 1
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(byte)
}

fn special_key(key: &str, modifiers: Modifiers) -> Option<&'static str> {
    Some(match key {
        "enter" | "return" => "\r",
        "tab" if modifiers.shift => "\x1b[Z",
        "tab" => "\t",
        "backspace" => "\x7f",
        "escape" => "\x1b",
        "delete" => "\x1b[3~",
        "insert" => "\x1b[2~",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "pageup" | "page_up" => "\x1b[5~",
        "pagedown" | "page_down" => "\x1b[6~",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "f1" => "\x1bOP",
        "f2" => "\x1bOQ",
        "f3" => "\x1bOR",
        "f4" => "\x1bOS",
        "f5" => "\x1b[15~",
        "f6" => "\x1b[17~",
        "f7" => "\x1b[18~",
        "f8" => "\x1b[19~",
        "f9" => "\x1b[20~",
        "f10" => "\x1b[21~",
        "f11" => "\x1b[23~",
        "f12" => "\x1b[24~",
        _ => return None,
    })
}

fn printable(key: &str, key_char: Option<&str>, shift: bool) -> Option<Vec<u8>> {
    if let Some(text) = key_char.filter(|text| !text.is_empty() && !text.chars().any(char::is_control)) {
        return Some(text.as_bytes().to_vec());
    }
    if key == "space" {
        return Some(b" ".to_vec());
    }
    if shift && key.len() == 1 {
        let ch = key.as_bytes()[0];
        if ch.is_ascii_digit() {
            let shifted = match ch {
                b'0' => ')', b'1' => '!', b'2' => '@', b'3' => '#', b'4' => '$',
                b'5' => '%', b'6' => '^', b'7' => '&', b'8' => '*', _ => '(',
            };
            return Some(vec![shifted as u8]);
        }
    }
    if key.chars().count() == 1 && !key.chars().next()?.is_control() {
        if shift && key.len() == 1 && key.as_bytes()[0].is_ascii_lowercase() {
            return Some(vec![key.as_bytes()[0].to_ascii_uppercase()]);
        }
        return Some(key.as_bytes().to_vec());
    }
    let named = match key {
        "comma" => ",",
        "period" => ".",
        "slash" => "/",
        "backslash" => "\\",
        "semicolon" => ";",
        "apostrophe" => "'",
        "minus" => "-",
        "equal" => "=",
        "leftbracket" => "[",
        "rightbracket" => "]",
        "grave" => "`",
        _ => return None,
    };
    let shifted = if shift {
        match named {
            "1" => "!",
            "2" => "@",
            "3" => "#",
            "4" => "$",
            "5" => "%",
            "6" => "^",
            "7" => "&",
            "8" => "*",
            "9" => "(",
            "0" => ")",
            "-" => "_",
            "=" => "+",
            "[" => "{",
            "]" => "}",
            "\\" => "|",
            ";" => ":",
            "'" => "\"",
            "," => "<",
            "." => ">",
            "/" => "?",
            "`" => "~",
            other => other,
        }
    } else {
        named
    };
    Some(shifted.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.to_string(),
            key_char: None,
        }
    }

    fn encoded(key: &str, modifiers: Modifiers) -> Option<Vec<u8>> {
        encode_keystroke(&stroke(key, modifiers))
    }

    fn shift() -> Modifiers {
        Modifiers { shift: true, ..Default::default() }
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers { control: true, shift: true, ..Default::default() }
    }

    fn command() -> Modifiers {
        Modifiers { platform: true, ..Default::default() }
    }

    #[test]
    fn plain_text_and_shift() {
        assert_eq!(encoded("a", Modifiers::none()).unwrap(), b"a");
        assert_eq!(encoded("a", shift()).unwrap(), b"A");
        assert_eq!(encoded("1", Modifiers::none()).unwrap(), b"1");
        assert_eq!(encoded("1", shift()).unwrap(), b"!");
        assert_eq!(encoded("space", Modifiers::none()).unwrap(), b" ");
        assert_eq!(encoded("comma", Modifiers::none()).unwrap(), b",");
    }

    #[test]
    fn key_char_wins_for_international_layouts() {
        let mut keystroke = stroke("s", Modifiers { alt: true, ..Default::default() });
        keystroke.key_char = Some("ß".into());
        assert_eq!(encode_keystroke(&keystroke).unwrap(), b"\x1b\xc3\x9f");
    }

    #[test]
    fn control_keys() {
        assert_eq!(encoded("c", Modifiers::control()).unwrap(), vec![3]);
        assert_eq!(encoded("d", Modifiers::control()).unwrap(), vec![4]);
        assert_eq!(encoded("z", Modifiers::control()).unwrap(), vec![26]);
        assert_eq!(encoded("[", Modifiers::control()).unwrap(), vec![27]);
        assert_eq!(encoded("space", Modifiers::control()).unwrap(), vec![0]);
    }

    #[test]
    fn terminal_sequences() {
        assert_eq!(encoded("enter", Modifiers::none()).unwrap(), b"\r");
        assert_eq!(encoded("backspace", Modifiers::none()).unwrap(), b"\x7f");
        assert_eq!(encoded("tab", Modifiers::none()).unwrap(), b"\t");
        assert_eq!(encoded("tab", shift()).unwrap(), b"\x1b[Z");
        assert_eq!(encoded("up", Modifiers::none()).unwrap(), b"\x1b[A");
        assert_eq!(encoded("down", Modifiers::none()).unwrap(), b"\x1b[B");
        assert_eq!(encoded("right", Modifiers::none()).unwrap(), b"\x1b[C");
        assert_eq!(encoded("left", Modifiers::none()).unwrap(), b"\x1b[D");
        assert_eq!(encoded("home", Modifiers::none()).unwrap(), b"\x1b[H");
        assert_eq!(encoded("delete", Modifiers::none()).unwrap(), b"\x1b[3~");
        assert_eq!(encoded("f5", Modifiers::none()).unwrap(), b"\x1b[15~");
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(encoded("x", Modifiers::alt()).unwrap(), b"\x1bx");
        assert_eq!(encoded("enter", Modifiers::alt()).unwrap(), b"\r");
    }

    #[test]
    fn platform_shortcuts_are_not_sent() {
        assert_eq!(encoded("a", command()), None);
        assert_eq!(encoded("p", ctrl_shift()).unwrap(), vec![16]);
    }
}
