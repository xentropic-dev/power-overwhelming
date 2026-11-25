use crate::{
    persistence::SqliteArtifacts,
    plugins::Plugins,
    terminal::{Command, PtyRuntime},
    wire::{self, Request, Response},
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
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

struct Server {
    application: Arc<Application<PtyRuntime>>,
    plugins: Option<Plugins>,
}

pub fn serve() -> Result<()> {
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
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let application = Arc::new(Application::new(
        artifacts,
        LocalWorkspace,
        PtyRuntime(socket),
        || uuid::Uuid::new_v4().to_string(),
    ));
    let server = Arc::new(Server {
        plugins: Plugins::start(application.clone()),
        application,
    });
    for socket in listener.incoming() {
        let socket = socket?;
        let server = server.clone();
        thread::spawn(move || {
            let _ = handle(socket, &server.application, server.plugins.as_ref());
        });
    }
    Ok(())
}

fn handle(
    mut socket: UnixStream,
    application: &Application<PtyRuntime>,
    plugins: Option<&Plugins>,
) -> Result<()> {
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    let request = wire::receive(&mut BufReader::new(&socket))?.context("Missing request")?;
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

    let response =
        handle_request(request, application, plugins).unwrap_or_else(|error| Response::Error {
            message: format!("{error:#}"),
        });
    wire::send(socket, &response)
}

fn handle_request(
    request: Request,
    application: &Application<PtyRuntime>,
    plugins: Option<&Plugins>,
) -> Result<Response> {
    match request {
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
            let uri = if let Some(plugins) = plugins {
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
        Request::Subscribe { .. } => unreachable!("Subscriptions use the streaming handler"),
    }
}
