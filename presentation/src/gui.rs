use crate::{
    preview::Preview,
    terminal_view::{OpenLink, TerminalView},
};
use application::{ResolveArtifact, StartSession};
use domain::{
    artifact::{ResolvedArtifact, Target},
    session::{Agent, Session},
};
use gpui::{prelude::*, *};
use infrastructure::client;
use std::path::PathBuf;

struct TerminalTab {
    view: Entity<TerminalView>,
    session: Session,
    closing: bool,
    _links: Subscription,
}

struct PreviewTab {
    id: u64,
    title: String,
    target: Option<Target>,
    originating_terminal: String,
    preview: Preview,
}

struct Workspace {
    worktrees: Vec<PathBuf>,
    worktree: usize,
    select_worktree: bool,
    terminals: Vec<TerminalTab>,
    active_terminal: Option<usize>,
    previews: Vec<PreviewTab>,
    active_preview: Option<usize>,
    next_id: u64,
    error: Option<String>,
}

pub fn run(directory: PathBuf) {
    Application::new().run(move |cx| {
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
            cx.new(|cx| Workspace::new(directory, window, cx))
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
    fn new(directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let worktrees = match client::list_worktrees(directory.clone()) {
            Ok(worktrees) if !worktrees.is_empty() => worktrees,
            _ => vec![directory.clone()],
        };
        let worktree = worktrees
            .iter()
            .enumerate()
            .filter(|(_, path)| directory.starts_with(path))
            .max_by_key(|(_, path)| path.components().count())
            .map(|(index, _)| index)
            .unwrap_or(0);
        let mut this = Self {
            worktrees,
            worktree,
            select_worktree: false,
            terminals: Vec::new(),
            active_terminal: None,
            previews: Vec::new(),
            active_preview: None,
            next_id: 0,
            error: None,
        };
        this.refresh_terminals(window, cx);
        if let Some(index) = this
            .terminals
            .iter()
            .position(|tab| tab.session.worktree == this.worktrees[worktree])
        {
            this.activate_terminal(index, window, cx);
        }
        this
    }

    fn refresh_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match client::list_sessions() {
            Ok(sessions) => {
                let active = self
                    .active_terminal
                    .and_then(|index| self.terminals.get(index))
                    .map(|tab| tab.session.id.clone());
                self.terminals
                    .retain(|tab| sessions.iter().any(|session| session.id == tab.session.id));
                self.active_terminal = active
                    .as_ref()
                    .and_then(|id| self.terminals.iter().position(|tab| &tab.session.id == id));
                for session in sessions {
                    self.add_terminal(session, window, cx);
                }
                if self.active_terminal.is_none() && !self.terminals.is_empty() {
                    self.activate_terminal(0, window, cx);
                }
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn add_terminal(
        &mut self,
        session: Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(index) = self
            .terminals
            .iter()
            .position(|tab| tab.session.id == session.id)
        {
            return index;
        }
        let view = cx.new(|cx| TerminalView::new(session.clone(), window, cx));
        let links = cx.subscribe_in(&view, window, |app, source, link: &OpenLink, window, cx| {
            let id = source.read(cx).session.id.clone();
            app.resolve(id, link.0.clone(), window, cx);
        });
        self.terminals.push(TerminalTab {
            view,
            session,
            closing: false,
            _links: links,
        });
        self.terminals.len() - 1
    }

    fn launch(&mut self, agent: Agent, window: &mut Window, cx: &mut Context<Self>) {
        let command = StartSession {
            agent,
            worktree: self.worktrees[self.worktree].clone(),
        };
        let open = cx
            .background_executor()
            .spawn(async move { client::start_session(command) });
        cx.spawn_in(window, async move |app, cx| {
            let result = open.await;
            let _ = app.update_in(cx, |this, window, cx| {
                match result {
                    Ok(session) => {
                        this.error = None;
                        let index = this.add_terminal(session, window, cx);
                        this.activate_terminal(index, window, cx);
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn resolve(
        &mut self,
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
                        .position(|tab| tab.target.as_ref() == Some(target))
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
                let id = this.next_id;
                this.next_id += 1;
                this.previews.push(PreviewTab {
                    id,
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

    fn activate_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active_terminal = Some(index);
        let terminal = &self.terminals[index];
        terminal.view.read(cx).focus.focus(window);
        if let Some(worktree) = self
            .worktrees
            .iter()
            .position(|path| *path == terminal.session.worktree)
        {
            self.worktree = worktree;
        }
        cx.notify();
    }

    fn close_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &mut self.terminals[index];
        if tab.closing {
            return;
        }
        tab.closing = true;
        let session = tab.session.id.clone();
        let end = cx.background_executor().spawn({
            let session = session.clone();
            async move {
                let result = client::end_session(session.clone());
                if result.is_err()
                    && client::list_sessions()
                        .is_ok_and(|sessions| sessions.iter().all(|listed| listed.id != session))
                {
                    return Ok(());
                }
                result
            }
        });
        cx.spawn_in(window, async move |app, cx| {
            let result = end.await;
            let _ = app.update_in(cx, |this, window, cx| {
                let Some(index) = this
                    .terminals
                    .iter()
                    .position(|tab| tab.session.id == session)
                else {
                    return;
                };
                match result {
                    Ok(()) => {
                        this.error = None;
                        this.terminals.remove(index);
                        match this.active_terminal {
                            Some(active) if active == index => {
                                this.active_terminal = None;
                                if let Some(last) = this.terminals.len().checked_sub(1) {
                                    this.activate_terminal(index.min(last), window, cx);
                                }
                            }
                            Some(active) if active > index => {
                                this.active_terminal = Some(active - 1);
                            }
                            _ => {}
                        }
                    }
                    Err(error) => {
                        this.terminals[index].closing = false;
                        this.error = Some(format!("Could not close session. {error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn activate_preview(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.active_preview != Some(index) {
            if let Some(previous) = self.active_preview {
                self.previews[previous].preview.set_active(false, cx);
            }
            self.active_preview = Some(index);
            self.previews[index].preview.set_active(true, cx);
        }
        cx.notify();
    }

    fn close_preview(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.previews.remove(index);
        tab.preview.close(window, cx);
        self.active_preview = match self.active_preview {
            Some(active) if active == index => {
                (!self.previews.is_empty()).then(|| index.min(self.previews.len() - 1))
            }
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        if let Some(active) = self.active_preview {
            self.previews[active].preview.set_active(true, cx);
        }
        if let Some(terminal) = self
            .terminals
            .iter()
            .position(|terminal| terminal.session.id == tab.originating_terminal)
        {
            self.activate_terminal(terminal, window, cx);
        }
        cx.notify();
    }

    fn previews(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let active = self.active_preview?;
        let tabs = self.previews.iter().enumerate().map(|(index, tab)| {
            div()
                .flex()
                .min_w_0()
                .items_center()
                .when(index == active, |this| this.bg(rgb(0x293950)))
                .child(
                    button(("show-preview", tab.id), tab.title.clone()).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| this.activate_preview(index, cx),
                    )),
                )
                .child(button(("close-preview", tab.id), "×").on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| this.close_preview(index, window, cx),
                )))
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
                .border_color(rgb(0x293950))
                .bg(rgb(0x151e2c))
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
                        .child(self.previews[active].preview.render()),
                )
                .into_any_element(),
        )
    }
}

pub fn button(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .flex_none()
        .text_sm()
        .hover(|style| style.bg(rgb(0x293950)))
        .child(title.into())
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let directory = self.worktrees[self.worktree].display().to_string();
        let worktrees = self.worktrees.iter().enumerate().map(|(index, path)| {
            button(("worktree", index), path.display().to_string()).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| {
                    this.worktree = index;
                    this.select_worktree = false;
                    cx.notify();
                },
            ))
        });
        let sessions = self.terminals.iter().enumerate().map(|(index, tab)| {
            let id = tab.view.entity_id();
            let name = tab
                .session
                .worktree
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            div()
                .flex()
                .flex_none()
                .items_center()
                .rounded_sm()
                .when(Some(index) == self.active_terminal, |this| {
                    this.bg(rgb(0x293950))
                })
                .child(
                    button(
                        ("terminal-tab", id),
                        format!("{} · {name}", tab.session.agent.name()),
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.activate_terminal(index, window, cx)
                        },
                    )),
                )
                .child(if tab.closing {
                    button(("close-terminal", id), "…").cursor_default()
                } else {
                    button(("close-terminal", id), "×").on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.close_terminal(index, window, cx)
                        },
                    ))
                })
        });
        let terminal = self
            .active_terminal
            .map(|index| self.terminals[index].view.clone().into_any_element())
            .unwrap_or_else(|| {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0x9cacc3))
                    .child("Start a shell, Codex, or Claude in the selected worktree.")
                    .into_any_element()
            });
        let previews = self.previews(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x101722))
            .text_color(rgb(0xdbe3ee))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0x182232))
                    .child(
                        button("worktree-menu", format!("{directory} ▾")).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.select_worktree = !this.select_worktree;
                                cx.notify();
                            },
                        )),
                    )
                    .children([Agent::Shell, Agent::Codex, Agent::Claude].into_iter().map(
                        |agent| {
                            button(agent.name(), format!("+ {}", agent.name())).on_click(
                                cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.launch(agent, window, cx)
                                }),
                            )
                        },
                    ))
                    .child(button("refresh", "Refresh sessions").on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.refresh_terminals(window, cx),
                    ))),
            )
            .when(self.select_worktree, |this| {
                this.child(
                    div()
                        .id("worktrees")
                        .flex()
                        .flex_col()
                        .flex_none()
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .p_2()
                        .bg(rgb(0x182232))
                        .children(worktrees),
                )
            })
            .child(
                div()
                    .id("terminals")
                    .flex()
                    .flex_none()
                    .overflow_x_scroll()
                    .gap_1()
                    .p_1()
                    .border_b_1()
                    .border_color(rgb(0x293950))
                    .children(sessions),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().px_2().py_1().bg(rgb(0x3a2328)).child(error))
            })
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
                            .child(terminal),
                    )
                    .children(previews),
            )
    }
}
