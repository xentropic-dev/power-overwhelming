//! Key presses and releases as the program asked to receive them: each key is described to
//! libghostty's encoder, which writes it once, in the legacy, modifyOtherKeys or Kitty form
//! the program negotiated, as Ghostty does. Pasted text and focus changes are encoded here too.
//!
//! The key description is adapted from herdr-app `src/terminal.rs` (`key_bytes`,
//! `control_alias_bytes`, `map_key`; herdr-app 3e6d564), licensed under Apache-2.0.

use ghostty_vt::{
    Terminal, focus,
    key::{self, Key},
    paste,
    terminal::Mode,
};
use gpui::{Keystroke, Modifiers};

/// A GPUI key name as libghostty knows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Mapped {
    key: Key,
    /// The name is a shifted symbol, such as `!`, which GPUI reports without Shift.
    shifted: bool,
    /// The key's character without modifiers, which the Kitty protocol reports.
    unshifted: Option<char>,
}

const fn named(key: Key) -> Option<Mapped> {
    Some(Mapped {
        key,
        shifted: false,
        unshifted: None,
    })
}

const fn printable(key: Key, unshifted: char) -> Option<Mapped> {
    Some(Mapped {
        key,
        shifted: false,
        unshifted: Some(unshifted),
    })
}

#[rustfmt::skip]
const FUNCTION_KEYS: [Key; 25] = {
    use Key::*;
    [
        F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19,
        F20, F21, F22, F23, F24, F25,
    ]
};

/// Shifted symbols on a US layout and their base keys.
#[rustfmt::skip]
const SHIFTED: &[(char, char)] = &[
    ('~', '`'), ('!', '1'), ('@', '2'), ('#', '3'), ('$', '4'), ('%', '5'), ('^', '6'),
    ('&', '7'), ('*', '8'), ('(', '9'), (')', '0'), ('_', '-'), ('+', '='), ('{', '['),
    ('}', ']'), ('|', '\\'), (':', ';'), ('"', '\''), ('<', ','), ('>', '.'), ('?', '/'),
];

/// The libghostty key for a GPUI key name. Names cover every platform: Linux reports keypad
/// keys by their xkb names without `kp_` (`add`, `decimal`, `begin`), and xkb names for the
/// rest (`print`, `scroll_lock`, `xf86audioraisevolume`). Keypad digits and Enter share their
/// main-keyboard names in GPUI, so they encode as those.
fn map_key(name: &str) -> Option<Mapped> {
    use Key::*;
    let mapped = match name {
        "enter" => named(Enter),
        "tab" => named(Tab),
        "escape" => named(Escape),
        "backspace" => named(Backspace),
        "space" => printable(Space, ' '),
        "delete" => named(Delete),
        "insert" => named(Insert),
        "home" => named(Home),
        "end" => named(End),
        "pageup" => named(PageUp),
        "pagedown" => named(PageDown),
        "up" => named(ArrowUp),
        "down" => named(ArrowDown),
        "left" => named(ArrowLeft),
        "right" => named(ArrowRight),
        "add" => printable(NumpadAdd, '+'),
        "subtract" => printable(NumpadSubtract, '-'),
        "multiply" => printable(NumpadMultiply, '*'),
        "divide" => printable(NumpadDivide, '/'),
        "decimal" => printable(NumpadDecimal, '.'),
        "separator" => printable(NumpadSeparator, ','),
        "equal" => printable(NumpadEqual, '='),
        "begin" => named(NumpadBegin),
        "print" | "printscreen" => named(PrintScreen),
        "scroll_lock" | "scrolllock" => named(ScrollLock),
        "pause" => named(Pause),
        "menu" | "contextmenu" => named(ContextMenu),
        "help" => named(Help),
        "xf86audioraisevolume" => named(AudioVolumeUp),
        "xf86audiolowervolume" => named(AudioVolumeDown),
        "xf86audiomute" => named(AudioVolumeMute),
        "xf86audioplay" | "xf86audiopause" => named(MediaPlayPause),
        "xf86audiostop" => named(MediaStop),
        "xf86audionext" => named(MediaTrackNext),
        "xf86audioprev" => named(MediaTrackPrevious),
        _ => None,
    };
    if mapped.is_some() {
        return mapped;
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<usize>().ok())
        && (1..=FUNCTION_KEYS.len()).contains(&n)
    {
        return named(FUNCTION_KEYS[n - 1]);
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let (base, shifted) = match SHIFTED.iter().find(|(symbol, _)| *symbol == c) {
        Some((_, base)) => (*base, true),
        None => (c.to_ascii_lowercase(), false),
    };
    let key = match base {
        'a'..='z' => letter(base),
        '0'..='9' => digit(base),
        '`' => Backquote,
        '\\' => Backslash,
        '[' => BracketLeft,
        ']' => BracketRight,
        ',' => Comma,
        '=' => Equal,
        '-' => Minus,
        '.' => Period,
        '\'' => Quote,
        ';' => Semicolon,
        '/' => Slash,
        // Any other character (a layout's own letters) still reaches the program as text.
        _ => Unidentified,
    };
    Some(Mapped {
        key,
        shifted,
        unshifted: Some(base),
    })
}

fn letter(c: char) -> Key {
    use Key::*;
    const LETTERS: [Key; 26] = [
        A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    ];
    LETTERS[usize::from(c as u8 - b'a')]
}

fn digit(c: char) -> Key {
    use Key::*;
    const DIGITS: [Key; 10] = [
        Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    ];
    DIGITS[usize::from(c as u8 - b'0')]
}

/// GPUI modifiers as libghostty's key and mouse encoders take them.
pub(super) fn mods(modifiers: Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    mods.set(key::Mods::SHIFT, modifiers.shift);
    mods.set(key::Mods::CTRL, modifiers.control);
    mods.set(key::Mods::ALT, modifiers.alt);
    mods.set(key::Mods::SUPER, modifiers.platform);
    mods
}

/// Encodes key events against one terminal's negotiated keyboard modes.
pub(super) struct KeyEncoder<'a> {
    pub encoder: &'a mut key::Encoder,
    pub terminal: &'a Terminal,
}

