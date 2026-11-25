use anyhow::{Context, Result, bail};
use domain::{artifact, session};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Shell,
    Codex,
    Claude,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Session {
    pub id: String,
    pub worktree: PathBuf,
    pub agent: Agent,
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
    Worktrees {
        directory: PathBuf,
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
    End {
        session: String,
    },
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
    Published {
        uri: String,
    },
    Resolved {
        target: Target,
        title: String,
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

pub fn request(message: &Request) -> Result<Response> {
    let mut socket = UnixStream::connect(socket_path()).context("Connect to power daemon")?;
    let timeout = Some(std::time::Duration::from_secs(5));
    socket.set_read_timeout(timeout)?;
    socket.set_write_timeout(timeout)?;
    send(&mut socket, message)?;
    match receive::<Response>(&mut BufReader::new(socket))?.context("Daemon disconnected")? {
        Response::Error { message } => bail!("{message}"),
        response => Ok(response),
    }
}

impl From<Agent> for session::Agent {
    fn from(value: Agent) -> Self {
        match value {
            Agent::Shell => Self::Shell,
            Agent::Codex => Self::Codex,
            Agent::Claude => Self::Claude,
        }
    }
}

impl From<session::Agent> for Agent {
    fn from(value: session::Agent) -> Self {
        match value {
            session::Agent::Shell => Self::Shell,
            session::Agent::Codex => Self::Codex,
            session::Agent::Claude => Self::Claude,
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
