//! Output redraw scheduling and terminal event handling adapted from herdr-app
//! `src/terminal.rs` (Apache-2.0).

pub(crate) mod bench;
mod frame;
mod grid;
mod hold;
mod images;
mod ime;
mod input;
mod keys;
mod links;
mod metrics;
mod paint;
mod runs;
mod scroll;
mod selection;
mod sprites;
mod typing;

pub(crate) use self::links::{LinkAction, link_action};
#[cfg(test)]
pub(crate) use self::metrics::has_family;
use self::metrics::preload_font_files;
pub use self::metrics::{CellMetrics, TerminalFont, cell_metrics, column_width};

use self::frame::{CellPosition, Frame};
use self::grid::PtyGrid;
pub use self::input::ScrollTarget;
pub use self::typing::InputHold;
use self::typing::Typing;
use crate::settings::TerminalSettings;
use anyhow::{Result, bail};
use domain::session::{Agent, Session};
use ghostty_vt::{
    Terminal, mouse,
    render::RenderState,
    snapshot,
    terminal::{self, ClipboardLocation, Event as VtEvent, Mode, Options},
};
use gpui::*;
use infrastructure::{
    client::{self, SessionMetadata, TerminalControl, TerminalEvent},
    theme::Scheme,
};
use std::{
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const MAX_CLIPBOARD_WRITE: usize = 1024 * 1024;
const MAX_RENDER_HOLD: Duration = Duration::from_secs(1);

/// JetBrains Mono, shipped so the terminal's default font resolves on every machine. A family
/// GPUI can't find resolves to its UI fallback (Noto Sans and the like) with default features,
/// which are proportional and join `ffi` whatever `terminal.ligatures` says.
pub(crate) const BUNDLED_FONT: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");
/// The real bold and italic faces: GPUI doesn't apply variable-font axes, so a variable font
/// would draw bold and italic text exactly like regular.
const BUNDLED_FONTS: [&[u8]; 4] = [
    BUNDLED_FONT,
    include_bytes!("../../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../../assets/fonts/JetBrainsMono-Italic.ttf"),
    include_bytes!("../../assets/fonts/JetBrainsMono-BoldItalic.ttf"),
];
/// Symbols Nerd Font, shipped so Nerd Font icons the terminal font lacks still draw.
pub(crate) const SYMBOLS_FONT: &[u8] =
    include_bytes!("../../assets/fonts/SymbolsNerdFont-Regular.ttf");
const BUNDLED_FAMILY: &str = "JetBrains Mono";

/// Adds the bundled font to GPUI's text system; call once before any terminal is drawn.
pub fn load_bundled_fonts(cx: &App) {
    let mut fonts = BUNDLED_FONTS.map(std::borrow::Cow::Borrowed).to_vec();
    fonts.push(std::borrow::Cow::Borrowed(SYMBOLS_FONT));
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        eprintln!("Could not load the bundled terminal font. {error:#}");
    }
}

pub(crate) fn terminal_font_family(cx: &App) -> &'static str {
    let fonts = cx.text_system().all_font_names();
    ["JetBrainsMono Nerd Font Mono"]
        .into_iter()
        .find(|family| fonts.iter().any(|name| name == family))
        .unwrap_or(BUNDLED_FAMILY)
}

/// Whether the platform has a face of `family`, asked of fontdb, the database GPUI's Linux text
/// system loads. Resolving a family it has never reaches GPUI's fallback stack, which panics
/// when none of those faces are installed either, and `all_font_names` lists that stack's
/// names whether or not they're installed.
fn installed(family: &str) -> bool {
    metrics::has_family(family)
}

/// The terminal font for `settings`: its `font_family` when the platform has it, else the
/// default, so the features set here reach the face that shapes every run. The bundled font is
/// named exactly, so it resolves from `load_bundled_fonts` with no fallback face installed.
pub fn terminal_font(settings: &TerminalSettings, cx: &App) -> TerminalFont {
    let family: SharedString = match &settings.font_family {
        Some(family) if installed(family) => family.clone().into(),
        Some(_) => BUNDLED_FAMILY.into(),
        None => terminal_font_family(cx).into(),
    };
    let mut font = font(family);
    font.weight = FontWeight(f32::from(settings.font_weight));
    font.features = runs::ligature_features(false);
    TerminalFont {
        font,
        size: settings.font_size,
        line_height: settings.line_height,
        cell_width: settings.cell_width,
    }
}

