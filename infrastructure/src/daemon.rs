use crate::{
    agents::AgentProfiles,
    box_icons::IconFiles,
    boxes::BoxesFile,
    budget::{LocalWorkerBudget, ProcMemory},
    cgroup::{self, WorkerScopes},
    dag_adapter::{self, DaemonDag},
    dag_store::SqliteDagStore,
    persistence::{SqliteArtifacts, SqliteSessions},
    plugins::Plugins,
    scheduler::{self, TimerScheduler},
    system::ProcSystem,
    talk_adapter::{self, DaemonTalk},
    terminal::{ActiveScheme, Command, PtyRuntime},
    theme,
    usage::ProviderUsage,
    wire::{self, DagRequest, DagResponse, Request, Response},
    workspace::{LocalWorkspace, LocalWorktreeRoot},
};
use anyhow::{Context, Result, bail, ensure};
use application::{Application, PublishArtifact, ResolveArtifact, StartSession, dag::EventScope};
use base64::{Engine, engine::general_purpose::STANDARD};
use domain::{
    session::{Agent, Geometry},
    talk::{HookState, NoticePolicy},
};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    io::BufReader,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

type Watchers = std::sync::Mutex<Vec<mpsc::SyncSender<Response>>>;

/// Clients streaming publish events. Senders whose client has gone are dropped on the next publish.
static ARTIFACT_WATCHERS: Watchers = std::sync::Mutex::new(Vec::new());
/// Clients streaming box icon changes, pruned the same way.
static BOX_ICON_WATCHERS: Watchers = std::sync::Mutex::new(Vec::new());

/// Streams every event sent to the watcher list until the client disconnects.
fn stream(mut socket: UnixStream, watchers: &Watchers) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel(64);
    watchers.lock().unwrap().push(tx);
    // Keepalives let a closed reader surface as a write error while events are quiet.
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(event) => wire::send(&mut socket, &event)?,
            Err(mpsc::RecvTimeoutError::Timeout) => wire::send(&mut socket, &Response::Ok)?,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn broadcast(watchers: &Watchers, event: impl Fn() -> Response) {
    watchers.lock().unwrap().retain(|watcher| {
        !matches!(
            watcher.try_send(event()),
            Err(mpsc::TrySendError::Disconnected(_))
        )
    });
}

struct Server {
    application: Arc<Application<PtyRuntime>>,
    plugins: Option<Plugins>,
    scheme: ActiveScheme,
    dag: Arc<DaemonDag>,
    talk: Arc<DaemonTalk>,
    talk_wake: mpsc::SyncSender<()>,
    system: ProcSystem,
    usage: ProviderUsage,
}

