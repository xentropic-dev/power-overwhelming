use super::{HEIGHT, OpenLink, TerminalView};
use ghostty_vt::{
    key::{self, Key},
    mouse,
    terminal::{Mode, ScrollViewport},
};
use gpui::*;
use infrastructure::client::TerminalControl;

impl TerminalView {
    fn input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() || self.exited {
            return;
        }
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        self.selection = None;
        self.pressed_link = None;
        if let Err(error) = self.commands.try_send(TerminalControl::Input(bytes)) {
            self.status = Some(format!("Could not send input. {error}"));
        }
        self.refresh();
        cx.notify();
    }

    fn mouse_input(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        action: mouse::Action,
        button: mouse::Button,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut event) = mouse::Event::new() else {
            return;
        };
        let local = position - self.bounds.origin;
        let geometry = self.geometry.unwrap_or_default();
        event
            .set_action(action)
            .set_button(Some(button))
            .set_mods(terminal_modifiers(modifiers));
        event.set_position(mouse::Position {
            x: local.x.into(),
            y: local.y.into(),
        });
        self.mouse
            .set_options_from_terminal(&self.terminal)
            .set_any_button_pressed(self.reporting_mouse)
            .set_size(mouse::EncoderSize {
                screen_width: u32::from(geometry.cols) * u32::from(geometry.cell_width),
                screen_height: u32::from(geometry.rows) * u32::from(geometry.cell_height),
                cell_width: geometry.cell_width.into(),
                cell_height: geometry.cell_height.into(),
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            });
        let mut bytes = Vec::new();
        if self.mouse.encode_to_vec(&event, &mut bytes).is_ok() {
            self.input(bytes, cx);
        }
    }

    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stroke = &event.keystroke;
        if stroke.modifiers.control && stroke.modifiers.shift {
            match stroke.key.as_str() {
                "c" => {
                    if let Some(copied) = self.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(copied));
                    }
                    return;
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        let clean = text.replace(['\x1b', '\0'], "");
                        let bytes = if self.terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false) {
                            format!("\x1b[200~{clean}\x1b[201~").into_bytes()
                        } else {
                            clean.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
                        };
                        self.input(bytes, cx);
                    }
                    return;
                }
                _ => {}
            }
        }
        let Ok(mut encoded) = key::Event::new() else {
            return;
        };
        let key = key_code(&stroke.key);
        let first = stroke
            .key
            .chars()
            .next()
            .filter(|_| stroke.key.chars().count() == 1);
        let text = stroke
            .key_char
            .clone()
            .or_else(|| first.map(|value| value.to_string()));
        encoded
            .set_key(key)
            .set_mods(terminal_modifiers(stroke.modifiers))
            .set_action(if event.is_held {
                key::Action::Repeat
            } else {
                key::Action::Press
            });
        if let Some(character) = first {
            encoded.set_unshifted_codepoint(character);
        }
        encoded.set_utf8(text);
        self.encoder.set_options_from_terminal(&self.terminal);
        let mut bytes = Vec::new();
        if self.encoder.encode_to_vec(&encoded, &mut bytes).is_ok() {
            self.input(bytes, cx);
        }
    }

    pub(super) fn hover(&mut self, hovered: &bool, window: &mut Window, cx: &mut Context<Self>) {
        let changed = if *hovered {
            self.set_pointer(Some(window.mouse_position()))
        } else {
            self.clear_pointer()
        };
        if changed {
            cx.notify();
        }
    }

    pub(super) fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window);
        self.set_pointer(Some(event.position));
        let cell = self.at(event.position);
        let link = self.link_at_point(event.position).cloned();
        if !event.modifiers.shift
            && self.terminal.is_mouse_tracking().unwrap_or(false)
            && link.is_none()
        {
            self.reporting_mouse = true;
            self.mouse_input(
                event.position,
                event.modifiers,
                mouse::Action::Press,
                mouse::Button::Left,
                cx,
            );
            return;
        }
        self.pressed_link = link.filter(|_| !event.modifiers.shift);
        self.selection = Some((cell, cell));
        self.dragging = true;
        cx.notify();
    }

    pub(super) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut changed = self.set_pointer(Some(event.position));
        if self.reporting_mouse {
            self.mouse_input(
                event.position,
                event.modifiers,
                mouse::Action::Motion,
                mouse::Button::Left,
                cx,
            );
        } else if self.dragging
            && let Some((anchor, previous)) = self.selection
        {
            let cell = self.at(event.position);
            if previous != cell {
                self.pressed_link = None;
                self.selection = Some((anchor, cell));
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    pub(super) fn mouse_up(
        &mut self,
        event: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_pointer(Some(event.position));
        let pressed = self.pressed_link.take();
        if self.reporting_mouse {
            self.mouse_input(
                event.position,
                event.modifiers,
                mouse::Action::Release,
                mouse::Button::Left,
                cx,
            );
            self.reporting_mouse = false;
        } else if self.dragging {
            let cell = self.at(event.position);
            if self.selection == Some((cell, cell)) {
                self.selection = None;
                if let Some(pressed) = pressed
                    && self
                        .link_at_point(event.position)
                        .is_some_and(|link| *link == pressed)
                {
                    cx.emit(OpenLink(pressed.to_string()));
                }
            }
            self.dragging = false;
            cx.notify();
        }
    }

    pub(super) fn mouse_up_out(
        &mut self,
        event: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.clear_pointer();
        if self.reporting_mouse {
            self.mouse_input(
                event.position,
                event.modifiers,
                mouse::Action::Release,
                mouse::Button::Left,
                cx,
            );
            self.reporting_mouse = false;
        }
        self.dragging = false;
        if changed {
            cx.notify();
        }
    }

    pub(super) fn scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let amount = -(f32::from(event.delta.pixel_delta(px(HEIGHT)).y) / HEIGHT).round() as isize;
        if amount != 0 {
            if !event.modifiers.shift && self.terminal.is_mouse_tracking().unwrap_or(false) {
                let button = if amount < 0 {
                    mouse::Button::Four
                } else {
                    mouse::Button::Five
                };
                for _ in 0..amount.unsigned_abs().min(12) {
                    self.mouse_input(
                        event.position,
                        event.modifiers,
                        mouse::Action::Press,
                        button,
                        cx,
                    );
                }
                return;
            }
            self.terminal.scroll_viewport(ScrollViewport::Delta(amount));
            self.selection = None;
            self.pressed_link = None;
            self.refresh();
            cx.notify();
        }
    }
}

