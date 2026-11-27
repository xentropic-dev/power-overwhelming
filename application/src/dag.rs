use crate::{
    Application, BranchStatus, RestoreChoice, Scheduler, SessionOptions, StartSession,
    TerminalRuntime, WorktreeRoot,
};
use anyhow::{Context, Result, bail, ensure};
use domain::{
    dag::{
        AgentState, AgentStatus, Attempt, AttemptArtifact, AttemptState, BoxAttention, BoxInfo,
        BudgetLimit, Coordinator, ManagedAgent, Provider, ReportStatus, ReportTransition, Run,
        RunEvent, RunEventKind, RunStatus, Task, TaskSpec, TaskState, WorkerBudget, WorkerLoad,
        can_dispatch, report_transition, validate_batch,
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
    /// Dispatches waiting for the worker budget, oldest first.
    fn queued_dispatches(&self) -> Result<Vec<QueuedDispatch>>;
    /// The task's queued dispatch, if any.
    fn queued_dispatch(&self, run: &str, task: &str) -> Result<Option<QueuedDispatch>> {
        Ok(self
            .queued_dispatches()?
            .into_iter()
            .find(|entry| entry.run == run && entry.task == task))
    }
}

pub trait DagTransaction: DagReader {
    fn insert_agent(&self, agent: &ManagedAgent) -> Result<()>;
    fn set_agent_exited(&self, id: &str) -> Result<()>;
    /// Marks the agent exited and releases its session, so the session can host another
    /// identity. Its messages stay readable under its ID.
    fn retire_agent(&self, id: &str) -> Result<()>;
    fn set_agent_model(&self, id: &str, model: Option<&str>) -> Result<()>;
    fn insert_run(&self, run: &Run) -> Result<()>;
    /// Sets or clears the run's archive time.
    fn set_run_archived(&self, id: &str, archived_at: Option<u64>) -> Result<()>;
    fn insert_tasks(&self, run: &str, tasks: &[TaskSpec]) -> Result<()>;
    fn insert_dep(&self, run: &str, task: &str, dep: &str) -> Result<()>;
    fn insert_attempt(&self, run: &str, task: &str, attempt: &Attempt) -> Result<()>;
    fn update_attempt(&self, attempt: &Attempt) -> Result<()>;
    fn insert_worktree(&self, worktree: &ManagedWorktree) -> Result<()>;
    fn remove_worktree(&self, path: &Path) -> Result<()>;
    /// At most one dispatch is queued per task.
    fn insert_queued(&self, queued: &QueuedDispatch) -> Result<()>;
    /// Removes the queued dispatch with this ID. Returns whether it was still queued.
    fn remove_queued(&self, id: &str) -> Result<bool>;
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
    pub provider: Provider,
    pub worktree: WorktreeRequest,
    pub retry: bool,
    pub args: Vec<String>,
    pub caller: Option<String>,
    pub budget: BudgetMode,
}

/// What a dispatch does when the worker budget is full.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BudgetMode {
    /// Queue the task. Power starts it when capacity frees up.
    #[default]
    Queue,
    /// Fail with the limit that holds the dispatch.
    Refuse,
    /// Start anyway. Also starts an already queued task now.
    Override,
}

/// A dispatch waiting for the worker budget. A new worktree's base is resolved to a commit
/// when it is queued, so the dispatch does not depend on the caller's HEAD or a moving ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedDispatch {
    /// Identifies this request. A later request for the same task gets a new ID, so a stale
    /// copy of the queue never starts or removes it.
    pub id: String,
    pub run: String,
    pub task: String,
    pub provider: Provider,
    pub worktree: WorktreeRequest,
    pub retry: bool,
    pub args: Vec<String>,
    pub caller: Option<String>,
    /// Unix seconds.
    pub queued_at: u64,
}

impl QueuedDispatch {
    fn into_dispatch(self, budget: BudgetMode) -> Dispatch {
        Dispatch {
            run: self.run,
            task: self.task,
            provider: self.provider,
            worktree: self.worktree,
            retry: self.retry,
            args: self.args,
            caller: self.caller,
            budget,
        }
    }
}

pub enum DispatchOutcome {
    Started(Box<DispatchResult>),
    /// The task waits for the worker budget. `reason` names the limit.
    Queued {
        reason: String,
    },
}

/// Reads the worker budget, at each dispatch so config edits apply at once.
pub trait WorkerBudgetSource: Send + Sync {
    fn worker_budget(&self) -> Result<WorkerBudget>;
}

