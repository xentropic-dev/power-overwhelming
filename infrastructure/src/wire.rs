use anyhow::{Context, Result, bail};
use domain::{artifact, dag, session, worktree};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

pub mod overview;
pub mod talk;
pub use talk::{TalkRequest, TalkResponse};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Shell,
    #[serde(untagged)]
    Coding(String),
}

/// A published artifact in history or in a publish event.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Artifact {
    pub session: String,
    pub uri: String,
    pub title: String,
}

impl From<application::RecentArtifact> for Artifact {
    fn from(value: application::RecentArtifact) -> Self {
        Self {
            session: value.session,
            uri: value.uri,
            title: value.title,
        }
    }
}

impl From<Artifact> for application::RecentArtifact {
    fn from(value: Artifact) -> Self {
        Self {
            session: value.session,
            uri: value.uri,
            title: value.title,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Session {
    pub id: String,
    pub worktree: PathBuf,
    pub agent: Agent,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct SessionMetadata {
    pub task: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataField {
    Task,
    Model,
    /// A Claude Code rate-limit report, kept by the daemon per model rather than on the
    /// session. See `usage::encode_usage`.
    Usage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Target {
    Image(PathBuf),
    Video(PathBuf),
    Html(PathBuf),
    Loopback(String),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    List,
    SetColorScheme {
        name: String,
    },
    Worktrees {
        directory: PathBuf,
    },
    WorkingDirectory {
        session: String,
    },
    SetMetadata {
        session: String,
        field: MetadataField,
        value: String,
    },
    /// Sent by an agent wrapper in a shell session before it starts the agent, with the
    /// conversation when the wrapper knows it.
    SetConversation {
        session: String,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
    },
    Launch {
        worktree: PathBuf,
        agent: Agent,
    },
    Subscribe {
        session: String,
    },
    Input {
        session: String,
        data: String,
    },
    Resize {
        session: String,
        geometry: Geometry,
    },
    Publish {
        session: String,
        cwd: PathBuf,
        target: String,
        title: Option<String>,
    },
    Resolve {
        session: String,
        target: String,
    },
    /// Artifacts published from these sessions, newest first.
    RecentArtifacts {
        sessions: Vec<String>,
        limit: usize,
    },
    /// Stream an `artifact_published` event for every publish.
    WatchArtifacts,
    /// Stream a `box_icon_changed` event whenever a box's icon is set or reset.
    WatchBoxIcons,
    End {
        session: String,
    },
    Talk {
        talk: TalkRequest,
    },
    Dag {
        dag: DagRequest,
    },
    /// Every box, live run, model in use and the machine's load, for the dashboard.
    Overview,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error {
        message: String,
    },
    Sessions {
        sessions: Vec<Session>,
    },
    Worktrees {
        worktrees: Vec<PathBuf>,
    },
    WorkingDirectory {
        directory: Option<PathBuf>,
    },
    Launched {
        session: Session,
    },
    Snapshot {
        sequence: u64,
        data: String,
        exited: bool,
    },
    Output {
        sequence: u64,
        data: String,
    },
    Geometry {
        sequence: u64,
        geometry: Geometry,
    },
    Exit,
    Agent {
        agent: Agent,
    },
    Metadata {
        metadata: SessionMetadata,
    },
    Published {
        uri: String,
    },
    RecentArtifacts {
        artifacts: Vec<Artifact>,
    },
    ArtifactPublished {
        artifact: Artifact,
    },
    BoxIconChanged {
        box_id: String,
    },
    Resolved {
        target: Target,
        title: String,
    },
    Talk {
        talk: TalkResponse,
    },
    Dag {
        dag: Box<DagResponse>,
    },
    Overview {
        overview: Box<overview::Overview>,
    },
}

pub fn send(mut stream: impl Write, message: &impl Serialize) -> Result<()> {
    let mut data = serde_json::to_vec(message)?;
    data.push(b'\n');
    stream.write_all(&data)?;
    Ok(())
}

pub fn receive<T: DeserializeOwned>(stream: &mut impl BufRead) -> Result<Option<T>> {
    let mut data = Vec::new();
    if stream.read_until(b'\n', &mut data)? == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&data)?))
}

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("POWER_SOCKET") {
        return path.into();
    }
    let uid = unsafe { libc::geteuid() };
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/power-overwhelming-{uid}")))
        .join("power-overwhelming.sock")
}

/// Connects to the daemon. A loaded daemon can fill its listen backlog, so `EAGAIN` and
/// `ECONNREFUSED` while the socket exists are retried for a few seconds. Nothing has been
/// sent yet, so retrying cannot apply a request twice.
pub fn connect() -> Result<UnixStream> {
    connect_to(&socket_path(), std::time::Duration::from_secs(10))
}

fn connect_to(path: &std::path::Path, patience: std::time::Duration) -> Result<UnixStream> {
    let deadline = std::time::Instant::now() + patience;
    let mut delay = std::time::Duration::from_millis(20);
    loop {
        match UnixStream::connect(path) {
            Ok(socket) => return Ok(socket),
            Err(error) if is_busy(&error, path) && std::time::Instant::now() + delay < deadline => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(500));
            }
            Err(error) => return Err(error).context("Connect to power daemon"),
        }
    }
}