/// Runs the daemon. `finished_worker_grace` is how long a DAG worker runs after its
/// terminal report before its session ends, or `None` to leave it running.
pub fn serve(
    profiles: AgentProfiles,
    worktree_root: LocalWorktreeRoot,
    finished_worker_grace: Option<Duration>,
    talk_policy: NoticePolicy,
    budget: LocalWorkerBudget,
    stall_after: Option<Duration>,
) -> Result<()> {
    let socket = wire::socket_path();
    let parent = socket
        .parent()
        .context("Socket requires a parent directory")?;
    if !parent.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(socket.with_extension("lock"))?;
    if FileExt::try_lock_exclusive(&lock).is_err() {
        bail!("Power daemon is already running");
    }
    match fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&socket)?,
        Ok(_) => bail!(
            "Socket path already contains a different file: {}",
            socket.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let artifacts = SqliteArtifacts::open()?;
    let sessions = SqliteSessions::open().context("Open session registry")?;
    let store = SqliteDagStore::open().context("Open DAG store")?;
    let terminal_application = Arc::new(OnceLock::<Weak<Application<PtyRuntime>>>::new());
    let foreground_application = terminal_application.clone();
    let labels_application = terminal_application.clone();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let terminal_dag = Arc::new(OnceLock::<Weak<DaemonDag>>::new());
    let exit_dag = terminal_dag.clone();
    let model_dag = terminal_dag.clone();
    let foreground_dag = terminal_dag.clone();
    let scheme = Arc::new(RwLock::new(theme::default_scheme()));
    let profiles = Arc::new(profiles);
    let scopes = worker_scopes();
    let (talk_wake, talk_activity) = mpsc::sync_channel(1);
    let application = Arc::new(Application::new(
        artifacts,
        Arc::new(sessions),
        LocalWorkspace,
        PtyRuntime {
            socket: socket.clone(),
            scheme: scheme.clone(),
            profiles: profiles.clone(),
            talk_wake: talk_wake.clone(),
            on_exit: Arc::new(move |session, detail, complete| {
                if let Some(dag) = exit_dag.get().and_then(Weak::upgrade)
                    && let Err(error) = dag.session_exited(session, detail, complete)
                {
                    eprintln!("Could not record exit for terminal {session}. {error:#}");
                }
            }),
            on_model_changed: Arc::new(move |session, agent, model| {
                if let Some(dag) = model_dag.get().and_then(Weak::upgrade) {
                    dag.agent_model_changed(session, agent, model)?;
                }
                Ok(())
            }),
            on_foreground_changed: Arc::new(move |session, agent, conversation, cwd, labels| {
                if let Some(application) = foreground_application.get().and_then(Weak::upgrade) {
                    application.record_foreground(session, agent, conversation, cwd, labels)?;
                }
                Ok(())
            }),
            on_labels_changed: Arc::new(move |session, labels| {
                if let Some(application) = labels_application.get().and_then(Weak::upgrade) {
                    application.record_labels(session, labels)?;
                }
                Ok(())
            }),
            on_foreground_left: Arc::new(move |session, agent| {
                if let Some(dag) = foreground_dag.get().and_then(Weak::upgrade)
                    && let Err(error) = dag.foreground_left(session, agent)
                {
                    eprintln!("Could not end the foreground identity in {session}. {error:#}");
                }
            }),
            shutting_down: shutting_down.clone(),
            scopes,
        },
        profiles,
        || uuid::Uuid::new_v4().to_string(),
    ));
    let mut dag = DaemonDag::new(
        application.clone(),
        store.clone(),
        BoxesFile,
        IconFiles::open()?,
    )
    .with_worktree_root(worktree_root)
    .with_worker_budget(budget, ProcMemory);
    if let Some(grace) = finished_worker_grace {
        // Teardowns never wait behind the store writes that follow them.
        let teardown = TimerScheduler::start("power-teardown", scheduler::DEFAULT_CAPACITY)?;
        let persistence = TimerScheduler::start("power-forget", scheduler::DEFAULT_CAPACITY)?;
        dag = dag.with_finished_worker_cleanup(grace, Arc::new(teardown), Arc::new(persistence));
    }
    if let Some(after) = stall_after {
        dag = dag.with_stall_check(
            after,
            crate::stall_probe::LocalWorkerProbe::new(application.clone()),
        );
    }
    let dag = dag.shared();
    terminal_application
        .set(Arc::downgrade(&application))
        .expect("connect terminal callbacks to the application once");
    // Connect exits first, so a restored worker that exits at once is still reported.
    terminal_dag
        .set(Arc::downgrade(&dag))
        .expect("connect terminal callbacks to the DAG once");
    // Listen before restoring. A restored worker can finish its continuation turn while a
    // later session is still restoring, and its turn-end report must not be lost.
    delay_for_tests("POWER_TEST_BIND_DELAY_MS");
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let startup = Arc::new(Startup::default());
    let acceptor = {
        let listener = listener.try_clone()?;
        let startup = startup.clone();
        let application = application.clone();
        thread::spawn(move || -> Result<()> {
            for socket in listener.incoming() {
                let socket = socket?;
                let startup = startup.clone();
                let application = application.clone();
                thread::spawn(move || {
                    let _ = accept(socket, &startup, &application);
                });
            }
            Ok(())
        })
    };
    dag.restore().context("Restore sessions")?;
    let talk = talk_adapter::start(application.clone(), store, talk_policy, talk_activity);
    handle_shutdown(application.clone(), shutting_down)?;
    start_queue(dag.clone(), &listener);
    start_stall_checks(dag.clone());
    // Lets a test hold startup open, as if a later session were still restoring.
    delay_for_tests("POWER_TEST_RESTORE_DELAY_MS");
    startup.open(Arc::new(Server {
        plugins: Plugins::start(application.clone()),
        talk,
        talk_wake,
        dag,
        application,
        scheme,
        system: ProcSystem::default(),
        usage: ProviderUsage::from_env(),
    }));
    acceptor
        .join()
        .unwrap_or_else(|_| bail!("The daemon's listener stopped"))
}

