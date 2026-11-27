use super::{
    OpenLink, TerminalView,
    grid::PtyGrid,
    keys::{self, paste_for},
    scroll,
    typing::KeyDown,
};
use ghostty_vt::{mouse, screen::Screen, terminal::ScrollViewport};
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
        let had_selection = self.selection.take().is_some();
        let had_pressed_link = self.pressed_link.take().is_some();
        if !self.send(bytes) {
            cx.notify();
        }
        if self.refresh() || had_selection || had_pressed_link {
            cx.notify();
        }
    }

    /// Writes bytes to the program without touching the view: reports it asked for. False
    /// when sending failed and the status changed.
    pub(super) fn send(&mut self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() || self.exited {
            return true;
        }
        if let Err(error) = self.commands.try_send(TerminalControl::Input(bytes)) {
            self.status = Some(format!("Could not send input. {error}"));
            return false;
        }
        true
    }

    /// Encodes a mouse event for the program, as its tracking mode and format ask. Motion
    /// that encodes the same as the last report (same cell, or same pixel) sends nothing.
    fn mouse_input(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        action: mouse::Action,
        button: Option<mouse::Button>,
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
            .set_button(button)
            .set_mods(keys::mods(modifiers));
        event.set_position(mouse::Position {
            x: f32::from(local.x) * metrics.scale,
            y: f32::from(local.y) * metrics.scale,
        });
        self.mouse
            .set_options_from_terminal(&self.terminal)
            .set_any_button_pressed(!self.buttons.is_empty())
            .set_size(encoder_size(&self.grid, pad));
        let mut bytes = Vec::new();
        if self.mouse.encode_to_vec(&event, &mut bytes).is_err() || bytes.is_empty() {
            return;
        }
        if action == mouse::Action::Motion && self.last_mouse_report == bytes {
            return;
        }
        // Remember a press or release as the motion it equals, so the click that focuses a
        // pane doesn't also start a drag in the same cell.
        let mut motion = Vec::new();
        if action == mouse::Action::Motion {
            motion.clone_from(&bytes);
        } else {
            event.set_action(mouse::Action::Motion);
            if action == mouse::Action::Release {
                event.set_button(None);
            }
            if self.mouse.encode_to_vec(&event, &mut motion).is_err() {
                motion.clear();
            }
        }
        self.last_mouse_report = motion;
        self.send_mouse(bytes, cx);
    }

    /// Mouse reports go to the program without scrolling to the bottom: a wheel report
    /// leaves the scrollback where it is.
    fn send_mouse(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let had_pressed_link = self.pressed_link.take().is_some();
        if !self.send(bytes) || had_pressed_link {
            cx.notify();
        }
    }

    /// Whether the program takes the mouse: it turned tracking on and Shift isn't held,
    /// since Shift keeps the mouse for selecting.
    fn takes_mouse(&self, modifiers: Modifiers) -> bool {
        !modifiers.shift && self.terminal.is_mouse_tracking().unwrap_or(false)
    }

    /// Reports a press (`Some(true)`), release (`Some(false)`) or drag (`None`) of `button`
    /// if the program takes the mouse. True when it did.
    fn report_button(
        &mut self,
        press: Option<bool>,
        button: MouseButton,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        if !scroll::track_button(&mut self.buttons, press, button) {
            return false;
        }
        if press == Some(true) && !self.takes_mouse(modifiers) {
            self.buttons.retain(|b| *b != button);
            return false;
        }
        let action = match press {
            Some(true) => mouse::Action::Press,
            Some(false) => mouse::Action::Release,
            None => mouse::Action::Motion,
        };
        self.mouse_input(position, modifiers, action, Some(mouse_button(button)), cx);
        true
    }

    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let composing = self.typing.marked().is_some();
        let KeyDown::Send(bytes) =
            self.typing
                .key_down(&self.terminal, &event.keystroke, event.is_held)
        else {
            return;
        };
        // Handled here, so the platform doesn't also type the key's text through the input
        // method handler.
        cx.stop_propagation();
        if composing && self.typing.marked().is_none() {
            cx.notify();
        }
        self.keyboard_input(bytes, cx);
    }

    /// Releases reach programs that asked the Kitty protocol to report them, for keys whose
    /// press this terminal sent; a press the app took as a shortcut releases nothing.
    pub(super) fn key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(bytes) = self.typing.key_up(&self.terminal, &event.keystroke) {
            cx.stop_propagation();
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
            self.keyboard_input(bytes, cx);
        }
    }

    /// Tells the program about a focus change if it turned focus reporting on. The terminal
    /// counts as focused while its window is active and it holds keyboard focus. The cursor
    /// follows the same state, so a change repaints it.
    pub(super) fn sync_focus(&mut self, window: &Window, cx: &mut Context<Self>) {
        let focused = window.is_window_active() && self.focus.is_focused(window);
        let was = self.typing.focused();
        if let Some(bytes) = self.typing.set_focused(&self.terminal, focused) {
            self.send(bytes);
        }
        if was != focused {
            cx.notify();
        }
    }

    pub(super) fn keyboard_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if !bytes.is_empty() {
            self.typed_at = Some(std::time::Instant::now());
        }
        self.input(bytes, cx);
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
        if event.button != MouseButton::Left {
            if self.report_button(
                Some(true),
                event.button,
                event.position,
                event.modifiers,
                cx,
            ) {
                cx.stop_propagation();
            }
            return;
        }
        self.focus.focus(window);
        self.set_pointer(Some(event.position));
        if self.grab_scrollbar(event.position) {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let cell = self.at(event.position);
        let link = self
            .clickable_link_at(event.position, event.modifiers)
            .cloned();
        if link.is_none()
            && self.report_button(
                Some(true),
                event.button,
                event.position,
                event.modifiers,
                cx,
            )
        {
            return;
        }
        self.pressed_link = link;
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
        if self.scrollbar_grab.is_some() || !self.buttons.is_empty() {
            // The window-wide capture listener handles drags, inside and outside the view.
        } else if self.dragging
            && let Some((anchor, previous)) = self.selection
        {
            let cell = self.at(event.position);
            if previous != cell {
                self.pressed_link = None;
                self.selection = Some((anchor, cell));
                changed = true;
            }
        } else if event.pressed_button.is_none() && self.takes_mouse(event.modifiers) {
            // Any-motion tracking (mode 1003) reports moves with no button held; the encoder
            // drops them in the other modes.
            self.mouse_input(
                event.position,
                event.modifiers,
                mouse::Action::Motion,
                None,
                cx,
            );
        }
        if changed {
            cx.notify();
        }
    }

    /// A drag of the scrollbar thumb or of a button the program took, anywhere in the window.
    fn drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.scrollbar_grab.is_some() {
            if event.pressed_button == Some(MouseButton::Left) {
                self.drag_scrollbar(event.position, cx);
            } else {
                self.scrollbar_grab = None;
            }
            return;
        }
        if let Some(button) = event.pressed_button
            && self.buttons.contains(&button)
        {
            self.report_button(None, button, event.position, event.modifiers, cx);
        }
    }

    /// Releases of buttons other than the left one, which `mouse_up` handles.
    fn release(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            self.report_button(
                Some(false),
                event.button,
                event.position,
                event.modifiers,
                cx,
            );
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
        if self.scrollbar_grab.take().is_some() {
            cx.notify();
        } else if self.report_button(
            Some(false),
            MouseButton::Left,
            event.position,
            event.modifiers,
            cx,
        ) {
        } else if self.dragging {
            let cell = self.at(event.position);
            if self.selection == Some((cell, cell)) {
                self.selection = None;
                if let Some(pressed) = pressed
                    && self
                        .clickable_link_at(event.position, event.modifiers)
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
        self.scrollbar_grab = None;
        self.report_button(
            Some(false),
            MouseButton::Left,
            event.position,
            event.modifiers,
            cx,
        );
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
        cx.stop_propagation();
        let geometry = self.grid.geometry();
        let steps = scroll::scroll_delta_steps(
            &mut self.scrolled,
            event.delta,
            event.touch_phase,
            self.grid.metrics.size,
            (geometry.cols, geometry.rows),
            self.settings.scroll_speed,
        );
        if steps.x == 0 && steps.y == 0 {
            return;
        }
        if self.takes_mouse(event.modifiers) {
            // Up and left are buttons 4 and 6, down and right 5 and 7.
            for (count, positive, negative) in [
                (steps.y, mouse::Button::Four, mouse::Button::Five),
                (steps.x, mouse::Button::Six, mouse::Button::Seven),
            ] {
                let button = if count > 0 { positive } else { negative };
                for _ in 0..count.unsigned_abs().min(MAX_WHEEL_REPORTS) {
                    self.mouse_input(
                        event.position,
                        event.modifiers,
                        mouse::Action::Press,
                        Some(button),
                        cx,
                    );
                }
            }
            return;
        }
        if steps.y == 0 {
            return;
        }
        let up = steps.y > 0;
        let lines = steps.y.unsigned_abs() as usize;
        if self.alternate_screen() {
            // A full-screen program that doesn't track the mouse scrolls by arrow keys.
            let bytes = self.typing.arrows(&self.terminal, up, lines);
            self.send(bytes);
            return;
        }
        let delta = lines as isize;
        self.scroll_viewport(ScrollViewport::Delta(if up { -delta } else { delta }), cx);
    }

    fn alternate_screen(&self) -> bool {
        self.terminal
            .active_screen()
            .is_ok_and(|screen| screen == Screen::Alternate)
    }

    fn scroll_viewport(&mut self, scroll: ScrollViewport, cx: &mut Context<Self>) {
        self.terminal.scroll_viewport(scroll);
        self.pressed_link = None;
        self.refresh();
        cx.notify();
    }

    /// Scrolls the scrollback by keyboard command. The alternate screen has none.
    pub fn scroll_history(&mut self, target: ScrollTarget, cx: &mut Context<Self>) {
        if self.alternate_screen() {
            return;
        }
        let rows = self.grid.geometry().rows as isize;
        let scroll = match target {
            ScrollTarget::Lines(lines) => ScrollViewport::Delta(lines),
            ScrollTarget::Pages(pages) => ScrollViewport::Delta(pages * rows.max(1)),
            ScrollTarget::Top => ScrollViewport::Top,
            ScrollTarget::Bottom => ScrollViewport::Bottom,
        };
        self.scroll_viewport(scroll, cx);
    }

    /// The scrollbar's track and thumb, when there is scrollback to show and the primary
    /// screen is up. The track runs down the view's right edge, over the grid's height,
    /// using the padding when it's wide enough and the last pixels of the grid otherwise,
    /// so scrollback appearing never resizes the terminal.
    pub(super) fn scrollbar(&self) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        if self.alternate_screen() {
            return None;
        }
        let bar = self.terminal.scrollbar().ok()?;
        let grid = self.grid.bounds;
        let cell = self.grid.metrics.size.height;
        let track_height = cell * f32::from(self.grid.geometry().rows);
        let pad = px(self.settings.padding);
        let edge = grid.origin.x + grid.size.width + pad;
        let width = SCROLLBAR_WIDTH;
        let track = Bounds::new(
            point(edge - width, grid.origin.y),
            size(width, track_height),
        );
        let (top, length) = scroll::scrollbar_thumb(
            bar.total,
            bar.offset,
            bar.len,
            f32::from(track_height),
            f32::from(cell),
        )?;
        let thumb = Bounds::new(
            point(track.origin.x, track.origin.y + px(top)),
            size(width, px(length)),
        );
        Some((track, thumb))
    }

    /// Starts a scrollbar drag for a press on the track: the thumb jumps under the pointer
    /// when the press misses it. False when the press is elsewhere.
    fn grab_scrollbar(&mut self, position: Point<Pixels>) -> bool {
        let Some((track, thumb)) = self.scrollbar() else {
            return false;
        };
        if !track.contains(&position) {
            return false;
        }
        let grab = scroll::scrollbar_grab_at(
            f32::from(thumb.origin.y),
            f32::from(thumb.size.height),
            f32::from(position.y),
        );
        self.scrollbar_grab = Some(grab);
        if !thumb.contains(&position) {
            self.place_thumb(track, f32::from(position.y - track.origin.y) - grab);
        }
        self.refresh();
        true
    }

    fn drag_scrollbar(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(grab), Some((track, _))) = (self.scrollbar_grab, self.scrollbar()) else {
            return;
        };
        self.place_thumb(track, f32::from(position.y - track.origin.y) - grab);
        if self.refresh() {
            cx.notify();
        }
    }

    fn place_thumb(&mut self, track: Bounds<Pixels>, top: f32) {
        let Ok(bar) = self.terminal.scrollbar() else {
            return;
        };
        let row = scroll::scrollbar_row_at(
            bar.total,
            bar.len,
            f32::from(track.size.height),
            f32::from(self.grid.metrics.size.height),
            top,
        );
        self.terminal.scroll_viewport(ScrollViewport::Row(row));
    }
}

/// A keyboard scroll through the scrollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollTarget {
    /// Lines down, negative for up.
    Lines(isize),
    /// Screens down, negative for up.
    Pages(isize),
    Top,
    Bottom,
}

/// Wheel reports one event may send, so a fling can't flood the program.
const MAX_WHEEL_REPORTS: u32 = 12;
pub(super) const SCROLLBAR_WIDTH: Pixels = px(8.);

fn mouse_button(button: MouseButton) -> mouse::Button {
    match button {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Right => mouse::Button::Right,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Navigate(NavigationDirection::Back) => mouse::Button::Eight,
        MouseButton::Navigate(NavigationDirection::Forward) => mouse::Button::Nine,
    }
}

/// Lets a view follow drags of the scrollbar or of buttons the program took, and hear
/// their releases, wherever in the window the pointer goes.
pub(super) fn register_mouse_drag(owner: WeakEntity<TerminalView>, window: &mut Window) {
    let moves = owner.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
        if phase == DispatchPhase::Capture {
            let _ = moves.update(cx, |view, cx| view.drag(event, cx));
        }
    });
    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
        if phase == DispatchPhase::Capture {
            let _ = owner.update(cx, |view, cx| view.release(event, cx));
        }
    });
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