fn peer_credentials(socket: &UnixStream) -> Result<libc::ucred> {
    use std::os::fd::AsRawFd as _;
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // Linux supplies the credentials of the process that connected to, or listened on,
    // this socket.
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
    anyhow::ensure!(
        length as usize == std::mem::size_of::<libc::ucred>(),
        "Missing peer credentials"
    );
    Ok(unsafe { credentials.assume_init() })
}

/// The PID of the process on the other end of a connected socket. Fails when the peer
/// has no PID in this namespace: Linux then reports 0, which `kill` would read as the
/// caller's own process group.
pub(crate) fn peer_pid(socket: &UnixStream) -> Result<u32> {
    let pid = peer_credentials(socket)?.pid;
    u32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .with_context(|| {
            format!("The peer's PID is unknown here (got {pid}), so it cannot be identified")
        })
}

/// A daemon process, held by a pidfd so a signal reaches only it: once it exits, its
/// PID may be reused, but the pidfd never refers to another process.
pub struct DaemonProcess {
    pub pid: u32,
    /// When it started, in clock ticks since boot.
    started: u64,
    pidfd: std::os::fd::OwnedFd,
}

impl DaemonProcess {
    /// The process listening at the other end of `socket`.
    fn of_peer(socket: &UnixStream) -> Result<Self> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        let pid = peer_pid(socket)?;
        // SO_PEERPIDFD (Linux 6.5) names the peer itself. Older kernels open the PID,
        // which is checked below against the peer's credentials and start time.
        let mut raw: libc::c_int = -1;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let peer = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut raw as *mut libc::c_int).cast(),
                &mut length,
            )
        };
        let started = start_time(pid).context("Read the daemon's start time")?;
        let pidfd = if peer == 0 && raw >= 0 {
            unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
        } else {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("Open the daemon process (pid {pid})"));
            }
            unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as libc::c_int) }
        };
        let process = Self {
            pid,
            started,
            pidfd,
        };
        anyhow::ensure!(
            peer_pid(socket)? == pid && process.is_same(),
            "The daemon (pid {pid}) changed while it was being identified"
        );
        Ok(process)
    }

    /// Whether the PID still names the process this was opened for.
    pub fn is_same(&self) -> bool {
        self.alive() && start_time(self.pid).ok() == Some(self.started)
    }

    /// Whether it has not exited.
    pub fn alive(&self) -> bool {
        use std::os::fd::AsRawFd as _;
        let mut poll = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // A pidfd turns readable once its process exits.
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        ready == 0
    }

    /// Sends `signal` to this process only. `Ok(false)` when it had already exited.
    pub fn signal(&self, signal: libc::c_int) -> Result<bool> {
        use std::os::fd::AsRawFd as _;
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        Err(error).with_context(|| format!("Signal the daemon (pid {})", self.pid))
    }

    /// Asks it to exit with SIGTERM, then sends SIGKILL if it still runs after `grace`.
    /// Signals nothing once it has exited.
    pub fn stop(&self, grace: std::time::Duration) -> Result<()> {
        anyhow::ensure!(
            self.is_same(),
            "The daemon (pid {}) is no longer the one that was checked",
            self.pid
        );
        if !self.signal(libc::SIGTERM)? {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            if !self.alive() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.signal(libc::SIGKILL)?;
        Ok(())
    }
}

/// When `pid` started, in clock ticks since boot.
fn start_time(pid: u32) -> Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, tail) = stat.rsplit_once(')').context("Malformed stat")?;
    // proc_pid_stat(5): starttime is the 20th field after the name.
    tail.split_whitespace()
        .nth(19)
        .context("Malformed stat")?
        .parse()
        .context("Malformed stat")
}

/// What connecting to the daemon socket found.
pub enum Probe {
    /// Nothing listens: the socket file is missing, or refuses connections.
    Absent,
    /// A daemon accepted, and this is its process.
    Listening(UnixStream, DaemonProcess),
}

/// Connects to the daemon at `path` once. Only a definite "nothing listens" is
/// `Absent`. Any other failure, such as a permission error, is an error.
pub fn probe(path: &std::path::Path) -> Result<Probe> {
    use std::io::ErrorKind;
    match UnixStream::connect(path) {
        Ok(socket) => {
            let process = DaemonProcess::of_peer(&socket).context("Identify the daemon")?;
            Ok(Probe::Listening(socket, process))
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(Probe::Absent)
        }
        Err(error) => Err(error).with_context(|| format!("Connect to {}", path.display())),
    }
}

fn is_busy(error: &std::io::Error, path: &std::path::Path) -> bool {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::WouldBlock | ErrorKind::Interrupted => true,
        ErrorKind::ConnectionRefused => path.exists(),
        _ => false,
    }
}