pub struct OpenLink(pub String);
impl EventEmitter<OpenLink> for TerminalView {}

pub struct AgentChanged(pub Agent);
impl EventEmitter<AgentChanged> for TerminalView {}

pub struct BarContextChanged;
impl EventEmitter<BarContextChanged> for TerminalView {}

pub struct TerminalTitleChanged(pub String);
impl EventEmitter<TerminalTitleChanged> for TerminalView {}

pub struct TerminalBell;
impl EventEmitter<TerminalBell> for TerminalView {}

pub struct TerminalNotification {
    pub title: String,
    pub body: String,
}
impl EventEmitter<TerminalNotification> for TerminalView {}

/// All terminals with pending output share one GPUI timer.
#[derive(Default)]
struct OutputClock(Vec<WeakEntity<TerminalView>>);
impl Global for OutputClock {}

const OUTPUT_TURN: Duration = Duration::from_millis(6);
const OUTPUT_INTERVAL: Duration = Duration::from_millis(33);
const TYPING_INTERVAL: Duration = Duration::from_millis(200);

pub struct TerminalView {
    pub session: Session,
    pub focus: FocusHandle,
    pub metadata: SessionMetadata,
    scheme: &'static Scheme,
    settings: TerminalSettings,
    font: TerminalFont,
    grid: PtyGrid,
    terminal: Terminal,
    renderer: RenderState,
    frame: Rc<Frame>,
    typed_at: Option<Instant>,
    paint_revision: u64,
    /// The terminal's selection in viewport cells, as last painted.
    selection: Option<(CellPosition, CellPosition)>,
    selector: selection::Selector,
    /// The pointer while a selection drag runs, for autoscroll ticks.
    select_pointer: Option<Point<Pixels>>,
    _autoscroll: Option<Task<()>>,
    /// Where the press on `pressed_link` landed.
    press_cell: Option<CellPosition>,
    pointer: Option<Point<Pixels>>,
    pressed_link: Option<Arc<str>>,
    /// Numbered link labels shown while the user picks a link by keyboard.
    hints: Vec<(CellPosition, Arc<str>)>,
    hint_footer: Option<String>,
    /// Buttons whose press went to the program, so their motion and release go too.
    buttons: Vec<MouseButton>,
    /// The last mouse report sent, as motion, so motion within one cell isn't repeated.
    last_mouse_report: Vec<u8>,
    /// Fractions of a line or column the wheel moved that haven't scrolled yet.
    scrolled: Point<f32>,
    /// Where in the scrollbar thumb a drag holds it, while one is in progress.
    scrollbar_grab: Option<f32>,
    exited: bool,
    status: Option<String>,
    render_held: bool,
    render_hold_generation: u64,
    /// The frame captured inside `vt_write` when synchronized output last began.
    held_frame: hold::Captured,
    contrast: Rc<std::cell::Cell<f32>>,
    /// Kitty images uploaded for painting, keyed by generation.
    images: Rc<std::cell::RefCell<images::ImageCache>>,
    mouse: mouse::Encoder,
    /// Keys, input methods and focus as the program hears them.
    typing: Typing,
    commands: async_channel::Sender<TerminalControl>,
    _follow: Task<()>,
    _focus_changes: [Subscription; 3],
    _release: Subscription,
}

impl TerminalView {
    pub fn new(
        session: Session,
        scheme: &'static Scheme,
        settings: TerminalSettings,
        hold: InputHold,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        static PRELOAD: std::sync::Once = std::sync::Once::new();
        PRELOAD.call_once(preload_font_files);
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
        Self::with_transport(session, scheme, settings, hold, commands, rx, window, cx)
    }

