use super::{
    ActiveView as ViewSnapshot, StatusBarPlugins,
    clock::Clock,
    host::{CALL_EPOCHS, engine, load_component, store_limits},
};
use crate::{
    config::{self, Sections},
    theme::{self, Scheme},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};
use wasmtime::{
    Engine, Store, StoreLimits,
    component::{Component, HasSelf, Linker},
};

wasmtime::component::bindgen!({
    path: "../plugins/wit/terminal-ui.wit",
    world: "terminal-bottom-with-palette",
});

use self::power::terminal_ui::{palette, types};
pub use power::terminal_ui::types::{ActionMenu as UiActions, Overflow, Segment as UiSegment};

pub use self::Row as UiRow;

struct HostState {
    limits: StoreLimits,
    scheme: Option<&'static Scheme>,
}

impl types::Host for HostState {}

impl palette::Host for HostState {
    fn get_palette(&mut self) -> Option<palette::Colors> {
        let scheme = self.scheme?;
        let colors = &scheme.colors;
        Some(palette::Colors {
            scheme: scheme.name.clone(),
            foreground: colors.foreground,
            background: colors.background,
            cursor: colors.cursor,
            selection_background: scheme.selection_background,
            selection_foreground: scheme.selection_foreground,
            indexed: colors.indexed.to_vec(),
            accent: colors.accent,
            muted: colors.muted,
            surface: colors.surface,
            raised: colors.raised,
            border: colors.border,
            link: colors.link,
            error: colors.error,
        })
    }
}

const BUNDLED: &[u8] = include_bytes!("../../../plugins/power-bar/power-bar.wasm");

pub enum UiUpdate<S> {
    Settings(S),
    Row { revision: u64, row: Option<Row> },
    Error(Option<String>),
}

enum Event {
    View(u64, Option<ViewSnapshot>, &'static Scheme),
    Theme(u64, &'static Scheme),
    Reload,
    Fields(u64, BTreeMap<String, String>),
    Stop,
}

pub struct UiPlugins(mpsc::Sender<Event>);

impl UiPlugins {
    pub fn start<S: Send + Sync + 'static>(
        defaults: Sections,
        validate: fn(&Sections) -> Result<S>,
    ) -> (Self, async_channel::Receiver<UiUpdate<S>>) {
        let (events, receiver) = mpsc::channel();
        let (updates, results) = async_channel::unbounded();
        let fields_events = events.clone();
        let (field_requests, requests) = async_channel::bounded(1);
        thread::spawn(move || publish_fields(requests, fields_events));
        thread::spawn(move || {
            if let Err(error) = run(receiver, &updates, field_requests, defaults, validate) {
                let _ = updates.send_blocking(UiUpdate::Error(Some(format!(
                    "Status UI failed. {error:#}"
                ))));
            }
        });
        (Self(events), results)
    }

    pub fn view(&self, revision: u64, view: Option<ViewSnapshot>, scheme: &'static Scheme) {
        let _ = self.0.send(Event::View(revision, view, scheme));
    }

    pub fn theme(&self, revision: u64, scheme: &'static Scheme) {
        let _ = self.0.send(Event::Theme(revision, scheme));
    }

    pub fn reload(&self) {
        let _ = self.0.send(Event::Reload);
    }
}

impl Drop for UiPlugins {
    fn drop(&mut self) {
        // The field worker also holds a sender and shuts down after the UI worker drops its queue.
        let _ = self.0.send(Event::Stop);
    }
}

struct Runtime {
    engine: Engine,
    bundled: Component,
    linker: Linker<HostState>,
    watchdog: mpsc::Sender<()>,
}

impl Runtime {
    fn new() -> Result<Self> {
        let (engine, watchdog) = engine()?;
        let bundled = Component::from_binary(&engine, BUNDLED)?;
        let mut linker = Linker::new(&engine);
        TerminalBottomWithPalette::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        Ok(Self {
            engine,
            bundled,
            linker,
            watchdog,
        })
    }

