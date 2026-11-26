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

#[cfg(test)]
pub(crate) use self::metrics::has_family;
use self::metrics::preload_font_files;
pub use self::metrics::{CellMetrics, TerminalFont, cell_metrics, column_width};

use self::frame::{CellPosition, Frame, frame};
use self::grid::PtyGrid;
use crate::settings::TerminalSettings;
use anyhow::{Result, bail};
use domain::session::{Agent, Session};
use ghostty_vt::{Terminal, key, mouse, render::RenderState, snapshot, terminal::Options};
use gpui::*;
use infrastructure::{
    client::{self, SessionMetadata, TerminalControl, TerminalEvent},
    theme::Scheme,
};
use std::{collections::HashSet, path::PathBuf, sync::Arc, thread};

/// JetBrains Mono, shipped so the terminal's default font resolves on every machine. A family
/// GPUI can't find resolves to its UI fallback (Noto Sans and the like) with default features,
/// which are proportional and join `ffi` whatever `terminal.ligatures` says.
pub(crate) const BUNDLED_FONT: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMono-Variable.ttf");
const BUNDLED_FAMILY: &str = "JetBrains Mono";

/// Adds the bundled font to GPUI's text system; call once before any terminal is drawn.
pub fn load_bundled_fonts(cx: &App) {
    if let Err(error) = cx
        .text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(BUNDLED_FONT)])
    {
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
    frame: Arc<Frame>,
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
    encoder: key::Encoder,
    mouse: mouse::Encoder,
    /// Keys whose press went to the program, so their releases go too.
    pressed_keys: HashSet<String>,
    /// Whether the program was last told it has focus, as focus reporting sees it.
    focused: bool,
    /// What an input method is composing, shown at the cursor until it commits.
    marked: Option<String>,
    commands: async_channel::Sender<TerminalControl>,
    _follow: Task<()>,
    _focus_changes: [Subscription; 3],
}

impl TerminalView {
    pub fn new(
        session: Session,
        scheme: &'static Scheme,
        settings: TerminalSettings,
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
                if view
                    .update_in(cx, |view, _, cx| {
                        let agent = view.session.agent.clone();
                        let metadata = view.metadata.clone();
                        view.apply(first);
                        for _ in 0..127 {
                            let Ok(event) = rx.try_recv() else { break };
                            view.apply(event);
                        }
                        if view.session.agent != agent {
                            cx.emit(AgentChanged(view.session.agent.clone()));
                        }
                        if view.session.agent != agent || view.metadata != metadata {
                            cx.emit(BarContextChanged);
                        }
                        view.refresh();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let focus = cx.focus_handle();
        let focus_changes = [
            cx.on_focus(&focus, window, |view, window, _| view.sync_focus(window)),
            cx.on_blur(&focus, window, |view, window, _| view.sync_focus(window)),
            cx.observe_window_activation(window, |view, window, _| view.sync_focus(window)),
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
            frame: Arc::default(),
            selection: None,
            pointer: None,
            pressed_link: None,
            hints: Vec::new(),
            hint_footer: None,
            dragging: false,
            reporting_mouse: false,
            exited: false,
            status: None,
            encoder: key::Encoder::new().expect("allocate key encoder"),
            mouse: mouse::Encoder::new().expect("allocate mouse encoder"),
            pressed_keys: HashSet::new(),
            focused: false,
            marked: None,
            commands,
            _follow: follow,
            _focus_changes: focus_changes,
        }
    }

    pub fn set_scheme(&mut self, scheme: &'static Scheme, cx: &mut Context<Self>) {
        self.scheme = scheme;
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
        self.font = terminal_font(&settings, cx);
        self.settings = settings;
        if !self.exited {
            let metrics = cell_metrics(cx.text_system(), &self.font, self.grid.metrics.scale);
            self.grid.remeasure(metrics, &self.commands);
        }
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

    fn apply(&mut self, response: TerminalEvent) {
        let result = (|| -> Result<()> {
            match response {
                TerminalEvent::Snapshot { data, exited } => {
                    let mut terminal = snapshot::Decoder::from_slice(&data).decode()?;
                    self.scheme.apply_to(&mut terminal)?;
                    self.terminal = terminal;
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

    fn refresh(&mut self) {
        let foreground = self.scheme.terminal_ink(&self.terminal).0;
        match frame(&mut self.renderer, &self.terminal, foreground) {
            Ok(frame) => self.frame = Arc::new(frame),
            Err(error) => self.status = Some(error.to_string()),
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
