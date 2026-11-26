mod frame;
mod input;
mod links;
mod metrics;
mod paint;
mod selection;

use self::metrics::preload_font_files;
pub use self::metrics::{CellMetrics, TerminalFont, cell_metrics, column_width};

use self::frame::{CellPosition, Frame, frame};
use crate::settings::TerminalSettings;
use anyhow::{Result, bail};
use domain::session::{Agent, Geometry, Session};
use ghostty_vt::{Terminal, key, mouse, render::RenderState, snapshot, terminal::Options};
use gpui::*;
use infrastructure::{
    client::{self, SessionMetadata, TerminalControl, TerminalEvent},
    theme::Scheme,
};
use std::{path::PathBuf, sync::Arc, thread};

pub(crate) fn terminal_font_family(cx: &App) -> &'static str {
    let fonts = cx.text_system().all_font_names();
    ["JetBrainsMono Nerd Font Mono", "JetBrains Mono"]
        .into_iter()
        .find(|family| fonts.iter().any(|name| name == family))
        .unwrap_or("monospace")
}

/// The terminal font for `settings`: its `font_family` when installed, else the default.
pub fn terminal_font(settings: &TerminalSettings, cx: &App) -> TerminalFont {
    let family: SharedString = match &settings.font_family {
        Some(family)
            if cx
                .text_system()
                .all_font_names()
                .iter()
                .any(|name| name == family) =>
        {
            family.clone().into()
        }
        _ => terminal_font_family(cx).into(),
    };
    let mut font = font(family);
    font.weight = FontWeight(f32::from(settings.font_weight));
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
    /// The grid as last measured in `render`.
    metrics: CellMetrics,
    terminal: Terminal,
    renderer: RenderState,
    frame: Arc<Frame>,
    bounds: Bounds<Pixels>,
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
    geometry: Option<Geometry>,
    encoder: key::Encoder,
    mouse: mouse::Encoder,
    commands: async_channel::Sender<TerminalControl>,
    _follow: Task<()>,
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
        let mut terminal = Terminal::new(Options::default()).expect("allocate Ghostty terminal");
        scheme
            .apply_to(&mut terminal)
            .expect("apply bundled terminal palette");
        Self {
            session,
            focus: cx.focus_handle(),
            metadata: SessionMetadata::default(),
            scheme,
            font: terminal_font(&settings, cx),
            settings,
            metrics: CellMetrics::default(),
            terminal,
            renderer: RenderState::new().expect("allocate render state"),
            frame: Arc::default(),
            bounds: Bounds::default(),
            selection: None,
            pointer: None,
            pressed_link: None,
            hints: Vec::new(),
            hint_footer: None,
            dragging: false,
            reporting_mouse: false,
            exited: false,
            status: None,
            geometry: None,
            encoder: key::Encoder::new().expect("allocate key encoder"),
            mouse: mouse::Encoder::new().expect("allocate mouse encoder"),
            commands,
            _follow: follow,
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
    /// resizes the PTY on the next frame.
    pub fn set_settings(&mut self, settings: TerminalSettings, cx: &mut Context<Self>) {
        if settings == self.settings {
            return;
        }
        self.font = terminal_font(&settings, cx);
        self.settings = settings;
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
    /// fit. Cell sizes go to the PTY in device pixels, as Ghostty reports them.
    fn set_bounds(&mut self, bounds: Bounds<Pixels>, metrics: CellMetrics) {
        self.bounds = Bounds::new(metrics.snap(bounds.origin), bounds.size);
        self.metrics = metrics;
        let cell = metrics.device_size();
        let fit = |length: Pixels, cell: Pixels| {
            (f32::from(length) / f32::from(cell)).clamp(1., 500.) as u16
        };
        let geometry = Geometry {
            cols: fit(bounds.size.width, metrics.size.width),
            rows: fit(bounds.size.height, metrics.size.height),
            cell_width: cell.width.clamp(1, 100) as u16,
            cell_height: cell.height.clamp(1, 100) as u16,
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
            row: (f32::from(local.y) / f32::from(self.metrics.size.height)).max(0.) as usize,
            column: (f32::from(local.x) / f32::from(self.metrics.size.width)).max(0.) as usize,
        }
    }
}