/// How long the queue waits for a run event before it checks the budget again, so memory
/// that frees up without an event still starts queued work.
const QUEUE_RECHECK: Duration = Duration::from_secs(5);

/// Holds the bind in debug builds, so tests can show that nothing a worker needs the
/// socket for starts before the daemon listens.
fn delay_for_tests(_variable: &str) {
    #[cfg(debug_assertions)]
    if let Some(delay) = std::env::var(_variable)
        .ok()
        .and_then(|millis| millis.parse().ok())
    {
        thread::sleep(Duration::from_millis(delay));
    }
}

/// Starts queued dispatches whenever a run event may have freed capacity. It takes the
/// bound listener because a queued worker may report at once, and its report needs the
/// socket.
fn start_queue(dag: Arc<DaemonDag>, _listening: &UnixListener) {
    thread::spawn(move || {
        let mut cursor = dag.latest_seq().unwrap_or(0);
        loop {
            if let Err(error) = dag.start_queued() {
                eprintln!("Could not start queued DAG workers. {error:#}");
            }
            match dag.events(EventScope::Watch(None), cursor, QUEUE_RECHECK) {
                Ok(page) => cursor = page.cursor,
                Err(_) => thread::sleep(QUEUE_RECHECK),
            }
        }
    });
}

/// Flags idle workers whose builds and tests are not running, so a coordinator sees
/// them without polling transcripts.
fn start_stall_checks(dag: Arc<DaemonDag>) {
    let Some(interval) = dag.stall_interval() else {
        return;
    };
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            match dag.check_stalls() {
                Ok(stalled) => {
                    for attempt in stalled {
                        eprintln!("Attempt {attempt} looks stalled.");
                    }
                }
                Err(error) => eprintln!("Could not check for stalled DAG workers. {error:#}"),
            }
        }
    });
}

/// Finds how this system lets Power own workers, and logs it. Without cgroup
/// ownership, Power falls back to process discovery.
fn worker_scopes() -> Option<Arc<WorkerScopes>> {
    if std::env::var_os(cgroup::DISABLE_ENV).is_some_and(|value| value == "0") {
        eprintln!(
            "WARNING: {}=0, so workers run without cgroup ownership. Each runs under a \
             subreaper that ends everything it started.",
            cgroup::DISABLE_ENV
        );
        return None;
    }
    let slice = std::env::var(cgroup::SLICE_ENV).unwrap_or_else(|_| cgroup::DEFAULT_SLICE.into());
    match WorkerScopes::detect(&slice) {
        Ok(scopes) => {
            eprintln!(
                "Each worker runs in its own systemd user scope in {}, whose cgroup owns every process it starts.",
                scopes.slice()
            );
            Some(Arc::new(scopes))
        }
        Err(error) => {
            eprintln!(
                "WARNING: workers run without cgroup ownership ({error:#}). Each runs under a \
                 subreaper instead, which adopts and ends everything it started, though it \
                 cannot freeze them."
            );
            None
        }
    }
}

/// Bounds the final save so a reboot is not held up by a stuck terminal.
const SHUTDOWN_FLUSH: Duration = Duration::from_secs(2);

/// On SIGTERM or SIGINT, saves scrollback and foreground state for every live
/// session, then exits. The saved records restore the sessions on the next start.
fn handle_shutdown(
    application: Arc<Application<PtyRuntime>>,
    shutting_down: Arc<AtomicBool>,
) -> Result<()> {
    use signal_hook::{
        consts::{SIGINT, SIGTERM},
        iterator::Signals,
    };

    let mut signals = Signals::new([SIGTERM, SIGINT]).context("Install shutdown handler")?;
    thread::spawn(move || {
        if signals.forever().next().is_none() {
            return;
        }
        // Agents killed with the daemon must not look like sessions the user ended.
        shutting_down.store(true, Ordering::Release);
        let deadline = Instant::now() + SHUTDOWN_FLUSH;
        let (done, flushed) = mpsc::sync_channel(64);
        let mut pending = 0;
        for session in application.list_sessions() {
            if let Ok(terminal) = application.terminal(&session.id)
                && terminal
                    .send(Command::FlushScrollback(done.clone()))
                    .is_ok()
            {
                pending += 1;
            }
        }
        drop(done);
        for _ in 0..pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if flushed.recv_timeout(remaining).is_err() {
                eprintln!("Some sessions did not save before shutdown.");
                break;
            }
        }
        std::process::exit(0);
    });
    Ok(())
}

