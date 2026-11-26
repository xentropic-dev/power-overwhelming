use crate::{
    agents::AgentProfiles,
    box_icons::IconFiles,
    boxes::BoxesFile,
    dag_adapter::{self, DaemonDag},
    dag_store::SqliteDagStore,
    persistence::{SqliteArtifacts, SqliteSessions},
    plugins::Plugins,
    scheduler::{self, TimerScheduler},
    terminal::{ActiveScheme, Command, PtyRuntime},
    theme,
    wire::{self, DagRequest, DagResponse, Request, Response},
    workspace::{LocalWorkspace, LocalWorktreeRoot},
};
use anyhow::{Context, Result, bail, ensure};
use application::{Application, PublishArtifact, ResolveArtifact, StartSession};
use base64::{Engine, engine::general_purpose::STANDARD};
use domain::session::{Agent, Geometry};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    io::BufReader,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    sync::{
        Arc, OnceLock, RwLock, Weak,
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
}

/// Runs the daemon. `finished_worker_grace` is how long a DAG worker runs after its
/// terminal report before its session ends, or `None` to leave it running.
pub fn serve(
    profiles: AgentProfiles,
    worktree_root: LocalWorktreeRoot,
    finished_worker_grace: Option<Duration>,
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
    let shutting_down = Arc::new(AtomicBool::new(false));
    let terminal_dag = Arc::new(OnceLock::<Weak<DaemonDag>>::new());
    let exit_dag = terminal_dag.clone();
    let model_dag = terminal_dag.clone();
    let scheme = Arc::new(RwLock::new(theme::default_scheme()));
    let profiles = Arc::new(profiles);
    let application = Arc::new(Application::new(
        artifacts,
        Arc::new(sessions),
        LocalWorkspace,
        PtyRuntime {
            socket: socket.clone(),
            scheme: scheme.clone(),
            profiles: profiles.clone(),
            on_exit: Arc::new(move |session, detail| {
                if let Some(dag) = exit_dag.get().and_then(Weak::upgrade)
                    && let Err(error) = dag.session_exited(session, detail)
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
            on_foreground_changed: Arc::new(move |session, agent, conversation, cwd| {
                if let Some(application) = foreground_application.get().and_then(Weak::upgrade) {
                    application.record_foreground(session, agent, conversation, cwd)?;
                }
                Ok(())
            }),
            shutting_down: shutting_down.clone(),
        },
        profiles,
        || uuid::Uuid::new_v4().to_string(),
    ));
    let mut dag = DaemonDag::new(application.clone(), store, BoxesFile, IconFiles::open()?)
        .with_worktree_root(worktree_root);
    if let Some(grace) = finished_worker_grace {
        let scheduler = TimerScheduler::start(scheduler::DEFAULT_CAPACITY)?;
        dag = dag.with_finished_worker_cleanup(grace, Arc::new(scheduler));
    }
    let dag = Arc::new(dag);
    terminal_application
        .set(Arc::downgrade(&application))
        .expect("connect terminal callbacks to the application once");
    // Connect exits first, so a restored worker that exits at once is still reported.
    terminal_dag
        .set(Arc::downgrade(&dag))
        .expect("connect terminal callbacks to the DAG once");
    dag.restore().context("Restore sessions")?;
    handle_shutdown(application.clone(), shutting_down)?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let server = Arc::new(Server {
        plugins: Plugins::start(application.clone()),
        dag,
        application,
        scheme,
    });
    for socket in listener.incoming() {
        let socket = socket?;
        let server = server.clone();
        thread::spawn(move || {
            let _ = handle(socket, &server);
        });
    }
    Ok(())
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

fn handle(mut socket: UnixStream, server: &Server) -> Result<()> {
    let application = &server.application;
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    let request = wire::receive(&mut BufReader::new(&socket))?.context("Missing request")?;
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

    let response =
        handle_request(request, server, &socket).unwrap_or_else(|error| Response::Error {
            message: format!("{error:#}"),
        });
    wire::send(socket, &response)
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
            let value = (!value.is_empty()).then(|| value.to_owned());
            let (send, receive) = mpsc::sync_channel(1);
            application.terminal(&session)?.send(Command::SetMetadata {
                pid: peer_pid(socket)?,
                field,
                value,
                reply: send,
            })?;
            ensure!(
                receive.recv_timeout(Duration::from_secs(3))??,
                "Session metadata must come from the active agent"
            );
            Ok(Response::Ok)
        }
        Request::SetConversation {
            session,
            agent,
            conversation,
        } => {
            ensure!(
                !agent.is_empty()
                    && !conversation.is_empty()
                    && crate::terminal::valid_metadata(&agent)
                    && crate::terminal::valid_metadata(&conversation),
                "Conversation IDs must be at most 120 characters on one line"
            );
            let (send, receive) = mpsc::sync_channel(1);
            application
                .terminal(&session)?
                .send(Command::SetConversation {
                    pid: peer_pid(socket)?,
                    agent: Agent::Coding(agent.into()),
                    conversation,
                    reply: send,
                })?;
            ensure!(
                receive.recv_timeout(Duration::from_secs(3))?,
                "Conversations must come from a process in this shell session"
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
            application
                .terminal(&session)?
                .send(Command::Input(STANDARD.decode(data)?))?;
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
            application.end_session(&session)?;
            Ok(Response::Ok)
        }
        Request::RecentArtifacts { sessions, limit } => Ok(Response::RecentArtifacts {
            artifacts: application
                .recent_artifacts(&sessions, limit.min(500))?
                .into_iter()
                .map(Into::into)
                .collect(),
        }),
        Request::Dag { dag } => {
            let dag = dag_adapter::request(&server.dag, dag)?;
            if let DagResponse::BoxIcon { box_id } = &dag {
                broadcast(&BOX_ICON_WATCHERS, || Response::BoxIconChanged {
                    box_id: box_id.clone(),
                });
            }
            Ok(Response::Dag { dag: Box::new(dag) })
        }
        Request::WatchArtifacts | Request::WatchBoxIcons => {
            unreachable!("Watches use the streaming handler")
        }
        Request::Subscribe { .. } => unreachable!("Subscriptions use the streaming handler"),
    }
}

fn peer_pid(socket: &UnixStream) -> Result<u32> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // Linux supplies the credentials of the process that connected to this socket.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(
        length as usize == std::mem::size_of::<libc::ucred>(),
        "Missing peer credentials"
    );
    let credentials = unsafe { credentials.assume_init() };
    Ok(credentials.pid.try_into()?)
}
