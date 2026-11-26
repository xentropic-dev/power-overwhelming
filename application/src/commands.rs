use crate::{Application, Live, TerminalRuntime};
use anyhow::{Context, Result, ensure};
use domain::{
    artifact::{ArtifactRecord, ResolvedArtifact, URI_PREFIX},
    session::{Agent, Session},
};
use std::{ffi::OsString, path::PathBuf};

pub struct StartSession {
    pub worktree: PathBuf,
    pub agent: Agent,
}

#[derive(Default)]
pub struct SessionOptions {
    /// Extra coding-agent arguments, or the full argv for a shell command.
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, Option<OsString>)>,
    pub initial_prompt: Option<OsString>,
}

pub struct PublishArtifact {
    pub session: String,
    pub cwd: PathBuf,
    pub target: String,
    pub title: Option<String>,
}

pub struct ArtifactPublished {
    pub session: String,
    pub uri: String,
    pub title: String,
}

impl<T: TerminalRuntime> Application<T> {
    pub fn start_session(&self, command: StartSession) -> Result<Session> {
        self.start_session_with(command, SessionOptions::default())
    }

    pub fn start_session_with(
        &self,
        command: StartSession,
        options: SessionOptions,
    ) -> Result<Session> {
        let profile = match &command.agent {
            Agent::Shell => None,
            Agent::Coding(id) => Some(self.agents.resolve(id)?),
        };
        // Prune holds this lock from its live-session check through removal.
        let mut sessions = self.sessions.lock().unwrap();
        let session = Session {
            id: (self.new_id)(),
            worktree: self.workspace.open_worktree(&command.worktree)?,
            agent: command.agent,
        };
        let terminal = self.runtime.start(&session, options, profile)?;
        sessions.insert(
            session.id.clone(),
            Live {
                session: session.clone(),
                terminal,
            },
        );
        Ok(session)
    }

    pub fn end_session(&self, id: &str) -> Result<()> {
        let live = self
            .sessions
            .lock()
            .unwrap()
            .remove(id)
            .context("Terminal session is unavailable")?;
        self.runtime.end(live.terminal)
    }

    pub fn publish_artifact(&self, command: PublishArtifact) -> Result<ArtifactPublished> {
        let session = self.session(&command.session)?;
        ensure!(
            command.cwd.is_absolute(),
            "Publishing requires an absolute working directory"
        );
        let target = self.resolve_target(&command.target, &command.cwd)?;
        let id = (self.new_id)();
        let uri = format!("{URI_PREFIX}{id}");
        let title = command.title.unwrap_or_else(|| target.title());
        self.artifacts.insert(ArtifactRecord {
            id,
            session: session.id,
            worktree: session.worktree,
            artifact: ResolvedArtifact {
                title: title.clone(),
                target,
            },
        })?;
        Ok(ArtifactPublished {
            session: command.session,
            uri,
            title,
        })
    }
}
