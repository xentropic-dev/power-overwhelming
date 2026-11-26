use crate::{
    Application, BranchStatus, SessionOptions, StartSession, TerminalRuntime, WorktreeRoot,
};
use anyhow::{Context, Result, bail, ensure};
use domain::{
    dag::{
        AgentState, Attempt, AttemptArtifact, AttemptState, BoxAttention, BoxInfo, Coordinator,
        ManagedAgent, Provider, ReportStatus, ReportTransition, Run, RunEvent, RunEventKind,
        RunStatus, Task, TaskSpec, TaskState, can_dispatch, report_transition, validate_batch,
    },
    session::Agent,
    worktree::{ManagedWorktree, WorktreeCreator, WorktreeRequest, default_worktree_path},
};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    /// Agents of any state whose worktree is exactly `worktree`.
    fn worktree_agents(&self, worktree: &Path) -> Result<Vec<ManagedAgent>>;
    fn run(&self, id: &str) -> Result<Run>;
    fn run_box(&self, id: &str) -> Result<String>;
    fn list_runs(&self, box_id: Option<&str>) -> Result<Vec<Run>>;
    fn open_attempts(&self, session: Option<&str>) -> Result<Vec<OpenAttempt>>;
    fn events(&self, scope: EventScope<'_>, since: u64) -> Result<Vec<RunEvent>>;
    fn latest_seq(&self) -> Result<u64>;
    /// Lists ownership records for one box, or for all boxes when unfiltered.
    fn list_worktrees(&self, box_id: Option<&str>) -> Result<Vec<ManagedWorktree>>;
    /// Looks up the ownership record created for this attempt ID.
    fn find_worktree_by_attempt(&self, attempt: &str) -> Result<Option<ManagedWorktree>>;
    fn find_worktree_by_path(&self, path: &Path) -> Result<Option<ManagedWorktree>>;
}

pub trait DagTransaction: DagReader {
    fn insert_agent(&self, agent: &ManagedAgent) -> Result<()>;
    fn set_agent_exited(&self, id: &str) -> Result<()>;
    fn set_agent_model(&self, id: &str, model: Option<&str>) -> Result<()>;
    fn insert_run(&self, run: &Run) -> Result<()>;
    fn insert_tasks(&self, run: &str, tasks: &[TaskSpec]) -> Result<()>;
    fn insert_dep(&self, run: &str, task: &str, dep: &str) -> Result<()>;
    fn set_task_plan(
        &self,
        run: &str,
        task: &str,
        provider: Option<&Provider>,
        model: Option<&str>,
    ) -> Result<()>;
    fn insert_attempt(&self, run: &str, task: &str, attempt: &Attempt) -> Result<()>;
    fn update_attempt(&self, attempt: &Attempt) -> Result<()>;
    fn insert_worktree(&self, worktree: &ManagedWorktree) -> Result<()>;
    fn remove_worktree(&self, path: &Path) -> Result<()>;
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

/// Custom images that replace a box's generated identicon.
pub trait BoxIcons: Send + Sync {
    /// Stores the image at `source`, an absolute path, as the box's icon.
    fn set(&self, box_id: &str, source: &Path) -> Result<()>;
    /// Restores the generated identicon. Succeeds when the box has no icon.
    fn clear(&self, box_id: &str) -> Result<()>;
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
    pub worktree: WorktreeRequest,
    pub parent: Option<String>,
    pub label: String,
    pub prompt: Option<String>,
    pub args: Vec<String>,
}

pub struct Dispatch {
    pub run: String,
    pub task: String,
    /// `None` uses the task's planned provider and model.
    pub provider: Option<Provider>,
    pub worktree: WorktreeRequest,
    pub retry: bool,
    pub args: Vec<String>,
    pub caller: Option<String>,
}

/// A change to a task's planned provider and model.
pub struct PlanTask {
    pub run: String,
    pub task: String,
    /// Drops the current plan before applying `provider` and `model`.
    pub clear: bool,
    /// A new provider. Without `model`, it also drops the planned model.
    pub provider: Option<Provider>,
    /// A new model. An empty string drops the planned model.
    pub model: Option<String>,
}

pub struct DispatchResult {
    pub agent: ManagedAgent,
    pub attempt: Attempt,
    pub worktree: PathBuf,
    /// The branch when this dispatch creates or resumes a managed worktree.
    pub branch: Option<String>,
}

pub struct ReportResult {
    pub applied: bool,
    pub reason: Option<String>,
}

pub struct DagApplication<T: TerminalRuntime, S: DagStore, B: BoxRegistry, I: BoxIcons> {
    application: Arc<Application<T>>,
    store: S,
    registry: B,
    icons: I,
    worktree_root: Option<Box<dyn WorktreeRoot>>,
    event_generation: Mutex<u64>,
    events_changed: Condvar,
}

struct DispatchWorktree {
    path: PathBuf,
    kind: DispatchWorktreeKind,
}

enum DispatchWorktreeKind {
    Existing,
    Resume(String),
    New { branch: String, base: String },
}

struct WorkerAttempt {
    run: String,
    task: String,
    id: String,
    number: u32,
}

impl<T: TerminalRuntime, S: DagStore, B: BoxRegistry, I: BoxIcons> DagApplication<T, S, B, I> {
    pub fn new(application: Arc<Application<T>>, store: S, registry: B, icons: I) -> Self {
        Self {
            application,
            store,
            registry,
            icons,
            worktree_root: None,
            event_generation: Mutex::new(0),
            events_changed: Condvar::new(),
        }
    }

