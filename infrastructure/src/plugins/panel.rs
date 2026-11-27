//! Panel plugins: Wasm components that register commands with default keys and draw full
//! panels, such as the bundled dashboard. One worker thread owns every instance.

use super::{
    CommandScope, PluginCommand,
    host::{CALL_EPOCHS, components, engine, load_component, store_limits},
    ui::{self, UiColor},
};
use crate::theme::Scheme;
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};
use wasmtime::{
    Engine, Store, StoreLimits,
    component::{Component, HasSelf, Linker},
};

mod bindings {
    wasmtime::component::bindgen!({
        path: ["../plugins/wit/terminal-ui.wit", "../plugins/wit/panel.wit"],
        world: "power:panel/panel-plugin",
        with: {
            "power:terminal-ui/types": crate::plugins::ui::power::terminal_ui::types,
            "power:terminal-ui/palette": crate::plugins::ui::power::terminal_ui::palette,
        },
    });
}

use bindings::PanelPlugin as PanelWorld;
pub use bindings::power::panel::types::{
    Bar as PanelBar, BoxOverview, Effect, Item as PanelItem, Line as PanelLine, ModelUsage,
    Overview, Panel, RunOverview, RunStatus, Section as PanelSection, SystemStats, Totals,
    UsageWindow,
};
use bindings::power::panel::types::{Command as WitCommand, CommandScope as WitScope};

/// Components that ship with Power, by name. The dashboard joins this list.
const BUNDLED: &[(&str, &[u8])] = &[(
    "dashboard",
    include_bytes!("../../../plugins/dashboard/dashboard.wasm"),
)];

const MAX_COMMANDS: usize = 32;
const MAX_KEYS: usize = 8;

/// Results the worker sends back to the desktop.
#[derive(Debug)]
pub enum PanelUpdate {
    /// A command asked to open the panel with this id. Redraw it every `refresh_ms` while
    /// it stays open, or at the host's default pace when `None`.
    OpenPanel {
        panel_id: String,
        refresh_ms: Option<u32>,
    },
    /// The panel drawn for a `render` request.
    Panel { panel_id: String, panel: Panel },
    /// A plugin failed. Failing components are dropped until the next reload.
    Error(String),
}

/// A plugin's Lua configuration table and its defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelSettings {
    pub key: String,
    pub defaults: serde_json::Value,
}

/// What the loaded panel plugins add to the configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelSetup {
    /// Every command, in load order.
    pub commands: Vec<PluginCommand>,
    /// Each plugin's settings table, at most one per key.
    pub settings: Vec<PanelSettings>,
}

/// The fastest redraw a plugin can ask for.
const MIN_REFRESH_MS: u32 = 250;

enum Event {
    Setup(mpsc::Sender<PanelSetup>),
    Configure(BTreeMap<String, serde_json::Value>),
    Invoke(String),
    Render {
        panel_id: String,
        overview: Box<Overview>,
        width: u32,
        scheme: &'static Scheme,
    },
    Reload(Option<PathBuf>, mpsc::Sender<()>),
}

/// A handle to the panel plugin worker. Clones share the worker, which stops once every
/// handle is dropped.
#[derive(Clone)]
pub struct PanelPlugins(mpsc::Sender<Event>);

impl PanelPlugins {
    /// Loads the bundled components and `<directory>/plugins/*.wasm` on a worker thread.
    pub fn start(directory: Option<PathBuf>) -> (Self, async_channel::Receiver<PanelUpdate>) {
        let (events, receiver) = mpsc::channel();
        let (updates, results) = async_channel::unbounded();
        thread::spawn(move || {
            if let Err(error) = run(receiver, &updates, directory) {
                let _ = updates.send_blocking(PanelUpdate::Error(format!(
                    "Panel plugins failed. {error:#}"
                )));
            }
        });
        (Self(events), results)
    }

    /// The loaded plugins' commands and settings tables. Blocks until the worker answers;
    /// empty when it has stopped.
    pub fn setup(&self) -> PanelSetup {
        let (reply, answer) = mpsc::channel();
        if self.0.send(Event::Setup(reply)).is_err() {
            return PanelSetup::default();
        }
        answer.recv().unwrap_or_default()
    }

    /// Every loaded command, in load order.
    pub fn commands(&self) -> Vec<PluginCommand> {
        self.setup().commands
    }

