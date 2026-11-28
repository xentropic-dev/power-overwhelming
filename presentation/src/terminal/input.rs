//! Mouse motion deduplication and wheel reporting adapted from herdr-app `src/vt.rs` and
//! `src/terminal.rs` (Apache-2.0).

use super::{
    OpenLink, TerminalView,
    grid::PtyGrid,
    keys::{self, paste_for},
    scroll,
    selection::{self, AUTOSCROLL_INTERVAL, Pointer, Surface},
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
        let had_selection = super::selection::clear(&self.terminal);
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

    /// Whether the program turned mouse tracking on.
    fn tracks_mouse(&self) -> bool {
        self.terminal.is_mouse_tracking().unwrap_or(false)
    }

    /// Whether the program takes the mouse buttons: it tracks the mouse and Shift isn't
    /// held, since Shift keeps the mouse for selecting.
    fn takes_mouse(&self, modifiers: Modifiers) -> bool {
        !modifiers.shift && self.tracks_mouse()
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
        // The find bar owns the keyboard while open. Its keys are handled by the workspace,
        // and any text left over arrives through the input method handler, which feeds the
        // query, so nothing typed here reaches the program.
        if self.finding() {
            return;
        }
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
        if let Some(copied) = selection::text(&self.terminal) {
            cx.write_to_clipboard(ClipboardItem::new_string(copied));
        }
    }

    /// Selects the whole screen and its scrollback.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        if selection::select_all(&self.terminal) {
            self.publish_selection(cx);
        }
        if self.refresh() {
            cx.notify();
        }
    }

    /// A finished selection goes to the clipboard with `copy_on_select`, and to the
    /// PRIMARY selection on Linux either way, for middle-click paste.
    fn publish_selection(&mut self, cx: &mut Context<Self>) {
        let Some(text) = selection::text(&self.terminal) else {
            return;
        };
        if self.settings.copy_on_select {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        cx.write_to_primary(ClipboardItem::new_string(text));
    }

    /// Pastes the PRIMARY selection on Linux; other platforms have none.
    fn paste_primary(&mut self, cx: &mut Context<Self>) {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(text) = cx
            .read_from_primary()
            .and_then(|item| item.text())
            .filter(|text| !text.is_empty())
        {
            let bytes = paste_for(&self.terminal, &text);
            self.keyboard_input(bytes, cx);
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let _ = cx;
    }

    fn select_pointer(&self, position: Point<Pixels>) -> Pointer {
        let local = position - self.grid.bounds.origin;
        Pointer {
            cell: self.at(position),
            x: f32::from(local.x),
            y: f32::from(local.y),
        }
    }

    fn surface(&self) -> Surface {
        Surface {
            cell_width: f32::from(self.grid.metrics.size.width),
            cell_height: f32::from(self.grid.metrics.size.height),
        }
    }

    /// Ends a selection gesture and copies what it selected.
    fn finish_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        self._autoscroll = None;
        self.select_pointer = None;
        let pointer = self.select_pointer(position);
        let selected = self.selector.release(&self.terminal, Some(pointer));
        if selected {
            self.publish_selection(cx);
        }
        self.refresh();
        cx.notify();
        selected
    }

    /// While a drag is held past the top or bottom edge, scrolls one row every
    /// `AUTOSCROLL_INTERVAL` (herdr-app `src/terminal.rs`, Apache-2.0).
    fn start_autoscroll(&mut self, cx: &mut Context<Self>) {
        self._autoscroll = Some(cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(AUTOSCROLL_INTERVAL).await;
                let keep = view
                    .update(cx, |view, cx| {
                        if !view.selector.selecting {
                            return false;
                        }
                        if let Some(position) = view.select_pointer {
                            let pointer = view.select_pointer(position);
                            let surface = view.surface();
                            if view
                                .selector
                                .autoscroll(&mut view.terminal, pointer, surface)
                            {
                                view.refresh();
                                cx.notify();
                            }
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
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
        let link = self
            .clickable_link_at(event.position, event.modifiers)
            .cloned();
        // A modifier-click on a link opens it even where the scrollbar overlaps it.
        if link.is_none() && self.grab_scrollbar(event.position) {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let cell = self.at(event.position);
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
        self.press_cell = Some(cell);
        self.select_pointer = Some(event.position);
        let pointer = self.select_pointer(event.position);
        self.selector
            .press(&self.terminal, pointer, event.modifiers.shift);
        self.start_autoscroll(cx);
        self.refresh();
        cx.notify();
    }

    /// Whether a button press is held, so moves count wherever the pointer goes.
    pub(super) fn pointer_held(&self) -> bool {
        !self.buttons.is_empty() || self.scrollbar_grab.is_some() || self.selector.selecting
    }

    /// Moves over the terminal while no button is held. Held moves come through the
    /// window's capture phase instead, so they keep arriving past the terminal's edges.
    pub(super) fn hover_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.pointer_held() {
            self.mouse_move(event, window, cx);
        }
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
        } else if self.selector.selecting {
            self.select_pointer = Some(event.position);
            let pointer = self.select_pointer(event.position);
            let surface = self.surface();
            self.selector.drag(&self.terminal, pointer, surface);
            if self.selector.dragged(&self.terminal) && self.pressed_link.take().is_some() {
                changed = true;
            }
            changed |= self.refresh();
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
        // GPUI keeps one pressed button, and forgets it when any button is released while
        // others are still held, so the drag reports the earliest button this view holds.
        let button = event
            .pressed_button
            .filter(|button| self.buttons.contains(button))
            .or_else(|| self.buttons.first().copied());
        if let Some(button) = button {
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
        } else if self.selector.selecting {
            let selected = self.finish_selection(event.position, cx);
            if !selected
                && let Some(pressed) = pressed
                && self
                    .clickable_link_at(event.position, event.modifiers)
                    .is_some_and(|link| *link == pressed)
            {
                cx.emit(OpenLink(pressed.to_string()));
            }
        }
        self.press_cell = None;
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
        if self.selector.selecting {
            self.finish_selection(event.position, cx);
        }
        self.press_cell = None;
        if changed {
            cx.notify();
        }
    }

    /// Middle-click reports to a program tracking the mouse, and otherwise pastes the
    /// PRIMARY selection on Linux.
    pub(super) fn middle_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window);
        // `report_button` latches the route at the press, so the release goes to the
        // program too, wherever it lands.
        if self.report_button(
            Some(true),
            MouseButton::Middle,
            event.position,
            event.modifiers,
            cx,
        ) {
            cx.stop_propagation();
        } else {
            self.paste_primary(cx);
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
        if self.tracks_mouse() {
            // Wheel reports go to the program whatever the modifiers, Shift included, as in
            // herdr-app. GPUI already turns Shift+wheel into horizontal steps.
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
        // Without tracking, Shift+wheel arrives horizontal from GPUI: scroll it vertically.
        let lines = if event.modifiers.shift && steps.y == 0 {
            steps.x
        } else {
            steps.y
        };
        if lines == 0 {
            return;
        }
        let up = lines > 0;
        let lines = lines.unsigned_abs() as usize;
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
        self.viewport_moved();
        cx.notify();
    }

    /// The viewport moved: a selection holds viewport cells, so it would now copy other
    /// text. Drop it, and any link press.
    fn viewport_moved(&mut self) {
        selection::clear(&self.terminal);
        if self.selector.selecting {
            self.selector.reset(&self.terminal);
            self._autoscroll = None;
            self.select_pointer = None;
        }
        self.pressed_link = None;
        self.refresh();
    }

    /// Scrolls the scrollback by keyboard command. False on the alternate screen, which
    /// has none, so the key can go to the program instead.
    pub fn scroll_history(&mut self, target: ScrollTarget, cx: &mut Context<Self>) -> bool {
        if self.alternate_screen() {
            return false;
        }
        let rows = self.grid.geometry().rows as isize;
        let scroll = match target {
            ScrollTarget::Lines(lines) => ScrollViewport::Delta(lines),
            ScrollTarget::Pages(pages) => ScrollViewport::Delta(pages * rows.max(1)),
            ScrollTarget::Top => ScrollViewport::Top,
            ScrollTarget::Bottom => ScrollViewport::Bottom,
        };
        self.scroll_viewport(scroll, cx);
        true
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
            self.viewport_moved();
        } else {
            self.refresh();
        }
        true
    }

    fn drag_scrollbar(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(grab), Some((track, _))) = (self.scrollbar_grab, self.scrollbar()) else {
            return;
        };
        let before = self.terminal.scrollbar().ok().map(|bar| bar.offset);
        self.place_thumb(track, f32::from(position.y - track.origin.y) - grab);
        if self.terminal.scrollbar().ok().map(|bar| bar.offset) != before {
            self.viewport_moved();
            cx.notify();
        } else if self.refresh() {
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
    // While a button is held, follow the pointer anywhere in the window, as herdr-app
    // does, so a drag past the terminal's edges keeps selecting and autoscrolling.
    let mover = pointer_owner.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
        if phase == DispatchPhase::Capture {
            let _ = mover.update(cx, |view, cx| {
                if view.pointer_held() {
                    view.mouse_move(event, window, cx);
                }
            });
        }
    });
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

/// Real GPUI dispatch into a laid-out view, with the PTY replaced by channels.
#[cfg(test)]
mod event_tests {
    use super::super::{InputHold, TerminalView};
    use crate::settings::TerminalSettings;
    use domain::session::{Agent, Session};
    use gpui::{
        AppContext, Context, Entity, IntoElement, Modifiers, MouseButton, NavigationDirection,
        ParentElement, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, Styled,
        TestAppContext, TouchPhase, VisualTestContext, Window, div, point, px,
    };
    use infrastructure::client::{TerminalControl, TerminalEvent};

    /// Lays the terminal out as the workspace does, in a flex column.
    struct Host(Entity<TerminalView>);

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().child(self.0.clone())
        }
    }

    struct Harness {
        view: Entity<TerminalView>,
        cx: VisualTestContext,
        sent: async_channel::Receiver<TerminalControl>,
        events: async_channel::Sender<TerminalEvent>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with(TerminalSettings::default())
        }

        fn with(settings: TerminalSettings) -> Self {
            let mut app = TestAppContext::single();
            let (commands, sent) = async_channel::unbounded();
            let (events, rx) = async_channel::unbounded();
            let window = app.add_window(|window, cx| {
                let view = cx.new(|cx| {
                    TerminalView::with_transport(
                        Session {
                            id: "test".into(),
                            worktree: "/".into(),
                            agent: Agent::Shell,
                        },
                        infrastructure::theme::default_scheme(),
                        settings,
                        InputHold::default(),
                        commands,
                        rx,
                        window,
                        cx,
                    )
                });
                Host(view)
            });
            let view = window.root(&mut app.clone()).unwrap();
            let view = app.read_entity(&view, |host, _| host.0.clone());
            let cx = VisualTestContext::from_window(window.into(), &app);
            let mut harness = Self {
                view,
                cx,
                sent,
                events,
            };
            harness.cx.run_until_parked();
            // Answer the layout's resize as the daemon does.
            while let Ok(control) = harness.sent.try_recv() {
                if let TerminalControl::Resize(geometry) = control {
                    harness
                        .events
                        .try_send(TerminalEvent::Geometry(geometry))
                        .unwrap();
                }
            }
            harness.cx.run_until_parked();
            harness.drain();
            harness
        }

        /// Feeds program output and lets the view draw it.
        fn output(&mut self, data: &str) {
            self.events
                .try_send(TerminalEvent::Output(data.as_bytes().to_vec()))
                .unwrap();
            self.cx.run_until_parked();
            self.view.update(&mut self.cx, |view, cx| {
                view.refresh();
                cx.notify();
            });
            self.cx.run_until_parked();
        }

        /// Input bytes sent to the program since the last call.
        fn drain(&mut self) -> String {
            let mut out = Vec::new();
            while let Ok(control) = self.sent.try_recv() {
                if let TerminalControl::Input(bytes) = control {
                    out.extend(bytes);
                }
            }
            String::from_utf8(out).unwrap()
        }

        /// The middle of a cell, zero-based.
        fn cell(&mut self, column: usize, row: usize) -> Point<Pixels> {
            self.view.read_with(&self.cx, |view, _| {
                let size = view.grid.metrics.size;
                view.grid.bounds.origin
                    + point(
                        size.width * (column as f32 + 0.5),
                        size.height * (row as f32 + 0.5),
                    )
            })
        }

        fn wheel(&mut self, at: Point<Pixels>, lines: Point<f32>, modifiers: Modifiers) {
            self.cx.simulate_event(ScrollWheelEvent {
                position: at,
                delta: ScrollDelta::Lines(lines),
                modifiers,
                touch_phase: TouchPhase::Moved,
            });
        }

        fn selection(&mut self) -> Option<String> {
            self.view
                .read_with(&self.cx, |view, _| super::selection::text(&view.terminal))
        }
    }

    const TRACK_ANY_SGR: &str = "\x1b[?1003h\x1b[?1006h";
    const SHIFT: Modifiers = Modifiers {
        shift: true,
        control: false,
        alt: false,
        platform: false,
        function: false,
    };

    fn history() -> String {
        (0..200).map(|n| format!("line {n}\r\n")).collect()
    }

    #[test]
    fn side_buttons_press_drag_and_release() {
        let mut h = Harness::new();
        h.output(TRACK_ANY_SGR);
        let (a, b) = (h.cell(1, 0), h.cell(3, 0));
        for (button, code) in [
            (MouseButton::Navigate(NavigationDirection::Back), 128),
            (MouseButton::Navigate(NavigationDirection::Forward), 129),
        ] {
            h.cx.simulate_mouse_down(a, button, Modifiers::none());
            h.cx.simulate_mouse_move(b, button, Modifiers::none());
            h.cx.simulate_mouse_up(b, button, Modifiers::none());
            assert_eq!(
                h.drain(),
                format!("\x1b[<{code};2;1M\x1b[<{};4;1M\x1b[<{code};4;1m", code + 32)
            );
        }
    }

    #[test]
    fn releasing_a_second_button_keeps_the_first_drag() {
        let mut h = Harness::new();
        h.output(TRACK_ANY_SGR);
        let (a, b, c) = (h.cell(0, 0), h.cell(2, 0), h.cell(4, 0));
        h.cx.simulate_mouse_down(a, MouseButton::Left, Modifiers::none());
        h.cx.simulate_mouse_down(a, MouseButton::Right, Modifiers::none());
        h.cx.simulate_mouse_up(b, MouseButton::Right, Modifiers::none());
        h.drain();
        // GPUI forgets the left button after the right one's release.
        h.cx.simulate_mouse_move(c, None, Modifiers::none());
        assert_eq!(h.drain(), "\x1b[<32;5;1M");
        h.cx.simulate_mouse_up(c, MouseButton::Left, Modifiers::none());
        assert_eq!(h.drain(), "\x1b[<0;5;1m");
    }

    #[test]
    fn wheel_reports_carry_modifiers_on_both_axes() {
        let mut h = Harness::new();
        h.output(TRACK_ANY_SGR);
        let at = h.cell(0, 0);
        h.wheel(at, point(0., 1.), Modifiers::none());
        assert_eq!(h.drain(), "\x1b[<64;1;1M");
        h.wheel(at, point(0., -1.), Modifiers::control());
        assert_eq!(h.drain(), "\x1b[<81;1;1M");
        h.wheel(at, point(1., 0.), Modifiers::none());
        assert_eq!(h.drain(), "\x1b[<66;1;1M");
        // GPUI turns Shift+wheel horizontal; the program hears it, with Shift.
        h.wheel(at, point(-1., 0.), SHIFT);
        assert_eq!(h.drain(), "\x1b[<71;1;1M");
    }

    #[test]
    fn shift_wheel_without_tracking_scrolls_the_scrollback() {
        let mut h = Harness::new();
        h.output(&history());
        let at = h.cell(0, 0);
        let offset = |h: &mut Harness| {
            h.view
                .read_with(&h.cx, |view, _| view.terminal.scrollbar().unwrap().offset)
        };
        let bottom = offset(&mut h);
        h.wheel(at, point(3., 0.), SHIFT);
        assert!(offset(&mut h) < bottom);
        assert_eq!(h.drain(), "");
    }

    #[test]
    fn scrolling_drops_the_selection() {
        let mut h = Harness::new();
        h.output(&history());
        let select = |h: &mut Harness| {
            let (a, b) = (h.cell(0, 0), h.cell(5, 0));
            h.cx.simulate_mouse_down(a, MouseButton::Left, Modifiers::none());
            h.cx.simulate_mouse_move(b, MouseButton::Left, Modifiers::none());
            h.cx.simulate_mouse_up(b, MouseButton::Left, Modifiers::none());
            assert!(h.selection().is_some());
        };
        select(&mut h);
        let at = h.cell(0, 0);
        h.wheel(at, point(0., 3.), Modifiers::none());
        assert_eq!(h.selection(), None);

        select(&mut h);
        h.view.update(&mut h.cx, |view, cx| {
            assert!(view.scroll_history(super::ScrollTarget::Top, cx));
        });
        assert_eq!(h.selection(), None);

        // A press on the scrollbar track away from the thumb.
        select(&mut h);
        let track = h
            .view
            .read_with(&h.cx, |view, _| view.scrollbar().unwrap().0);
        let on_track = point(
            track.center().x,
            track.origin.y + track.size.height - px(2.),
        );
        h.cx.simulate_mouse_down(on_track, MouseButton::Left, Modifiers::none());
        h.cx.simulate_mouse_up(on_track, MouseButton::Left, Modifiers::none());
        assert_eq!(h.selection(), None);

        // A drag of the thumb.
        select(&mut h);
        let thumb = h
            .view
            .read_with(&h.cx, |view, _| view.scrollbar().unwrap().1);
        let grab = thumb.center();
        h.cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
        assert!(h.selection().is_some());
        h.cx.simulate_mouse_move(
            point(grab.x, track.origin.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        h.cx.simulate_mouse_up(
            point(grab.x, track.origin.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(h.selection(), None);
    }

    #[test]
    fn scrollback_shortcuts_reach_the_program_on_the_alternate_screen() {
        let mut h = Harness::new();
        h.output("\x1b[?1049h");
        h.view.update(&mut h.cx, |view, cx| {
            assert!(!view.scroll_history(super::ScrollTarget::Pages(-1), cx));
            assert!(!view.scroll_history(super::ScrollTarget::Top, cx));
        });
        h.output("\x1b[?1049l");
        h.output(&history());
        h.view.update(&mut h.cx, |view, cx| {
            assert!(view.scroll_history(super::ScrollTarget::Pages(-1), cx));
        });
    }

    #[test]
    fn a_link_click_beats_the_scrollbar() {
        // No padding, so the bar lies over the last column.
        let mut h = Harness::with(TerminalSettings {
            padding: 0.,
            ..TerminalSettings::default()
        });
        let cols = h
            .view
            .read_with(&h.cx, |view, _| view.grid.geometry().cols as usize);
        let url = "https://example.com/";
        let text = format!("{}{}\r\n{}", " ".repeat(cols - url.len()), url, history());
        h.output(&text);
        h.view.update(&mut h.cx, |view, cx| {
            view.scroll_history(super::ScrollTarget::Top, cx);
        });
        h.cx.run_until_parked();
        let last = h.cell(cols - 1, 0);
        let last = h.view.read_with(&h.cx, |view, _| {
            let bounds = view.grid.bounds;
            let width = view.grid.metrics.size.width * cols as f32;
            point(bounds.origin.x + width - px(1.), last.y)
        });
        let track = h
            .view
            .read_with(&h.cx, |view, _| view.scrollbar().unwrap().0);
        assert!(track.contains(&last));
        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = opened.clone();
        h.cx.update(|_, cx| {
            cx.subscribe(&h.view, move |_, event: &super::OpenLink, _| {
                seen.borrow_mut().push(event.0.clone());
            })
            .detach();
        });
        h.cx.simulate_mouse_down(last, MouseButton::Left, Modifiers::control());
        h.cx.simulate_mouse_up(last, MouseButton::Left, Modifiers::control());
        assert_eq!(*opened.borrow(), [url.to_owned()]);
        assert!(
            h.view
                .read_with(&h.cx, |view, _| view.scrollbar_grab.is_none())
        );
    }
}
