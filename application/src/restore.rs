use crate::{Application, ResumeCommand, SessionOptions, SessionRecord, TerminalRuntime};
use anyhow::Result;
use domain::{
    agent::Resume,
    session::{Agent, Session},
};
use std::ffi::OsString;

/// A session that `restore_sessions_with` relaunched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoredSession {
    pub id: String,
    /// Whether the resumed agent received a continuation prompt.
    pub prompted: bool,
}

/// What `restore_sessions_deferring` did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Restore {
    pub restored: Vec<RestoredSession>,
    /// Sessions left unrestored because what an earlier daemon left of them still runs.
    pub deferred: Vec<String>,
}

/// What the caller of `restore_sessions_with` wants for one saved session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreChoice {
    /// Relaunch it. A coding agent that resumes its conversation also receives this
    /// continuation prompt, passed the way a launch passes the initial prompt.
    Resume(Option<String>),
    /// Forget the record without relaunching. The reason is logged.
    Forget(&'static str),
}

/// What to do with one saved session on daemon start.
enum Plan {
    Launch(Box<SessionOptions>),
    /// Forget the record. The reason is logged.
    Drop(&'static str),
}

impl<T: TerminalRuntime> Application<T> {
    /// Recreates every saved session under its original ID and returns the restored IDs.
    /// One failing record does not stop the others. A failed record is kept so a later
    /// restart can retry it, or the user can close its tab to forget it.
    pub fn restore_sessions(&self) -> Result<Vec<String>> {
        Ok(self
            .restore_sessions_with(|_| RestoreChoice::Resume(None))?
            .into_iter()
            .map(|restored| restored.id)
            .collect())
    }

    /// Restores like `restore_sessions`, asking `choose` about each session ID first.
    /// A forgotten session is never relaunched. A fresh fallback start never receives
    /// the continuation prompt.
    pub fn restore_sessions_with(
        &self,
        choose: impl Fn(&str) -> RestoreChoice,
    ) -> Result<Vec<RestoredSession>> {
        Ok(self.restore_sessions_deferring(choose)?.restored)
    }

    /// Restores like `restore_sessions_with`, and also returns the sessions whose
    /// earlier processes could not all be ended. Those keep their records and are
    /// neither relaunched nor forgotten, so a replacement never runs beside them. A
    /// later `reclaim` that succeeds lets them go.
    pub fn restore_sessions_deferring(
        &self,
        choose: impl Fn(&str) -> RestoreChoice,
    ) -> Result<Restore> {
        let mut restored = Vec::new();
        let mut deferred = Vec::new();
        for record in self.records.list()? {
            // Anything an earlier daemon left of the session ends first, whether it
            // is relaunched or forgotten, so a finished worker's teardown that the
            // restart interrupted still completes.
            let reclaimed = self.runtime.reclaim(&record.id);
            if let Err(error) = &reclaimed {
                eprintln!(
                    "Could not end what an earlier daemon left of session {}. {error:#}",
                    record.id
                );
            }
            if reclaimed.is_err() {
                // The record is how a later start or retry finds what still runs.
                eprintln!(
                    "Keeping session {} unrestored until what still runs of it ends.",
                    record.id
                );
                deferred.push(record.id);
                continue;
            }
            let plan = match choose(&record.id) {
                RestoreChoice::Resume(continuation) => self.restore_plan(&record, continuation),
                RestoreChoice::Forget(reason) => Ok(Plan::Drop(reason)),
            };
            let options = match plan {
                Ok(Plan::Launch(options)) => options,
                Ok(Plan::Drop(reason)) => {
                    eprintln!("Not restoring session {}: {reason}.", record.id);
                    if let Err(error) = self.records.remove(&record.id) {
                        eprintln!("Could not forget session {}. {error:#}", record.id);
                    }
                    continue;
                }
                Err(error) => {
                    eprintln!("Could not restore session {}. {error:#}", record.id);
                    continue;
                }
            };
            let session = Session {
                id: record.id.clone(),
                worktree: record.worktree.clone(),
                agent: record.agent.clone(),
            };
            let prompted = options.initial_prompt.is_some();
            match self.launch(session, *options) {
                Ok(_) => restored.push(RestoredSession {
                    id: record.id,
                    prompted,
                }),
                Err(error) => eprintln!("Could not restore session {}. {error:#}", record.id),
            }
        }
        Ok(Restore { restored, deferred })
    }

