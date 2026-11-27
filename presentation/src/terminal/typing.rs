//! What the keyboard and input methods send to the program, kept apart from the view so the
//! whole lifecycle (presses and their releases, compositions, focus reports) can be tested
//! without a window.

use super::keys::{KeyEncoder, focus_report, key_id};
use ghostty_vt::{Terminal, key, terminal::Mode};
use gpui::Keystroke;
use std::{cell::Cell, rc::Rc};

/// Whether Power itself is reading the keyboard (a prefix key is pending or link hints are
/// up). The workspace sets it; while it is set, input-method text never reaches a terminal,
/// just as its keys don't.
#[derive(Clone, Default)]
pub struct InputHold(Rc<Cell<bool>>);

impl InputHold {
    pub fn set(&self, held: bool) {
        self.0.set(held);
    }

    pub fn is_held(&self) -> bool {
        self.0.get()
    }
}

/// A key as the program knows it across presses and releases. Modifiers change GPUI's name
/// for a key (Shift+1 presses `!` and, once Shift is up, releases `1`), so keys libghostty
/// knows are tracked by their physical key, and the rest by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum KeyId {
    Physical(key::Key),
    Named(String),
}

/// What a key-down meant for the terminal.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum KeyDown {
    /// Not a key the terminal encodes; it may still type through the input method.
    Ignored,
    /// Handled here: these bytes (maybe none) go to the program.
    Send(Vec<u8>),
}

pub(super) struct Typing {
    encoder: key::Encoder,
    /// Keys whose press went to the program, so their releases go too.
    pressed: Vec<KeyId>,
    /// What an input method is composing, shown at the cursor until it commits.
    marked: Option<String>,
    /// Whether the terminal has focus, as focus reporting sees it.
    focused: bool,
    hold: InputHold,
}

impl Typing {
    pub fn new(hold: InputHold) -> Self {
        Self {
            encoder: key::Encoder::new().expect("allocate key encoder"),
            pressed: Vec::new(),
            marked: None,
            focused: false,
            hold,
        }
    }

    /// The composition to draw, if one is in progress and the terminal owns the keyboard.
    pub fn marked(&self) -> Option<&str> {
        self.marked.as_deref().filter(|_| !self.hold.is_held())
    }

    pub fn key_down(&mut self, terminal: &Terminal, stroke: &Keystroke, held: bool) -> KeyDown {
        if self.marked.is_some() {
            // GPUI composes dead keys itself: it marks the accent, then delivers the dead
            // key's press with no text, and later the composed character as a key-down with
            // text. The dead key is part of the composition, not a key the program sees, and
            // the composed key ends it.
            if stroke.key_char.is_none() {
                if is_printable(stroke) {
                    return KeyDown::Send(Vec::new());
                }
            } else {
                self.marked = None;
            }
        }
        let action = if held {
            key::Action::Repeat
        } else {
            key::Action::Press
        };
        let Some(bytes) = (KeyEncoder {
            encoder: &mut self.encoder,
            terminal,
        })
        .key(stroke, action) else {
            return KeyDown::Ignored;
        };
        let id = key_id(&stroke.key);
        if !self.pressed.contains(&id) {
            self.pressed.push(id);
        }
        KeyDown::Send(bytes)
    }

    /// The release of a key whose press this terminal sent, as the program asked for it, or
    /// `None` when the press was not the terminal's (an app shortcut, a dead key).
    pub fn key_up(&mut self, terminal: &Terminal, stroke: &Keystroke) -> Option<Vec<u8>> {
        let id = key_id(&stroke.key);
        let index = self.pressed.iter().position(|pressed| *pressed == id)?;
        self.pressed.swap_remove(index);
        Some(
            KeyEncoder {
                encoder: &mut self.encoder,
                terminal,
            }
            .key(stroke, key::Action::Release)
            .unwrap_or_default(),
        )
    }

