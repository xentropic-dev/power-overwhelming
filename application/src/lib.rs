mod commands;
mod queries;

pub use commands::{ArtifactPublished, PublishArtifact, StartSession};
pub use queries::ResolveArtifact;

use anyhow::{Context, Result};
use domain::{
    artifact::{ArtifactRecord, ResolvedArtifact, Target, TargetLocation},
    session::Session,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub trait ArtifactStore: Send + Sync {
    fn insert(&self, record: ArtifactRecord) -> Result<()>;
    fn find(&self, id: &str) -> Result<ResolvedArtifact>;
}

pub trait Workspace: Send + Sync {
    fn open_worktree(&self, directory: &Path) -> Result<PathBuf>;
    fn list_worktrees(&self, directory: &Path) -> Result<Vec<PathBuf>>;
    fn open_artifact_file(&self, path: &Path) -> Result<PathBuf>;
}

pub trait TerminalRuntime: Send + Sync {
    type Handle: Clone + Send;

    fn start(&self, session: &Session) -> Result<Self::Handle>;
    fn end(&self, handle: Self::Handle) -> Result<()>;
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
    new_id: fn() -> String,
}

impl<T: TerminalRuntime> Application<T> {
    pub fn new(
        artifacts: impl ArtifactStore + 'static,
        workspace: impl Workspace + 'static,
        runtime: T,
        new_id: fn() -> String,
    ) -> Self {
        Self {
            sessions: Mutex::default(),
            artifacts: Box::new(artifacts),
            workspace: Box::new(workspace),
            runtime,
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

    fn session(&self, id: &str) -> Result<Session> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|live| live.session.clone())
            .context("Terminal session is unavailable")
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
