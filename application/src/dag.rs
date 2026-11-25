use crate::{Application, SessionOptions, StartSession, TerminalRuntime};
use anyhow::{Context, Result, ensure};
use domain::{
    dag::{
        AgentState, Attempt, AttemptState, BoxAttention, BoxInfo, Coordinator, ManagedAgent,
        Provider, ReportStatus, ReportTransition, Run, RunEvent, RunEventKind, Task, TaskSpec,
        TaskState, can_dispatch, report_transition, validate_batch,
    },
    session::Agent,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub trait DagStore: Send + Sync {
    fn read<T>(&self, query: impl FnOnce(&dyn DagReader) -> Result<T>) -> Result<T>;

    /// Runs the operation atomically and in isolation from all other mutations.
    /// An error rolls back the operation, including any events it emitted.
    fn write<T>(&self, operation: impl FnOnce(&dyn DagTransaction) -> Result<T>) -> Result<T>;
}

pub trait DagReader {
    fn agent(&self, id: &str) -> Result<ManagedAgent>;
    fn session_agent(&self, session: &str) -> Result<Option<ManagedAgent>>;
    fn list_agents(&self, box_id: Option<&str>) -> Result<Vec<ManagedAgent>>;
    fn run(&self, id: &str) -> Result<Run>;
    fn run_box(&self, id: &str) -> Result<String>;
    fn list_runs(&self, box_id: Option<&str>) -> Result<Vec<Run>>;
    fn open_attempts(&self, session: Option<&str>) -> Result<Vec<OpenAttempt>>;
    fn events(&self, scope: EventScope<'_>, since: u64) -> Result<Vec<RunEvent>>;
    fn latest_seq(&self) -> Result<u64>;
}

pub trait DagTransaction: DagReader {
    fn insert_agent(&self, agent: &ManagedAgent) -> Result<()>;
    fn set_agent_exited(&self, id: &str) -> Result<()>;
    fn insert_run(&self, run: &Run) -> Result<()>;
    fn insert_tasks(&self, run: &str, tasks: &[TaskSpec]) -> Result<()>;
    fn insert_dep(&self, run: &str, task: &str, dep: &str) -> Result<()>;
    fn insert_attempt(&self, run: &str, task: &str, attempt: &Attempt) -> Result<()>;
    fn update_attempt(&self, attempt: &Attempt) -> Result<()>;
    fn event(
        &self,
        run: &str,
        kind: RunEventKind,
        task: Option<&str>,
        attempt: Option<&str>,
        summary: Option<&str>,
    ) -> Result<()>;
}

pub struct OpenAttempt {
    pub run: String,
    pub box_id: String,
    pub task: String,
    pub attempt: Attempt,
}

#[derive(Clone, Copy)]
pub enum EventScope<'a> {
    Run(&'a str, &'a [RunEventKind]),
    Watch(Option<&'a str>),
}

pub struct EventsPage {
    pub events: Vec<RunEvent>,
    pub cursor: u64,
}

pub trait BoxRegistry: Send + Sync {
    fn boxes(&self) -> Result<Vec<RegisteredBox>>;
}

pub struct RegisteredBox {
    pub info: BoxInfo,
    pub sessions: Vec<String>,
}

pub struct BoxSelection {
    pub selector: Option<String>,
    pub caller: Option<String>,
}

pub struct StartAgent {
    pub selection: BoxSelection,
    pub provider: Provider,
    pub worktree: PathBuf,
    pub parent: Option<String>,
    pub label: String,
    pub prompt: Option<String>,
    pub args: Vec<String>,
}

pub struct Dispatch {
    pub run: String,
    pub task: String,
    pub provider: Provider,
    pub worktree: PathBuf,
    pub retry: bool,
    pub args: Vec<String>,
    pub caller: Option<String>,
}

pub struct DispatchResult {
    pub agent: ManagedAgent,
    pub attempt: Attempt,
}

pub struct ReportResult {
    pub applied: bool,
    pub reason: Option<String>,
}

pub struct DagApplication<T: TerminalRuntime, S: DagStore, B: BoxRegistry> {
    application: Arc<Application<T>>,
    store: S,
    registry: B,
    event_generation: Mutex<u64>,
    events_changed: Condvar,
}

struct Worker {
    agent: ManagedAgent,
    args: Vec<String>,
    prompt: Option<String>,
    attempt: Option<WorkerAttempt>,
}

struct WorkerAttempt {
    run: String,
    task: String,
    id: String,
    number: u32,
}

impl<T: TerminalRuntime, S: DagStore, B: BoxRegistry> DagApplication<T, S, B> {
    pub fn new(application: Arc<Application<T>>, store: S, registry: B) -> Self {
        Self {
            application,
            store,
            registry,
            event_generation: Mutex::new(0),
            events_changed: Condvar::new(),
        }
    }

    pub fn boxes(&self) -> Result<Vec<BoxInfo>> {
        Ok(self
            .registry
            .boxes()?
            .into_iter()
            .map(|entry| entry.info)
            .collect())
    }

    pub fn select_box(&self, selector: &str) -> Result<String> {
        Ok(select_box(&self.registry.boxes()?, selector)?
            .info
            .id
            .clone())
    }

    pub fn list_agents(&self, selection: BoxSelection) -> Result<Vec<ManagedAgent>> {
        self.store
            .read(|db| db.list_agents(self.resolve_box(db, &selection)?.as_deref()))
    }

    pub fn list_runs(&self, selection: BoxSelection) -> Result<Vec<Run>> {
        self.store.read(|db| {
            let mut runs = db.list_runs(self.resolve_box(db, &selection)?.as_deref())?;
            for run in &mut runs {
                run.derive_task_states();
            }
            Ok(runs)
        })
    }

    pub fn run(&self, id: &str) -> Result<Run> {
        self.store.read(|db| load_run(db, id))
    }

    pub fn create_run(
        &self,
        selection: BoxSelection,
        title: String,
        coordinator: Option<String>,
    ) -> Result<Run> {
        self.write(|db| {
            let box_id = self
                .resolve_box(db, &selection)?
                .context("Choose a box with --box or create the run from a Power terminal")?;
            let coordinator = if let Some(label) = coordinator {
                Coordinator::External { label }
            } else {
                let session = selection
                    .caller
                    .as_deref()
                    .context("Specify --coordinator when creating a run outside Power")?;
                Coordinator::Agent {
                    agent: self.register_caller(db, session, &box_id)?.id,
                }
            };
            let run = Run {
                id: self.new_id(),
                box_id,
                title,
                coordinator,
                created_at: now(),
                tasks: Vec::new(),
            };
            db.insert_run(&run)?;
            db.event(&run.id, RunEventKind::RunCreated, None, None, None)?;
            Ok(run)
        })
    }

    pub fn add_tasks(&self, run: &str, tasks: &[TaskSpec]) -> Result<Run> {
        self.write(|db| {
            let before = load_run(db, run)?;
            validate_batch(&task_specs(&before), tasks)?;
            db.insert_tasks(run, tasks)?;
            let run = load_run(db, run)?;
            for task in &run.tasks[before.tasks.len()..] {
                db.event(&run.id, RunEventKind::TaskAdded, Some(&task.id), None, None)?;
                if task.state == TaskState::Ready {
                    db.event(&run.id, RunEventKind::TaskReady, Some(&task.id), None, None)?;
                }
            }
            Ok(run)
        })
    }

    pub fn add_dependency(&self, run: &str, task: &str, dep: &str) -> Result<Run> {
        self.write(|db| {
            let mut specs = task_specs(&load_run(db, run)?);
            let target = specs
                .iter_mut()
                .find(|spec| spec.id == task)
                .with_context(|| format!("Unknown task {task:?} in run {run:?}"))?;
            target.deps.push(dep.into());
            validate_batch(&specs, &[])?;
            db.insert_dep(run, task, dep)?;
            load_run(db, run)
        })
    }

    pub fn start_agent(&self, command: StartAgent) -> Result<ManagedAgent> {
        let box_id = self
            .store
            .read(|db| self.resolve_box(db, &command.selection))?
            .context("Choose a box with --box or start the agent from a Power terminal")?;
        let worktree = self.validate_worktree(&box_id, &command.worktree)?;
        self.spawn_worker(|db| {
            let parent = if let Some(id) = command.parent {
                ensure!(
                    db.agent(&id)?.box_id == box_id,
                    "Parent agent is in another box"
                );
                Some(id)
            } else {
                self.hosted_caller(command.selection.caller.as_deref())
                    .map(|session| self.register_caller(db, session, &box_id))
                    .transpose()?
                    .map(|agent| agent.id)
            };
            Ok(Worker {
                agent: self.new_agent(box_id, worktree, command.provider, parent, command.label),
                args: command.args,
                prompt: command.prompt,
                attempt: None,
            })
        })
        .map(|(agent, _)| agent)
    }

    pub fn dispatch(&self, command: Dispatch) -> Result<DispatchResult> {
        let box_id = self.store.read(|db| db.run_box(&command.run))?;
        let worktree = self.validate_worktree(&box_id, &command.worktree)?;
        let (agent, attempt) = self.spawn_worker(|db| {
            let run = load_run(db, &command.run)?;
            let task = find_task(&run, &command.task)?;
            ensure!(
                can_dispatch(task.state, command.retry),
                "Task {:?} cannot be dispatched in state {:?}{}",
                task.id,
                task.state,
                if !command.retry
                    && matches!(task.state, TaskState::Failed | TaskState::Interrupted)
                {
                    ". Use --retry to start another attempt"
                } else {
                    ""
                },
            );
            let number = task.attempts.last().map_or(Ok(1), |previous| {
                previous
                    .number
                    .checked_add(1)
                    .context("Too many attempts for this task")
            })?;
            let prompt = (command.provider != Provider::Command)
                .then(|| worker_prompt(&run, task, &worktree));
            let label = task.title.clone();
            // An external coordinator keeps its reference on the run, so its workers have no
            // parent agent, whatever terminal the CLI happens to run in. A Power caller that this
            // daemon hosts becomes the parent; otherwise the run's coordinating agent is.
            let parent = match run.coordinator {
                Coordinator::External { .. } => None,
                Coordinator::Agent { agent } => match self.hosted_caller(command.caller.as_deref()) {
                    Some(session) => Some(self.register_caller(db, session, &box_id)?.id),
                    None => Some(agent),
                },
            };
            Ok(Worker {
                agent: self.new_agent(box_id, worktree, command.provider, parent, label),
                args: command.args,
                prompt,
                attempt: Some(WorkerAttempt {
                    id: self.new_id(),
                    run: command.run,
                    task: command.task,
                    number,
                }),
            })
        })?;
        Ok(DispatchResult {
            agent,
            attempt: attempt.expect("dispatch creates an attempt"),
        })
    }

    fn spawn_worker(
        &self,
        prepare: impl FnOnce(&dyn DagTransaction) -> Result<Worker>,
    ) -> Result<(ManagedAgent, Option<Attempt>)> {
        // A fast worker can connect immediately. A report or exit waits for this
        // transaction to commit so it observes the new agent and attempt together.
        let mut launched = None;
        let result = self.write(|db| {
            let Worker {
                mut agent,
                args,
                prompt,
                attempt,
            } = prepare(db)?;
            if let Some(target) = &attempt {
                agent.run = Some(target.run.clone());
                agent.task = Some(target.task.clone());
                agent.attempt = Some(target.id.clone());
            }
            let terminal = terminal_agent(agent.provider);
            let mut argv = Vec::<OsString>::new();
            let initial_prompt = if agent.provider == Provider::Command {
                ensure!(
                    args.first().is_some_and(|program| !program.is_empty()),
                    "Specify a command after --"
                );
                None
            } else {
                argv.push(terminal.command().into());
                prompt.map(Into::into)
            };
            argv.extend(args.into_iter().map(OsString::from));
            let env = [
                ("POWER_BOX", Some(agent.box_id.as_str())),
                ("POWER_AGENT", Some(agent.id.as_str())),
                ("POWER_RUN", agent.run.as_deref()),
                ("POWER_TASK", agent.task.as_deref()),
                ("POWER_ATTEMPT", agent.attempt.as_deref()),
            ]
            .into_iter()
            .map(|(name, value)| (name.into(), value.map(OsString::from)))
            .collect();
            let session = self.application.start_session_with(
                StartSession {
                    worktree: agent.worktree.clone(),
                    agent: terminal,
                },
                SessionOptions {
                    argv,
                    env,
                    initial_prompt,
                },
            )?;
            launched = Some(session.id.clone());
            agent.session = Some(session.id);
            agent.worktree = session.worktree;
            db.insert_agent(&agent)?;
            let attempt = if let Some(WorkerAttempt {
                run,
                task,
                id,
                number,
            }) = attempt
            {
                let attempt = Attempt {
                    id,
                    number,
                    agent: agent.id.clone(),
                    session: agent.session.clone(),
                    worktree: agent.worktree.clone(),
                    provider: agent.provider,
                    state: AttemptState::Active,
                    summary: None,
                    blocker: None,
                    detail: None,
                    started_at: agent.created_at,
                    ended_at: None,
                };
                db.insert_attempt(&run, &task, &attempt)?;
                db.event(
                    &run,
                    RunEventKind::AttemptStarted,
                    Some(&task),
                    Some(&attempt.id),
                    None,
                )?;
                Some(attempt)
            } else {
                None
            };
            Ok((agent, attempt))
        });
        if result.is_err()
            && let Some(session) = launched
        {
            let _ = self.application.end_session(&session);
        }
        result
    }

    pub fn report(
        &self,
        run: &str,
        task: &str,
        attempt: &str,
        status: ReportStatus,
        summary: &str,
    ) -> Result<ReportResult> {
        self.write(|db| report(db, run, task, attempt, status, summary))
    }

    pub fn session_exited(&self, session: &str, detail: &str) -> Result<()> {
        self.write(|db| {
            if let Some(agent) = db.session_agent(session)? {
                interrupt(db, Some(session), detail)?;
                db.set_agent_exited(&agent.id)?;
            }
            Ok(())
        })
    }

    pub fn reconcile(&self) -> Result<()> {
        self.write(|db| {
            interrupt(db, None, "daemon restarted; session unavailable")?;
            for agent in db.list_agents(None)? {
                if agent.state == AgentState::Running {
                    db.set_agent_exited(&agent.id)?;
                }
            }
            Ok(())
        })
    }

    pub fn attention(&self) -> Result<Vec<BoxAttention>> {
        self.store.read(|db| {
            let mut boxes = BTreeMap::new();
            for OpenAttempt {
                box_id, attempt, ..
            } in db.open_attempts(None)?
            {
                if attempt.state == AttemptState::NeedsInput {
                    let attention = boxes.entry(box_id.clone()).or_insert(BoxAttention {
                        box_id,
                        sessions: Vec::new(),
                        agents: Vec::new(),
                    });
                    attention.agents.push(attempt.agent);
                    attention.sessions.extend(attempt.session);
                }
            }
            Ok(boxes.into_values().collect())
        })
    }

    pub fn latest_seq(&self) -> Result<u64> {
        self.store.read(|db| db.latest_seq())
    }

    pub fn events(&self, scope: EventScope<'_>, since: u64, wait: Duration) -> Result<EventsPage> {
        let started = Instant::now();
        let mut generation = self.event_generation.lock().unwrap();
        loop {
            let events = self.store.read(|db| db.events(scope, since))?;
            let remaining = wait.saturating_sub(started.elapsed());
            if !events.is_empty() || remaining.is_zero() {
                let cursor = events.last().map_or(since, |event| event.seq);
                return Ok(EventsPage { events, cursor });
            }
            let previous = *generation;
            (generation, _) = self
                .events_changed
                .wait_timeout_while(generation, remaining, |current| *current == previous)
                .unwrap();
        }
    }

    fn write<V>(&self, operation: impl FnOnce(&dyn DagTransaction) -> Result<V>) -> Result<V> {
        let result = self.store.write(operation)?;
        // Release the store before taking the notification mutex. A reader holds
        // that mutex between its last query and entering the wait.
        let mut generation = self.event_generation.lock().unwrap();
        *generation = generation.wrapping_add(1);
        self.events_changed.notify_all();
        Ok(result)
    }

    fn resolve_box(&self, db: &dyn DagReader, selection: &BoxSelection) -> Result<Option<String>> {
        if let Some(selector) = &selection.selector {
            return self.select_box(selector).map(Some);
        }
        if let Some(session) = &selection.caller {
            if let Some(agent) = db.session_agent(session)? {
                return Ok(Some(agent.box_id));
            }
            return self.view_box(session).map(Some);
        }
        Ok(None)
    }

    fn view_box(&self, session: &str) -> Result<String> {
        self.registry
            .boxes()?
            .into_iter()
            .find(|entry| entry.sessions.iter().any(|view| view == session))
            .map(|entry| entry.info.id)
            .with_context(|| format!("Power session {session:?} does not belong to a box"))
    }

    /// The caller's session when this daemon hosts it. The CLI forwards POWER_SESSION from any
    /// Power terminal, which can belong to another daemon when a coordinator runs in a
    /// multiplexer inside Power and targets a different socket.
    fn hosted_caller<'a>(&self, caller: Option<&'a str>) -> Option<&'a str> {
        caller.filter(|session| self.application.session_is_running(session).unwrap_or(false))
    }

    fn register_caller(
        &self,
        db: &dyn DagTransaction,
        session: &str,
        box_id: &str,
    ) -> Result<ManagedAgent> {
        // The exit hook also takes the store's write lock. It either observes the
        // registration afterward, or the terminal is already marked as exited.
        ensure!(
            self.application.session_is_running(session)?,
            "Caller session has exited"
        );
        if let Some(agent) = db.session_agent(session)? {
            ensure!(agent.box_id == box_id, "Caller session is in another box");
            return Ok(agent);
        }
        ensure!(
            self.view_box(session)? == box_id,
            "Caller session is in another box"
        );
        let session = self.application.session(session)?;
        let provider = match session.agent {
            Agent::Codex => Provider::Codex,
            Agent::Claude => Provider::Claude,
            Agent::Shell => Provider::Command,
        };
        let mut agent = self.new_agent(
            box_id.into(),
            session.worktree,
            provider,
            None,
            session.agent.name().into(),
        );
        agent.session = Some(session.id);
        db.insert_agent(&agent)?;
        Ok(agent)
    }

    fn validate_worktree(&self, box_id: &str, worktree: &Path) -> Result<PathBuf> {
        let entry = self
            .registry
            .boxes()?
            .into_iter()
            .find(|entry| entry.info.id == box_id)
            .with_context(|| format!("Unknown box {box_id:?}"))?;
        self.application
            .workspace
            .validate_box_worktree(&entry.info.repository, worktree)
    }

    fn new_id(&self) -> String {
        (self.application.new_id)()
    }

    fn new_agent(
        &self,
        box_id: String,
        worktree: PathBuf,
        provider: Provider,
        parent: Option<String>,
        label: String,
    ) -> ManagedAgent {
        ManagedAgent {
            id: self.new_id(),
            box_id,
            provider,
            parent,
            label,
            worktree,
            session: None,
            run: None,
            task: None,
            attempt: None,
            state: AgentState::Running,
            created_at: now(),
        }
    }
}