    /// Text an input method committed, or `None` while Power holds the keyboard.
    pub fn commit(&mut self, text: &str) -> Option<Vec<u8>> {
        self.marked = None;
        (!self.hold.is_held() && !text.is_empty()).then(|| text.as_bytes().to_vec())
    }

    pub fn mark(&mut self, text: &str) {
        self.marked = (!text.is_empty() && !self.hold.is_held()).then(|| text.to_owned());
    }

    /// Drops the composition; whether there was one.
    pub fn unmark(&mut self) -> bool {
        self.marked.take().is_some()
    }

    /// Records a focus change and reports it if the program turned focus reporting on.
    pub fn set_focused(&mut self, terminal: &Terminal, focused: bool) -> Option<Vec<u8>> {
        if focused == self.focused {
            return None;
        }
        self.focused = focused;
        if !focused {
            // Keys held as focus leaves never deliver their releases here.
            self.pressed.clear();
            self.marked = None;
        }
        focus_report(terminal, focused)
    }

    /// Reports focus to a program whose snapshot just replaced `previous`. A view can take
    /// focus before its first snapshot arrives, against a terminal without focus reporting,
    /// so a program that turned it on learns the state now, once.
    pub fn installed(&mut self, previous: &Terminal, terminal: &Terminal) -> Option<Vec<u8>> {
        if focus_reporting(previous) {
            return None;
        }
        focus_report(terminal, self.focused)
    }
}

fn focus_reporting(terminal: &Terminal) -> bool {
    terminal.mode(Mode::FOCUS_EVENT).unwrap_or(false)
}