    /// Builds the view over a given command sink and event source, starting no
    /// daemon-client threads.
    #[allow(clippy::too_many_arguments)]
    fn with_transport(
        session: Session,
        scheme: &'static Scheme,
        settings: TerminalSettings,
        hold: InputHold,
        commands: async_channel::Sender<TerminalControl>,
        rx: async_channel::Receiver<TerminalEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let follow = cx.spawn_in(window, async move |view, cx| {
            while let Ok(first) = rx.recv().await {
                let deadline = Instant::now() + OUTPUT_TURN;
                let queued = std::iter::from_fn(|| {
                    if Instant::now() < deadline {
                        rx.try_recv().ok()
                    } else {
                        None
                    }
                });
                if view
                    .update_in(cx, |view, _, cx| {
                        view.ingest(std::iter::once(first).chain(queued), cx);
                    })
                    .is_err()
                {
                    break;
                }
                if !rx.is_empty() {
                    // A ready receiver can otherwise keep GPUI from running the
                    // redraw timer or delivering keystrokes during a large cat.
                    cx.foreground_executor().spawn(async {}).await;
                }
            }
        });
        let focus = cx.focus_handle();
        let focus_changes = [
            cx.on_focus(&focus, window, |view, window, cx| {
                view.sync_focus(window, cx)
            }),
            cx.on_blur(&focus, window, |view, window, cx| {
                view.sync_focus(window, cx)
            }),
            cx.observe_window_activation(window, |view, window, cx| view.sync_focus(window, cx)),
        ];
        let mut terminal = Terminal::new(Options::default()).expect("allocate Ghostty terminal");
        scheme
            .apply_to(&mut terminal)
            .expect("apply bundled terminal palette");
        let held_frame = hold::Captured::default();
        let contrast = Rc::new(std::cell::Cell::new(settings.minimum_contrast));
        let images = Rc::<std::cell::RefCell<images::ImageCache>>::default();
        configure(&mut terminal, &settings, &held_frame, &contrast, &images);
        // Detaching or ending a tab drops the view without another paint; its uploads
        // would otherwise stay in the window's atlas.
        let release = cx.on_release(|view: &mut Self, cx| {
            for image in view.images.borrow_mut().drain() {
                cx.drop_image(image, None);
            }
        });
        Self {
            session,
            focus,
            metadata: SessionMetadata::default(),
            scheme,
            font: terminal_font(&settings, cx),
            settings,
            grid: PtyGrid::default(),
            terminal,
            renderer: RenderState::new().expect("allocate render state"),
            frame: Rc::default(),
            typed_at: None,
            paint_revision: 0,
            selection: None,
            selector: selection::Selector::new(),
            select_pointer: None,
            _autoscroll: None,
            press_cell: None,
            pointer: None,
            pressed_link: None,
            hints: Vec::new(),
            hint_footer: None,
            buttons: Vec::new(),
            last_mouse_report: Vec::new(),
            scrolled: Point::default(),
            scrollbar_grab: None,
            exited: false,
            status: None,
            render_held: false,
            render_hold_generation: 0,
            held_frame,
            contrast,
            images,
            mouse: mouse::Encoder::new().expect("allocate mouse encoder"),
            typing: Typing::new(hold),
            commands,
            _follow: follow,
            _focus_changes: focus_changes,
            _release: release,
        }
    }

    pub fn set_scheme(&mut self, scheme: &'static Scheme, cx: &mut Context<Self>) {
        self.scheme = scheme;
        self.paint_revision += 1;
        if let Err(error) = scheme.apply_to(&mut self.terminal) {
            self.status = Some(format!("Could not apply color scheme. {error:#}"));
        }
        self.refresh();
        cx.notify();
    }

    /// Applies new terminal settings, with zoom already in `font_size`. A new cell size
    /// resizes the PTY now, whether or not this terminal is on screen.
    pub fn set_settings(&mut self, settings: TerminalSettings, cx: &mut Context<Self>) {
        if settings == self.settings {
            return;
        }
        self.paint_revision += 1;
        self.font = terminal_font(&settings, cx);
        self.terminal
            .set_clipboard_write_policy(clipboard_policy(&settings));
        self.contrast.set(settings.minimum_contrast);
        self.settings = settings;
        if !self.exited {
            let metrics = cell_metrics(cx.text_system(), &self.font, self.grid.metrics.scale);
            self.grid.remeasure(metrics, &self.commands);
        }
        self.refresh();
        cx.notify();
    }

    pub fn reported_directory(&self) -> Option<PathBuf> {
        let pwd = self.terminal.pwd().ok()?;
        if pwd.starts_with('/') {
            Some(PathBuf::from(pwd))
        } else {
            url::Url::parse(pwd).ok()?.to_file_path().ok()
        }
    }