impl KeyEncoder<'_> {
    fn encode(
        &mut self,
        key: Key,
        mods: key::Mods,
        consumed: key::Mods,
        text: Option<&str>,
        unshifted: Option<char>,
        action: key::Action,
    ) -> Vec<u8> {
        let Ok(mut event) = key::Event::new() else {
            return Vec::new();
        };
        event
            .set_key(key)
            .set_mods(mods)
            .set_consumed_mods(consumed)
            .set_action(action)
            .set_utf8(text.map(str::to_owned));
        if let Some(unshifted) = unshifted {
            event.set_unshifted_codepoint(unshifted);
        }
        self.encoder.set_options_from_terminal(self.terminal);
        let mut bytes = Vec::new();
        if self.encoder.encode_to_vec(&event, &mut bytes).is_err() {
            bytes.clear();
        }
        bytes
    }

    /// What a key event sends to the program, or `None` when the key means nothing to it and
    /// may still type text through the input method.
    pub fn key(&mut self, stroke: &Keystroke, action: key::Action) -> Option<Vec<u8>> {
        let m = stroke.modifiers;
        let release = action == key::Action::Release;
        // libghostty leaves these aliases to fixterms, but GPUI drops their control text.
        // Legacy encoding needs them spelled out; negotiated modes keep the chord.
        let kitty = self
            .terminal
            .kitty_keyboard_flags()
            .is_ok_and(|flags| !flags.is_empty());
        if !release && !kitty && m.control && !m.alt && !m.platform && !m.function {
            let alias = match stroke.key.as_str() {
                "[" if !m.shift => Some((Key::BracketLeft, Key::Escape)),
                "i" if !m.shift => Some((Key::I, Key::Tab)),
                "m" if !m.shift => Some((Key::M, Key::Enter)),
                _ => None,
            };
            if let Some((key, alias)) = alias {
                let legacy = self.encode(
                    alias,
                    key::Mods::empty(),
                    key::Mods::empty(),
                    None,
                    None,
                    action,
                );
                return Some(self.control_alias(key, &stroke.key, false, legacy, action));
            }
            if matches!(stroke.key.as_str(), "-" | "_") {
                let shifted = m.shift || stroke.key == "_";
                let text = if shifted { "_" } else { "-" };
                return Some(self.control_alias(Key::Minus, text, shifted, vec![0x1f], action));
            }
        }
        let Some(mapped) = map_key(&stroke.key) else {
            return (!release)
                .then(|| {
                    stroke
                        .key_char
                        .as_ref()
                        .map(|text| text.as_bytes().to_vec())
                })
                .flatten();
        };
        let mut mods = mods(m);
        mods.set(key::Mods::SHIFT, m.shift || mapped.shifted);
        let text = match stroke.key.as_str() {
            _ if release => None,
            "space" => Some(" ".to_owned()),
            _ => stroke.key_char.clone(),
        };
        // Shift is always spent on the text. Alt is too when it typed something other than
        // ASCII (AltGr-like layouts), and on macOS whenever Option produced text.
        let mut consumed = mods & key::Mods::SHIFT;
        if m.alt
            && text
                .as_deref()
                .is_some_and(|text| cfg!(target_os = "macos") || !text.is_ascii())
        {
            consumed |= key::Mods::ALT;
        }
        Some(self.encode(
            mapped.key,
            mods,
            consumed,
            text.as_deref(),
            mapped.unshifted,
            action,
        ))
    }

    fn control_alias(
        &mut self,
        key: Key,
        text: &str,
        shifted: bool,
        legacy: Vec<u8>,
        action: key::Action,
    ) -> Vec<u8> {
        // libghostty has no modifyOtherKeys getter. Probe with printable base text so the
        // encoder can keep the chord when modifyOtherKeys asks for CSI 27.
        let mut mods = key::Mods::CTRL;
        mods.set(key::Mods::SHIFT, shifted);
        let encoded = self.encode(
            key,
            mods,
            key::Mods::empty(),
            Some(text),
            text.chars().next(),
            action,
        );
        if encoded.starts_with(b"\x1b[27;") {
            encoded
        } else {
            legacy
        }
    }
}