    fn load(&self, directory: Option<&Path>, defaults: &Sections) -> Result<(Plugin, Sections)> {
        let external = directory.map(|dir| dir.join("terminal-bottom.wasm"));
        let component = match external.as_ref().filter(|path| path.exists()) {
            Some(path) => load_component(path, &self.engine)
                .with_context(|| format!("Load {}", path.display()))?,
            None => self.bundled.clone(),
        };

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
            let api = TerminalBottomWithPalette::instantiate(&mut store, &component, &self.linker)?;
            store.set_epoch_deadline(CALL_EPOCHS);
            let mut defaults = defaults.clone();
            defaults
                .entry(config::WORKTREES_SECTION.into())
                .or_insert_with(|| serde_json::json!({}));
            let settings = api.call_settings(&mut store)?;
            if let Some(settings) = &settings {
                ensure!(
                    super::valid_status_field(&settings.key),
                    "Invalid plugin configuration key"
                );
                let value = serde_json::from_str(&settings.defaults_json)?;
                defaults.entry(settings.key.clone()).or_insert(value);
            }
            let sections = match directory {
                Some(directory) => config::load_from(&directory.join("config.lua"), defaults)?,
                None => defaults,
            };
            config::worktree_root(&sections)?;
            let config = settings.map_or_else(
                || "{}".into(),
                |settings| sections[&settings.key].to_string(),
            );
            store.set_epoch_deadline(CALL_EPOCHS);
            let format = api
                .call_configure(&mut store, &config)?
                .map_err(anyhow::Error::msg)?;
            let clock = format.map(Clock::new).transpose()?;
            Ok((Plugin { store, api, clock }, sections))
        })();
        self.watchdog.send(())?;
        loaded
    }

    fn render(
        &self,
        plugin: &mut Plugin,
        view: &ViewSnapshot,
        fields: &BTreeMap<String, String>,
        home: Option<&str>,
        scheme: &'static Scheme,
    ) -> Result<Option<Row>> {
        let view = self::ActiveViewInput::from_view(view, home);
        let fields = fields
            .iter()
            .map(|(name, value)| Field {
                name: name.clone(),
                value: value.clone(),
            })
            .collect::<Vec<_>>();
        let time = plugin.clock.as_ref().map(Clock::value);
        self.watchdog.send(())?;
        plugin.store.data_mut().scheme = Some(scheme);
        plugin.store.set_epoch_deadline(CALL_EPOCHS);
        let row = plugin
            .api
            .call_render(&mut plugin.store, &view, &fields, time.as_deref());
        plugin.store.data_mut().scheme = None;
        self.watchdog.send(())?;
        let row = row?.map_err(anyhow::Error::msg)?;
        if let Some(row) = &row {
            validate_row(row)?;
        }
        Ok(row)
    }
}

use self::power::terminal_ui::types::ActiveView as ActiveViewInput;

impl ActiveViewInput {
    fn from_view(view: &ViewSnapshot, home: Option<&str>) -> Self {
        Self {
            session: view.session.clone(),
            worktree_name: view.worktree_name.clone(),
            worktree_path: view.worktree_path.clone(),
            repository_path: view.repository_path.clone(),
            agent_type: view.agent_type.clone(),
            agent_task: view.agent_task.clone(),
            agent_model: view.agent_model.clone(),
            home: home.map(str::to_owned),
        }
    }
}

