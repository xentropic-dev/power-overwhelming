pub use crate::wire::SessionMetadata;
use crate::wire::{self, Request, Response};
use anyhow::{Context, Result, bail};
use application::{PublishArtifact, RecentArtifact, ResolveArtifact, StartSession};
use base64::{Engine, engine::general_purpose::STANDARD};
use domain::{
    artifact::{LEGACY_URI_PREFIX, ResolvedArtifact, URI_PREFIX, artifact_id},
    session::{Agent, Geometry, Session},
};
use std::{
    fs::{DirBuilder, OpenOptions},
    io::BufReader,
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        net::UnixStream,
        process::CommandExt,
    },
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

pub fn list_sessions() -> Result<Vec<Session>> {
    match wire::request(&Request::List)? {
        Response::Sessions { sessions } => Ok(sessions.into_iter().map(Into::into).collect()),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn list_worktrees(directory: PathBuf) -> Result<Vec<PathBuf>> {
    match wire::request(&Request::Worktrees { directory })? {
        Response::Worktrees { worktrees } => Ok(worktrees),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn start_session(command: StartSession) -> Result<Session> {
    match wire::request(&Request::Launch {
        worktree: command.worktree,
        agent: command.agent.into(),
    })? {
        Response::Launched { session } => Ok(session.into()),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn working_directory(session: String) -> Result<Option<PathBuf>> {
    match wire::request(&Request::WorkingDirectory { session })? {
        Response::WorkingDirectory { directory } => Ok(directory),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn set_task(session: String, value: String) -> Result<()> {
    expect_ok(&Request::SetMetadata {
        session,
        field: wire::MetadataField::Task,
        value,
    })
}

pub fn set_model(session: String, value: String) -> Result<()> {
    expect_ok(&Request::SetMetadata {
        session,
        field: wire::MetadataField::Model,
        value,
    })
}

/// Reports the conversation an agent wrapper is about to start, so a restore can resume it.
pub fn set_conversation(session: String, agent: String, conversation: String) -> Result<()> {
    expect_ok(&Request::SetConversation {
        session,
        agent,
        conversation,
    })
}

pub fn end_session(session: String) -> Result<()> {
    expect_ok(&Request::End { session })
}

pub fn publish_artifact(command: PublishArtifact) -> Result<String> {
    match wire::request(&Request::Publish {
        session: command.session,
        cwd: command.cwd,
        target: command.target,
        title: command.title,
    })? {
        Response::Published { uri } => {
            let id = artifact_id(&uri).context("Daemon returned an invalid artifact URI")?;
            Ok(format!("{URI_PREFIX}{id}"))
        }
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn resolve_artifact(query: ResolveArtifact) -> Result<ResolvedArtifact> {
    // Terminals can remain connected to a daemon that only accepts this prefix.
    let target = artifact_id(&query.target)
        .map(|id| format!("{LEGACY_URI_PREFIX}{id}"))
        .unwrap_or(query.target);
    match wire::request(&Request::Resolve {
        session: query.session,
        target,
    })? {
        Response::Resolved { target, title } => Ok(ResolvedArtifact {
            target: target.into(),
            title,
        }),
        _ => bail!("Unexpected daemon response"),
    }
}

pub enum TerminalControl {
    Input(Vec<u8>),
    Resize(Geometry),
}

pub fn set_color_scheme(name: &str) -> Result<()> {
    expect_ok(&Request::SetColorScheme {
        name: name.to_owned(),
    })
}

pub fn control_terminal(session: &str, control: TerminalControl) -> Result<()> {
    let session = session.to_owned();
    let request = match control {
        TerminalControl::Input(bytes) => Request::Input {
            session,
            data: STANDARD.encode(bytes),
        },
        TerminalControl::Resize(geometry) => Request::Resize {
            session,
            geometry: geometry.into(),
        },
    };
    expect_ok(&request)
}

fn expect_ok(request: &Request) -> Result<()> {
    match wire::request(request)? {
        Response::Ok => Ok(()),
        _ => bail!("Unexpected daemon response"),
    }
}

pub enum TerminalEvent {
    Snapshot { data: Vec<u8>, exited: bool },
    Output(Vec<u8>),
    Geometry(Geometry),
    Exit,
    Agent(Agent),
    Metadata(SessionMetadata),
    Status(String),
}

/// Artifacts published from these sessions, newest first.
pub fn recent_artifacts(sessions: Vec<String>, limit: usize) -> Result<Vec<RecentArtifact>> {
    match wire::request(&Request::RecentArtifacts { sessions, limit })? {
        Response::RecentArtifacts { artifacts } => {
            Ok(artifacts.into_iter().map(Into::into).collect())
        }
        _ => bail!("Unexpected daemon response"),
    }
}

/// Delivers every artifact publish until `deliver` returns false, reconnecting as needed.
pub fn watch_artifacts(deliver: impl Fn(RecentArtifact) -> bool) {
    watch(&Request::WatchArtifacts, |response| match response {
        Response::ArtifactPublished { artifact } => Ok(deliver(artifact.into())),
        _ => bail!("Unexpected daemon response"),
    })
}

/// Delivers the box ID of every icon change until `deliver` returns false, reconnecting as
/// needed.
pub fn watch_box_icons(deliver: impl Fn(String) -> bool) {
    watch(&Request::WatchBoxIcons, |response| match response {
        Response::BoxIconChanged { box_id } => Ok(deliver(box_id)),
        _ => bail!("Unexpected daemon response"),
    })
}

/// Streams events for `request`, skipping keepalives, until `deliver` returns false.
fn watch(request: &Request, deliver: impl Fn(Response) -> Result<bool>) {
    loop {
        let result = (|| -> Result<bool> {
            let mut socket = UnixStream::connect(wire::socket_path())?;
            socket.set_read_timeout(Some(Duration::from_secs(15)))?;
            wire::send(&mut socket, request)?;
            let mut reader = BufReader::new(socket);
            loop {
                match wire::receive::<Response>(&mut reader)?.context("Daemon disconnected")? {
                    Response::Ok => {}
                    Response::Error { message } => bail!("{message}"),
                    response => {
                        if !deliver(response)? {
                            return Ok(false);
                        }
                    }
                }
            }
        })();
        if matches!(result, Ok(false)) {
            return;
        }
        thread::sleep(Duration::from_millis(700));
    }
}

pub fn follow_terminal(
    session: &str,
    is_closed: impl Fn() -> bool,
    deliver: impl Fn(TerminalEvent) -> bool,
) {
    while !is_closed() {
        match subscribe(session, &is_closed, &deliver) {
            Ok(()) => break,
            Err(error) => {
                if !deliver(TerminalEvent::Status(format!("Reconnecting… {error}"))) {
                    break;
                }
                thread::sleep(Duration::from_millis(700));
            }
        }
    }
}

fn subscribe(
    session: &str,
    is_closed: &impl Fn() -> bool,
    deliver: &impl Fn(TerminalEvent) -> bool,
) -> Result<()> {
    let mut socket = UnixStream::connect(wire::socket_path())?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    wire::send(
        &mut socket,
        &Request::Subscribe {
            session: session.to_owned(),
        },
    )?;
    let mut reader = BufReader::new(socket);
    let mut sequence = None;
    while !is_closed() {
        let response: Response = wire::receive(&mut reader)?.context("Daemon disconnected")?;
        match &response {
            Response::Snapshot {
                sequence: value, ..
            } => sequence = Some(*value),
            Response::Output {
                sequence: value, ..
            }
            | Response::Geometry {
                sequence: value, ..
            } => {
                if sequence.map(|previous| previous + 1) != Some(*value) {
                    bail!("Terminal output was interrupted");
                }
                sequence = Some(*value);
            }
            _ => {}
        }
        let event = match response {
            Response::Snapshot { data, exited, .. } => TerminalEvent::Snapshot {
                data: STANDARD.decode(data)?,
                exited,
            },
            Response::Output { data, .. } => TerminalEvent::Output(STANDARD.decode(data)?),
            Response::Geometry { geometry, .. } => TerminalEvent::Geometry(geometry.into()),
            Response::Exit => TerminalEvent::Exit,
            Response::Agent { agent } => TerminalEvent::Agent(agent.into()),
            Response::Metadata { metadata } => TerminalEvent::Metadata(metadata),
            Response::Error { message } => {
                deliver(TerminalEvent::Status(message));
                break;
            }
            Response::Ok => continue,
            _ => bail!("Unexpected daemon response"),
        };
        if !deliver(event) {
            break;
        }
    }
    Ok(())
}

pub fn ensure_daemon() -> Result<()> {
    if list_sessions().is_ok() {
        return Ok(());
    }
    let socket = wire::socket_path();
    if let Some(parent) = socket.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let log_path = socket.with_extension("log");
    let log = OpenOptions::new()
        .mode(0o600)
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // Detach the daemon from the original controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut daemon = command.spawn()?;
    for _ in 0..100 {
        if list_sessions().is_ok() {
            thread::spawn(move || {
                let _ = daemon.wait();
            });
            return Ok(());
        }
        let _ = daemon.try_wait();
        thread::sleep(Duration::from_millis(30));
    }
    bail!("Power daemon failed to start. See {}", log_path.display())
}

// ---------------------------------------------------------------------------
// DAG orchestration client. Typed calls return domain types; `protocol` exposes the wire DTOs
// for callers that speak the JSON contract directly, such as the CLI.
// ---------------------------------------------------------------------------

pub use crate::wire::WorktreeScope;
pub use domain::dag::{
    AgentState, Attempt, AttemptState, BoxAttention, BoxInfo, Coordinator, ManagedAgent, Provider,
    ReportStatus, Run, RunEvent, RunEventKind, RunStatus, Task, TaskSpec, TaskState,
};
pub use domain::worktree::{KeepReason, ManagedWorktree, PruneVerdict, WorktreeCreator};

/// The DAG wire contract, for callers that print or forward its JSON.
pub mod protocol {
    pub use crate::wire::{
        AgentState, Attempt, AttemptState, BoxAttention, BoxInfo, Coordinator, DagRequest,
        DagResponse, ManagedAgent, ManagedWorktree, Provider, ReportStatus, Run, RunEvent,
        RunEventKind, RunStatus, Task, TaskSpec, TaskState, WorktreeRequest, WorktreeScope,
    };
}

use protocol::{DagRequest, DagResponse};

/// Sends one DAG request. `timeout` bounds the reply, which matters for blocking waits.
pub fn dag(request: DagRequest, timeout: Duration) -> Result<DagResponse> {
    use std::io::Write as _;
    let mut socket = UnixStream::connect(wire::socket_path()).context("Connect to power daemon")?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    wire::send(&mut socket, &Request::Dag { dag: request })?;
    socket.flush()?;
    match wire::receive::<Response>(&mut BufReader::new(socket))?.context("Daemon disconnected")? {
        Response::Dag { dag } => Ok(*dag),
        Response::Error { message } => bail!("{message}"),
        _ => bail!("Unexpected daemon response"),
    }
}

const DAG_TIMEOUT: Duration = Duration::from_secs(10);

fn domain_list<T: Into<U>, U>(values: Vec<T>) -> Vec<U> {
    values.into_iter().map(Into::into).collect()
}

pub fn list_boxes() -> Result<Vec<BoxInfo>> {
    match dag(DagRequest::ListBoxes, DAG_TIMEOUT)? {
        DagResponse::Boxes { boxes } => Ok(domain_list(boxes)),
        _ => bail!("Unexpected daemon response"),
    }
}

/// Sets a box's icon from an absolute image path, or resets it when `path` is absent.
/// Returns the box ID.
pub fn set_box_icon(
    box_sel: Option<String>,
    caller: Option<String>,
    path: Option<PathBuf>,
) -> Result<String> {
    match dag(
        DagRequest::SetBoxIcon {
            box_sel,
            caller,
            path,
        },
        DAG_TIMEOUT,
    )? {
        DagResponse::BoxIcon { box_id } => Ok(box_id),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn list_agents(box_sel: Option<String>, caller: Option<String>) -> Result<Vec<ManagedAgent>> {
    match dag(DagRequest::ListAgents { box_sel, caller }, DAG_TIMEOUT)? {
        DagResponse::Agents { agents } => Ok(domain_list(agents)),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn list_runs(box_sel: Option<String>, caller: Option<String>) -> Result<Vec<Run>> {
    match dag(DagRequest::ListRuns { box_sel, caller }, DAG_TIMEOUT)? {
        DagResponse::Runs { runs } => Ok(domain_list(runs)),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn get_run(run: String) -> Result<Run> {
    match dag(DagRequest::GetRun { run }, DAG_TIMEOUT)? {
        DagResponse::Run { run } => Ok(run.into()),
        _ => bail!("Unexpected daemon response"),
    }
}

/// Creates a checkout based on the source view's HEAD and returns its path.
pub fn create_view_worktree(
    box_sel: String,
    from_session: String,
    branch: String,
) -> Result<PathBuf> {
    match dag(
        DagRequest::CreateViewWorktree {
            box_sel,
            from_session,
            branch,
        },
        DAG_TIMEOUT,
    )? {
        DagResponse::ViewWorktreeCreated { path } => Ok(path),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn list_managed_worktrees(scope: WorktreeScope) -> Result<Vec<ManagedWorktree>> {
    match dag(DagRequest::ListManagedWorktrees { scope }, DAG_TIMEOUT)? {
        DagResponse::ManagedWorktrees { worktrees } => Ok(domain_list(worktrees)),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn prune_worktrees(
    scope: WorktreeScope,
    dry_run: bool,
    delete_branch: bool,
) -> Result<Vec<(ManagedWorktree, PruneVerdict)>> {
    match dag(
        DagRequest::PruneWorktrees {
            scope,
            dry_run,
            delete_branch,
        },
        DAG_TIMEOUT,
    )? {
        DagResponse::WorktreesPruned { entries } => Ok(entries
            .into_iter()
            .map(|entry| (entry.worktree.into(), entry.verdict.into()))
            .collect()),
        _ => bail!("Unexpected daemon response"),
    }
}

pub fn attention() -> Result<Vec<BoxAttention>> {
    match dag(DagRequest::Attention, DAG_TIMEOUT)? {
        DagResponse::Attention { boxes } => Ok(domain_list(boxes)),
        _ => bail!("Unexpected daemon response"),
    }
}

/// Streams run events until `deliver` returns false or the daemon goes away.
pub fn watch_runs(box_sel: Option<String>, deliver: impl Fn(RunEvent) -> bool) -> Result<()> {
    let mut socket = UnixStream::connect(wire::socket_path()).context("Connect to power daemon")?;
    // The daemon sends keepalives every few seconds, so a long silence means it is gone.
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    wire::send(
        &mut socket,
        &Request::Dag {
            dag: DagRequest::Watch { box_sel },
        },
    )?;
    let mut reader = BufReader::new(socket);
    while let Some(response) = wire::receive::<Response>(&mut reader)? {
        match response {
            Response::Dag { dag } => match *dag {
                DagResponse::Event { event } => {
                    if !deliver(event.into()) {
                        break;
                    }
                }
                other => bail!("unexpected DAG response: {other:?}"),
            },
            Response::Ok => continue,
            Response::Error { message } => bail!("{message}"),
            _ => bail!("Unexpected daemon response"),
        }
    }
    Ok(())
}