    fn ingest(&mut self, batch: impl Iterator<Item = TerminalEvent>, cx: &mut Context<Self>) {
        let agent = self.session.agent.clone();
        let metadata = self.metadata.clone();
        let status = self.status.clone();
        let exited = self.exited;
        let selection = self.selection;
        let mut output = false;
        let mut refresh_now = false;
        for event in batch {
            match event {
                TerminalEvent::Output(_) => output = true,
                TerminalEvent::Snapshot { .. } | TerminalEvent::Geometry(_) => refresh_now = true,
                _ => {}
            }
            self.apply(event, cx);
        }
        if self.session.agent != agent {
            cx.emit(AgentChanged(self.session.agent.clone()));
        }
        if self.session.agent != agent || self.metadata != metadata {
            cx.emit(BarContextChanged);
        }
        let overlay_changed =
            self.exited != exited || self.status != status || self.selection != selection;
        if refresh_now || overlay_changed {
            if self.refresh() || overlay_changed {
                cx.notify();
            }
        } else if output {
            self.redraw_output(cx);
        }
    }

    fn apply(&mut self, response: TerminalEvent, cx: &mut Context<Self>) {
        let result = (|| -> Result<()> {
            match response {
                TerminalEvent::Snapshot { data, exited } => {
                    let mut terminal = snapshot::Decoder::from_slice(&data).decode()?;
                    self.scheme.apply_to(&mut terminal)?;
                    self.configure_terminal(&mut terminal);
                    let title = terminal.title()?.to_owned();
                    self.renderer = RenderState::new()?;
                    self.frame = Rc::default();
                    let previous = std::mem::replace(&mut self.terminal, terminal);
                    if let Some(bytes) = self.typing.installed(&previous, &self.terminal) {
                        self.send(bytes);
                    }
                    self.exited = exited;
                    self.status = None;
                    // The gesture tracks cells of the terminal it replaced.
                    self.selector.reset(&previous);
                    self.metadata = SessionMetadata::default();
                    let title = shown_title(&title).into_owned();
                    cx.emit(TerminalTitleChanged(title));
                }
                TerminalEvent::Output(data) => {
                    self.terminal.vt_write(&data);
                    self.handle_vt_events(cx);
                }
                TerminalEvent::Geometry(geometry) => {
                    self.terminal.resize(
                        geometry.cols,
                        geometry.rows,
                        geometry.cell_width.into(),
                        geometry.cell_height.into(),
                    )?;
                    self.handle_vt_events(cx);
                    self.selection = None;
                }
                TerminalEvent::Exit => {
                    self.exited = true;
                    self.metadata = SessionMetadata::default();
                }
                TerminalEvent::Agent(agent) => self.session.agent = agent,
                TerminalEvent::Metadata(metadata) => self.metadata = metadata,
                TerminalEvent::Status(message) => bail!("{message}"),
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.status = Some(error.to_string());
        }
    }

    fn handle_vt_events(&mut self, cx: &mut Context<Self>) {
        for event in self.terminal.events() {
            match event {
                VtEvent::Bell => cx.emit(TerminalBell),
                VtEvent::TitleChanged(title) => {
                    let title = shown_title(&title).into_owned();
                    cx.emit(TerminalTitleChanged(title));
                }
                VtEvent::ClipboardWrite(write) => {
                    let Some(text) = clipboard_text(&write.contents) else {
                        continue;
                    };
                    match write.location {
                        ClipboardLocation::Standard => {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                        ClipboardLocation::Primary | ClipboardLocation::Selection => {
                            #[cfg(target_os = "linux")]
                            cx.write_to_primary(ClipboardItem::new_string(text));
                            #[cfg(not(target_os = "linux"))]
                            let _ = text;
                        }
                    }
                }
                VtEvent::DesktopNotification { title, body } => {
                    cx.emit(TerminalNotification { title, body });
                }
                VtEvent::RenderHold(held) => {
                    if held && let Some((renderer, frame)) = self.held_frame.borrow_mut().take() {
                        // Show the screen as it stood when the hold began, which may be
                        // newer than the last paint.
                        self.renderer = renderer;
                        self.frame = Rc::new(frame);
                        cx.notify();
                    }
                    self.render_held = held;
                    self.render_hold_generation = self.render_hold_generation.wrapping_add(1);
                    if held {
                        let generation = self.render_hold_generation;
                        cx.spawn(async move |view, cx| {
                            cx.background_executor().timer(MAX_RENDER_HOLD).await;
                            let _ = view.update(cx, |view, cx| {
                                if view.render_held && view.render_hold_generation == generation {
                                    let _ = view.terminal.set_mode(Mode::SYNC_OUTPUT, false);
                                    view.handle_vt_events(cx);
                                    view.render_held = false;
                                    view.refresh();
                                    cx.notify();
                                }
                            });
                        })
                        .detach();
                    }
                }
                VtEvent::PtyWrite(_) | VtEvent::PwdChanged(_) | VtEvent::ProgressReport { .. } => {}
            }
        }
    }

    fn refresh(&mut self) -> bool {
        if self.render_held {
            return false;
        }
        let selection = selection::viewport_range(&self.terminal);
        let moved = selection != self.selection;
        self.selection = selection;
        moved | self.refresh_frame()
    }

    fn refresh_frame(&mut self) -> bool {
        match self.frame.update_with(
            &mut self.renderer,
            &self.terminal,
            self.settings.minimum_contrast,
        ) {
            Ok(Some(frame)) => {
                self.frame = Rc::new(frame);
                true
            }
            Ok(None) => false,
            Err(error) => {
                let message = error.to_string();
                if self.status.as_ref() == Some(&message) {
                    return false;
                }
                self.status = Some(message);
                true
            }
        }
    }

    fn redraw_output(&mut self, cx: &mut Context<Self>) {
        if self.typed_at.is_some_and(|t| t.elapsed() < TYPING_INTERVAL) {
            if self.refresh() {
                cx.notify();
            }
            return;
        }
        let me = cx.weak_entity();
        let pending = &mut cx.default_global::<OutputClock>().0;
        if pending.contains(&me) {
            return;
        }
        pending.push(me);
        if pending.len() == 1 {
            cx.spawn(async |_, cx| {
                cx.background_executor().timer(OUTPUT_INTERVAL).await;
                let _ = cx.update(|cx| {
                    for view in std::mem::take(&mut cx.default_global::<OutputClock>().0) {
                        let _ = view.update(cx, |view, cx| {
                            if view.refresh() {
                                cx.notify();
                            }
                        });
                    }
                });
            })
            .detach();
        }
    }

    fn configure_terminal(&self, terminal: &mut Terminal) {
        configure(
            terminal,
            &self.settings,
            &self.held_frame,
            &self.contrast,
            &self.images,
        );
    }

    /// Records the grid's bounds, inside the padding, and resizes the PTY to the cells that
    /// fit.
    fn set_bounds(&mut self, bounds: Bounds<Pixels>, metrics: CellMetrics) {
        self.grid.lay_out(bounds, metrics, &self.commands);
    }

    fn at(&self, position: Point<Pixels>) -> CellPosition {
        self.grid.at(position)
    }
}

/// The text a clipboard write puts on the clipboard. An empty content list clears it (OSC 52
/// with no data, as Ghostty reports and herdr-app applies). Only UTF-8 `text/plain` within
/// the size limit is written; anything else leaves the clipboard alone.
fn clipboard_text(contents: &[terminal::ClipboardContent]) -> Option<String> {
    if contents.is_empty() {
        return Some(String::new());
    }
    let size = contents
        .iter()
        .try_fold(0usize, |size, content| size.checked_add(content.data.len()))?;
    if size > MAX_CLIPBOARD_WRITE {
        return None;
    }
    let content = contents.iter().find(|content| {
        content
            .mime
            .split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/plain"))
    })?;
    String::from_utf8(content.data.clone()).ok()
}

fn clipboard_policy(settings: &TerminalSettings) -> terminal::ClipboardWritePolicy {
    match settings.clipboard_write {
        crate::settings::ClipboardWrite::Allow => terminal::ClipboardWritePolicy::Allow,
        crate::settings::ClipboardWrite::Deny => terminal::ClipboardWritePolicy::Deny,
    }
}

fn configure(
    terminal: &mut Terminal,
    settings: &TerminalSettings,
    held_frame: &hold::Captured,
    contrast: &Rc<std::cell::Cell<f32>>,
    images: &Rc<std::cell::RefCell<images::ImageCache>>,
) {
    terminal.set_clipboard_write_policy(clipboard_policy(settings));
    client::limit_scrollback(terminal).expect("set terminal scrollback limits");
    terminal
        .set_clipboard_write_max_bytes(Some(MAX_CLIPBOARD_WRITE))
        .expect("set terminal clipboard size limit");
    hold::install(
        terminal,
        held_frame.clone(),
        contrast.clone(),
        images.clone(),
    );
    // The mirror stores and draws the images the program sends.
    infrastructure::kitty::enable(terminal).expect("enable Kitty graphics");
}

#[cfg(test)]
mod event_tests {
    use super::{MAX_CLIPBOARD_WRITE, clipboard_text, shown_title};
    use base64::Engine;
    use ghostty_vt::{
        Terminal,
        terminal::{ClipboardLocation, ClipboardWritePolicy, Event, Options},
    };

    /// What each OSC 52 write in `data` would put on the clipboard under `policy`.
    fn clipboard(policy: ClipboardWritePolicy, data: &[u8]) -> Vec<(ClipboardLocation, String)> {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.set_clipboard_write_policy(policy);
        terminal
            .set_clipboard_write_max_bytes(Some(MAX_CLIPBOARD_WRITE))
            .unwrap();
        terminal.vt_write(data);
        terminal
            .events()
            .filter_map(|event| match event {
                Event::ClipboardWrite(write) => {
                    Some((write.location, clipboard_text(&write.contents)?))
                }
                _ => None,
            })
            .collect()
    }

    fn osc52(target: &str, data: &[u8]) -> Vec<u8> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        format!("\x1b]52;{target};{encoded}\x07").into_bytes()
    }

    #[test]
    fn osc52_writes_text_only_when_allowed() {
        let write = osc52("c", "héllo".as_bytes());
        assert_eq!(
            clipboard(ClipboardWritePolicy::Allow, &write),
            [(ClipboardLocation::Standard, "héllo".to_owned())]
        );
        assert!(clipboard(ClipboardWritePolicy::Deny, &write).is_empty());
        assert_eq!(
            clipboard(ClipboardWritePolicy::Allow, &osc52("p", b"sel")),
            [(ClipboardLocation::Primary, "sel".to_owned())]
        );
    }

    #[test]
    fn osc52_with_no_data_clears_only_when_allowed() {
        assert_eq!(
            clipboard(ClipboardWritePolicy::Allow, b"\x1b]52;c;\x07"),
            [(ClipboardLocation::Standard, String::new())]
        );
        assert!(clipboard(ClipboardWritePolicy::Deny, b"\x1b]52;c;\x07").is_empty());
    }

    #[test]
    fn osc52_ignores_text_that_is_not_utf8() {
        assert!(clipboard(ClipboardWritePolicy::Allow, &osc52("c", b"\xff\xfe")).is_empty());
    }

    #[test]
    fn osc52_accepts_one_mebibyte_and_drops_anything_larger() {
        let at_limit = vec![b'a'; MAX_CLIPBOARD_WRITE];
        let written = clipboard(ClipboardWritePolicy::Allow, &osc52("c", &at_limit));
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].1.len(), MAX_CLIPBOARD_WRITE);
        let over = vec![b'a'; MAX_CLIPBOARD_WRITE + 1];
        assert!(clipboard(ClipboardWritePolicy::Allow, &osc52("c", &over)).is_empty());
    }

