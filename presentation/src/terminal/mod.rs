//! Output redraw scheduling adapted from herdr-app `src/terminal.rs` (Apache-2.0).

pub(crate) mod bench;
mod frame;
mod grid;
mod ime;
mod input;
mod keys;
mod links;
mod metrics;
mod paint;
mod runs;
mod selection;
mod sprites;
mod typing;

#[cfg(test)]
pub(crate) use self::metrics::has_family;
use self::metrics::preload_font_files;
pub use self::metrics::{CellMetrics, TerminalFont, cell_metrics, column_width};

use self::frame::{CellPosition, Frame};
use self::grid::PtyGrid;
pub use self::typing::InputHold;
use self::typing::Typing;
use crate::settings::TerminalSettings;
use anyhow::{Result, bail};
use domain::session::{Agent, Session};
use ghostty_vt::{Terminal, mouse, render::RenderState, snapshot, terminal::Options};
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
    selection: Option<(CellPosition, CellPosition)>,
    pointer: Option<Point<Pixels>>,
    pressed_link: Option<Arc<str>>,
    /// Numbered link labels shown while the user picks a link by keyboard.
    hints: Vec<(CellPosition, Arc<str>)>,
    hint_footer: Option<String>,
    dragging: bool,
    reporting_mouse: bool,
    exited: bool,
    status: Option<String>,
    mouse: mouse::Encoder,
    /// Keys, input methods and focus as the program hears them.
    typing: Typing,
    commands: async_channel::Sender<TerminalControl>,
    _follow: Task<()>,
    _focus_changes: [Subscription; 3],
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
            pointer: None,
            pressed_link: None,
            hints: Vec::new(),
            hint_footer: None,
            dragging: false,
            reporting_mouse: false,
            exited: false,
            status: None,
            mouse: mouse::Encoder::new().expect("allocate mouse encoder"),
            typing: Typing::new(hold),
            commands,
            _follow: follow,
            _focus_changes: focus_changes,
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
            self.apply(event);
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

    fn apply(&mut self, response: TerminalEvent) {
        let result = (|| -> Result<()> {
            match response {
                TerminalEvent::Snapshot { data, exited } => {
                    let mut terminal = snapshot::Decoder::from_slice(&data).decode()?;
                    self.scheme.apply_to(&mut terminal)?;
                    self.renderer = RenderState::new()?;
                    self.frame = Rc::default();
                    let previous = std::mem::replace(&mut self.terminal, terminal);
                    if let Some(bytes) = self.typing.installed(&previous, &self.terminal) {
                        self.send(bytes);
                    }
                    self.exited = exited;
                    self.status = None;
                    self.selection = None;
                    self.metadata = SessionMetadata::default();
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

    fn refresh(&mut self) -> bool {
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

    /// Records the grid's bounds, inside the padding, and resizes the PTY to the cells that
    /// fit.
    fn set_bounds(&mut self, bounds: Bounds<Pixels>, metrics: CellMetrics) {
        self.grid.lay_out(bounds, metrics, &self.commands);
    }

    fn at(&self, position: Point<Pixels>) -> CellPosition {
        self.grid.at(position)
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