/// Reads the daemon's reply to a request already sent. A timeout here means the daemon may
/// still apply the request, so it is reported as unknown rather than as a plain OS error.
pub fn reply<T: DeserializeOwned>(stream: &mut impl BufRead) -> Result<T> {
    match receive(stream) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => bail!("Daemon disconnected before replying; the request may have been applied"),
        Err(error) if is_timeout(&error) => bail!(
            "The daemon did not reply in time and may still apply the request; check its state before retrying"
        ),
        Err(error) => Err(error),
    }
}

fn is_timeout(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    })
}

pub fn request(message: &Request) -> Result<Response> {
    let mut socket = connect()?;
    let timeout = Some(std::time::Duration::from_secs(30));
    socket.set_read_timeout(timeout)?;
    socket.set_write_timeout(timeout)?;
    send(&mut socket, message)?;
    match reply::<Response>(&mut BufReader::new(socket))? {
        Response::Error { message } => bail!("{message}"),
        response => Ok(response),
    }
}

impl From<Agent> for session::Agent {
    fn from(value: Agent) -> Self {
        match value {
            Agent::Shell => Self::Shell,
            Agent::Coding(id) => Self::Coding(id.into()),
        }
    }
}

impl From<session::Agent> for Agent {
    fn from(value: session::Agent) -> Self {
        match value {
            session::Agent::Shell => Self::Shell,
            session::Agent::Coding(id) => Self::Coding(id.into()),
        }
    }
}

impl From<Session> for session::Session {
    fn from(value: Session) -> Self {
        Self {
            id: value.id,
            worktree: value.worktree,
            agent: value.agent.into(),
        }
    }
}

impl From<session::Session> for Session {
    fn from(value: session::Session) -> Self {
        Self {
            id: value.id,
            worktree: value.worktree,
            agent: value.agent.into(),
        }
    }
}

impl From<Geometry> for session::Geometry {
    fn from(value: Geometry) -> Self {
        Self {
            cols: value.cols,
            rows: value.rows,
            cell_width: value.cell_width,
            cell_height: value.cell_height,
        }
    }
}

impl From<session::Geometry> for Geometry {
    fn from(value: session::Geometry) -> Self {
        Self {
            cols: value.cols,
            rows: value.rows,
            cell_width: value.cell_width,
            cell_height: value.cell_height,
        }
    }
}

impl From<Target> for artifact::Target {
    fn from(value: Target) -> Self {
        match value {
            Target::Image(value) => Self::Image(value),
            Target::Video(value) => Self::Video(value),
            Target::Html(value) => Self::Html(value),
            Target::Loopback(value) => Self::Loopback(value),
        }
    }
}

impl From<artifact::Target> for Target {
    fn from(value: artifact::Target) -> Self {
        match value {
            artifact::Target::Image(value) => Self::Image(value),
            artifact::Target::Video(value) => Self::Video(value),
            artifact::Target::Html(value) => Self::Html(value),
            artifact::Target::Loopback(value) => Self::Loopback(value),
        }
    }
}

// ---------------------------------------------------------------------------
// DAG orchestration contract. Boxes, managed agents, runs, tasks and attempts.
// ---------------------------------------------------------------------------

/// A saved box as the daemon sees it in the box registry.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct BoxInfo {
    pub id: String,
    /// Folder name of the repository, for display and `--box` selection.
    pub name: String,
    pub repository: PathBuf,
    pub checkout: PathBuf,
    pub shortcut: Option<u8>,
    /// False when the box is detached from the strip.
    pub open: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Runs the given argv directly. Used for deterministic worker fixtures.
    Command,
    #[serde(untagged)]
    Agent(String),
}

impl From<String> for Provider {
    fn from(id: String) -> Self {
        if id == "command" {
            Self::Command
        } else {
            Self::Agent(id)
        }
    }
}

impl From<dag::Provider> for Provider {
    fn from(value: dag::Provider) -> Self {
        match value {
            dag::Provider::Command => Self::Command,
            dag::Provider::Agent(id) => Self::Agent(id.into()),
        }
    }
}

