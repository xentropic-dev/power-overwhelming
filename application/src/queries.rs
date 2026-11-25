use crate::{Application, TerminalRuntime};
use anyhow::Result;
use domain::{
    artifact::{ResolvedArtifact, artifact_id},
    session::Session,
};
use std::path::{Path, PathBuf};

pub struct ResolveArtifact {
    pub session: String,
    pub target: String,
}

impl<T: TerminalRuntime> Application<T> {
    pub fn list_sessions(&self) -> Vec<Session> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|live| live.session.clone())
            .collect()
    }

    pub fn list_worktrees(&self, directory: &Path) -> Result<Vec<PathBuf>> {
        self.workspace.list_worktrees(directory)
    }

    pub fn resolve_artifact(&self, query: ResolveArtifact) -> Result<ResolvedArtifact> {
        let session = self.session(&query.session)?;
        if let Some(id) = artifact_id(&query.target) {
            return self.artifacts.find(id);
        }
        let target = self.resolve_target(&query.target, &session.worktree)?;
        Ok(ResolvedArtifact {
            title: target.title(),
            target,
        })
    }
}