    #[test]
    fn titles_drop_agent_spinner_frames() {
        assert_eq!(shown_title("\u{2733} Fix the tests"), "Fix the tests");
        assert_eq!(shown_title("\u{280b} working"), "working");
        assert_eq!(shown_title("\u{25d0} \u{2733} two of them"), "two of them");
        assert_eq!(shown_title("\u{03c0} \u{2839} task"), "\u{03c0} task");
        assert_eq!(shown_title("\u{2713} build"), "\u{2713} build");
        assert_eq!(shown_title("\u{2733}"), "\u{2733}");
    }
}

/// Adapted from herdr-app/src/terminal.rs (Apache-2.0): remove changing agent spinner frames
/// from the title while preserving stable status symbols and the actual title text.
fn shown_title(title: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let spinner = |c: char| matches!(c, '\u{2800}'..='\u{28ff}' | '\u{00b7}' | '\u{2722}' | '\u{2733}' | '\u{2736}' | '\u{273b}' | '\u{273d}' | '\u{25d0}'..='\u{25d3}');
    let rest = title.trim_start_matches(|c: char| spinner(c) || c.is_whitespace());
    let led = title.len() - rest.len();
    if !rest.is_empty()
        && title[..led].chars().any(spinner)
        && title[..led].ends_with(char::is_whitespace)
    {
        return Cow::Borrowed(rest);
    }
    let braille =
        |word: &str| !word.is_empty() && word.chars().all(|c| matches!(c, '\u{2800}'..='\u{28ff}'));
    let mut words = title.splitn(3, ' ');
    match (words.next(), words.next()) {
        (Some(prefix), Some(frame))
            if !prefix.is_empty()
                && prefix.chars().count() <= 3
                && !braille(prefix)
                && braille(frame) =>
        {
            match words
                .next()
                .map(str::trim_start)
                .filter(|rest| !rest.is_empty())
            {
                Some(rest) => Cow::Owned(format!("{prefix} {rest}")),
                None => Cow::Borrowed(prefix),
            }
        }
        _ => Cow::Borrowed(title),
    }
}