    /// Hands each plugin its evaluated settings table, by key. Plugins without one, or whose
    /// key is missing, get `{}`.
    pub fn configure(&self, sections: BTreeMap<String, serde_json::Value>) {
        let _ = self.0.send(Event::Configure(sections));
    }

    /// Runs a plugin command by id. Its effect arrives as an update.
    pub fn invoke(&self, id: impl Into<String>) {
        let _ = self.0.send(Event::Invoke(id.into()));
    }

    /// Asks the plugin that opened `panel_id` to draw it.
    pub fn render(
        &self,
        panel_id: impl Into<String>,
        overview: &domain::overview::Overview,
        width: u32,
        scheme: &'static Scheme,
    ) {
        let _ = self.0.send(Event::Render {
            panel_id: panel_id.into(),
            overview: Box::new(overview.into()),
            width,
            scheme,
        });
    }

    /// Reloads every component from `directory`, or the start directory when `None`, and
    /// waits until they are loaded.
    pub fn reload(&self, directory: Option<PathBuf>) {
        let (reply, done) = mpsc::channel();
        if self.0.send(Event::Reload(directory, reply)).is_ok() {
            let _ = done.recv();
        }
    }
}

struct HostState {
    limits: StoreLimits,
    scheme: Option<&'static Scheme>,
}

impl ui::power::terminal_ui::types::Host for HostState {}

impl ui::power::terminal_ui::palette::Host for HostState {
    fn get_palette(&mut self) -> Option<ui::power::terminal_ui::palette::Colors> {
        self.scheme.map(ui::palette_colors)
    }
}

impl bindings::power::panel::types::Host for HostState {}

struct Plugin {
    name: String,
    store: Store<HostState>,
    api: PanelWorld,
    commands: Vec<PluginCommand>,
    settings: Option<PanelSettings>,
    refresh_ms: Option<u32>,
}

struct Runtime {
    engine: Engine,
    linker: Linker<HostState>,
    bundled: Vec<(&'static str, Component)>,
    watchdog: mpsc::Sender<()>,
}

impl Runtime {
    fn new() -> Result<Self> {
        let (engine, watchdog) = engine()?;
        let mut linker = Linker::new(&engine);
        PanelWorld::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let bundled = BUNDLED
            .iter()
            .map(|(name, bytes)| Ok((*name, Component::from_binary(&engine, bytes)?)))
            .collect::<Result<_>>()?;
        Ok(Self {
            engine,
            linker,
            bundled,
            watchdog,
        })
    }