    fn restore_plan(&self, record: &SessionRecord, continuation: Option<String>) -> Result<Plan> {
        if !record.worktree.is_dir() {
            return Ok(Plan::Drop("its worktree no longer exists"));
        }
        let mut options = SessionOptions {
            env: record.env.clone(),
            ..SessionOptions::default()
        };
        match &record.agent {
            Agent::Coding(id) => {
                let profile = self.agents.resolve(id)?;
                let mut args = record.args.clone();
                if let Some((resume, mut fresh)) =
                    restart_args(profile.resume.as_ref(), record.conversation.as_deref())
                {
                    // The saved conversation replaces any selector the launch passed,
                    // which would conflict with it or resume the wrong one, and the fresh
                    // start must not retry the selector that failed.
                    if let Some(spec) = &profile.resume {
                        args = spec.without_selectors(&args);
                    }
                    fresh.extend(args.iter().cloned());
                    options.args = resume;
                    options.fallback_args = Some(fresh);
                    options.initial_prompt = continuation.map(Into::into);
                }
                options.args.extend(args);
            }
            // Command fixtures are never re-run, since their argv can be anything.
            Agent::Shell if !record.args.is_empty() => {
                return Ok(Plan::Drop("it ran a command"));
            }
            Agent::Shell => {
                options.cwd = record.cwd.clone().filter(|cwd| cwd.is_dir());
                if let Agent::Coding(id) = &record.foreground {
                    match self.agents.resolve(id) {
                        Ok(profile) => {
                            let restart = restart_args(
                                profile.resume.as_ref(),
                                record.conversation.as_deref(),
                            );
                            let (args, fallback) = match restart {
                                Some((resume, fresh)) => (resume, Some(fresh)),
                                None => (Vec::new(), None),
                            };
                            options.resume = Some(ResumeCommand {
                                program: profile.id.as_str().to_owned(),
                                args,
                                conversation: record.conversation.clone(),
                                fallback,
                            })
                        }
                        Err(error) => eprintln!(
                            "Not resuming {} in session {}. {error:#}",
                            id.as_str(),
                            record.id
                        ),
                    }
                }
            }
        }
        // Labels belong to the conversation that reported them. Only a resume pinned to
        // that conversation's ID continues it. `--last`, `--continue`, no resume at all,
        // or a fresh start may open any other conversation, so the labels are dropped,
        // from the record too, so no later restore brings them back either.
        if self.pinned(record) {
            options.labels = record.labels.clone();
        } else if record.labels != crate::Labels::default()
            && let Err(error) = self
                .records
                .update_labels(&record.id, &crate::Labels::default())
        {
            eprintln!("Could not clear labels of session {}. {error:#}", record.id);
        }
        Ok(Plan::Launch(Box::new(options)))
    }
}

impl<R: TerminalRuntime> Application<R> {
    /// Whether restoring `record` resumes the very conversation that saved its labels.
    fn pinned(&self, record: &SessionRecord) -> bool {
        let agent = match &record.agent {
            Agent::Coding(id) => id,
            Agent::Shell => match &record.foreground {
                Agent::Coding(id) if record.args.is_empty() => id,
                _ => return false,
            },
        };
        record.conversation.is_some()
            && self
                .agents
                .resolve(agent)
                .ok()
                .and_then(|profile| profile.resume.as_ref())
                // Only an `{id}` argument selects the saved conversation itself.
                .is_some_and(|resume| {
                    resume.resume.iter().any(|arg| arg.contains("{id}"))
                        // A forked launch opened a new conversation. A record saved
                        // before forks were told apart names its parent instead.
                        && !resume.forks(&record.args)
                })
    }
}

/// Arguments that resume the conversation, and arguments that start a new one in
/// its place when the resume fails. `None` when the profile adds no resume arguments,
/// since then there is nothing to fall back from.
fn restart_args(
    resume: Option<&Resume>,
    conversation: Option<&str>,
) -> Option<(Vec<OsString>, Vec<OsString>)> {
    let resume = resume?;
    let args = resume.resume_args(conversation);
    (!args.is_empty()).then(|| (args, resume.fresh_args(conversation)))
}

#[cfg(test)]
mod tests {
    use super::RestoreChoice;
    use crate::{
        Application, ArtifactStore, BranchStatus, CreatedWorktree, Labels, MemorySessions,
        RecentArtifact, ResumeCommand, SessionOptions, SessionRecord, SessionStore, StartSession,
        TerminalRuntime, Workspace, WorktreeStatus, agents::AgentProfileRegistry,
    };
    use anyhow::{Result, bail};
    use domain::{
        agent::{AgentProfile, Injection, Resume},
        artifact::{ArtifactRecord, ResolvedArtifact},
        session::{Agent, Session},
    };
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    struct NoArtifacts;