/// Connections accepted while the daemon restores sessions. Turn reports are applied at
/// once, or kept until their session is restored. Other requests wait for the server.
#[derive(Default)]
struct Startup {
    server: Mutex<StartupState>,
    opened: Condvar,
}

#[derive(Default)]
struct StartupState {
    server: Option<Arc<Server>>,
    /// Turn reports from sessions that were not registered yet when they arrived.
    turns: Vec<(String, HookState, bool)>,
}

impl Startup {
    fn open(&self, server: Arc<Server>) {
        let turns = {
            let mut state = self.server.lock().unwrap();
            state.server = Some(server.clone());
            std::mem::take(&mut state.turns)
        };
        self.opened.notify_all();
        for (session, hook, ready_only) in turns {
            match server.application.terminal(&session) {
                Ok(terminal) => terminal.report_turn(hook, ready_only),
                Err(_) => eprintln!("Dropped a turn report from unrestored session {session}"),
            }
        }
    }

    fn server(&self) -> Arc<Server> {
        let mut state = self.server.lock().unwrap();
        loop {
            if let Some(server) = &state.server {
                return server.clone();
            }
            state = self.opened.wait(state).unwrap();
        }
    }

    /// Applies a turn report during startup. Returns false once the server is running.
    fn report_turn(
        &self,
        application: &Application<PtyRuntime>,
        session: &str,
        hook: HookState,
        ready_only: bool,
    ) -> bool {
        let mut state = self.server.lock().unwrap();
        if state.server.is_some() {
            return false;
        }
        match application.terminal(session) {
            Ok(terminal) => terminal.report_turn(hook, ready_only),
            Err(_) => state.turns.push((session.into(), hook, ready_only)),
        }
        true
    }
}

fn accept(
    mut socket: UnixStream,
    startup: &Startup,
    application: &Application<PtyRuntime>,
) -> Result<()> {
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    let request = wire::receive(&mut BufReader::new(&socket))?.context("Missing request")?;
    if let Request::Talk {
        talk:
            wire::TalkRequest {
                session: Some(session),
                command: wire::talk::TalkCommand::Turn { ready, ready_only },
                ..
            },
    } = &request
    {
        let hook = if *ready {
            HookState::Ready
        } else {
            HookState::Busy
        };
        if startup.report_turn(application, session, hook, *ready_only) {
            return wire::send(
                &mut socket,
                &Response::Talk {
                    talk: wire::talk::TalkResponse::Ok,
                },
            );
        }
    }
    handle(socket, request, &startup.server())
}

/// Whether the process at the other end of `socket` is the desktop: it holds the box
/// layout lock, and runs in no Power session, so no agent can pass as it.
fn verified_desktop(socket: &UnixStream, server: &Server) -> bool {
    let Ok(pid) = wire::peer_pid(socket) else {
        return false;
    };
    crate::boxes::holds_layout_lock(pid)
        && !server
            .dag
            .list_sessions()
            .iter()
            .any(|session| server.application.session_owns_process(&session.id, pid))
}

