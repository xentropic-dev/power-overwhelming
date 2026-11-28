use crate::{Application, TerminalRuntime};
use anyhow::Result;
use domain::{
    artifact::{ResolvedArtifact, artifact_id},
    session::{Session, SessionListing},
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
            .map(|live| self.current(live))
            .collect()
    }

    /// Every live session with its DAG attempt, read under one lock.
    pub fn list_session_listings(&self) -> Vec<SessionListing> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|live| SessionListing {
                session: self.current(live),
                attempt: live.attempt.clone(),
            })
            .collect()
    }

    pub fn recent_artifacts(
        &self,
        sessions: &[String],
        limit: usize,
    ) -> Result<Vec<crate::RecentArtifact>> {
        self.artifacts.recent(sessions, limit)
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