/// Machine memory, for `WorkerBudget::min_available_memory`.
pub trait MemoryProbe: Send + Sync {
    /// Available memory plus free swap, in bytes, or `None` when unknown.
    fn available_memory(&self) -> Option<u64>;
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

/// How long a worker keeps running after it reports success or failure, so it can
/// finish its last message before its session ends.
pub const FINISHED_WORKER_GRACE: Duration = Duration::from_secs(10);

pub struct DagApplication<T: TerminalRuntime, S: DagStore, B: BoxRegistry, I: BoxIcons> {
    application: Arc<Application<T>>,
    store: S,
    registry: B,
    icons: I,
    worktree_root: Option<Box<dyn WorktreeRoot>>,
    /// How long a worker runs after its terminal report, and what ends it then.
    /// `None` keeps finished workers running.
    finished_cleanup: Option<FinishedCleanup>,
    budget: Option<(Box<dyn WorkerBudgetSource>, Box<dyn MemoryProbe>)>,
    /// This application, once shared, so a scheduled reclaim can call back.
    weak: Mutex<std::sync::Weak<Self>>,
    event_generation: Mutex<u64>,
    events_changed: Condvar,
}

/// Ends finished workers. Due teardowns and the record writes that follow them run
/// on separate schedulers, so a blocked session store write never delays the next
/// worker's teardown.
struct FinishedCleanup {
    grace: Duration,
    /// Runs due teardowns, which never touch a store.
    teardown: Arc<dyn Scheduler>,
    /// Forgets each ended session's record afterwards.
    persistence: Arc<dyn Scheduler>,
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
            finished_cleanup: None,
            budget: None,
            weak: Mutex::new(std::sync::Weak::new()),
            event_generation: Mutex::new(0),
            events_changed: Condvar::new(),
        }
    }

    /// Shares the application so scheduled reclaims can reach it. Without this, a
    /// session that kept processes is retried only by the next daemon start.
    pub fn shared(self) -> Arc<Self> {
        let shared = Arc::new(self);
        *shared.weak.lock().unwrap() = Arc::downgrade(&shared);
        shared
    }

    /// Enables managed worktree dispatch for stores and workspaces that support it.
    pub fn with_worktree_root(mut self, root: impl WorktreeRoot + 'static) -> Self {
        self.worktree_root = Some(Box::new(root));
        self
    }

    /// Ends a worker's session `grace` after it reports success or failure, using
    /// `teardown` to wait, then forgets its record through `persistence`. Without
    /// this, finished workers keep running.
    pub fn with_finished_worker_cleanup(
        mut self,
        grace: Duration,
        teardown: Arc<dyn Scheduler>,
        persistence: Arc<dyn Scheduler>,
    ) -> Self {
        self.finished_cleanup = Some(FinishedCleanup {
            grace,
            teardown,
            persistence,
        });
        self
    }

    /// Limits concurrent workers. Without a budget, every dispatch starts at once.
    pub fn with_worker_budget(
        mut self,
        source: impl WorkerBudgetSource + 'static,
        memory: impl MemoryProbe + 'static,
    ) -> Self {
        self.budget = Some((Box::new(source), Box::new(memory)));
        self
    }

    fn worker_budget(&self) -> Result<Option<WorkerBudget>> {
        self.budget
            .as_ref()
            .map(|(source, _)| source.worker_budget())
            .transpose()
            .context("Read the worker budget from config.lua")
    }

    /// The limit that holds one more worker in `run`, if any.
    fn budget_limit(
        &self,
        db: &dyn DagReader,
        run: &Run,
        budget: Option<&WorkerBudget>,
    ) -> Result<Option<BudgetLimit>> {
        let (Some(budget), Some((_, memory))) = (budget, &self.budget) else {
            return Ok(None);
        };
        let open = db.open_attempts(None)?;
        let load = WorkerLoad {
            total: open.len(),
            run: open.iter().filter(|open| open.run == run.id).count(),
            box_workers: open.iter().filter(|open| open.box_id == run.box_id).count(),
            available_memory: budget
                .min_available_memory
                .and_then(|_| memory.available_memory()),
        };
        Ok(budget.limit(load))
    }

    /// Starts queued dispatches, oldest first, while the worker budget has room. A queued
    /// dispatch that cannot start is dropped from the queue with a `QueuedDispatchFailed`
    /// event. Returns how many started.
    pub fn start_queued(&self) -> Result<usize> {
        let queued = self.store.read(|db| db.queued_dispatches())?;
        if queued.is_empty() {
            return Ok(0);
        }
        self.start_queued_entries(queued)
    }

    /// Starts entries from a snapshot of the queue. Each entry is claimed by ID inside its
    /// dispatch transaction, so an entry that started, left or was replaced since the
    /// snapshot is skipped, and only the claimed entry is ever removed.
    fn start_queued_entries(&self, queued: Vec<QueuedDispatch>) -> Result<usize> {
        let budget = self.worker_budget()?;
        let mut started = 0;
        for entry in queued {
            let (id, run, task) = (entry.id.clone(), entry.run.clone(), entry.task.clone());
            match self.dispatch_with(
                entry.into_dispatch(BudgetMode::Queue),
                Some(&id),
                budget.as_ref(),
            ) {
                Ok(DispatchOutcome::Started(_)) => started += 1,
                Ok(DispatchOutcome::Queued { .. }) => {}
                // A failure before the attempt was recorded rolls back, so the entry is still
                // queued. A failure after it, such as worktree creation, already removed it and
                // committed a failed attempt.
                Err(error) => self.write(|db| {
                    if db.remove_queued(&id)? {
                        db.event(
                            &run,
                            RunEventKind::QueuedDispatchFailed,
                            Some(&task),
                            None,
                            Some(&format!("{error:#}")),
                        )?;
                    }
                    Ok(())
                })?,
            }
        }
        Ok(started)
    }

    /// Starts a queued task now, over the worker budget, with the dispatch it was queued with.
    pub fn start_queued_now(&self, run: &str, task: &str) -> Result<DispatchResult> {
        let entry = self
            .store
            .read(|db| db.queued_dispatches())?
            .into_iter()
            .find(|entry| entry.run == run && entry.task == task)
            .with_context(|| format!("Task {task:?} in run {run:?} is not queued"))?;
        let id = entry.id.clone();
        match self.dispatch_with(entry.into_dispatch(BudgetMode::Override), Some(&id), None)? {
            DispatchOutcome::Started(result) => Ok(*result),
            DispatchOutcome::Queued { reason } => bail!("Task {task:?} did not start: {reason}"),
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

    pub fn box_for(&self, selection: BoxSelection) -> Result<Option<String>> {
        self.store.read(|db| self.resolve_box(db, &selection))
    }

    /// Gives a live coding session without an agent row (one started with `power session
    /// start`, or a provider running in a shell session's foreground) its peer identity in
    /// its box. A plain shell session or an unknown session is left alone. The foreground is
    /// resolved now, before the store lock, never from the terminal's periodic check.
    pub fn register_coding_session(&self, session: &str) -> Result<Option<ManagedAgent>> {
        let Some(live) = self.application.live_agent(session) else {
            return Ok(None);
        };
        if live == Agent::Shell {
            return Ok(None);
        }
        let Ok(info) = self.application.session(session) else {
            return Ok(None);
        };
        self.write(|db| {
            if let Some(agent) = db.session_agent(session)? {
                if agent.speaks_for(&live) {
                    return Ok(agent);
                }
                // A foreground identity whose coding process has left the shell.
                db.retire_agent(&agent.id)?;
            }
            ensure!(
                self.application.session_is_running(session)?,
                "Caller session has exited"
            );
            // A session started outside the strip (`power session start`) has no view,
            // so its box is the one whose repository holds its worktree.
            let box_id = match self.view_box(session) {
                Ok(id) => id,
                Err(_) => self.worktree_box(&info.worktree)?,
            };
            self.insert_session_agent(db, session, &box_id, &live)
        })
        .map(Some)
        .with_context(|| {
            format!("Coding session {session:?} has no Power agent identity to send as")
        })
    }

    /// A coding agent left the foreground of shell session `session`. The identity Power
    /// derived from that foreground ends with it. A launched session's own agent, and a
    /// command worker's row, are not foreground identities and are left alone.
    pub fn foreground_left(&self, session: &str, agent: &Agent) -> Result<()> {
        if self.application.launched_agent(session) != Some(Agent::Shell) {
            return Ok(());
        }
        self.write(|db| {
            if let Some(row) = db.session_agent(session)?
                && matches!(row.provider, Provider::Agent(_))
                && row.speaks_for(agent)
            {
                db.retire_agent(&row.id)?;
            }
            Ok(())
        })
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

    /// Lists the box's agents. Unless `include_finished`, exited and done agents are left out,
    /// except ancestors of an agent that is shown, so the tree stays connected.
    pub fn list_agents(
        &self,
        selection: BoxSelection,
        include_finished: bool,
    ) -> Result<Vec<ManagedAgent>> {
        let agents = self
            .store
            .read(|db| db.list_agents(self.resolve_box(db, &selection)?.as_deref()))?;
        Ok(if include_finished {
            agents
        } else {
            live_agents(agents)
        })
    }

    /// Lists the box's runs, live ones first. Unless `include_finished`, finished runs (see
    /// `RunStatus::is_finished`) are left out.
    pub fn list_runs(&self, selection: BoxSelection, include_finished: bool) -> Result<Vec<Run>> {
        let mut runs = self.store.read(|db| {
            let mut runs = db.list_runs(self.resolve_box(db, &selection)?.as_deref())?;
            for run in &mut runs {
                run.derive_task_states();
            }
            runs.retain(|run| include_finished || !run.status().is_finished());
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
                archived_at: None,
                tasks: Vec::new(),
            };
            db.insert_run(&run)?;
            db.event(&run.id, RunEventKind::RunCreated, None, None, None)?;
            Ok(run)
        })
    }

    /// Archives the run so default listings leave it out. A run with open attempts is refused
    /// unless `force`, so live workers are not hidden by accident. Archiving an archived run
    /// keeps its original archive time.
    pub fn archive_run(&self, id: &str, force: bool) -> Result<Run> {
        self.write(|db| {
            let mut run = load_run(db, id)?;
            if run.archived_at.is_some() {
                return Ok(run);
            }
            let open = run.open_attempts().count();
            ensure!(
                force || open == 0,
                "Run {id:?} has {open} open attempt{}. Finish {} first, or archive with --force",
                if open == 1 { "" } else { "s" },
                if open == 1 { "it" } else { "them" },
            );
            let archived_at = now();
            db.set_run_archived(id, Some(archived_at))?;
            db.event(id, RunEventKind::RunArchived, None, None, None)?;
            run.archived_at = Some(archived_at);
            Ok(run)
        })
    }

    /// Restores an archived run to listings. Unarchiving a run that is not archived is a no-op.
    pub fn unarchive_run(&self, id: &str) -> Result<Run> {
        self.write(|db| {
            let mut run = load_run(db, id)?;
            if run.archived_at.is_none() {
                return Ok(run);
            }
            db.set_run_archived(id, None)?;
            db.event(id, RunEventKind::RunUnarchived, None, None, None)?;
            run.archived_at = None;
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
        let entry = self.registered_box(&box_id)?;
        let caller = self.hosted_caller(command.selection.caller.as_deref());
        let (worktree, new_branch) = match &command.worktree {
            WorktreeRequest::Existing {
                path,
                allow_main_checkout,
            } => (
                self.existing_worktree(&entry.info.repository, path, *allow_main_checkout)?,
                None,
            ),
            WorktreeRequest::New { branch, base } => {
                let (path, base) =
                    self.plan_new_worktree(&entry.info, branch, base.as_deref(), caller)?;
                (path, Some((branch.clone(), base)))
            }
            WorktreeRequest::RetryManaged => {
                bail!("Specify --branch or --worktree when starting an agent")
            }
        };
        let mut created_path = None;
        let result = self.worker_transaction(|db, launched| {
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
            let mut agent = self.new_agent(
                box_id.clone(),
                worktree,
                command.provider,
                parent,
                command.label,
            );
            let created = match new_branch {
                Some((branch, base)) => {
                    let created = self.application.workspace.create_worktree(
                        &entry.info.repository,
                        &agent.worktree,
                        &branch,
                        &base,
                    )?;
                    created_path = Some(created.path.clone());
                    agent.worktree.clone_from(&created.path);
                    Some((branch, created))
                }
                None => None,
            };
            self.launch_worker(&mut agent, command.args, command.prompt, None, launched)?;
            db.insert_agent(&agent)?;
            if let Some((branch, created)) = created {
                // The agent's terminal session is the creator, as for a worktree made from a tab.
                let session = agent
                    .session
                    .clone()
                    .context("Started agent has no session")?;
                db.insert_worktree(&ManagedWorktree {
                    box_id: box_id.clone(),
                    repository: entry.info.repository.clone(),
                    path: created.path,
                    branch,
                    base_commit: created.base_commit,
                    created_at: now(),
                    creator: WorktreeCreator::View { box_id, session },
                })?;
            }
            Ok(agent)
        });
        if result.is_err()
            && let Some(path) = created_path
            && let Err(cleanup) = self.application.workspace.remove_worktree(&path, true)
        {
            return result
                .with_context(|| format!("Could not undo worktree creation: {cleanup:#}"));
        }
        result
    }

    /// Starts a worker for the task, or queues the dispatch when the worker budget is full,
    /// as `command.budget` says. The budget is checked in the same transaction that records
    /// the attempt, so concurrent dispatches cannot overshoot it.
    pub fn dispatch(&self, command: Dispatch) -> Result<DispatchOutcome> {
        let budget = match command.budget {
            BudgetMode::Override => None,
            BudgetMode::Queue | BudgetMode::Refuse => self.worker_budget()?,
        };
        self.dispatch_with(command, None, budget.as_ref())
    }

    /// `claim` starts the queued dispatch with that ID, and only while it is still the
    /// task's queued entry. With `BudgetMode::Queue`, a claimed entry starts only if the
    /// budget has room, and otherwise stays queued.
    fn dispatch_with(
        &self,
        command: Dispatch,
        claim: Option<&str>,
        budget: Option<&WorkerBudget>,
    ) -> Result<DispatchOutcome> {
        let mut created_path = None;
        let result = self.worker_transaction(|db, launched| {
            let run = load_run(db, &command.run)?;
            let task = find_task(&run, &command.task)?;
            let queued_entry = db.queued_dispatch(&run.id, &task.id)?;
            let queued = queued_entry.is_some();
            if let Some(claim) = claim
                && queued_entry.as_ref().is_none_or(|entry| entry.id != claim)
            {
                // Started, dropped or replaced since the queue was read.
                return Ok(Ok(DispatchOutcome::Queued {
                    reason: "it is no longer queued".into(),
                }));
            }
            let from_queue = claim.is_some() && command.budget == BudgetMode::Queue;
            if from_queue && run.archived_at.is_some() {
                // An archived run starts no workers until it is unarchived.
                return Ok(Ok(DispatchOutcome::Queued {
                    reason: "the run is archived".into(),
                }));
            }
            ensure!(
                !queued || claim.is_some() || command.budget == BudgetMode::Override,
                "Task {:?} is already queued for the worker budget. Dispatch it with \
                 --over-budget to start it now",
                task.id
            );
            let state = if queued {
                unqueued_state(&run, &task.id)
            } else {
                task.state
            };
            ensure!(
                can_dispatch(state, command.retry),
                "Task {:?} cannot be dispatched in state {:?}{}",
                task.id,
                state,
                if !command.retry && matches!(state, TaskState::Failed | TaskState::Interrupted) {
                    ". Use --retry to start another attempt"
                } else {
                    ""
                },
            );
            let limit = self.budget_limit(db, &run, budget)?;
            if from_queue && let Some(limit) = limit {
                return Ok(Ok(DispatchOutcome::Queued {
                    reason: limit.to_string(),
                }));
            }
            let number = task.attempts.last().map_or(Ok(1), |previous| {
                previous
                    .number
                    .checked_add(1)
                    .context("Too many attempts for this task")
            })?;
            let entry = self.registered_box(&run.box_id)?;
            let caller = self.hosted_caller(command.caller.as_deref());
            let DispatchWorktree { path, kind } =
                self.dispatch_worktree(db, &entry.info, task, &command, caller)?;
            if let Some(limit) = limit {
                ensure!(
                    command.budget == BudgetMode::Queue,
                    "The worker budget is full: {limit}. Wait for a worker to report, or \
                     dispatch with --over-budget to start anyway"
                );
                let reason = limit.to_string();
                let worktree = match kind {
                    // Pin the base to its commit, so the dispatch starts where it was asked to
                    // even if the ref moves while it waits. The implicit base is already one.
                    DispatchWorktreeKind::New { branch, base } => WorktreeRequest::New {
                        base: Some(
                            self.application
                                .workspace
                                .resolve_commit(&entry.info.repository, &base)
                                .with_context(|| format!("Resolve --base {base:?}"))?,
                        ),
                        branch,
                    },
                    DispatchWorktreeKind::Existing | DispatchWorktreeKind::Resume(_) => {
                        command.worktree
                    }
                };
                db.insert_queued(&QueuedDispatch {
                    id: self.new_id(),
                    run: run.id.clone(),
                    task: task.id.clone(),
                    provider: command.provider,
                    worktree,
                    retry: command.retry,
                    args: command.args,
                    caller: caller.map(str::to_owned),
                    queued_at: now(),
                })?;
                db.event(
                    &run.id,
                    RunEventKind::TaskQueued,
                    Some(&task.id),
                    None,
                    Some(&reason),
                )?;
                return Ok(Ok(DispatchOutcome::Queued { reason }));
            }
            if let Some(queued_entry) = &queued_entry {
                db.remove_queued(&queued_entry.id)?;
            }
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
                command.provider,
                parent,
                task.title.clone(),
            );
            agent.run = Some(target.run.clone());
            agent.task = Some(target.task.clone());
            agent.attempt = Some(target.id.clone());
            agent.status = AgentStatus::Working;
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
                self.launch_worker(
                    &mut agent,
                    command.args,
                    prompt,
                    Some(&task.title),
                    launched,
                )?;
                Ok(branch)
            })();
            let start = match start {
                Err(error) if !provisioning => return Err(error),
                Err(error) => {
                    agent.state = AgentState::Exited;
                    agent.status = AgentStatus::Exited;
                    Err(error)
                }
                success => success,
            };
            let attempt = insert_dispatch_attempt(db, &agent, target, start.as_ref().err())?;
            if let Some(ownership) = ownership {
                db.insert_worktree(&ownership)?;
            }
            // Commit the failed attempt, if any, before returning the error to the caller.
            Ok(start.map(|branch| {
                DispatchOutcome::Started(Box::new(DispatchResult {
                    worktree: attempt.worktree.clone(),
                    agent,
                    attempt,
                    branch,
                }))
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
    ) -> Result<ReportResult>
    where
        T: 'static,
    {
        let (result, finished) =
            self.write(|db| report(db, run, task, attempt, status, summary))?;
        if let (Some(session), Some(cleanup)) = (finished, &self.finished_cleanup) {
            self.end_finished_session(session, cleanup);
        }
        Ok(result)
    }

    /// Ends a finished worker's session after the grace. Ending takes everything the
    /// worker started down. Its record, which is how a restart finds what may still
    /// run, is forgotten only once the session's exit confirms that all of it is gone
    /// (see `session_exited`). The due teardown holds the terminal handle taken now, so
    /// it takes no lock a launch holds across a store write and never writes a store
    /// itself. A scheduler that refuses the teardown leaves the work here, so the
    /// session ends now.
    fn end_finished_session(&self, session: String, cleanup: &FinishedCleanup)
    where
        T: 'static,
    {
        let Ok(terminal) = self.application.terminal(&session) else {
            return;
        };
        let application = self.application.clone();
        let scheduled = session.clone();
        let handle = terminal.clone();
        let job = Box::new(move || {
            if let Err(error) = application.stop_terminal(&handle) {
                eprintln!("Could not end finished worker session {scheduled}. {error:#}");
            }
        });
        if let Err(error) = cleanup.teardown.schedule(cleanup.grace, job) {
            eprintln!(
                "Could not schedule the end of finished worker session {session}, so it ends now. {error:#}"
            );
            if let Err(error) = self.application.stop_terminal(&terminal) {
                eprintln!("Could not end finished worker session {session}. {error:#}");
            }
        }
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

    /// Records that a session's process exited. `complete` says whether everything it
    /// started has exited too. Only then is its agent exited, and a finished worker's
    /// record forgotten so a restart cannot bring it back. Otherwise the agent stays
    /// running and the record stays, so a later reclaim can end what remains.
    pub fn session_exited(&self, session: &str, detail: &str, complete: bool) -> Result<()>
    where
        T: 'static,
        S: 'static,
        B: 'static,
        I: 'static,
    {
        let finished = self.write(|db| {
            let Some(agent) = db.session_agent(session)? else {
                return Ok(false);
            };
            interrupt(db, Some(session), detail, &|_| false)?;
            if complete {
                db.set_agent_exited(&agent.id)?;
            }
            Ok(agent.attempt.is_some())
        })?;
        if !complete {
            eprintln!("Session {session} left processes running, so it stays for a reclaim.");
            // The exited terminal no longer owns anything, so the retry must not
            // mistake its handle for a live one. The record stays for a restart.
            self.application.retire_terminal(session);
            self.schedule_reclaim(session.to_owned());
            return Ok(());
        }
        if finished && self.finished_cleanup.is_some() {
            self.forget_ended(session.to_owned());
        }
        Ok(())
    }

    /// Forgets an ended worker's record off the caller's thread, which may be a
    /// terminal's, so a slow session store never holds it.
    fn forget_ended(&self, session: String)
    where
        T: 'static,
        S: 'static,
        B: 'static,
        I: 'static,
    {
        let application = self.application.clone();
        let forget = Box::new(move || {
            if let Err(error) = application.forget_session(&session) {
                eprintln!("Could not forget ended session {session}. {error:#}");
            }
        });
        let refused = match &self.finished_cleanup {
            Some(cleanup) => cleanup.persistence.schedule(Duration::ZERO, forget).err(),
            None => Some(anyhow::anyhow!("no cleanup")),
        };
        if let Some(error) = refused {
            // The attempt is finished, so a restart forgets the record anyway.
            eprintln!("Could not schedule forgetting an ended session. {error:#}");
        }
    }

    /// Retries ending what still runs of `session` until it is gone, then exits its
    /// agent and forgets its record if its attempt is over. Only a live daemon with
    /// cleanup enabled retries. Otherwise the next start does.
    fn schedule_reclaim(&self, session: String)
    where
        T: 'static,
        S: 'static,
        B: 'static,
        I: 'static,
    {
        let Some(cleanup) = &self.finished_cleanup else {
            return;
        };
        let Some(dag) = self.weak.lock().unwrap().upgrade() else {
            return;
        };
        let teardown = cleanup.teardown.clone();
        // The timer only starts the retry. The reclaim can wait seconds for a stuck
        // scope and the record write can wait on the store, so both run on a thread
        // of their own and never delay a due teardown of another worker.
        let job = Box::new(move || {
            let retry = dag.clone();
            let retried = session.clone();
            let spawned = std::thread::Builder::new()
                .name("power-reclaim".into())
                .spawn(move || retry.retry_reclaim(retried));
            if let Err(error) = spawned {
                eprintln!("Could not start a reclaim of session {session}. {error}");
                dag.schedule_reclaim(session);
            }
        });
        if let Err(error) = teardown.schedule(RECLAIM_RETRY, job) {
            eprintln!("Could not schedule another reclaim. {error:#}");
        }
    }

    fn retry_reclaim(&self, session: String)
    where
        T: 'static,
        S: 'static,
        B: 'static,
        I: 'static,
    {
        if self.application.terminal_running(&session) {
            // A live terminal owns the session again.
            return;
        }
        match self.application.reclaim(&session) {
            Ok(()) => {
                if let Err(error) = self.reclaimed(&session) {
                    eprintln!("Could not record the end of session {session}. {error:#}");
                }
            }
            Err(error) => {
                eprintln!("Session {session} still has processes running. {error:#}");
                self.schedule_reclaim(session);
            }
        }
    }

    /// Records that nothing of `session` runs any more.
    fn reclaimed(&self, session: &str) -> Result<()> {
        let finished = self.write(|db| {
            let Some(agent) = db.session_agent(session)? else {
                return Ok(true);
            };
            db.set_agent_exited(&agent.id)?;
            Ok(db.open_attempts(Some(session))?.is_empty())
        })?;
        if finished {
            self.application.forget_session(session)?;
        }
        Ok(())
    }

    /// Restores saved sessions on daemon start, then reconciles attempts with them.
    /// A resumed conversation waits for input, so a worker whose attempt is still active
    /// is told to continue, and its run records `AttemptResumed`. Workers waiting on the
    /// user, coordinators and plain sessions resume without a prompt. A worker whose
    /// attempt already succeeded, failed or was interrupted is forgotten, not restored.
    pub fn restore(&self) -> Result<Vec<String>>
    where
        T: 'static,
        S: 'static,
        B: 'static,
        I: 'static,
    {
        let (open, finished): (BTreeMap<String, OpenAttempt>, BTreeSet<String>) =
            self.store.read(|db| {
                let open: BTreeMap<_, _> = db
                    .open_attempts(None)?
                    .into_iter()
                    .filter_map(|open| Some((open.attempt.session.clone()?, open)))
                    .collect();
                // A worker whose attempt is no longer open has nothing left to do.
                let finished = db
                    .list_agents(None)?
                    .into_iter()
                    .filter(|agent| agent.attempt.is_some())
                    .filter_map(|agent| agent.session)
                    .filter(|session| !open.contains_key(session))
                    .collect();
                Ok((open, finished))
            })?;
        let active: BTreeMap<String, OpenAttempt> = open
            .into_iter()
            .filter(|(_, open)| open.attempt.state == AttemptState::Active)
            .collect();
        let restore = self.application.restore_sessions_deferring(|session| {
            if finished.contains(session) {
                RestoreChoice::Forget("its DAG attempt already finished")
            } else {
                RestoreChoice::Resume(active.get(session).map(continuation_prompt))
            }
        })?;
        let (restored, deferred) = (restore.restored, restore.deferred);
        let resumed: Vec<_> = restored
            .iter()
            .filter(|session| session.prompted)
            .filter_map(|session| active.get(&session.id))
            .collect();
        let restored: Vec<_> = restored.into_iter().map(|session| session.id).collect();
        self.reconcile_with(&restored, &deferred, &resumed)?;
        for session in deferred {
            self.schedule_reclaim(session);
        }
        Ok(restored)
    }

    /// Interrupts attempts and stops agents whose session did not survive a restart.
    /// Those in `restored` keep running under their original session IDs, unless their
    /// worktree is gone and they have no open attempt.
    pub fn reconcile(&self, restored: &[String]) -> Result<()> {
        self.reconcile_restored(restored, &[])
    }

    /// Reconciles like `reconcile`, and records `AttemptResumed` for each attempt in
    /// `resumed`, whose worker was told to continue.
    fn reconcile_restored(&self, restored: &[String], resumed: &[&OpenAttempt]) -> Result<()> {
        self.reconcile_with(restored, &[], resumed)
    }

    /// Reconciles like `reconcile_restored`. An agent of a session in `running`, whose
    /// earlier processes could not be ended, stays running until a reclaim ends them,
    /// though its open attempt is interrupted since no daemon follows it.
    fn reconcile_with(
        &self,
        restored: &[String],
        running: &[String],
        resumed: &[&OpenAttempt],
    ) -> Result<()> {
        let restored_ids = restored;
        let restored = |session: Option<&str>| {
            session.is_some_and(|session| restored.iter().any(|id| id == session))
        };
        let workspace = self.application.workspace.as_ref();
        // A coding agent Power found in a restored shell's foreground ran in a process that
        // the restart ended. Its identity ends with it; a resumed agent registers anew.
        let shells: BTreeSet<&str> = restored_ids
            .iter()
            .map(String::as_str)
            .filter(|id| self.application.launched_agent(id) == Some(Agent::Shell))
            .collect();
        self.write(|db| {
            interrupt(db, None, RESTART_DETAIL, &restored)?;
            for open in resumed {
                db.event(
                    &open.run,
                    RunEventKind::AttemptResumed,
                    Some(&open.task),
                    Some(&open.attempt.id),
                    Some(RESUME_DETAIL),
                )?;
            }
            let attempting: BTreeSet<_> = db
                .open_attempts(None)?
                .into_iter()
                .map(|open| open.attempt.agent)
                .collect();
            for agent in db.list_agents(None)? {
                if agent.state != AgentState::Running {
                    continue;
                }
                if matches!(agent.provider, Provider::Agent(_))
                    && agent.task.is_none()
                    && agent
                        .session
                        .as_deref()
                        .is_some_and(|session| shells.contains(session))
                {
                    db.retire_agent(&agent.id)?;
                    continue;
                }
                let orphaned =
                    !attempting.contains(&agent.id) && !workspace.worktree_exists(&agent.worktree);
                let still_running = agent
                    .session
                    .as_deref()
                    .is_some_and(|session| running.iter().any(|id| id == session));
                if still_running {
                    continue;
                }
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

    fn worktree_box(&self, worktree: &Path) -> Result<String> {
        let mut boxes = self.registry.boxes()?.into_iter().filter(|entry| {
            self.application
                .workspace
                .validate_box_worktree(&entry.info.repository, worktree)
                .is_ok()
        });
        match (boxes.next(), boxes.next()) {
            (Some(entry), None) => Ok(entry.info.id),
            (Some(_), Some(_)) => bail!("{} belongs to several boxes", worktree.display()),
            (None, _) => bail!("{} does not belong to a Power box", worktree.display()),
        }
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
        let agent = self.application.session(session)?.agent;
        self.insert_session_agent(db, session, box_id, &agent)
    }

    fn insert_session_agent(
        &self,
        db: &dyn DagTransaction,
        session: &str,
        box_id: &str,
        live: &Agent,
    ) -> Result<ManagedAgent> {
        let mut session = self.application.session(session)?;
        session.agent = live.clone();
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

    /// Validates an existing worktree of the box, refusing the main checkout unless allowed.
    fn existing_worktree(
        &self,
        repository: &Path,
        path: &Path,
        allow_main_checkout: bool,
    ) -> Result<PathBuf> {
        let workspace = &self.application.workspace;
        let worktree = workspace.validate_box_worktree(repository, path)?;
        if !allow_main_checkout
            && workspace.main_checkout(repository)?.as_deref() == Some(worktree.as_path())
        {
            bail!(
                "{} is the repository's main checkout. Pass --branch <name> to give the agent \
                 its own worktree, or --allow-main-checkout for read-only work",
                worktree.display()
            );
        }
        Ok(worktree)
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
            WorktreeRequest::Existing {
                path,
                allow_main_checkout,
            } => Ok(DispatchWorktree {
                path: self.existing_worktree(repository, path, *allow_main_checkout)?,
                kind: DispatchWorktreeKind::Existing,
            }),
            WorktreeRequest::RetryManaged => {
                ensure!(
                    command.retry,
                    "Specify --branch or --worktree when starting a task"
                );
                let previous = task
                    .attempts
                    .last()
                    .context("No earlier attempt to retry")?;
                let worktree = db
                    .find_worktree_by_path(&previous.worktree)?
                    .filter(|worktree| worktree.box_id == box_info.id)
                    .context("Specify --branch or --worktree to retry an attempt without a managed worktree")?;
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
                let (path, base) =
                    self.plan_new_worktree(box_info, branch, base.as_deref(), caller)?;
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

    /// Checks a new branch name and chooses the worktree path and starting commit. Without
    /// `base`, the branch starts at the Power caller's HEAD.
    fn plan_new_worktree(
        &self,
        box_info: &BoxInfo,
        branch: &str,
        base: Option<&str>,
        caller: Option<&str>,
    ) -> Result<(PathBuf, String)> {
        let workspace = &self.application.workspace;
        let repository = &box_info.repository;
        ensure!(
            !branch.trim().is_empty(),
            "Specify a branch name with --branch"
        );
        ensure!(
            !matches!(branch, "." | ".."),
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
        // An explicit base stays as given here, so an invalid one fails in worktree creation
        // and records a failed attempt. A queued dispatch pins it when it is stored.
        let base = match base {
            Some(base) => base.to_owned(),
            None => {
                let caller =
                    caller.context("Specify --base when creating a worktree from outside Power")?;
                workspace.head(&self.application.session(caller)?.worktree)?
            }
        };
        let path = default_worktree_path(&root.worktree_root()?, &box_info.name, branch, |path| {
            path.symlink_metadata().is_ok()
        });
        Ok((path, base))
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
            status: AgentStatus::Idle,
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

/// The task's state as if its dispatch were not queued.
fn unqueued_state(run: &Run, task: &str) -> TaskState {
    let mut run = run.clone();
    for entry in &mut run.tasks {
        entry.queued_at = None;
    }
    run.derive_task_states();
    find_task(&run, task).map_or(TaskState::Waiting, |task| task.state)
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
const RESUME_DETAIL: &str = "daemon restarted; session resumed and told to continue";

/// What a worker whose session was resumed after a daemon restart is told.
fn continuation_prompt(open: &OpenAttempt) -> String {
    format!(
        "Power restarted and resumed this session. Continue your task (run {}, task {}) \
         from where you left off, and report through the worker contract when done: \
         `power skills get power-dag --reference worker-contract`.",
        open.run, open.task
    )
}

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
        })
        .collect()
}

/// Active runs first, then stalled, then completed; newest first within each group.
/// Keeps unfinished agents and every ancestor of one, in their original order.
fn live_agents(agents: Vec<ManagedAgent>) -> Vec<ManagedAgent> {
    let parents: BTreeMap<&str, &str> = agents
        .iter()
        .filter_map(|agent| Some((agent.id.as_str(), agent.parent.as_deref()?)))
        .collect();
    let mut keep = BTreeSet::new();
    for agent in agents.iter().filter(|agent| !agent.status.is_finished()) {
        let mut id = agent.id.as_str();
        // Stop at an ancestor already kept; this also ends any parent cycle.
        while keep.insert(id.to_owned()) {
            match parents.get(id) {
                Some(parent) => id = parent,
                None => break,
            }
        }
    }
    agents
        .into_iter()
        .filter(|agent| keep.contains(&agent.id))
        .collect()
}

fn sort_runs(runs: &mut [Run]) {
    let rank = |status| match status {
        RunStatus::Active => 0,
        RunStatus::Stalled => 1,
        RunStatus::Completed => 2,
        RunStatus::Archived => 3,
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

/// How long a session that still has processes waits before the next reclaim.
const RECLAIM_RETRY: Duration = Duration::from_secs(5);

fn report(
    db: &dyn DagTransaction,
    run_id: &str,
    task_id: &str,
    attempt_id: &str,
    status: ReportStatus,
    summary: &str,
) -> Result<(ReportResult, Option<String>)> {
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
            return Ok((
                ReportResult {
                    applied: false,
                    reason: Some("stale attempt".into()),
                },
                None,
            ));
        }
        ReportTransition::AlreadyReported => {
            // A replayed terminal report asks for the cleanup again, so a cleanup
            // lost after the first report still happens.
            let finished = matches!(
                attempt.state,
                AttemptState::Succeeded | AttemptState::Failed
            )
            .then(|| attempt.session.clone())
            .flatten();
            return Ok((
                ReportResult {
                    applied: false,
                    reason: Some("already reported".into()),
                },
                finished,
            ));
        }
        ReportTransition::Unchanged => {
            return Ok((
                ReportResult {
                    applied: true,
                    reason: None,
                },
                None,
            ));
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
    let finished = matches!(status, ReportStatus::Success | ReportStatus::Failed)
        .then(|| attempt.session.clone())
        .flatten();
    Ok((
        ReportResult {
            applied: true,
            reason: None,
        },
        finished,
    ))
}

#[path = "worktrees.rs"]
mod worktrees;

#[cfg(test)]
mod tests;