#[cfg(test)]
mod font_tests {
    use super::BUNDLED_FONTS;

    /// Bold and italic text must reach real faces: a variable font's axes aren't applied by
    /// GPUI, so the four bundled files must be distinct static faces.
    #[test]
    fn bundled_fonts_carry_real_bold_and_italic_faces() {
        let faces: Vec<_> = BUNDLED_FONTS
            .iter()
            .map(|data| {
                let face = ttf_parser::Face::parse(data, 0).unwrap();
                assert!(!face.is_variable());
                (face.is_bold(), face.is_italic())
            })
            .collect();
        assert_eq!(
            faces,
            [(false, false), (true, false), (false, true), (true, true)]
        );
    }
}

/// Gestures driven through a headless GPUI window: real hitbox, capture-phase moves,
/// mouse-tracking routing and the clipboard path.
#[cfg(test)]
mod gesture_tests {
    use super::{TerminalView, typing::InputHold};
    use crate::settings::TerminalSettings;
    use domain::session::{Agent, Session};
    use gpui::{
        AppContext, Bounds, Context, Entity, IntoElement, Modifiers, MouseButton, ParentElement,
        Pixels, Point, Render, Styled, TestAppContext, VisualTestContext, Window, div, point, px,
    };
    use infrastructure::client::TerminalControl;