fn handle(mut socket: UnixStream, mut request: Request, server: &Server) -> Result<()> {
    // A client only claims the desktop. The daemon decides.
    if let Request::Talk { talk } = &mut request
        && talk.desktop
    {
        talk.desktop = verified_desktop(&socket, server);
    }
    let application = &server.application;
    match request {
        Request::WatchArtifacts => return stream(socket, &ARTIFACT_WATCHERS),
        Request::WatchBoxIcons => return stream(socket, &BOX_ICON_WATCHERS),
        _ => {}
    }
    if let Request::Subscribe { session } = &request {
        let (tx, rx) = mpsc::sync_channel(64);
        match application
            .terminal(session)
            .and_then(|actor| Ok(actor.send(Command::Attach(tx))?))
        {
            Ok(()) => {
                // Check for a closed reader even when its terminal is idle.
                loop {
                    match rx.recv_timeout(Duration::from_secs(10)) {
                        Ok(event) => wire::send(&mut socket, &event)?,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            wire::send(&mut socket, &Response::Ok)?
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                return Ok(());
            }
            Err(error) => {
                return wire::send(
                    socket,
                    &Response::Error {
                        message: error.to_string(),
                    },
                );
            }
        }
    }

    if let Request::Dag {
        dag: DagRequest::Watch { box_sel },
    } = request
    {
        if let Err(error) = dag_adapter::watch(&server.dag, &mut socket, box_sel.as_deref()) {
            wire::send(
                socket,
                &Response::Error {
                    message: format!("{error:#}"),
                },
            )?;
        }
        return Ok(());
    }

    if let Request::Talk {
        talk:
            talk @ wire::TalkRequest {
                command: wire::talk::TalkCommand::Watch,
                ..
            },
    } = request
    {
        if let Err(error) = talk_adapter::watch(&server.talk, &server.dag, talk, &socket) {
            wire::send(
                socket,
                &Response::Error {
                    message: format!("{error:#}"),
                },
            )?;
        }
        return Ok(());
    }

    if let Request::Talk {
        talk:
            talk @ wire::TalkRequest {
                command: wire::talk::TalkCommand::HookStop | wire::talk::TalkCommand::HookWait,
                ..
            },
    } = request
    {
        let handler = if matches!(talk.command, wire::talk::TalkCommand::HookStop) {
            talk_adapter::hook_stop
        } else {
            talk_adapter::hook_wait
        };
        let result = handler(&server.talk, &server.dag, talk, &socket);
        // A turn change or a returned claim may make notices due.
        let _ = server.talk_wake.try_send(());
        if let Err(error) = result {
            wire::send(
                socket,
                &Response::Error {
                    message: format!("{error:#}"),
                },
            )?;
        }
        return Ok(());
    }

    let is_talk = matches!(&request, Request::Talk { .. })
        || matches!(&request, Request::Dag { dag } if matches!(dag, DagRequest::EditTask { .. }));
    let response =
        handle_request(request, server, &socket).unwrap_or_else(|error| Response::Error {
            message: format!("{error:#}"),
        });
    wire::send(socket, &response)?;
    if is_talk {
        let _ = server.talk_wake.try_send(());
    }
    Ok(())
}

fn handle_request(request: Request, server: &Server, socket: &UnixStream) -> Result<Response> {
    let application = &server.application;
    match request {
        Request::SetColorScheme { name } => {
            let scheme =
                theme::find(&name).with_context(|| format!("Unknown color scheme {name:?}"))?;
            *server.scheme.write().unwrap() = scheme;
            for session in application.list_sessions() {
                if let Ok(terminal) = application.terminal(&session.id) {
                    terminal.refresh_theme();
                }
            }
            Ok(Response::Ok)
        }
        Request::List => Ok(Response::Sessions {
            sessions: application
                .list_sessions()
                .into_iter()
                .map(Into::into)
                .collect(),
        }),
        Request::ListWithAttempts => Ok(Response::SessionListings {
            sessions: application
                .list_session_listings()
                .into_iter()
                .map(Into::into)
                .collect(),
        }),
        Request::Worktrees { directory } => Ok(Response::Worktrees {
            worktrees: application.list_worktrees(&directory)?,
        }),
        Request::WorkingDirectory { session } => {
            let (send, receive) = mpsc::sync_channel(1);
            application
                .terminal(&session)?
                .send(Command::WorkingDirectory(send))?;
            let directory = receive.recv_timeout(Duration::from_secs(3))?;
            Ok(Response::WorkingDirectory { directory })
        }
        Request::SetMetadata {
            session,
            field,
            value,
        } => {
            let value = value.trim();
            ensure!(
                crate::terminal::valid_metadata(value),
                "Session metadata must be at most 120 characters on one line"
            );
            if matches!(field, wire::MetadataField::Usage) {
                crate::usage::decode_usage(value)?;
            }
            let usage = matches!(field, wire::MetadataField::Usage).then(|| value.to_owned());
            let value = (!value.is_empty()).then(|| value.to_owned());
            let (send, receive) = mpsc::sync_channel(1);
            application.terminal(&session)?.send(Command::SetMetadata {
                pid: wire::peer_pid(socket)?,
                field,
                value,
                reply: send,
            })?;
            ensure!(
                receive.recv_timeout(Duration::from_secs(3))??,
                "Session metadata must come from the active agent"
            );
            if let Some(usage) = usage {
                server.usage.claude.record(&usage)?;
            }
            Ok(Response::Ok)
        }
        Request::SetConversation {
            session,
            agent,
            conversation,
            fresh,
            resumed,
        } => {
            ensure!(
                !agent.is_empty()
                    && crate::terminal::valid_metadata(&agent)
                    && conversation.as_deref().is_none_or(|conversation| {
                        !conversation.is_empty() && crate::terminal::valid_metadata(conversation)
                    }),
                "Conversation IDs must be at most 120 characters on one line"
            );
            let (send, receive) = mpsc::sync_channel(1);
            application
                .terminal(&session)?
                .send(Command::SetConversation {
                    pid: wire::peer_pid(socket)?,
                    agent: Agent::Coding(agent.into()),
                    conversation,
                    fresh,
                    resumed,
                    reply: send,
                })?;
            ensure!(
                receive.recv_timeout(Duration::from_secs(3))?,
                "Conversations must come from a process in this session"
            );
            Ok(Response::Ok)
        }
        Request::Launch { worktree, agent } => {
            let session = application.start_session(StartSession {
                worktree,
                agent: agent.into(),
            })?;
            Ok(Response::Launched {
                session: session.into(),
            })
        }
        Request::Input { session, data } => {
            ensure!(data.len() <= 350_000, "Terminal input is too large");
            ensure!(
                application
                    .terminal(&session)?
                    .input(STANDARD.decode(data)?, None)?,
                "Terminal session has exited"
            );
            Ok(Response::Ok)
        }
        Request::Resize { session, geometry } => {
            let geometry = Geometry::from(geometry);
            ensure!(geometry.is_valid(), "Invalid terminal geometry");
            application
                .terminal(&session)?
                .send(Command::Resize(geometry))?;
            Ok(Response::Ok)
        }
        Request::Publish {
            session,
            cwd,
            target,
            title,
        } => {
            let published = application.publish_artifact(PublishArtifact {
                session,
                cwd,
                target,
                title,
            })?;
            let artifact = wire::Artifact {
                session: published.session.clone(),
                uri: published.uri.clone(),
                title: published.title.clone(),
            };
            broadcast(&ARTIFACT_WATCHERS, || Response::ArtifactPublished {
                artifact: artifact.clone(),
            });
            let uri = if let Some(plugins) = &server.plugins {
                let uri = published.uri.clone();
                plugins.notify(published);
                uri
            } else {
                published.uri
            };
            Ok(Response::Published { uri })
        }
        Request::Resolve { session, target } => {
            let artifact = application.resolve_artifact(ResolveArtifact { session, target })?;
            Ok(Response::Resolved {
                target: artifact.target.into(),
                title: artifact.title,
            })
        }
        Request::End { session } => {
            server.dag.end_session(&session)?;
            Ok(Response::Ok)
        }
        Request::RecentArtifacts { sessions, limit } => Ok(Response::RecentArtifacts {
            artifacts: application
                .recent_artifacts(&sessions, limit.min(500))?
                .into_iter()
                .map(Into::into)
                .collect(),
        }),
        Request::Talk { talk } => Ok(Response::Talk {
            talk: talk_adapter::request(
                &server.talk,
                &server.dag,
                talk,
                wire::peer_pid(socket).ok(),
            )?,
        }),
        Request::Dag { dag } => {
            let dag =
                dag_adapter::request(&server.dag, &server.talk, dag, wire::peer_pid(socket).ok())?;
            if let DagResponse::BoxIcon { box_id } = &dag {
                broadcast(&BOX_ICON_WATCHERS, || Response::BoxIconChanged {
                    box_id: box_id.clone(),
                });
            }
            Ok(Response::Dag { dag: Box::new(dag) })
        }
        Request::Overview => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            let overview = application::overview::OverviewQuery {
                system: &server.system,
                usage: &server.usage,
            }
            .execute(&*server.dag, now)?;
            Ok(Response::Overview {
                overview: Box::new(overview.into()),
            })
        }
        Request::WatchArtifacts | Request::WatchBoxIcons => {
            unreachable!("Watches use the streaming handler")
        }
        Request::Subscribe { .. } => unreachable!("Subscriptions use the streaming handler"),
    }
}
