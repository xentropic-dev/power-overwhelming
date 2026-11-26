#[path = "agent_tree.rs"]
pub(crate) mod agent_tree;
#[path = "dag_view.rs"]
mod dag_view;

use self::{agent_tree::AgentTree, dag_view::DagView};
use crate::{
    keymap::{Command, Keymap, Scope},
    picker::{Choice, Kind, Picker, Row},
    preview::{Preview, PreviewAction, decode_image},
    settings::{AutoOpen, MAX_FONT_SIZE, MIN_FONT_SIZE, Previews, Settings, TerminalSettings},
    status_bar::{ConfigChanged, StatusBar},
    terminal::{AgentChanged, BarContextChanged, OpenLink, TerminalView, load_bundled_fonts},
};
use anyhow::{Context as _, Result, ensure};
use application::{ResolveArtifact, StartSession, agents::AgentProfileRegistry};
use domain::{
    artifact::{ResolvedArtifact, Target},
    dag::{AgentStatus, Coordinator, RunStatus},
    link::dag_run_id,
    session::{Agent, Session},
};
use gpui::{prelude::*, *};
use infrastructure::{
    agents::AgentProfiles,
    box_icons::icon_path,
    boxes::{Layout, Registry, SavedBox, SavedView},
    client::{
        self, Attempt, ManagedAgent, ManagedWorktree, Run, Task, WorktreeCreator, WorktreeScope,
    },
    plugins::ActiveView,
    shell::{self, Shell},
    theme::{self, Colors, Scheme},
    workspace::{
        Checkout, WorktreeHead, main_checkout, repository_worktree_details, repository_worktrees,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

const SHORTCUTS: [u8; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 0];

struct DesktopScheme(&'static Scheme);
impl Global for DesktopScheme {}

pub(crate) fn colors(cx: &App) -> &'static Colors {
    &cx.global::<DesktopScheme>().0.colors
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prefix {
    Box,
    View,
}

enum DagQuery {
    Runs {
        box_id: String,
        include_finished: bool,
    },
    Run {
        id: String,
        repository: PathBuf,
    },
    Agents {
        box_id: String,
        include_finished: bool,
    },
}

enum DagSnapshot {
    Runs(Vec<Run>),
    Run(Run, Box<DagWorktrees>),
    Agents(Vec<ManagedAgent>),
}

/// The empty picker text, naming the key that shows finished items while they are hidden.
fn empty_message(
    none: &str,
    none_live: &str,
    show_finished: bool,
    toggle: &str,
    finished: &str,
) -> String {
    if show_finished {
        none.to_owned()
    } else {
        format!("{none_live} {toggle} shows {finished}.")
    }
}

/// What one press of the end-worker key does to the selected agent.
#[derive(Debug, Eq, PartialEq)]
enum EndWorker {
    NoSession,
    /// The worker is still busy; the next press of the key ends it.
    Confirm(String),
    End(String),
}

/// A still-working worker needs a second press, so a stray key never cuts it off.
fn end_worker_step(agent: &ManagedAgent, armed: Option<&str>, key: &str) -> EndWorker {
    let Some(session) = agent.session.clone() else {
        return EndWorker::NoSession;
    };
    if armed == Some(agent.id.as_str()) {
        return EndWorker::End(session);
    }
    match agent.status {
        AgentStatus::Working | AgentStatus::NeedsInput => EndWorker::Confirm(format!(
            "Worker is still working — press {key} again to end it."
        )),
        // Idle agents are interactive sessions such as coordinators, not finished workers.
        AgentStatus::Idle => EndWorker::Confirm(format!(
            "This agent has no task and may be in use — press {key} again to end its session."
        )),
        _ => EndWorker::End(session),
    }
}

fn running_progress(run: &Run) -> Option<String> {
    if run.tasks.is_empty() || run.status() != RunStatus::Active {
        return None;
    }
    Some(format!(
        "DAG {}/{} done",
        run.completed_tasks(),
        run.tasks.len()
    ))
}

fn session_dag_progress(box_id: String, session: &str) -> Result<Option<String>> {
    // A worker whose attempt is done keeps its session, so finished agents count here.
    let agents = client::list_agents(Some(box_id.clone()), None, true)?;
    let Some(agent) = agents
        .into_iter()
        .find(|agent| agent.session.as_deref() == Some(session))
    else {
        return Ok(None);
    };
    if let Some(run) = agent.run {
        return Ok(running_progress(&client::get_run(run)?));
    }
    let runs = client::list_runs(Some(box_id), None, false)?;
    Ok(runs
        .iter()
        .filter(
            |run| matches!(&run.coordinator, Coordinator::Agent { agent: id } if id == &agent.id),
        )
        .filter_map(|run| Some((run.created_at, running_progress(run)?)))
        .max_by_key(|(created_at, _)| *created_at)
        .map(|(_, progress)| progress))
}

#[derive(Default, PartialEq)]
struct DagWorktrees {
    heads: HashMap<PathBuf, WorktreeHead>,
    managed: Option<HashMap<PathBuf, ManagedWorktree>>,
}

impl DagWorktrees {
    fn load(repository: &Path) -> Self {
        Self {
            heads: repository_worktree_details(repository)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|worktree| Some((worktree.path, worktree.head?)))
                .collect(),
            managed: managed_worktrees(repository).ok(),
        }
    }

    fn details(&self, run: &str, task: &Task, attempt: &Attempt) -> (String, &'static str) {
        let worktree = self
            .managed
            .as_ref()
            .and_then(|worktrees| worktrees.get(&attempt.worktree));
        let branch = match self.heads.get(&attempt.worktree) {
            Some(WorktreeHead::Branch(branch)) => branch.clone(),
            Some(WorktreeHead::Detached) => "Detached HEAD".into(),
            None => {
                worktree.map_or_else(|| "Unavailable".into(), |worktree| worktree.branch.clone())
            }
        };
        let creator = match worktree.map(|worktree| &worktree.creator) {
            Some(WorktreeCreator::Attempt {
                run: creator_run,
                task: creator_task,
                ..
            }) if creator_run == run && creator_task == &task.id => "Yes, for this task",
            Some(WorktreeCreator::Attempt { .. }) => "Yes, for another task",
            Some(WorktreeCreator::View { .. }) => "Yes, from a tab",
            None if self.managed.is_some() => "No",
            None => "Unavailable",
        };
        (branch, creator)
    }
}

#[derive(Debug, Eq, PartialEq)]
enum LinkAction<'a> {
    OpenDag(&'a str),
    ResolveArtifact,
}

fn link_action(target: &str) -> LinkAction<'_> {
    dag_run_id(target).map_or(LinkAction::ResolveArtifact, LinkAction::OpenDag)
}

struct TerminalTab {
    view: Entity<TerminalView>,
    closing: bool,
    _links: Subscription,
    _agent: Subscription,
    _bar_context: Subscription,
}

struct PreviewTab {
    id: u64,
    box_id: String,
    title: String,
    target: Option<Target>,
    originating_terminal: String,
    preview: Preview,
}

struct Workspace {
    registry: Option<Registry>,
    layout: Layout,
    /// Custom icons set with `power box icon`, by box ID. Other boxes show an identicon.
    box_icons: HashMap<String, Arc<RenderImage>>,
    sessions: HashMap<String, Session>,
    terminals: HashMap<String, TerminalTab>,
    tab_scroll: ScrollHandle,
    launching: bool,
    focus: FocusHandle,
    prefix: Option<Prefix>,
    picker: Option<Picker>,
    dag: Option<DagView>,
    dag_refresh: async_channel::Sender<()>,
    dag_progress: Option<(String, String)>,
    attention: HashMap<String, HashSet<String>>,
    previews: Vec<PreviewTab>,
    active_previews: HashMap<String, u64>,
    next_id: u64,
    error: Option<String>,
    save_error: Option<String>,
    status_bar: Entity<StatusBar>,
    main_checkouts: HashMap<PathBuf, Option<PathBuf>>,
    config_error: Option<String>,
    keymap: Keymap,
    preview_settings: Previews,
    terminal_settings: TerminalSettings,
    /// Logical pixels the zoom keys add to `terminal.font_size`, for every terminal.
    terminal_zoom: f32,
    agent_profiles: AgentProfiles,
    config_scheme: Option<&'static Scheme>,
    settings_ready: bool,
    daemon_theme: async_channel::Sender<&'static Scheme>,
    preview_focus: FocusHandle,
    /// The terminal showing numbered link hints, if any.
    hinting: Option<Entity<TerminalView>>,
    _bar_config: Subscription,
}

pub fn run() {
    Application::new().run(|cx| {
        load_bundled_fonts(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Power Overwhelming".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1260.), px(800.)),
                cx,
            ))),
            ..Default::default()
        };
        let result = cx.open_window(options, |window, cx| {
            cx.new(|cx| Workspace::new(window, cx))
        });
        if let Err(error) = result {
            eprintln!("Could not open the desktop window. {error:#}");
            cx.quit();
        } else {
            cx.activate(true);
        }
    });
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        cx.set_global(DesktopScheme(theme::default_scheme()));
        let (daemon_theme, changes) = async_channel::bounded::<&'static Scheme>(1);
        thread::spawn(move || {
            while let Ok(scheme) = changes.recv_blocking() {
                if let Err(error) = client::set_color_scheme(&scheme.name) {
                    eprintln!("Could not update daemon color scheme. {error:#}");
                }
            }
        });
        let load = cx.background_executor().spawn(async {
            let (registry, layout) = Registry::open()?;
            let icons = layout
                .boxes
                .iter()
                .filter_map(|entry| Some((entry.id.clone(), load_box_icon(&entry.id)?)))
                .collect::<HashMap<_, _>>();
            client::ensure_daemon()?;
            Ok::<_, anyhow::Error>((registry, layout, icons, client::list_sessions()?))
        });
        cx.spawn_in(window, async move |app, cx| {
            let result = load.await;
            let _ = app.update_in(cx, |this, window, cx| {
                match result {
                    Ok((registry, layout, icons, sessions)) => {
                        this.box_icons = icons;
                        this.load(registry, layout, sessions, window, cx);
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        Self::watch_artifacts(window, cx);
        Self::watch_box_icons(window, cx);
        let dag_refresh = Self::watch_dag(window, cx);
        let status_bar =
            cx.new(|cx| StatusBar::new(window, cx, Settings::defaults(), Settings::from_sections));
        let bar_config = cx.subscribe_in(
            &status_bar,
            window,
            |app, _, changed: &ConfigChanged<Settings>, window, cx| {
                match changed {
                    ConfigChanged::Settings(settings) => {
                        app.keymap = settings.keymap.clone();
                        app.preview_settings = settings.previews.clone();
                        app.terminal_settings = settings.terminal.clone();
                        app.apply_terminal_settings(cx);
                        app.agent_profiles = settings.agents.clone();
                        app.config_scheme = settings.scheme;
                        app.apply_preferred_scheme(cx);
                    }
                    ConfigChanged::Error(error) => app.config_error = error.clone(),
                }
                if !app.settings_ready {
                    app.settings_ready = true;
                    app.show_startup(window, cx);
                }
                cx.notify();
            },
        );
        Self {
            registry: None,
            layout: Layout::default(),
            box_icons: HashMap::new(),
            sessions: HashMap::new(),
            terminals: HashMap::new(),
            tab_scroll: ScrollHandle::new(),
            launching: false,
            focus,
            prefix: None,
            picker: None,
            dag: None,
            dag_refresh,
            dag_progress: None,
            attention: HashMap::new(),
            previews: Vec::new(),
            active_previews: HashMap::new(),
            next_id: 0,
            error: None,
            save_error: None,
            status_bar,
            keymap: Keymap::default(),
            preview_settings: Previews::default(),
            terminal_settings: TerminalSettings::default(),
            terminal_zoom: 0.,
            agent_profiles: AgentProfiles::default(),
            config_scheme: None,
            settings_ready: false,
            daemon_theme,
            preview_focus: cx.focus_handle(),
            hinting: None,
            main_checkouts: HashMap::new(),
            config_error: None,
            _bar_config: bar_config,
        }
    }

    fn load(
        &mut self,
        registry: Registry,
        mut layout: Layout,
        sessions: Vec<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.registry = Some(registry);
        let _ = self.dag_refresh.try_send(());
        self.sessions = sessions
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect();
        let mut changed = false;
        for entry in &mut layout.boxes {
            for index in (0..entry.views.len()).rev() {
                if !self.sessions.contains_key(&entry.views[index].session) {
                    entry.remove_view(index);
                    changed = true;
                }
            }
        }
        self.layout = layout;
        self.apply_preferred_scheme(cx);
        let _ = self.daemon_theme.force_send(cx.global::<DesktopScheme>().0);
        for repository in self
            .open_boxes()
            .into_iter()
            .map(|entry| entry.repository.clone())
            .collect::<Vec<_>>()
        {
            self.load_main_checkout(repository, cx);
        }
        if !self
            .active_box()
            .is_some_and(|entry| entry.shortcut.is_some())
        {
            self.layout.active_box = self.open_boxes().first().map(|entry| entry.id.clone());
            changed = true;
        }
        if changed && !self.layout.boxes.is_empty() {
            self.save();
        }
        self.show_startup(window, cx);
    }

    fn show_startup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.registry.is_none() || !self.settings_ready {
            return;
        }
        if !self.layout.onboarded {
            self.pick_color_scheme(true, window, cx);
            return;
        }
        self.open_initial_view(window, cx);
    }

    fn open_initial_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_box().is_some() {
            self.restore_focus(window, cx);
        } else if self.layout.boxes.is_empty() {
            self.pick_repository(window, cx);
        } else {
            self.pick_boxes(window, cx);
        }
    }

    fn apply_preferred_scheme(&mut self, cx: &mut Context<Self>) {
        let scheme = self
            .config_scheme
            .or_else(|| self.layout.color_scheme.as_deref().and_then(theme::find))
            .unwrap_or_else(theme::default_scheme);
        self.apply_scheme(scheme, cx);
    }

    fn apply_scheme(&mut self, scheme: &'static Scheme, cx: &mut Context<Self>) {
        if std::ptr::eq(scheme, cx.global::<DesktopScheme>().0) {
            return;
        }
        cx.set_global(DesktopScheme(scheme));
        for terminal in self.terminals.values() {
            terminal
                .view
                .update(cx, |view, cx| view.set_scheme(scheme, cx));
        }
        if self.registry.is_some() {
            let _ = self.daemon_theme.force_send(scheme);
        }
        self.sync_status_bar(cx);
        cx.refresh_windows();
        cx.notify();
    }

    /// The terminal settings with the zoom added to the font size.
    fn zoomed_terminal_settings(&self) -> TerminalSettings {
        let mut settings = self.terminal_settings.clone();
        settings.font_size =
            (settings.font_size + self.terminal_zoom).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        settings
    }

    fn apply_terminal_settings(&mut self, cx: &mut Context<Self>) {
        let settings = self.zoomed_terminal_settings();
        for terminal in self.terminals.values() {
            terminal
                .view
                .update(cx, |view, cx| view.set_settings(settings.clone(), cx));
        }
    }

    /// Grows or shrinks every terminal's text by `step` logical pixels, or resets it.
    fn zoom_terminals(&mut self, step: Option<f32>, cx: &mut Context<Self>) {
        let size = self.terminal_settings.font_size;
        self.terminal_zoom = match step {
            Some(step) => {
                (size + self.terminal_zoom + step).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE) - size
            }
            None => 0.,
        };
        self.apply_terminal_settings(cx);
    }

    fn pick_color_scheme(&mut self, first_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let active = cx.global::<DesktopScheme>().0;
        let rows = std::iter::once(active)
            .chain(
                theme::schemes()
                    .iter()
                    .filter(|scheme| !std::ptr::eq(*scheme, active)),
            )
            .map(|scheme| Row {
                label: scheme.name.clone(),
                detail: if scheme.is_light() { "Light" } else { "Dark" }.into(),
                choice: Choice::ColorScheme(scheme),
            })
            .collect();
        self.open_picker(Kind::ColorSchemes { first_run }, rows, window, cx);
    }

    fn finish_theme_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if Shell::detect().is_some_and(Shell::installed) {
            self.finish_onboarding(false, window, cx);
        } else {
            self.open_setup(true, window, cx);
        }
    }

    fn open_setup(&mut self, first_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let shell = Shell::detect();
        let rc = shell.and_then(Shell::rc_file);
        let mut lines = vec![
            "Power can wrap coding agents in its terminals, so agents you start from a \
             shell get the list of bundled Power skills."
                .to_owned(),
        ];
        let rows = match (shell, &rc) {
            (Some(shell), Some(rc)) if shell.installed() => {
                lines.push(format!(
                    "Shell integration is installed in {}.",
                    rc.display()
                ));
                vec![Row {
                    label: "Close".into(),
                    detail: String::new(),
                    choice: Choice::Setup(false),
                }]
            }
            (Some(shell), Some(rc)) => {
                lines.push(format!("This appends one line to {}:", rc.display()));
                lines.push(format!("    {}", shell.hook()));
                lines.push("The line does nothing outside Power terminals.".to_owned());
                vec![
                    Row {
                        label: "Install shell integration".into(),
                        detail: format!("Edit {}", rc.display()),
                        choice: Choice::Setup(true),
                    },
                    Row {
                        label: "Skip".into(),
                        detail: format!(
                            "Press {} {} later to install it",
                            self.keymap.first(Scope::Prefix, Command::BoxPrefix),
                            self.keymap.first(Scope::Box, Command::BoxSetup),
                        ),
                        choice: Choice::Setup(false),
                    },
                ]
            }
            _ => {
                lines.push(
                    "Shell integration supports bash, zsh and fish. Tabs still show \
                     which agent is running."
                        .to_owned(),
                );
                vec![Row {
                    label: "Continue".into(),
                    detail: String::new(),
                    choice: Choice::Setup(false),
                }]
            }
        };
        for profile in self.agent_profiles.profiles() {
            if !shell::agent_available(profile) {
                lines.push(format!("Could not find executable {}.", profile.command));
            }
        }
        let key = |scope, command| self.keymap.first(scope, command);
        lines.push(format!(
            "{}/{} or {}/{} to move · {} to choose · {} to skip",
            key(Scope::Dialog, Command::DialogDown),
            key(Scope::Dialog, Command::DialogUp),
            key(Scope::Picker, Command::PickerDown),
            key(Scope::Picker, Command::PickerUp),
            key(Scope::Picker, Command::PickerSelect),
            key(Scope::Picker, Command::PickerCancel),
        ));
        self.open_picker(Kind::Setup { lines, first_run }, rows, window, cx);
    }

    fn finish_onboarding(&mut self, install: bool, window: &mut Window, cx: &mut Context<Self>) {
        if install
            && let Some(shell) = Shell::detect()
            && let Err(error) = shell.install()
        {
            self.show_error(
                format!("Could not install shell integration. {error:#}"),
                cx,
            );
            return;
        }
        let first_run = self.picker.as_ref().is_none_or(|picker| {
            matches!(
                picker.kind,
                Kind::Setup {
                    first_run: true,
                    ..
                }
            )
        });
        self.picker = None;
        if !first_run {
            self.restore_focus(window, cx);
            return;
        }
        self.layout.onboarded = true;
        self.save();
        self.open_initial_view(window, cx);
    }

    fn active_view(&self) -> Option<String> {
        self.active_box()?.active_view.clone()
    }

    /// Numbers the links on screen. Digits then open one, the search key opens the search.
    fn start_hints(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_view().and_then(|id| self.terminals.get(&id)) else {
            return;
        };
        let search = self.keymap.first(Scope::Hints, Command::HintSearch);
        let cancel = self.keymap.first(Scope::Hints, Command::HintCancel);
        tab.view.update(cx, |view, cx| {
            view.show_hints(|count| match count {
                0 => format!("No links on screen · {search} search · {cancel} cancel"),
                1 => format!("1 open · {search} search · {cancel} cancel"),
                count => format!("1–{count} open · {search} search · {cancel} cancel"),
            });
            cx.notify();
        });
        self.hinting = Some(tab.view.clone());
    }

    fn end_hints(&mut self, cx: &mut Context<Self>) -> Option<Entity<TerminalView>> {
        let view = self.hinting.take()?;
        view.update(cx, |view, cx| {
            view.clear_hints();
            cx.notify();
        });
        Some(view)
    }

    fn hint_key(&mut self, stroke: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let mods = stroke.modifiers;
        let digit = (!mods.control && !mods.alt && !mods.platform)
            .then(|| shortcut_digit(&stroke.key))
            .flatten()
            .filter(|digit| *digit != 0);
        let command = self.keymap.command(Scope::Hints, stroke);
        // Read the chosen link before ending hint mode, which clears the numbered links.
        let chosen = self.hinting.as_ref().and_then(|view| {
            let view = view.read(cx);
            let link = view.hint(usize::from(digit?))?;
            Some((view.session.id.clone(), link))
        });
        if self.end_hints(cx).is_none() {
            return;
        }
        if digit.is_some() {
            if let Some((session, link)) = chosen {
                self.resolve(session, link, true, window, cx);
            }
        } else if command == Some(Command::HintSearch) {
            self.search_links(window, cx);
        }
    }

    /// Searches this box's published artifacts, newest first, then other links on screen.
    fn search_links(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.active_box() else {
            return;
        };
        let tabs: HashMap<String, usize> = entry
            .views
            .iter()
            .enumerate()
            .map(|(index, view)| (view.session.clone(), index + 1))
            .collect();
        let visible = self
            .active_view()
            .and_then(|id| self.terminals.get(&id))
            .map(|tab| tab.view.read(cx).visible_links())
            .unwrap_or_default();
        let limit = self.preview_settings.history;
        let id = self.open_picker(Kind::Links, Vec::new(), window, cx);
        self.load_picker_rows(
            id,
            "Looking up artifacts…",
            "No published artifacts or links on screen.",
            window,
            cx,
            move || {
                let published = client::recent_artifacts(tabs.keys().cloned().collect(), limit)?;
                let mut rows: Vec<Row> = published
                    .iter()
                    .map(|artifact| Row {
                        label: artifact.title.clone(),
                        detail: match tabs.get(&artifact.session) {
                            Some(tab) => format!("tab {tab} · {}", artifact.uri),
                            None => artifact.uri.clone(),
                        },
                        choice: Choice::Link(artifact.uri.clone()),
                    })
                    .collect();
                rows.extend(
                    visible
                        .into_iter()
                        .filter(|link| published.iter().all(|artifact| &artifact.uri != link))
                        .map(|link| Row {
                            label: link.clone(),
                            detail: "on screen".into(),
                            choice: Choice::Link(link),
                        }),
                );
                Ok(rows)
            },
        );
    }

    /// Streams publish events from the daemon for auto-open.
    fn watch_artifacts(window: &mut Window, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::bounded(64);
        std::thread::spawn(move || {
            client::watch_artifacts(|artifact| tx.send_blocking(artifact).is_ok());
        });
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(artifact) = rx.recv().await {
                if app
                    .update_in(cx, |this, window, cx| {
                        this.artifact_published(artifact, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Reloads a box's icon whenever an agent sets or resets it.
    fn watch_box_icons(window: &mut Window, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::bounded(16);
        std::thread::spawn(move || {
            client::watch_box_icons(|box_id| tx.send_blocking(box_id).is_ok());
        });
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(box_id) = rx.recv().await {
                let icon = cx.background_executor().spawn({
                    let box_id = box_id.clone();
                    async move { load_box_icon(&box_id) }
                });
                let icon = icon.await;
                if app
                    .update(cx, |this, cx| {
                        match icon {
                            Some(icon) => this.box_icons.insert(box_id, icon),
                            None => this.box_icons.remove(&box_id),
                        };
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Run events invalidate snapshots. One pending refresh is enough, even during a burst.
    fn watch_dag(window: &mut Window, cx: &mut Context<Self>) -> async_channel::Sender<()> {
        let (tx, rx) = async_channel::bounded(1);
        let watch = tx.clone();
        std::thread::spawn(move || {
            while !watch.is_closed() {
                let _ = client::watch_runs(None, |_| {
                    let _ = watch.try_send(());
                    !watch.is_closed()
                });
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        let poll = tx.clone();
        let executor = cx.background_executor().clone();
        cx.background_executor()
            .spawn(async move {
                // Also catch up after a disconnect or a change made before the stream subscribed.
                while !poll.is_closed() {
                    executor.timer(Duration::from_secs(5)).await;
                    let _ = poll.try_send(());
                }
            })
            .detach();
        cx.spawn_in(window, async move |app, cx| {
            while rx.recv().await.is_ok() {
                let Ok((query, view)) = app.update(cx, |this, _| {
                    let view = this
                        .active_box()
                        .and_then(|entry| Some((entry.id.clone(), entry.active_view.clone()?)));
                    (this.dag_query(), view)
                }) else {
                    break;
                };
                let progress = view.map(|(box_id, session)| {
                    cx.background_executor().spawn(async move {
                        session_dag_progress(box_id, &session).map(|progress| (session, progress))
                    })
                });
                let attention = cx
                    .background_executor()
                    .spawn(async { client::attention() });
                if let Some((id, query)) = query {
                    let snapshot = cx
                        .background_executor()
                        .spawn(async move {
                            match query {
                                DagQuery::Runs {
                                    box_id,
                                    include_finished,
                                } => client::list_runs(Some(box_id), None, include_finished)
                                    .map(DagSnapshot::Runs),
                                DagQuery::Run { id, repository } => {
                                    let run = client::get_run(id)?;
                                    let worktrees = DagWorktrees::load(&repository);
                                    Ok(DagSnapshot::Run(run, Box::new(worktrees)))
                                }
                                DagQuery::Agents {
                                    box_id,
                                    include_finished,
                                } => client::list_agents(Some(box_id), None, include_finished)
                                    .map(DagSnapshot::Agents),
                            }
                        })
                        .await;
                    if app
                        .update_in(cx, |this, window, cx| {
                            this.update_dag(id, snapshot, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                if let Some(progress) = progress
                    && let Ok((session, progress)) = progress.await
                    && app
                        .update(cx, |this, cx| {
                            if this.active_view().as_deref() == Some(&session) {
                                let progress = progress.map(|value| (session, value));
                                if this.dag_progress != progress {
                                    this.dag_progress = progress;
                                    this.sync_status_bar(cx);
                                }
                            }
                        })
                        .is_err()
                {
                    break;
                }
                let Ok(attention) = attention.await else {
                    continue;
                };
                let attention = attention
                    .into_iter()
                    .map(|entry| (entry.box_id, entry.sessions.into_iter().collect()))
                    .collect();
                if app
                    .update(cx, |this, cx| {
                        if this.attention != attention {
                            this.attention = attention;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        tx
    }

    fn dag_query(&self) -> Option<(u64, DagQuery)> {
        if let Some(dag) = &self.dag {
            return Some((
                dag.id,
                DagQuery::Run {
                    id: dag.run.id.clone(),
                    repository: self.active_box()?.repository.clone(),
                },
            ));
        }
        let picker = self.picker.as_ref()?;
        let include_finished = picker.show_finished;
        let query = match &picker.kind {
            // The first load lists every run, so a box with finished runs never skips the list.
            Kind::Runs { box_id, .. } => DagQuery::Runs {
                box_id: box_id.clone(),
                include_finished: include_finished || picker.loading,
            },
            Kind::Agents { box_id, .. } => DagQuery::Agents {
                box_id: box_id.clone(),
                include_finished,
            },
            _ => return None,
        };
        Some((picker.id, query))
    }

    fn update_dag(
        &mut self,
        id: u64,
        result: Result<DagSnapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dag) = self.dag.as_mut().filter(|dag| dag.id == id) {
            match result {
                Ok(DagSnapshot::Run(run, worktrees)) => {
                    let changed = dag.refresh_error.take().is_some() || dag.worktrees != *worktrees;
                    dag.worktrees = *worktrees;
                    if dag.run != run {
                        dag.set_run(run);
                    } else if !changed {
                        return;
                    }
                }
                Err(error) => {
                    let message = format!("Could not refresh run. {error:#}");
                    if dag.refresh_error.as_ref() == Some(&message) {
                        return;
                    }
                    dag.refresh_error = Some(message);
                }
                _ => return,
            }
        } else if let Some(picker) = self.picker.as_mut().filter(|picker| picker.id == id) {
            let initial = std::mem::take(&mut picker.loading);
            let toggle = |scope| self.keymap.first(scope, Command::ToggleFinished);
            match result {
                Ok(DagSnapshot::Runs(mut runs)) => {
                    // Skip the list only when the box has no other run, finished or not.
                    if initial && runs.len() == 1 {
                        if let Some(run) = runs.pop() {
                            self.open_dag(run, window, cx);
                        }
                        return;
                    }
                    if !picker.show_finished {
                        runs.retain(|run| !run.status().is_finished());
                    }
                    picker.message = runs.is_empty().then(|| {
                        empty_message(
                            "This box has no DAG runs.",
                            "This box has no unfinished DAG runs.",
                            picker.show_finished,
                            &toggle(Scope::Runs),
                            "completed runs",
                        )
                    });
                    picker.set_runs(runs);
                }
                Ok(DagSnapshot::Agents(agents)) => {
                    picker.message = agents.is_empty().then(|| {
                        empty_message(
                            "This box has no managed agents.",
                            "This box has no live managed agents.",
                            picker.show_finished,
                            &toggle(Scope::AgentTree),
                            "exited and done agents",
                        )
                    });
                    picker.set_agents(agents);
                }
                Err(error) => {
                    picker.accept_pending = false;
                    let name = if matches!(picker.kind, Kind::Agents { .. }) {
                        "agents"
                    } else {
                        "runs"
                    };
                    picker.message = Some(format!("Could not list {name}. {error:#}"));
                }
                _ => return,
            }
            if std::mem::take(&mut picker.accept_pending)
                && let Some(choice) = picker.selection()
            {
                self.choose(choice, window, cx);
            }
        } else {
            return;
        }
        cx.notify();
    }

    fn artifact_published(
        &mut self,
        artifact: application::RecentArtifact,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(box_id) = self
            .layout
            .boxes
            .iter()
            .find(|entry| {
                entry
                    .views
                    .iter()
                    .any(|view| view.session == artifact.session)
            })
            .map(|entry| entry.id.clone())
        else {
            return;
        };
        let open = match self.preview_settings.auto_open {
            AutoOpen::Always => true,
            AutoOpen::WhenPaneOpen => self.active_previews.contains_key(&box_id),
            AutoOpen::Never => false,
        };
        if open {
            self.resolve_in(box_id, artifact.session, artifact.uri, false, window, cx);
        }
    }

    fn focus_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_preview_index().is_some() {
            self.preview_focus.focus(window);
            cx.notify();
        }
    }

    fn preview_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_preview_index() else {
            self.restore_focus(window, cx);
            return;
        };
        let action = match command {
            Command::PreviewExit => return self.restore_focus(window, cx),
            Command::PreviewClose => return self.close_preview(active, window, cx),
            Command::PreviewPrevious | Command::PreviewNext => {
                let box_id = &self.previews[active].box_id;
                let indices = self
                    .previews
                    .iter()
                    .enumerate()
                    .filter_map(|(index, tab)| (&tab.box_id == box_id).then_some(index))
                    .collect::<Vec<_>>();
                let position = indices.iter().position(|index| *index == active).unwrap();
                let next = if command == Command::PreviewNext {
                    (position + 1) % indices.len()
                } else {
                    position.checked_sub(1).unwrap_or(indices.len() - 1)
                };
                return self.activate_preview(indices[next], cx);
            }
            Command::PreviewPlay => PreviewAction::TogglePlay,
            Command::PreviewBack => PreviewAction::Seek(-5),
            Command::PreviewForward => PreviewAction::Seek(5),
            Command::PreviewScrollDown => PreviewAction::Scroll(3.),
            Command::PreviewScrollUp => PreviewAction::Scroll(-3.),
            Command::PreviewReload => PreviewAction::Reload,
            Command::PreviewInteract => PreviewAction::Interact,
            _ => return,
        };
        if self.previews[active].preview.act(action, window, cx)
            && action == PreviewAction::Interact
        {
            cx.notify();
        }
    }

    fn open_help(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(Kind::Help(self.keymap.help()), Vec::new(), window, cx);
    }

    fn save(&mut self) {
        self.save_error = self
            .registry
            .as_ref()
            .and_then(|registry| registry.save(&self.layout).err())
            .map(|error| format!("{error:#}"));
    }

    fn active_box(&self) -> Option<&SavedBox> {
        self.layout
            .boxes
            .iter()
            .find(|entry| Some(&entry.id) == self.layout.active_box.as_ref())
    }

    fn box_mut(&mut self, id: &str) -> Option<&mut SavedBox> {
        self.layout.boxes.iter_mut().find(|entry| entry.id == id)
    }

    fn open_boxes(&self) -> Vec<&SavedBox> {
        let mut boxes: Vec<_> = self
            .layout
            .boxes
            .iter()
            .filter(|entry| entry.shortcut.is_some())
            .collect();
        boxes.sort_by_key(|entry| entry.shortcut.map(|digit| (digit == 0, digit)));
        boxes
    }

    fn free_shortcut(&self) -> Option<u8> {
        SHORTCUTS.into_iter().find(|digit| {
            !self
                .layout
                .boxes
                .iter()
                .any(|entry| entry.shortcut == Some(*digit))
        })
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn add_terminal(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.contains_key(id) {
            return;
        }
        let Some(session) = self.sessions.get(id).cloned() else {
            return;
        };
        let scheme = cx.global::<DesktopScheme>().0;
        let settings = self.zoomed_terminal_settings();
        let view = cx.new(|cx| TerminalView::new(session, scheme, settings, window, cx));
        let links = cx.subscribe_in(&view, window, |app, source, link: &OpenLink, window, cx| {
            let id = source.read(cx).session.id.clone();
            app.resolve(id, link.0.clone(), false, window, cx);
        });
        let agent = cx.subscribe(&view, |app, source, changed: &AgentChanged, cx| {
            let id = &source.read(cx).session.id;
            if let Some(session) = app.sessions.get_mut(id) {
                session.agent = changed.0.clone();
                cx.notify();
            }
        });
        let bar_context = cx.subscribe(&view, |app, source, _: &BarContextChanged, cx| {
            if app
                .active_box()
                .and_then(|entry| entry.active_view.as_ref())
                == Some(&source.read(cx).session.id)
            {
                app.sync_status_bar(cx);
            }
        });
        self.terminals.insert(
            id.to_owned(),
            TerminalTab {
                view,
                closing: false,
                _links: links,
                _agent: agent,
                _bar_context: bar_context,
            },
        );
    }

    fn restore_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.hinting.is_some() {
            self.end_hints(cx);
        }
        if let Some(dag) = &self.dag {
            dag.focus.focus(window);
        } else if let Some(picker) = &self.picker {
            picker.focus.focus(window);
        } else if let Some(id) = self
            .active_box()
            .and_then(|entry| entry.active_view.clone())
        {
            if let Some(index) = self
                .active_box()
                .and_then(|entry| entry.views.iter().position(|view| view.session == id))
            {
                self.tab_scroll.scroll_to_item(index);
            }
            self.add_terminal(&id, window, cx);
            if let Some(tab) = self.terminals.get(&id) {
                tab.view.read(cx).focus.focus(window);
            }
        } else {
            self.focus.focus(window);
        }
        self.sync_status_bar(cx);
        let _ = self.dag_refresh.try_send(());
        cx.notify();
    }

    fn load_main_checkout(&mut self, repository: PathBuf, cx: &mut Context<Self>) {
        if self.main_checkouts.contains_key(&repository) {
            return;
        }
        self.main_checkouts.insert(repository.clone(), None);
        let task = cx.background_executor().spawn(async move {
            let checkout = main_checkout(&repository);
            (repository, checkout)
        });
        cx.spawn(async move |app, cx| {
            let (repository, checkout) = task.await;
            let _ = app.update(cx, |app, cx| {
                match checkout {
                    Ok(checkout) => {
                        app.main_checkouts.insert(repository, checkout);
                    }
                    Err(error) => {
                        app.main_checkouts.remove(&repository);
                        eprintln!("Could not resolve main checkout. {error:#}");
                    }
                }
                app.sync_status_bar(cx);
            });
        })
        .detach();
    }

    fn sync_status_bar(&self, cx: &mut Context<Self>) {
        let active = self.active_box().and_then(|entry| {
            let session = entry.active_view.as_ref()?;
            let view = entry.views.iter().find(|view| &view.session == session)?;
            let terminal = self.terminals.get(session)?.view.read(cx);
            let repository_path = self
                .main_checkouts
                .get(&entry.repository)
                .and_then(Option::as_ref)
                .map(|path| path.display().to_string());
            Some(ActiveView {
                session: session.clone(),
                worktree_name: path_name(&view.worktree),
                worktree_path: view.worktree.display().to_string(),
                repository_path,
                agent_type: terminal.session.agent.name().into(),
                agent_task: terminal.metadata.task.clone(),
                agent_model: terminal.metadata.model.clone(),
                dag_progress: self
                    .dag_progress
                    .as_ref()
                    .and_then(|(id, progress)| (id == session).then(|| progress.clone())),
            })
        });
        let scheme = cx.global::<DesktopScheme>().0;
        self.status_bar
            .update(cx, |bar, cx| bar.set_view(active, scheme, cx));
    }

    fn activate_box(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.layout.boxes.iter().find(|entry| entry.id == id) else {
            return;
        };
        if entry.shortcut.is_none() {
            let Some(shortcut) = self.free_shortcut() else {
                self.show_error("Detach a box to free a shortcut.", cx);
                return;
            };
            self.box_mut(&id).unwrap().shortcut = Some(shortcut);
        }
        self.picker = None;
        self.dag = None;
        self.set_active_box(Some(id), cx);
        self.save();
        self.restore_focus(window, cx);
    }

    fn set_active_box(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.layout.active_box != id {
            self.tab_scroll.set_offset(point(px(0.), px(0.)));
            if let Some(index) = self.active_preview_index() {
                self.previews[index].preview.set_active(false, cx);
            }
            self.layout.active_box = id;
            if let Some(repository) = self.active_box().map(|entry| entry.repository.clone()) {
                self.load_main_checkout(repository, cx);
            }
            if let Some(index) = self.active_preview_index() {
                self.previews[index].preview.set_active(true, cx);
            }
        }
    }

    fn detach_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.layout.active_box.clone() else {
            return;
        };
        let entry = self
            .layout
            .boxes
            .iter_mut()
            .find(|entry| entry.id == id)
            .unwrap();
        entry.shortcut = None;
        for view in &entry.views {
            self.terminals.remove(&view.session);
        }
        let next = self.open_boxes().first().map(|entry| entry.id.clone());
        self.set_active_box(next, cx);
        self.save();
        if self.layout.active_box.is_some() {
            self.restore_focus(window, cx);
        } else {
            self.sync_status_bar(cx);
            self.pick_boxes(window, cx);
        }
    }

    fn activate_tab(&mut self, session: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.layout.active_box.clone() else {
            return;
        };
        let entry = self.box_mut(&id).unwrap();
        if entry.views.iter().any(|view| view.session == session) {
            entry.select_view(session);
            self.picker = None;
            self.dag = None;
            self.save();
            self.restore_focus(window, cx);
        }
    }

    fn navigate_tabs(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.active_box().filter(|entry| !entry.views.is_empty()) else {
            return;
        };
        let active = entry
            .views
            .iter()
            .position(|view| Some(&view.session) == entry.active_view.as_ref())
            .unwrap_or(0);
        let view = match command {
            "n" => entry.views.get((active + 1) % entry.views.len()),
            "p" => entry
                .views
                .get(active.checked_sub(1).unwrap_or(entry.views.len() - 1)),
            "l" => entry
                .previous_view
                .as_ref()
                .and_then(|id| entry.views.iter().find(|view| &view.session == id)),
            digit => shortcut_digit(digit)
                .filter(|digit| *digit != 0)
                .and_then(|digit| entry.views.get(usize::from(digit) - 1)),
        };
        if let Some(view) = view {
            self.activate_tab(view.session.clone(), window, cx);
        }
    }

    fn open_picker(
        &mut self,
        kind: Kind,
        rows: Vec<Row>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.next_id();
        let focus = cx.focus_handle();
        focus.focus(window);
        self.prefix = None;
        self.dag = None;
        self.picker = Some(Picker::new(id, kind, focus, rows));
        cx.notify();
        id
    }

    /// Shows the highlighted scheme while the color scheme picker is open.
    fn preview_selected_scheme(&mut self, cx: &mut Context<Self>) {
        if let Some(picker) = &self.picker
            && matches!(picker.kind, Kind::ColorSchemes { .. })
            && let Some(Choice::ColorScheme(scheme)) = picker.selection()
        {
            self.apply_scheme(scheme, cx);
        }
    }

    fn dismiss_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| matches!(picker.kind, Kind::ColorSchemes { .. }))
        {
            // Undo the live preview.
            self.apply_preferred_scheme(cx);
        }
        if matches!(
            self.picker.as_ref().map(|picker| &picker.kind),
            Some(Kind::ColorSchemes { first_run: true })
        ) {
            self.finish_theme_onboarding(window, cx);
            return;
        }
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| matches!(picker.kind, Kind::Setup { .. }))
        {
            self.finish_onboarding(false, window, cx);
            return;
        }
        self.picker = None;
        self.restore_focus(window, cx);
    }

    fn show_error(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.picker {
            picker.message = Some(error.into());
        } else if let Some(dag) = &mut self.dag {
            dag.message = Some(error.into());
        } else {
            self.error = Some(error.into());
        }
        cx.notify();
    }

    fn pick_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        for entry in &self.layout.boxes {
            if rows.iter().any(|row: &Row| matches!(&row.choice, Choice::Directory(path) if path == &entry.checkout)) {
                continue;
            }
            rows.push(Row {
                label: path_name(&entry.checkout),
                detail: entry.checkout.display().to_string(),
                choice: Choice::Directory(entry.checkout.clone()),
            });
        }
        let active = self
            .active_box()
            .and_then(|entry| {
                entry
                    .views
                    .iter()
                    .find(|view| Some(&view.session) == entry.active_view.as_ref())
            })
            .map(|view| (view.session.clone(), view.worktree.clone()));
        let id = self.open_picker(Kind::Repository { directory: None }, rows, window, cx);
        if self.free_shortcut().is_none() {
            self.show_error("Detach a box to free a shortcut.", cx);
        }
        let Some((session, worktree)) = active else {
            return;
        };
        let terminal_directory = self
            .terminals
            .get(&session)
            .and_then(|tab| tab.view.read(cx).reported_directory());
        let picker = self.picker.as_mut().unwrap();
        picker.suggest_directory(terminal_directory.unwrap_or(worktree));
        picker.loading = true;
        self.start_directory_walk(cx);
        let query = cx
            .background_executor()
            .spawn(async move { client::working_directory(session) });
        cx.spawn_in(window, async move |app, cx| {
            let result = query.await;
            let _ = app.update_in(cx, |this, window, cx| {
                let Some(picker) = this.picker.as_mut().filter(|picker| picker.id == id) else {
                    return;
                };
                picker.loading = false;
                if let Ok(Some(directory)) = result {
                    picker.suggest_directory(directory);
                    this.start_directory_walk(cx);
                }
                let Some(picker) = this.picker.as_mut() else {
                    return;
                };
                if std::mem::take(&mut picker.accept_pending)
                    && let Some(choice) = picker.selection()
                {
                    this.choose(choice, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Streams a recursive directory listing into the new box picker when its query asks for one.
    fn start_directory_walk(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let Some(root) = picker.take_walk_request() else {
            return;
        };
        let id = picker.id;
        let (sender, receiver) = async_channel::bounded(2);
        let walk_root = root.clone();
        std::thread::spawn(move || crate::picker::walk(&walk_root, &sender));
        cx.spawn(async move |app, cx| {
            loop {
                let batch = receiver.recv().await;
                let done = batch.is_err();
                let keep = app
                    .update(cx, |this, cx| {
                        let Some(picker) = this.picker.as_mut().filter(|picker| picker.id == id)
                        else {
                            return false;
                        };
                        let keep = picker.add_entries(&root, batch.unwrap_or_default(), done);
                        cx.notify();
                        keep
                    })
                    .unwrap_or(false);
                if done || !keep {
                    break;
                }
            }
        })
        .detach();
    }

    fn pick_boxes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let boxes = self.open_boxes().into_iter().chain(
            self.layout
                .boxes
                .iter()
                .filter(|entry| entry.shortcut.is_none()),
        );
        let mut rows: Vec<_> = boxes
            .map(|entry| Row {
                label: match entry.shortcut {
                    Some(digit) => format!("{digit} · {}", path_name(&entry.checkout)),
                    None => format!("{} · Detached", path_name(&entry.checkout)),
                },
                detail: entry.checkout.display().to_string(),
                choice: Choice::Box(entry.id.clone()),
            })
            .collect();
        rows.push(Row {
            label: "New box…".into(),
            detail: String::new(),
            choice: Choice::NewBox,
        });
        self.open_picker(Kind::Boxes, rows, window, cx);
    }

    fn pick_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.active_box() else {
            return;
        };
        let rows: Vec<_> = entry
            .views
            .iter()
            .enumerate()
            .map(|(index, view)| Row {
                label: self.tab_label(index, view),
                detail: view.worktree.display().to_string(),
                choice: Choice::Tab(view.session.clone()),
            })
            .collect();
        let empty = rows.is_empty();
        self.open_picker(Kind::Tabs, rows, window, cx);
        if empty {
            self.show_error("This box has no tabs.", cx);
        }
    }

    fn pick_runs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(box_id) = self.layout.active_box.clone() else {
            return;
        };
        self.open_picker(
            Kind::Runs {
                box_id,
                runs: Vec::new(),
            },
            Vec::new(),
            window,
            cx,
        );
        if let Some(picker) = &mut self.picker {
            picker.loading = true;
            picker.message = Some("Looking up runs…".into());
        }
        let _ = self.dag_refresh.try_send(());
    }

    fn open_dag(&mut self, run: Run, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        focus.focus(window);
        self.picker = None;
        self.dag = Some(DagView::new(self.next_id(), run, focus));
        let _ = self.dag_refresh.try_send(());
        cx.notify();
    }

    fn open_dag_link(&mut self, run: String, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.open_picker(Kind::RunLink, Vec::new(), window, cx);
        if let Some(picker) = &mut self.picker {
            picker.waiting = true;
            picker.message = Some("Looking up run…".into());
        }
        let lookup = cx
            .background_executor()
            .spawn(async move { client::get_run(run) });
        cx.spawn_in(window, async move |app, cx| {
            let result = lookup.await;
            let _ = app.update_in(cx, |this, window, cx| {
                let Some(picker) = this.picker.as_mut().filter(|picker| picker.id == id) else {
                    return;
                };
                picker.waiting = false;
                let run = match result {
                    Ok(run) => run,
                    Err(error) => {
                        this.show_error(format!("Could not open DAG run. {error:#}"), cx);
                        return;
                    }
                };
                if this.layout.active_box.as_ref() != Some(&run.box_id) {
                    if !this
                        .layout
                        .boxes
                        .iter()
                        .any(|entry| entry.id == run.box_id && entry.shortcut.is_some())
                    {
                        this.show_error("Open this run’s box before following its link.", cx);
                        return;
                    }
                    this.activate_box(run.box_id.clone(), window, cx);
                }
                this.open_dag(run, window, cx);
            });
        })
        .detach();
    }

    fn pick_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(box_id) = self.layout.active_box.clone() else {
            return;
        };
        self.open_picker(
            Kind::Agents {
                box_id,
                tree: AgentTree::default(),
            },
            Vec::new(),
            window,
            cx,
        );
        if let Some(picker) = &mut self.picker {
            picker.loading = true;
            picker.message = Some("Looking up agents…".into());
        }
        let _ = self.dag_refresh.try_send(());
    }

    /// Ends the agent tree's selected worker through the same path as ending its tab.
    fn end_selected_worker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.keymap.first(Scope::AgentTree, Command::EndWorker);
        let Some(picker) = &mut self.picker else {
            return;
        };
        let Some(Choice::Agent(id)) = picker.selection() else {
            return;
        };
        let Kind::Agents { tree, .. } = &picker.kind else {
            return;
        };
        let Some(agent) = tree.agent(&id) else {
            return;
        };
        let step = end_worker_step(agent, picker.armed_end.as_deref(), &key);
        picker.armed_end = None;
        let session = match step {
            EndWorker::NoSession => {
                self.show_error("This worker has no terminal session.", cx);
                return;
            }
            EndWorker::Confirm(message) => {
                picker.armed_end = Some(id);
                picker.message = Some(message);
                cx.notify();
                return;
            }
            EndWorker::End(session) => session,
        };
        picker.message = None;
        self.end_session(session, window, cx);
    }

    fn open_worker(
        &mut self,
        modal_id: u64,
        box_id: String,
        session: Option<String>,
        worktree: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = session else {
            self.show_error("This worker has no terminal session.", cx);
            return;
        };
        let Some(entry) = self.active_box().filter(|entry| entry.id == box_id) else {
            return;
        };
        if entry.views.iter().any(|view| view.session == session) {
            self.activate_tab(session, window, cx);
            return;
        }

        if let Some(dag) = &mut self.dag {
            dag.opening = true;
            dag.message = None;
        } else if let Some(picker) = &mut self.picker {
            picker.waiting = true;
            picker.message = Some("Opening worker terminal…".into());
        }
        cx.notify();
        let lookup = cx.background_executor().spawn(async move {
            client::list_sessions()?
                .into_iter()
                .find(|listed| listed.id == session)
                .with_context(|| format!("Session {session} is no longer available."))
        });
        cx.spawn_in(window, async move |app, cx| {
            let result = lookup.await;
            let _ = app.update_in(cx, |this, window, cx| {
                if this.layout.active_box.as_ref() != Some(&box_id) {
                    return;
                }
                if let Some(dag) = this.dag.as_mut().filter(|dag| dag.id == modal_id) {
                    dag.opening = false;
                } else if let Some(picker) =
                    this.picker.as_mut().filter(|picker| picker.id == modal_id)
                {
                    picker.waiting = false;
                } else {
                    return;
                }
                match result {
                    Ok(session) => {
                        this.picker = None;
                        this.dag = None;
                        this.add_view(&box_id, session, worktree, true);
                        this.save();
                        this.restore_focus(window, cx);
                    }
                    Err(error) => {
                        this.show_error(format!("Could not open worker terminal. {error:#}"), cx)
                    }
                }
            });
        })
        .detach();
    }

    fn pick_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.active_box() else {
            return;
        };
        let repository = entry.repository.clone();
        let rows = entry
            .active_view
            .iter()
            .map(|session| Row {
                label: "New worktree".into(),
                detail: String::new(),
                choice: Choice::NewWorktree(session.clone()),
            })
            .collect();
        let id = self.open_picker(Kind::Worktrees(entry.id.clone()), rows, window, cx);
        self.load_picker_rows(
            id,
            "Looking up worktrees…",
            "No available worktrees.",
            window,
            cx,
            move || worktree_rows(&repository),
        );
    }

    fn create_worktree(
        &mut self,
        box_id: String,
        from_session: String,
        branch: String,
        picker_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picker = self.picker.as_mut().unwrap();
        picker.waiting = true;
        picker.message = Some("Creating worktree…".into());
        cx.notify();
        let source_box = box_id.clone();
        let task = cx
            .background_executor()
            .spawn(async move { client::create_view_worktree(source_box, from_session, branch) });
        cx.spawn_in(window, async move |app, cx| {
            let result = task.await;
            let _ = app.update_in(cx, |this, window, cx| {
                let Some(picker) = this.picker.as_mut().filter(|picker| picker.id == picker_id)
                else {
                    return;
                };
                picker.waiting = false;
                match result {
                    Ok(path) => {
                        picker.worktree_created(path.clone());
                        this.launch(path, Some(box_id), Some(picker_id), window, cx);
                    }
                    Err(error) => this.show_error(format!("{error:#}"), cx),
                }
            });
        })
        .detach();
    }

    fn open_status_actions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(actions) = self.status_bar.read(cx).actions() else {
            return;
        };
        let rows = actions
            .choices
            .iter()
            .map(|choice| Row {
                label: choice.title.clone(),
                detail: choice.detail.clone(),
                choice: Choice::CopyText(choice.copy_text.clone()),
            })
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            self.open_picker(Kind::StatusActions(actions.title.clone()), rows, window, cx);
        }
    }

    fn pick_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.active_box() else {
            return;
        };
        let repository = entry.repository.clone();
        let owned: std::collections::HashSet<_> = self
            .layout
            .boxes
            .iter()
            .flat_map(|entry| entry.views.iter().map(|view| view.session.clone()))
            .collect();
        let id = self.open_picker(Kind::Sessions(entry.id.clone()), Vec::new(), window, cx);
        self.load_picker_rows(
            id,
            "Looking up sessions…",
            "No unassigned sessions in this repository.",
            window,
            cx,
            move || {
                Ok(client::list_sessions()?
                    .into_iter()
                    .filter(|session| !owned.contains(&session.id))
                    .filter_map(|session| {
                        let checkout =
                            Checkout::in_repository(&session.worktree, &repository).ok()?;
                        Some(Row {
                            label: format!(
                                "{} · {}",
                                session.agent.name(),
                                path_name(&checkout.worktree)
                            ),
                            detail: format!("{} · {}", checkout.worktree.display(), session.id),
                            choice: Choice::Session(session, checkout.worktree),
                        })
                    })
                    .collect())
            },
        );
    }

    fn load_picker_rows(
        &mut self,
        id: u64,
        loading: &'static str,
        empty: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        load: impl FnOnce() -> Result<Vec<Row>> + Send + 'static,
    ) {
        let picker = self.picker.as_mut().unwrap();
        picker.message = Some(loading.into());
        picker.loading = true;
        let task = cx.background_executor().spawn(async move { load() });
        cx.spawn_in(window, async move |app, cx| {
            let result = task.await;
            let _ = app.update_in(cx, |this, window, cx| {
                let Some(picker) = this.picker.as_mut().filter(|picker| picker.id == id) else {
                    return;
                };
                picker.loading = false;
                match result {
                    Ok(rows) => {
                        picker.rows.extend(rows);
                        picker.message = picker.rows.is_empty().then(|| empty.to_owned());
                    }
                    Err(error) => picker.message = Some(format!("{error:#}")),
                }
                if std::mem::take(&mut picker.accept_pending)
                    && let Some(choice) = picker.selection()
                {
                    this.choose(choice, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.as_ref().filter(|picker| !picker.waiting) else {
            return;
        };
        match choice {
            Choice::NewBox => self.pick_repository(window, cx),
            Choice::Link(link) => {
                self.picker = None;
                self.restore_focus(window, cx);
                if let Some(session) = self.active_view() {
                    self.resolve(session, link, true, window, cx);
                }
            }
            Choice::Setup(install) => self.finish_onboarding(install, window, cx),
            Choice::ColorScheme(scheme) => {
                let first_run = matches!(picker.kind, Kind::ColorSchemes { first_run: true });
                self.layout.color_scheme = Some(scheme.name.clone());
                self.apply_scheme(scheme, cx);
                self.save();
                if first_run {
                    self.finish_theme_onboarding(window, cx);
                } else {
                    self.dismiss_picker(window, cx);
                }
            }
            Choice::Directory(path) => {
                if self.free_shortcut().is_none() {
                    self.show_error("Detach a box to free a shortcut.", cx);
                    return;
                }
                self.launch(path, None, Some(picker.id), window, cx);
            }
            Choice::Box(id) => self.activate_box(id, window, cx),
            Choice::Tab(id) => self.activate_tab(id, window, cx),
            Choice::Run(id) => {
                if let Some(Picker {
                    kind: Kind::Runs { runs, .. },
                    ..
                }) = self.picker.as_mut()
                    && let Some(index) = runs.iter().position(|run| run.id == id)
                {
                    let run = runs.swap_remove(index);
                    self.open_dag(run, window, cx);
                }
            }
            Choice::Agent(id) => {
                if let Kind::Agents { box_id, tree } = &picker.kind
                    && let Some(agent) = tree.agent(&id)
                {
                    self.open_worker(
                        picker.id,
                        box_id.clone(),
                        agent.session.clone(),
                        agent.worktree.clone(),
                        window,
                        cx,
                    );
                }
            }
            Choice::CopyText(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.dismiss_picker(window, cx);
            }
            Choice::NewWorktree(from_session) => {
                if let Kind::Worktrees(box_id) = &picker.kind {
                    self.open_picker(
                        Kind::NewWorktree {
                            box_id: box_id.clone(),
                            from_session,
                            created: false,
                        },
                        Vec::new(),
                        window,
                        cx,
                    );
                    if let Some(picker) = self.picker.as_mut() {
                        picker.suggest_branch(domain::worktree::generated_worktree_name(
                            random_seed(),
                        ));
                    }
                }
            }
            Choice::CreateWorktree(branch) => {
                if let Kind::NewWorktree {
                    box_id,
                    from_session,
                    ..
                } = &picker.kind
                {
                    self.create_worktree(
                        box_id.clone(),
                        from_session.clone(),
                        branch,
                        picker.id,
                        window,
                        cx,
                    );
                }
            }
            Choice::Worktree(path) => {
                if let Kind::Worktrees(box_id) | Kind::NewWorktree { box_id, .. } = &picker.kind {
                    self.launch(path, Some(box_id.clone()), Some(picker.id), window, cx);
                }
            }
            Choice::Session(session, worktree) => {
                if let Kind::Sessions(box_id) = &picker.kind {
                    let box_id = box_id.clone();
                    self.picker = None;
                    self.add_view(&box_id, session, worktree, true);
                    self.save();
                    self.restore_focus(window, cx);
                }
            }
        }
    }

    fn launch_in_active_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.active_box() else {
            return;
        };
        let path = entry
            .views
            .iter()
            .find(|view| Some(&view.session) == entry.active_view.as_ref())
            .map_or_else(|| entry.checkout.clone(), |view| view.worktree.clone());
        self.launch(path, Some(entry.id.clone()), None, window, cx);
    }

    fn launch(
        &mut self,
        path: PathBuf,
        box_id: Option<String>,
        picker_id: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.launching {
            self.show_error("A shell is already starting.", cx);
            return;
        }
        self.launching = true;
        self.error = None;
        if let Some(picker) = &mut self.picker {
            picker.waiting = true;
            picker.message = Some("Opening shell…".into());
        }
        let repository = box_id
            .as_ref()
            .and_then(|id| self.box_mut(id))
            .map(|entry| entry.repository.clone());
        let task = cx.background_executor().spawn(async move {
            let checkout = match repository {
                Some(repository) => {
                    let checkout = Checkout::in_repository(&path, &repository)?;
                    ensure!(
                        checkout.worktree == path.canonicalize()?,
                        "Choose an existing worktree root"
                    );
                    checkout
                }
                None => Checkout::discover(&path)?,
            };
            let session = client::start_session(StartSession {
                agent: Agent::Shell,
                worktree: checkout.worktree.clone(),
            })
            .context("Open shell")?;
            Ok::<_, anyhow::Error>((checkout, session))
        });
        cx.spawn_in(window, async move |app, cx| {
            let result = task.await;
            let _ = app.update_in(cx, |this, window, cx| {
                this.launching = false;
                let same_picker = picker_id.is_some()
                    && this.picker.as_ref().map(|picker| picker.id) == picker_id;
                if same_picker {
                    this.picker.as_mut().unwrap().waiting = false;
                } else if picker_id.is_some() {
                    if let Ok((_, session)) = result {
                        cx.background_executor()
                            .spawn(async move {
                                let _ = client::end_session(session.id);
                            })
                            .detach();
                    }
                    return;
                }

                match result {
                    Ok((checkout, session)) => {
                        if same_picker {
                            this.picker = None;
                        }
                        let id = match box_id {
                            Some(id) => id,
                            None => {
                                // Launches are serialized, and no picker action can consume this digit
                                // while the new box picker is still open.
                                let shortcut = this.free_shortcut().expect("reserved box shortcut");
                                let entry = SavedBox::new(
                                    checkout.repository,
                                    checkout.worktree.clone(),
                                    shortcut,
                                );
                                let id = entry.id.clone();
                                this.layout.boxes.push(entry);
                                this.set_active_box(Some(id.clone()), cx);
                                id
                            }
                        };
                        let focus = this.layout.active_box.as_ref() == Some(&id)
                            && this.picker.is_none()
                            && this.dag.is_none();
                        this.add_view(&id, session, checkout.worktree, focus);
                        this.save();
                        if focus {
                            this.restore_focus(window, cx);
                        }
                    }
                    Err(error) => this.show_error(format!("{error:#}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn add_view(&mut self, box_id: &str, session: Session, worktree: PathBuf, select: bool) {
        let id = session.id.clone();
        self.sessions.insert(id.clone(), session);
        let entry = self.box_mut(box_id).unwrap();
        entry.views.push(SavedView {
            session: id.clone(),
            worktree,
        });
        if select || entry.active_view.is_none() {
            entry.select_view(id);
        }
    }

    fn end_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self
            .active_box()
            .and_then(|entry| entry.active_view.clone())
        {
            let message = self.end_session_message();
            self.open_picker(
                Kind::End {
                    session: id,
                    message,
                },
                Vec::new(),
                window,
                cx,
            );
        }
    }

    /// Confirmation text for ending a session, naming the keys the user has bound.
    fn end_session_message(&self) -> String {
        let key = |scope, command| self.keymap.first(scope, command);
        let mut message = format!(
            "{} ends the session and removes its tab. {} cancels.",
            key(Scope::Picker, Command::PickerSelect),
            key(Scope::Picker, Command::PickerCancel),
        );
        let bound = |scope, command| !self.keymap.keys(scope, command).is_empty();
        if bound(Scope::Prefix, Command::TabPrefix) && bound(Scope::Tab, Command::TabDetach) {
            message.push_str(&format!(
                " To close the tab and keep the session running, use {} then {}.",
                key(Scope::Prefix, Command::TabPrefix),
                key(Scope::Tab, Command::TabDetach),
            ));
        }
        message
    }

    fn detach_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(box_id) = self.layout.active_box.clone() else {
            return;
        };
        let Some(entry) = self.box_mut(&box_id) else {
            return;
        };
        if let Some(index) = entry
            .views
            .iter()
            .position(|view| Some(&view.session) == entry.active_view.as_ref())
        {
            let session = entry.views[index].session.clone();
            entry.remove_view(index);
            self.terminals.remove(&session);
            self.save();
            self.restore_focus(window, cx);
        }
    }

    /// Ends a session, with or without an open tab, and removes any saved view of it.
    fn end_session(&mut self, session: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.terminals.get_mut(&session) {
            if tab.closing {
                return;
            }
            tab.closing = true;
        }
        let target = session.clone();
        let end = cx.background_executor().spawn(async move {
            let result = client::end_session(target.clone());
            if result.is_err()
                && client::list_sessions()
                    .is_ok_and(|sessions| sessions.iter().all(|listed| listed.id != target))
            {
                return Ok(());
            }
            result
        });
        cx.spawn_in(window, async move |app, cx| {
            let result = end.await;
            let _ = app.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    this.terminals.remove(&session);
                    this.sessions.remove(&session);
                    for entry in &mut this.layout.boxes {
                        if let Some(index) =
                            entry.views.iter().position(|view| view.session == session)
                        {
                            entry.remove_view(index);
                        }
                    }
                    this.error = None;
                    this.save();
                    let _ = this.dag_refresh.try_send(());
                    this.restore_focus(window, cx);
                }
                Err(error) => {
                    if let Some(tab) = this.terminals.get_mut(&session) {
                        tab.closing = false;
                    }
                    this.show_error(format!("Could not end session. {error:#}"), cx);
                }
            });
        })
        .detach();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        let key = stroke.key.as_str();
        if let Some(dag) = &mut self.dag {
            cx.stop_propagation();
            if dag.picking_artifact {
                if event.is_held {
                    return;
                }
                dag.picking_artifact = false;
                let mods = stroke.modifiers;
                let digit = (!mods.control && !mods.alt && !mods.platform)
                    .then(|| shortcut_digit(key))
                    .flatten();
                if let Some(digit) = digit {
                    match dag.artifact_link(usize::from(digit)) {
                        Some(link) => self.open_dag_artifact(link, window, cx),
                        None => dag.message = Some(format!("No artifact {digit}.")),
                    }
                }
                cx.notify();
                return;
            }
            match self.keymap.command(Scope::Dag, stroke) {
                Some(Command::DagClose) => {
                    self.dag = None;
                    self.restore_focus(window, cx);
                }
                Some(Command::DagOpen) if !event.is_held && !dag.opening => {
                    if let Some(attempt) = dag.attempt() {
                        let (id, box_id, session, worktree) = (
                            dag.id,
                            dag.run.box_id.clone(),
                            attempt.session.clone(),
                            attempt.worktree.clone(),
                        );
                        self.open_worker(id, box_id, session, worktree, window, cx);
                    } else {
                        self.show_error("This task has no assigned worker.", cx);
                    }
                }
                Some(Command::DagArtifact) if !event.is_held && !dag.opening => {
                    let artifacts = dag.artifacts();
                    match artifacts.as_slice() {
                        [] => dag.message = Some("This task has no artifacts yet.".into()),
                        [artifact] => {
                            let link = artifact.link();
                            self.open_dag_artifact(link, window, cx);
                        }
                        _ => {
                            dag.picking_artifact = true;
                            dag.message = None;
                        }
                    }
                    cx.notify();
                }
                Some(
                    command @ (Command::DagUp
                    | Command::DagDown
                    | Command::DagLeft
                    | Command::DagRight),
                ) if !dag.opening => {
                    dag.step(command);
                    cx.notify();
                }
                Some(command @ (Command::DagZoomIn | Command::DagZoomOut)) => {
                    let steps = if command == Command::DagZoomIn {
                        1.
                    } else {
                        -1.
                    };
                    if dag.zoom(steps) {
                        cx.notify();
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some(picker) = &mut self.picker {
            cx.stop_propagation();
            let scope = if matches!(picker.kind, Kind::Agents { .. }) {
                Scope::AgentTree
            } else if matches!(picker.kind, Kind::Runs { .. }) {
                Scope::Runs
            } else if picker.has_text_field() {
                Scope::Picker
            } else {
                Scope::Dialog
            };
            let command = self.keymap.command(scope, stroke);
            if command != Some(Command::EndWorker) && picker.armed_end.take().is_some() {
                picker.message = None;
                cx.notify();
            }
            match command {
                Some(Command::PickerCancel) => self.dismiss_picker(window, cx),
                Some(Command::PickerSelect) => {
                    if event.is_held || picker.waiting {
                        return;
                    }
                    let choice = picker.selection();
                    if picker.loading && !matches!(choice, Some(Choice::NewWorktree(_))) {
                        picker.accept_pending = true;
                    } else if let Kind::End { session, .. } = &picker.kind {
                        let id = session.clone();
                        self.dismiss_picker(window, cx);
                        self.end_session(id, window, cx);
                    } else if let Some(choice) = choice {
                        self.choose(choice, window, cx);
                    }
                }
                Some(Command::PickerUp | Command::DialogUp) => {
                    picker.step(true);
                    self.preview_selected_scheme(cx);
                    cx.notify();
                }
                Some(Command::PickerDown | Command::DialogDown) => {
                    picker.step(false);
                    self.preview_selected_scheme(cx);
                    cx.notify();
                }
                Some(Command::ToggleFinished) => {
                    if !picker.waiting {
                        picker.show_finished = !picker.show_finished;
                        let _ = self.dag_refresh.try_send(());
                    }
                }
                Some(Command::EndWorker) => {
                    if !event.is_held && !picker.waiting {
                        self.end_selected_worker(window, cx);
                    }
                }
                Some(command @ (Command::AgentCollapse | Command::AgentExpand)) => {
                    if !picker.waiting {
                        picker.agent_branch(command == Command::AgentExpand);
                        cx.notify();
                    }
                }
                command => {
                    if command != Some(Command::PickerComplete) || !picker.complete_selection() {
                        picker.edit(stroke, cx);
                    }
                    self.start_directory_walk(cx);
                    self.preview_selected_scheme(cx);
                    cx.notify();
                }
            }
            return;
        }

        if self.hinting.is_some() {
            cx.stop_propagation();
            self.hint_key(stroke, window, cx);
            return;
        }

        let incoming = match self.keymap.command(Scope::Prefix, stroke) {
            Some(Command::BoxPrefix) => Some(Prefix::Box),
            Some(Command::TabPrefix) => Some(Prefix::View),
            _ => None,
        };
        if let Some(incoming) = incoming {
            if !event.is_held && self.prefix == Some(incoming) {
                self.prefix = None;
                cx.notify();
                return;
            }
            if !event.is_held {
                self.prefix = Some(incoming);
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }

        if self.preview_focus.is_focused(window) && self.prefix.is_none() {
            cx.stop_propagation();
            if let Some(command) = self.keymap.command(Scope::Preview, stroke) {
                self.preview_command(command, window, cx);
            }
            return;
        }

        let Some(prefix) = self.prefix.take() else {
            return;
        };
        cx.stop_propagation();
        cx.notify();
        if event.is_held {
            return;
        }
        let scope = match prefix {
            Prefix::Box => Scope::Box,
            Prefix::View => Scope::Tab,
        };
        let command = self.keymap.command(scope, stroke);
        match command {
            Some(Command::Help) => return self.open_help(window, cx),
            Some(Command::Quit) => return window.remove_window(),
            _ => {}
        }
        if self.registry.is_none() {
            return;
        }
        let mods = stroke.modifiers;
        let digit = (!mods.control && !mods.alt && !mods.platform)
            .then(|| shortcut_digit(key))
            .flatten();
        match command {
            Some(Command::BoxNew) => self.pick_repository(window, cx),
            Some(Command::BoxPick) => self.pick_boxes(window, cx),
            Some(Command::BoxRuns) => self.pick_runs(window, cx),
            Some(Command::BoxAgents) => self.pick_agents(window, cx),
            Some(Command::Reload) => {
                self.status_bar.update(cx, StatusBar::reload);
            }
            Some(Command::BoxDetach) => self.detach_box(window, cx),
            Some(Command::BoxColorScheme) => self.pick_color_scheme(false, window, cx),
            Some(Command::BoxSetup) => self.open_setup(false, window, cx),
            Some(Command::TabNew) => self.launch_in_active_worktree(window, cx),
            Some(Command::TabWorktree) => self.pick_worktrees(window, cx),
            Some(Command::TabPick) => self.pick_tabs(window, cx),
            Some(Command::TabRecover) => self.pick_sessions(window, cx),
            Some(Command::TabEnd) => self.end_active_tab(window, cx),
            Some(Command::TabDetach) => self.detach_active_tab(window, cx),
            Some(Command::TabPaths) => self.open_status_actions(window, cx),
            Some(Command::TabNext) => self.navigate_tabs("n", window, cx),
            Some(Command::TabPrevious) => self.navigate_tabs("p", window, cx),
            Some(Command::TabLast) => self.navigate_tabs("l", window, cx),
            Some(Command::TabOpenLink) => self.start_hints(cx),
            Some(Command::TabPreview) => self.focus_preview(window, cx),
            Some(Command::TerminalZoomIn) => self.zoom_terminals(Some(1.), cx),
            Some(Command::TerminalZoomOut) => self.zoom_terminals(Some(-1.), cx),
            Some(Command::TerminalZoomReset) => self.zoom_terminals(None, cx),
            Some(_) => {}
            None => match (prefix, digit) {
                (Prefix::Box, Some(digit)) => {
                    if let Some(entry) = self
                        .layout
                        .boxes
                        .iter()
                        .find(|entry| entry.shortcut == Some(digit))
                    {
                        self.activate_box(entry.id.clone(), window, cx);
                    }
                }
                (Prefix::View, Some(_)) => self.navigate_tabs(key, window, cx),
                _ => {}
            },
        }
    }

    /// Closes the task graph and opens an artifact link in the box's preview pane, focused,
    /// as choosing a link from the link search does.
    fn open_dag_artifact(&mut self, link: String, window: &mut Window, cx: &mut Context<Self>) {
        let worker = self
            .dag
            .as_ref()
            .and_then(|dag| dag.attempt())
            .and_then(|attempt| attempt.session.clone());
        self.dag = None;
        self.restore_focus(window, cx);
        // Resolution runs as a live session; the worker's may have ended.
        match self.active_view().or(worker) {
            Some(session) => self.resolve(session, link, true, window, cx),
            None => self.show_error("Open a terminal in this box to preview artifacts.", cx),
        }
    }

    fn tab_label(&self, index: usize, view: &SavedView) -> String {
        let agent = self
            .sessions
            .get(&view.session)
            .map_or("shell", |session| session.agent.as_str());
        format!("{} {}", index + 1, agent)
    }

    fn resolve(
        &mut self,
        session: String,
        target: String,
        focus_preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match link_action(&target) {
            LinkAction::OpenDag(run) => self.open_dag_link(run.to_owned(), window, cx),
            LinkAction::ResolveArtifact => {
                if let Some(box_id) = self.layout.active_box.clone() {
                    self.resolve_in(box_id, session, target, focus_preview, window, cx);
                }
            }
        }
    }

    /// Opens `target` in `box_id`'s preview pane. Keyboard opens can take focus.
    fn resolve_in(
        &mut self,
        box_id: String,
        session: String,
        target: String,
        focus_preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = session.clone();
        let opening_focus = focus_preview.then(|| window.focused(cx)).flatten();
        let query = ResolveArtifact { session, target };
        let resolved = cx
            .background_executor()
            .spawn(async move { client::resolve_artifact(query) });
        cx.spawn_in(window, async move |app, cx| {
            let result = resolved.await;
            let _ = app.update_in(cx, |this, window, cx| {
                let (target, title, error) = match result {
                    Ok(ResolvedArtifact { target, title }) => (Some(target), title, None),
                    Err(error) => (
                        None,
                        "Preview unavailable".into(),
                        Some(format!("{error:#}")),
                    ),
                };
                // A link may resolve after the user has moved on to another view or shortcut.
                let take_focus = this.layout.active_box.as_ref() == Some(&box_id)
                    && this.prefix.is_none()
                    && this.hinting.is_none()
                    && opening_focus.is_some_and(|focus| focus.is_focused(window));
                let existing = this
                    .previews
                    .iter()
                    .position(|tab| tab.box_id == box_id && tab.target == target);
                let index = if let (Some(target), Some(index)) = (&target, existing) {
                    let tab = &mut this.previews[index];
                    tab.originating_terminal = source;
                    tab.title = title;
                    if !matches!(target, Target::Loopback(_))
                        || matches!(tab.preview, Preview::Error(_))
                    {
                        tab.preview.close(window, cx);
                        tab.preview = Preview::open(target, window, cx);
                    }
                    index
                } else {
                    let preview = match &target {
                        Some(target) => Preview::open(target, window, cx),
                        None => Preview::Error(error.unwrap_or_default()),
                    };
                    let id = this.next_id();
                    this.previews.push(PreviewTab {
                        id,
                        box_id,
                        title,
                        target,
                        preview,
                        originating_terminal: source,
                    });
                    this.previews.len() - 1
                };
                this.activate_preview(index, cx);
                if take_focus {
                    this.focus_preview(window, cx);
                }
            });
        })
        .detach();
    }

    fn active_preview_index(&self) -> Option<usize> {
        let id = self.active_previews.get(self.layout.active_box.as_ref()?)?;
        self.previews.iter().position(|tab| tab.id == *id)
    }

    fn activate_preview(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = &self.previews[index];
        let box_id = tab.box_id.clone();
        if self.layout.active_box.as_ref() == Some(&box_id) {
            if let Some(previous) = self
                .active_preview_index()
                .filter(|previous| *previous != index)
            {
                self.previews[previous].preview.set_active(false, cx);
            }
            self.previews[index].preview.set_active(true, cx);
        } else {
            self.previews[index].preview.set_active(false, cx);
        }
        self.active_previews.insert(box_id, self.previews[index].id);
        cx.notify();
    }

    fn close_preview(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let closing_active = self.active_preview_index() == Some(index);
        let tab = self.previews.remove(index);
        tab.preview.close(window, cx);
        if self.active_previews.get(&tab.box_id) == Some(&tab.id) {
            self.active_previews.remove(&tab.box_id);
            if let Some(next) = self
                .previews
                .iter()
                .position(|preview| preview.box_id == tab.box_id)
            {
                self.activate_preview(next, cx);
            }
        }
        if self.active_box().is_some_and(|entry| {
            entry
                .views
                .iter()
                .any(|view| view.session == tab.originating_terminal)
        }) {
            self.activate_tab(tab.originating_terminal, window, cx);
        } else if closing_active {
            self.restore_focus(window, cx);
        }
        cx.notify();
    }

    fn previews(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let colors = colors(cx);
        let active = self.active_preview_index()?;
        let focused = self.preview_focus.is_focused(window);
        let tabs =
            self.previews
                .iter()
                .enumerate()
                .filter(|(_, tab)| self.layout.active_box.as_ref() == Some(&tab.box_id))
                .map(|(index, tab)| {
                    div()
                        .flex()
                        .min_w_0()
                        .items_center()
                        .when(index == active, |this| this.bg(rgb(colors.raised)))
                        .child(
                            button(colors, ("show-preview", tab.id), tab.title.clone()).on_click(
                                cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.activate_preview(index, cx)
                                }),
                            ),
                        )
                        .child(button(colors, ("close-preview", tab.id), "×").on_click(
                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.close_preview(index, window, cx)
                            }),
                        ))
                });
        Some(
            div()
                .track_focus(&self.preview_focus)
                .w(relative(0.48))
                .flex_none()
                .min_w(px(245.))
                .h_full()
                .min_h_0()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(if focused {
                    rgb(colors.accent)
                } else {
                    rgb(colors.raised)
                })
                .bg(rgb(colors.surface))
                .child(
                    div()
                        .id("preview-tabs")
                        .flex()
                        .flex_none()
                        .p_1()
                        .gap_1()
                        .overflow_x_scroll()
                        .children(tabs),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .child(self.previews[active].preview.render(colors)),
                )
                .when(focused, |pane| {
                    let key = |command| self.keymap.first(Scope::Preview, command);
                    pane.child(
                        div()
                            .flex_none()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(rgb(colors.muted))
                            .child(format!(
                                "{}/{} switch · {} close · {} play · {}/{} scroll · {} type · {} back",
                                key(Command::PreviewPrevious),
                                key(Command::PreviewNext),
                                key(Command::PreviewClose),
                                key(Command::PreviewPlay),
                                key(Command::PreviewScrollDown),
                                key(Command::PreviewScrollUp),
                                key(Command::PreviewInteract),
                                key(Command::PreviewExit),
                            )),
                    )
                })
                .into_any_element(),
        )
    }
}

fn managed_worktrees(repository: &Path) -> Result<HashMap<PathBuf, ManagedWorktree>> {
    Ok(client::list_managed_worktrees(WorktreeScope::All)?
        .into_iter()
        .filter(|worktree| worktree.repository == repository)
        .map(|worktree| (worktree.path.clone(), worktree))
        .collect())
}

fn worktree_rows(repository: &Path) -> Result<Vec<Row>> {
    let paths = repository_worktrees(repository)?;
    let managed = managed_worktrees(repository).unwrap_or_else(|error| {
        eprintln!("Could not list Power-created worktrees. {error:#}");
        HashMap::new()
    });
    let mut runs = HashMap::new();
    Ok(paths
        .into_iter()
        .filter(|path| path != repository && path.is_dir())
        .map(|path| {
            let detail = match managed.get(&path) {
                Some(worktree) => {
                    let creator = match &worktree.creator {
                        WorktreeCreator::View { .. } => "tab".into(),
                        WorktreeCreator::Attempt { run, task, .. } => runs
                            .entry(run)
                            .or_insert_with(|| client::get_run(run.clone()).ok())
                            .as_ref()
                            .and_then(|run| run.tasks.iter().find(|entry| &entry.id == task))
                            .map_or_else(|| format!("Task {task}"), |entry| entry.title.clone()),
                    };
                    format!(
                        "Power-created · {} · {creator} · {}",
                        worktree.branch,
                        path.display()
                    )
                }
                None => path.display().to_string(),
            };
            Row {
                label: path_name(&path),
                detail,
                choice: Choice::Worktree(path),
            }
        })
        .collect())
}

fn path_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn shortcut_digit(key: &str) -> Option<u8> {
    let [digit] = key.as_bytes() else { return None };
    digit.is_ascii_digit().then(|| digit - b'0')
}

/// The icon stored for a box, or `None` when it has none or the file is unreadable.
fn load_box_icon(box_id: &str) -> Option<Arc<RenderImage>> {
    let path = icon_path(box_id).ok()?;
    path.exists().then(|| decode_image(&path).ok()).flatten()
}

fn avatar(id: &str, colors: &Colors) -> impl IntoElement {
    let hash = id.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    let color = rgb(colors.indexed[1 + (hash >> 16) as usize % 6]);
    div()
        .flex_none()
        .p(px(1.))
        .bg(rgb(colors.background))
        .flex()
        .flex_col()
        .children((0..5).map(move |row| {
            div().flex().children((0_u32..5).map(move |column| {
                let bit = 3 * row + column.min(4 - column);
                div()
                    .size(px(3.))
                    .when(bit == 8 || hash & (1 << bit) != 0, |cell| cell.bg(color))
            }))
        }))
}

pub fn button(
    colors: &Colors,
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
) -> Stateful<Div> {
    unstyled_button(id, title).hover(|style| style.bg(rgb(colors.raised)))
}

/// A button without a hover style, for callers that choose their own. GPUI allows one per element.
fn unstyled_button(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .cursor_pointer()
        .flex_none()
        .text_sm()
        .child(title.into())
}

fn attention_fill(colors: &Colors, strength: u32) -> u32 {
    theme::blend(colors.raised, colors.indexed[3], strength)
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = colors(cx);
        let box_buttons = self.open_boxes().into_iter().map(|entry| {
            let digit = entry.shortcut.unwrap();
            let active = self.layout.active_box.as_ref() == Some(&entry.id);
            let needs_input = self.attention.contains_key(&entry.id);
            let id = entry.id.clone();
            div()
                .id(("box", usize::from(digit)))
                .w_full()
                .h(px(32.))
                .flex()
                .gap(px(3.))
                .items_center()
                .justify_center()
                .text_xs()
                .cursor_pointer()
                .border_l_2()
                .border_color(if active {
                    rgb(colors.accent)
                } else {
                    rgb(colors.surface)
                })
                .when(active, |strip| strip.bg(rgb(colors.raised)))
                .hover(move |strip| {
                    strip.bg(rgb(if needs_input {
                        attention_fill(colors, 58)
                    } else {
                        colors.raised
                    }))
                })
                .when(needs_input, |strip| {
                    let background = attention_fill(colors, if active { 48 } else { 32 });
                    strip
                        .bg(rgb(background))
                        .text_color(rgb(colors.contrast(background)))
                })
                .child(digit.to_string())
                .child(match self.box_icons.get(&entry.id) {
                    Some(icon) => img(icon.clone())
                        .flex_none()
                        .size(px(17.))
                        .into_any_element(),
                    None => avatar(&entry.id, colors).into_any_element(),
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.activate_box(id.clone(), window, cx);
                }))
        });
        let entry = self.active_box();
        let tabs = entry.into_iter().flat_map(|entry| {
            entry.views.iter().enumerate().map(|(index, view)| {
                let id = view.session.clone();
                let active = entry.active_view.as_ref() == Some(&id);
                let needs_input = self
                    .attention
                    .values()
                    .any(|sessions| sessions.contains(&id));
                unstyled_button(("terminal-tab", index), self.tab_label(index, view))
                    .hover(move |tab| {
                        tab.bg(rgb(if needs_input {
                            attention_fill(colors, 58)
                        } else {
                            colors.raised
                        }))
                    })
                    .when(active, |tab| tab.bg(rgb(colors.raised)))
                    .when(needs_input, |tab| {
                        let background = attention_fill(colors, if active { 48 } else { 32 });
                        tab.bg(rgb(background))
                            .text_color(rgb(colors.contrast(background)))
                            .when(active, |tab| tab.font_weight(FontWeight::SEMIBOLD))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.activate_tab(id.clone(), window, cx);
                    }))
            })
        });
        let terminal = entry
            .and_then(|entry| entry.active_view.as_ref())
            .and_then(|id| self.terminals.get(id))
            .map(|terminal| terminal.view.clone().into_any_element())
            .unwrap_or_else(|| {
                let message = if entry.is_some() {
                    "Ctrl+S then c opens a shell."
                } else if self.registry.is_some() {
                    "Ctrl+B then c creates a box. Ctrl+B then s opens the box picker."
                } else if self.error.is_none() {
                    "Loading boxes…"
                } else {
                    ""
                };
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(colors.muted))
                    .child(message)
                    .into_any_element()
            });
        let previews = self.previews(window, cx);
        let entity = cx.weak_entity();
        let modal = self.picker.as_ref().map(|picker| {
            let picker_id = picker.id;
            picker.render(colors, move |index, window, cx| {
                let _ = entity.update(cx, |this, cx| {
                    if let Some(choice) = this
                        .picker
                        .as_ref()
                        .filter(|picker| picker.id == picker_id)
                        .and_then(|picker| picker.choice(index))
                    {
                        this.choose(choice, window, cx);
                    }
                });
            })
        });
        let graph = self.dag.as_ref().map(|dag| {
            let entity = cx.weak_entity();
            let id = dag.id;
            let open = entity.clone();
            dag.render(
                colors,
                &self.keymap,
                move |action, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        if let Some(dag) = this.dag.as_mut().filter(|dag| dag.id == id)
                            && dag.graph_action(action)
                        {
                            cx.notify();
                        }
                    });
                },
                move |link, window, cx| {
                    let _ = open.update(cx, |this, cx| {
                        if this.dag.as_ref().is_some_and(|dag| dag.id == id) {
                            this.open_dag_artifact(link, window, cx);
                        }
                    });
                },
            )
        });
        div()
            .id("workspace")
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::key_down))
            .relative()
            .size_full()
            .flex()
            .bg(rgb(colors.background))
            .text_color(rgb(colors.foreground))
            .child(
                div()
                    .w(px(36.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_center()
                    .bg(rgb(colors.surface))
                    .border_r_1()
                    .border_color(rgb(colors.border))
                    .children(box_buttons)
                    .when(self.registry.is_some(), |strip| {
                        strip.child(button(colors, "boxes-button", "≡").on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.pick_boxes(window, cx),
                        )))
                    })
                    .child(div().flex_1())
                    .child(
                        button(
                            colors,
                            "keyboard-help",
                            match self.prefix {
                                Some(Prefix::Box) => "B",
                                Some(Prefix::View) => "S",
                                None => "?",
                            },
                        )
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.open_help(window, cx);
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .when(entry.is_some(), |main| {
                        main.child(
                            div()
                                .id("terminals")
                                .flex()
                                .flex_none()
                                .overflow_x_scroll()
                                .track_scroll(&self.tab_scroll)
                                .gap_1()
                                .p_1()
                                .border_b_1()
                                .border_color(rgb(colors.border))
                                .children(tabs)
                                .child(button(colors, "new-tab", "+").on_click(cx.listener(
                                    |this, _: &ClickEvent, window, cx| {
                                        this.launch_in_active_worktree(window, cx)
                                    },
                                ))),
                        )
                    })
                    .children(
                        [
                            self.error.clone(),
                            self.save_error.clone(),
                            self.config_error.clone(),
                        ]
                        .into_iter()
                        .flatten()
                        .map(|error| {
                            div()
                                .px_2()
                                .py_1()
                                .bg(rgb(colors.surface))
                                .text_color(rgb(colors.error))
                                .child(error)
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .child(terminal)
                                    .child(self.status_bar.clone()),
                            )
                            .children(previews),
                    ),
            )
            .children(modal)
            .children(graph)
    }
}

/// A per-call random value from the standard library's randomly keyed hasher.
fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

#[cfg(test)]
mod tests {
    use super::{
        AgentStatus, Coordinator, EndWorker, LinkAction, ManagedAgent, Run, Task, empty_message,
        end_worker_step, link_action, running_progress,
    };
    use domain::dag::{AgentState, Provider, TaskState};

    #[test]
    fn ending_a_busy_worker_needs_a_second_press() {
        let mut agent = ManagedAgent {
            id: "worker".into(),
            box_id: "box".into(),
            worktree: "/repo".into(),
            provider: Provider::Command,
            model: None,
            parent: None,
            session: None,
            run: None,
            task: None,
            attempt: None,
            state: AgentState::Running,
            status: AgentStatus::Working,
            label: "worker".into(),
            created_at: 0,
        };
        assert_eq!(
            end_worker_step(&agent, None, "Ctrl+X"),
            EndWorker::NoSession
        );
        agent.session = Some("session".into());
        let end = EndWorker::End("session".into());
        for status in [AgentStatus::Working, AgentStatus::NeedsInput] {
            agent.status = status;
            assert_eq!(
                end_worker_step(&agent, None, "Ctrl+X"),
                EndWorker::Confirm(
                    "Worker is still working — press Ctrl+X again to end it.".into()
                )
            );
            // Arming another worker does not confirm this one.
            assert!(matches!(
                end_worker_step(&agent, Some("other"), "Ctrl+X"),
                EndWorker::Confirm(_)
            ));
            assert_eq!(end_worker_step(&agent, Some("worker"), "Ctrl+X"), end);
        }
        agent.status = AgentStatus::Idle;
        assert!(matches!(
            end_worker_step(&agent, None, "Ctrl+X"),
            EndWorker::Confirm(_)
        ));
        assert_eq!(end_worker_step(&agent, Some("worker"), "Ctrl+X"), end);
        for status in [AgentStatus::Done, AgentStatus::Failed] {
            agent.status = status;
            assert_eq!(end_worker_step(&agent, None, "Ctrl+X"), end);
        }
    }

    #[test]
    fn empty_pickers_name_the_toggle_while_finished_items_are_hidden() {
        let message = |show_finished| {
            empty_message(
                "No runs.",
                "No unfinished runs.",
                show_finished,
                "Ctrl+F",
                "completed runs",
            )
        };
        assert_eq!(
            message(false),
            "No unfinished runs. Ctrl+F shows completed runs."
        );
        assert_eq!(message(true), "No runs.");
    }

    #[test]
    fn shows_progress_while_the_dag_has_actionable_work() {
        let mut run = Run {
            id: "run".into(),
            box_id: "box".into(),
            title: "Review a change".into(),
            coordinator: Coordinator::Agent {
                agent: "coordinator".into(),
            },
            created_at: 0,
            tasks: Vec::new(),
        };
        assert_eq!(running_progress(&run), None);
        for (id, state) in [("done", TaskState::Completed), ("review", TaskState::Ready)] {
            run.tasks.push(Task {
                id: id.into(),
                title: id.into(),
                instructions: String::new(),
                deps: Vec::new(),
                state,
                current_attempt: None,
                attempts: Vec::new(),
            });
        }
        for state in [TaskState::Ready, TaskState::Working, TaskState::NeedsInput] {
            run.tasks[1].state = state;
            assert_eq!(running_progress(&run).as_deref(), Some("DAG 1/2 done"));
        }
        for state in [TaskState::Waiting, TaskState::Failed, TaskState::Completed] {
            run.tasks[1].state = state;
            assert_eq!(running_progress(&run), None);
        }
    }

    #[test]
    fn dag_links_open_runs_directly() {
        assert_eq!(
            link_action("pwr://dag/Run-123"),
            LinkAction::OpenDag("Run-123")
        );
        for target in [
            "pwr://artifact/abc",
            "pwr://dag/",
            "pwr://dag/a/b",
            "./report.html",
        ] {
            assert_eq!(link_action(target), LinkAction::ResolveArtifact);
        }
    }
}
