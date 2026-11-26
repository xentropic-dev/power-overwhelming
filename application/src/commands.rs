use crate::{Application, Live, SessionRecord, TerminalRuntime};
use anyhow::{Context, Result, ensure};
use domain::{
    artifact::{ArtifactRecord, ResolvedArtifact, URI_PREFIX},
    session::{Agent, Session},
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

pub struct StartSession {
    pub worktree: PathBuf,
    pub agent: Agent,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionOptions {
    /// Extra coding-agent arguments, or the full argv for a shell command.
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, Option<OsString>)>,
    /// Sent once at launch and never persisted, so a restore cannot replay it.
    pub initial_prompt: Option<OsString>,
    /// Initial task label for a dispatched worker.
    pub initial_task: Option<String>,
    /// Starting directory. Defaults to the worktree.
    pub cwd: Option<PathBuf>,
    /// Agent a restored shell runs once its integration has loaded.
    pub resume: Option<ResumeCommand>,
    /// Set when `args` resume a conversation. If the agent exits right after
    /// launch, the resume failed and the runtime relaunches it once with these
    /// arguments, which start a new conversation.
    pub fallback_args: Option<Vec<OsString>>,
}

/// An agent wrapper call that a restored shell evaluates through `POWER_RESUME`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeCommand {
    /// Shell function name, which is the agent profile ID.
    pub program: String,
    pub args: Vec<OsString>,
    /// Arguments for a second call that starts a new conversation when the resume
    /// exits right away. `None` runs no second call.
    pub fallback: Option<Vec<OsString>>,
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
        // Prune checks live sessions and removes worktrees while holding this lock.
        let mut sessions = self.sessions.lock().unwrap();
        let session = Session {
            id: (self.new_id)(),
            worktree: self.workspace.open_worktree(&command.worktree)?,
            agent: command.agent,
        };
        // Save first so the runtime can record a conversation while it launches.
        self.records.save(&SessionRecord {
            id: session.id.clone(),
            worktree: session.worktree.clone(),
            agent: session.agent.clone(),
            args: options.args.clone(),
            env: options.env.clone(),
            foreground: session.agent.clone(),
            conversation: None,
            cwd: None,
        })?;
        let id = session.id.clone();
        self.launch_into(&mut sessions, session, options)
            .inspect_err(|_| {
                let _ = self.records.remove(&id);
            })
    }

    /// Starts a terminal under the session's existing ID without touching its record.
    pub(crate) fn launch(&self, session: Session, options: SessionOptions) -> Result<Session> {
        self.launch_into(&mut self.sessions.lock().unwrap(), session, options)
    }

    fn launch_into(
        &self,
        sessions: &mut std::collections::BTreeMap<String, Live<T::Handle>>,
        session: Session,
        options: SessionOptions,
    ) -> Result<Session> {
        let profile = match &session.agent {
            Agent::Shell => None,
            Agent::Coding(id) => Some(self.agents.resolve(id)?),
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

    /// Ends a session and forgets it, so it is not restored after a restart. The
    /// session ends even when forgetting its record fails, and a retry forgets the
    /// record of a session whose terminal is already gone.
    pub fn end_session(&self, id: &str) -> Result<()> {
        let live = self.sessions.lock().unwrap().remove(id);
        let ended = live
            .context("Terminal session is unavailable")
            .and_then(|live| self.runtime.end(live.terminal));
        let forgotten = self
            .records
            .remove(id)
            .context("Could not forget the ended session");
        // A record that could not be forgotten is what a retry must fix.
        forgotten.and(ended)
    }

    /// Saves the foreground state a restore resumes from.
    pub fn record_foreground(
        &self,
        id: &str,
        foreground: &Agent,
        conversation: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<()> {
        self.records
            .update_foreground(id, foreground, conversation, cwd)
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