/// Whether the key types a character when it isn't composing: a dead key reports the
/// printable key it sits on, named keys (Escape, arrows) are not.
fn is_printable(stroke: &Keystroke) -> bool {
    let m = stroke.modifiers;
    !m.control && !m.alt && !m.platform && super::keys::types_text(&stroke.key)
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

    fn down(typing: &mut Typing, t: &Terminal, text: &str, key_char: Option<&str>) -> String {
        match typing.key_down(t, &stroke(text, key_char), false) {
            KeyDown::Send(bytes) => String::from_utf8(bytes).unwrap(),
            KeyDown::Ignored => "<ignored>".into(),
        }
    }

    fn up(typing: &mut Typing, t: &Terminal, text: &str) -> Option<String> {
        typing
            .key_up(t, &stroke(text, None))
            .map(|bytes| String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn input_method_commits_wait_for_power_to_release_the_keyboard() {
        let hold = InputHold::default();
        let mut typing = Typing::new(hold.clone());
        typing.mark("せんたく");
        assert_eq!(typing.marked(), Some("せんたく"));
        assert_eq!(typing.commit("選択").unwrap(), "選択".as_bytes());
        assert_eq!(typing.marked(), None);

        // Link hints or a pending prefix own the keyboard: nothing composes or commits.
        hold.set(true);
        typing.mark("せんたく");
        assert_eq!(typing.marked(), None);
        assert_eq!(typing.commit("選択"), None);
        hold.set(false);
        assert_eq!(typing.marked(), None);
        assert_eq!(typing.commit("選択").unwrap(), "選択".as_bytes());
    }

    #[test]
    fn a_composition_already_showing_hides_while_power_holds_the_keyboard() {
        let hold = InputHold::default();
        let mut typing = Typing::new(hold.clone());
        typing.mark("に");
        hold.set(true);
        assert_eq!(typing.marked(), None);
        assert_eq!(typing.commit("日本"), None);
        hold.set(false);
        assert_eq!(typing.marked(), None);
    }

    /// GPUI's xkb compose: dead acute marks `´` and presses `'` without text, then E
    /// arrives as `eacute` typing `é`.
    fn dead_acute_e(t: &Terminal) -> (Typing, Vec<String>) {
        let mut typing = Typing::new(InputHold::default());
        let mut sent = Vec::new();
        typing.mark("´");
        sent.push(down(&mut typing, t, "'", None));
        assert_eq!(typing.marked(), Some("´"));
        sent.extend(up(&mut typing, t, "'"));
        sent.push(down(&mut typing, t, "eacute", Some("é")));
        assert_eq!(typing.marked(), None, "the composed key ends the preedit");
        sent.extend(up(&mut typing, t, "e"));
        (typing, sent)
    }

    #[test]
    fn dead_keys_compose_without_sending_their_key_legacy() {
        let (_, sent) = dead_acute_e(&terminal(""));
        assert_eq!(sent.concat(), "é");
    }

    #[test]
    fn dead_keys_compose_without_sending_their_key_kitty() {
        // Disambiguate, report event types, report all keys as escape codes.
        let (mut typing, sent) = dead_acute_e(&terminal("\x1b[>11u"));
        assert_eq!(sent.concat(), "é");
        // Keys after the composition encode as usual.
        let t = terminal("\x1b[>11u");
        assert_eq!(down(&mut typing, &t, "'", Some("'")), "\x1b[39u");
        assert_eq!(up(&mut typing, &t, "'").unwrap(), "\x1b[39;1:3u");
    }

    #[test]
    fn named_keys_still_reach_the_program_during_a_composition() {
        let t = terminal("");
        let mut typing = Typing::new(InputHold::default());
        typing.mark("´");
        assert_eq!(down(&mut typing, &t, "left", None), "\x1b[D");
        assert_eq!(typing.marked(), Some("´"));
    }

    #[test]
    fn releases_pair_with_presses_across_shift_changes() {
        let t = terminal("\x1b[>11u");
        // Shift released first: GPUI presses `!` and releases `1`.
        let mut typing = Typing::new(InputHold::default());
        assert_eq!(down(&mut typing, &t, "!", Some("!")), "\x1b[49;2u");
        assert_eq!(up(&mut typing, &t, "1").unwrap(), "\x1b[49;1:3u");
        assert_eq!(up(&mut typing, &t, "1"), None);
        // Shift held through the release.
        assert_eq!(down(&mut typing, &t, "!", Some("!")), "\x1b[49;2u");
        assert_eq!(up(&mut typing, &t, "!").unwrap(), "\x1b[49;2:3u");
        // A press the app took as a shortcut releases nothing.
        assert_eq!(up(&mut typing, &t, "1"), None);
        assert_eq!(up(&mut typing, &t, "a"), None);
    }

    #[test]
    fn layout_keys_without_a_physical_key_pair_by_name() {
        let t = terminal("\x1b[>11u");
        let mut typing = Typing::new(InputHold::default());
        down(&mut typing, &t, "é", Some("é"));
        assert_eq!(up(&mut typing, &t, "ü"), None);
        assert!(up(&mut typing, &t, "é").is_some());
    }

    #[test]
    fn a_reattached_view_reports_focus_once_its_snapshot_turns_reporting_on() {
        let blank = terminal("");
        let mut typing = Typing::new(InputHold::default());
        // Focus arrives before the snapshot, against the default terminal.
        assert_eq!(typing.set_focused(&blank, true), None);
        let restored = terminal("\x1b[?1004h");
        assert_eq!(typing.installed(&blank, &restored).unwrap(), b"\x1b[I");
        // A later snapshot of the same program doesn't repeat it.
        let again = terminal("\x1b[?1004h");
        assert_eq!(typing.installed(&restored, &again), None);
        assert_eq!(typing.set_focused(&again, false).unwrap(), b"\x1b[O");
        assert_eq!(typing.set_focused(&again, true).unwrap(), b"\x1b[I");
    }

    #[test]
    fn an_unfocused_reattached_view_reports_focus_lost() {
        let blank = terminal("");
        let mut typing = Typing::new(InputHold::default());
        let restored = terminal("\x1b[?1004h");
        assert_eq!(typing.installed(&blank, &restored).unwrap(), b"\x1b[O");
        assert_eq!(typing.installed(&blank, &terminal("")), None);
    }
}