    /// Lays the terminal out as the workspace does, in a full-size flex column.
    struct Host(Entity<TerminalView>);

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().child(self.0.clone())
        }
    }

    fn open(cx: &mut TestAppContext) -> (Entity<TerminalView>, &mut VisualTestContext) {
        let (host, cx) = cx.add_window_view(|window, cx| {
            let session = Session {
                id: "gesture-test".into(),
                worktree: std::env::temp_dir(),
                agent: Agent::Shell,
            };
            let settings = TerminalSettings {
                copy_on_select: true,
                ..TerminalSettings::default()
            };
            // In-memory transport: no daemon client, so the view runs on what the test feeds it.
            let (commands, _) = async_channel::unbounded();
            let (_, events) = async_channel::unbounded();
            Host(cx.new(|cx| {
                TerminalView::with_transport(
                    session,
                    infrastructure::theme::default_scheme(),
                    settings,
                    InputHold::default(),
                    commands,
                    events,
                    window,
                    cx,
                )
            }))
        });
        cx.run_until_parked();
        let view = host.read_with(cx, |host, _| host.0.clone());
        (view, cx)
    }

    /// Writes program output and takes a fresh frame, then a requests receiver that
    /// sees what the view sends to the program.
    fn feed(
        view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
        bytes: &[u8],
    ) -> async_channel::Receiver<TerminalControl> {
        let (tx, rx) = async_channel::unbounded();
        view.update(cx, |view, cx| {
            view.commands = tx;
            view.terminal.vt_write(bytes);
            view.refresh();
            cx.notify();
        });
        cx.run_until_parked();
        rx
    }

    fn bounds(view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> Bounds<Pixels> {
        view.read_with(cx, |view, _| view.grid.bounds)
    }

    fn cell(
        view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
        row: f32,
        col: f32,
    ) -> Point<Pixels> {
        let (bounds, size) =
            view.read_with(cx, |view, _| (view.grid.bounds, view.grid.metrics.size));
        bounds.origin + point(size.width * (col + 0.5), size.height * (row + 0.5))
    }

    fn inputs(rx: &async_channel::Receiver<TerminalControl>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|control| match control {
                TerminalControl::Input(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                _ => None,
            })
            .collect()
    }

    /// A workspace whose tab can close, as detaching or ending one does.
    struct Closable(Option<Entity<TerminalView>>);

    impl Render for Closable {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().children(self.0.clone())
        }
    }

    #[gpui::test]
    fn closing_a_tab_releases_its_image_uploads(cx: &mut TestAppContext) {
        use base64::Engine;
        let (host, cx) = cx.add_window_view(|window, cx| {
            let session = Session {
                id: "image-teardown".into(),
                worktree: std::env::temp_dir(),
                agent: Agent::Shell,
            };
            let (commands, _) = async_channel::unbounded();
            let (_, events) = async_channel::unbounded();
            Closable(Some(cx.new(|cx| {
                TerminalView::with_transport(
                    session,
                    infrastructure::theme::default_scheme(),
                    TerminalSettings::default(),
                    InputHold::default(),
                    commands,
                    events,
                    window,
                    cx,
                )
            })))
        });
        cx.run_until_parked();
        let view = host.read_with(cx, |host, _| host.0.clone().unwrap());
        let data = base64::engine::general_purpose::STANDARD.encode(vec![9u8; 4 * 4 * 4]);
        // Placements need the cell's pixel size, which the daemon's geometry carries.
        view.update(cx, |view, _| view.terminal.resize(20, 5, 8, 16).unwrap());
        feed(
            &view,
            cx,
            format!("\x1b_Ga=T,f=32,s=4,v=4,i=1,q=2;{data}\x1b\\").as_bytes(),
        );
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let (placements, images) =
            view.read_with(cx, |view, _| (view.frame.images.len(), view.images.clone()));
        assert_eq!(placements, 1, "the frame shows the image");
        assert_eq!(images.borrow().len(), 1, "the paint uploaded the image");
        drop(view);
        host.update(cx, |host, cx| {
            host.0 = None;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(images.borrow().len(), 0, "the release dropped every upload");
    }

    #[gpui::test]
    fn drag_past_the_grid_keeps_selecting_and_copies(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        feed(&view, cx, b"alpha beta\r\ngamma delta");
        let none = Modifiers::none();
        let start = cell(&view, cx, 0., 0.);
        cx.simulate_mouse_down(start, MouseButton::Left, none);
        let inside = cell(&view, cx, 1., 2.);
        cx.simulate_mouse_move(inside, MouseButton::Left, none);
        // Past the bottom-right of the terminal: only capture-phase delivery sees this.
        let outside = bounds(&view, cx).bottom_right() + point(px(40.), px(40.));
        cx.simulate_mouse_move(outside, MouseButton::Left, none);
        let selected = view.read_with(cx, |view, _| super::selection::text(&view.terminal));
        assert_eq!(selected.as_deref(), Some("alpha beta\ngamma delta"));
        cx.simulate_mouse_up(outside, MouseButton::Left, none);
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("alpha beta\ngamma delta"));
        assert!(!view.read_with(cx, |view, _| view.selector.selecting));
    }

    #[gpui::test]
    fn tracked_mouse_goes_to_the_program_with_paired_releases(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        // Normal tracking with SGR encoding.
        feed(&view, cx, b"text\x1b[?1000h\x1b[?1006h");
        let none = Modifiers::none();
        let at = cell(&view, cx, 0., 1.);
        let outside = bounds(&view, cx).bottom_right() + point(px(40.), px(40.));
        let rx = feed(&view, cx, b"");
        cx.simulate_mouse_down(at, MouseButton::Left, none);
        cx.simulate_mouse_up(outside, MouseButton::Left, none);
        cx.simulate_mouse_down(at, MouseButton::Middle, none);
        cx.simulate_mouse_up(outside, MouseButton::Middle, none);
        let sent = inputs(&rx);
        assert_eq!(sent.len(), 4, "{sent:?}");
        assert!(
            sent[0].starts_with("\x1b[<0;") && sent[0].ends_with('M'),
            "{sent:?}"
        );
        assert!(
            sent[1].starts_with("\x1b[<0;") && sent[1].ends_with('m'),
            "{sent:?}"
        );
        assert!(
            sent[2].starts_with("\x1b[<1;") && sent[2].ends_with('M'),
            "{sent:?}"
        );
        assert!(
            sent[3].starts_with("\x1b[<1;") && sent[3].ends_with('m'),
            "{sent:?}"
        );
        let selected = view.read_with(cx, |view, _| super::selection::text(&view.terminal));
        assert_eq!(selected, None, "a tracked click doesn't select");
    }
}