    impl ArtifactStore for NoArtifacts {
        fn insert(&self, _: ArtifactRecord) -> Result<()> {
            bail!("unused")
        }
        fn find(&self, _: &str) -> Result<ResolvedArtifact> {
            bail!("unused")
        }
        fn recent(&self, _: &[String], _: usize) -> Result<Vec<RecentArtifact>> {
            bail!("unused")
        }
        fn published(&self, _: &[String]) -> Result<Vec<crate::SessionArtifact>> {
            bail!("unused")
        }
    }

    struct PassThrough;

    impl Workspace for PassThrough {
        fn open_worktree(&self, directory: &Path) -> Result<PathBuf> {
            Ok(directory.into())
        }
        fn list_worktrees(&self, _: &Path) -> Result<Vec<PathBuf>> {
            bail!("unused")
        }
        fn validate_box_worktree(&self, _: &Path, _: &Path) -> Result<PathBuf> {
            bail!("unused")
        }
        fn main_checkout(&self, _: &Path) -> Result<Option<PathBuf>> {
            bail!("unused")
        }
        fn head(&self, _: &Path) -> Result<String> {
            bail!("unused")
        }
        fn resolve_commit(&self, _: &Path, _: &str) -> Result<String> {
            bail!("unused")
        }
        fn branch_exists(&self, _: &Path, _: &str) -> Result<BranchStatus> {
            bail!("unused")
        }
        fn create_worktree(&self, _: &Path, _: &Path, _: &str, _: &str) -> Result<CreatedWorktree> {
            bail!("unused")
        }
        fn worktree_status(&self, _: &Path) -> Result<WorktreeStatus> {
            bail!("unused")
        }
        fn remove_worktree(&self, _: &Path, _: bool) -> Result<()> {
            bail!("unused")
        }
        fn worktree_exists(&self, _: &Path) -> bool {
            true
        }
        fn open_artifact_file(&self, _: &Path) -> Result<PathBuf> {
            bail!("unused")
        }
    }

    #[derive(Default)]
    struct Starts(Mutex<Vec<(Session, SessionOptions)>>);

    impl TerminalRuntime for Arc<Starts> {
        type Handle = ();

        fn start(
            &self,
            session: &Session,
            options: SessionOptions,
            _: Option<&AgentProfile>,
        ) -> Result<()> {
            if options.args.first().is_some_and(|arg| arg == "fail") {
                bail!("launch failed");
            }
            self.0.lock().unwrap().push((session.clone(), options));
            Ok(())
        }
        fn is_running(&self, _: &()) -> bool {
            true
        }
        fn end(&self, _: ()) -> Result<()> {
            Ok(())
        }
        fn agent(&self, _: &()) -> Option<Agent> {
            None
        }
    }

    struct Profiles(Vec<AgentProfile>);

    impl AgentProfileRegistry for Profiles {
        fn profiles(&self) -> &[AgentProfile] {
            &self.0
        }
    }

    fn profile(id: &str, resume: Option<Resume>) -> AgentProfile {
        AgentProfile {
            id: id.into(),
            command: id.into(),
            injection: Injection::None,
            skip_subcommands: Vec::new(),
            skip_if_args_contain: Vec::new(),
            model_callback: domain::agent::ModelCallback::None,
            turn_hook: domain::agent::TurnHook::None,
            skills_dir: None,
            resume,
            env: Vec::new(),
            conversation_env: None,
            server_socket_env: None,
        }
    }