pub(super) fn register_mouse_exit(pointer_owner: WeakEntity<TerminalView>, window: &mut Window) {
    window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
        if phase == DispatchPhase::Bubble {
            let _ = pointer_owner.update(cx, |view, cx| {
                if view.clear_pointer() {
                    cx.notify();
                }
            });
        }
    });
}

fn terminal_modifiers(modifiers: Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    mods.set(key::Mods::SHIFT, modifiers.shift);
    mods.set(key::Mods::CTRL, modifiers.control);
    mods.set(key::Mods::ALT, modifiers.alt);
    mods.set(key::Mods::SUPER, modifiers.platform);
    mods
}

fn key_code(key: &str) -> Key {
    use Key::*;
    match key {
        "a" => A,
        "b" => B,
        "c" => C,
        "d" => D,
        "e" => E,
        "f" => F,
        "g" => G,
        "h" => H,
        "i" => I,
        "j" => J,
        "k" => K,
        "l" => L,
        "m" => M,
        "n" => N,
        "o" => O,
        "p" => P,
        "q" => Q,
        "r" => R,
        "s" => S,
        "t" => T,
        "u" => U,
        "v" => V,
        "w" => W,
        "x" => X,
        "y" => Y,
        "z" => Z,
        "0" => Digit0,
        "1" => Digit1,
        "2" => Digit2,
        "3" => Digit3,
        "4" => Digit4,
        "5" => Digit5,
        "6" => Digit6,
        "7" => Digit7,
        "8" => Digit8,
        "9" => Digit9,
        "-" => Minus,
        "=" => Equal,
        "[" => BracketLeft,
        "]" => BracketRight,
        "/" => Slash,
        "." => Period,
        "," => Comma,
        ";" => Semicolon,
        "space" => Space,
        "enter" => Enter,
        "backspace" => Backspace,
        "tab" => Tab,
        "escape" => Escape,
        "left" => ArrowLeft,
        "right" => ArrowRight,
        "up" => ArrowUp,
        "down" => ArrowDown,
        "home" => Home,
        "end" => End,
        "pageup" => PageUp,
        "pagedown" => PageDown,
        "insert" => Insert,
        "delete" => Delete,
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        _ => Unidentified,
    }
}
