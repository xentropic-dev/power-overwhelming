pub mod agents;
mod commands;
pub mod dag;
mod queries;
mod restore;
mod scheduler;
mod sessions;
pub mod skill_install;
pub mod skills;

pub use commands::{
    ArtifactPublished, PublishArtifact, ResumeCommand, SessionOptions, StartSession,
};
pub use queries::ResolveArtifact;
pub use restore::{RestoreChoice, RestoredSession};
pub use scheduler::{Job, Scheduler};
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
    /// The canonical path of the repository's main checkout, if it has one.
    fn main_checkout(&self, repository: &Path) -> Result<Option<PathBuf>>;
    /// The full commit ID at the worktree's HEAD.
    fn head(&self, worktree: &Path) -> Result<String>;
    /// The full commit ID that a commit-ish such as a branch name names in the repository.
    fn resolve_commit(&self, repository: &Path, commitish: &str) -> Result<String>;
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
    /// Whether a worktree directory is still present on disk.
    fn worktree_exists(&self, path: &Path) -> bool;
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
    /// Ends whatever an earlier daemon left running for the session `id`, before it
    /// is restored or forgotten. An error means some of it may still run.
    fn reclaim(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    /// Rechecks the foreground process now, rather than trusting the last periodic check.
    /// None when the terminal cannot answer. Never call it while holding a store lock: the
    /// terminal may need that store to record the change it finds.
    fn resolve_agent(&self, handle: &Self::Handle) -> Option<Agent> {
        self.agent(handle)
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

    /// Ends whatever still runs of session `id` outside a live terminal, such as what
    /// an earlier daemon or an incomplete teardown left. An error means some of it may
    /// still run.
    pub fn reclaim(&self, id: &str) -> Result<()> {
        self.runtime.reclaim(id)
    }

    /// Forgets a session whose processes are gone, without ending anything.
    pub fn forget_session(&self, id: &str) -> Result<()> {
        self.sessions.lock().unwrap().remove(id);
        self.records.remove(id)
    }

    /// Drops session `id`'s terminal handle once its process has exited, keeping
    /// the durable record, so the session no longer looks live while what it left is
    /// reclaimed. A handle that still runs, such as a relaunch, stays.
    pub fn retire_terminal(&self, id: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions
            .get(id)
            .is_some_and(|live| !self.runtime.is_running(&live.terminal))
        {
            sessions.remove(id);
        }
    }

    /// Whether a live terminal runs session `id`.
    pub fn terminal_running(&self, id: &str) -> bool {
        self.session_is_running(id).unwrap_or(false)
    }

    pub fn terminal(&self, id: &str) -> Result<T::Handle> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|live| live.terminal.clone())
            .context("Terminal session is unavailable")
    }

    /// The agent of a running session: its launched coding agent, or for a shell session,
    /// whichever agent holds its foreground at this moment (see
    /// `TerminalRuntime::resolve_agent`). None if the session is unknown, has exited or its
    /// foreground cannot be resolved. Do not call it while holding a store lock.
    pub fn live_agent(&self, id: &str) -> Option<Agent> {
        let (launched, terminal) = {
            let sessions = self.sessions.lock().unwrap();
            let live = sessions.get(id)?;
            (live.session.agent.clone(), live.terminal.clone())
        };
        if !self.runtime.is_running(&terminal) {
            return None;
        }
        match launched {
            Agent::Coding(_) => Some(launched),
            Agent::Shell => self.runtime.resolve_agent(&terminal),
        }
    }

    /// The agent a session was launched with, regardless of its current foreground.
    pub fn launched_agent(&self, id: &str) -> Option<Agent> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|live| live.session.agent.clone())
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

pub mod talk;