    /// Enables managed worktree dispatch for stores and workspaces that support it.
    pub fn with_worktree_root(mut self, root: impl WorktreeRoot + 'static) -> Self {
        self.worktree_root = Some(Box::new(root));
        self
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

    /// Sets the selected box's icon from an image on disk, or restores the identicon when
    /// `source` is absent. Returns the box ID.
    pub fn set_box_icon(&self, selection: BoxSelection, source: Option<PathBuf>) -> Result<String> {
        let box_id = self
            .store
            .read(|db| self.resolve_box(db, &selection))?
            .context("Select a box with --box or run from a Power terminal")?;
        match source {
            Some(source) => {
                ensure!(source.is_absolute(), "The icon path must be absolute");
                self.icons.set(&box_id, &source)?;
            }
            None => self.icons.clear(&box_id)?,
        }
        Ok(box_id)
    }

    pub fn list_agents(&self, selection: BoxSelection) -> Result<Vec<ManagedAgent>> {
        self.store
            .read(|db| db.list_agents(self.resolve_box(db, &selection)?.as_deref()))
    }

    pub fn list_runs(&self, selection: BoxSelection) -> Result<Vec<Run>> {
        let mut runs = self.store.read(|db| {
            let mut runs = db.list_runs(self.resolve_box(db, &selection)?.as_deref())?;
            for run in &mut runs {
                run.derive_task_states();
            }
            Ok(runs)
        })?;
        for run in &mut runs {
            self.attach_artifacts(run)?;
        }
        sort_runs(&mut runs);
        Ok(runs)
    }

    pub fn run(&self, id: &str) -> Result<Run> {
        let mut run = self.store.read(|db| load_run(db, id))?;
        self.attach_artifacts(&mut run)?;
        Ok(run)
    }

    /// Fills each attempt's artifacts from the artifact store, with one lookup for the run.
    fn attach_artifacts(&self, run: &mut Run) -> Result<()> {
        let mut sessions: Vec<String> = run
            .tasks
            .iter()
            .flat_map(|task| &task.attempts)
            .filter_map(|attempt| attempt.session.clone())
            .collect();
        sessions.sort();
        sessions.dedup();
        if sessions.is_empty() {
            return Ok(());
        }
        let mut by_session: BTreeMap<String, Vec<AttemptArtifact>> = BTreeMap::new();
        for published in self.application.artifacts.published(&sessions)? {
            by_session
                .entry(published.session)
                .or_default()
                .push(published.artifact);
        }
        for attempt in run.tasks.iter_mut().flat_map(|task| &mut task.attempts) {
            attempt.artifacts = attempt
                .session
                .as_ref()
                .and_then(|session| by_session.get(session))
                .cloned()
                .unwrap_or_default();
        }
        Ok(())
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
        let tasks = tasks
            .iter()
            .map(|task| {
                let (provider, model) = self
                    .validate_plan(task.provider.clone(), task.model.clone())
                    .with_context(|| format!("Task {:?}", task.id))?;
                Ok(TaskSpec {
                    provider,
                    model,
                    ..task.clone()
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let tasks = tasks.as_slice();
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

    /// Sets, changes or clears a task's planned provider and model.
    pub fn plan_task(&self, command: PlanTask) -> Result<Run> {
        self.write(|db| {
            let run = load_run(db, &command.run)?;
            let task = find_task(&run, &command.task)?;
            let (mut provider, mut model) = if command.clear {
                (None, None)
            } else {
                (task.provider.clone(), task.model.clone())
            };
            if let Some(next) = command.provider {
                provider = Some(next);
                model = None;
            }
            if let Some(next) = command.model {
                model = (!next.trim().is_empty()).then_some(next);
            }
            let (provider, model) = self.validate_plan(provider, model)?;
            if (&provider, &model) == (&task.provider, &task.model) {
                return Ok(run);
            }
            db.set_task_plan(&run.id, &task.id, provider.as_ref(), model.as_deref())?;
            db.event(
                &run.id,
                RunEventKind::TaskPlanned,
                Some(&task.id),
                None,
                Some(&plan_summary(provider.as_ref(), model.as_deref())),
            )?;
            load_run(db, &run.id)
        })
    }

    /// Normalizes a planned model and rejects plans that dispatch could not honour.
    fn validate_plan(
        &self,
        provider: Option<Provider>,
        model: Option<String>,
    ) -> Result<(Option<Provider>, Option<String>)> {
        if let Some(Provider::Agent(id)) = &provider {
            self.application.agents.resolve(id)?;
        }
        let model = match model {
            None => None,
            Some(value) => Some(
                domain::agent::model_name(&value)
                    .with_context(|| format!("Invalid model name {value:?}"))?,
            ),
        };
        if let Some(model) = &model {
            match &provider {
                Some(Provider::Command) => bail!("The command provider takes no planned model"),
                Some(Provider::Agent(id)) => ensure!(
                    domain::agent::model_args(id.as_str(), model).is_some(),
                    "Power does not know the model flag of {:?}, so it cannot launch a \
                     planned model. Plan the provider alone and pass the model after --",
                    id.as_str()
                ),
                // Dispatch names the provider and Power checks its model flag then.
                None => {}
            }
        }
        Ok((provider, model))
    }

    pub fn start_agent(&self, command: StartAgent) -> Result<ManagedAgent> {
        let box_id = self
            .store
            .read(|db| self.resolve_box(db, &command.selection))?
            .context("Choose a box with --box or start the agent from a Power terminal")?;
        let worktree = self.validate_worktree(&box_id, &command.worktree)?;
        self.worker_transaction(|db, launched| {
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
            let mut agent =
                self.new_agent(box_id, worktree, command.provider, parent, command.label);
            self.launch_worker(&mut agent, command.args, command.prompt, None, launched)?;
            db.insert_agent(&agent)?;
            Ok(agent)
        })
    }

    pub fn dispatch(&self, command: Dispatch) -> Result<DispatchResult> {
        let mut created_path = None;
        let result = self.worker_transaction(|db, launched| {
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
            let (provider, args) =
                planned_launch(&run.id, task, command.provider.clone(), &command.args)?;
            let entry = self.registered_box(&run.box_id)?;
            let caller = self.hosted_caller(command.caller.as_deref());
            let DispatchWorktree { path, kind } =
                self.dispatch_worktree(db, &entry.info, task, &command, caller)?;
            // An external coordinator keeps its reference on the run, so its workers have no
            // parent agent, whatever terminal the CLI happens to run in. A Power caller that this
            // daemon hosts becomes the parent; otherwise the run's coordinating agent is.
            let parent = match &run.coordinator {
                Coordinator::External { .. } => None,
                Coordinator::Agent { agent } => match caller {
                    Some(session) => Some(self.register_caller(db, session, &run.box_id)?.id),
                    None => Some(agent.clone()),
                },
            };
            let target = WorkerAttempt {
                id: self.new_id(),
                run: command.run,
                task: command.task,
                number,
            };
            let mut agent = self.new_agent(
                run.box_id.clone(),
                path,
                provider,
                parent,
                task.title.clone(),
            );
            agent.run = Some(target.run.clone());
            agent.task = Some(target.task.clone());
            agent.attempt = Some(target.id.clone());
            let provisioning = matches!(kind, DispatchWorktreeKind::New { .. });
            let mut ownership = None;
            let start = (|| -> Result<_> {
                let (branch, resuming) = match kind {
                    DispatchWorktreeKind::Existing => (None, false),
                    DispatchWorktreeKind::Resume(branch) => (Some(branch), true),
                    DispatchWorktreeKind::New { branch, base } => {
                        let worktree = self.create_dispatch_worktree(
                            &agent,
                            &target,
                            entry.info.repository,
                            &branch,
                            &base,
                        )?;
                        agent.worktree.clone_from(&worktree.path);
                        created_path = Some(worktree.path.clone());
                        ownership = Some(worktree);
                        (Some(branch), false)
                    }
                };
                let prompt = (agent.provider != Provider::Command).then(|| {
                    worker_prompt(&run, task, &agent.worktree, branch.as_deref(), resuming)
                });
                self.launch_worker(&mut agent, args, prompt, Some(&task.title), launched)?;
                Ok(branch)
            })();
            let start = match start {
                Err(error) if !provisioning => return Err(error),
                Err(error) => {
                    agent.state = AgentState::Exited;
                    Err(error)
                }
                success => success,
            };
            let attempt = insert_dispatch_attempt(db, &agent, target, start.as_ref().err())?;
            if let Some(ownership) = ownership {
                db.insert_worktree(&ownership)?;
            }
            // Commit the failed attempt, if any, before returning the error to the caller.
            Ok(start.map(|branch| DispatchResult {
                worktree: attempt.worktree.clone(),
                agent,
                attempt,
                branch,
            }))
        });
        if result.is_err()
            && let Some(path) = created_path
            && let Err(cleanup) = self.application.workspace.remove_worktree(&path, true)
        {
            return result
                .with_context(|| format!("Could not undo worktree creation: {cleanup:#}"))?;
        }
        result?
    }

    fn create_dispatch_worktree(
        &self,
        agent: &ManagedAgent,
        target: &WorkerAttempt,
        repository: PathBuf,
        branch: &str,
        base: &str,
    ) -> Result<ManagedWorktree> {
        let created = self.application.workspace.create_worktree(
            &repository,
            &agent.worktree,
            branch,
            base,
        )?;
        Ok(ManagedWorktree {
            box_id: agent.box_id.clone(),
            repository,
            path: created.path,
            branch: branch.to_owned(),
            base_commit: created.base_commit,
            created_at: now(),
            creator: WorktreeCreator::Attempt {
                run: target.run.clone(),
                task: target.task.clone(),
                attempt: target.id.clone(),
            },
        })
    }

    fn worker_transaction<R>(
        &self,
        operation: impl FnOnce(&dyn DagTransaction, &mut Option<String>) -> Result<R>,
    ) -> Result<R> {
        // A fast worker can connect immediately. A report or exit waits for this
        // transaction to commit so it observes the new agent and attempt together.
        let mut launched = None;
        let result = self.write(|db| operation(db, &mut launched));
        if result.is_err()
            && let Some(session) = launched
        {
            let _ = self.application.end_session(&session);
        }
        result
    }

    fn launch_worker(
        &self,
        agent: &mut ManagedAgent,
        args: Vec<String>,
        prompt: Option<String>,
        task_title: Option<&str>,
        launched: &mut Option<String>,
    ) -> Result<()> {
        let terminal = terminal_agent(&agent.provider);
        let initial_prompt = if agent.provider == Provider::Command {
            ensure!(
                args.first().is_some_and(|program| !program.is_empty()),
                "Specify a command after --"
            );
            None
        } else {
            prompt.map(Into::into)
        };
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
                args: args.into_iter().map(OsString::from).collect(),
                env,
                initial_prompt,
                initial_task: task_title.map(str::to_owned),
                ..SessionOptions::default()
            },
        )?;
        *launched = Some(session.id.clone());
        agent.model = self.session_model(&session)?;
        agent.session = Some(session.id);
        agent.worktree = session.worktree;
        Ok(())
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

    pub fn agent_model_changed(
        &self,
        session: &str,
        agent_type: &Agent,
        model: Option<&str>,
    ) -> Result<()> {
        self.write(|db| {
            let Some(agent) = db.session_agent(session)? else {
                return Ok(());
            };
            if agent.state != AgentState::Running
                || terminal_agent(&agent.provider) != *agent_type
                || agent.model.as_deref() == model
            {
                return Ok(());
            }
            db.set_agent_model(&agent.id, model)?;
            if let Some(run) = agent.run.as_deref() {
                db.event(
                    run,
                    RunEventKind::AgentModelChanged,
                    agent.task.as_deref(),
                    agent.attempt.as_deref(),
                    model,
                )?;
            }
            Ok(())
        })
    }

    pub fn session_exited(&self, session: &str, detail: &str) -> Result<()> {
        self.write(|db| {
            if let Some(agent) = db.session_agent(session)? {
                interrupt(db, Some(session), detail, &|_| false)?;
                db.set_agent_exited(&agent.id)?;
            }
            Ok(())
        })
    }

    /// Interrupts attempts and stops agents whose session did not survive a restart.
    /// Those in `restored` keep running under their original session IDs, unless their
    /// worktree is gone and they have no open attempt.
    pub fn reconcile(&self, restored: &[String]) -> Result<()> {
        let restored = |session: Option<&str>| {
            session.is_some_and(|session| restored.iter().any(|id| id == session))
        };
        let workspace = self.application.workspace.as_ref();
        self.write(|db| {
            interrupt(db, None, RESTART_DETAIL, &restored)?;
            let attempting: BTreeSet<_> = db
                .open_attempts(None)?
                .into_iter()
                .map(|open| open.attempt.agent)
                .collect();
            for agent in db.list_agents(None)? {
                if agent.state != AgentState::Running {
                    continue;
                }
                let orphaned =
                    !attempting.contains(&agent.id) && !workspace.worktree_exists(&agent.worktree);
                if orphaned || !restored(agent.session.as_deref()) {
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
        caller.filter(|session| {
            self.application
                .session_is_running(session)
                .unwrap_or(false)
        })
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
        let provider = match session.agent.clone() {
            Agent::Coding(id) => Provider::Agent(id),
            Agent::Shell => Provider::Command,
        };
        let model = self.session_model(&session)?;
        let mut agent = self.new_agent(
            box_id.into(),
            session.worktree,
            provider,
            None,
            session.agent.name().into(),
        );
        agent.model = model;
        agent.session = Some(session.id);
        db.insert_agent(&agent)?;
        Ok(agent)
    }

    fn session_model(&self, session: &domain::session::Session) -> Result<Option<String>> {
        let terminal = self.application.terminal(&session.id)?;
        Ok(self.application.runtime.model(&terminal, &session.agent))
    }

    fn validate_worktree(&self, box_id: &str, request: &WorktreeRequest) -> Result<PathBuf> {
        let WorktreeRequest::Existing(worktree) = request else {
            bail!("Worktree provisioning is unavailable");
        };
        let entry = self.registered_box(box_id)?;
        self.application
            .workspace
            .validate_box_worktree(&entry.info.repository, worktree)
    }

    fn registered_box(&self, box_id: &str) -> Result<RegisteredBox> {
        self.registry
            .boxes()?
            .into_iter()
            .find(|entry| entry.info.id == box_id)
            .with_context(|| format!("Unknown box {box_id:?}"))
    }

    fn dispatch_worktree(
        &self,
        db: &dyn DagReader,
        box_info: &BoxInfo,
        task: &Task,
        command: &Dispatch,
        caller: Option<&str>,
    ) -> Result<DispatchWorktree> {
        let workspace = &self.application.workspace;
        let repository = &box_info.repository;
        match &command.worktree {
            WorktreeRequest::Existing(path) => Ok(DispatchWorktree {
                path: workspace.validate_box_worktree(repository, path)?,
                kind: DispatchWorktreeKind::Existing,
            }),
            WorktreeRequest::RetryManaged => {
                ensure!(command.retry, "Specify --worktree when starting a task");
                let previous = task
                    .attempts
                    .last()
                    .context("No earlier attempt to retry")?;
                let worktree = db
                    .find_worktree_by_path(&previous.worktree)?
                    .filter(|worktree| worktree.box_id == box_info.id)
                    .context("Specify --worktree to retry an attempt without a managed worktree")?;
                let path = workspace.validate_box_worktree(repository, &worktree.path)?;
                ensure!(
                    workspace.owns_worktree(&worktree),
                    "Managed branch {:?} no longer belongs to Power at {}. Specify --worktree to retry",
                    worktree.branch,
                    path.display(),
                );
                Ok(DispatchWorktree {
                    path,
                    kind: DispatchWorktreeKind::Resume(worktree.branch),
                })
            }
            WorktreeRequest::New { branch, base } => {
                ensure!(
                    !branch.trim().is_empty(),
                    "Specify --branch when using --worktree new"
                );
                ensure!(
                    !matches!(branch.as_str(), "." | ".."),
                    "Invalid branch name {branch:?}"
                );
                let root = self
                    .worktree_root
                    .as_ref()
                    .context("Worktree provisioning is unavailable")?;
                if let BranchStatus::Exists { checked_out_at } =
                    workspace.branch_exists(repository, branch)?
                {
                    if let Some(path) = checked_out_at {
                        bail!(
                            "Branch {branch:?} already exists and is checked out at {}",
                            path.display()
                        );
                    }
                    bail!("Branch {branch:?} already exists");
                }
                let base = match base {
                    Some(base) => base.clone(),
                    None => {
                        let caller = caller.context(
                            "Specify --base when dispatching a new worktree from outside Power",
                        )?;
                        workspace.head(&self.application.session(caller)?.worktree)?
                    }
                };
                let path =
                    default_worktree_path(&root.worktree_root()?, &box_info.name, branch, |path| {
                        path.symlink_metadata().is_ok()
                    });
                Ok(DispatchWorktree {
                    path,
                    kind: DispatchWorktreeKind::New {
                        branch: branch.clone(),
                        base,
                    },
                })
            }
        }
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
            model: None,
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

fn insert_dispatch_attempt(
    db: &dyn DagTransaction,
    agent: &ManagedAgent,
    target: WorkerAttempt,
    error: Option<&anyhow::Error>,
) -> Result<Attempt> {
    let attempt = Attempt {
        id: target.id,
        number: target.number,
        agent: agent.id.clone(),
        session: agent.session.clone(),
        worktree: agent.worktree.clone(),
        provider: agent.provider.clone(),
        model: agent.model.clone(),
        state: if error.is_some() {
            AttemptState::Failed
        } else {
            AttemptState::Active
        },
        summary: error.map(|error| format!("{error:#}")),
        blocker: None,
        detail: None,
        started_at: agent.created_at,
        ended_at: error.map(|_| now()),
        artifacts: Vec::new(),
    };
    db.insert_agent(agent)?;
    db.insert_attempt(&target.run, &target.task, &attempt)?;
    let event = if error.is_some() {
        db.update_attempt(&attempt)?;
        RunEventKind::AttemptFailed
    } else {
        RunEventKind::AttemptStarted
    };
    db.event(
        &target.run,
        event,
        Some(&target.task),
        Some(&attempt.id),
        attempt.summary.as_deref(),
    )?;
    Ok(attempt)
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

const RESTART_DETAIL: &str = "daemon restarted; session unavailable";

/// Interrupts open attempts of one session, or of every session, except those `keep` accepts.
fn interrupt(
    db: &dyn DagTransaction,
    session: Option<&str>,
    detail: &str,
    keep: &dyn Fn(Option<&str>) -> bool,
) -> Result<()> {
    for OpenAttempt {
        run,
        task,
        mut attempt,
        ..
    } in db.open_attempts(session)?
    {
        if !keep(attempt.session.as_deref()) && attempt.interrupt(detail, now()) {
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

fn terminal_agent(provider: &Provider) -> Agent {
    match provider {
        Provider::Agent(id) => Agent::Coding(id.clone()),
        Provider::Command => Agent::Shell,
    }
}

fn worker_prompt(
    run: &Run,
    task: &Task,
    worktree: &Path,
    branch: Option<&str>,
    resuming: bool,
) -> String {
    use std::fmt::Write as _;
    let mut prompt = format!(
        "Run: {}\nTask: {} — {}\nWorktree: {}\n",
        run.title,
        task.id,
        task.title,
        worktree.display(),
    );
    if let Some(branch) = branch {
        let _ = writeln!(prompt, "Branch: {branch}");
    }
    if resuming {
        prompt.push_str("You are resuming in the existing worktree. Review any partial work before continuing.\n");
    }
    let _ = writeln!(prompt, "\nInstructions:\n{}", task.instructions);
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
            provider: task.provider.clone(),
            model: task.model.clone(),
        })
        .collect()
}

fn plan_summary(provider: Option<&Provider>, model: Option<&str>) -> String {
    match (provider, model) {
        (None, None) => "no plan".into(),
        (Some(provider), None) => provider_id(provider).into(),
        (None, Some(model)) => model.into(),
        (Some(provider), Some(model)) => format!("{} / {model}", provider_id(provider)),
    }
}

fn provider_id(provider: &Provider) -> &str {
    match provider {
        Provider::Agent(id) => id.as_str(),
        Provider::Command => "command",
    }
}

/// The provider and launch arguments for an attempt.
///
/// Without `explicit`, the attempt uses the planned provider. An explicit provider replaces
/// the plan for this attempt when it differs from the planned one. An explicit provider equal
/// to the planned one keeps the planned model, so `--provider codex` on a `codex / gpt` plan
/// still launches `gpt`; pass model arguments to run another model. A plan with a model and
/// no provider applies its model to whichever provider the dispatch names. Arguments that do
/// not select a model never drop the planned model: when Power does not know the provider's
/// model flag, the launch fails rather than run without the planned model.
fn planned_launch(
    run: &str,
    task: &Task,
    explicit: Option<Provider>,
    args: &[String],
) -> Result<(Provider, Vec<String>)> {
    let clear_model = || format!("`power dag plan {run} {} --model ''`", task.id);
    let provider = match (explicit, &task.provider, &task.model) {
        (Some(provider), _, _) => provider,
        (None, Some(planned), _) => planned.clone(),
        (None, None, Some(model)) => bail!(
            "Task {:?} plans the model {model:?} without a provider. Pass --provider <id> to \
             launch it with that provider, or plan one with `power dag plan {run} {} --provider <id>`",
            task.id,
            task.id
        ),
        (None, None, None) => bail!(
            "Specify --provider, or plan one with `power dag plan {run} {} --provider <id>`",
            task.id
        ),
    };
    let Some(model) = &task.model else {
        return Ok((provider, args.to_vec()));
    };
    if task
        .provider
        .as_ref()
        .is_some_and(|planned| planned != &provider)
    {
        return Ok((provider, args.to_vec()));
    }
    let Provider::Agent(id) = &provider else {
        bail!(
            "The command provider takes no model. Clear the planned model {model:?} with {}",
            clear_model()
        )
    };
    if domain::agent::passes_model_flag(id.as_str(), args) {
        return Ok((provider, args.to_vec()));
    }
    let model_args = domain::agent::model_args(id.as_str(), model).with_context(|| {
        format!(
            "Power does not know the model flag of {:?}, so it cannot pass the planned model \
             {model:?}. Clear it with {} and pass the model after --",
            id.as_str(),
            clear_model()
        )
    })?;
    let mut launch = model_args;
    launch.extend_from_slice(args);
    Ok((provider, launch))
}

/// Active runs first, then stalled, then completed; newest first within each group.
fn sort_runs(runs: &mut [Run]) {
    let rank = |status| match status {
        RunStatus::Active => 0,
        RunStatus::Stalled => 1,
        RunStatus::Completed => 2,
    };
    runs.sort_by_key(|run| (rank(run.status()), std::cmp::Reverse(run.created_at)));
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

#[path = "worktrees.rs"]
mod worktrees;

#[cfg(test)]
mod tests;
