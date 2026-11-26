pub mod agents;
mod commands;
pub mod dag;
mod queries;
pub mod skill_install;
pub mod skills;

pub use commands::{ArtifactPublished, PublishArtifact, SessionOptions, StartSession};
pub use queries::ResolveArtifact;

use anyhow::{Context, Result};
use domain::{
    agent::AgentProfile,
    artifact::{ArtifactRecord, ResolvedArtifact, Target, TargetLocation},
    session::{Agent, Session},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub trait ArtifactStore: Send + Sync {
    fn insert(&self, record: ArtifactRecord) -> Result<()>;
    fn find(&self, id: &str) -> Result<ResolvedArtifact>;
    /// Artifacts published from these sessions, newest first.
    fn recent(&self, sessions: &[String], limit: usize) -> Result<Vec<RecentArtifact>>;
}

/// A published artifact as listed in history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecentArtifact {
    pub session: String,
    pub uri: String,
    pub title: String,
}

pub trait Workspace: Send + Sync {
    fn open_worktree(&self, directory: &Path) -> Result<PathBuf>;
    fn list_worktrees(&self, directory: &Path) -> Result<Vec<PathBuf>>;
    fn validate_box_worktree(&self, repository: &Path, worktree: &Path) -> Result<PathBuf>;
    fn open_artifact_file(&self, path: &Path) -> Result<PathBuf>;
}

pub trait TerminalRuntime: Send + Sync {
    type Handle: Clone + Send;

    fn start(
        &self,
        session: &Session,
        options: SessionOptions,
        profile: Option<&AgentProfile>,
    ) -> Result<Self::Handle>;
    fn is_running(&self, handle: &Self::Handle) -> bool;
    fn end(&self, handle: Self::Handle) -> Result<()>;
    /// The agent currently in the foreground, which can differ from the launched one
    /// when a shell starts an agent.
    fn agent(&self, handle: &Self::Handle) -> Option<Agent>;
}

struct Live<H> {
    session: Session,
    terminal: H,
}

pub struct Application<T: TerminalRuntime> {
    sessions: Mutex<BTreeMap<String, Live<T::Handle>>>,
    artifacts: Box<dyn ArtifactStore>,
    workspace: Box<dyn Workspace>,
    runtime: T,
    agents: Arc<dyn agents::AgentProfileRegistry>,
    new_id: fn() -> String,
}

impl<T: TerminalRuntime> Application<T> {
    pub fn new(
        artifacts: impl ArtifactStore + 'static,
        workspace: impl Workspace + 'static,
        runtime: T,
        agents: Arc<dyn agents::AgentProfileRegistry>,
        new_id: fn() -> String,
    ) -> Self {
        Self {
            sessions: Mutex::default(),
            artifacts: Box::new(artifacts),
            workspace: Box::new(workspace),
            runtime,
            agents,
            new_id,
        }
    }

    pub fn terminal(&self, id: &str) -> Result<T::Handle> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|live| live.terminal.clone())
            .context("Terminal session is unavailable")
    }

    fn session_is_running(&self, id: &str) -> Result<bool> {
        Ok(self.runtime.is_running(&self.terminal(id)?))
    }

    fn session(&self, id: &str) -> Result<Session> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|live| self.current(live))
            .context("Terminal session is unavailable")
    }

    fn current(&self, live: &Live<T::Handle>) -> Session {
        let mut session = live.session.clone();
        if let Some(agent) = self.runtime.agent(&live.terminal) {
            session.agent = agent;
        }
        session
    }

    fn resolve_target(&self, value: &str, cwd: &Path) -> Result<Target> {
        match TargetLocation::parse(value, cwd)? {
            TargetLocation::File(path) => Ok(Target::from_file(
                self.workspace.open_artifact_file(&path)?,
            )?),
            TargetLocation::Loopback(url) => Ok(Target::Loopback(url)),
        }
    }
}
