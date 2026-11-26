use super::{
    OpenLink, TerminalView,
    grid::PtyGrid,
    keys::{self, KeyEncoder, focus_report, paste_for},
};
use ghostty_vt::{key, mouse, terminal::ScrollViewport};
use gpui::*;
use infrastructure::client::TerminalControl;

/// The surface the mouse encoder maps positions on: the grid with its padding. Space past
/// the PTY's last column and row, including whole cells beyond its 500-cell limit, counts as
/// padding, so clicks there report the edge cell.
fn encoder_size(grid: &PtyGrid, pad: Pixels) -> mouse::EncoderSize {
    let metrics = grid.metrics;
    let pad = metrics.device_pixels(pad);
    let geometry = grid.geometry();
    let (cell_width, cell_height) = (
        u32::from(geometry.cell_width),
        u32::from(geometry.cell_height),
    );
    let used = size(
        u32::from(geometry.cols) * cell_width,
        u32::from(geometry.rows) * cell_height,
    );
    let area = grid.device_size();
    let (width, height) = (area.width.max(used.width), area.height.max(used.height));
    mouse::EncoderSize {
        screen_width: width + pad * 2,
        screen_height: height + pad * 2,
        cell_width,
        cell_height,
        padding_top: pad,
        padding_left: pad,
        padding_right: pad + width - used.width,
        padding_bottom: pad + height - used.height,
    }
}

impl TerminalView {
    pub(super) fn input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() || self.exited {
            return;
        }
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        self.selection = None;
        self.pressed_link = None;
        self.send(bytes);
        self.refresh();
        cx.notify();
    }

    /// Writes bytes to the program without touching the view: reports it asked for.
    fn send(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() || self.exited {
            return;
        }
        if let Err(error) = self.commands.try_send(TerminalControl::Input(bytes)) {
            self.status = Some(format!("Could not send input. {error}"));
        }
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
        // Ghostty encodes surface positions in device pixels, padding included.
        let metrics = self.grid.metrics;
        let pad = px(self.settings.padding);
        let local = position - (self.grid.bounds.origin - point(pad, pad));
        event
            .set_action(action)
            .set_button(Some(button))
            .set_mods(keys::mods(modifiers));
        event.set_position(mouse::Position {
            x: f32::from(local.x) * metrics.scale,
            y: f32::from(local.y) * metrics.scale,
        });
        self.mouse
            .set_options_from_terminal(&self.terminal)
            .set_any_button_pressed(self.reporting_mouse)
            .set_size(encoder_size(&self.grid, pad));
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
        let action = if event.is_held {
            key::Action::Repeat
        } else {
            key::Action::Press
        };
        let Some(bytes) = (KeyEncoder {
            encoder: &mut self.encoder,
            terminal: &self.terminal,
        })
        .key(&event.keystroke, action) else {
            return;
        };
        // Handled here, so the platform doesn't also type the key's text through the input
        // method handler.
        cx.stop_propagation();
        self.pressed_keys.insert(event.keystroke.key.clone());
        self.input(bytes, cx);
    }

    /// Releases reach programs that asked the Kitty protocol to report them, for keys whose
    /// press this terminal sent; a press the app took as a shortcut releases nothing.
    pub(super) fn key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.pressed_keys.remove(&event.keystroke.key) {
            return;
        }
        cx.stop_propagation();
        if let Some(bytes) = (KeyEncoder {
            encoder: &mut self.encoder,
            terminal: &self.terminal,
        })
        .key(&event.keystroke, key::Action::Release)
        {
            self.send(bytes);
        }
    }

    /// Copies the selection to the clipboard.
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(copied) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(copied));
        }
    }

    /// Types the clipboard's text in as a paste, bracketed if the program asked for it.
    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let bytes = paste_for(&self.terminal, &text);
            self.input(bytes, cx);
        }
    }

    /// Tells the program about a focus change if it turned focus reporting on. The terminal
    /// counts as focused while its window is active and it holds keyboard focus.
    pub(super) fn sync_focus(&mut self, window: &Window) {
        let focused = window.is_window_active() && self.focus.is_focused(window);
        if focused == self.focused {
            return;
        }
        self.focused = focused;
        if !focused {
            // Keys held as focus leaves never deliver their releases here.
            self.pressed_keys.clear();
            self.marked = None;
        }
        if let Some(bytes) = focus_report(&self.terminal, focused) {
            self.send(bytes);
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
        let line = self.grid.metrics.size.height;
        let amount =
            -(f32::from(event.delta.pixel_delta(line).y) / f32::from(line)).round() as isize;
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

#[cfg(test)]
mod tests {
    use super::{PtyGrid, encoder_size};
    use crate::terminal::CellMetrics;
    use ghostty_vt::mouse;
    use gpui::{Bounds, Pixels, Point, Size, point, px, size};

    fn sgr_press(grid: &PtyGrid, pad: f32, at: Point<Pixels>) -> String {
        let mut encoder = mouse::Encoder::new().unwrap();
        encoder
            .set_tracking_mode(mouse::TrackingMode::Normal)
            .set_format(mouse::Format::Sgr)
            .set_size(encoder_size(grid, px(pad)));
        let mut event = mouse::Event::new().unwrap();
        event
            .set_action(mouse::Action::Press)
            .set_button(Some(mouse::Button::Left));
        let local = at - (grid.bounds.origin - point(px(pad), px(pad)));
        event.set_position(mouse::Position {
            x: f32::from(local.x),
            y: f32::from(local.y),
        });
        let mut bytes = Vec::new();
        encoder.encode_to_vec(&event, &mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn grid(width: f32, height: f32, cell: Size<Pixels>) -> PtyGrid {
        let (tx, _rx) = async_channel::unbounded();
        let mut grid = PtyGrid::default();
        let metrics = CellMetrics {
            size: cell,
            ..CellMetrics::default()
        };
        let bounds = Bounds::new(point(px(4.), px(4.)), size(px(width), px(height)));
        grid.lay_out(bounds, metrics, &tx);
        grid
    }

    /// A 1912-pixel grid of 3-pixel cells fits 637 columns; the PTY takes 500, so a click at
    /// the right edge must report column 500, not 637.
    #[test]
    fn clicks_past_the_capped_grid_report_its_last_column() {
        let grid = grid(1912., 180., size(px(3.), px(18.)));
        assert_eq!(grid.geometry().cols, 500);
        assert_eq!(
            sgr_press(&grid, 4., point(px(1910.), px(10.))),
            "\x1b[<0;500;1M"
        );
        assert_eq!(
            sgr_press(&grid, 4., point(px(1495.), px(10.))),
            "\x1b[<0;498;1M"
        );
    }

    #[test]
    fn clicks_in_the_leftover_strip_report_the_edge_cell() {
        // 805 × 190: 100 columns and 10 rows with 5 and 10 pixels left over.
        let grid = grid(805., 190., size(px(8.), px(18.)));
        assert_eq!((grid.geometry().cols, grid.geometry().rows), (100, 10));
        assert_eq!(
            sgr_press(&grid, 4., point(px(807.), px(5.))),
            "\x1b[<0;100;1M"
        );
        assert_eq!(
            sgr_press(&grid, 4., point(px(5.), px(192.))),
            "\x1b[<0;1;10M"
        );
        assert_eq!(
            sgr_press(&grid, 4., point(px(13.), px(23.))),
            "\x1b[<0;2;2M"
        );
    }
}
