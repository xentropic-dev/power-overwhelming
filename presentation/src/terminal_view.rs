use anyhow::{Result, bail};
use domain::{
    artifact::artifact_id,
    session::{Geometry, Session},
};
use ghostty_vt::{
    Terminal,
    key::{self, Key},
    mouse,
    render::{CellIterator, RenderState, RowIterator},
    snapshot,
    terminal::{Mode, Options, Point as VtPoint, PointCoordinate, ScrollViewport},
};
use gpui::{prelude::*, *};
use infrastructure::client::{self, TerminalControl, TerminalEvent};
use regex::Regex;
use std::{
    sync::{Arc, LazyLock},
    thread,
};

const WIDTH: f32 = 8.;
const HEIGHT: f32 = 18.;
const PAD: f32 = 6.;
const LINK_COLOR: u32 = 0x8fbaff;
const LINK_HOVER_COLOR: u32 = 0xc4ddff;
const LINK_PRESSED_COLOR: u32 = 0x6495e8;

pub struct OpenLink(pub String);
impl EventEmitter<OpenLink> for TerminalView {}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct CellPosition {
    row: usize,
    column: usize,
}

struct Cell {
    text: String,
    fg: Hsla,
    bg: Option<Hsla>,
    link: Option<Arc<str>>,
}

struct Row {
    cells: Vec<Cell>,
}

#[derive(Default)]
struct Frame {
    rows: Vec<Row>,
    cursor: Option<CellPosition>,
}

pub struct TerminalView {
    pub session: Session,
    pub focus: FocusHandle,
    font: Font,
    terminal: Terminal,
    renderer: RenderState,
    frame: Arc<Frame>,
    bounds: Bounds<Pixels>,
    selection: Option<(CellPosition, CellPosition)>,
    pointer: Option<Point<Pixels>>,
    pressed_link: Option<Arc<str>>,
    dragging: bool,
    reporting_mouse: bool,
    exited: bool,
    status: Option<String>,
    geometry: Option<Geometry>,
    encoder: key::Encoder,
    mouse: mouse::Encoder,
    commands: async_channel::Sender<TerminalControl>,
}