    /// Loads every component. Failures are skipped and returned as messages.
    fn load_all(&self, directory: Option<&Path>) -> (Vec<Plugin>, Vec<String>) {
        let mut found: Vec<(String, Result<Component>)> = self
            .bundled
            .iter()
            .map(|(name, component)| ((*name).to_owned(), Ok(component.clone())))
            .collect();
        let mut errors = Vec::new();
        if let Some(directory) = directory.map(|dir| dir.join("plugins"))
            && directory.is_dir()
        {
            match components(&directory) {
                Ok(paths) => found.extend(paths.into_iter().map(|path| {
                    let component = load_component(&path, &self.engine)
                        .with_context(|| format!("Load {}", path.display()));
                    (path.display().to_string(), component)
                })),
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        let mut plugins = Vec::new();
        let mut ids = HashSet::new();
        let mut keys = HashSet::new();
        for (name, component) in found {
            match component.and_then(|component| self.instantiate(&name, &component)) {
                Ok(mut plugin) => {
                    plugin.commands.retain(|command| {
                        let fresh = ids.insert(command.id.clone());
                        if !fresh {
                            errors.push(format!(
                                "Plugin {name} repeats the command {:?}. It was skipped.",
                                command.id
                            ));
                        }
                        fresh
                    });
                    if let Some(settings) = &plugin.settings
                        && !keys.insert(settings.key.clone())
                    {
                        errors.push(format!(
                            "Plugin {name} repeats the settings table {:?}. It gets no settings.",
                            settings.key
                        ));
                        plugin.settings = None;
                    }
                    plugins.push(plugin);
                }
                Err(error) => errors.push(format!("Could not load plugin {name}. {error:#}")),
            }
        }
        (plugins, errors)
    }

    fn instantiate(&self, name: &str, component: &Component) -> Result<Plugin> {
        let mut store = Store::new(
            &self.engine,
            HostState {
                limits: store_limits(),
                scheme: None,
            },
        );
        store.limiter(|state| &mut state.limits);
        self.watchdog.send(())?;
        let loaded = (|| -> Result<_> {
            store.set_epoch_deadline(CALL_EPOCHS);
            let api = PanelWorld::instantiate(&mut store, component, &self.linker)?;
            store.set_epoch_deadline(CALL_EPOCHS);
            let commands = api.call_commands(&mut store)?;
            store.set_epoch_deadline(CALL_EPOCHS);
            let settings = api.call_settings(&mut store)?;
            Ok((api, commands, settings))
        })();
        self.watchdog.send(())?;
        let (api, commands, settings) = loaded?;
        let settings = settings
            .map(|settings| -> Result<_> {
                ensure!(
                    super::valid_status_field(&settings.key)
                        && settings.defaults_json.len() <= 64 * 1024,
                    "Invalid plugin configuration key"
                );
                let defaults: serde_json::Value = serde_json::from_str(&settings.defaults_json)
                    .context("Read the plugin's default settings")?;
                ensure!(
                    defaults.is_object(),
                    "Plugin default settings must be an object"
                );
                Ok(PanelSettings {
                    key: settings.key,
                    defaults,
                })
            })
            .transpose()?;
        Ok(Plugin {
            name: name.to_owned(),
            store,
            api,
            commands: validate_commands(commands)?,
            settings,
            refresh_ms: None,
        })
    }

    fn configure(&self, plugin: &mut Plugin, config: &str) -> Result<Result<Option<u32>, String>> {
        self.watchdog.send(())?;
        plugin.store.set_epoch_deadline(CALL_EPOCHS);
        let refresh = plugin.api.call_configure(&mut plugin.store, config);
        self.watchdog.send(())?;
        Ok(refresh?)
    }

    fn invoke(&self, plugin: &mut Plugin, id: &str) -> Result<Result<Effect, String>> {
        self.watchdog.send(())?;
        plugin.store.set_epoch_deadline(CALL_EPOCHS);
        let effect = plugin.api.call_invoke(&mut plugin.store, id);
        self.watchdog.send(())?;
        Ok(effect?)
    }

    fn render(
        &self,
        plugin: &mut Plugin,
        panel_id: &str,
        overview: &Overview,
        width: u32,
        scheme: &'static Scheme,
    ) -> Result<Result<Panel, String>> {
        self.watchdog.send(())?;
        plugin.store.data_mut().scheme = Some(scheme);
        plugin.store.set_epoch_deadline(CALL_EPOCHS);
        let panel = plugin
            .api
            .call_render_panel(&mut plugin.store, panel_id, overview, width);
        plugin.store.data_mut().scheme = None;
        self.watchdog.send(())?;
        Ok(panel?)
    }
}

fn text(value: &str, max: usize) -> bool {
    value.len() <= max && !value.chars().any(char::is_control)
}

/// A namespaced command id: `plugin.action`, ASCII letters, digits, `_`, `-` and `.`.
pub fn valid_command_id(id: &str) -> bool {
    super::valid_status_field(id)
        && id.contains('.')
        && !id.starts_with('.')
        && !id.ends_with('.')
        && !id.contains("..")
}

fn validate_commands(commands: Vec<WitCommand>) -> Result<Vec<PluginCommand>> {
    ensure!(
        commands.len() <= MAX_COMMANDS,
        "A plugin can register at most {MAX_COMMANDS} commands"
    );
    commands
        .into_iter()
        .map(|command| {
            ensure!(
                valid_command_id(&command.id),
                "Invalid command id {:?}. Use a namespaced id such as dashboard.open.",
                command.id
            );
            ensure!(
                text(&command.title, 100) && text(&command.description, 200),
                "Invalid title or description for {}",
                command.id
            );
            ensure!(
                command.default_keys.len() <= MAX_KEYS
                    && command.default_keys.iter().all(|key| text(key, 32)),
                "Invalid default keys for {}",
                command.id
            );
            Ok(PluginCommand {
                id: command.id,
                title: command.title,
                description: command.description,
                scope: match command.scope {
                    WitScope::Box => CommandScope::Box,
                    WitScope::Tab => CommandScope::Tab,
                },
                default_keys: command.default_keys,
            })
        })
        .collect()
}

/// Checks a drawn panel against the host's size limits and text rules.
pub fn validate_panel(panel: &mut Panel) -> Result<()> {
    ensure!(text(&panel.title, 200), "Invalid panel title");
    ensure!(panel.sections.len() <= 32, "Too many panel sections");
    let mut bytes = panel.title.len();
    let mut lines = 0;
    for section in &mut panel.sections {
        ensure!(text(&section.heading, 200), "Invalid panel heading");
        bytes += section.heading.len();
        lines += section.lines.len();
        ensure!(lines <= 1024, "Too many panel lines");
        for line in &mut section.lines {
            ensure!(
                line.left.len() + line.right.len() <= 64,
                "Too many items on a panel line"
            );
            for item in line.left.iter_mut().chain(&mut line.right) {
                let colors = match item {
                    PanelItem::Text(segment) => {
                        ensure!(text(&segment.text, 4 * 1024), "Invalid panel text");
                        bytes += segment.text.len();
                        [segment.fg.as_deref(), segment.bg.as_deref()]
                    }
                    PanelItem::Bar(bar) => {
                        ensure!(bar.width <= 1000, "Panel bar is too wide");
                        bar.ratio = if bar.ratio.is_finite() {
                            bar.ratio.clamp(0., 1.)
                        } else {
                            0.
                        };
                        [bar.fg.as_deref(), bar.bg.as_deref()]
                    }
                    PanelItem::Spacer => [None, None],
                };
                for color in colors.into_iter().flatten() {
                    ensure!(text(color, 32), "Invalid panel color");
                    UiColor::parse(color)?;
                }
            }
        }
    }
    ensure!(bytes <= 256 * 1024, "Panel text exceeds the size limit");
    Ok(())
}

fn run(
    receiver: mpsc::Receiver<Event>,
    updates: &async_channel::Sender<PanelUpdate>,
    mut directory: Option<PathBuf>,
) -> Result<()> {
    let runtime = Runtime::new()?;
    let report = |errors: Vec<String>| -> Result<()> {
        for error in errors {
            updates.send_blocking(PanelUpdate::Error(error))?;
        }
        Ok(())
    };
    let (mut plugins, errors) = runtime.load_all(directory.as_deref());
    report(errors)?;
    // Which plugin opened each panel, by index into `plugins`.
    let mut panels: BTreeMap<String, usize> = BTreeMap::new();
    while let Ok(event) = receiver.recv() {
        match event {
            Event::Setup(reply) => {
                let _ = reply.send(PanelSetup {
                    commands: plugins
                        .iter()
                        .flat_map(|plugin| plugin.commands.iter().cloned())
                        .collect(),
                    settings: plugins
                        .iter()
                        .filter_map(|plugin| plugin.settings.clone())
                        .collect(),
                });
            }
            Event::Configure(sections) => {
                let mut index = 0;
                while index < plugins.len() {
                    let plugin = &mut plugins[index];
                    let config = plugin
                        .settings
                        .as_ref()
                        .and_then(|settings| sections.get(&settings.key))
                        .map_or_else(|| "{}".to_owned(), ToString::to_string);
                    match runtime.configure(plugin, &config) {
                        Ok(Ok(refresh)) => {
                            plugin.refresh_ms = refresh.map(|ms| ms.max(MIN_REFRESH_MS));
                        }
                        Ok(Err(message)) => {
                            plugin.refresh_ms = None;
                            updates.send_blocking(PanelUpdate::Error(format!(
                                "Plugin {} rejected its settings. {message}",
                                plugin.name
                            )))?;
                        }
                        Err(error) => {
                            drop_plugin(&mut plugins, &mut panels, index, &error, updates)?;
                            continue;
                        }
                    }
                    index += 1;
                }
            }
            Event::Reload(next, done) => {
                if next.is_some() {
                    directory = next;
                }
                let (loaded, errors) = runtime.load_all(directory.as_deref());
                plugins = loaded;
                panels.clear();
                let _ = done.send(());
                report(errors)?;
            }
            Event::Invoke(id) => {
                let Some(index) = plugins
                    .iter()
                    .position(|plugin| plugin.commands.iter().any(|command| command.id == id))
                else {
                    updates.send_blocking(PanelUpdate::Error(format!(
                        "No plugin provides the command {id:?}"
                    )))?;
                    continue;
                };
                match runtime.invoke(&mut plugins[index], &id) {
                    Ok(Ok(Effect::None)) => {}
                    Ok(Ok(Effect::OpenPanel(panel_id))) => {
                        if text(&panel_id, 64) && !panel_id.is_empty() {
                            panels.insert(panel_id.clone(), index);
                            updates.send_blocking(PanelUpdate::OpenPanel {
                                panel_id,
                                refresh_ms: plugins[index].refresh_ms,
                            })?;
                        } else {
                            updates.send_blocking(PanelUpdate::Error(format!(
                                "Plugin {} returned an invalid panel id",
                                plugins[index].name
                            )))?;
                        }
                    }
                    Ok(Err(message)) => updates
                        .send_blocking(PanelUpdate::Error(format!("{id} failed. {message}")))?,
                    Err(error) => {
                        drop_plugin(&mut plugins, &mut panels, index, &error, updates)?;
                    }
                }
            }
            Event::Render {
                panel_id,
                overview,
                width,
                scheme,
            } => {
                let Some(&index) = panels.get(&panel_id) else {
                    updates.send_blocking(PanelUpdate::Error(format!(
                        "No plugin draws the panel {panel_id:?}"
                    )))?;
                    continue;
                };
                match runtime.render(&mut plugins[index], &panel_id, &overview, width, scheme) {
                    Ok(Ok(mut panel)) => match validate_panel(&mut panel) {
                        Ok(()) => updates.send_blocking(PanelUpdate::Panel { panel_id, panel })?,
                        Err(error) => updates.send_blocking(PanelUpdate::Error(format!(
                            "Plugin {} drew an invalid panel. {error:#}",
                            plugins[index].name
                        )))?,
                    },
                    Ok(Err(message)) => updates.send_blocking(PanelUpdate::Error(format!(
                        "Panel {panel_id} failed. {message}"
                    )))?,
                    Err(error) => {
                        drop_plugin(&mut plugins, &mut panels, index, &error, updates)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Drops a plugin that trapped or ran too long, keeping the other plugins' panels.
fn drop_plugin(
    plugins: &mut Vec<Plugin>,
    panels: &mut BTreeMap<String, usize>,
    index: usize,
    error: &anyhow::Error,
    updates: &async_channel::Sender<PanelUpdate>,
) -> Result<()> {
    let plugin = plugins.remove(index);
    panels.retain(|_, owner| *owner != index);
    for owner in panels.values_mut() {
        if *owner > index {
            *owner -= 1;
        }
    }
    updates.send_blocking(PanelUpdate::Error(format!(
        "Disabled plugin {} until the next reload. {error:#}",
        plugin.name
    )))?;
    Ok(())
}

impl From<&domain::overview::Overview> for Overview {
    fn from(overview: &domain::overview::Overview) -> Self {
        use domain::{dag::RunStatus as Status, overview as source};
        let count = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
        Self {
            boxes: overview
                .boxes
                .iter()
                .map(|entry: &source::BoxOverview| BoxOverview {
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                    terminals: count(entry.terminals),
                    attached: count(entry.attached),
                    agents_working: count(entry.agents_working),
                    runs_active: count(entry.runs_active),
                })
                .collect(),
            runs: overview
                .runs
                .iter()
                .map(|run| RunOverview {
                    run: run.run.clone(),
                    box_id: run.box_id.clone(),
                    title: run.title.clone(),
                    completed: count(run.completed),
                    total: count(run.total),
                    workers: count(run.workers),
                    eta_secs: run.eta_secs,
                    status: match run.status {
                        Status::Active => RunStatus::Active,
                        Status::Completed => RunStatus::Completed,
                        Status::Stalled => RunStatus::Stalled,
                        Status::Archived => RunStatus::Archived,
                    },
                    paused: run.paused,
                })
                .collect(),
            usage: overview
                .usage
                .iter()
                .map(|usage| ModelUsage {
                    provider: usage.provider.clone(),
                    model: usage.model.clone(),
                    windows: usage
                        .windows
                        .iter()
                        .map(|window| UsageWindow {
                            label: window.label.clone(),
                            used_percent: window.used_percent,
                            resets_at: window.resets_at,
                        })
                        .collect(),
                })
                .collect(),
            system: SystemStats {
                cpu_percent: overview.system.cpu_percent,
                mem_used: overview.system.mem_used,
                mem_total: overview.system.mem_total,
            },
            totals: Totals {
                agents_working: count(overview.totals.agents_working),
                terminals: count(overview.totals.terminals),
                attached: count(overview.totals.attached),
            },
            at: overview.at,
        }
    }
}
