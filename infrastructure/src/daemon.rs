use crate::{
    agents::AgentProfiles,
    box_icons::IconFiles,
    boxes::BoxesFile,
    dag_adapter::{self, DaemonDag},
    dag_store::SqliteDagStore,
    persistence::SqliteArtifacts,
    plugins::Plugins,
    terminal::{ActiveScheme, Command, PtyRuntime},
    theme,
    wire::{self, DagRequest, DagResponse, Request, Response},
    workspace::LocalWorkspace,
};
use anyhow::{Context, Result, bail, ensure};
use application::{Application, PublishArtifact, ResolveArtifact, StartSession};
use base64::{Engine, engine::general_purpose::STANDARD};
use domain::session::Geometry;
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
    sync::{Arc, OnceLock, RwLock, Weak, mpsc},
    thread,
    time::Duration,
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

pub fn serve(profiles: AgentProfiles) -> Result<()> {
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
    let store = SqliteDagStore::open().context("Open DAG store")?;
    let exit_handler = Arc::new(OnceLock::<Weak<DaemonDag>>::new());
    let terminal_exit_handler = exit_handler.clone();
    let scheme = Arc::new(RwLock::new(theme::default_scheme()));
    let profiles = Arc::new(profiles);
    let application = Arc::new(Application::new(
        artifacts,
        LocalWorkspace,
        PtyRuntime {
            socket: socket.clone(),
            scheme: scheme.clone(),
            profiles: profiles.clone(),
            on_exit: Arc::new(move |session, detail| {
                if let Some(dag) = terminal_exit_handler.get().and_then(Weak::upgrade)
                    && let Err(error) = dag.session_exited(session, detail)
                {
                    eprintln!("Could not record exit for terminal {session}. {error:#}");
                }
            }),
        },
        profiles,
        || uuid::Uuid::new_v4().to_string(),
    ));
    let dag = Arc::new(DaemonDag::new(
        application.clone(),
        store,
        BoxesFile,
        IconFiles::open()?,
    ));
    dag.reconcile().context("Reconcile DAG sessions")?;
    exit_handler
        .set(Arc::downgrade(&dag))
        .expect("install terminal exit handler once");
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
                value.chars().count() <= 120 && !value.chars().any(char::is_control),
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
                receive.recv_timeout(Duration::from_secs(3))?,
                "Session metadata must come from the active agent"
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
            Ok(Response::Dag { dag })
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