impl TerminalView {
    pub fn new(session: Session, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (commands, requests) = async_channel::bounded(128);
        let id = session.id.clone();
        thread::spawn(move || {
            while let Ok(request) = requests.recv_blocking() {
                if let Err(error) = client::control_terminal(&id, request) {
                    eprintln!("Terminal control failed. {error:#}");
                }
            }
        });
        let (tx, rx) = async_channel::bounded(128);
        let id = session.id.clone();
        thread::spawn(move || {
            client::follow_terminal(
                &id,
                || tx.is_closed(),
                |event| tx.send_blocking(event).is_ok(),
            );
        });
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(first) = rx.recv().await {
                if view
                    .update_in(cx, |view, _, cx| {
                        view.apply(first);
                        for _ in 0..127 {
                            let Ok(event) = rx.try_recv() else { break };
                            view.apply(event);
                        }
                        view.refresh();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let font_names = cx.text_system().all_font_names();
        let family = ["JetBrainsMono Nerd Font Mono", "JetBrains Mono"]
            .into_iter()
            .find(|family| font_names.iter().any(|name| name == family))
            .unwrap_or("monospace");
        Self {
            session,
            focus: cx.focus_handle(),
            font: font(family),
            terminal: Terminal::new(Options::default()).expect("allocate Ghostty terminal"),
            renderer: RenderState::new().expect("allocate render state"),
            frame: Arc::default(),
            bounds: Bounds::default(),
            selection: None,
            pointer: None,
            pressed_link: None,
            dragging: false,
            reporting_mouse: false,
            exited: false,
            status: None,
            geometry: None,
            encoder: key::Encoder::new().expect("allocate key encoder"),
            mouse: mouse::Encoder::new().expect("allocate mouse encoder"),
            commands,
        }
    }

    fn apply(&mut self, response: TerminalEvent) {
        let result = (|| -> Result<()> {
            match response {
                TerminalEvent::Snapshot { data, exited } => {
                    self.terminal = snapshot::Decoder::from_slice(&data).decode()?;
                    self.exited = exited;
                    self.status = None;
                    self.selection = None;
                }
                TerminalEvent::Output(data) => {
                    self.terminal.vt_write(&data);
                    self.terminal.events().for_each(drop);
                }
                TerminalEvent::Geometry(geometry) => {
                    self.terminal.resize(
                        geometry.cols,
                        geometry.rows,
                        geometry.cell_width.into(),
                        geometry.cell_height.into(),
                    )?;
                    self.terminal.events().for_each(drop);
                    self.selection = None;
                }
                TerminalEvent::Exit => self.exited = true,
                TerminalEvent::Status(message) => bail!("{message}"),
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.status = Some(error.to_string());
        }
    }

    fn refresh(&mut self) {
        match frame(&mut self.renderer, &self.terminal) {
            Ok(frame) => self.frame = Arc::new(frame),
            Err(error) => self.status = Some(error.to_string()),
        }
    }

    fn set_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.bounds = bounds;
        let geometry = Geometry {
            cols: (f32::from(bounds.size.width) / WIDTH).clamp(1., 500.) as u16,
            rows: (f32::from(bounds.size.height) / HEIGHT).clamp(1., 500.) as u16,
            cell_width: WIDTH as u16,
            cell_height: HEIGHT as u16,
        };
        if self.geometry != Some(geometry)
            && self
                .commands
                .try_send(TerminalControl::Resize(geometry))
                .is_ok()
        {
            self.geometry = Some(geometry);
        }
    }

    fn at(&self, position: Point<Pixels>) -> CellPosition {
        let local = position - self.bounds.origin;
        CellPosition {
            row: (f32::from(local.y) / HEIGHT).max(0.) as usize,
            column: (f32::from(local.x) / WIDTH).max(0.) as usize,
        }
    }

    fn link_at(&self, cell: CellPosition) -> Option<&Arc<str>> {
        self.frame
            .rows
            .get(cell.row)?
            .cells
            .get(cell.column)?
            .link
            .as_ref()
    }

    fn link_at_point(&self, point: Point<Pixels>) -> Option<&Arc<str>> {
        if !self.bounds.contains(&point) {
            return None;
        }
        self.link_at(self.at(point))
    }

    fn hovered_link(&self) -> Option<&Arc<str>> {
        self.link_at_point(self.pointer?)
    }

    fn set_pointer(&mut self, position: Option<Point<Pixels>>) -> bool {
        let previous = self.hovered_link().cloned();
        self.pointer = position;
        match (previous.as_ref(), self.hovered_link()) {
            (None, None) => false,
            (Some(previous), Some(current)) => !Arc::ptr_eq(previous, current),
            _ => true,
        }
    }

    fn clear_pointer(&mut self) -> bool {
        let moved_off_link = self.set_pointer(None);
        let released_link = self.pressed_link.take().is_some();
        moved_off_link || released_link
    }

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

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        if stroke.modifiers.control && stroke.modifiers.shift {
            match stroke.key.as_str() {
                "c" => {
                    if let Some((start, end)) = self.selection {
                        let (start, end) = ordered(start, end);
                        let mut copied = String::new();
                        for row in start.row..=end.row {
                            if row > start.row {
                                copied.push('\n');
                            }
                            if let Some(line) = self.frame.rows.get(row) {
                                let left = if row == start.row { start.column } else { 0 };
                                let right = if row == end.row {
                                    end.column.saturating_add(1)
                                } else {
                                    line.cells.len()
                                };
                                let text: String = line
                                    .cells
                                    .iter()
                                    .skip(left)
                                    .take(right.saturating_sub(left))
                                    .map(|cell| cell.text.as_str())
                                    .collect();
                                copied.push_str(text.trim_end());
                            }
                        }
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
}

fn terminal_modifiers(modifiers: Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    mods.set(key::Mods::SHIFT, modifiers.shift);
    mods.set(key::Mods::CTRL, modifiers.control);
    mods.set(key::Mods::ALT, modifiers.alt);
    mods.set(key::Mods::SUPER, modifiers.platform);
    mods
}

fn frame(renderer: &mut RenderState, terminal: &Terminal) -> Result<Frame> {
    let state = renderer.update(terminal)?;
    let cursor = state
        .cursor_viewport()?
        .filter(|_| state.cursor_visible().unwrap_or(false))
        .map(|cursor| CellPosition {
            row: cursor.y.into(),
            column: cursor.x.into(),
        });
    let mut row_iter = RowIterator::new()?;
    let mut cell_iter = CellIterator::new()?;
    let mut source = row_iter.update(&state)?;
    let mut rows = Vec::new();
    let mut osc_link: Option<Arc<str>> = None;
    let mut uri_buffer = [0; 8192];
    while let Some(row) = source.next() {
        if !row.raw_row()?.is_wrap_continuation()? {
            osc_link = None;
        }
        let y = rows.len() as u32;
        let mut source = cell_iter.update(row)?;
        let mut cells = Vec::new();
        let mut text = String::new();
        let mut offsets = Vec::new();
        while let Some(cell) = source.next() {
            let mut grapheme = String::new();
            cell.graphemes_utf8(&mut grapheme)?;
            if grapheme.is_empty() {
                grapheme.push(' ');
            }
            offsets.push(text.len());
            text.push_str(&grapheme);
            let osc_uri = if cell.raw_cell()?.has_hyperlink()? {
                terminal
                    .grid_ref(VtPoint::Viewport(PointCoordinate {
                        x: cells.len() as u16,
                        y,
                    }))
                    .ok()
                    .and_then(|reference| reference.hyperlink_uri(&mut uri_buffer).ok())
                    .and_then(|count| std::str::from_utf8(&uri_buffer[..count]).ok())
                    .filter(|uri| candidate(uri))
            } else {
                None
            };
            osc_link = osc_uri.map(|uri| {
                osc_link
                    .take()
                    .filter(|previous| previous.as_ref() == uri)
                    .unwrap_or_else(|| Arc::from(uri))
            });
            cells.push(Cell {
                text: grapheme,
                fg: cell
                    .fg_color()?
                    .map_or(rgb(0xdbe3ee).into(), |c| rgba_color(c.r, c.g, c.b)),
                bg: cell.bg_color()?.map(|c| rgba_color(c.r, c.g, c.b)),
                link: osc_link.clone(),
            });
        }
        for (range, uri) in links(&text) {
            let start = offsets.partition_point(|offset| *offset < range.start);
            let end = offsets.partition_point(|offset| *offset < range.end);
            let link = Arc::from(uri);
            for cell in &mut cells[start..end] {
                if cell.link.is_none() {
                    cell.link = Some(Arc::clone(&link));
                }
            }
        }
        rows.push(Row { cells });
    }
    Ok(Frame { rows, cursor })
}

static MARKDOWN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\[[^\]]+\]\(([^)]+)\)|"([^"]+)"|'([^']+)'|`([^`]+)`"#).unwrap());
static PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)(?:/|\.{1,2}/)[^<>\[\]()"'`\r\n]*?\.(?:png|jpe?g|webp|mp4|webm|html?)\b|[^\s<>\[\]()"'`,;]+"#,
).unwrap()
});

fn candidate(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    artifact_id(&value).is_some()
        || value.starts_with("file://")
        || value.starts_with("http://localhost")
        || value.starts_with("https://localhost")
        || value.starts_with("http://127.")
        || value.starts_with("https://127.")
        || value.starts_with("http://[::1]")
        || value.starts_with("https://[::1]")
        || [
            ".png", ".jpg", ".jpeg", ".webp", ".mp4", ".webm", ".html", ".htm",
        ]
        .iter()
        .any(|extension| value.ends_with(extension))
}

fn links(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut result: Vec<_> = MARKDOWN
        .captures_iter(text)
        .filter_map(|captures| {
            let source = (1..=4).find_map(|index| captures.get(index))?;
            let target = source.as_str().trim().trim_matches(['<', '>']);
            candidate(target).then(|| (captures.get(0).unwrap().range(), target.to_owned()))
        })
        .collect();
    for token in PATH.find_iter(text) {
        if result
            .iter()
            .any(|(range, _)| range.contains(&token.start()))
        {
            continue;
        }
        let target = token.as_str().trim_end_matches(['.', ':']);
        if candidate(target) {
            result.push((
                token.start()..token.start() + target.len(),
                target.to_owned(),
            ));
        }
    }
    result
}

fn ordered(a: CellPosition, b: CellPosition) -> (CellPosition, CellPosition) {
    if a <= b { (a, b) } else { (b, a) }
}

fn rgba_color(r: u8, g: u8, b: u8) -> Hsla {
    rgb(u32::from_be_bytes([0, r, g, b])).into()
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

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = cx.entity();
        let pointer_owner = terminal.downgrade();
        let frame = self.frame.clone();
        let terminal_font = self.font.clone();
        let hovered_link = self.hovered_link().cloned();
        let pressed_link = self.pressed_link.as_ref().and_then(|pressed| {
            let (start, end) = self.selection?;
            let link = self.link_at(start)?;
            (start == end && pressed == link).then(|| link.clone())
        });
        let selection = self
            .selection
            .filter(|_| pressed_link.is_none())
            .map(|(a, b)| ordered(a, b));
        div()
            .id("terminal")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(0x101722))
            .when(hovered_link.is_some(), |terminal| terminal.cursor_pointer())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_hover(cx.listener(|view, hovered: &bool, window, cx| {
                let changed = if *hovered {
                    view.set_pointer(Some(window.mouse_position()))
                } else {
                    view.clear_pointer()
                };
                if changed {
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    view.focus.focus(window);
                    view.set_pointer(Some(event.position));
                    let cell = view.at(event.position);
                    let link = view.link_at_point(event.position).cloned();
                    if !event.modifiers.shift
                        && view.terminal.is_mouse_tracking().unwrap_or(false)
                        && link.is_none()
                    {
                        view.reporting_mouse = true;
                        view.mouse_input(
                            event.position,
                            event.modifiers,
                            mouse::Action::Press,
                            mouse::Button::Left,
                            cx,
                        );
                        return;
                    }
                    view.pressed_link = link.filter(|_| !event.modifiers.shift);
                    view.selection = Some((cell, cell));
                    view.dragging = true;
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, cx| {
                let mut changed = view.set_pointer(Some(event.position));
                if view.reporting_mouse {
                    view.mouse_input(
                        event.position,
                        event.modifiers,
                        mouse::Action::Motion,
                        mouse::Button::Left,
                        cx,
                    );
                } else if view.dragging
                    && let Some((anchor, previous)) = view.selection
                {
                    let cell = view.at(event.position);
                    if previous != cell {
                        view.pressed_link = None;
                        view.selection = Some((anchor, cell));
                        changed = true;
                    }
                }
                if changed {
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, _, cx| {
                    view.set_pointer(Some(event.position));
                    let pressed = view.pressed_link.take();
                    if view.reporting_mouse {
                        view.mouse_input(
                            event.position,
                            event.modifiers,
                            mouse::Action::Release,
                            mouse::Button::Left,
                            cx,
                        );
                        view.reporting_mouse = false;
                    } else if view.dragging {
                        let cell = view.at(event.position);
                        if view.selection == Some((cell, cell)) {
                            view.selection = None;
                            if let Some(pressed) = pressed
                                && view
                                    .link_at_point(event.position)
                                    .is_some_and(|link| *link == pressed)
                            {
                                cx.emit(OpenLink(pressed.to_string()));
                            }
                        }
                        view.dragging = false;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, _, cx| {
                    let changed = view.clear_pointer();
                    if view.reporting_mouse {
                        view.mouse_input(
                            event.position,
                            event.modifiers,
                            mouse::Action::Release,
                            mouse::Button::Left,
                            cx,
                        );
                        view.reporting_mouse = false;
                    }
                    view.dragging = false;
                    if changed {
                        cx.notify();
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|view, event: &ScrollWheelEvent, _, cx| {
                let amount =
                    -(f32::from(event.delta.pixel_delta(px(HEIGHT)).y) / HEIGHT).round() as isize;
                if amount != 0 {
                    if !event.modifiers.shift && view.terminal.is_mouse_tracking().unwrap_or(false)
                    {
                        let button = if amount < 0 {
                            mouse::Button::Four
                        } else {
                            mouse::Button::Five
                        };
                        for _ in 0..amount.unsigned_abs().min(12) {
                            view.mouse_input(
                                event.position,
                                event.modifiers,
                                mouse::Action::Press,
                                button,
                                cx,
                            );
                        }
                        return;
                    }
                    view.terminal.scroll_viewport(ScrollViewport::Delta(amount));
                    view.selection = None;
                    view.pressed_link = None;
                    view.refresh();
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        terminal.update(cx, |terminal, _| terminal.set_bounds(bounds));
                    },
                    move |bounds, (), window, cx| {
                        let pointer_owner = pointer_owner.clone();
                        window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble {
                                let _ = pointer_owner.update(cx, |view, cx| {
                                    if view.clear_pointer() {
                                        cx.notify();
                                    }
                                });
                            }
                        });
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            for (row, line) in frame.rows.iter().enumerate() {
                                for (column, cell) in line.cells.iter().enumerate() {
                                    let position = CellPosition { row, column };
                                    let origin = bounds.origin
                                        + point(px(column as f32 * WIDTH), px(row as f32 * HEIGHT));
                                    let selected = selection.is_some_and(|(start, end)| {
                                        start <= position && position <= end
                                    });
                                    let cursor = frame.cursor == Some(position);
                                    if let Some(color) = if selected {
                                        Some(rgb(0x304b6f).into())
                                    } else {
                                        cell.bg
                                    } {
                                        window.paint_quad(fill(
                                            Bounds::new(origin, size(px(WIDTH), px(HEIGHT))),
                                            color,
                                        ));
                                    }
                                    if cursor {
                                        let bar = Bounds::new(
                                            origin + point(px(0.), px(HEIGHT - 2.)),
                                            size(px(WIDTH), px(2.)),
                                        );
                                        window.paint_quad(fill(bar, rgb(0x8fbaff)));
                                    }
                                    let mut fg = cell.fg;
                                    if let Some(link) = &cell.link {
                                        let color = if pressed_link
                                            .as_ref()
                                            .is_some_and(|pressed| Arc::ptr_eq(link, pressed))
                                        {
                                            LINK_PRESSED_COLOR
                                        } else if hovered_link
                                            .as_ref()
                                            .is_some_and(|hovered| Arc::ptr_eq(link, hovered))
                                        {
                                            LINK_HOVER_COLOR
                                        } else {
                                            LINK_COLOR
                                        };
                                        fg = rgb(color).into();
                                        let underline = Bounds::new(
                                            origin + point(px(0.), px(HEIGHT - 3.)),
                                            size(px(WIDTH), px(1.)),
                                        );
                                        window.paint_quad(fill(underline, fg));
                                    }
                                    if cell.text.trim().is_empty() {
                                        continue;
                                    }
                                    let run = TextRun {
                                        len: cell.text.len(),
                                        font: terminal_font.clone(),
                                        color: fg,
                                        background_color: None,
                                        underline: None,
                                        strikethrough: None,
                                    };
                                    let shaped = window.text_system().shape_line(
                                        cell.text.clone().into(),
                                        px(13.),
                                        &[run],
                                        None,
                                    );
                                    let _ = shaped.paint(origin, px(HEIGHT), window, cx);
                                }
                            }
                        });
                    },
                )
                .absolute()
                .top(px(PAD))
                .left(px(PAD))
                .right(px(PAD))
                .bottom(px(PAD)),
            )
            .when_some(
                self.status
                    .clone()
                    .or_else(|| self.exited.then(|| "Process exited".into())),
                |this, status| {
                    this.child(
                        div()
                            .absolute()
                            .bottom_1()
                            .right_2()
                            .bg(rgb(0x26354b))
                            .px_2()
                            .text_xs()
                            .child(status),
                    )
                },
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{RenderState, Terminal, frame, links};
    use ghostty_vt::terminal::Options;
    use std::sync::Arc;

    #[test]
    fn detects_agent_paths_and_artifact_tokens() {
        let text = "Video [preview](./out/my clip.mp4) pwr://artifact/a power://artifact/b https://localhost:4000/app";
        let detected: Vec<_> = links(text).into_iter().map(|(_, value)| value).collect();
        assert_eq!(
            detected,
            [
                "./out/my clip.mp4",
                "pwr://artifact/a",
                "power://artifact/b",
                "https://localhost:4000/app"
            ]
        );
        assert_eq!(links("Saved /tmp/my clip.mp4.")[0].1, "/tmp/my clip.mp4");
    }

    #[test]
    fn clickable_cells_share_their_link_span() {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(
            b"\x1b]8;;pwr://artifact/one\x1b\\First link\x1b]8;;\x1b\\ pwr://artifact/two \x1b]8;;https://example.com\x1b\\Remote\x1b]8;;\x1b\\",
        );
        let frame = frame(&mut RenderState::new().unwrap(), &terminal).unwrap();
        let cells = &frame.rows[0].cells;
        let first = cells[0].link.as_ref().unwrap();
        let second = cells[11].link.as_ref().unwrap();
        assert_eq!(first.as_ref(), "pwr://artifact/one");
        assert!(Arc::ptr_eq(first, cells[9].link.as_ref().unwrap()));
        assert!(cells[10].link.is_none());
        assert_eq!(second.as_ref(), "pwr://artifact/two");
        assert!(Arc::ptr_eq(second, cells[28].link.as_ref().unwrap()));
        assert!(cells[30..36].iter().all(|cell| cell.link.is_none()));
    }
}
