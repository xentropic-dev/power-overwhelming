#[path = "agent_tree.rs"]
pub(crate) mod agent_tree;
#[path = "dag_view.rs"]
mod dag_view;

use self::{agent_tree::AgentTree, dag_view::DagView};
use crate::{
    keymap::{Command, Keymap, Scope},
    picker::{Choice, Kind, Picker, Row},
    preview::{Preview, PreviewAction},
    settings::{AutoOpen, Previews, Settings},
    status_bar::{ConfigChanged, StatusBar},
    terminal_view::{AgentChanged, BarContextChanged, OpenLink, TerminalView},
};
use anyhow::{Context as _, Result, ensure};
use application::{ResolveArtifact, StartSession, agents::AgentProfileRegistry};
use domain::{
    artifact::{ResolvedArtifact, Target},
    link::dag_run_id,
    session::{Agent, Session},
};
use gpui::{prelude::*, *};
use infrastructure::{
    agents::AgentProfiles,
    boxes::{Layout, Registry, SavedBox, SavedView},
    client::{self, Run},
    plugins::ActiveView,
    shell::{self, Shell},
    theme::{self, Colors, Scheme},
    workspace::{Checkout, main_checkout, repository_worktrees},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
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
    Runs(String),
    Run(String),
}

enum DagSnapshot {
    Runs(Vec<Run>),
    Run(Run),
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
    sessions: HashMap<String, Session>,
    terminals: HashMap<String, TerminalTab>,
    tab_scroll: ScrollHandle,
    launching: bool,
    focus: FocusHandle,
    prefix: Option<Prefix>,
    picker: Option<Picker>,
    dag: Option<DagView>,
    dag_refresh: async_channel::Sender<()>,
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
    agent_profiles: AgentProfiles,
    config_scheme: Option<&'static Scheme>,
    settings_ready: bool,
    daemon_theme: async_channel::Sender<&'static Scheme>,
    /// Whether keys go to the preview pane instead of the terminal.
    preview_focus: bool,
    /// The terminal showing numbered link hints, if any.
    hinting: Option<Entity<TerminalView>>,
    _bar_config: Subscription,
}