/// Pasted text as the program asked for it: bracketed when it turned bracketed paste on,
/// with unsafe control bytes replaced, and line breaks sent as Enter otherwise. Windows line
/// breaks count once.
pub(super) fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let mut data = text.replace("\r\n", "\n").into_bytes();
    let mut buffer = vec![0; data.len() + 16];
    loop {
        match paste::encode(&mut data, bracketed, &mut buffer) {
            Ok(written) => {
                buffer.truncate(written);
                return buffer;
            }
            Err(ghostty_vt::Error::OutOfSpace { required }) if required > buffer.len() => {
                buffer.resize(required, 0);
            }
            Err(_) => return Vec::new(),
        }
    }
}

/// Paste bytes for `terminal`'s current mode.
pub(super) fn paste_for(terminal: &Terminal, text: &str) -> Vec<u8> {
    let bracketed = terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
    paste_bytes(text, bracketed)
}

/// The focus report for `focused`, when the program turned focus reporting (mode 1004) on.
pub(super) fn focus_report(terminal: &Terminal, focused: bool) -> Option<Vec<u8>> {
    if !terminal.mode(Mode::FOCUS_EVENT).unwrap_or(false) {
        return None;
    }
    let event = if focused {
        focus::Event::Gained
    } else {
        focus::Event::Lost
    };
    let mut buffer = [0; 8];
    let written = event.encode(&mut buffer).ok()?;
    Some(buffer[..written].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::terminal::Options;

    fn terminal(setup: &str) -> Terminal {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(setup.as_bytes());
        terminal
    }

    fn stroke(text: &str, key_char: Option<&str>) -> Keystroke {
        let mut stroke = Keystroke::parse(text).unwrap();
        stroke.key_char = key_char.map(str::to_owned);
        stroke
    }

    fn press(terminal: &Terminal, text: &str, key_char: Option<&str>) -> String {
        send(terminal, text, key_char, key::Action::Press)
    }

    fn send(
        terminal: &Terminal,
        text: &str,
        key_char: Option<&str>,
        action: key::Action,
    ) -> String {
        let mut encoder = key::Encoder::new().unwrap();
        let bytes = KeyEncoder {
            encoder: &mut encoder,
            terminal,
        }
        .key(&stroke(text, key_char), action)
        .unwrap_or_default();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn legacy_keys_type_text_and_control_codes() {
        let t = terminal("");
        assert_eq!(press(&t, "a", Some("a")), "a");
        assert_eq!(press(&t, "shift-a", Some("A")), "A");
        assert_eq!(press(&t, "!", Some("!")), "!");
        assert_eq!(press(&t, "ctrl-c", None), "\x03");
        assert_eq!(press(&t, "enter", None), "\r");
        assert_eq!(press(&t, "space", Some(" ")), " ");
        assert_eq!(press(&t, "alt-x", Some("x")), "\x1bx");
        assert_eq!(press(&t, "ctrl-[", None), "\x1b");
        assert_eq!(press(&t, "ctrl-i", None), "\t");
        assert_eq!(press(&t, "ctrl-m", None), "\r");
        assert_eq!(press(&t, "ctrl--", None), "\x1f");
        assert_eq!(press(&t, "ctrl-_", None), "\x1f");
        assert_eq!(press(&t, "f1", None), "\x1bOP");
        assert_eq!(press(&t, "f12", None), "\x1b[24~");
        assert_eq!(press(&t, "shift-f5", None), "\x1b[15;2~");
        assert_eq!(press(&t, "ctrl-up", None), "\x1b[1;5A");
    }

    #[test]
    fn layout_text_without_a_known_key_still_types() {
        let t = terminal("");
        assert_eq!(press(&t, "é", Some("é")), "é");
        assert_eq!(press(&t, "e", Some("€")), "€");
        // A dead key's press carries no text and sends nothing.
        assert_eq!(press(&t, "dead_acute", None), "");
    }

    #[test]
    fn punctuation_maps_to_its_physical_keys() {
        for (name, key, shifted, unshifted) in [
            ("`", Key::Backquote, false, '`'),
            ("~", Key::Backquote, true, '`'),
            ("\\", Key::Backslash, false, '\\'),
            ("|", Key::Backslash, true, '\\'),
            ("'", Key::Quote, false, '\''),
            ("\"", Key::Quote, true, '\''),
            ("?", Key::Slash, true, '/'),
            ("@", Key::Digit2, true, '2'),
            (";", Key::Semicolon, false, ';'),
        ] {
            assert_eq!(
                map_key(name),
                Some(Mapped {
                    key,
                    shifted,
                    unshifted: Some(unshifted)
                }),
                "{name}"
            );
        }
        assert_eq!(map_key("f13").unwrap().key, Key::F13);
        assert_eq!(map_key("f25").unwrap().key, Key::F25);
        assert_eq!(map_key("f26"), None);
        assert_eq!(map_key("add").unwrap().key, Key::NumpadAdd);
        assert_eq!(map_key("print").unwrap().key, Key::PrintScreen);
    }

    #[test]
    fn keypad_keys_follow_keypad_application_mode() {
        let t = terminal("");
        assert_eq!(press(&t, "add", Some("+")), "+");
        assert_eq!(press(&t, "decimal", Some(".")), ".");
        // Like Ghostty, application keypad needs mode 1035 (numlock ignores it) off too.
        let t = terminal("\x1b=");
        assert_eq!(press(&t, "add", Some("+")), "+");
        let t = terminal("\x1b=\x1b[?1035l");
        assert_eq!(press(&t, "add", Some("+")), "\x1bOk");
        assert_eq!(press(&t, "divide", Some("/")), "\x1bOo");
    }

    #[test]
    fn kitty_protocol_reports_numpad_function_keys_and_releases() {
        // Disambiguate escape codes.
        let t = terminal("\x1b[>1u");
        assert_eq!(press(&t, "add", Some("+")), "+");
        assert_eq!(press(&t, "f13", None), "\x1b[57376u");
        assert_eq!(press(&t, "f25", None), "\x1b[57388u");
        assert_eq!(press(&t, "ctrl-[", None), "\x1b[91;5u");
        assert_eq!(press(&t, "ctrl-i", None), "\x1b[105;5u");
        assert_eq!(press(&t, "ctrl-!", None), "\x1b[49;6u");
        // Without event reporting a release sends nothing.
        assert_eq!(send(&t, "a", Some("a"), key::Action::Release), "");
        // Disambiguate plus report event types.
        let t = terminal("\x1b[>3u");
        assert_eq!(
            send(&t, "ctrl-a", None, key::Action::Release),
            "\x1b[97;5:3u"
        );
        assert_eq!(
            send(&t, "ctrl-a", None, key::Action::Repeat),
            "\x1b[97;5:2u"
        );
        assert_eq!(
            send(&t, "escape", None, key::Action::Release),
            "\x1b[27;1:3u"
        );
        // Report all keys as escape codes: keypad keys get their own codes.
        let t = terminal("\x1b[>9u");
        assert_eq!(press(&t, "add", Some("+")), "\x1b[57413u");
        assert_eq!(press(&t, "decimal", Some(".")), "\x1b[57409u");
    }

    #[test]
    fn legacy_releases_send_nothing() {
        let t = terminal("");
        assert_eq!(send(&t, "a", Some("a"), key::Action::Release), "");
        assert_eq!(send(&t, "ctrl-[", None, key::Action::Release), "");
        assert_eq!(send(&t, "é", Some("é"), key::Action::Release), "");
    }

    #[test]
    fn paste_is_bracketed_when_asked_and_made_safe() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc");
        assert_eq!(paste_bytes("a\r\nb", true), b"\x1b[200~a\nb\x1b[201~");
        let hostile = paste_bytes("x\x1b[201~rm -rf ~\n", true);
        assert!(hostile.starts_with(b"\x1b[200~"));
        assert!(hostile.ends_with(b"\x1b[201~"));
        assert_eq!(
            hostile
                .windows(6)
                .filter(|window| window == b"\x1b[201~")
                .count(),
            1,
            "{}",
            hostile.escape_ascii()
        );
        assert_eq!(paste_bytes("", false), b"");
        let t = terminal("\x1b[?2004h");
        assert_eq!(paste_for(&t, "hi"), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste_for(&terminal(""), "hi"), b"hi");
    }

    #[test]
    fn focus_reports_only_when_the_program_asks() {
        assert_eq!(focus_report(&terminal(""), true), None);
        let t = terminal("\x1b[?1004h");
        assert_eq!(focus_report(&t, true).unwrap(), b"\x1b[I");
        assert_eq!(focus_report(&t, false).unwrap(), b"\x1b[O");
    }
}