fn select_box<'a>(entries: &'a [RegisteredBox], selector: &str) -> Result<&'a RegisteredBox> {
    ensure!(!selector.is_empty(), "Box selector is empty");
    if let Some(entry) = entries.iter().find(|entry| entry.info.id == selector) {
        return Ok(entry);
    }
    if selector.len() == 1
        && let Ok(digit) = selector.parse::<u8>()
        && let Some(entry) = entries
            .iter()
            .find(|entry| entry.info.shortcut == Some(digit))
    {
        return Ok(entry);
    }
    let mut matches = entries
        .iter()
        .filter(|entry| entry.info.id.starts_with(selector) || entry.info.name == selector);
    let entry = matches
        .next()
        .with_context(|| format!("Unknown box {selector:?}"))?;
    ensure!(
        matches.next().is_none(),
        "Ambiguous box selector {selector:?}"
    );
    Ok(entry)
}

fn interrupt(db: &dyn DagTransaction, session: Option<&str>, detail: &str) -> Result<()> {
    for OpenAttempt {
        run,
        task,
        mut attempt,
        ..
    } in db.open_attempts(session)?
    {
        if attempt.interrupt(detail, now()) {
            db.update_attempt(&attempt)?;
            db.event(
                &run,
                RunEventKind::AttemptInterrupted,
                Some(&task),
                Some(&attempt.id),
                Some(detail),
            )?;
        }
    }
    Ok(())
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn terminal_agent(provider: Provider) -> Agent {
    match provider {
        Provider::Codex => Agent::Codex,
        Provider::Claude => Agent::Claude,
        Provider::Command => Agent::Shell,
    }
}

fn worker_prompt(run: &Run, task: &Task, worktree: &Path) -> String {
    use std::fmt::Write as _;
    let mut prompt = format!(
        "Run: {}\nTask: {} — {}\nWorktree: {}\n\nInstructions:\n{}\n",
        run.title,
        task.id,
        task.title,
        worktree.display(),
        task.instructions,
    );
    if !task.deps.is_empty() {
        prompt.push_str("\nCompleted prerequisites:\n");
        for dep in &task.deps {
            if let Ok(prerequisite) = find_task(run, dep)
                && let Some(summary) = prerequisite
                    .attempts
                    .last()
                    .and_then(|attempt| attempt.summary.as_deref())
            {
                let _ = writeln!(
                    prompt,
                    "- {} — {}: {summary}",
                    prerequisite.id, prerequisite.title
                );
            }
        }
    }
    prompt.push_str(concat!(
        "\nReport before exiting this terminal. The run, task and attempt are already in your environment.\n",
        "When done, run `power dag report success --summary \"...\"`.\n",
        "If you cannot complete the task, run `power dag report failed --summary \"...\"`.\n",
        "If you need user input, run `power dag report needs-input --summary \"question\"`, ",
        "wait for the user's answer in this terminal, then run ",
        "`power dag report working --summary \"resolved\"` before continuing.\n",
        "Full worker contract: `power skills get power-dag --reference worker-contract`.\n",
    ));
    prompt
}

fn task_specs(run: &Run) -> Vec<TaskSpec> {
    run.tasks
        .iter()
        .map(|task| TaskSpec {
            id: task.id.clone(),
            title: task.title.clone(),
            instructions: task.instructions.clone(),
            deps: task.deps.clone(),
        })
        .collect()
}

fn load_run(db: &dyn DagReader, id: &str) -> Result<Run> {
    let mut run = db.run(id)?;
    run.derive_task_states();
    Ok(run)
}

fn find_task<'a>(run: &'a Run, id: &str) -> Result<&'a Task> {
    run.tasks
        .iter()
        .find(|task| task.id == id)
        .with_context(|| format!("Unknown task {id:?} in run {:?}", run.id))
}