    fn app(store: Arc<MemorySessions>) -> (Application<Arc<Starts>>, Arc<Starts>) {
        static ID: AtomicU64 = AtomicU64::new(0);
        let starts = Arc::new(Starts::default());
        let claude = Resume {
            assign: vec!["--session-id".into(), "{id}".into()],
            resume: vec!["--resume".into(), "{id}".into()],
            resume_last: vec!["--continue".into()],
            conflicts: vec!["--resume".into()],
            forks: vec!["--fork-session".into()],
        };
        let codex = Resume {
            resume_last: vec!["resume".into(), "--last".into()],
            ..Resume::default()
        };
        // A custom profile whose resume never names the conversation.
        let last = Resume {
            resume: vec!["resume".into(), "--last".into()],
            ..Resume::default()
        };
        let profiles = vec![
            profile("last", Some(last)),
            profile("claude", Some(claude)),
            profile("codex", Some(codex)),
            profile("plain", None),
        ];
        let application = Application::new(
            NoArtifacts,
            store,
            PassThrough,
            starts.clone(),
            Arc::new(Profiles(profiles)),
            || format!("new-{}", ID.fetch_add(1, Ordering::Relaxed)),
        );
        (application, starts)
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn record(id: &str, worktree: &Path, agent: Agent, args: &[&str]) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            worktree: worktree.into(),
            agent: agent.clone(),
            args: os(args),
            env: vec![("POWER_TASK".into(), Some("task".into()))],
            foreground: agent,
            conversation: None,
            cwd: None,
            labels: Labels::default(),
        }
    }

    fn coding(id: &str) -> Agent {
        Agent::Coding(id.into())
    }

    #[test]
    fn start_saves_launch_options_without_the_prompt_and_end_forgets_them() -> Result<()> {
        let store = Arc::new(MemorySessions::default());
        let (application, _) = app(store.clone());
        let session = application.start_session_with(
            StartSession {
                worktree: "/repo".into(),
                agent: coding("claude"),
            },
            SessionOptions {
                args: os(&["--model", "opus"]),
                env: vec![("POWER_TASK".into(), None)],
                initial_prompt: Some("do the thing".into()),
                ..SessionOptions::default()
            },
        )?;
        let saved = store.list()?;
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, session.id);
        assert_eq!(saved[0].args, os(&["--model", "opus"]));
        assert_eq!(saved[0].env, vec![("POWER_TASK".into(), None)]);
        assert!(
            !format!("{saved:?}").contains("do the thing"),
            "the initial prompt must never be persisted"
        );