struct Plugin {
    store: Store<HostState>,
    api: TerminalBottomWithPalette,
    clock: Option<Clock>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiColor {
    Foreground,
    Background,
    Accent,
    Muted,
    Surface,
    Raised,
    Contrast,
    Ansi(usize),
    Hex(u32),
}

impl UiColor {
    pub fn parse(value: &str) -> Result<Self> {
        const ANSI: [&str; 16] = [
            "black",
            "red",
            "green",
            "yellow",
            "blue",
            "magenta",
            "cyan",
            "white",
            "bright-black",
            "bright-red",
            "bright-green",
            "bright-yellow",
            "bright-blue",
            "bright-magenta",
            "bright-cyan",
            "bright-white",
        ];
        Ok(match value {
            "foreground" => Self::Foreground,
            "background" => Self::Background,
            "accent" => Self::Accent,
            "muted" => Self::Muted,
            "surface" => Self::Surface,
            "raised" => Self::Raised,
            "contrast" => Self::Contrast,
            hex if hex.starts_with('#') && hex.len() == 7 => {
                Self::Hex(u32::from_str_radix(&hex[1..], 16)?)
            }
            name => Self::Ansi(
                ANSI.iter()
                    .position(|color| *color == name)
                    .with_context(|| format!("Unknown color {name:?}"))?,
            ),
        })
    }
}

fn validate_row(row: &Row) -> Result<()> {
    let line = |text: &str, max| text.len() <= max && !text.chars().any(char::is_control);
    ensure!(line(&row.separator, 32), "Invalid status separator");
    ensure!(
        row.left.len() + row.right.len() <= 48,
        "Too many status segments"
    );
    ensure!(
        row.left
            .iter()
            .chain(&row.right)
            .map(|segment| segment.text.len())
            .sum::<usize>()
            <= 64 * 1024,
        "Status text exceeds the row size limit"
    );
    for segment in row.left.iter().chain(&row.right) {
        ensure!(
            line(&segment.text, 16 * 1024),
            "Invalid status segment text"
        );
        for color in [segment.fg.as_deref(), segment.bg.as_deref()]
            .into_iter()
            .flatten()
        {
            ensure!(line(color, 32), "Invalid status segment color");
            UiColor::parse(color)?;
        }
    }
    if let Some(command) = &row.actions {
        ensure!(
            line(&command.title, 100) && command.choices.len() <= 20,
            "Invalid status command"
        );
        for choice in &command.choices {
            ensure!(
                line(&choice.title, 100)
                    && line(&choice.detail, 16 * 1024)
                    && choice.copy_text.len() <= 16 * 1024,
                "Invalid status command choice"
            );
        }
    }
    Ok(())
}

fn run<S: Send + Sync + 'static>(
    receiver: mpsc::Receiver<Event>,
    updates: &async_channel::Sender<UiUpdate<S>>,
    field_requests: async_channel::Sender<(u64, ViewSnapshot)>,
    defaults: Sections,
    validate: fn(&Sections) -> Result<S>,
) -> Result<()> {
    let runtime = Runtime::new()?;
    let directory = config::directory();
    let home = env::var("HOME").ok();
    let (bundled, defaults) = runtime.load(None, &defaults)?;
    let mut plugin = if let Some(directory) = directory.as_deref() {
        match runtime
            .load(Some(directory), &defaults)
            .and_then(|(plugin, sections)| Ok((plugin, validate(&sections)?)))
        {
            Ok((plugin, settings)) => {
                updates.send_blocking(UiUpdate::Settings(settings))?;
                plugin
            }
            Err(error) => {
                updates.send_blocking(UiUpdate::Error(Some(format!(
                    "Status UI configuration failed. {error:#}"
                ))))?;
                bundled
            }
        }
    } else {
        updates.send_blocking(UiUpdate::Settings(validate(&defaults)?))?;
        bundled
    };
    let mut active = None;
    let mut fields = BTreeMap::new();
    let mut revision = 0;
    let mut fields_revision = 0;
    let mut scheme = theme::default_scheme();
    let mut next_tick: Option<Instant> = None;
    loop {
        let message = if let Some(deadline) = next_tick {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        } else {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        let mut replacement_row = None;
        match message {
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Event::View(next, view, next_scheme)) => {
                active = view;
                fields.clear();
                revision = next;
                fields_revision = next;
                scheme = next_scheme;
                if let Some(view) = &active {
                    let _ = field_requests.force_send((revision, view.clone()));
                }
            }
            Ok(Event::Theme(next, next_scheme)) => {
                revision = next;
                scheme = next_scheme;
            }
            Ok(Event::Fields(next, values)) if next == fields_revision => fields = values,
            Ok(Event::Fields(..)) => continue,
            Ok(Event::Reload) => {
                let replacement = runtime.load(directory.as_deref(), &defaults).and_then(
                    |(mut candidate, sections)| {
                        let settings = validate(&sections)?;
                        let row = active
                            .as_ref()
                            .map(|view| {
                                runtime.render(
                                    &mut candidate,
                                    view,
                                    &fields,
                                    home.as_deref(),
                                    scheme,
                                )
                            })
                            .transpose()?
                            .flatten();
                        Ok((candidate, row, settings))
                    },
                );
                match replacement {
                    Ok((candidate, row, settings)) => {
                        plugin = candidate;
                        replacement_row = Some(row);
                        updates.send_blocking(UiUpdate::Settings(settings))?;
                        updates.send_blocking(UiUpdate::Error(None))?;
                    }
                    Err(error) => {
                        updates.send_blocking(UiUpdate::Error(Some(format!(
                            "Status UI configuration failed. {error:#}"
                        ))))?;
                        continue;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let row = match replacement_row {
            Some(row) => Ok(row),
            None => active
                .as_ref()
                .map(|view| runtime.render(&mut plugin, view, &fields, home.as_deref(), scheme))
                .transpose()
                .map(Option::flatten),
        };
        match row {
            Ok(row) => {
                updates.send_blocking(UiUpdate::Row { revision, row })?;
                next_tick = active
                    .as_ref()
                    .and(plugin.clock.as_ref())
                    .map(|clock| Instant::now() + clock.next_tick() + Duration::from_millis(5));
            }
            Err(error) => {
                updates.send_blocking(UiUpdate::Error(Some(format!(
                    "Status UI plugin failed. {error:#}"
                ))))?;
                next_tick = None;
            }
        }
    }
    Ok(())
}

fn publish_fields(
    requests: async_channel::Receiver<(u64, ViewSnapshot)>,
    events: mpsc::Sender<Event>,
) {
    let directory = match env::var_os("POWER_STATUS_PLUGIN_DIR") {
        Some(directory) if directory.is_empty() => return,
        Some(directory) => PathBuf::from(directory),
        None => {
            let Some(directory) = config::directory()
                .map(|dir| dir.join("status-plugins"))
                .filter(|dir| dir.is_dir())
            else {
                return;
            };
            directory
        }
    };
    let mut worker = None;
    let mut retry_after = Instant::now();
    while let Ok((revision, view)) = requests.recv_blocking() {
        if Instant::now() < retry_after {
            continue;
        }
        let result = (|| {
            let worker = match &mut worker {
                Some(worker) => worker,
                slot @ None => slot.insert(StatusBarPlugins::start(&directory)?),
            };
            worker.fields(view)
        })();
        match result {
            Ok(fields) => {
                if events.send(Event::Fields(revision, fields)).is_err() {
                    break;
                }
            }
            Err(error) => {
                eprintln!("Status field plugin host failed. {error:#}");
                worker = None;
                retry_after = Instant::now() + Duration::from_secs(3);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn view() -> ViewSnapshot {
        ViewSnapshot {
            session: "active".into(),
            worktree_name: "fix-webm".into(),
            worktree_path: "/home/test/trees/fix-webm".into(),
            repository_path: Some("/home/test/power".into()),
            agent_type: "Codex".into(),
            agent_task: Some("Fix WebM playback".into()),
            agent_model: None,
        }
    }

    #[test]
    fn bundled_component_owns_the_row_and_copy_actions() -> Result<()> {
        let runtime = Runtime::new()?;
        let (mut plugin, _) = runtime.load(None, &Sections::new())?;
        let row = runtime
            .render(
                &mut plugin,
                &view(),
                &BTreeMap::new(),
                Some("/home/test"),
                theme::default_scheme(),
            )?
            .context("Missing bundled row")?;
        assert_eq!(row.left.len(), 2);
        assert_eq!(row.left[0].text, "▣ power");
        assert_eq!(row.left[1].text, " fix-webm");
        assert_eq!(row.separator, "");
        assert_eq!(row.left[0].bg.as_deref(), Some("accent"));
        assert_eq!(row.right[0].text, "Fix WebM playback");
        assert_eq!(row.right[1].text, "Codex");
        assert_eq!(row.right[1].bg.as_deref(), Some("bright-blue"));
        let mut with_model = view();
        with_model.agent_model = Some("gpt-6".into());
        let model_row = runtime
            .render(
                &mut plugin,
                &with_model,
                &BTreeMap::new(),
                None,
                theme::default_scheme(),
            )?
            .context("Missing model row")?;
        assert_eq!(model_row.right[1].text, "gpt-6");
        assert_eq!(model_row.right[2].text, "Codex");
        let mut shell = view();
        shell.agent_type = "Shell".into();
        shell.agent_task = None;
        let row_for_shell = runtime
            .render(
                &mut plugin,
                &shell,
                &BTreeMap::new(),
                Some("/home/test"),
                theme::default_scheme(),
            )?
            .context("Missing shell row")?;
        assert_eq!(row_for_shell.right.len(), 1);
        assert_eq!(row_for_shell.right[0].bg.as_deref(), Some("accent"));
        assert!(plugin.clock.is_none());
        let actions = row.actions.context("Missing plugin actions")?;
        assert_eq!(actions.choices[0].copy_text, "/home/test/trees/fix-webm");
        assert_eq!(actions.choices[1].copy_text, "/home/test/power");
        Ok(())
    }

    #[test]
    fn resolved_lua_config_reaches_the_component() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("config.lua");
        fs::write(
            &config,
            r##"
            power_bar.left = {}
            power_bar.right = {{ text = "{time}" }, { text = "{agent.model}" }, { icon = "", text = "{custom.tag}", bg = "agent" }}
            power_bar.clock.format = "%H:%M:%S"
            power_bar.separator = ""
            power_bar.agent_colors.Codex = "#62a0ea"
            keys.tab.paths = "z"
            "##,
        )?;
        let runtime = Runtime::new()?;
        let defaults =
            Sections::from([("keys".into(), serde_json::json!({"tab": {"paths": "y"}}))]);
        let (mut plugin, sections) = runtime.load(Some(dir.path()), &defaults)?;
        assert_eq!(sections["keys"]["tab"]["paths"], "z");
        let fields = BTreeMap::from([("tag".into(), "sample".into())]);
        let row = runtime
            .render(&mut plugin, &view(), &fields, None, theme::default_scheme())?
            .context("Missing configured row")?;
        assert!(row.left.is_empty());
        assert_eq!(row.right.len(), 2);
        assert_eq!(row.right[0].text.matches(':').count(), 2);
        assert_eq!(row.separator, "");
        assert_eq!(row.right[1].text, " sample");
        assert_eq!(row.right[1].bg.as_deref(), Some("#62a0ea"));

        fs::write(&config, r#"power_bar.right = {{ text = "{unknown}" }}"#)?;
        assert!(runtime.load(Some(dir.path()), &Sections::new()).is_err());
        Ok(())
    }

    #[test]
    fn full_path_templates_can_be_used_with_claudes_task() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("config.lua"),
            r#"
            power_bar.left = {
                { icon = "▣", text = "{repo.path}" },
                { icon = "", text = "{worktree.path}" },
            }
            power_bar.right = {{ text = "{agent.task}" }}
            "#,
        )?;
        let runtime = Runtime::new()?;
        let (mut plugin, _) = runtime.load(Some(dir.path()), &Sections::new())?;
        let mut claude = view();
        claude.agent_type = "Claude".into();
        let row = runtime
            .render(
                &mut plugin,
                &claude,
                &BTreeMap::new(),
                None,
                theme::default_scheme(),
            )?
            .context("Missing custom row")?;
        assert_eq!(row.left[0].text, "▣ /home/test/power");
        assert_eq!(row.left[1].text, " /home/test/trees/fix-webm");
        assert_eq!(row.right[0].text, "Fix WebM playback");
        let actions = row.actions.context("Missing full paths")?;
        assert_eq!(actions.choices[1].copy_text, "/home/test/power");
        Ok(())
    }

    #[test]
    fn bundled_plugin_can_request_the_palette_for_the_current_render() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("config.lua"),
            r#"power_bar.left = {{ text = "{theme.name}", bg = "accent" }}"#,
        )?;
        let runtime = Runtime::new()?;
        let (mut plugin, _) = runtime.load(Some(dir.path()), &Sections::new())?;
        let view = view();
        for scheme in [
            theme::default_scheme(),
            theme::find(theme::DEFAULT_LIGHT).unwrap(),
        ] {
            let row = runtime
                .render(&mut plugin, &view, &BTreeMap::new(), None, scheme)?
                .context("Missing row")?;
            assert_eq!(row.left[0].text, scheme.name);
            assert_eq!(row.left[0].bg.as_deref(), Some("accent"));
            assert!(plugin.store.data().scheme.is_none());
        }
        Ok(())
    }
}