fn report(
    db: &dyn DagTransaction,
    run_id: &str,
    task_id: &str,
    attempt_id: &str,
    status: ReportStatus,
    summary: &str,
) -> Result<ReportResult> {
    use RunEventKind as Event;
    let run = load_run(db, run_id)?;
    let task = find_task(&run, task_id)?;
    let mut attempt = task
        .attempts
        .iter()
        .find(|attempt| attempt.id == attempt_id)
        .cloned()
        .with_context(|| {
            format!("Attempt {attempt_id:?} does not belong to task {task_id:?} in run {run_id:?}")
        })?;
    let emit = |kind| db.event(run_id, kind, Some(task_id), Some(attempt_id), Some(summary));
    let (state, resolves_input) = match report_transition(
        task.current_attempt.as_deref() == Some(attempt_id),
        attempt.state,
        status,
    ) {
        ReportTransition::Stale => {
            emit(Event::StaleReport)?;
            return Ok(ReportResult {
                applied: false,
                reason: Some("stale attempt".into()),
            });
        }
        ReportTransition::AlreadyReported => {
            return Ok(ReportResult {
                applied: false,
                reason: Some("already reported".into()),
            });
        }
        ReportTransition::Unchanged => {
            return Ok(ReportResult {
                applied: true,
                reason: None,
            });
        }
        ReportTransition::Update {
            state,
            resolves_input,
        } => (state, resolves_input),
    };
    if resolves_input {
        emit(Event::InputResolved)?;
    }
    attempt.state = state;
    attempt.blocker = (state == AttemptState::NeedsInput).then(|| summary.into());
    let event = match status {
        ReportStatus::Working => None,
        ReportStatus::NeedsInput => Some(Event::NeedsInput),
        ReportStatus::Success | ReportStatus::Failed => {
            attempt.summary = Some(summary.into());
            attempt.ended_at = Some(now());
            Some(if status == ReportStatus::Success {
                Event::AttemptSucceeded
            } else {
                Event::AttemptFailed
            })
        }
    };
    db.update_attempt(&attempt)?;
    if let Some(event) = event {
        emit(event)?;
    }
    if status == ReportStatus::Success {
        for dependent in load_run(db, run_id)?.tasks {
            if dependent.state == TaskState::Ready
                && dependent.deps.iter().any(|dep| dep == task_id)
            {
                db.event(run_id, Event::TaskReady, Some(&dependent.id), None, None)?;
            }
        }
    }
    Ok(ReportResult {
        applied: true,
        reason: None,
    })
}

#[cfg(test)]
mod tests;