pub fn run() {
    Application::new().run(|cx| {
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
            client::ensure_daemon()?;
            Ok::<_, anyhow::Error>((registry, layout, client::list_sessions()?))
        });
        cx.spawn_in(window, async move |app, cx| {
            let result = load.await;
            let _ = app.update_in(cx, |this, window, cx| {
                match result {
                    Ok((registry, layout, sessions)) => {
                        this.load(registry, layout, sessions, window, cx);
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        Self::watch_artifacts(window, cx);
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
            sessions: HashMap::new(),
            terminals: HashMap::new(),
            tab_scroll: ScrollHandle::new(),
            launching: false,
            focus,
            prefix: None,
            picker: None,
            dag: None,
            dag_refresh,
            attention: HashMap::new(),
            previews: Vec::new(),
            active_previews: HashMap::new(),
            next_id: 0,
            error: None,
            save_error: None,
            status_bar,
            keymap: Keymap::default(),
            preview_settings: Previews::default(),
            agent_profiles: AgentProfiles::default(),
            config_scheme: None,
            settings_ready: false,
            daemon_theme,
            preview_focus: false,
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
                lines.push(format!("Shell integration is installed in {}.", rc.display()));
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
        let first_run = self
            .picker
            .as_ref()
            .is_none_or(|picker| matches!(picker.kind, Kind::Setup { first_run: true, .. }));
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
                self.resolve(session, link, window, cx);
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
                let Ok(query) = app.update(cx, |this, _| this.dag_query()) else {
                    break;
                };
                let attention = cx
                    .background_executor()
                    .spawn(async { client::attention() });
                if let Some((id, query)) = query {
                    let snapshot = cx
                        .background_executor()
                        .spawn(async move {
                            match query {
                                DagQuery::Runs(box_id) => {
                                    client::list_runs(Some(box_id), None).map(DagSnapshot::Runs)
                                }
                                DagQuery::Run(run) => client::get_run(run).map(DagSnapshot::Run),
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
            return Some((dag.id, DagQuery::Run(dag.run.id.clone())));
        }
        let picker = self.picker.as_ref()?;
        if let Kind::Runs { box_id, .. } = &picker.kind {
            return Some((picker.id, DagQuery::Runs(box_id.clone())));
        }
        None
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
                Ok(DagSnapshot::Run(run)) => {
                    let changed = dag.refresh_error.take().is_some();
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
            match result {
                Ok(DagSnapshot::Runs(mut runs)) => {
                    if initial && runs.len() == 1 {
                        if let Some(run) = runs.pop() {
                            self.open_dag(run, window, cx);
                        }
                        return;
                    }
                    picker.message = runs.is_empty().then(|| "This box has no DAG runs.".into());
                    picker.set_runs(runs);
                    if std::mem::take(&mut picker.accept_pending)
                        && let Some(choice) = picker.selection()
                    {
                        self.choose(choice, window, cx);
                    }
                }
                Err(error) => {
                    picker.accept_pending = false;
                    picker.message = Some(format!("Could not list runs. {error:#}"));
                }
                _ => return,
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
            self.resolve_in(box_id, artifact.session, artifact.uri, window, cx);
        }
    }

    fn focus_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_preview_index().is_some() {
            self.preview_focus = true;
            self.focus.focus(window);
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
            self.preview_focus = false;
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
        let view = cx.new(|cx| TerminalView::new(session, scheme, window, cx));
        let links = cx.subscribe_in(&view, window, |app, source, link: &OpenLink, window, cx| {
            let id = source.read(cx).session.id.clone();
            app.resolve(id, link.0.clone(), window, cx);
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
        self.preview_focus = false;
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
        let id = self.open_picker(
            Kind::Agents {
                box_id: box_id.clone(),
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
        let agents = cx
            .background_executor()
            .spawn(async move { client::list_agents(Some(box_id), None) });
        cx.spawn_in(window, async move |app, cx| {
            let result = agents.await;
            let _ = app.update_in(cx, |this, window, cx| {
                let Some(picker) = this.picker.as_mut().filter(|picker| picker.id == id) else {
                    return;
                };
                picker.loading = false;
                match result {
                    Ok(agents) => {
                        picker.message = agents
                            .is_empty()
                            .then(|| "This box has no managed agents.".into());
                        picker.set_agent_tree(AgentTree::new(agents));
                    }
                    Err(error) => {
                        picker.message = Some(format!("Could not list agents. {error:#}"))
                    }
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
        let id = self.open_picker(Kind::Worktrees(entry.id.clone()), Vec::new(), window, cx);
        self.load_picker_rows(
            id,
            "Looking up worktrees…",
            "No available worktrees.",
            window,
            cx,
            move || {
                Ok(repository_worktrees(&repository)?
                    .into_iter()
                    .filter(|path| path != &repository && path.is_dir())
                    .map(|path| Row {
                        label: path_name(&path),
                        detail: path.display().to_string(),
                        choice: Choice::Worktree(path),
                    })
                    .collect())
            },
        );
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
                        picker.message = rows.is_empty().then(|| empty.to_owned());
                        picker.rows = rows;
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
                    self.resolve(session, link, window, cx);
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
            Choice::Worktree(path) => {
                if let Kind::Worktrees(box_id) = &picker.kind {
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
            self.open_picker(Kind::End(id), Vec::new(), window, cx);
        }
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

    fn end_session(&mut self, session: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.terminals.get_mut(&session).filter(|tab| !tab.closing) else {
            return;
        };
        tab.closing = true;
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
            } else if picker.has_text_field() {
                Scope::Picker
            } else {
                Scope::Dialog
            };
            match self.keymap.command(scope, stroke) {
                Some(Command::PickerCancel) => self.dismiss_picker(window, cx),
                Some(Command::PickerSelect) => {
                    if event.is_held || picker.waiting {
                        return;
                    }
                    if picker.loading {
                        picker.accept_pending = true;
                    } else if let Kind::End(id) = &picker.kind {
                        let id = id.clone();
                        self.dismiss_picker(window, cx);
                        self.end_session(id, window, cx);
                    } else if let Some(choice) = picker.selection() {
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

        if self.preview_focus && self.prefix.is_none() {
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match link_action(&target) {
            LinkAction::OpenDag(run) => self.open_dag_link(run.to_owned(), window, cx),
            LinkAction::ResolveArtifact => {
                if let Some(box_id) = self.layout.active_box.clone() {
                    self.resolve_in(box_id, session, target, window, cx);
                }
            }
        }
    }

    /// Opens `target` in `box_id`'s preview pane without moving keyboard focus.
    fn resolve_in(
        &mut self,
        box_id: String,
        session: String,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = session.clone();
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
                if let Some(target) = &target
                    && let Some(index) = this
                        .previews
                        .iter()
                        .position(|tab| tab.box_id == box_id && tab.target.as_ref() == Some(target))
                {
                    let tab = &mut this.previews[index];
                    tab.originating_terminal = source;
                    tab.title = title;
                    if !matches!(target, Target::Loopback(_))
                        || matches!(tab.preview, Preview::Error(_))
                    {
                        tab.preview.close(window, cx);
                        tab.preview = Preview::open(target, window, cx);
                    }
                    this.activate_preview(index, cx);
                    return;
                }
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
                this.activate_preview(this.previews.len() - 1, cx);
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
        }
        cx.notify();
    }

    fn previews(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let colors = colors(cx);
        let active = self.active_preview_index()?;
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
                .w(relative(0.48))
                .flex_none()
                .min_w(px(245.))
                .h_full()
                .min_h_0()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(if self.preview_focus {
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
                .when(self.preview_focus, |pane| {
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

#[cfg(test)]
mod tests {
    use super::{LinkAction, link_action};

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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                .child(avatar(&entry.id, colors))
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
        let previews = self.previews(cx);
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
            dag.render(colors, &self.keymap, move |action, cx| {
                let _ = entity.update(cx, |this, cx| {
                    if let Some(dag) = this.dag.as_mut().filter(|dag| dag.id == id)
                        && dag.graph_action(action)
                    {
                        cx.notify();
                    }
                });
            })
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