impl From<Provider> for dag::Provider {
    fn from(value: Provider) -> Self {
        match value {
            Provider::Command => Self::Command,
            Provider::Agent(id) => Self::Agent(id.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Running,
    Exited,
}

/// What an agent is doing, derived by the daemon from its state and its attempt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Working,
    Paused,
    NeedsInput,
    Done,
    Failed,
    /// Also assumed when an older daemon omits the status.
    #[default]
    Idle,
    Exited,
}

/// A Claude Code, Codex, or fixture process managed by Power. Tab attachment is tracked by the
/// desktop, independently of this record.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ManagedAgent {
    pub id: String,
    pub box_id: String,
    pub worktree: PathBuf,
    pub provider: Provider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The delegating Power agent, if any.
    pub parent: Option<String>,
    /// The daemon terminal session, while it has one.
    pub session: Option<String>,
    pub run: Option<String>,
    pub task: Option<String>,
    pub attempt: Option<String>,
    pub state: AgentState,
    /// Whether its processes are paused.
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub status: AgentStatus,
    /// Short label, the task title for DAG workers.
    pub label: String,
    /// Unix seconds.
    pub created_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Coordinator {
    /// A Power agent, such as a managed Claude Code or Codex session in a box.
    Agent { agent: String },
    /// A coordinator outside Power, such as a Herdr pane. The label is opaque.
    External { label: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Some prerequisite has not completed successfully.
    Waiting,
    Ready,
    /// Dispatched while the worker budget was full. Power starts it when capacity frees up.
    Queued,
    Working,
    /// Its attempt is open and its worker is paused.
    Paused,
    NeedsInput,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Active,
    NeedsInput,
    Succeeded,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Attempt {
    pub id: String,
    /// 1 for the first dispatch of a task.
    pub number: u32,
    pub agent: String,
    pub session: Option<String>,
    pub worktree: PathBuf,
    pub provider: Provider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub state: AttemptState,
    /// Result summary from the worker's final report.
    pub summary: Option<String>,
    /// Outstanding request for input, while state is `needs_input`.
    pub blocker: Option<String>,
    /// Daemon-supplied context, such as why an attempt was interrupted.
    pub detail: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    /// Whether the attempt's worker is paused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    /// Whether the attempt's worker looks stalled: idle, with no build or test running.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stalled: bool,
    /// Artifacts published from the attempt's session, in publish order, newest last.
    #[serde(default)]
    pub artifacts: Vec<AttemptArtifact>,
}

/// An artifact published from an attempt's session.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct AttemptArtifact {
    pub id: String,
    pub title: String,
    /// `pwr://artifact/<id>`; derived from `id` when decoding.
    pub link: String,
}

/// A task as the coordinator defines it.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct TaskSpec {
    /// Stable ID, unique within the run, matching `[a-z0-9][a-z0-9_-]{0,63}`.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub deps: Vec<String>,
    /// Planned provider, used when a dispatch does not pass `--provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    /// Planned model for the planned provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub deps: Vec<String>,
    /// Planned provider, chosen before dispatch. Attempts record what actually ran.
    #[serde(default)]
    pub provider: Option<Provider>,
    /// Planned model, chosen before dispatch.
    #[serde(default)]
    pub model: Option<String>,
    /// Derived by the daemon from persisted attempts and prerequisite states.
    pub state: TaskState,
    /// Unix seconds, while a dispatch of the task waits for the worker budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<u64>,
    pub current_attempt: Option<String>,
    /// Oldest first.
    pub attempts: Vec<Attempt>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Run {
    pub id: String,
    pub box_id: String,
    pub title: String,
    pub coordinator: Coordinator,
    pub created_at: u64,
    /// Unix seconds, while the run is archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<u64>,
    /// Unix seconds, while the run is paused and dispatches nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<u64>,
    /// Derived by the daemon from the task states, or `archived` while archived.
    #[serde(default)]
    pub status: RunStatus,
    /// In insertion order.
    pub tasks: Vec<Task>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Active,
    Completed,
    Stalled,
    Archived,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    Success,
    Failed,
    NeedsInput,
    /// Clears an outstanding input request and continues the attempt.
    Working,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    RunCreated,
    TaskAdded,
    TaskReady,
    /// The planned provider or model changed. The summary is the new plan.
    TaskPlanned,
    AttemptStarted,
    AgentModelChanged,
    AttemptSucceeded,
    AttemptFailed,
    NeedsInput,
    InputResolved,
    /// The worker exited without a result, or its session could not be recovered.
    AttemptInterrupted,
    /// A daemon restart resumed the worker's session, and the worker was told to continue.
    AttemptResumed,
    /// The worker has been idle past the stall threshold with nothing it started working.
    AttemptStalled,
    /// A report from an attempt that is no longer current. Recorded, not applied.
    StaleReport,
    RunArchived,
    RunUnarchived,
    /// The run's workers were paused, and it dispatches nothing until it resumes.
    RunPaused,
    RunResumed,
    /// A dispatch waits for the worker budget. The summary says which limit held it.
    TaskQueued,
    /// A queued dispatch could not be started. The summary has the error.
    QueuedDispatchFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct RunEvent {
    /// Daemon-wide, strictly increasing, persisted.
    pub seq: u64,
    pub run: String,
    pub box_id: String,
    pub kind: RunEventKind,
    pub task: Option<String>,
    pub attempt: Option<String>,
    /// Result summary, blocker text, or interruption context.
    pub summary: Option<String>,
    pub at: u64,
}

/// A box with at least one outstanding request for input.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct BoxAttention {
    pub box_id: String,
    /// Sessions of workers that need input, for tab highlights.
    pub sessions: Vec<String>,
    /// Agents that need input, including those without a session view.
    pub agents: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeRequest {
    New {
        branch: String,
        base: Option<String>,
    },
    RetryManaged,
    /// Existing paths retain the original string representation on the wire.
    #[serde(untagged)]
    Existing(PathBuf),
}

impl WorktreeRequest {
    /// `allow_main_checkout` applies only to an existing path.
    pub fn into_domain(self, allow_main_checkout: bool) -> worktree::WorktreeRequest {
        match self {
            Self::New { branch, base } => worktree::WorktreeRequest::New { branch, base },
            Self::RetryManaged => worktree::WorktreeRequest::RetryManaged,
            Self::Existing(path) => worktree::WorktreeRequest::Existing {
                path,
                allow_main_checkout,
            },
        }
    }
}

/// What a dispatch does when the worker budget is full.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMode {
    #[default]
    Queue,
    Refuse,
    Override,
}

impl From<BudgetMode> for application::dag::BudgetMode {
    fn from(value: BudgetMode) -> Self {
        match value {
            BudgetMode::Queue => Self::Queue,
            BudgetMode::Refuse => Self::Refuse,
            BudgetMode::Override => Self::Override,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeScope {
    All,
    Box { selector: String },
    View { session: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ManagedWorktree {
    pub box_id: String,
    pub repository: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub created_at: u64,
    pub creator: WorktreeCreator,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeCreator {
    Attempt {
        run: String,
        task: String,
        attempt: String,
    },
    View {
        box_id: String,
        session: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WorktreePruneEntry {
    pub worktree: ManagedWorktree,
    pub verdict: PruneVerdict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum PruneVerdict {
    Remove,
    Keep(KeepReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepReason {
    Dirty,
    Unmerged,
    OpenAttempt,
    InUse,
    NoLongerManaged,
}

impl From<worktree::ManagedWorktree> for ManagedWorktree {
    fn from(value: worktree::ManagedWorktree) -> Self {
        Self {
            box_id: value.box_id,
            repository: value.repository,
            path: value.path,
            branch: value.branch,
            base_commit: value.base_commit,
            created_at: value.created_at,
            creator: match value.creator {
                worktree::WorktreeCreator::Attempt { run, task, attempt } => {
                    WorktreeCreator::Attempt { run, task, attempt }
                }
                worktree::WorktreeCreator::View { box_id, session } => {
                    WorktreeCreator::View { box_id, session }
                }
            },
        }
    }
}

impl From<ManagedWorktree> for worktree::ManagedWorktree {
    fn from(value: ManagedWorktree) -> Self {
        Self {
            box_id: value.box_id,
            repository: value.repository,
            path: value.path,
            branch: value.branch,
            base_commit: value.base_commit,
            created_at: value.created_at,
            creator: match value.creator {
                WorktreeCreator::Attempt { run, task, attempt } => {
                    worktree::WorktreeCreator::Attempt { run, task, attempt }
                }
                WorktreeCreator::View { box_id, session } => {
                    worktree::WorktreeCreator::View { box_id, session }
                }
            },
        }
    }
}

impl From<worktree::PruneVerdict> for PruneVerdict {
    fn from(value: worktree::PruneVerdict) -> Self {
        match value {
            worktree::PruneVerdict::Remove => Self::Remove,
            worktree::PruneVerdict::Keep(reason) => Self::Keep(match reason {
                worktree::KeepReason::Dirty => KeepReason::Dirty,
                worktree::KeepReason::Unmerged => KeepReason::Unmerged,
                worktree::KeepReason::OpenAttempt => KeepReason::OpenAttempt,
                worktree::KeepReason::InUse => KeepReason::InUse,
                worktree::KeepReason::NoLongerManaged => KeepReason::NoLongerManaged,
            }),
        }
    }
}

impl From<PruneVerdict> for worktree::PruneVerdict {
    fn from(value: PruneVerdict) -> Self {
        match value {
            PruneVerdict::Remove => Self::Remove,
            PruneVerdict::Keep(reason) => Self::Keep(match reason {
                KeepReason::Dirty => worktree::KeepReason::Dirty,
                KeepReason::Unmerged => worktree::KeepReason::Unmerged,
                KeepReason::OpenAttempt => worktree::KeepReason::OpenAttempt,
                KeepReason::InUse => worktree::KeepReason::InUse,
                KeepReason::NoLongerManaged => worktree::KeepReason::NoLongerManaged,
            }),
        }
    }
}

/// DAG requests. They travel inside `Request::Dag`, so existing clients are unaffected.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DagRequest {
    ListBoxes,
    /// `box_sel` accepts a box ID, a unique ID prefix, a shortcut digit, or a unique repo name.
    /// `caller` is the caller's POWER_SESSION and selects its box when `box_sel` is absent.
    /// Finished agents are left out unless `include_finished`.
    ListAgents {
        box_sel: Option<String>,
        caller: Option<String>,
        #[serde(default)]
        include_finished: bool,
    },
    StartAgent {
        box_sel: Option<String>,
        caller: Option<String>,
        provider: Provider,
        worktree: WorktreeRequest,
        /// Lets an existing `worktree` be the repository's main checkout.
        #[serde(default)]
        allow_main_checkout: bool,
        parent: Option<String>,
        label: String,
        prompt: Option<String>,
        #[serde(default)]
        args: Vec<String>,
    },
    CreateRun {
        box_sel: Option<String>,
        caller: Option<String>,
        title: String,
        /// External coordinator label. When absent, `caller` must be a Power session and
        /// becomes the coordinating agent.
        coordinator: Option<String>,
    },
    /// Finished runs are left out unless `include_finished`.
    ListRuns {
        box_sel: Option<String>,
        caller: Option<String>,
        #[serde(default)]
        include_finished: bool,
    },
    GetRun {
        run: String,
    },
    /// Hides the run from default listings. Refused while it has open attempts, unless
    /// `force`.
    ArchiveRun {
        run: String,
        #[serde(default)]
        force: bool,
    },
    UnarchiveRun {
        run: String,
    },
    /// Pauses every running worker of the run and stops dispatching from it.
    PauseRun {
        run: String,
    },
    /// Resumes the run's paused workers and lets it dispatch again.
    ResumeRun {
        run: String,
    },
    /// Pauses or resumes one agent's whole process tree.
    SetAgentPaused {
        agent: String,
        paused: bool,
    },
    /// Ends an agent's own terminal session, interrupting its open attempt.
    StopAgent {
        agent: String,
    },
    /// Pauses every running agent except `keep` and the caller's own session.
    PauseAllExcept {
        keep: Vec<String>,
        caller: Option<String>,
    },
    /// Adds tasks atomically. References may point at tasks in this batch or already in the
    /// run. Unknown references, duplicate IDs, and cycles reject the whole batch.
    AddTasks {
        run: String,
        tasks: Vec<TaskSpec>,
    },
    AddDependency {
        run: String,
        task: String,
        dep: String,
    },
    /// Changes a task's planned provider and model. `clear` drops the plan first. A new
    /// `provider` without `model` drops the planned model. An empty `model` clears it.
    PlanTask {
        run: String,
        task: String,
        provider: Option<Provider>,
        model: Option<String>,
        #[serde(default)]
        clear: bool,
    },
    Dispatch {
        run: String,
        task: String,
        /// Absent means the task's planned provider and model.
        #[serde(default)]
        provider: Option<Provider>,
        worktree: WorktreeRequest,
        /// Lets an existing `worktree` be the repository's main checkout.
        #[serde(default)]
        allow_main_checkout: bool,
        /// Required to start a new attempt after a failed or interrupted one.
        #[serde(default)]
        retry: bool,
        /// Extra provider arguments, or the argv for `Provider::Command`.
        #[serde(default)]
        args: Vec<String>,
        caller: Option<String>,
        /// What to do when the worker budget is full. Queues by default.
        #[serde(default)]
        budget: BudgetMode,
    },
    /// Starts a queued task now, over the worker budget, as it was dispatched.
    StartQueued {
        run: String,
        task: String,
    },
    CreateViewWorktree {
        box_sel: String,
        from_session: String,
        branch: String,
        /// Where to check out the branch. Defaults to the box's folder under the worktree root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    ListManagedWorktrees {
        scope: WorktreeScope,
    },
    PruneWorktrees {
        scope: WorktreeScope,
        dry_run: bool,
        delete_branch: bool,
    },
    Report {
        run: String,
        task: String,
        attempt: String,
        status: ReportStatus,
        summary: String,
    },
    /// Events with `seq > since`. Blocks up to `wait_ms` when none match yet.
    Events {
        run: String,
        since: u64,
        #[serde(default)]
        wait_ms: u64,
        /// Empty means every kind.
        #[serde(default)]
        kinds: Vec<RunEventKind>,
    },
    Attention,
    /// Sets the box's icon from an image at an absolute path, or restores the identicon
    /// when `path` is absent.
    SetBoxIcon {
        box_sel: Option<String>,
        caller: Option<String>,
        path: Option<PathBuf>,
    },
    /// Streams `DagResponse::Event` for every run event in the selected box, or all boxes.
    Watch {
        box_sel: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DagResponse {
    Boxes {
        boxes: Vec<BoxInfo>,
    },
    Agents {
        agents: Vec<ManagedAgent>,
    },
    Agent {
        agent: ManagedAgent,
    },
    Run {
        run: Run,
    },
    Runs {
        runs: Vec<Run>,
    },
    Dispatched {
        attempt: Attempt,
        agent: Box<ManagedAgent>,
        /// Present for a newly created or resumed managed checkout.
        branch: Option<String>,
    },
    /// The worker budget is full. The task is queued and starts when capacity frees up.
    Queued {
        run: String,
        task: String,
        /// The limit that holds the dispatch.
        reason: String,
    },
    ViewWorktreeCreated {
        path: PathBuf,
    },
    ManagedWorktrees {
        worktrees: Vec<ManagedWorktree>,
    },
    WorktreesPruned {
        entries: Vec<WorktreePruneEntry>,
    },
    Reported {
        applied: bool,
        reason: Option<String>,
    },
    Events {
        events: Vec<RunEvent>,
        cursor: u64,
    },
    Attention {
        boxes: Vec<BoxAttention>,
    },
    Event {
        event: RunEvent,
    },
    BoxIcon {
        box_id: String,
    },
}

// Domain entities do not depend on serde. These conversions keep the transport
// fields, enum spellings, and optional values independent of the use cases.
mod dag_dto {
    use super::*;
    use domain::dag;

    macro_rules! enum_dto {
        ($name:ident { $($variant:ident),+ $(,)? }) => {
            impl From<dag::$name> for $name {
                fn from(value: dag::$name) -> Self {
                    match value { $(dag::$name::$variant => Self::$variant),+ }
                }
            }
            impl From<$name> for dag::$name {
                fn from(value: $name) -> Self {
                    match value { $($name::$variant => Self::$variant),+ }
                }
            }
        };
        ($name:ident { $($variant:ident { $field:ident }),+ $(,)? }) => {
            impl From<dag::$name> for $name {
                fn from(value: dag::$name) -> Self {
                    match value { $(dag::$name::$variant { $field } => Self::$variant { $field }),+ }
                }
            }
            impl From<$name> for dag::$name {
                fn from(value: $name) -> Self {
                    match value { $($name::$variant { $field } => Self::$variant { $field }),+ }
                }
            }
        };
    }

    macro_rules! struct_dto {
        (
            $name:ident { $($field:ident),+ $(,)? }
            $(options { $($option:ident),+ $(,)? })?
            $(lists { $($list:ident),+ $(,)? })?
        ) => {
            impl From<dag::$name> for $name {
                fn from(value: dag::$name) -> Self {
                    Self {
                        $($field: value.$field.into(),)+
                        $($($option: value.$option.map(Into::into),)+)?
                        $($($list: value.$list.into_iter().map(Into::into).collect(),)+)?
                    }
                }
            }
            impl From<$name> for dag::$name {
                fn from(value: $name) -> Self {
                    Self {
                        $($field: value.$field.into(),)+
                        $($($option: value.$option.map(Into::into),)+)?
                        $($($list: value.$list.into_iter().map(Into::into).collect(),)+)?
                    }
                }
            }
        };
    }

    enum_dto!(AgentState { Running, Exited });
    enum_dto!(AgentStatus {
        Working,
        Paused,
        NeedsInput,
        Done,
        Failed,
        Idle,
        Exited
    });
    enum_dto!(Coordinator { Agent { agent }, External { label } });
    enum_dto!(TaskState {
        Waiting,
        Ready,
        Queued,
        Working,
        Paused,
        NeedsInput,
        Completed,
        Failed,
        Interrupted
    });
    enum_dto!(AttemptState {
        Active,
        NeedsInput,
        Succeeded,
        Failed,
        Interrupted
    });
    enum_dto!(ReportStatus {
        Success,
        Failed,
        NeedsInput,
        Working
    });
    enum_dto!(RunEventKind {
        RunCreated,
        TaskAdded,
        TaskReady,
        TaskPlanned,
        AttemptStarted,
        AgentModelChanged,
        AttemptSucceeded,
        AttemptFailed,
        NeedsInput,
        InputResolved,
        AttemptInterrupted,
        AttemptResumed,
        AttemptStalled,
        StaleReport,
        RunArchived,
        RunUnarchived,
        RunPaused,
        RunResumed,
        TaskQueued,
        QueuedDispatchFailed,
    });

    struct_dto!(BoxInfo {
        id,
        name,
        repository,
        checkout,
        shortcut,
        open
    });
    struct_dto!(ManagedAgent {
        id,
        box_id,
        worktree,
        provider,
        model,
        parent,
        session,
        run,
        task,
        attempt,
        state,
        paused,
        status,
        label,
        created_at,
    });
    struct_dto!(Attempt {
        id,
        number,
        agent,
        session,
        worktree,
        provider,
        model,
        state,
        summary,
        blocker,
        detail,
        started_at,
        ended_at,
        paused,
        stalled,
    } lists { artifacts });
    impl From<dag::AttemptArtifact> for AttemptArtifact {
        fn from(value: dag::AttemptArtifact) -> Self {
            Self {
                link: value.link(),
                id: value.id,
                title: value.title,
            }
        }
    }
    impl From<AttemptArtifact> for dag::AttemptArtifact {
        fn from(value: AttemptArtifact) -> Self {
            Self {
                id: value.id,
                title: value.title,
            }
        }
    }
    struct_dto!(TaskSpec {
        id,
        title,
        instructions,
        deps,
        model
    } options { provider });
    struct_dto!(
        Task { id, title, instructions, deps, model, state, queued_at, current_attempt }
        options { provider }
        lists { attempts }
    );
    enum_dto!(RunStatus {
        Active,
        Completed,
        Stalled,
        Archived
    });
    impl From<dag::Run> for Run {
        fn from(value: dag::Run) -> Self {
            Self {
                status: value.status().into(),
                id: value.id,
                box_id: value.box_id,
                title: value.title,
                coordinator: value.coordinator.into(),
                created_at: value.created_at,
                archived_at: value.archived_at,
                paused_at: value.paused_at,
                tasks: value.tasks.into_iter().map(Into::into).collect(),
            }
        }
    }
    /// The status is derived, so the domain recomputes it from the task states.
    impl From<Run> for dag::Run {
        fn from(value: Run) -> Self {
            Self {
                id: value.id,
                box_id: value.box_id,
                title: value.title,
                coordinator: value.coordinator.into(),
                created_at: value.created_at,
                archived_at: value.archived_at,
                paused_at: value.paused_at,
                tasks: value.tasks.into_iter().map(Into::into).collect(),
            }
        }
    }
    struct_dto!(RunEvent {
        seq,
        run,
        box_id,
        kind,
        task,
        attempt,
        summary,
        at
    });
    struct_dto!(BoxAttention {
        box_id,
        sessions,
        agents
    });
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_conversation_round_trips() -> Result<()> {
        let request = Request::SetConversation {
            session: "session".into(),
            agent: "claude".into(),
            conversation: Some("conversation".into()),
        };
        let mut line = Vec::new();
        send(&mut line, &request)?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&line)?,
            json!({
                "request": "set_conversation",
                "session": "session",
                "agent": "claude",
                "conversation": "conversation",
            })
        );
        let decoded = receive::<Request>(&mut line.as_slice())?;
        assert!(matches!(
            decoded,
            Some(Request::SetConversation { session, agent, conversation })
                if session == "session" && agent == "claude"
                    && conversation.as_deref() == Some("conversation")
        ));
        // An agent that cannot pin its conversation reports only its start.
        let decoded = receive::<Request>(
            &mut br#"{"request":"set_conversation","session":"s","agent":"codex"}"#.as_slice(),
        )?;
        assert!(matches!(
            decoded,
            Some(Request::SetConversation {
                conversation: None,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn reads_existing_sessions_and_new_profile_ids_as_strings() -> Result<()> {
        for (name, expected) in [
            ("shell", session::Agent::Shell),
            ("codex", session::Agent::Coding("codex".into())),
            ("claude", session::Agent::Coding("claude".into())),
            ("custom", session::Agent::Coding("custom".into())),
        ] {
            let record = json!({"id": "session", "worktree": "/repo", "agent": name});
            let session: session::Session =
                serde_json::from_value::<Session>(record.clone())?.into();
            assert_eq!(session.agent, expected);
            assert_eq!(serde_json::to_value(Session::from(session))?, record);
        }
        Ok(())
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;
    use std::{os::unix::net::UnixListener, time::Duration};

    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        (dir, path)
    }

    #[test]
    fn connect_waits_for_a_socket_that_starts_listening() {
        let (_dir, path) = scratch();
        // A bound but not yet listening socket refuses connections, like a busy daemon.
        std::fs::write(&path, b"").unwrap();
        let later = path.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            std::fs::remove_file(&later).unwrap();
            let listener = UnixListener::bind(&later).unwrap();
            listener.accept().unwrap();
        });
        connect_to(&path, Duration::from_secs(5)).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn a_daemon_process_signals_only_while_it_is_the_one_identified() {
        let (_dir, path) = scratch();
        // A child listens, so the peer is a process this test may signal.
        let mut child = std::process::Command::new("python3")
            .args([
                "-c",
                "import socket,sys,time;s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);\
                 s.listen();time.sleep(60)",
            ])
            .arg(&path)
            .spawn()
            .unwrap();
        while !path.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        let Probe::Listening(_socket, process) = probe(&path).unwrap() else {
            panic!("nothing listened")
        };
        assert_eq!(process.pid, child.id());
        assert!(process.is_same());
        process.stop(Duration::from_secs(5)).unwrap();
        child.wait().unwrap();
        // It has exited and been reaped, so its PID may name another process. Neither
        // signal nor stop reaches anything.
        assert!(!process.alive());
        assert!(!process.is_same());
        assert!(!process.signal(libc::SIGKILL).unwrap());
        assert!(process.stop(Duration::from_secs(1)).is_err());
    }

    #[test]
    fn connect_fails_fast_without_a_socket() {
        let (_dir, path) = scratch();
        let start = std::time::Instant::now();
        assert!(connect_to(&path, Duration::from_secs(5)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn reply_timeout_says_the_request_may_have_applied() {
        let (_dir, path) = scratch();
        let listener = UnixListener::bind(&path).unwrap();
        let socket = UnixStream::connect(&path).unwrap();
        let _held = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let error = reply::<Response>(&mut BufReader::new(socket)).unwrap_err();
        assert!(error.to_string().contains("may still apply"), "{error}");
    }
}
