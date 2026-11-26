pub mod agents;
mod commands;
pub mod dag;
mod queries;
mod restore;
mod sessions;
pub mod skill_install;
pub mod skills;

pub use commands::{
    ArtifactPublished, Labels, PublishArtifact, ResumeCommand, SessionOptions, StartSession,
};
pub use queries::ResolveArtifact;
pub use sessions::{MemorySessions, SessionRecord, SessionStore};

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
    /// Every artifact published from these sessions, in publish order, newest last.
    fn published(&self, sessions: &[String]) -> Result<Vec<SessionArtifact>>;
}

/// A published artifact and the session it was published from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionArtifact {
    pub session: String,
    pub artifact: domain::dag::AttemptArtifact,
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
    /// The full commit ID at the worktree's HEAD.
    fn head(&self, worktree: &Path) -> Result<String>;
    /// Looks up a local branch and reports where it is checked out, if anywhere.
    fn branch_exists(&self, repository: &Path, branch: &str) -> Result<BranchStatus>;
    /// Creates a new branch and checks it out at `path`, starting at the given commit-ish.
    fn create_worktree(
        &self,
        repository: &Path,
        path: &Path,
        branch: &str,
        base: &str,
    ) -> Result<CreatedWorktree>;
    /// Confirms that this is still the checkout created by Power.
    fn owns_worktree(&self, _worktree: &domain::worktree::ManagedWorktree) -> bool {
        false
    }
    fn worktree_status(&self, path: &Path) -> Result<WorktreeStatus>;
    fn remove_worktree(&self, path: &Path, delete_branch: bool) -> Result<()>;
    fn open_artifact_file(&self, path: &Path) -> Result<PathBuf>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchStatus {
    Missing,
    Exists { checked_out_at: Option<PathBuf> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedWorktree {
    /// Canonical path to the new checkout.
    pub path: PathBuf,
    /// Full commit ID resolved from the requested base.
    pub base_commit: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorktreeStatus {
    /// Includes untracked files.
    pub dirty: bool,
    pub merged_into_default: bool,
}

pub trait WorktreeRoot: Send + Sync {
    /// Resolves the current configured root or the default data directory location.
    fn worktree_root(&self) -> Result<PathBuf>;
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
    /// Model reported or requested for the given foreground agent, when known.
    fn model(&self, _handle: &Self::Handle, _agent: &Agent) -> Option<String> {
        None
    }
}

struct Live<H> {
    session: Session,
    terminal: H,
}

pub struct Application<T: TerminalRuntime> {
    sessions: Mutex<BTreeMap<String, Live<T::Handle>>>,
    artifacts: Box<dyn ArtifactStore>,
    records: Arc<dyn SessionStore>,
    workspace: Box<dyn Workspace>,
    runtime: T,
    agents: Arc<dyn agents::AgentProfileRegistry>,
    new_id: fn() -> String,
}

impl<T: TerminalRuntime> Application<T> {
    pub fn new(
        artifacts: impl ArtifactStore + 'static,
        records: Arc<dyn SessionStore>,
        workspace: impl Workspace + 'static,
        runtime: T,
        agents: Arc<dyn agents::AgentProfileRegistry>,
        new_id: fn() -> String,
    ) -> Self {
        Self {
            sessions: Mutex::default(),
            artifacts: Box::new(artifacts),
            records,
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
