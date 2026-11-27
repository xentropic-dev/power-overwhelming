// Event handling follows herdr-app/src/terminal.rs (Apache-2.0).
mod frame;
mod grid;
mod input;
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
use ghostty_vt::{
    Terminal, key, mouse,
    render::RenderState,
    snapshot,
    terminal::{self, ClipboardLocation, Event as VtEvent, Mode, Options},
};
use gpui::*;
use infrastructure::{
    client::{self, SessionMetadata, TerminalControl, TerminalEvent},
    theme::Scheme,
};
use std::{path::PathBuf, sync::Arc, thread, time::Duration};

const MAX_CLIPBOARD_WRITE: usize = 1024 * 1024;
const MAX_RENDER_HOLD: Duration = Duration::from_secs(1);

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

pub struct TerminalTitleChanged(pub String);
impl EventEmitter<TerminalTitleChanged> for TerminalView {}

pub struct TerminalBell;
impl EventEmitter<TerminalBell> for TerminalView {}

pub struct TerminalNotification {
    pub title: String,
    pub body: String,
}
impl EventEmitter<TerminalNotification> for TerminalView {}

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
    render_held: bool,
    render_hold_generation: u64,
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
                        view.apply(first, cx);
                        for _ in 0..127 {
                            let Ok(event) = rx.try_recv() else { break };
                            view.apply(event, cx);
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
        terminal.set_clipboard_write_policy(match settings.clipboard_write {
            crate::settings::ClipboardWrite::Allow => terminal::ClipboardWritePolicy::Allow,
            crate::settings::ClipboardWrite::Deny => terminal::ClipboardWritePolicy::Deny,
        });
        terminal
            .set_clipboard_write_max_bytes(Some(MAX_CLIPBOARD_WRITE))
            .expect("set terminal clipboard size limit");
        Self {
            session,
            focus: cx.focus_handle(),
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
            render_held: false,
            render_hold_generation: 0,
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
    /// resizes the PTY now, whether or not this terminal is on screen.
    pub fn set_settings(&mut self, settings: TerminalSettings, cx: &mut Context<Self>) {
        if settings == self.settings {
            return;
        }
        self.font = terminal_font(&settings, cx);
        self.terminal
            .set_clipboard_write_policy(match settings.clipboard_write {
                crate::settings::ClipboardWrite::Allow => terminal::ClipboardWritePolicy::Allow,
                crate::settings::ClipboardWrite::Deny => terminal::ClipboardWritePolicy::Deny,
            });
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

    fn apply(&mut self, response: TerminalEvent, cx: &mut Context<Self>) {
        let result = (|| -> Result<()> {
            match response {
                TerminalEvent::Snapshot { data, exited } => {
                    let mut terminal = snapshot::Decoder::from_slice(&data).decode()?;
                    self.scheme.apply_to(&mut terminal)?;
                    self.configure_terminal(&mut terminal);
                    let title = terminal.title()?.to_owned();
                    self.terminal = terminal;
                    self.exited = exited;
                    self.status = None;
                    self.selection = None;
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
                    let total_size = write
                        .contents
                        .iter()
                        .try_fold(0usize, |size, content| size.checked_add(content.data.len()));
                    if !matches!(total_size, Some(size) if size <= MAX_CLIPBOARD_WRITE) {
                        continue;
                    }
                    let Some(content) = write.contents.iter().find(|content| {
                        content.mime.split(';').next().is_some_and(|media_type| {
                            media_type.trim().eq_ignore_ascii_case("text/plain")
                        })
                    }) else {
                        continue;
                    };
                    let Ok(text) = std::str::from_utf8(&content.data) else {
                        continue;
                    };
                    match write.location {
                        ClipboardLocation::Standard => {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                        }
                        ClipboardLocation::Primary | ClipboardLocation::Selection => {
                            #[cfg(target_os = "linux")]
                            cx.write_to_primary(ClipboardItem::new_string(text.to_owned()));
                            #[cfg(not(target_os = "linux"))]
                            let _ = text;
                        }
                    }
                }
                VtEvent::DesktopNotification { title, body } => {
                    cx.emit(TerminalNotification { title, body });
                }
                VtEvent::RenderHold(held) => {
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

    fn refresh(&mut self) {
        if self.render_held {
            return;
        }
        let foreground = self.scheme.terminal_ink(&self.terminal).0;
        match frame(&mut self.renderer, &self.terminal, foreground) {
            Ok(frame) => self.frame = Arc::new(frame),
            Err(error) => self.status = Some(error.to_string()),
        }
    }

    fn configure_terminal(&self, terminal: &mut Terminal) {
        terminal.set_clipboard_write_policy(match self.settings.clipboard_write {
            crate::settings::ClipboardWrite::Allow => terminal::ClipboardWritePolicy::Allow,
            crate::settings::ClipboardWrite::Deny => terminal::ClipboardWritePolicy::Deny,
        });
        terminal
            .set_clipboard_write_max_bytes(Some(MAX_CLIPBOARD_WRITE))
            .expect("set terminal clipboard size limit");
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
mod event_tests {
    use super::shown_title;

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