        application.end_session(&session.id)?;
        assert!(store.list()?.is_empty());
        Ok(())
    }

    #[test]
    fn failed_start_forgets_its_record() {
        let store = Arc::new(MemorySessions::default());
        let (application, _) = app(store.clone());
        let started = application.start_session_with(
            StartSession {
                worktree: "/repo".into(),
                agent: coding("claude"),
            },
            SessionOptions {
                args: os(&["fail"]),
                ..SessionOptions::default()
            },
        );
        assert!(started.is_err());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn restore_relaunches_sessions_under_their_ids_with_resume_args() -> Result<()> {
        let worktree = tempfile::tempdir()?;
        let subdir = worktree.path().join("sub");
        std::fs::create_dir(&subdir)?;
        let root = worktree.path();
        let store = Arc::new(MemorySessions::default());
        let mut claude = record("claude-tab", root, coding("claude"), &["--model", "opus"]);
        claude.conversation = Some("conv-1".into());
        let codex = record("codex-tab", root, coding("codex"), &[]);
        let plain = record("plain-tab", root, coding("plain"), &["--flag"]);
        let mut shell = record("shell-tab", root, Agent::Shell, &[]);
        shell.foreground = coding("claude");
        shell.conversation = Some("conv-2".into());
        shell.cwd = Some(subdir.clone());
        let mut moved = record("moved-tab", root, Agent::Shell, &[]);
        moved.cwd = Some(root.join("gone"));
        let gone = record("gone-tab", &root.join("missing"), coding("claude"), &[]);
        let command = record("command-tab", root, Agent::Shell, &["sh", "-c", "rm -rf /"]);
        let broken = record("broken-tab", root, coding("plain"), &["fail"]);
        for record in [
            &claude, &codex, &plain, &shell, &moved, &gone, &command, &broken,
        ] {
            store.save(record)?;
        }
        let (application, starts) = app(store.clone());

        let restored = application.restore_sessions()?;

        assert_eq!(
            restored,
            [
                "claude-tab",
                "codex-tab",
                "plain-tab",
                "shell-tab",
                "moved-tab"
            ]
        );
        let starts = starts.0.lock().unwrap();
        let launched: Vec<_> = starts
            .iter()
            .map(|(session, options)| (session.id.as_str(), options))
            .collect();
        let env = vec![("POWER_TASK".into(), Some("task".into()))];
        assert_eq!(
            launched,
            [
                (
                    "claude-tab",
                    &SessionOptions {
                        args: os(&["--resume", "conv-1", "--model", "opus"]),
                        env: env.clone(),
                        // No transcript exists under the ID if the resume fails.
                        fallback_args: Some(os(&["--session-id", "conv-1", "--model", "opus"])),
                        ..SessionOptions::default()
                    }
                ),
                (
                    "codex-tab",
                    &SessionOptions {
                        args: os(&["resume", "--last"]),
                        env: env.clone(),
                        // Codex cannot pin an ID, so it starts with no resume arguments.
                        fallback_args: Some(Vec::new()),
                        ..SessionOptions::default()
                    }
                ),
                (
                    "plain-tab",
                    &SessionOptions {
                        args: os(&["--flag"]),
                        env: env.clone(),
                        ..SessionOptions::default()
                    }
                ),
                (
                    "shell-tab",
                    &SessionOptions {
                        env: env.clone(),
                        cwd: Some(subdir),
                        resume: Some(ResumeCommand {
                            program: "claude".into(),
                            args: os(&["--resume", "conv-2"]),
                            conversation: Some("conv-2".into()),
                            fallback: Some(os(&["--session-id", "conv-2"])),
                        }),
                        ..SessionOptions::default()
                    }
                ),
                (
                    "moved-tab",
                    &SessionOptions {
                        env,
                        ..SessionOptions::default()
                    }
                ),
            ]
        );
        assert!(
            starts
                .iter()
                .all(|(session, options)| session.worktree == root
                    && options.initial_prompt.is_none())
        );
        let kept: Vec<_> = store.list()?.into_iter().map(|record| record.id).collect();
        assert_eq!(
            kept,
            [
                "claude-tab",
                "codex-tab",
                "plain-tab",
                "shell-tab",
                "moved-tab",
                "broken-tab"
            ]
        );
        assert!(application.terminal("shell-tab").is_ok());
        assert_eq!(store.list()?[3].args, Vec::<OsString>::new());
        Ok(())
    }

    #[test]
    fn restore_replaces_launch_selectors_with_the_saved_conversation() -> Result<()> {
        let worktree = tempfile::tempdir()?;
        let store = Arc::new(MemorySessions::default());
        let mut worker = record(
            "worker",
            worktree.path(),
            coding("claude"),
            &["--resume", "old", "--model", "opus", "--resume=older"],
        );
        worker.conversation = Some("reported".into());
        store.save(&worker)?;
        let (application, starts) = app(store);

        application.restore_sessions_with(|_| RestoreChoice::Resume(Some("continue".into())))?;

        let starts = starts.0.lock().unwrap();
        let (_, options) = &starts[0];
        // One selector, never two, and the fresh start does not retry the old one.
        assert_eq!(
            options.args,
            os(&["--resume", "reported", "--model", "opus"])
        );
        assert_eq!(
            options.fallback_args,
            Some(os(&["--session-id", "reported", "--model", "opus"]))
        );
        assert_eq!(options.initial_prompt.as_deref(), Some("continue".as_ref()));
        Ok(())
    }

    #[test]
    fn restore_passes_a_continuation_prompt_only_to_resumed_coding_agents() -> Result<()> {
        let worktree = tempfile::tempdir()?;
        let root = worktree.path();
        let store = Arc::new(MemorySessions::default());
        let mut worker = record("worker", root, coding("claude"), &["--model", "opus"]);
        worker.conversation = Some("conv".into());
        let idle = record("idle", root, coding("claude"), &[]);
        // Without resume arguments the agent starts fresh, so a nudge means nothing.
        let fresh = record("fresh", root, coding("plain"), &[]);
        let mut shell = record("shell", root, Agent::Shell, &[]);
        shell.foreground = coding("claude");
        for record in [&worker, &idle, &fresh, &shell] {
            store.save(record)?;
        }
        let (application, starts) = app(store.clone());

        let restored = application.restore_sessions_with(|id| {
            RestoreChoice::Resume((id != "idle").then(|| format!("continue {id}")))
        })?;

        let prompted: Vec<_> = restored
            .iter()
            .map(|session| (session.id.as_str(), session.prompted))
            .collect();
        assert_eq!(
            prompted,
            [
                ("worker", true),
                ("idle", false),
                ("fresh", false),
                ("shell", false)
            ]
        );
        let starts = starts.0.lock().unwrap();
        let (_, options) = &starts[0];
        assert_eq!(options.args, os(&["--resume", "conv", "--model", "opus"]));
        assert_eq!(
            options.initial_prompt.as_deref(),
            Some("continue worker".as_ref())
        );
        assert!(
            starts[1..]
                .iter()
                .all(|(_, options)| options.initial_prompt.is_none())
        );
        assert!(
            !format!("{:?}", store.list()?).contains("continue"),
            "the continuation prompt must never be persisted"
        );
        Ok(())
    }

    #[test]
    fn restore_forgets_sessions_the_caller_rejects_without_launching_them() -> Result<()> {
        let worktree = tempfile::tempdir()?;
        let root = worktree.path();
        let store = Arc::new(MemorySessions::default());
        for id in ["finished", "active"] {
            store.save(&record(id, root, coding("claude"), &[]))?;
        }
        let (application, starts) = app(store.clone());

        let restored = application.restore_sessions_with(|id| match id {
            "finished" => RestoreChoice::Forget("its attempt finished"),
            _ => RestoreChoice::Resume(None),
        })?;

        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, "active");
        let starts = starts.0.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].0.id, "active");
        let kept: Vec<_> = store.list()?.into_iter().map(|record| record.id).collect();
        assert_eq!(kept, ["active"]);
        assert!(application.terminal("finished").is_err());
        Ok(())
    }

    #[test]
    fn restored_shells_fall_back_to_a_fresh_agent_only_after_a_resume() -> Result<()> {
        let worktree = tempfile::tempdir()?;
        let root = worktree.path();
        let store = Arc::new(MemorySessions::default());
        for (id, agent, conversation) in [
            ("codex-shell", "codex", Some("ignored")),
            ("fresh-claude-shell", "claude", None),
            ("plain-shell", "plain", Some("conv")),
        ] {
            let mut shell = record(id, root, Agent::Shell, &[]);
            shell.foreground = coding(agent);
            shell.conversation = conversation.map(Into::into);
            store.save(&shell)?;
        }
        let (application, starts) = app(store);

        application.restore_sessions()?;

        let resumes: Vec<_> = starts
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, options)| options.resume.clone().unwrap())
            .collect();
        let resume = |program: &str,
                      args: &[&str],
                      conversation: Option<&str>,
                      fallback: Option<&[&str]>| {
            ResumeCommand {
                program: program.into(),
                args: os(args),
                conversation: conversation.map(Into::into),
                fallback: fallback.map(os),
            }
        };
        assert_eq!(
            resumes,
            [
                resume("codex", &["resume", "--last"], Some("ignored"), Some(&[])),
                resume("claude", &["--continue"], None, Some(&[])),
                resume("plain", &[], Some("conv"), None),
            ]
        );
        Ok(())
    }

    #[test]
    fn record_foreground_updates_the_saved_record() -> Result<()> {
        let store = Arc::new(MemorySessions::default());
        let (application, _) = app(store.clone());
        let session = application.start_session(StartSession {
            worktree: "/repo".into(),
            agent: Agent::Shell,
        })?;
        application.record_foreground(
            &session.id,
            &coding("claude"),
            Some("conv"),
            Some(Path::new("/repo/sub")),
            None,
        )?;
        let saved = &store.list()?[0];
        assert_eq!(saved.foreground, coding("claude"));
        assert_eq!(saved.conversation.as_deref(), Some("conv"));
        assert_eq!(saved.cwd.as_deref(), Some(Path::new("/repo/sub")));
        Ok(())
    }

    #[test]
    fn a_forked_launch_is_never_restored_as_a_pinned_resume() -> Result<()> {
        let worktree = tempfile::tempdir()?;
        let store = Arc::new(MemorySessions::default());
        let saved = Labels {
            task: Some("Fork task".into()),
            model: Some("model".into()),
            model_reported: true,
        };
        // A record from before forks were told apart names the parent conversation.
        let mut fork = record(
            "fork",
            worktree.path(),
            coding("claude"),
            &["--resume", "parent", "--fork-session", "--model", "opus"],
        );
        fork.conversation = Some("parent".into());
        fork.labels = saved.clone();
        store.save(&fork)?;
        // Positive control: the same launch without the fork keeps its labels.
        let mut pinned = record(
            "pinned",
            worktree.path(),
            coding("claude"),
            &["--resume", "parent", "--model", "opus"],
        );
        pinned.conversation = Some("parent".into());
        pinned.labels = saved.clone();
        store.save(&pinned)?;
        let (application, starts) = app(store.clone());
        application.restore_sessions()?;

        let starts = starts.0.lock().unwrap();
        let options = |id: &str| &starts.iter().find(|(s, _)| s.id == id).unwrap().1;
        // The restore resumes the saved conversation itself and never forks again.
        assert_eq!(
            options("fork").args,
            os(&["--resume", "parent", "--model", "opus"])
        );
        assert_eq!(
            options("fork").fallback_args,
            Some(os(&["--session-id", "parent", "--model", "opus"]))
        );
        assert_eq!(options("fork").labels, Labels::default());
        assert_eq!(options("pinned").labels, saved);
        let records = store.list()?;
        let labels = |id: &str| records.iter().find(|r| r.id == id).unwrap().labels.clone();
        assert_eq!(labels("fork"), Labels::default());
        assert_eq!(labels("pinned"), saved);
        Ok(())
    }

    #[test]
    fn saved_labels_return_only_with_a_resume_pinned_to_their_conversation() -> Result<()> {
        let worktree = tempfile::tempdir()?;
        let root = worktree.path();
        let store = Arc::new(MemorySessions::default());
        let saved = Labels {
            task: Some("Task".into()),
            model: Some("model".into()),
            model_reported: true,
        };
        // (id, launched agent, foreground agent, conversation, whether the labels return)
        let cases = [
            (
                "managed-pinned",
                coding("claude"),
                coding("claude"),
                Some("c"),
                true,
            ),
            (
                "managed-unknown",
                coding("claude"),
                coding("claude"),
                None,
                false,
            ),
            (
                "managed-last",
                coding("codex"),
                coding("codex"),
                Some("c"),
                false,
            ),
            (
                "managed-custom-last",
                coding("last"),
                coding("last"),
                Some("c"),
                false,
            ),
            (
                "managed-no-resume",
                coding("plain"),
                coding("plain"),
                Some("c"),
                false,
            ),
            (
                "shell-pinned",
                Agent::Shell,
                coding("claude"),
                Some("c"),
                true,
            ),
            ("shell-unknown", Agent::Shell, coding("claude"), None, false),
            (
                "shell-last",
                Agent::Shell,
                coding("codex"),
                Some("c"),
                false,
            ),
            (
                "shell-custom-last",
                Agent::Shell,
                coding("last"),
                Some("c"),
                false,
            ),
            (
                "shell-no-resume",
                Agent::Shell,
                coding("plain"),
                Some("c"),
                false,
            ),
            ("shell-bare", Agent::Shell, Agent::Shell, Some("c"), false),
        ];
        for (id, agent, foreground, conversation, _) in &cases {
            let mut record = record(id, root, agent.clone(), &[]);
            record.foreground = foreground.clone();
            record.conversation = conversation.map(Into::into);
            record.labels = saved.clone();
            store.save(&record)?;
        }
        let (application, starts) = app(store.clone());
        application.restore_sessions()?;

        let starts = starts.0.lock().unwrap();
        for (id, .., kept) in &cases {
            let expected = if *kept {
                saved.clone()
            } else {
                Labels::default()
            };
            let (_, options) = starts.iter().find(|(s, _)| s.id == *id).unwrap();
            assert_eq!(options.labels, expected, "{id}");
            // The record drops them too, so no later restore brings them back.
            let record = store.list()?.into_iter().find(|r| r.id == *id).unwrap();
            assert_eq!(record.labels, expected, "{id}");
        }
        Ok(())
    }
}
