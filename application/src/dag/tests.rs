use super::*;
use crate::{
    ArtifactStore, BranchStatus, CreatedWorktree, MemorySessions, RecentArtifact, SessionArtifact,
    SessionStore, Workspace, WorktreeStatus, agents::AgentProfileRegistry,
};
use anyhow::bail;
use domain::{
    agent::{AgentProfile, Injection},
    artifact::{ArtifactRecord, ResolvedArtifact, Target},
    session::Session,
};
use std::{
    cell::RefCell,
    sync::{
        Barrier, Condvar,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

#[path = "../worktrees/tests.rs"]
mod worktrees;

#[derive(Clone, Default)]
struct Records {
    agents: Vec<ManagedAgent>,
    runs: Vec<Run>,
    events: Vec<RunEvent>,
    worktrees: Vec<ManagedWorktree>,
    queued: Vec<QueuedDispatch>,
    stopping: BTreeSet<String>,
    /// Queue writes fail while set, as a transiently failing database does.
    fail_queue_writes: bool,
}

#[derive(Default)]
struct MemoryStore(Mutex<Records>);

struct MemoryDb(RefCell<Records>);

impl MemoryDb {
    /// Derives the display status from the agent's attempt, as the SQLite store does.
    fn with_status(&self, mut agent: ManagedAgent) -> ManagedAgent {
        let attempt = self
            .0
            .borrow()
            .runs
            .iter()
            .flat_map(|run| &run.tasks)
            .flat_map(|task| &task.attempts)
            .filter(|attempt| attempt.agent == agent.id)
            .max_by_key(|attempt| (attempt.number, attempt.started_at))
            .map(|attempt| attempt.state);
        agent.status = AgentStatus::derive(agent.state, agent.paused, attempt);
        agent
    }
}

impl DagStore for MemoryStore {
    fn read<T>(&self, query: impl FnOnce(&dyn DagReader) -> Result<T>) -> Result<T> {
        let records = self.0.lock().unwrap();
        query(&MemoryDb(RefCell::new(records.clone())))
    }

    fn write<T>(&self, operation: impl FnOnce(&dyn DagTransaction) -> Result<T>) -> Result<T> {
        let mut records = self.0.lock().unwrap();
        let transaction = MemoryDb(RefCell::new(records.clone()));
        let value = operation(&transaction)?;
        *records = transaction.0.into_inner();
        Ok(value)
    }
}

impl DagReader for MemoryDb {
    fn list_worktrees(&self, box_id: Option<&str>) -> Result<Vec<ManagedWorktree>> {
        Ok(self
            .0
            .borrow()
            .worktrees
            .iter()
            .filter(|worktree| box_id.is_none_or(|id| worktree.box_id == id))
            .cloned()
            .collect())
    }

    fn find_worktree_by_attempt(&self, attempt: &str) -> Result<Option<ManagedWorktree>> {
        Ok(self.0.borrow().worktrees.iter().find(|worktree| {
            matches!(&worktree.creator, WorktreeCreator::Attempt { attempt: id, .. } if id == attempt)
        }).cloned())
    }

    fn find_worktree_by_path(&self, path: &Path) -> Result<Option<ManagedWorktree>> {
        Ok(self
            .0
            .borrow()
            .worktrees
            .iter()
            .find(|worktree| worktree.path == path)
            .cloned())
    }

    fn agent(&self, id: &str) -> Result<ManagedAgent> {
        self.0
            .borrow()
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .cloned()
            .map(|agent| self.with_status(agent))
            .context("Unknown agent")
    }

    fn session_agent(&self, session: &str) -> Result<Option<ManagedAgent>> {
        Ok(self
            .0
            .borrow()
            .agents
            .iter()
            .find(|agent| agent.session.as_deref() == Some(session))
            .cloned()
            .map(|agent| self.with_status(agent)))
    }

    fn list_agents(&self, box_id: Option<&str>) -> Result<Vec<ManagedAgent>> {
        Ok(self
            .0
            .borrow()
            .agents
            .iter()
            .filter(|agent| box_id.is_none_or(|id| agent.box_id == id))
            .map(|agent| self.with_status(agent.clone()))
            .collect())
    }

    fn worktree_agents(&self, worktree: &Path) -> Result<Vec<ManagedAgent>> {
        Ok(self
            .0
            .borrow()
            .agents
            .iter()
            .filter(|agent| agent.worktree == worktree)
            .map(|agent| self.with_status(agent.clone()))
            .collect())
    }

    fn run(&self, id: &str) -> Result<Run> {
        let records = self.0.borrow();
        let mut run = records
            .runs
            .iter()
            .find(|run| run.id == id)
            .cloned()
            .context("Unknown run")?;
        // An attempt is paused when its agent is, as the SQLite store joins it.
        for attempt in run.tasks.iter_mut().flat_map(|task| &mut task.attempts) {
            attempt.paused = records
                .agents
                .iter()
                .any(|agent| agent.id == attempt.agent && agent.paused);
        }
        Ok(run)
    }

    fn run_box(&self, id: &str) -> Result<String> {
        Ok(self.run(id)?.box_id)
    }

    fn list_runs(&self, box_id: Option<&str>) -> Result<Vec<Run>> {
        Ok(self
            .0
            .borrow()
            .runs
            .iter()
            .filter(|run| box_id.is_none_or(|id| run.box_id == id))
            .cloned()
            .collect())
    }

    fn open_attempts(&self, session: Option<&str>) -> Result<Vec<OpenAttempt>> {
        let mut open = Vec::new();
        for run in &self.0.borrow().runs {
            for task in &run.tasks {
                for attempt in &task.attempts {
                    if attempt.state.is_open()
                        && session.is_none_or(|id| attempt.session.as_deref() == Some(id))
                    {
                        open.push(OpenAttempt {
                            run: run.id.clone(),
                            box_id: run.box_id.clone(),
                            task: task.id.clone(),
                            attempt: attempt.clone(),
                        });
                    }
                }
            }
        }
        Ok(open)
    }

    fn events(&self, scope: EventScope<'_>, since: u64) -> Result<Vec<RunEvent>> {
        Ok(self
            .0
            .borrow()
            .events
            .iter()
            .filter(|event| {
                event.seq > since
                    && match scope {
                        EventScope::Run(run, kinds) => {
                            event.run == run && (kinds.is_empty() || kinds.contains(&event.kind))
                        }
                        EventScope::Watch(box_id) => box_id.is_none_or(|id| event.box_id == id),
                    }
            })
            .cloned()
            .collect())
    }

    fn latest_seq(&self) -> Result<u64> {
        Ok(self.0.borrow().events.last().map_or(0, |event| event.seq))
    }

    fn queued_dispatches(&self) -> Result<Vec<QueuedDispatch>> {
        Ok(self.0.borrow().queued.clone())
    }

    fn stopping_sessions(&self) -> Result<BTreeSet<String>> {
        Ok(self.0.borrow().stopping.clone())
    }
}

impl MemoryDb {
    fn set_queued_at(&self, run: &str, task: &str, queued_at: Option<u64>) -> Result<()> {
        self.0
            .borrow_mut()
            .runs
            .iter_mut()
            .find(|entry| entry.id == run)
            .context("Unknown run")?
            .tasks
            .iter_mut()
            .find(|entry| entry.id == task)
            .context("Unknown task")?
            .queued_at = queued_at;
        Ok(())
    }
}

impl DagTransaction for MemoryDb {
    fn insert_queued(&self, queued: &QueuedDispatch) -> Result<()> {
        ensure!(!self.0.borrow().fail_queue_writes, "database is locked");
        ensure!(
            !self
                .0
                .borrow()
                .queued
                .iter()
                .any(
                    |entry| (entry.run == queued.run && entry.task == queued.task)
                        || entry.id == queued.id
                ),
            "Duplicate queued dispatch"
        );
        self.set_queued_at(&queued.run, &queued.task, Some(queued.queued_at))?;
        self.0.borrow_mut().queued.push(queued.clone());
        Ok(())
    }

    fn remove_queued(&self, id: &str) -> Result<bool> {
        let position = self
            .0
            .borrow()
            .queued
            .iter()
            .position(|entry| entry.id == id);
        let Some(position) = position else {
            return Ok(false);
        };
        let removed = self.0.borrow_mut().queued.remove(position);
        self.set_queued_at(&removed.run, &removed.task, None)?;
        Ok(true)
    }

    fn insert_worktree(&self, worktree: &ManagedWorktree) -> Result<()> {
        ensure!(
            self.find_worktree_by_path(&worktree.path)?.is_none(),
            "Duplicate worktree"
        );
        if let WorktreeCreator::Attempt { run, task, attempt } = &worktree.creator {
            let run = self.run(run)?;
            ensure!(
                find_task(&run, task)?
                    .attempts
                    .iter()
                    .any(|entry| &entry.id == attempt),
                "Worktree creator attempt must be recorded first"
            );
        }
        self.0.borrow_mut().worktrees.push(worktree.clone());
        Ok(())
    }

    fn remove_worktree(&self, path: &Path) -> Result<()> {
        self.0
            .borrow_mut()
            .worktrees
            .retain(|record| record.path != path);
        Ok(())
    }

    fn insert_agent(&self, agent: &ManagedAgent) -> Result<()> {
        self.0.borrow_mut().agents.push(agent.clone());
        Ok(())
    }

    fn set_agent_exited(&self, id: &str) -> Result<()> {
        self.0
            .borrow_mut()
            .agents
            .iter_mut()
            .find(|agent| agent.id == id)
            .context("Unknown agent")?
            .state = AgentState::Exited;
        Ok(())
    }

    fn retire_agent(&self, id: &str) -> Result<()> {
        let mut db = self.0.borrow_mut();
        let agent = db
            .agents
            .iter_mut()
            .find(|agent| agent.id == id)
            .context("Unknown agent")?;
        agent.state = AgentState::Exited;
        agent.session = None;
        Ok(())
    }

    fn insert_stopping(&self, session: &str) -> Result<bool> {
        Ok(self.0.borrow_mut().stopping.insert(session.to_owned()))
    }

    fn remove_stopping(&self, session: &str) -> Result<bool> {
        Ok(self.0.borrow_mut().stopping.remove(session))
    }

    fn set_agent_paused(&self, id: &str, paused: bool) -> Result<()> {
        self.0
            .borrow_mut()
            .agents
            .iter_mut()
            .find(|agent| agent.id == id)
            .context("Unknown agent")?
            .paused = paused;
        Ok(())
    }

    fn set_run_paused(&self, id: &str, paused_at: Option<u64>) -> Result<()> {
        self.0
            .borrow_mut()
            .runs
            .iter_mut()
            .find(|run| run.id == id)
            .context("Unknown run")?
            .paused_at = paused_at;
        Ok(())
    }

    fn adopt_coordinated_runs(&self, _session: &str, _agent: &str) -> Result<()> {
        Ok(())
    }

    fn set_run_archived(&self, id: &str, archived_at: Option<u64>) -> Result<()> {
        self.0
            .borrow_mut()
            .runs
            .iter_mut()
            .find(|run| run.id == id)
            .context("Unknown run")?
            .archived_at = archived_at;
        Ok(())
    }

    fn set_agent_model(&self, id: &str, model: Option<&str>) -> Result<()> {
        let mut db = self.0.borrow_mut();
        let agent = db
            .agents
            .iter_mut()
            .find(|agent| agent.id == id)
            .context("Unknown agent")?;
        agent.model = model.map(str::to_owned);
        Ok(())
    }

    fn insert_run(&self, run: &Run) -> Result<()> {
        self.0.borrow_mut().runs.push(run.clone());
        Ok(())
    }

    fn insert_tasks(&self, run: &str, tasks: &[TaskSpec]) -> Result<()> {
        self.0
            .borrow_mut()
            .runs
            .iter_mut()
            .find(|entry| entry.id == run)
            .context("Unknown run")?
            .tasks
            .extend(tasks.iter().map(|task| Task {
                id: task.id.clone(),
                title: task.title.clone(),
                instructions: task.instructions.clone(),
                deps: task.deps.clone(),
                provider: task.provider.clone(),
                model: task.model.clone(),
                state: TaskState::Waiting,
                queued_at: None,
                current_attempt: None,
                attempts: Vec::new(),
            }));
        Ok(())
    }

    fn set_task_plan(
        &self,
        run: &str,
        task: &str,
        provider: Option<&Provider>,
        model: Option<&str>,
    ) -> Result<()> {
        let mut records = self.0.borrow_mut();
        let task = records
            .runs
            .iter_mut()
            .find(|entry| entry.id == run)
            .context("Unknown run")?
            .tasks
            .iter_mut()
            .find(|entry| entry.id == task)
            .context("Unknown task")?;
        task.provider = provider.cloned();
        task.model = model.map(str::to_owned);
        Ok(())
    }

    fn insert_dep(&self, run: &str, task: &str, dep: &str) -> Result<()> {
        let mut records = self.0.borrow_mut();
        let deps = &mut records
            .runs
            .iter_mut()
            .find(|entry| entry.id == run)
            .context("Unknown run")?
            .tasks
            .iter_mut()
            .find(|entry| entry.id == task)
            .context("Unknown task")?
            .deps;
        if !deps.iter().any(|id| id == dep) {
            deps.push(dep.into());
        }
        Ok(())
    }

    fn update_task(&self, run: &str, task: &str, title: &str, instructions: &str) -> Result<()> {
        let mut records = self.0.borrow_mut();
        let entry = records
            .runs
            .iter_mut()
            .find(|entry| entry.id == run)
            .context("Unknown run")?
            .tasks
            .iter_mut()
            .find(|entry| entry.id == task)
            .context("Unknown task")?;
        entry.title = title.into();
        entry.instructions = instructions.into();
        Ok(())
    }

    fn insert_attempt(&self, run: &str, task: &str, attempt: &Attempt) -> Result<()> {
        self.0
            .borrow_mut()
            .runs
            .iter_mut()
            .find(|entry| entry.id == run)
            .context("Unknown run")?
            .tasks
            .iter_mut()
            .find(|entry| entry.id == task)
            .context("Unknown task")?
            .attempts
            .push(Attempt {
                summary: None,
                blocker: None,
                detail: None,
                ended_at: None,
                ..attempt.clone()
            });
        Ok(())
    }

    fn update_attempt(&self, attempt: &Attempt) -> Result<()> {
        let mut records = self.0.borrow_mut();
        let current = records
            .runs
            .iter_mut()
            .flat_map(|run| &mut run.tasks)
            .flat_map(|task| &mut task.attempts)
            .find(|current| current.id == attempt.id)
            .context("Unknown attempt")?;
        *current = attempt.clone();
        Ok(())
    }

    fn event(
        &self,
        run: &str,
        kind: RunEventKind,
        task: Option<&str>,
        attempt: Option<&str>,
        summary: Option<&str>,
    ) -> Result<()> {
        let box_id = self.run_box(run)?;
        let seq = self.latest_seq()? + 1;
        self.0.borrow_mut().events.push(RunEvent {
            seq,
            run: run.into(),
            box_id,
            kind,
            task: task.map(str::to_owned),
            attempt: attempt.map(str::to_owned),
            summary: summary.map(str::to_owned),
            at: now(),
        });
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeWorkspace(Arc<Mutex<FakeWorktrees>>);

#[derive(Default)]
struct FakeWorktrees {
    branches: BTreeMap<String, Option<PathBuf>>,
    head_lookups: Vec<PathBuf>,
    creation_requests: Vec<(PathBuf, String, String)>,
    /// Commits that refs resolve to. `main`, `caller-head`, existing branches and `commits`
    /// resolve to themselves. Anything else is missing, as it would be in Git.
    refs: BTreeMap<String, String>,
    commits: BTreeSet<String>,
    missing: BTreeSet<PathBuf>,
    removed: Vec<PathBuf>,
}

impl FakeWorktrees {
    fn resolve(&self, commitish: &str) -> Result<String> {
        if let Some(commit) = self.refs.get(commitish) {
            return Ok(commit.clone());
        }
        ensure!(
            matches!(commitish, "main" | "caller-head")
                || self.branches.contains_key(commitish)
                || self.commits.contains(commitish),
            "fatal: invalid reference: {commitish}"
        );
        Ok(commitish.into())
    }
}

impl Workspace for FakeWorkspace {
    fn open_worktree(&self, directory: &Path) -> Result<PathBuf> {
        Ok(directory.to_owned())
    }

    fn list_worktrees(&self, directory: &Path) -> Result<Vec<PathBuf>> {
        Ok(vec![directory.to_owned()])
    }

    fn validate_box_worktree(&self, repository: &Path, worktree: &Path) -> Result<PathBuf> {
        let worktrees = self.0.lock().unwrap();
        ensure!(
            repository == Path::new("/repo")
                && (worktree == Path::new("/worktree")
                    || worktree == Path::new("/main")
                    || worktrees
                        .branches
                        .values()
                        .flatten()
                        .any(|path| path == worktree)),
            "Wrong worktree"
        );
        Ok(worktree.to_owned())
    }

    fn main_checkout(&self, repository: &Path) -> Result<Option<PathBuf>> {
        assert_eq!(repository, Path::new("/repo"));
        Ok(Some("/main".into()))
    }

    fn head(&self, worktree: &Path) -> Result<String> {
        self.0
            .lock()
            .unwrap()
            .head_lookups
            .push(worktree.to_owned());
        Ok("caller-head".into())
    }

    fn resolve_commit(&self, repository: &Path, commitish: &str) -> Result<String> {
        assert_eq!(repository, Path::new("/repo"));
        self.0.lock().unwrap().resolve(commitish)
    }

    fn branch_exists(&self, repository: &Path, branch: &str) -> Result<BranchStatus> {
        assert_eq!(repository, Path::new("/repo"));
        let worktrees = self.0.lock().unwrap();
        Ok(match worktrees.branches.get(branch) {
            Some(path) => BranchStatus::Exists {
                checked_out_at: path.clone(),
            },
            None => BranchStatus::Missing,
        })
    }

    fn create_worktree(
        &self,
        repository: &Path,
        path: &Path,
        branch: &str,
        base: &str,
    ) -> Result<CreatedWorktree> {
        assert_eq!(repository, Path::new("/repo"));
        let mut worktrees = self.0.lock().unwrap();
        worktrees
            .creation_requests
            .push((path.into(), branch.into(), base.into()));
        // Git resolves the base before it creates anything.
        worktrees.resolve(base)?;
        let path = Path::new("/canonical").join(path.strip_prefix("/")?);
        worktrees.branches.insert(branch.into(), Some(path.clone()));
        Ok(CreatedWorktree {
            path,
            base_commit: format!("resolved-{base}"),
        })
    }

    fn owns_worktree(&self, worktree: &ManagedWorktree) -> bool {
        self.0.lock().unwrap().branches.get(&worktree.branch) == Some(&Some(worktree.path.clone()))
    }

    fn worktree_status(&self, _path: &Path) -> Result<WorktreeStatus> {
        bail!("Unexpected worktree status lookup")
    }

    fn remove_worktree(&self, path: &Path, delete_branch: bool) -> Result<()> {
        assert!(delete_branch, "Undoing a creation also deletes its branch");
        self.0.lock().unwrap().removed.push(path.to_owned());
        Ok(())
    }

    fn worktree_exists(&self, path: &Path) -> bool {
        !self.0.lock().unwrap().missing.contains(path)
    }

    fn open_artifact_file(&self, _: &Path) -> Result<PathBuf> {
        bail!("Unexpected artifact operation")
    }
}

/// Records published artifacts in insertion order, which stands for publish order.
#[derive(Default)]
struct FakeArtifacts(Mutex<Vec<ArtifactRecord>>);

impl ArtifactStore for FakeArtifacts {
    fn insert(&self, record: ArtifactRecord) -> Result<()> {
        self.0.lock().unwrap().push(record);
        Ok(())
    }

    fn find(&self, _: &str) -> Result<ResolvedArtifact> {
        bail!("Unexpected artifact operation")
    }

    fn recent(&self, _: &[String], _: usize) -> Result<Vec<RecentArtifact>> {
        bail!("Unexpected artifact operation")
    }

    fn published(&self, sessions: &[String]) -> Result<Vec<SessionArtifact>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|record| sessions.contains(&record.session))
            .map(|record| SessionArtifact {
                session: record.session.clone(),
                artifact: AttemptArtifact {
                    id: record.id.clone(),
                    title: record.artifact.title.clone(),
                },
            })
            .collect())
    }
}

#[derive(Default)]
struct FakeTerminal {
    launches: Mutex<Vec<(Session, SessionOptions)>>,
    fail_start: AtomicBool,
    /// Reclaims fail while set, as when what an earlier daemon left cannot be ended.
    fail_reclaim: AtomicBool,
    /// Sessions whose reclaims fail, whatever `fail_reclaim` says.
    stuck: Mutex<BTreeSet<String>>,
    /// How long each reclaim takes, in milliseconds, as a scope slow to empty does.
    reclaim_millis: AtomicU64,
    /// Reclaims started so far.
    reclaims: AtomicU64,
    /// Sessions whose processes are paused, as the terminal applies them.
    paused: Mutex<BTreeSet<usize>>,
    /// Slows each pause or resume, in milliseconds, so concurrent transitions overlap.
    pause_millis: AtomicU64,
    /// The one process every live session owns, standing in for its worker.
    worker_pid: Mutex<Option<u32>>,
}

impl TerminalRuntime for FakeTerminal {
    type Handle = Arc<AtomicBool>;

    fn start(
        &self,
        session: &Session,
        options: SessionOptions,
        _: Option<&AgentProfile>,
    ) -> Result<Self::Handle> {
        ensure!(
            !self.fail_start.load(Ordering::Acquire),
            "Terminal startup failed"
        );
        self.launches
            .lock()
            .unwrap()
            .push((session.clone(), options));
        Ok(Arc::new(AtomicBool::new(true)))
    }

    fn is_running(&self, handle: &Self::Handle) -> bool {
        handle.load(Ordering::Acquire)
    }

    fn end(&self, handle: Self::Handle) -> Result<()> {
        handle.store(false, Ordering::Release);
        Ok(())
    }

    fn pause(&self, handle: &Self::Handle, paused: bool) -> Result<()> {
        thread::sleep(Duration::from_millis(
            self.pause_millis.load(Ordering::Acquire),
        ));
        let key = Arc::as_ptr(handle) as usize;
        let mut set = self.paused.lock().unwrap();
        if paused {
            set.insert(key);
        } else {
            set.remove(&key);
        }
        Ok(())
    }

    fn agent(&self, _: &Self::Handle) -> Option<Agent> {
        None
    }

    fn owns_process(&self, handle: &Self::Handle, pid: u32) -> bool {
        handle.load(Ordering::Acquire) && *self.worker_pid.lock().unwrap() == Some(pid)
    }

    fn reclaim(&self, session: &str) -> Result<()> {
        self.reclaims.fetch_add(1, Ordering::AcqRel);
        ensure!(
            !self.stuck.lock().unwrap().contains(session),
            "cgroup.kill: Device or resource busy"
        );
        thread::sleep(Duration::from_millis(
            self.reclaim_millis.load(Ordering::Acquire),
        ));
        ensure!(
            !self.fail_reclaim.load(Ordering::Acquire),
            "cgroup.kill: Read-only file system"
        );
        Ok(())
    }
}

#[derive(Default)]
struct FakeRegistry(Vec<String>);

impl BoxRegistry for FakeRegistry {
    fn boxes(&self) -> Result<Vec<RegisteredBox>> {
        Ok(vec![RegisteredBox {
            info: BoxInfo {
                id: "box-id".into(),
                name: "test-repo".into(),
                repository: "/repo".into(),
                checkout: "/worktree".into(),
                shortcut: Some(2),
                open: true,
            },
            sessions: self.0.clone(),
        }])
    }
}

/// Records icon changes as `(box_id, Some(path))` for a set and `(box_id, None)` for a reset.
#[derive(Default)]
struct FakeIcons(Mutex<Vec<(String, Option<PathBuf>)>>);

impl BoxIcons for FakeIcons {
    fn set(&self, box_id: &str, source: &Path) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push((box_id.into(), Some(source.into())));
        Ok(())
    }

    fn clear(&self, box_id: &str) -> Result<()> {
        self.0.lock().unwrap().push((box_id.into(), None));
        Ok(())
    }
}

type TestDag = DagApplication<FakeTerminal, MemoryStore, FakeRegistry, FakeIcons>;

impl TestDag {
    /// Dispatches and expects the worker to start.
    fn start(&self, command: Dispatch) -> Result<DispatchResult> {
        match self.dispatch(command)? {
            DispatchOutcome::Started(result) => Ok(*result),
            DispatchOutcome::Queued { reason } => bail!("Dispatch was queued: {reason}"),
        }
    }
}

struct FakeProfiles(Vec<AgentProfile>);

impl AgentProfileRegistry for FakeProfiles {
    fn profiles(&self) -> &[AgentProfile] {
        &self.0
    }
}

fn application() -> Arc<Application<FakeTerminal>> {
    application_with_workspace(FakeWorkspace::default())
}

fn application_with_workspace(workspace: FakeWorkspace) -> Arc<Application<FakeTerminal>> {
    application_with(workspace, Arc::new(MemorySessions::default()))
}

fn application_with(
    workspace: FakeWorkspace,
    records: Arc<dyn SessionStore>,
) -> Arc<Application<FakeTerminal>> {
    static ID: AtomicU64 = AtomicU64::new(0);
    let profiles = ["claude", "codex", "custom"]
        .into_iter()
        .map(|id| AgentProfile {
            id: id.into(),
            command: id.into(),
            injection: Injection::None,
            skip_subcommands: Vec::new(),
            skip_if_args_contain: Vec::new(),
            model_callback: domain::agent::ModelCallback::None,
            turn_hook: domain::agent::TurnHook::None,
            skills_dir: None,
            resume: Some(domain::agent::Resume {
                resume_last: vec!["--continue".into()],
                ..Default::default()
            }),
            env: Vec::new(),
            conversation_env: None,
            server_socket_env: None,
        })
        .collect();
    Arc::new(Application::new(
        FakeArtifacts::default(),
        records,
        workspace,
        FakeTerminal::default(),
        Arc::new(FakeProfiles(profiles)),
        || format!("id-{}", ID.fetch_add(1, Ordering::Relaxed)),
    ))
}

fn setup() -> Result<(TestDag, String)> {
    let dag = TestDag::new(
        application(),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    );
    let run = external_run(&dag)?;
    Ok((dag, run))
}

fn external_run(dag: &TestDag) -> Result<String> {
    let run = dag.create_run(
        BoxSelection {
            selector: Some("2".into()),
            caller: None,
        },
        "Fixture".into(),
        Some("external".into()),
    )?;
    dag.add_tasks(&run.id, &[spec("work", &[])])?;
    Ok(run.id)
}

fn spec(id: &str, deps: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        title: id.into(),
        instructions: format!("Implement {id}"),
        deps: deps.iter().map(|id| (*id).into()).collect(),
        provider: None,
        model: None,
    }
}

fn command(run: &str, retry: bool) -> Dispatch {
    Dispatch {
        run: run.into(),
        task: "work".into(),
        provider: Some(Provider::Command),
        worktree: WorktreeRequest::Existing {
            path: "/worktree".into(),
            allow_main_checkout: false,
        },
        retry,
        args: vec!["worker".into()],
        caller: None,
        budget: BudgetMode::Queue,
    }
}

struct FakeWorktreeRoot;

impl WorktreeRoot for FakeWorktreeRoot {
    fn worktree_root(&self) -> Result<PathBuf> {
        Ok("/managed-worktrees".into())
    }
}

fn managed_setup() -> (TestDag, FakeWorkspace) {
    let workspace = FakeWorkspace::default();
    let dag = TestDag::new(
        application_with_workspace(workspace.clone()),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .with_worktree_root(FakeWorktreeRoot);
    (dag, workspace)
}

fn new_worktree(run: &str, branch: &str, base: Option<&str>) -> Dispatch {
    Dispatch {
        worktree: WorktreeRequest::New {
            branch: branch.into(),
            base: base.map(str::to_owned),
        },
        provider: Some(Provider::Agent("codex".into())),
        args: Vec::new(),
        ..command(run, false)
    }
}

fn last_worker_prompt(dag: &TestDag) -> String {
    let launches = dag.application.runtime.launches.lock().unwrap();
    let (session, options) = launches.last().unwrap();
    let prompt = options.initial_prompt.as_ref().unwrap().to_str().unwrap();
    assert!(prompt.contains(&format!("Worktree: {}\n", session.worktree.display())));
    prompt.to_owned()
}

#[test]
fn new_dispatch_uses_caller_head_or_explicit_base_and_records_ownership() -> Result<()> {
    let (mut dag, workspace) = managed_setup();
    let caller = dag.application.start_session(StartSession {
        worktree: "/coordinator-worktree".into(),
        agent: Agent::Coding("codex".into()),
    })?;
    dag.registry.0.push(caller.id.clone());
    let run = dag.create_run(
        BoxSelection {
            selector: None,
            caller: Some(caller.id.clone()),
        },
        "Managed fixture".into(),
        None,
    )?;
    dag.add_tasks(&run.id, &[spec("work", &[]), spec("next", &["work"])])?;

    let first = dag.start(Dispatch {
        caller: Some(caller.id.clone()),
        ..new_worktree(&run.id, "feature/first", None)
    })?;
    assert_eq!(first.branch.as_deref(), Some("feature/first"));
    assert_eq!(first.worktree, first.attempt.worktree);
    assert_eq!(first.worktree, first.agent.worktree);
    assert_eq!(
        first.worktree,
        Path::new("/canonical/managed-worktrees/test-repo/feature-first")
    );
    assert!(last_worker_prompt(&dag).contains("Branch: feature/first\n"));
    let record = dag
        .store
        .read(|db| db.find_worktree_by_attempt(&first.attempt.id))?
        .context("Ownership record missing")?;
    assert_eq!(record.box_id, run.box_id);
    assert_eq!(record.repository, Path::new("/repo"));
    assert_eq!(record.path, first.worktree);
    assert_eq!(record.branch, "feature/first");
    assert_eq!(record.base_commit, "resolved-caller-head");
    assert_eq!(
        record.creator,
        WorktreeCreator::Attempt {
            run: run.id.clone(),
            task: "work".into(),
            attempt: first.attempt.id.clone(),
        }
    );

    dag.report(
        &run.id,
        "work",
        &first.attempt.id,
        ReportStatus::Success,
        "Done",
    )?;
    dag.start(Dispatch {
        task: "next".into(),
        caller: Some(caller.id),
        ..new_worktree(&run.id, "feature/next", Some("feature/first"))
    })?;
    let git = workspace.0.lock().unwrap();
    assert_eq!(git.head_lookups, [PathBuf::from("/coordinator-worktree")]);
    assert_eq!(
        git.creation_requests,
        [
            (
                "/managed-worktrees/test-repo/feature-first".into(),
                "feature/first".into(),
                "caller-head".into()
            ),
            (
                "/managed-worktrees/test-repo/feature-next".into(),
                "feature/next".into(),
                "feature/first".into()
            ),
        ]
    );
    Ok(())
}

#[test]
fn new_dispatch_rejects_missing_inputs_and_existing_branches_without_an_attempt() -> Result<()> {
    let (dag, workspace) = managed_setup();
    let run = external_run(&dag)?;
    {
        let mut git = workspace.0.lock().unwrap();
        git.branches
            .insert("in-use".into(), Some("/another-worktree".into()));
        git.branches.insert("local".into(), None);
    }
    let before = dag.latest_seq()?;
    for (dispatch, expected) in [
        (new_worktree(&run, "", Some("main")), "--branch"),
        (
            Dispatch {
                caller: Some("session-from-another-daemon".into()),
                ..new_worktree(&run, "fresh", None)
            },
            "--base",
        ),
        (
            new_worktree(&run, "in-use", Some("main")),
            "Branch \"in-use\" already exists and is checked out at /another-worktree",
        ),
        (
            new_worktree(&run, "local", Some("main")),
            "Branch \"local\" already exists",
        ),
    ] {
        let error = dag
            .start(dispatch)
            .err()
            .context("Dispatch unexpectedly succeeded")?;
        assert!(error.to_string().contains(expected), "{error:#}");
    }
    let snapshot = dag.run(&run)?;
    assert_eq!(snapshot.tasks[0].state, TaskState::Ready);
    assert!(snapshot.tasks[0].attempts.is_empty());
    assert_eq!(dag.latest_seq()?, before);
    assert!(dag.application.runtime.launches.lock().unwrap().is_empty());
    assert!(workspace.0.lock().unwrap().creation_requests.is_empty());
    Ok(())
}

#[test]
fn git_creation_error_persists_a_failed_attempt_without_launching() -> Result<()> {
    let (dag, workspace) = managed_setup();
    let run = external_run(&dag)?;
    let git_error = "fatal: invalid reference: missing-base";
    let before = dag.latest_seq()?;
    let error = dag
        .start(new_worktree(&run, "fresh", Some("missing-base")))
        .err()
        .context("Dispatch unexpectedly succeeded")?;
    assert_eq!(error.to_string(), git_error);
    // The missing base reached worktree creation rather than failing in planning.
    assert_eq!(workspace.0.lock().unwrap().creation_requests.len(), 1);
    let snapshot = dag.run(&run)?;
    let task = &snapshot.tasks[0];
    assert_eq!(task.state, TaskState::Failed);
    let [attempt] = task.attempts.as_slice() else {
        panic!("Expected one failed attempt, got {:?}", task.attempts);
    };
    assert_eq!(attempt.state, AttemptState::Failed);
    assert_eq!(attempt.summary.as_deref(), Some(git_error));
    assert!(attempt.ended_at.is_some());
    assert!(attempt.session.is_none());
    let agent = dag.store.read(|db| db.agent(&attempt.agent))?;
    assert_eq!(agent.state, AgentState::Exited);
    assert!(dag.application.runtime.launches.lock().unwrap().is_empty());
    assert!(dag.store.read(|db| db.list_worktrees(None))?.is_empty());
    let page = dag.events(EventScope::Run(&run, &[]), before, Duration::ZERO)?;
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, RunEventKind::AttemptFailed);
    assert_eq!(page.events[0].attempt.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(page.events[0].summary.as_deref(), Some(git_error));
    let retry = dag.start(command(&run, true))?;
    assert_eq!(retry.attempt.number, 2);
    assert_eq!(retry.worktree, Path::new("/worktree"));
    assert_eq!(retry.branch, None);
    Ok(())
}

#[test]
fn new_worktree_stays_owned_and_reusable_after_terminal_startup_fails() -> Result<()> {
    let (dag, workspace) = managed_setup();
    let run = external_run(&dag)?;
    dag.application
        .runtime
        .fail_start
        .store(true, Ordering::Release);
    let error = dag
        .start(new_worktree(&run, "fresh", Some("main")))
        .err()
        .context("Dispatch unexpectedly succeeded")?;
    assert_eq!(error.to_string(), "Terminal startup failed");
    let snapshot = dag.run(&run)?;
    let failed = &snapshot.tasks[0].attempts[0];
    assert_eq!(failed.state, AttemptState::Failed);
    let ownership = dag
        .store
        .read(|db| db.find_worktree_by_attempt(&failed.id))?
        .context("Ownership record missing")?;
    assert_eq!(ownership.path, failed.worktree);

    dag.application
        .runtime
        .fail_start
        .store(false, Ordering::Release);
    let retry = dag.start(Dispatch {
        worktree: WorktreeRequest::RetryManaged,
        ..command(&run, true)
    })?;
    assert_eq!(retry.worktree, ownership.path);
    assert_eq!(retry.branch.as_deref(), Some("fresh"));
    assert_eq!(workspace.0.lock().unwrap().creation_requests.len(), 1);
    Ok(())
}

#[test]
fn managed_retries_keep_reusing_the_worktree_until_a_new_one_is_requested() -> Result<()> {
    let (dag, workspace) = managed_setup();
    let run = external_run(&dag)?;
    let first = dag.start(new_worktree(&run, "feature/original", Some("main")))?;
    let mut previous = first.attempt.clone();
    for number in 2..=3 {
        dag.session_exited(previous.session.as_deref().unwrap(), "interrupted", true)?;
        let retry = dag.start(Dispatch {
            provider: Some(Provider::Agent("codex".into())),
            worktree: WorktreeRequest::RetryManaged,
            args: Vec::new(),
            ..command(&run, true)
        })?;
        assert_eq!(retry.attempt.number, number);
        assert_eq!(retry.worktree, first.worktree);
        assert_eq!(retry.branch, first.branch);
        let prompt = last_worker_prompt(&dag);
        assert!(prompt.contains("Branch: feature/original\n"));
        assert!(prompt.contains("resuming in the existing worktree"));
        previous = retry.attempt;
    }
    assert_eq!(workspace.0.lock().unwrap().creation_requests.len(), 1);

    dag.session_exited(previous.session.as_deref().unwrap(), "interrupted", true)?;
    let fresh = dag.start(Dispatch {
        retry: true,
        ..new_worktree(&run, "feature/fresh", Some("main"))
    })?;
    assert_eq!(fresh.attempt.number, 4);
    assert_ne!(fresh.worktree, first.worktree);
    assert_eq!(fresh.branch.as_deref(), Some("feature/fresh"));
    let prompt = last_worker_prompt(&dag);
    assert!(prompt.contains("Branch: feature/fresh\n"));
    assert!(!prompt.contains("resuming"));
    assert_eq!(workspace.0.lock().unwrap().creation_requests.len(), 2);
    let ownership = dag.store.read(|db| db.list_worktrees(None))?;
    assert_eq!(ownership.len(), 2);
    assert_eq!(ownership[0].path, first.worktree);
    assert_eq!(ownership[1].path, fresh.worktree);
    Ok(())
}

#[test]
fn concurrent_dispatch_launches_once() -> Result<()> {
    let (dag, run) = setup()?;
    let start = Barrier::new(2);
    let results = thread::scope(|scope| {
        let dispatch = || {
            start.wait();
            dag.start(command(&run, false))
        };
        let first = scope.spawn(dispatch);
        let second = scope.spawn(dispatch);
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(dag.application.runtime.launches.lock().unwrap().len(), 1);
    let task = &dag.run(&run)?.tasks[0];
    assert_eq!(task.state, TaskState::Working);
    assert_eq!(task.attempts.len(), 1);
    Ok(())
}

fn publish(dag: &TestDag, id: &str, session: &str, title: &str) -> Result<()> {
    dag.application.artifacts.insert(ArtifactRecord {
        id: id.into(),
        session: session.into(),
        worktree: "/worktree".into(),
        artifact: ResolvedArtifact {
            title: title.into(),
            target: Target::Loopback("http://localhost:3000/".into()),
        },
    })
}

#[test]
fn attempts_list_their_sessions_artifacts_in_publish_order() -> Result<()> {
    let (dag, run) = setup()?;
    let first = dag.start(command(&run, false))?;
    let first_session = first.agent.session.unwrap();
    dag.session_exited(&first_session, "worker exited", true)?;
    let second = dag.start(command(&run, true))?;
    let second_session = second.agent.session.unwrap();

    publish(&dag, "b-first", &first_session, "First report")?;
    publish(&dag, "x-other", "unrelated-session", "Elsewhere")?;
    publish(&dag, "c-retry", &second_session, "Retry screenshot")?;
    publish(&dag, "a-later", &first_session, "Late report")?;

    let artifact = |id: &str, title: &str| AttemptArtifact {
        id: id.into(),
        title: title.into(),
    };
    let expected = [
        vec![
            artifact("b-first", "First report"),
            artifact("a-later", "Late report"),
        ],
        vec![artifact("c-retry", "Retry screenshot")],
    ];
    let shown = dag.run(&run)?;
    let attempts: Vec<_> = shown.tasks[0]
        .attempts
        .iter()
        .map(|attempt| attempt.artifacts.clone())
        .collect();
    assert_eq!(attempts, expected);
    assert_eq!(
        shown.tasks[0].attempts[0].artifacts[0].link(),
        "pwr://artifact/b-first"
    );

    let listed = dag.list_runs(
        BoxSelection {
            selector: Some("2".into()),
            caller: None,
        },
        true,
    )?;
    let listed = listed.iter().find(|listed| listed.id == run).unwrap();
    assert_eq!(listed.tasks[0].attempts[0].artifacts, expected[0]);
    // Artifacts are derived at read time, not stored with the run.
    let stored = dag.store.read(|db| db.run(&run))?;
    assert!(
        stored.tasks[0]
            .attempts
            .iter()
            .all(|attempt| attempt.artifacts.is_empty())
    );
    Ok(())
}

#[test]
fn exit_without_report_interrupts_and_late_and_replayed_reports_are_safe() -> Result<()> {
    let (dag, run) = setup()?;
    let first = dag.start(command(&run, false))?;
    let first_id = first.attempt.id;
    let session = first.agent.session.unwrap();
    dag.session_exited(&session, "worker exited with code 7", true)?;
    assert_eq!(dag.run(&run)?.tasks[0].state, TaskState::Interrupted);
    let agent = dag.store.read(|db| db.agent(&first.agent.id))?;
    assert_eq!(agent.state, AgentState::Exited);
    assert!(dag.start(command(&run, false)).is_err());

    let second = dag.start(command(&run, true))?;
    let before = dag.latest_seq()?;
    let stale = dag.report(&run, "work", &first_id, ReportStatus::Success, "old result")?;
    assert!(!stale.applied);
    assert_eq!(stale.reason.as_deref(), Some("stale attempt"));
    let page = dag.events(EventScope::Run(&run, &[]), before, Duration::ZERO)?;
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, RunEventKind::StaleReport);
    assert_eq!(dag.run(&run)?.tasks[0].state, TaskState::Working);

    let current = &second.attempt.id;
    assert!(
        dag.report(&run, "work", current, ReportStatus::Success, "new result")?
            .applied
    );
    let before = dag.latest_seq()?;
    for status in [ReportStatus::Success, ReportStatus::Failed] {
        let replay = dag.report(&run, "work", current, status, "different summary")?;
        assert!(!replay.applied);
        assert_eq!(replay.reason.as_deref(), Some("already reported"));
    }
    assert_eq!(dag.latest_seq()?, before);
    let run = dag.run(&run)?;
    let attempts = &run.tasks[0].attempts;
    assert_eq!(
        attempts[0].detail.as_deref(),
        Some("worker exited with code 7")
    );
    assert_eq!(attempts[1].summary.as_deref(), Some("new result"));
    Ok(())
}

#[test]
fn reconcile_interrupts_active_and_blocked_attempts_and_keeps_reported_results() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(&run, &[spec("blocked", &[]), spec("done", &[])])?;
    let active = dag.start(command(&run, false))?;
    let blocked = dag.start(Dispatch {
        task: "blocked".into(),
        ..command(&run, false)
    })?;
    let done = dag.start(Dispatch {
        task: "done".into(),
        ..command(&run, false)
    })?;
    dag.report(
        &run,
        "blocked",
        &blocked.attempt.id,
        ReportStatus::NeedsInput,
        "question",
    )?;
    dag.report(
        &run,
        "done",
        &done.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    let before = dag.latest_seq()?;
    assert_eq!(dag.attention()?.len(), 1);
    dag.reconcile(&[])?;
    assert!(dag.attention()?.is_empty());
    let snapshot = dag.run(&run)?;
    assert_eq!(snapshot.tasks[2].state, TaskState::Completed);
    for task in &snapshot.tasks[..2] {
        let latest = task.attempts.last().unwrap();
        assert_eq!(task.state, TaskState::Interrupted);
        assert_eq!(latest.blocker, None);
        assert_eq!(
            latest.detail.as_deref(),
            Some("daemon restarted; session unavailable")
        );
    }
    for agent in [active.agent.id, blocked.agent.id, done.agent.id] {
        assert_eq!(
            dag.store.read(|db| db.agent(&agent))?.state,
            AgentState::Exited
        );
    }
    let page = dag.events(
        EventScope::Run(&run, &[RunEventKind::AttemptInterrupted]),
        before,
        Duration::ZERO,
    )?;
    assert_eq!(page.events.len(), 2);
    dag.reconcile(&[])?;
    assert_eq!(dag.latest_seq()?, page.cursor);
    Ok(())
}

#[test]
fn reconcile_keeps_attempts_and_agents_whose_session_was_restored() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(&run, &[spec("lost", &[])])?;
    let restored = dag.start(command(&run, false))?;
    let lost = dag.start(Dispatch {
        task: "lost".into(),
        ..command(&run, false)
    })?;
    let session = restored.agent.session.clone().unwrap();

    dag.reconcile(std::slice::from_ref(&session))?;

    let snapshot = dag.run(&run)?;
    let kept = snapshot.tasks[0].attempts.last().unwrap();
    assert_eq!(kept.state, AttemptState::Active);
    assert_eq!(kept.session.as_deref(), Some(session.as_str()));
    assert_eq!(snapshot.tasks[1].state, TaskState::Interrupted);
    let agent = |id: &str| dag.store.read(|db| db.agent(id)).map(|agent| agent.state);
    assert_eq!(agent(&restored.agent.id)?, AgentState::Running);
    assert_eq!(agent(&lost.agent.id)?, AgentState::Exited);
    Ok(())
}

#[test]
fn reconcile_stops_restored_agents_whose_worktree_is_gone_unless_an_attempt_is_open() -> Result<()>
{
    let workspace = FakeWorkspace::default();
    let dag = TestDag::new(
        application_with_workspace(workspace.clone()),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    );
    let run = external_run(&dag)?;
    dag.add_tasks(&run, &[spec("done", &[])])?;
    let working = dag.start(command(&run, false))?;
    let finished = dag.start(Dispatch {
        task: "done".into(),
        ..command(&run, false)
    })?;
    dag.report(
        &run,
        "done",
        &finished.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    let sessions =
        [&working, &finished].map(|dispatched| dispatched.agent.session.clone().unwrap());
    let agent = |id: &str| dag.store.read(|db| db.agent(id)).map(|agent| agent.state);

    dag.reconcile(&sessions)?;
    assert_eq!(agent(&finished.agent.id)?, AgentState::Running);

    workspace
        .0
        .lock()
        .unwrap()
        .missing
        .insert("/worktree".into());
    dag.reconcile(&sessions)?;

    assert_eq!(agent(&working.agent.id)?, AgentState::Running);
    assert_eq!(agent(&finished.agent.id)?, AgentState::Exited);
    let snapshot = dag.run(&run)?;
    assert_eq!(
        snapshot.tasks[0].attempts.last().unwrap().state,
        AttemptState::Active
    );
    Ok(())
}

#[test]
fn restore_tells_only_workers_with_active_attempts_to_continue() -> Result<()> {
    let application = application();
    let caller = application.start_session(StartSession {
        worktree: "/worktree".into(),
        agent: Agent::Coding("claude".into()),
    })?;
    let plain = application.start_session(StartSession {
        worktree: "/worktree".into(),
        agent: Agent::Coding("claude".into()),
    })?;
    let dag = TestDag::new(
        application,
        MemoryStore::default(),
        FakeRegistry(vec![caller.id.clone(), plain.id.clone()]),
        FakeIcons::default(),
    );
    let run = dag.create_run(
        BoxSelection {
            selector: None,
            caller: Some(caller.id.clone()),
        },
        "Restart".into(),
        None,
    )?;
    dag.add_tasks(&run.id, &[spec("work", &[]), spec("blocked", &[])])?;
    let claude = |task: &str| Dispatch {
        task: task.into(),
        provider: Some(Provider::Agent("claude".into())),
        args: Vec::new(),
        caller: Some(caller.id.clone()),
        ..command(&run.id, false)
    };
    let working = dag.start(claude("work"))?;
    let blocked = dag.start(claude("blocked"))?;
    dag.report(
        &run.id,
        "blocked",
        &blocked.attempt.id,
        ReportStatus::NeedsInput,
        "question",
    )?;
    // Saved records must point at a real directory to be restored.
    let worktree = tempfile::tempdir()?;
    for mut record in dag.application.records.list()? {
        record.worktree = worktree.path().into();
        dag.application.records.save(&record)?;
    }
    let launched = dag.application.runtime.launches.lock().unwrap().len();
    let before = dag.latest_seq()?;

    let restored = dag.restore()?;

    let working_session = working.agent.session.clone().unwrap();
    let blocked_session = blocked.agent.session.clone().unwrap();
    assert_eq!(restored.len(), 4);
    let launches = dag.application.runtime.launches.lock().unwrap();
    let prompts: BTreeMap<_, _> = launches[launched..]
        .iter()
        .map(|(session, options)| {
            assert_eq!(options.args, ["--continue"]);
            (session.id.clone(), options.initial_prompt.clone())
        })
        .collect();
    assert_eq!(prompts.len(), 4);
    let prompt = prompts[&working_session]
        .as_deref()
        .unwrap()
        .to_str()
        .unwrap();
    for expected in [
        "Power restarted",
        "killed every background command",
        "re-run your background commands",
        &format!("run {}", run.id),
        "task work",
        "worker-contract",
    ] {
        assert!(
            prompt.contains(expected),
            "{expected:?} missing from {prompt:?}"
        );
    }
    for session in [&blocked_session, &caller.id, &plain.id] {
        assert_eq!(
            prompts[session], None,
            "{session} must resume without a prompt"
        );
    }
    drop(launches);
    let page = dag.events(EventScope::Run(&run.id, &[]), before, Duration::ZERO)?;
    let events: Vec<_> = page
        .events
        .iter()
        .map(|event| (event.kind, event.task.as_deref(), event.attempt.as_deref()))
        .collect();
    assert_eq!(
        events,
        [(
            RunEventKind::AttemptResumed,
            Some("work"),
            Some(working.attempt.id.as_str())
        )]
    );
    let snapshot = dag.run(&run.id)?;
    assert_eq!(snapshot.tasks[0].state, TaskState::Working);
    assert_eq!(
        snapshot.tasks[1].attempts.last().unwrap().state,
        AttemptState::NeedsInput
    );
    Ok(())
}

#[test]
fn restore_brings_back_only_workers_whose_attempt_is_open() -> Result<()> {
    let application = application();
    let caller = application.start_session(StartSession {
        worktree: "/worktree".into(),
        agent: Agent::Coding("claude".into()),
    })?;
    let dag = TestDag::new(
        application,
        MemoryStore::default(),
        FakeRegistry(vec![caller.id.clone()]),
        FakeIcons::default(),
    );
    // Without a cleanup, finished sessions stay alive, as before a restart that
    // interrupts their grace.
    let run = dag.create_run(
        BoxSelection {
            selector: None,
            caller: Some(caller.id.clone()),
        },
        "Restart".into(),
        None,
    )?;
    let tasks = ["active", "blocked", "succeeded", "failed"];
    dag.add_tasks(&run.id, &tasks.map(|id| spec(id, &[])))?;
    let mut sessions = BTreeMap::new();
    for task in tasks {
        let dispatched = dag.start(Dispatch {
            task: task.into(),
            provider: Some(Provider::Agent("claude".into())),
            args: Vec::new(),
            caller: Some(caller.id.clone()),
            ..command(&run.id, false)
        })?;
        let status = match task {
            "active" => None,
            "blocked" => Some(ReportStatus::NeedsInput),
            "succeeded" => Some(ReportStatus::Success),
            _ => Some(ReportStatus::Failed),
        };
        if let Some(status) = status {
            dag.report(&run.id, task, &dispatched.attempt.id, status, task)?;
        }
        sessions.insert(task, dispatched.agent.session.unwrap());
    }
    let worktree = tempfile::tempdir()?;
    for mut record in dag.application.records.list()? {
        record.worktree = worktree.path().into();
        dag.application.records.save(&record)?;
    }
    let launched = dag.application.runtime.launches.lock().unwrap().len();

    let restored = dag.restore()?;

    let expected: BTreeSet<_> = [&caller.id, &sessions["active"], &sessions["blocked"]]
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(restored.iter().cloned().collect::<BTreeSet<_>>(), expected);
    let relaunched: BTreeSet<_> = dag.application.runtime.launches.lock().unwrap()[launched..]
        .iter()
        .map(|(session, _)| session.id.clone())
        .collect();
    assert_eq!(relaunched, expected);
    let saved: BTreeSet<_> = dag
        .application
        .records
        .list()?
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(saved, expected, "finished workers' records are forgotten");
    let snapshot = dag.run(&run.id)?;
    let state = |task: &str| {
        snapshot
            .tasks
            .iter()
            .find(|candidate| candidate.id == task)
            .unwrap()
            .attempts[0]
            .state
    };
    assert_eq!(state("active"), AttemptState::Active);
    assert_eq!(state("blocked"), AttemptState::NeedsInput);
    assert_eq!(state("succeeded"), AttemptState::Succeeded);
    assert_eq!(state("failed"), AttemptState::Failed);
    for task in ["succeeded", "failed"] {
        let agent = dag
            .store
            .read(|db| db.session_agent(&sessions[task]))?
            .unwrap();
        assert_eq!(agent.state, AgentState::Exited);
    }
    Ok(())
}

#[test]
fn terminal_reports_end_the_worker_session_after_the_grace() -> Result<()> {
    let (dag, run) = setup()?;
    let dag = dag.with_finished_worker_cleanup(
        Duration::from_millis(50),
        Arc::new(ThreadScheduler),
        Arc::new(ThreadScheduler),
    );
    dag.add_tasks(&run, &[spec("failing", &[]), spec("waiting", &[])])?;
    let mut ended = Vec::new();
    for (task, status) in [
        ("work", ReportStatus::Success),
        ("failing", ReportStatus::Failed),
    ] {
        let dispatched = dag.start(Dispatch {
            task: task.into(),
            ..command(&run, false)
        })?;
        let session = dispatched.agent.session.unwrap();
        let handle = dag.application.terminal(&session)?;
        assert!(
            dag.report(&run, task, &dispatched.attempt.id, status, task)?
                .applied
        );
        // The worker can still finish its last message.
        assert!(handle.load(Ordering::Acquire));
        ended.push((session, handle));
    }
    let waiting = dag.start(Dispatch {
        task: "waiting".into(),
        ..command(&run, false)
    })?;
    let waiting_session = waiting.agent.session.unwrap();
    dag.report(
        &run,
        "waiting",
        &waiting.attempt.id,
        ReportStatus::NeedsInput,
        "question",
    )?;

    let deadline = Instant::now() + Duration::from_secs(10);
    while ended
        .iter()
        .any(|(_, handle)| handle.load(Ordering::Acquire))
    {
        assert!(Instant::now() < deadline, "finished sessions never ended");
        thread::sleep(Duration::from_millis(10));
    }
    // An ended session keeps its record until its exit confirms nothing of it runs.
    assert_eq!(dag.application.records.list()?.len(), 3);
    for (session, _) in &ended {
        dag.session_exited(session, "ended", true)?;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while dag.application.records.list()?.len() > 1 {
        assert!(
            Instant::now() < deadline,
            "ended sessions were never forgotten"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let saved: Vec<_> = dag
        .application
        .records
        .list()?
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(saved, std::slice::from_ref(&waiting_session));
    for (session, _) in &ended {
        assert!(dag.application.terminal(session).is_err());
    }
    assert!(dag.application.session_is_running(&waiting_session)?);
    let snapshot = dag.run(&run)?;
    let states: Vec<_> = snapshot
        .tasks
        .iter()
        .map(|task| (task.id.as_str(), task.attempts[0].state))
        .collect();
    assert_eq!(
        states,
        [
            ("work", AttemptState::Succeeded),
            ("failing", AttemptState::Failed),
            ("waiting", AttemptState::NeedsInput),
        ]
    );
    Ok(())
}

/// Runs each job on its own sleeping thread, which tests can afford.
struct ThreadScheduler;

impl Scheduler for ThreadScheduler {
    fn schedule(&self, delay: Duration, job: crate::Job) -> Result<()> {
        thread::spawn(move || {
            thread::sleep(delay);
            job();
        });
        Ok(())
    }
}

/// Refuses every job, as a full scheduler or one that cannot start a thread does.
struct RefusingScheduler(AtomicU64);

impl Scheduler for RefusingScheduler {
    fn schedule(&self, _: Duration, _: crate::Job) -> Result<()> {
        self.0.fetch_add(1, Ordering::AcqRel);
        bail!("Resource temporarily unavailable (os error 11)")
    }
}

/// Holds jobs until the test runs them.
#[derive(Default)]
struct HeldScheduler(Mutex<Vec<crate::Job>>);

impl Scheduler for HeldScheduler {
    fn schedule(&self, _: Duration, job: crate::Job) -> Result<()> {
        self.0.lock().unwrap().push(job);
        Ok(())
    }
}

#[test]
fn a_refused_cleanup_ends_the_finished_worker_at_once() -> Result<()> {
    let (dag, run) = setup()?;
    let scheduler = Arc::new(RefusingScheduler(AtomicU64::new(0)));
    let dag = dag.with_finished_worker_cleanup(
        Duration::from_secs(600),
        scheduler.clone(),
        scheduler.clone(),
    );
    let dispatched = dag.start(command(&run, false))?;
    let session = dispatched.agent.session.unwrap();
    let handle = dag.application.terminal(&session)?;

    let result = dag.report(
        &run,
        "work",
        &dispatched.attempt.id,
        ReportStatus::Success,
        "done",
    )?;

    assert!(result.applied);
    assert_eq!(scheduler.0.load(Ordering::Acquire), 1);
    assert!(!handle.load(Ordering::Acquire), "the cleanup was lost");
    // The exit confirms the end. A refused forget leaves the record to the next start.
    dag.session_exited(&session, "ended", true)?;
    assert_eq!(dag.application.records.list()?.len(), 1);
    Ok(())
}

#[test]
fn a_replayed_terminal_report_schedules_the_cleanup_again() -> Result<()> {
    let (dag, run) = setup()?;
    let scheduler = Arc::new(HeldScheduler::default());
    let dag = dag.with_finished_worker_cleanup(
        Duration::from_secs(600),
        scheduler.clone(),
        scheduler.clone(),
    );
    let dispatched = dag.start(command(&run, false))?;
    let session = dispatched.agent.session.unwrap();
    let handle = dag.application.terminal(&session)?;
    let report = || {
        dag.report(
            &run,
            "work",
            &dispatched.attempt.id,
            ReportStatus::Success,
            "done",
        )
    };
    assert!(report()?.applied);
    // The first cleanup is lost, as one that never ran before a crash would be.
    drop(scheduler.0.lock().unwrap().pop());

    let replayed = report()?;

    assert!(!replayed.applied);
    let jobs: Vec<_> = scheduler.0.lock().unwrap().drain(..).collect();
    assert_eq!(jobs.len(), 1);
    assert!(handle.load(Ordering::Acquire));
    for job in jobs {
        job();
    }
    assert!(!handle.load(Ordering::Acquire));
    Ok(())
}

#[test]
fn an_incomplete_exit_keeps_the_agent_running_and_the_record_until_a_reclaim_ends_it() -> Result<()>
{
    let (dag, run) = setup()?;
    let dag = dag
        .with_finished_worker_cleanup(
            Duration::from_secs(600),
            Arc::new(HeldScheduler::default()),
            Arc::new(ThreadScheduler),
        )
        .shared();
    let dispatched = dag.start(command(&run, false))?;
    let session = dispatched.agent.session.unwrap();
    dag.report(
        &run,
        "work",
        &dispatched.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    dag.application
        .runtime
        .fail_reclaim
        .store(true, Ordering::Release);
    // The terminal's process exits, as the runtime reports it: its handle stops
    // running, and nothing edits the session map.
    let handle = dag.application.terminal(&session)?;
    handle.store(false, Ordering::Release);
    dag.session_exited(&session, "ended, but a helper still runs", false)?;
    assert!(
        dag.application.terminal(&session).is_err(),
        "the exited terminal still looks live"
    );
    let agent = || -> Result<ManagedAgent> {
        Ok(dag.store.read(|db| db.session_agent(&session))?.unwrap())
    };
    assert_eq!(agent()?.state, AgentState::Running);
    assert_eq!(dag.application.records.list()?.len(), 1);

    // A failing reclaim keeps both; one that ends everything lets them go.
    dag.retry_reclaim(session.clone());
    assert_eq!(agent()?.state, AgentState::Running);
    assert_eq!(dag.application.records.list()?.len(), 1);
    dag.application
        .runtime
        .fail_reclaim
        .store(false, Ordering::Release);
    dag.retry_reclaim(session.clone());
    assert_eq!(agent()?.state, AgentState::Exited);
    assert!(dag.application.records.list()?.is_empty());
    Ok(())
}

#[test]
fn a_restart_whose_reclaim_fails_neither_exits_the_agent_nor_relaunches_the_session() -> Result<()>
{
    let (dag, run) = setup()?;
    let dag = dag.shared();
    dag.add_tasks(&run, &[spec("active", &[])])?;
    let mut sessions = BTreeMap::new();
    for task in ["work", "active"] {
        let dispatched = dag.start(Dispatch {
            task: task.into(),
            ..command(&run, false)
        })?;
        if task == "work" {
            dag.report(
                &run,
                task,
                &dispatched.attempt.id,
                ReportStatus::Success,
                "done",
            )?;
        }
        sessions.insert(task, dispatched.agent.session.unwrap());
    }
    let launches = dag.application.runtime.launches.lock().unwrap().len();
    // The daemon restarts, and what it left of both sessions cannot be ended.
    dag.application.sessions.lock().unwrap().clear();
    dag.application
        .runtime
        .fail_reclaim
        .store(true, Ordering::Release);

    let restored = dag.restore()?;

    assert!(restored.is_empty(), "{restored:?}");
    assert_eq!(
        dag.application.runtime.launches.lock().unwrap().len(),
        launches,
        "a replacement launched beside processes that still run"
    );
    for session in sessions.values() {
        let agent = dag.store.read(|db| db.session_agent(session))?.unwrap();
        assert_eq!(agent.state, AgentState::Running, "{session}");
    }
    assert_eq!(dag.application.records.list()?.len(), 2);
    // No daemon follows the active worker any more, so its attempt is interrupted.
    let snapshot = dag.run(&run)?;
    assert_eq!(
        task_in(&snapshot, "active").attempts[0].state,
        AttemptState::Interrupted
    );
    assert_eq!(
        task_in(&snapshot, "work").attempts[0].state,
        AttemptState::Succeeded
    );
    Ok(())
}

fn task_in<'a>(run: &'a Run, id: &str) -> &'a Task {
    run.tasks.iter().find(|task| task.id == id).unwrap()
}

/// Keeps records but refuses to delete them, as a failing session database does.
#[derive(Default)]
struct UndeletableSessions(MemorySessions);

impl SessionStore for UndeletableSessions {
    fn save(&self, record: &crate::SessionRecord) -> Result<()> {
        self.0.save(record)
    }

    fn update_foreground(
        &self,
        id: &str,
        foreground: &Agent,
        conversation: Option<&str>,
        cwd: Option<&Path>,
        labels: Option<&crate::Labels>,
    ) -> Result<()> {
        self.0
            .update_foreground(id, foreground, conversation, cwd, labels)
    }

    fn update_labels(&self, id: &str, labels: &crate::Labels) -> Result<()> {
        self.0.update_labels(id, labels)
    }

    fn remove(&self, _: &str) -> Result<()> {
        bail!("attempt to write a readonly database")
    }

    fn list(&self) -> Result<Vec<crate::SessionRecord>> {
        self.0.list()
    }
}

#[test]
fn ending_a_session_still_ends_it_when_its_record_cannot_be_deleted() -> Result<()> {
    let records = Arc::new(UndeletableSessions::default());
    let application = Application::new(
        FakeArtifacts::default(),
        records.clone(),
        FakeWorkspace::default(),
        FakeTerminal::default(),
        Arc::new(FakeProfiles(Vec::new())),
        || "rejected".into(),
    );
    let session = application.start_session(StartSession {
        worktree: "/worktree".into(),
        agent: Agent::Shell,
    })?;
    let handle = application.terminal(&session.id)?;

    let error = application.end_session(&session.id).unwrap_err();

    assert!(
        format!("{error:#}").contains("readonly database"),
        "{error:#}"
    );
    assert!(!handle.load(Ordering::Acquire), "the session kept running");
    assert!(application.terminal(&session.id).is_err());
    // A retry still tries to forget the record of the session that is gone.
    let retried = application.end_session(&session.id).unwrap_err();
    assert!(
        format!("{retried:#}").contains("readonly database"),
        "{retried:#}"
    );
    assert_eq!(records.list()?.len(), 1);
    Ok(())
}

/// A session store whose deletes block until the test opens the gate, as a session
/// database under another connection's write lock does.
#[derive(Default)]
struct BlockedSessions {
    records: MemorySessions,
    open: Mutex<bool>,
    opened: Condvar,
    waiting: AtomicU64,
}

impl BlockedSessions {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

impl SessionStore for BlockedSessions {
    fn save(&self, record: &crate::SessionRecord) -> Result<()> {
        self.records.save(record)
    }

    fn update_foreground(
        &self,
        id: &str,
        foreground: &Agent,
        conversation: Option<&str>,
        cwd: Option<&Path>,
        labels: Option<&crate::Labels>,
    ) -> Result<()> {
        self.records
            .update_foreground(id, foreground, conversation, cwd, labels)
    }

    fn update_labels(&self, id: &str, labels: &crate::Labels) -> Result<()> {
        self.records.update_labels(id, labels)
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.waiting.fetch_add(1, Ordering::AcqRel);
        let open = self.open.lock().unwrap();
        drop(self.opened.wait_while(open, |open| !*open).unwrap());
        self.records.remove(id)
    }

    fn list(&self) -> Result<Vec<crate::SessionRecord>> {
        self.records.list()
    }
}

/// Runs jobs one at a time on a single thread, in due order, as the daemon's timer
/// does.
struct SerialScheduler(Mutex<mpsc::Sender<(Instant, crate::Job)>>);

impl SerialScheduler {
    fn start() -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<(Instant, crate::Job)>();
        thread::spawn(move || {
            for (due, job) in rx {
                thread::sleep(due.saturating_duration_since(Instant::now()));
                job();
            }
        });
        Arc::new(Self(Mutex::new(tx)))
    }
}

impl Scheduler for SerialScheduler {
    fn schedule(&self, delay: Duration, job: crate::Job) -> Result<()> {
        if self
            .0
            .lock()
            .unwrap()
            .send((Instant::now() + delay, job))
            .is_err()
        {
            bail!("the scheduler thread stopped");
        }
        Ok(())
    }
}

#[test]
fn a_blocked_session_write_never_delays_the_next_finished_workers_teardown() -> Result<()> {
    let records = Arc::new(BlockedSessions::default());
    static ID: AtomicU64 = AtomicU64::new(0);
    let application = Arc::new(Application::new(
        FakeArtifacts::default(),
        records.clone(),
        FakeWorkspace::default(),
        FakeTerminal::default(),
        Arc::new(FakeProfiles(Vec::new())),
        || format!("blocked-{}", ID.fetch_add(1, Ordering::Relaxed)),
    ));
    let dag = TestDag::new(
        application,
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .with_finished_worker_cleanup(
        Duration::from_millis(50),
        SerialScheduler::start(),
        SerialScheduler::start(),
    );
    let run = external_run(&dag)?;
    dag.add_tasks(&run, &[spec("second", &[])])?;
    let mut handles = Vec::new();
    let mut sessions = Vec::new();
    for task in ["work", "second"] {
        let dispatched = dag.start(Dispatch {
            task: task.into(),
            ..command(&run, false)
        })?;
        let session = dispatched.agent.session.unwrap();
        handles.push(dag.application.terminal(&session)?);
        sessions.push(session);
        dag.report(
            &run,
            task,
            &dispatched.attempt.id,
            ReportStatus::Success,
            task,
        )?;
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while handles.iter().any(|handle| handle.load(Ordering::Acquire)) {
        assert!(
            Instant::now() < deadline,
            "a blocked record delete held up the second worker's teardown"
        );
        thread::sleep(Duration::from_millis(10));
    }
    for handle in ["work", "second"].iter().zip(&sessions) {
        dag.session_exited(handle.1, "ended", true)?;
    }
    // The first delete is still blocked while both workers have ended.
    let deadline = Instant::now() + Duration::from_secs(5);
    while records.waiting.load(Ordering::Acquire) != 1 {
        assert!(Instant::now() < deadline, "the first delete never started");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(records.list()?.len(), 2);

    records.open();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !records.list()?.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the records were never forgotten"
        );
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[test]
fn caller_registers_once_and_worker_receives_parent_and_prompt() -> Result<()> {
    let application = application();
    let caller = application.start_session(StartSession {
        worktree: "/worktree".into(),
        agent: Agent::Coding("codex".into()),
    })?;
    let dag = TestDag::new(
        application,
        MemoryStore::default(),
        FakeRegistry(vec![caller.id.clone()]),
        FakeIcons::default(),
    );
    let selection = || BoxSelection {
        selector: None,
        caller: Some(caller.id.clone()),
    };
    let run = dag.create_run(selection(), "My run".into(), None)?;
    let other_run = dag.create_run(selection(), "Another run".into(), None)?;
    assert_eq!(run.coordinator, other_run.coordinator);
    let Coordinator::Agent { agent: coordinator } = run.coordinator else {
        panic!("Expected a managed coordinator")
    };
    assert_eq!(dag.list_agents(selection(), true)?.len(), 1);
    dag.add_tasks(&run.id, &[spec("join", &["work"]), spec("work", &[])])?;
    let first = dag.start(command(&run.id, false))?;
    dag.report(
        &run.id,
        "work",
        &first.attempt.id,
        ReportStatus::Success,
        "Prerequisite result",
    )?;
    let statuses: Vec<_> = dag
        .list_agents(selection(), true)?
        .into_iter()
        .map(|agent| (agent.id, agent.status))
        .collect();
    assert_eq!(
        statuses,
        [
            (coordinator.clone(), AgentStatus::Idle),
            (first.agent.id.clone(), AgentStatus::Done)
        ]
    );
    let live: Vec<_> = dag
        .list_agents(selection(), false)?
        .into_iter()
        .map(|agent| agent.id)
        .collect();
    assert_eq!(live, std::slice::from_ref(&coordinator));
    let worker = dag.start(Dispatch {
        task: "join".into(),
        provider: Some(Provider::Agent("claude".into())),
        caller: Some(caller.id),
        args: vec!["--flag".into()],
        ..command(&run.id, false)
    })?;
    assert_eq!(worker.agent.parent.as_deref(), Some(coordinator.as_str()));
    let launches = dag.application.runtime.launches.lock().unwrap();
    let (session, options) = launches.last().unwrap();
    assert_eq!(session.agent, Agent::Coding("claude".into()));
    assert_eq!(options.args, ["--flag"]);
    let prompt = options.initial_prompt.as_deref().unwrap().to_str().unwrap();
    for expected in [
        "My run",
        "Task: join",
        "Implement join",
        "Prerequisite result",
        "/worktree",
        "power dag report success",
        "power dag report failed",
        "power dag report needs-input",
        "power dag report working",
        "Full worker contract: `power skills get power-dag --reference worker-contract`.",
    ] {
        assert!(prompt.contains(expected), "{prompt}");
    }
    Ok(())
}

#[test]
fn agent_status_follows_the_attempt_while_the_session_lives() -> Result<()> {
    let (dag, run) = setup()?;
    let tasks = ["blocked", "done", "failed", "exited"];
    dag.add_tasks(&run, &tasks.map(|id| spec(id, &[])))?;
    let working = dag.start(command(&run, false))?;
    assert_eq!(working.agent.status, AgentStatus::Working);
    let [blocked, done, failed, exited] = tasks.map(|task| {
        dag.start(Dispatch {
            task: task.into(),
            ..command(&run, false)
        })
        .unwrap()
    });
    for (task, attempt, status) in [
        ("blocked", &blocked, ReportStatus::NeedsInput),
        ("done", &done, ReportStatus::Success),
        ("failed", &failed, ReportStatus::Failed),
        ("exited", &exited, ReportStatus::Success),
    ] {
        dag.report(&run, task, &attempt.attempt.id, status, "summary")?;
    }
    dag.session_exited(exited.agent.session.as_deref().unwrap(), "closed", true)?;

    let status = |id: &str| dag.store.read(|db| db.agent(id)).map(|agent| agent.status);
    assert_eq!(status(&working.agent.id)?, AgentStatus::Working);
    assert_eq!(status(&blocked.agent.id)?, AgentStatus::NeedsInput);
    assert_eq!(status(&done.agent.id)?, AgentStatus::Done);
    assert_eq!(status(&failed.agent.id)?, AgentStatus::Failed);
    assert_eq!(status(&exited.agent.id)?, AgentStatus::Exited);
    for agent in [&working, &blocked, &done, &failed] {
        let state = dag.store.read(|db| db.agent(&agent.agent.id))?.state;
        assert_eq!(state, AgentState::Running);
    }
    Ok(())
}

#[test]
fn rejected_graph_edits_are_atomic() -> Result<()> {
    let (dag, run) = setup()?;
    let before = dag.latest_seq()?;
    assert!(
        dag.add_tasks(&run, &[spec("good", &[]), spec("bad", &["missing"])])
            .is_err()
    );
    assert_eq!(dag.latest_seq()?, before);
    assert_eq!(dag.run(&run)?.tasks.len(), 1);
    dag.add_tasks(&run, &[spec("after", &["work"])])?;
    assert!(dag.add_dependency(&run, "work", "after").is_err());
    assert!(dag.run(&run)?.tasks[0].deps.is_empty());
    Ok(())
}

#[test]
fn external_coordinator_ignores_a_caller_session_this_daemon_does_not_host() -> Result<()> {
    // A bootstrap coordinator can run in a multiplexer inside another Power daemon's terminal,
    // so the CLI forwards a POWER_SESSION this daemon has never seen.
    let (dag, run) = setup()?;
    let worker = dag.start(Dispatch {
        caller: Some("session-from-another-daemon".into()),
        ..command(&run, false)
    })?;
    assert_eq!(worker.agent.parent, None);
    assert_eq!(dag.run(&run)?.tasks[0].state, TaskState::Working);
    Ok(())
}

#[test]
fn starting_an_agent_ignores_a_caller_session_this_daemon_does_not_host() -> Result<()> {
    let (dag, _) = setup()?;
    let agent = dag.start_agent(StartAgent {
        selection: BoxSelection {
            selector: Some("2".into()),
            caller: Some("session-from-another-daemon".into()),
        },
        provider: Provider::Agent("codex".into()),
        worktree: WorktreeRequest::Existing {
            path: "/worktree".into(),
            allow_main_checkout: false,
        },
        parent: None,
        label: "coordinator".into(),
        prompt: None,
        args: Vec::new(),
    })?;
    assert_eq!(agent.parent, None);
    assert!(agent.session.is_some());
    Ok(())
}

fn start_agent(worktree: WorktreeRequest) -> StartAgent {
    StartAgent {
        selection: BoxSelection {
            selector: Some("2".into()),
            caller: None,
        },
        provider: Provider::Agent("codex".into()),
        worktree,
        parent: None,
        label: "background".into(),
        prompt: None,
        args: Vec::new(),
    }
}

fn main_checkout(allow_main_checkout: bool) -> WorktreeRequest {
    WorktreeRequest::Existing {
        path: "/main".into(),
        allow_main_checkout,
    }
}

#[test]
fn workers_need_an_explicit_opt_in_for_the_main_checkout() -> Result<()> {
    let (dag, run) = setup()?;
    let refused = |result: Result<()>| {
        let error = format!("{:#}", result.expect_err("main checkout accepted"));
        assert!(
            error.contains("/main is the repository's main checkout"),
            "{error}"
        );
        assert!(error.contains("--branch"), "{error}");
    };
    refused(
        dag.start(Dispatch {
            worktree: main_checkout(false),
            ..command(&run, false)
        })
        .map(|_| ()),
    );
    assert_eq!(dag.run(&run)?.tasks[0].state, TaskState::Ready);
    refused(
        dag.start_agent(start_agent(main_checkout(false)))
            .map(|_| ()),
    );
    assert!(
        dag.list_agents(
            BoxSelection {
                selector: Some("2".into()),
                caller: None,
            },
            true,
        )?
        .is_empty()
    );

    let worker = dag.start(Dispatch {
        worktree: main_checkout(true),
        ..command(&run, false)
    })?;
    assert_eq!(worker.worktree, Path::new("/main"));
    let agent = dag.start_agent(start_agent(main_checkout(true)))?;
    assert_eq!(agent.worktree, Path::new("/main"));
    Ok(())
}

#[test]
fn started_agents_can_get_a_managed_worktree() -> Result<()> {
    let (dag, workspace) = managed_setup();
    let agent = dag.start_agent(start_agent(WorktreeRequest::New {
        branch: "agent/notes".into(),
        base: Some("main".into()),
    }))?;
    let path = Path::new("/canonical/managed-worktrees/test-repo/agent-notes");
    assert_eq!(agent.worktree, path);
    assert_eq!(
        workspace.0.lock().unwrap().creation_requests,
        [(
            "/managed-worktrees/test-repo/agent-notes".into(),
            "agent/notes".into(),
            "main".into()
        )]
    );
    let record = dag
        .store
        .read(|db| db.find_worktree_by_path(path))?
        .context("Ownership record missing")?;
    assert_eq!(record.branch, "agent/notes");
    assert_eq!(record.base_commit, "resolved-main");
    assert_eq!(
        record.creator,
        WorktreeCreator::View {
            box_id: agent.box_id.clone(),
            session: agent.session.clone().unwrap(),
        }
    );

    // Outside Power there is no caller HEAD to start from.
    let error = dag
        .start_agent(start_agent(WorktreeRequest::New {
            branch: "agent/other".into(),
            base: None,
        }))
        .unwrap_err();
    assert!(format!("{error:#}").contains("Specify --base"), "{error:#}");
    Ok(())
}

#[test]
fn a_failed_agent_start_removes_its_new_worktree() -> Result<()> {
    let (dag, workspace) = managed_setup();
    let error = dag
        .start_agent(StartAgent {
            provider: Provider::Command,
            ..start_agent(WorktreeRequest::New {
                branch: "agent/broken".into(),
                base: Some("main".into()),
            })
        })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("Specify a command"),
        "{error:#}"
    );
    assert_eq!(workspace.0.lock().unwrap().creation_requests.len(), 1);
    assert_eq!(
        workspace.0.lock().unwrap().removed,
        [PathBuf::from(
            "/canonical/managed-worktrees/test-repo/agent-broken"
        )]
    );
    assert!(dag.store.read(|db| db.list_worktrees(None))?.is_empty());
    Ok(())
}

#[test]
fn box_icons_follow_the_caller_or_selector() -> Result<()> {
    let dag = TestDag::new(
        application(),
        MemoryStore::default(),
        FakeRegistry(vec!["view-session".into()]),
        FakeIcons::default(),
    );
    let caller = || BoxSelection {
        selector: None,
        caller: Some("view-session".into()),
    };
    assert_eq!(
        dag.set_box_icon(caller(), Some("/icons/robot.png".into()))?,
        "box-id"
    );
    let selected = BoxSelection {
        selector: Some("2".into()),
        caller: Some("unknown-session".into()),
    };
    assert_eq!(dag.set_box_icon(selected, None)?, "box-id");
    assert!(
        dag.set_box_icon(caller(), Some("relative.png".into()))
            .is_err()
    );
    let nobody = BoxSelection {
        selector: None,
        caller: None,
    };
    assert!(dag.set_box_icon(nobody, None).is_err());
    assert_eq!(
        *dag.icons.0.lock().unwrap(),
        [
            ("box-id".into(), Some("/icons/robot.png".into())),
            ("box-id".into(), None),
        ]
    );
    Ok(())
}

fn planned(id: &str, provider: Option<&str>, model: Option<&str>) -> TaskSpec {
    TaskSpec {
        provider: provider.map(|id| Provider::Agent(id.into())),
        model: model.map(str::to_owned),
        ..spec(id, &[])
    }
}

fn last_launch(dag: &TestDag) -> (Agent, Vec<OsString>) {
    let launches = dag.application.runtime.launches.lock().unwrap();
    let (session, options) = launches.last().unwrap();
    (session.agent.clone(), options.args.clone())
}

fn planned_dispatch(run: &str, task: &str, provider: Option<&str>, args: &[&str]) -> Dispatch {
    Dispatch {
        task: task.into(),
        provider: provider.map(|id| Provider::Agent(id.into())),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        ..command(run, false)
    }
}

#[test]
fn applied_plans_round_trip_and_reject_unusable_models() -> Result<()> {
    let (dag, run) = setup()?;
    let shown = dag.add_tasks(
        &run,
        &[planned("plan", Some("codex"), Some(" gpt-6.2-sol "))],
    )?;
    let task = find_task(&shown, "plan")?;
    assert_eq!(task.provider, Some(Provider::Agent("codex".into())));
    assert_eq!(task.model.as_deref(), Some("gpt-6.2-sol"));
    assert_eq!(find_task(&dag.run(&run)?, "plan")?, task);
    for bad in [
        planned("blank", Some("codex"), Some("  ")),
        TaskSpec {
            provider: Some(Provider::Command),
            ..planned("command", None, Some("model"))
        },
    ] {
        assert!(dag.add_tasks(&run, &[bad]).is_err());
    }
    assert_eq!(dag.run(&run)?.tasks.len(), 2);
    Ok(())
}

#[test]
fn dispatch_without_a_provider_uses_the_plan() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(
        &run,
        &[
            planned("codex", Some("codex"), Some("gpt-6.2-sol")),
            planned("bare", Some("claude"), None),
            planned("unplanned", None, None),
        ],
    )?;
    let worker = dag.start(planned_dispatch(&run, "codex", None, &["--full-auto"]))?;
    assert_eq!(worker.attempt.provider, Provider::Agent("codex".into()));
    assert_eq!(
        last_launch(&dag),
        (
            Agent::Coding("codex".into()),
            ["--model", "gpt-6.2-sol", "--full-auto"]
                .map(OsString::from)
                .into()
        )
    );
    dag.dispatch(planned_dispatch(&run, "bare", None, &[]))?;
    assert_eq!(last_launch(&dag), (Agent::Coding("claude".into()), vec![]));
    let error = dag
        .dispatch(planned_dispatch(&run, "unplanned", None, &[]))
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("--provider"), "{error:#}");
    Ok(())
}

#[test]
fn explicit_providers_and_model_args_override_the_plan() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(
        &run,
        &[
            planned("other", Some("codex"), Some("gpt")),
            planned("args", Some("codex"), Some("gpt")),
        ],
    )?;
    dag.dispatch(planned_dispatch(&run, "other", Some("claude"), &[]))?;
    assert_eq!(last_launch(&dag), (Agent::Coding("claude".into()), vec![]));
    dag.dispatch(planned_dispatch(&run, "args", None, &["-m=mini"]))?;
    assert_eq!(last_launch(&dag).1, [OsString::from("-m=mini")]);
    Ok(())
}

#[test]
fn an_explicit_planned_provider_keeps_the_planned_model() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(
        &run,
        &[
            planned("same", Some("codex"), Some("gpt")),
            planned("same-args", Some("codex"), Some("gpt")),
        ],
    )?;
    // Naming the planned provider is not an override: only model arguments replace the model.
    dag.dispatch(planned_dispatch(
        &run,
        "same",
        Some("codex"),
        &["--full-auto"],
    ))?;
    assert_eq!(
        last_launch(&dag).1,
        ["--model", "gpt", "--full-auto"].map(OsString::from)
    );
    dag.dispatch(planned_dispatch(
        &run,
        "same-args",
        Some("codex"),
        &["--model", "mini"],
    ))?;
    assert_eq!(last_launch(&dag).1, ["--model", "mini"].map(OsString::from));
    Ok(())
}

#[test]
fn an_attached_model_value_replaces_the_planned_model() -> Result<()> {
    let (dag, run) = setup()?;
    let ids = ["short", "short-eq", "long-eq"];
    dag.add_tasks(
        &run,
        &ids.map(|id| planned(id, Some("codex"), Some("plan-model"))),
    )?;
    for (id, arg) in ids
        .into_iter()
        .zip(["-mother-model", "-m=other-model", "--model=other-model"])
    {
        dag.dispatch(planned_dispatch(&run, id, None, &[arg]))?;
        let args = last_launch(&dag).1;
        assert_eq!(args, [OsString::from(arg)], "{id}");
        assert_eq!(
            domain::agent::requested_model("codex", &args).as_deref(),
            Some("other-model")
        );
    }
    Ok(())
}

#[test]
fn unrelated_args_never_drop_a_planned_model() -> Result<()> {
    let (dag, run) = setup()?;
    // Power cannot pass a model to a profile whose model flag it does not know.
    assert!(
        dag.add_tasks(&run, &[planned("custom", Some("custom"), Some("big"))])
            .is_err()
    );
    assert!(
        dag.plan_task(PlanTask {
            run: run.clone(),
            task: "work".into(),
            clear: false,
            provider: Some(Provider::Agent("custom".into())),
            model: Some("big".into()),
        })
        .is_err()
    );
    // A plan stored before that check, or a model-only plan dispatched to such a profile,
    // fails instead of launching without the model.
    let mut task = find_task(&dag.run(&run)?, "work")?.clone();
    task.provider = Some(Provider::Agent("custom".into()));
    task.model = Some("big".into());
    for args in [vec![], vec!["--verbose".to_owned()]] {
        let error = planned_launch(&run, &task, None, &args).err().unwrap();
        assert!(format!("{error:#}").contains("--model ''"), "{error:#}");
    }
    task.provider = None;
    let verbose = ["--verbose".to_owned()];
    assert!(
        planned_launch(
            &run,
            &task,
            Some(Provider::Agent("custom".into())),
            &verbose
        )
        .is_err()
    );
    assert!(planned_launch(&run, &task, Some(Provider::Command), &verbose).is_err());
    // Codex adds the planned model in front of unrelated arguments.
    dag.add_tasks(&run, &[planned("codex", Some("codex"), Some("big"))])?;
    dag.dispatch(planned_dispatch(&run, "codex", None, &["--verbose"]))?;
    assert_eq!(
        last_launch(&dag).1,
        ["--model", "big", "--verbose"].map(OsString::from)
    );
    Ok(())
}

#[test]
fn a_model_only_plan_launches_with_an_explicit_provider() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(&run, &[planned("solo", None, Some("solo-model"))])?;
    let replanned = dag.plan_task(PlanTask {
        run: run.clone(),
        task: "work".into(),
        clear: false,
        provider: None,
        model: Some("work-model".into()),
    })?;
    let work = find_task(&replanned, "work")?;
    assert_eq!(
        (&work.provider, work.model.as_deref()),
        (&None, Some("work-model"))
    );
    for (task, model) in [("solo", "solo-model"), ("work", "work-model")] {
        let error = dag
            .dispatch(planned_dispatch(&run, task, None, &[]))
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("Pass --provider"),
            "{error:#}"
        );
        let worker = dag.start(planned_dispatch(
            &run,
            task,
            Some("codex"),
            &["--full-auto"],
        ))?;
        assert_eq!(worker.attempt.provider, Provider::Agent("codex".into()));
        assert_eq!(
            last_launch(&dag),
            (
                Agent::Coding("codex".into()),
                ["--model", model, "--full-auto"].map(OsString::from).into()
            )
        );
    }
    Ok(())
}

#[test]
fn plans_name_a_known_agent_profile() -> Result<()> {
    let (dag, run) = setup()?;
    assert!(
        dag.add_tasks(&run, &[planned("typo", Some("codx"), None)])
            .is_err()
    );
    assert!(
        dag.plan_task(PlanTask {
            run,
            task: "work".into(),
            clear: false,
            provider: Some(Provider::Agent("codx".into())),
            model: None,
        })
        .is_err()
    );
    Ok(())
}

#[test]
fn planning_changes_clears_and_reports_the_plan() -> Result<()> {
    let (dag, run) = setup()?;
    let plan = |provider: Option<&str>, model: Option<&str>, clear: bool| {
        dag.plan_task(PlanTask {
            run: run.clone(),
            task: "work".into(),
            clear,
            provider: provider.map(|id| Provider::Agent(id.into())),
            model: model.map(str::to_owned),
        })
        .map(|run| {
            let task = run.tasks.into_iter().next().unwrap();
            (task.provider, task.model)
        })
    };
    let codex = Some(Provider::Agent("codex".into()));
    let claude = Some(Provider::Agent("claude".into()));
    assert_eq!(
        plan(Some("codex"), Some("gpt"), false)?,
        (codex.clone(), Some("gpt".into()))
    );
    assert_eq!(
        plan(None, Some("mini"), false)?,
        (codex.clone(), Some("mini".into()))
    );
    assert_eq!(plan(Some("claude"), None, false)?, (claude.clone(), None));
    assert_eq!(
        plan(None, Some("opus"), false)?,
        (claude.clone(), Some("opus".into()))
    );
    assert_eq!(plan(None, Some(""), false)?, (claude.clone(), None));
    let before = dag.latest_seq()?;
    assert_eq!(plan(None, None, false)?, (claude, None));
    assert_eq!(
        dag.latest_seq()?,
        before,
        "an unchanged plan emits no event"
    );
    assert_eq!(plan(None, None, true)?, (None, None));
    assert!(plan(None, Some("x"), false).is_ok());
    assert!(
        dag.plan_task(PlanTask {
            run: run.clone(),
            task: "missing".into(),
            clear: true,
            provider: None,
            model: None,
        })
        .is_err()
    );
    let summaries: Vec<_> = dag
        .events(
            EventScope::Run(&run, &[RunEventKind::TaskPlanned]),
            0,
            Duration::ZERO,
        )?
        .events
        .into_iter()
        .map(|event| event.summary.unwrap())
        .collect();
    assert_eq!(
        summaries,
        [
            "codex / gpt",
            "codex / mini",
            "claude",
            "claude / opus",
            "claude",
            "no plan",
            "x"
        ]
    );
    Ok(())
}

#[test]
fn runs_sort_active_then_stalled_then_completed_newest_first() {
    let run = |id: &str, created_at: u64, states: &[TaskState]| Run {
        id: id.into(),
        box_id: "box".into(),
        title: id.into(),
        coordinator: Coordinator::External {
            label: "test".into(),
        },
        created_at,
        archived_at: None,
        paused_at: None,
        tasks: states
            .iter()
            .enumerate()
            .map(|(index, state)| Task {
                id: format!("t{index}"),
                title: String::new(),
                instructions: String::new(),
                deps: Vec::new(),
                provider: None,
                model: None,
                state: *state,
                queued_at: None,
                current_attempt: None,
                attempts: Vec::new(),
            })
            .collect(),
    };
    let mut runs = vec![
        run("done-old", 1, &[TaskState::Completed]),
        run("stalled-old", 2, &[TaskState::Failed]),
        run("active-old", 3, &[TaskState::Working]),
        run("done-new", 4, &[TaskState::Completed]),
        run(
            "stalled-new",
            5,
            &[TaskState::Completed, TaskState::Interrupted],
        ),
        run("active-new", 6, &[]),
        Run {
            archived_at: Some(8),
            ..run("archived-active", 7, &[TaskState::Working])
        },
    ];
    sort_runs(&mut runs);
    let order: Vec<_> = runs.iter().map(|run| run.id.as_str()).collect();
    assert_eq!(
        order,
        [
            "active-new",
            "active-old",
            "stalled-new",
            "stalled-old",
            "done-new",
            "done-old",
            "archived-active"
        ]
    );
}

#[test]
fn completed_runs_are_listed_only_when_finished_are_included() -> Result<()> {
    let (dag, run) = setup()?;
    let box_two = || BoxSelection {
        selector: Some("2".into()),
        caller: None,
    };
    let ids = |include_finished| -> Result<Vec<String>> {
        Ok(dag
            .list_runs(box_two(), include_finished)?
            .into_iter()
            .map(|run| run.id)
            .collect())
    };
    assert_eq!(ids(false)?, std::slice::from_ref(&run));
    let worker = dag.start(command(&run, false))?;
    dag.report(
        &run,
        "work",
        &worker.attempt.id,
        ReportStatus::Success,
        "Done",
    )?;
    assert!(ids(false)?.is_empty());
    assert_eq!(ids(true)?, [run]);
    Ok(())
}

#[test]
fn archiving_refuses_open_attempts_unless_forced() -> Result<()> {
    let (dag, run) = setup()?;
    let worker = dag.start(command(&run, false))?;
    let error = dag.archive_run(&run, false).unwrap_err().to_string();
    assert!(error.contains("1 open attempt"), "{error}");
    assert!(error.contains("--force"), "{error}");
    assert_eq!(dag.run(&run)?.archived_at, None);

    let archived = dag.archive_run(&run, true)?;
    assert!(archived.archived_at.is_some());
    assert_eq!(archived.status(), RunStatus::Archived);
    // The worker is still live and can still report.
    let reported = dag.report(
        &run,
        "work",
        &worker.attempt.id,
        ReportStatus::Success,
        "Done",
    )?;
    assert!(reported.applied);
    assert_eq!(dag.run(&run)?.status(), RunStatus::Archived);
    Ok(())
}

#[test]
fn archived_runs_are_hidden_by_default_and_sort_last() -> Result<()> {
    let (dag, run) = setup()?;
    let box_two = || BoxSelection {
        selector: Some("2".into()),
        caller: None,
    };
    let other = dag
        .create_run(box_two(), "Newer".into(), Some("external".into()))?
        .id;
    let ids = |include_finished| -> Result<Vec<String>> {
        Ok(dag
            .list_runs(box_two(), include_finished)?
            .into_iter()
            .map(|run| run.id)
            .collect())
    };
    let both = |mut ids: Vec<String>| {
        ids.sort();
        let mut expected = vec![run.clone(), other.clone()];
        expected.sort();
        assert_eq!(ids, expected);
    };
    both(ids(false)?);

    let archived_at = dag.archive_run(&other, false)?.archived_at;
    assert!(archived_at.is_some());
    // Archiving again keeps the original time.
    assert_eq!(dag.archive_run(&other, false)?.archived_at, archived_at);
    assert_eq!(ids(false)?, std::slice::from_ref(&run));
    assert_eq!(ids(true)?, [run.clone(), other.clone()]);
    let kinds: Vec<_> = dag
        .events(EventScope::Run(&other, &[]), 0, Duration::ZERO)?
        .events
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(kinds, [RunEventKind::RunCreated, RunEventKind::RunArchived]);

    let restored = dag.unarchive_run(&other)?;
    assert_eq!(restored.archived_at, None);
    assert_eq!(restored.status(), RunStatus::Active);
    assert_eq!(dag.unarchive_run(&other)?.archived_at, None);
    both(ids(false)?);
    let events = dag.events(EventScope::Run(&other, &[]), 0, Duration::ZERO)?;
    assert_eq!(
        events.events.last().map(|event| event.kind),
        Some(RunEventKind::RunUnarchived)
    );
    Ok(())
}

#[test]
fn live_agents_keep_finished_ancestors_of_shown_agents() {
    let agent = |id: &str, parent: Option<&str>, status| ManagedAgent {
        id: id.into(),
        box_id: "box".into(),
        worktree: PathBuf::from("/worktree"),
        provider: Provider::Agent("claude".into()),
        model: None,
        parent: parent.map(Into::into),
        session: None,
        run: None,
        task: None,
        attempt: None,
        state: AgentState::Running,
        paused: false,
        status,
        label: id.into(),
        created_at: 0,
    };
    let agents = vec![
        agent("root", None, AgentStatus::Exited),
        agent("middle", Some("root"), AgentStatus::Done),
        agent("leaf", Some("middle"), AgentStatus::Working),
        agent("done-sibling", Some("root"), AgentStatus::Done),
        agent("gone", None, AgentStatus::Exited),
        agent("failed", None, AgentStatus::Failed),
        agent("waiting", Some("gone-parent"), AgentStatus::NeedsInput),
        agent("cycle-a", Some("cycle-b"), AgentStatus::Idle),
        agent("cycle-b", Some("cycle-a"), AgentStatus::Done),
    ];
    let kept: Vec<_> = live_agents(agents)
        .into_iter()
        .map(|agent| agent.id)
        .collect();
    assert_eq!(
        kept,
        [
            "root", "middle", "leaf", "failed", "waiting", "cycle-a", "cycle-b"
        ]
    );
}

struct FixedBudget(WorkerBudget);

impl WorkerBudgetSource for FixedBudget {
    fn worker_budget(&self) -> Result<WorkerBudget> {
        Ok(self.0)
    }
}

/// Available memory the test can change while the DAG holds the probe.
#[derive(Clone, Default)]
struct FakeMemory(Arc<AtomicU64>);

impl MemoryProbe for FakeMemory {
    fn available_memory(&self) -> Option<u64> {
        Some(self.0.load(Ordering::Relaxed))
    }
}

fn budgeted(budget: WorkerBudget, memory: &FakeMemory) -> Result<(TestDag, String)> {
    let dag = TestDag::new(
        application(),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .with_worker_budget(FixedBudget(budget), memory.clone());
    let run = dag.create_run(
        BoxSelection {
            selector: Some("2".into()),
            caller: None,
        },
        "Budget".into(),
        Some("external".into()),
    )?;
    dag.add_tasks(&run.id, &[spec("a", &[]), spec("b", &[]), spec("c", &[])])?;
    Ok((dag, run.id))
}

fn task_dispatch(run: &str, task: &str, budget: BudgetMode) -> Dispatch {
    Dispatch {
        task: task.into(),
        budget,
        ..command(run, false)
    }
}

fn state_of(dag: &TestDag, run: &str, task: &str) -> Result<TaskState> {
    Ok(find_task(&dag.run(run)?, task)?.state)
}

#[test]
fn a_full_budget_queues_dispatches_and_reports_start_them_in_order() -> Result<()> {
    let memory = FakeMemory::default();
    memory.0.store(64 * domain::dag::GIB, Ordering::Relaxed);
    let budget = WorkerBudget {
        max_active_workers: Some(2),
        ..WorkerBudget::default()
    };
    let (dag, run) = budgeted(budget, &memory)?;
    let first = dag.start(task_dispatch(&run, "a", BudgetMode::Queue))?;
    dag.start(task_dispatch(&run, "b", BudgetMode::Queue))?;
    let DispatchOutcome::Queued { reason } =
        dag.dispatch(task_dispatch(&run, "c", BudgetMode::Queue))?
    else {
        bail!("The third dispatch started");
    };
    assert_eq!(reason, "2 of 2 workers are active (dag.max_active_workers)");
    assert_eq!(state_of(&dag, &run, "c")?, TaskState::Queued);
    assert_eq!(dag.run(&run)?.status(), RunStatus::Active);
    let queued = dag
        .run(&run)?
        .tasks
        .iter()
        .filter(|task| task.state == TaskState::Queued)
        .count();
    assert_eq!(queued, 1);
    let events = dag.events(
        EventScope::Run(&run, &[RunEventKind::TaskQueued]),
        0,
        Duration::ZERO,
    )?;
    assert_eq!(events.events.len(), 1);
    assert_eq!(events.events[0].summary.as_deref(), Some(reason.as_str()));

    // Dispatching a queued task again is refused rather than queued twice.
    let again = dag
        .dispatch(task_dispatch(&run, "c", BudgetMode::Queue))
        .err()
        .context("A queued task was dispatched twice")?;
    assert!(format!("{again:#}").contains("--over-budget"), "{again:#}");

    // Nothing frees up, so nothing starts.
    assert_eq!(dag.start_queued()?, 0);
    assert_eq!(state_of(&dag, &run, "c")?, TaskState::Queued);

    dag.report(&run, "a", &first.attempt.id, ReportStatus::Success, "done")?;
    assert_eq!(dag.start_queued()?, 1);
    assert_eq!(state_of(&dag, &run, "c")?, TaskState::Working);
    assert!(dag.store.read(|db| db.queued_dispatches())?.is_empty());
    Ok(())
}

#[test]
fn low_memory_holds_dispatch_until_memory_frees_up() -> Result<()> {
    let memory = FakeMemory::default();
    memory.0.store(domain::dag::GIB, Ordering::Relaxed);
    let (dag, run) = budgeted(WorkerBudget::default(), &memory)?;
    let DispatchOutcome::Queued { reason } =
        dag.dispatch(task_dispatch(&run, "a", BudgetMode::Queue))?
    else {
        bail!("Dispatch started with too little memory");
    };
    assert!(reason.contains("dag.min_available_memory"), "{reason}");
    assert_eq!(dag.start_queued()?, 0);
    memory.0.store(8 * domain::dag::GIB, Ordering::Relaxed);
    dag.archive_run(&run, false)?;
    assert_eq!(dag.start_queued()?, 0, "An archived run started a worker");
    dag.unarchive_run(&run)?;
    assert_eq!(dag.start_queued()?, 1);
    assert_eq!(state_of(&dag, &run, "a")?, TaskState::Working);
    Ok(())
}

#[test]
fn refuse_fails_and_override_starts_over_the_budget() -> Result<()> {
    let memory = FakeMemory::default();
    let (dag, run) = budgeted(WorkerBudget::default(), &memory)?;
    let refused = dag
        .dispatch(task_dispatch(&run, "a", BudgetMode::Refuse))
        .err()
        .context("Refuse started a worker with no memory")?;
    assert!(
        format!("{refused:#}").contains("The worker budget is full"),
        "{refused:#}"
    );
    assert_eq!(state_of(&dag, &run, "a")?, TaskState::Ready);

    dag.start(task_dispatch(&run, "a", BudgetMode::Override))?;
    assert_eq!(state_of(&dag, &run, "a")?, TaskState::Working);

    // Override also starts a queued task at once, from the CLI or the stored dispatch.
    dag.dispatch(task_dispatch(&run, "b", BudgetMode::Queue))?;
    dag.dispatch(task_dispatch(&run, "c", BudgetMode::Queue))?;
    dag.start(task_dispatch(&run, "b", BudgetMode::Override))?;
    dag.start_queued_now(&run, "c")?;
    for task in ["b", "c"] {
        assert_eq!(state_of(&dag, &run, task)?, TaskState::Working);
    }
    assert!(dag.store.read(|db| db.queued_dispatches())?.is_empty());
    assert!(dag.start_queued_now(&run, "c").is_err());
    Ok(())
}

#[test]
fn per_run_caps_let_other_runs_start() -> Result<()> {
    let memory = FakeMemory::default();
    memory.0.store(64 * domain::dag::GIB, Ordering::Relaxed);
    let budget = WorkerBudget {
        max_active_workers: Some(10),
        max_active_workers_per_run: Some(1),
        ..WorkerBudget::default()
    };
    let (dag, run) = budgeted(budget, &memory)?;
    let other = external_run(&dag)?;
    dag.start(task_dispatch(&run, "a", BudgetMode::Queue))?;
    assert!(matches!(
        dag.dispatch(task_dispatch(&run, "b", BudgetMode::Queue))?,
        DispatchOutcome::Queued { .. }
    ));
    dag.start(task_dispatch(&other, "work", BudgetMode::Queue))?;
    assert_eq!(state_of(&dag, &run, "b")?, TaskState::Queued);
    Ok(())
}

#[test]
fn a_queued_dispatch_that_cannot_start_leaves_the_queue_with_an_event() -> Result<()> {
    let memory = FakeMemory::default();
    let (dag, run) = budgeted(WorkerBudget::default(), &memory)?;
    dag.dispatch(Dispatch {
        args: Vec::new(),
        ..task_dispatch(&run, "a", BudgetMode::Queue)
    })?;
    memory.0.store(64 * domain::dag::GIB, Ordering::Relaxed);
    assert_eq!(dag.start_queued()?, 0);
    assert_eq!(state_of(&dag, &run, "a")?, TaskState::Ready);
    let failed = dag.events(
        EventScope::Run(&run, &[RunEventKind::QueuedDispatchFailed]),
        0,
        Duration::ZERO,
    )?;
    assert_eq!(failed.events.len(), 1);
    assert!(
        failed.events[0]
            .summary
            .as_deref()
            .is_some_and(|summary| summary.contains("Specify a command")),
        "{:?}",
        failed.events[0].summary
    );
    Ok(())
}

fn launched_args(dag: &TestDag) -> Vec<Vec<OsString>> {
    dag.application
        .runtime
        .launches
        .lock()
        .unwrap()
        .iter()
        .map(|(_, options)| options.args.clone())
        .collect()
}

#[test]
fn a_paused_scheduler_never_starts_or_removes_a_replaced_queue_entry() -> Result<()> {
    let memory = FakeMemory::default();
    memory.0.store(64 * domain::dag::GIB, Ordering::Relaxed);
    let budget = WorkerBudget {
        max_active_workers: Some(1),
        ..WorkerBudget::default()
    };
    let (dag, run) = budgeted(budget, &memory)?;
    let with_args = |args: &str, retry: bool| Dispatch {
        args: vec![args.into()],
        retry,
        ..task_dispatch(&run, "b", BudgetMode::Queue)
    };
    let first = dag.start(task_dispatch(&run, "a", BudgetMode::Queue))?;
    dag.dispatch(with_args("old", false))?;
    // The scheduler reads the queue, then pauses before it dispatches.
    let snapshot = dag.store.read(|db| db.queued_dispatches())?;
    assert_eq!(snapshot.len(), 1);
    let stale = snapshot[0].id.clone();

    // Meanwhile the old request starts by override and exits, and a retry queues a new one.
    let old = dag.start_queued_now(&run, "b")?;
    dag.session_exited(old.agent.session.as_deref().unwrap(), "exited", true)?;
    dag.dispatch(with_args("new", true))?;
    let replacement = dag.store.read(|db| db.queued_dispatch(&run, "b"))?;
    let replacement = replacement.context("The retry was not queued")?;
    assert_ne!(replacement.id, stale);
    dag.report(&run, "a", &first.attempt.id, ReportStatus::Success, "done")?;

    // The scheduler resumes with its stale snapshot. The old command must not launch again,
    // and the new request stays queued.
    let launches = launched_args(&dag).len();
    assert_eq!(dag.start_queued_entries(snapshot)?, 0);
    assert_eq!(launched_args(&dag).len(), launches);
    assert_eq!(
        dag.store.read(|db| db.queued_dispatches())?,
        std::slice::from_ref(&replacement)
    );
    assert_eq!(state_of(&dag, &run, "b")?, TaskState::Queued);

    // A failure cleanup for the stale entry leaves the replacement alone too.
    assert!(!dag.store.write(|db| db.remove_queued(&stale))?);
    assert_eq!(dag.start_queued()?, 1);
    assert_eq!(
        launched_args(&dag).last(),
        Some(&vec![OsString::from("new")])
    );
    assert!(dag.store.read(|db| db.queued_dispatches())?.is_empty());
    Ok(())
}

#[test]
fn a_queued_dispatch_with_a_missing_base_is_refused_and_not_stored() -> Result<()> {
    let memory = FakeMemory::default();
    memory.0.store(64 * domain::dag::GIB, Ordering::Relaxed);
    let budget = WorkerBudget {
        max_active_workers: Some(1),
        ..WorkerBudget::default()
    };
    let dag = TestDag::new(
        application_with_workspace(FakeWorkspace::default()),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .with_worktree_root(FakeWorktreeRoot)
    .with_worker_budget(FixedBudget(budget), memory);
    let run = external_run(&dag)?;
    dag.add_tasks(&run, &[spec("queued", &[])])?;
    dag.start(command(&run, false))?;
    let before = dag.latest_seq()?;
    let error = dag
        .dispatch(Dispatch {
            task: "queued".into(),
            ..new_worktree(&run, "feature/queued", Some("missing-base"))
        })
        .err()
        .context("Dispatch unexpectedly queued")?;
    assert!(format!("{error:#}").contains("missing-base"), "{error:#}");
    assert!(dag.store.read(|db| db.queued_dispatches())?.is_empty());
    assert_eq!(dag.latest_seq()?, before);
    Ok(())
}

#[test]
fn a_queued_explicit_base_starts_on_the_commit_it_named_when_queued() -> Result<()> {
    let workspace = FakeWorkspace::default();
    {
        let mut git = workspace.0.lock().unwrap();
        git.commits.extend(["commit-1".into(), "commit-2".into()]);
        git.refs.insert("main".into(), "commit-1".into());
    }
    let memory = FakeMemory::default();
    memory.0.store(64 * domain::dag::GIB, Ordering::Relaxed);
    let budget = WorkerBudget {
        max_active_workers: Some(1),
        ..WorkerBudget::default()
    };
    let dag = TestDag::new(
        application_with_workspace(workspace.clone()),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .with_worktree_root(FakeWorktreeRoot)
    .with_worker_budget(FixedBudget(budget), memory);
    let run = external_run(&dag)?;
    dag.add_tasks(&run, &[spec("queued", &[])])?;
    let first = dag.start(command(&run, false))?;
    let queued = Dispatch {
        task: "queued".into(),
        ..new_worktree(&run, "feature/queued", Some("main"))
    };
    assert!(matches!(
        dag.dispatch(queued)?,
        DispatchOutcome::Queued { .. }
    ));
    let entry = dag.store.read(|db| db.queued_dispatch(&run, "queued"))?;
    assert!(matches!(
        entry.context("Not queued")?.worktree,
        WorktreeRequest::New { base: Some(base), .. } if base == "commit-1"
    ));

    // The branch moves while the dispatch waits.
    workspace
        .0
        .lock()
        .unwrap()
        .refs
        .insert("main".into(), "commit-2".into());
    dag.report(
        &run,
        "work",
        &first.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    assert_eq!(dag.start_queued()?, 1);
    let git = workspace.0.lock().unwrap();
    let (_, branch, base) = git.creation_requests.last().context("No worktree")?;
    assert_eq!(
        (branch.as_str(), base.as_str()),
        ("feature/queued", "commit-1")
    );
    Ok(())
}

#[test]
fn a_slow_reclaim_retry_and_its_blocked_write_never_delay_another_workers_teardown() -> Result<()> {
    let records = Arc::new(BlockedSessions::default());
    static ID: AtomicU64 = AtomicU64::new(0);
    let application = Arc::new(Application::new(
        FakeArtifacts::default(),
        records.clone(),
        FakeWorkspace::default(),
        FakeTerminal::default(),
        Arc::new(FakeProfiles(Vec::new())),
        || format!("retry-{}", ID.fetch_add(1, Ordering::Relaxed)),
    ));
    let dag = TestDag::new(
        application,
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .with_finished_worker_cleanup(
        Duration::from_millis(50),
        SerialScheduler::start(),
        SerialScheduler::start(),
    )
    .shared();
    let run = external_run(&dag)?;
    dag.add_tasks(&run, &[spec("second", &[])])?;
    let runtime = &dag.application.runtime;

    // The first worker ends incompletely, so a reclaim retry is due on the timer.
    let first = dag.start(command(&run, false))?;
    let first_session = first.agent.session.unwrap();
    dag.report(
        &run,
        "work",
        &first.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while dag.application.terminal_running(&first_session) {
        assert!(Instant::now() < deadline, "the first teardown never ran");
        thread::sleep(Duration::from_millis(10));
    }
    runtime.reclaim_millis.store(3000, Ordering::Release);
    dag.session_exited(&first_session, "a helper still runs", false)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime.reclaims.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline, "the retry never started");
        thread::sleep(Duration::from_millis(10));
    }

    // While that reclaim waits, and then while its record delete blocks, the second
    // worker's teardown is still due on time.
    let second = dag.start(Dispatch {
        task: "second".into(),
        ..command(&run, false)
    })?;
    let second_session = second.agent.session.unwrap();
    let handle = dag.application.terminal(&second_session)?;
    let reported = Instant::now();
    dag.report(
        &run,
        "second",
        &second.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    while handle.load(Ordering::Acquire) {
        assert!(
            reported.elapsed() < Duration::from_secs(2),
            "a slow reclaim retry held up the next worker's teardown"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while records.waiting.load(Ordering::Acquire) == 0 {
        assert!(
            Instant::now() < deadline,
            "the retry never wrote its record"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let third = {
        dag.add_tasks(&run, &[spec("third", &[])])?;
        dag.start(Dispatch {
            task: "third".into(),
            ..command(&run, false)
        })?
    };
    let handle = dag
        .application
        .terminal(third.agent.session.as_ref().unwrap())?;
    let reported = Instant::now();
    dag.report(
        &run,
        "third",
        &third.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    while handle.load(Ordering::Acquire) {
        assert!(
            reported.elapsed() < Duration::from_secs(2),
            "a retry's blocked record write held up the next worker's teardown"
        );
        thread::sleep(Duration::from_millis(10));
    }
    records.open();
    let agent = dag
        .store
        .read(|db| db.session_agent(&first_session))?
        .unwrap();
    assert_eq!(agent.state, AgentState::Exited);
    Ok(())
}

fn terminal_paused(dag: &TestDag, session: &str) -> Result<bool> {
    let handle = dag.application.terminal(session)?;
    let key = Arc::as_ptr(&handle) as usize;
    Ok(dag
        .application
        .runtime
        .paused
        .lock()
        .unwrap()
        .contains(&key))
}

#[test]
fn concurrent_run_pause_and_resume_leave_every_worker_matching_the_final_flag() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(&run, &[spec("two", &[])])?;
    let first = dag.start(command(&run, false))?;
    let second = dag.start(Dispatch {
        task: "two".into(),
        ..command(&run, false)
    })?;
    let sessions = [
        first.agent.session.clone().unwrap(),
        second.agent.session.clone().unwrap(),
    ];
    // Each pause takes long enough that an unserialized resume would snapshot the
    // workers before the pause reached them.
    dag.application
        .runtime
        .pause_millis
        .store(20, Ordering::Release);
    for round in 0..8 {
        let start = Barrier::new(2);
        thread::scope(|scope| {
            let pause = scope.spawn(|| {
                start.wait();
                dag.pause_run(&run)
            });
            let resume = scope.spawn(|| {
                start.wait();
                dag.resume_run(&run)
            });
            pause.join().unwrap().unwrap();
            resume.join().unwrap().unwrap();
        });
        let flag = dag.run(&run)?.paused_at.is_some();
        for (session, agent) in sessions.iter().zip([&first.agent.id, &second.agent.id]) {
            let record = dag.store.read(|db| db.agent(agent))?;
            assert_eq!(record.paused, flag, "round {round}: record of {agent}");
            assert_eq!(
                terminal_paused(&dag, session)?,
                flag,
                "round {round}: processes of {agent}"
            );
        }
    }
    Ok(())
}

#[test]
fn a_restart_between_a_run_pause_and_its_workers_pauses_them() -> Result<()> {
    let (dag, run) = setup()?;
    let started = dag.start(command(&run, false))?;
    let session = started.agent.session.clone().unwrap();
    // The flag committed, then the daemon ended before the worker was paused.
    dag.write(|db| db.set_run_paused(&run, Some(now())))?;
    assert!(!terminal_paused(&dag, &session)?);
    dag.reapply_pauses(std::slice::from_ref(&session))?;
    assert!(dag.store.read(|db| db.agent(&started.agent.id))?.paused);
    assert!(terminal_paused(&dag, &session)?);
    Ok(())
}

#[test]
fn a_report_after_a_stop_begins_never_finishes_the_attempt() -> Result<()> {
    let (dag, run) = setup()?;
    let started = dag.start(command(&run, false))?;
    dag.stop_agent(&started.agent.id)?;
    // A worker that ignores SIGTERM reports while its teardown is still under way.
    let late = dag.report(
        &run,
        "work",
        &started.attempt.id,
        ReportStatus::Success,
        "Reported after stop",
    )?;
    assert!(!late.applied);
    assert_eq!(late.reason.as_deref(), Some("agent is stopping"));
    // Until the exit is confirmed, the attempt stays open: no retry, slot kept.
    let attempt = dag.run(&run)?.tasks[0].attempts[0].clone();
    assert_eq!(attempt.state, AttemptState::Active);
    assert!(attempt.summary.is_none(), "{attempt:?}");
    assert!(dag.start(command(&run, true)).is_err());
    let session = started.agent.session.clone().unwrap();
    dag.session_exited(&session, "ended, but a helper still runs", false)?;
    let attempt = dag.run(&run)?.tasks[0].attempts[0].clone();
    assert_eq!(attempt.state, AttemptState::Active);
    dag.session_exited(&session, "ended", true)?;
    let attempt = dag.run(&run)?.tasks[0].attempts[0].clone();
    assert_eq!(attempt.state, AttemptState::Interrupted);
    assert_eq!(attempt.detail.as_deref(), Some("stopped by Power"));
    assert!(attempt.summary.is_none(), "{attempt:?}");
    dag.start(command(&run, true))?;
    Ok(())
}

#[test]
fn a_coding_agent_in_a_shell_foreground_is_never_paused_or_stopped() -> Result<()> {
    let (dag, _) = setup()?;
    let shell = dag.application.start_session(StartSession {
        worktree: "/worktree".into(),
        agent: Agent::Shell,
    })?;
    // The identity Power derives for a coding agent in a shell tab's foreground.
    let mut agent = dag.new_agent(
        "2".into(),
        "/worktree".into(),
        Provider::Agent("claude".into()),
        None,
        "claude".into(),
    );
    agent.session = Some(shell.id.clone());
    dag.write(|db| db.insert_agent(&agent))?;

    let paused = dag.set_agent_paused(&agent.id, true).unwrap_err();
    assert!(format!("{paused:#}").contains("foreground"), "{paused:#}");
    let stopped = dag.stop_agent(&agent.id).unwrap_err();
    assert!(format!("{stopped:#}").contains("foreground"), "{stopped:#}");
    assert!(!terminal_paused(&dag, &shell.id)?);
    assert!(dag.application.session_is_running(&shell.id)?);
    assert!(!dag.store.read(|db| db.agent(&agent.id))?.paused);
    // Nor by the emergency pause, which skips what it cannot isolate.
    assert!(dag.pause_all_except(&[], None)?.is_empty());
    assert!(!terminal_paused(&dag, &shell.id)?);
    Ok(())
}

#[test]
fn a_second_stop_never_lifts_the_first_stops_fence() -> Result<()> {
    let (dag, run) = setup()?;
    let dag = dag.shared();
    let started = dag.start(command(&run, false))?;
    let session = started.agent.session.clone().unwrap();
    dag.stop_agent(&started.agent.id)?;
    // The first teardown is still under way when the stop is repeated.
    dag.stop_agent(&started.agent.id)?;
    let late = dag.report(
        &run,
        "work",
        &started.attempt.id,
        ReportStatus::Success,
        "Reported after two stops",
    )?;
    assert!(!late.applied);
    assert_eq!(late.reason.as_deref(), Some("agent is stopping"));
    dag.session_exited(&session, "ended", true)?;
    let attempt = dag.run(&run)?.tasks[0].attempts[0].clone();
    assert_eq!(attempt.state, AttemptState::Interrupted);
    assert_eq!(attempt.detail.as_deref(), Some("stopped by Power"));
    Ok(())
}

#[test]
fn a_stopped_session_keeps_its_record_until_its_exit_is_confirmed() -> Result<()> {
    let (dag, run) = setup()?;
    let dag = dag.shared();
    let started = dag.start(command(&run, false))?;
    let session = started.agent.session.clone().unwrap();
    dag.stop_agent(&started.agent.id)?;
    assert_eq!(dag.application.records.list()?.len(), 1);
    assert!(
        dag.store
            .read(|db| db.stopping_sessions())?
            .contains(&session)
    );
    dag.session_exited(&session, "ended", true)?;
    assert!(dag.application.records.list()?.is_empty());
    assert!(dag.store.read(|db| db.stopping_sessions())?.is_empty());
    Ok(())
}

#[test]
fn a_crash_during_a_stop_holds_the_attempt_until_a_restart_reclaims_the_tree() -> Result<()> {
    let (dag, run) = setup()?;
    let dag = dag.shared();
    let started = dag.start(command(&run, false))?;
    let session = started.agent.session.clone().unwrap();
    dag.stop_agent(&started.agent.id)?;
    // The daemon dies before the exit is confirmed, and its successor cannot yet end
    // what is left of the stopped tree.
    dag.application.sessions.lock().unwrap().clear();
    dag.application
        .runtime
        .fail_reclaim
        .store(true, Ordering::Release);
    let launches = dag.application.runtime.launches.lock().unwrap().len();
    let restored = dag.restore()?;
    assert!(restored.is_empty(), "{restored:?}");
    assert_eq!(
        dag.application.runtime.launches.lock().unwrap().len(),
        launches
    );
    let attempt = dag.run(&run)?.tasks[0].attempts[0].clone();
    assert_eq!(attempt.state, AttemptState::Active, "{attempt:?}");
    assert!(dag.start(command(&run, true)).is_err());
    let late = dag.report(
        &run,
        "work",
        &started.attempt.id,
        ReportStatus::Success,
        "Reported after the restart",
    )?;
    assert!(!late.applied);
    assert_eq!(dag.application.records.list()?.len(), 1);

    // Once the tree is gone, the stop completes and the task can run again.
    dag.application
        .runtime
        .fail_reclaim
        .store(false, Ordering::Release);
    dag.retry_reclaim(session.clone());
    let attempt = dag.run(&run)?.tasks[0].attempts[0].clone();
    assert_eq!(attempt.state, AttemptState::Interrupted);
    assert_eq!(attempt.detail.as_deref(), Some("stopped by Power"));
    assert!(dag.application.records.list()?.is_empty());
    dag.start(command(&run, true))?;
    Ok(())
}

#[test]
fn a_restart_that_reclaims_a_stopped_tree_completes_the_stop() -> Result<()> {
    let (dag, run) = setup()?;
    let dag = dag.shared();
    let started = dag.start(command(&run, false))?;
    dag.stop_agent(&started.agent.id)?;
    dag.application.sessions.lock().unwrap().clear();
    let launches = dag.application.runtime.launches.lock().unwrap().len();
    assert!(dag.restore()?.is_empty());
    assert_eq!(
        dag.application.runtime.launches.lock().unwrap().len(),
        launches,
        "a stopped session was relaunched"
    );
    let attempt = dag.run(&run)?.tasks[0].attempts[0].clone();
    assert_eq!(attempt.state, AttemptState::Interrupted);
    assert_eq!(attempt.detail.as_deref(), Some("stopped by Power"));
    assert!(dag.store.read(|db| db.stopping_sessions())?.is_empty());
    assert!(dag.application.records.list()?.is_empty());
    dag.start(command(&run, true))?;
    Ok(())
}

/// Refuses deletes while `fail` is set, as a session database with a transient
/// write fault does.
#[derive(Default)]
struct FlakySessions {
    records: MemorySessions,
    fail: AtomicBool,
}

impl SessionStore for FlakySessions {
    fn save(&self, record: &crate::SessionRecord) -> Result<()> {
        self.records.save(record)
    }

    fn update_foreground(
        &self,
        id: &str,
        foreground: &Agent,
        conversation: Option<&str>,
        cwd: Option<&Path>,
        labels: Option<&crate::Labels>,
    ) -> Result<()> {
        self.records
            .update_foreground(id, foreground, conversation, cwd, labels)
    }

    fn update_labels(&self, id: &str, labels: &crate::Labels) -> Result<()> {
        self.records.update_labels(id, labels)
    }

    fn remove(&self, id: &str) -> Result<()> {
        if self.fail.load(Ordering::Acquire) {
            bail!("disk I/O error");
        }
        self.records.remove(id)
    }

    fn list(&self) -> Result<Vec<crate::SessionRecord>> {
        self.records.list()
    }
}

#[test]
fn ending_a_stopped_session_keeps_its_slot_until_its_exit_is_confirmed() -> Result<()> {
    let memory = FakeMemory::default();
    memory.0.store(u64::MAX, Ordering::Relaxed);
    let budget = WorkerBudget {
        max_active_workers: Some(1),
        ..WorkerBudget::default()
    };
    let (dag, run) = budgeted(budget, &memory)?;
    let dag = dag.shared();
    let started = dag.start(task_dispatch(&run, "a", BudgetMode::Queue))?;
    let session = started.agent.session.clone().unwrap();
    dag.stop_agent(&started.agent.id)?;
    // `session end` after the stop must not discard what finds the stopped tree.
    dag.end_session(&session)?;
    assert_eq!(dag.application.records.list()?.len(), 1);
    // The daemon dies, and its successor cannot yet end the stopped tree.
    dag.application.sessions.lock().unwrap().clear();
    dag.application
        .runtime
        .fail_reclaim
        .store(true, Ordering::Release);
    assert!(dag.restore()?.is_empty());
    let retry = || Dispatch {
        retry: true,
        ..task_dispatch(&run, "a", BudgetMode::Refuse)
    };
    assert!(dag.start(retry()).is_err(), "retried before the exit");
    assert!(
        dag.start(task_dispatch(&run, "b", BudgetMode::Refuse))
            .is_err()
    );
    dag.application
        .runtime
        .fail_reclaim
        .store(false, Ordering::Release);
    dag.retry_reclaim(session);
    assert!(dag.application.records.list()?.is_empty());
    assert!(dag.store.read(|db| db.stopping_sessions())?.is_empty());
    dag.start(retry())?;
    Ok(())
}

#[test]
fn a_stopped_agent_whose_record_was_not_deleted_is_never_relaunched() -> Result<()> {
    let records = Arc::new(FlakySessions::default());
    let dag = TestDag::new(
        application_with(FakeWorkspace::default(), records.clone()),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .shared();
    let worktree = tempfile::tempdir()?;
    let agent = dag.start_agent(start_agent(WorktreeRequest::Existing {
        path: "/worktree".into(),
        allow_main_checkout: false,
    }))?;
    let session = agent.session.clone().unwrap();
    for mut record in records.list()? {
        record.worktree = worktree.path().into();
        records.save(&record)?;
    }
    dag.stop_agent(&agent.id)?;
    // The exit is confirmed, but deleting the record fails, as a crash before the
    // deletion would leave it.
    records.fail.store(true, Ordering::Release);
    assert!(dag.session_exited(&session, "ended", true).is_err());
    assert_eq!(records.list()?.len(), 1);
    assert!(
        dag.store
            .read(|db| db.stopping_sessions())?
            .contains(&session)
    );
    // The restarted daemon, with the fault gone, forgets the stopped agent.
    records.fail.store(false, Ordering::Release);
    dag.application.sessions.lock().unwrap().clear();
    let launches = dag.application.runtime.launches.lock().unwrap().len();
    assert!(dag.restore()?.is_empty());
    assert_eq!(
        dag.application.runtime.launches.lock().unwrap().len(),
        launches,
        "a stopped agent was relaunched"
    );
    assert!(records.list()?.is_empty());
    assert!(dag.store.read(|db| db.stopping_sessions())?.is_empty());
    Ok(())
}

/// A worker probe whose idle time and child work tests set per session.
#[derive(Clone, Default)]
struct FakeProbe(Arc<Mutex<BTreeMap<String, (Duration, bool)>>>);

impl FakeProbe {
    fn set(&self, session: &str, idle: Duration, working: bool) {
        self.0
            .lock()
            .unwrap()
            .insert(session.into(), (idle, working));
    }
}

impl WorkerProbe for FakeProbe {
    fn idle_for(&self, session: &str) -> Option<Duration> {
        self.0.lock().unwrap().get(session).map(|entry| entry.0)
    }

    fn commands_running(&self, session: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(session)
            .is_some_and(|entry| entry.1)
    }
}

#[test]
fn an_idle_worker_with_no_working_children_is_flagged_stalled_once() -> Result<()> {
    let probe = FakeProbe::default();
    let dag = TestDag::new(
        application(),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .with_stall_check(Duration::from_secs(60), probe.clone());
    let run = external_run(&dag)?;
    dag.add_tasks(&run, &[spec("busy", &[])])?;
    let idle = dag.start(command(&run, false))?;
    let busy = dag.start(Dispatch {
        task: "busy".into(),
        ..command(&run, false)
    })?;
    let idle_session = idle.agent.session.clone().unwrap();
    let busy_session = busy.agent.session.clone().unwrap();
    let before = dag.latest_seq()?;

    // Under the threshold, nothing is flagged.
    probe.set(&idle_session, Duration::from_secs(30), false);
    probe.set(&busy_session, Duration::from_secs(600), true);
    assert!(dag.check_stalls()?.is_empty());

    // Past it, only the worker whose children do no work is flagged, and only once.
    probe.set(&idle_session, Duration::from_secs(61), false);
    assert_eq!(dag.check_stalls()?, std::slice::from_ref(&idle.attempt.id));
    assert!(dag.check_stalls()?.is_empty());
    let page = dag.events(EventScope::Run(&run, &[]), before, Duration::ZERO)?;
    let stalled: Vec<_> = page
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::AttemptStalled)
        .map(|event| event.attempt.as_deref())
        .collect();
    assert_eq!(stalled, [Some(idle.attempt.id.as_str())]);
    let shown = dag.run(&run)?;
    let flags: BTreeMap<_, _> = shown
        .tasks
        .iter()
        .map(|task| (task.id.as_str(), task.attempts[0].stalled))
        .collect();
    assert_eq!(flags, BTreeMap::from([("work", true), ("busy", false)]));

    // Activity ends the idle period, and a later one is flagged again.
    probe.set(&idle_session, Duration::from_secs(1), false);
    assert!(dag.check_stalls()?.is_empty());
    assert!(!dag.run(&run)?.tasks[0].attempts[0].stalled);
    probe.set(&idle_session, Duration::from_secs(61), false);
    assert_eq!(dag.check_stalls()?, [idle.attempt.id]);
    Ok(())
}

/// A probe that, while it samples, lets a hook change the worker, as a report or an
/// exit could between the read and the write of a stall check.
struct RacingProbe {
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl WorkerProbe for RacingProbe {
    fn idle_for(&self, _: &str) -> Option<Duration> {
        if let Some(hook) = self.hook.lock().unwrap().take() {
            hook();
        }
        Some(Duration::from_secs(3600))
    }

    fn commands_running(&self, _: &str) -> bool {
        false
    }
}

#[test]
fn an_attempt_that_reports_while_it_is_sampled_is_not_flagged_stalled() -> Result<()> {
    let late: Arc<Mutex<Option<Arc<TestDag>>>> = Arc::default();
    let report: Arc<Mutex<Option<(String, String)>>> = Arc::default();
    let hook = {
        let late = late.clone();
        let report = report.clone();
        Box::new(move || {
            let dag = late.lock().unwrap().clone().unwrap();
            let (run, attempt) = report.lock().unwrap().clone().unwrap();
            dag.report(&run, "work", &attempt, ReportStatus::Success, "Done")
                .unwrap();
        }) as Box<dyn FnOnce() + Send>
    };
    let dag = TestDag::new(
        application(),
        MemoryStore::default(),
        FakeRegistry::default(),
        FakeIcons::default(),
    )
    .with_stall_check(
        Duration::from_secs(60),
        RacingProbe {
            hook: Mutex::new(Some(hook)),
        },
    )
    .shared();
    *late.lock().unwrap() = Some(dag.clone());
    let run = external_run(&dag)?;
    let started = dag.start(command(&run, false))?;
    *report.lock().unwrap() = Some((run.clone(), started.attempt.id.clone()));
    let before = dag.latest_seq()?;

    assert!(dag.check_stalls()?.is_empty());
    let page = dag.events(EventScope::Run(&run, &[]), before, Duration::ZERO)?;
    assert!(
        !page
            .events
            .iter()
            .any(|event| event.kind == RunEventKind::AttemptStalled),
        "{:?}",
        page.events
    );
    assert!(!dag.run(&run)?.tasks[0].attempts[0].stalled);
    *late.lock().unwrap() = None;
    Ok(())
}

/// Caller, run, agent and body of one message.
type SentEdit = (Option<String>, String, String, String);

#[derive(Default)]
struct FakeMessenger {
    sent: Mutex<Vec<SentEdit>>,
    fail: bool,
}

impl TaskMessenger for FakeMessenger {
    fn send_to_worker(
        &self,
        caller: Option<String>,
        run: &str,
        agent: &str,
        body: String,
    ) -> Result<String> {
        if self.fail {
            bail!("Talk target is gone");
        }
        let mut sent = self.sent.lock().unwrap();
        sent.push((caller, run.into(), agent.into(), body));
        Ok(format!("message-{}", sent.len()))
    }
}

fn edit(run: &str, task: &str, title: Option<&str>, instructions: Option<&str>) -> EditTask {
    EditTask {
        run: run.into(),
        task: task.into(),
        title: title.map(str::to_owned),
        instructions: instructions.map(str::to_owned),
        caller: Some("coordinator-session".into()),
    }
}

fn edit_events(dag: &TestDag, run: &str) -> Result<Vec<RunEvent>> {
    Ok(dag
        .events(
            EventScope::Run(run, &[RunEventKind::TaskEdited]),
            0,
            Duration::ZERO,
        )?
        .events)
}

fn agent_dispatch(run: &str, task: &str, retry: bool) -> Dispatch {
    Dispatch {
        task: task.into(),
        provider: Some(Provider::Agent("codex".into())),
        args: Vec::new(),
        ..command(run, retry)
    }
}

#[test]
fn editing_a_waiting_or_ready_task_updates_the_next_dispatch_without_a_message() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(&run, &[spec("later", &["work"])])?;
    let messenger = FakeMessenger::default();
    let edited = dag.edit_task(
        edit(&run, "later", None, Some("Use the new plan")),
        &messenger,
    )?;
    assert_eq!(edited.task.state, TaskState::Waiting);
    assert_eq!(edited.task.instructions, "Use the new plan");
    assert_eq!(edited.task.title, "later");
    assert!(matches!(edited.delivery, EditDelivery::None));
    let edited = dag.edit_task(edit(&run, "work", Some("Renamed"), None), &messenger)?;
    assert_eq!(edited.task.state, TaskState::Ready);
    assert_eq!(edited.task.title, "Renamed");
    assert_eq!(edited.task.instructions, "Implement work");
    assert!(messenger.sent.lock().unwrap().is_empty());

    let events = edit_events(&dag, &run)?;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].task.as_deref(), Some("later"));
    assert_eq!(events[0].attempt, None);
    assert_eq!(events[0].summary.as_deref(), Some("Updated instructions"));
    assert_eq!(events[1].summary.as_deref(), Some("Updated title"));

    dag.start(agent_dispatch(&run, "work", false))?;
    let prompt = last_worker_prompt(&dag);
    assert!(prompt.contains("Task: work — Renamed\n"), "{prompt}");
    Ok(())
}

#[test]
fn editing_a_running_task_messages_its_current_attempt() -> Result<()> {
    let (dag, run) = setup()?;
    let started = dag.start(command(&run, false))?;
    let messenger = FakeMessenger::default();
    let edited = dag.edit_task(
        edit(&run, "work", Some("Fix it"), Some("Do it the new way")),
        &messenger,
    )?;
    assert_eq!(edited.task.state, TaskState::Working);
    let EditDelivery::Sent {
        agent,
        attempt,
        message,
    } = &edited.delivery
    else {
        panic!("Expected a sent message");
    };
    assert_eq!(agent, &started.agent.id);
    assert_eq!(attempt, &started.attempt.id);
    assert_eq!(message, "message-1");
    {
        let sent = messenger.sent.lock().unwrap();
        let (caller, to_run, to_agent, body) = &sent[0];
        assert_eq!(caller.as_deref(), Some("coordinator-session"));
        assert_eq!((to_run, to_agent), (&run, &started.agent.id));
        assert!(body.contains("replace your earlier instructions"), "{body}");
        assert!(body.contains("Task: work — Fix it"), "{body}");
        assert!(body.ends_with("Instructions:\nDo it the new way"), "{body}");
    }
    let events = edit_events(&dag, &run)?;
    assert_eq!(
        events[0].attempt.as_deref(),
        Some(started.attempt.id.as_str())
    );
    assert_eq!(
        events[0].summary.as_deref(),
        Some("Updated title and instructions")
    );

    // A worker waiting on input is still running, so it is told as well.
    dag.report(
        &run,
        "work",
        &started.attempt.id,
        ReportStatus::NeedsInput,
        "Which way?",
    )?;
    let edited = dag.edit_task(edit(&run, "work", None, Some("Second fix")), &messenger)?;
    assert_eq!(edited.task.state, TaskState::NeedsInput);
    assert!(matches!(edited.delivery, EditDelivery::Sent { .. }));
    assert_eq!(messenger.sent.lock().unwrap().len(), 2);
    Ok(())
}

#[test]
fn a_failed_delivery_keeps_the_stored_edit() -> Result<()> {
    let (dag, run) = setup()?;
    dag.start(command(&run, false))?;
    let messenger = FakeMessenger {
        fail: true,
        ..FakeMessenger::default()
    };
    let edited = dag.edit_task(edit(&run, "work", None, Some("Kept")), &messenger)?;
    assert!(
        matches!(&edited.delivery, EditDelivery::Failed { error, .. } if error.contains("gone"))
    );
    assert_eq!(dag.run(&run)?.tasks[0].instructions, "Kept");
    Ok(())
}

#[test]
fn editing_a_failed_or_interrupted_task_updates_the_retry_without_a_message() -> Result<()> {
    let (dag, run) = setup()?;
    dag.add_tasks(&run, &[spec("failing", &[])])?;
    let interrupted = dag.start(command(&run, false))?;
    dag.session_exited(
        interrupted.agent.session.as_deref().unwrap(),
        "exited",
        true,
    )?;
    let failing = dag.start(Dispatch {
        task: "failing".into(),
        ..command(&run, false)
    })?;
    dag.report(
        &run,
        "failing",
        &failing.attempt.id,
        ReportStatus::Failed,
        "broken",
    )?;
    let messenger = FakeMessenger::default();
    for (task, state) in [
        ("work", TaskState::Interrupted),
        ("failing", TaskState::Failed),
    ] {
        let edited = dag.edit_task(edit(&run, task, None, Some("Retry this way")), &messenger)?;
        assert_eq!(edited.task.state, state);
        assert!(matches!(edited.delivery, EditDelivery::None));
    }
    assert!(messenger.sent.lock().unwrap().is_empty());
    dag.start(agent_dispatch(&run, "failing", true))?;
    let prompt = last_worker_prompt(&dag);
    assert!(
        prompt.contains("Instructions:\nRetry this way\n"),
        "{prompt}"
    );
    Ok(())
}

#[test]
fn edits_refuse_completed_tasks_unknown_targets_and_empty_text() -> Result<()> {
    let (dag, run) = setup()?;
    let started = dag.start(command(&run, false))?;
    dag.report(
        &run,
        "work",
        &started.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    let messenger = FakeMessenger::default();
    let before = dag.latest_seq()?;
    let error = |command| {
        dag.edit_task(command, &messenger)
            .err()
            .map(|error| error.to_string())
            .unwrap()
    };
    assert!(error(edit(&run, "work", Some("New"), None)).contains("Add a new task instead"));
    assert!(error(edit(&run, "missing", Some("New"), None)).contains("Unknown task"));
    assert!(error(edit("nope", "work", Some("New"), None)).contains("Unknown run"));
    assert!(error(edit(&run, "work", Some("  "), None)).contains("title is empty"));
    assert!(error(edit(&run, "work", None, Some(""))).contains("instructions are empty"));
    assert!(error(edit(&run, "work", None, None)).contains("Specify --title"));
    assert_eq!(dag.latest_seq()?, before);
    assert_eq!(dag.run(&run)?.tasks[0].title, "work");
    assert!(messenger.sent.lock().unwrap().is_empty());
    Ok(())
}

#[test]
fn a_paused_worker_reads_the_current_instructions_when_it_resumes() -> Result<()> {
    let (dag, run) = setup()?;
    let started = dag.start(command(&run, false))?;
    let session = started.agent.session.clone().unwrap();
    dag.pause_run(&run)?;
    assert_eq!(dag.run(&run)?.tasks[0].state, TaskState::Paused);
    let messenger = FakeMessenger::default();
    let edited = dag.edit_task(edit(&run, "work", None, Some("Paused fix")), &messenger)?;
    assert_eq!(edited.task.state, TaskState::Paused);
    assert!(
        matches!(&edited.delivery, EditDelivery::Sent { agent, .. } if agent == &started.agent.id)
    );
    // The edit leaves the worker paused. Its message waits in the inbox.
    assert!(terminal_paused(&dag, &session)?);
    dag.resume_run(&run)?;
    assert!(!terminal_paused(&dag, &session)?);
    let task = dag.run(&run)?.tasks[0].clone();
    assert_eq!(task.state, TaskState::Working);
    // Same worker and attempt, and the one message it reads matches what is stored.
    assert_eq!(task.attempts.len(), 1);
    assert_eq!(
        task.current_attempt.as_deref(),
        Some(started.attempt.id.as_str())
    );
    let sent = messenger.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].2, started.agent.id);
    assert!(
        sent[0].3.ends_with("Instructions:\nPaused fix"),
        "{}",
        sent[0].3
    );
    Ok(())
}

/// Holds the first send until the test releases it, and reports when it is inside.
struct GatedFirstMessenger {
    inner: FakeMessenger,
    first: std::sync::Once,
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl TaskMessenger for GatedFirstMessenger {
    fn send_to_worker(
        &self,
        caller: Option<String>,
        run: &str,
        agent: &str,
        body: String,
    ) -> Result<String> {
        let mut gated = false;
        self.first.call_once(|| gated = true);
        if gated {
            self.entered.lock().unwrap().send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .expect("The test releases the first send");
        }
        self.inner.send_to_worker(caller, run, agent, body)
    }
}

#[test]
fn crossed_edits_enqueue_messages_in_commit_order() -> Result<()> {
    let (dag, run) = setup()?;
    dag.start(command(&run, false))?;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let messenger = GatedFirstMessenger {
        inner: FakeMessenger::default(),
        first: std::sync::Once::new(),
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    };
    thread::scope(|scope| {
        let first =
            scope.spawn(|| dag.edit_task(edit(&run, "work", None, Some("Plan A")), &messenger));
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("Edit A reaches its send");
        // A has committed and is inside its send, still holding the edit lock.
        assert!(
            dag.task_edits.try_lock().is_err(),
            "The edit lock is held through the send"
        );
        let second =
            scope.spawn(|| dag.edit_task(edit(&run, "work", None, Some("Plan B")), &messenger));
        assert!(messenger.inner.sent.lock().unwrap().is_empty());
        release_tx.send(()).unwrap();
        assert!(matches!(
            first.join().unwrap().unwrap().delivery,
            EditDelivery::Sent { .. }
        ));
        assert!(matches!(
            second.join().unwrap().unwrap().delivery,
            EditDelivery::Sent { .. }
        ));
    });
    let stored = dag.run(&run)?.tasks[0].instructions.clone();
    let sent = messenger.inner.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1].3.ends_with(&format!("Instructions:\n{stored}")),
        "{stored}: {sent:?}"
    );
    assert!(sent[0].3.ends_with("Instructions:\nPlan A"), "{sent:?}");
    Ok(())
}

#[test]
fn oversized_titles_or_instructions_still_send_a_bounded_pointer() -> Result<()> {
    let (dag, run) = setup()?;
    dag.start(command(&run, false))?;
    let messenger = FakeMessenger::default();
    let huge = "x".repeat(17 * 1024);
    for (title, instructions) in [
        (Some(huge.as_str()), None),
        (None, Some(huge.as_str())),
        (Some(huge.as_str()), Some(huge.as_str())),
    ] {
        let edited = dag.edit_task(edit(&run, "work", title, instructions), &messenger)?;
        assert!(matches!(edited.delivery, EditDelivery::Sent { .. }));
    }
    let sent = messenger.sent.lock().unwrap();
    assert_eq!(sent.len(), 3);
    for (_, _, _, body) in sent.iter() {
        assert!(body.len() < 1024, "{} bytes", body.len());
        assert!(!body.contains("xxxx"));
        assert!(body.contains(&format!("power dag show {run}")), "{body}");
        assert!(body.contains("(task work)"), "{body}");
    }
    Ok(())
}

/// A DAG whose worker budget allows `max` workers, with `memory` GiB free, and a run
/// with `tasks` dispatched to coding-agent workers in `worktree`, in order.
fn budget_restart(
    workspace: FakeWorkspace,
    max: usize,
    min_memory: Option<u64>,
    memory_gib: u64,
    tasks: &[&str],
) -> Result<(TestDag, String, BTreeMap<String, DispatchResult>)> {
    let application = application_with_workspace(workspace);
    let caller = application.start_session(StartSession {
        worktree: "/worktree".into(),
        agent: Agent::Coding("claude".into()),
    })?;
    let memory = FakeMemory::default();
    memory
        .0
        .store(memory_gib * domain::dag::GIB, Ordering::Relaxed);
    let dag = TestDag::new(
        application,
        MemoryStore::default(),
        FakeRegistry(vec![caller.id.clone()]),
        FakeIcons::default(),
    )
    .with_worktree_root(FakeWorktreeRoot)
    .with_worker_budget(
        FixedBudget(WorkerBudget {
            max_active_workers: Some(max),
            min_available_memory: min_memory.map(|gib| gib * domain::dag::GIB),
            ..WorkerBudget::default()
        }),
        memory,
    );
    let run = dag.create_run(
        BoxSelection {
            selector: None,
            caller: Some(caller.id.clone()),
        },
        "Restart".into(),
        None,
    )?;
    dag.add_tasks(
        &run.id,
        &tasks.iter().map(|id| spec(id, &[])).collect::<Vec<_>>(),
    )?;
    let mut attempts = BTreeMap::new();
    for task in tasks {
        let started = dag.start(Dispatch {
            task: (*task).into(),
            provider: Some(Provider::Agent("claude".into())),
            args: vec!["--model".into(), "opus".into()],
            caller: Some(caller.id.clone()),
            budget: BudgetMode::Override,
            ..command(&run.id, false)
        })?;
        attempts.insert((*task).to_owned(), started);
    }
    Ok((dag, run.id, attempts))
}

/// Points every saved session at a real directory, so restore can relaunch it.
fn restorable(dag: &TestDag) -> Result<tempfile::TempDir> {
    let worktree = tempfile::tempdir()?;
    for mut record in dag.application.records.list()? {
        record.worktree = worktree.path().into();
        dag.application.records.save(&record)?;
    }
    Ok(worktree)
}

fn tasks_in(run: &Run, state: TaskState) -> Vec<String> {
    run.tasks
        .iter()
        .filter(|task| task.state == state)
        .map(|task| task.id.clone())
        .collect()
}

fn session_of(result: &DispatchResult) -> String {
    result.agent.session.clone().unwrap()
}

#[test]
fn restore_admits_waiting_workers_under_the_budget_and_queues_the_overflow_visibly() -> Result<()> {
    let (dag, run, attempts) =
        budget_restart(FakeWorkspace::default(), 2, None, 64, &["a", "b", "c"])?;
    for task in ["a", "b", "c"] {
        dag.report(
            &run,
            task,
            &attempts[task].attempt.id,
            ReportStatus::NeedsInput,
            &format!("question {task}"),
        )?;
    }
    let _worktree = restorable(&dag)?;

    let restored = dag.restore()?;

    // Most recent first, so the earliest dispatch is the one over the budget.
    let snapshot = dag.run(&run)?;
    assert_eq!(tasks_in(&snapshot, TaskState::NeedsInput), ["b", "c"]);
    assert_eq!(tasks_in(&snapshot, TaskState::Queued), ["a"]);
    assert!(!restored.contains(&session_of(&attempts["a"])));
    let interrupted = find_task(&snapshot, "a")?.attempts.last().unwrap().clone();
    assert_eq!(interrupted.state, AttemptState::Interrupted);
    // The overflowed question stays on record.
    assert!(
        interrupted
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("question a")),
        "{interrupted:?}"
    );
    let queued = dag.store.read(|db| db.queued_dispatches())?;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].args, ["--model", "opus"]);
    assert_eq!(dag.start_queued()?, 0, "A queued task started over budget");
    Ok(())
}

#[test]
fn restore_below_the_memory_floor_queues_waiting_workers_too() -> Result<()> {
    let (dag, run, attempts) =
        budget_restart(FakeWorkspace::default(), 4, Some(8), 1, &["a", "b"])?;
    dag.report(
        &run,
        "a",
        &attempts["a"].attempt.id,
        ReportStatus::NeedsInput,
        "question",
    )?;
    let _worktree = restorable(&dag)?;

    let restored = dag.restore()?;

    for task in ["a", "b"] {
        assert!(
            !restored.contains(&session_of(&attempts[task])),
            "{restored:?}"
        );
    }
    assert_eq!(tasks_in(&dag.run(&run)?, TaskState::Queued), ["a", "b"]);
    Ok(())
}

#[test]
fn an_over_budget_worker_that_still_runs_keeps_its_slot_until_a_reclaim_ends_it() -> Result<()> {
    let (dag, run, attempts) = budget_restart(FakeWorkspace::default(), 1, None, 64, &["a", "b"])?;
    dag.add_tasks(&run, &[spec("c", &[])])?;
    let _worktree = restorable(&dag)?;
    dag.application
        .runtime
        .fail_reclaim
        .store(true, Ordering::Release);

    dag.restore()?;

    // The older worker is over the budget, but what ran of it could not be ended, so its
    // attempt stays open and nothing retries it.
    let snapshot = dag.run(&run)?;
    let held = find_task(&snapshot, "a")?.attempts.last().unwrap().clone();
    assert_eq!(held.state, AttemptState::Active);
    assert_eq!(held.detail.as_deref(), Some(BUDGET_HOLD_DETAIL));
    assert!(dag.store.read(|db| db.queued_dispatches())?.is_empty());
    // The newer worker was within the budget, but its tree could not be ended either,
    // so it is held the same way rather than freed. New work queues and nothing starts.
    let newer = find_task(&snapshot, "b")?.attempts.last().unwrap().clone();
    assert_eq!(newer.state, AttemptState::Active);
    assert_eq!(newer.detail.as_deref(), Some(BUDGET_HOLD_DETAIL));
    let outcome = dag.dispatch(Dispatch {
        task: "c".into(),
        ..command(&run, false)
    })?;
    assert!(matches!(outcome, DispatchOutcome::Queued { .. }));
    assert_eq!(dag.start_queued()?, 0);
    assert_eq!(find_task(&dag.run(&run)?, "a")?.attempts.len(), 1);

    // Once a reclaim ends one, its task queues to retry, but the other tree still
    // holds the only slot.
    dag.application
        .runtime
        .fail_reclaim
        .store(false, Ordering::Release);
    dag.reclaimed(&session_of(&attempts["a"]))?;
    let snapshot = dag.run(&run)?;
    assert_eq!(
        find_task(&snapshot, "a")?.attempts.last().unwrap().state,
        AttemptState::Interrupted
    );
    assert_eq!(tasks_in(&snapshot, TaskState::Queued), ["a", "c"]);
    assert_eq!(dag.start_queued()?, 0);
    // Once both have ended, one worker starts.
    dag.reclaimed(&session_of(&attempts["b"]))?;
    assert_eq!(
        tasks_in(&dag.run(&run)?, TaskState::Queued),
        ["a", "b", "c"]
    );
    assert_eq!(dag.start_queued()?, 1);
    Ok(())
}

#[test]
fn a_queued_restart_retry_keeps_an_explicit_worktree_whose_branch_changed() -> Result<()> {
    queued_retry_keeps_a_changed_worktree(false)
}

#[test]
fn a_queued_restart_retry_keeps_the_worktree_of_a_migrated_attempt_whose_branch_changed()
-> Result<()> {
    queued_retry_keeps_a_changed_worktree(true)
}

/// Dispatches two workers to a Power-managed worktree named with `--worktree`, changes
/// its branch, and restarts over a budget of one. With `legacy`, the attempts carry no
/// recorded worktree mode, as rows stored before the schema upgrade do.
fn queued_retry_keeps_a_changed_worktree(legacy: bool) -> Result<()> {
    let workspace = FakeWorkspace::default();
    let (dag, run, _) = budget_restart(workspace.clone(), 1, None, 64, &[])?;
    dag.add_tasks(&run, &[spec("setup", &[]), spec("a", &[]), spec("b", &[])])?;
    // A Power-managed worktree that a later dispatch names with --worktree.
    let created = dag.start(Dispatch {
        task: "setup".into(),
        ..new_worktree(&run, "feature", Some("main"))
    })?;
    dag.report(
        &run,
        "setup",
        &created.attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    let managed = created.worktree.clone();
    let mut started = BTreeMap::new();
    for task in ["a", "b"] {
        let result = dag.start(Dispatch {
            task: task.into(),
            provider: Some(Provider::Agent("claude".into())),
            args: Vec::new(),
            budget: BudgetMode::Override,
            worktree: WorktreeRequest::Existing {
                path: managed.clone(),
                allow_main_checkout: false,
            },
            ..command(&run, false)
        })?;
        assert_eq!(result.attempt.explicit_worktree, Some(true));
        started.insert(task, result);
    }
    if legacy {
        dag.store.write(|db| {
            for open in db.open_attempts(None)? {
                let mut attempt = open.attempt;
                attempt.explicit_worktree = None;
                db.update_attempt(&attempt)?;
            }
            Ok(())
        })?;
    }
    // The worktree now has another branch checked out, so Power no longer owns it.
    {
        let mut git = workspace.0.lock().unwrap();
        git.branches.insert("feature".into(), None);
        git.branches.insert("other".into(), Some(managed.clone()));
    }
    let _worktree = restorable(&dag)?;

    dag.restore()?;
    let queued = dag.store.read(|db| db.queued_dispatches())?;
    assert_eq!(queued.len(), 1);
    assert!(
        matches!(&queued[0].worktree, WorktreeRequest::Existing { path, .. } if path == &managed),
        "{:?}",
        queued[0].worktree
    );
    dag.report(
        &run,
        "b",
        &started["b"].attempt.id,
        ReportStatus::Success,
        "done",
    )?;
    assert_eq!(dag.start_queued()?, 1);
    let retry = find_task(&dag.run(&run)?, "a")?
        .attempts
        .last()
        .unwrap()
        .clone();
    assert_eq!(retry.number, 2);
    assert_eq!(retry.worktree, managed);
    assert_eq!(retry.explicit_worktree, Some(true));
    Ok(())
}

/// Two workers under a budget of one, with the reclaim of `stuck` failing at restart.
fn restart_with_one_stuck(stuck: &str) -> Result<()> {
    let (dag, run, attempts) = budget_restart(FakeWorkspace::default(), 1, None, 64, &["a", "b"])?;
    let _worktree = restorable(&dag)?;
    let stuck_session = session_of(&attempts[stuck]);
    dag.application
        .runtime
        .stuck
        .lock()
        .unwrap()
        .insert(stuck_session.clone());

    let restored = dag.restore()?;

    // The stuck tree holds the only slot, so nothing relaunches beside it, whichever
    // order the records are in.
    for task in ["a", "b"] {
        assert!(
            !restored.contains(&session_of(&attempts[task])),
            "{restored:?}"
        );
    }
    let snapshot = dag.run(&run)?;
    let held = find_task(&snapshot, stuck)?
        .attempts
        .last()
        .unwrap()
        .clone();
    assert_eq!(held.state, AttemptState::Active);
    assert_eq!(held.detail.as_deref(), Some(BUDGET_HOLD_DETAIL));
    assert!(dag.application.has_record(&stuck_session)?);
    let other = if stuck == "a" { "b" } else { "a" };
    assert_eq!(tasks_in(&snapshot, TaskState::Queued), [other]);
    assert_eq!(
        dag.start_queued()?,
        0,
        "a retry started beside the stuck tree"
    );

    // Once the tree ends, the held task queues too, and one worker starts.
    dag.application.runtime.stuck.lock().unwrap().clear();
    dag.reclaimed(&stuck_session)?;
    assert!(!dag.application.has_record(&stuck_session)?);
    let snapshot = dag.run(&run)?;
    assert_eq!(
        find_task(&snapshot, stuck)?.attempts.last().unwrap().state,
        AttemptState::Interrupted
    );
    let mut queued = tasks_in(&snapshot, TaskState::Queued);
    queued.sort();
    assert_eq!(queued, ["a", "b"]);
    assert_eq!(dag.start_queued()?, 1);
    Ok(())
}

#[test]
fn a_stuck_older_worker_keeps_its_slot_so_the_newer_one_does_not_relaunch() -> Result<()> {
    restart_with_one_stuck("a")
}

#[test]
fn a_stuck_newer_worker_keeps_its_slot_so_the_older_one_does_not_relaunch() -> Result<()> {
    restart_with_one_stuck("b")
}

#[test]
fn a_stop_before_the_reclaim_succeeds_cancels_the_held_retry() -> Result<()> {
    let (dag, run, attempts) = budget_restart(FakeWorkspace::default(), 1, None, 64, &["a", "b"])?;
    let _worktree = restorable(&dag)?;
    let held = session_of(&attempts["a"]);
    dag.application
        .runtime
        .stuck
        .lock()
        .unwrap()
        .insert(held.clone());
    dag.restore()?;
    assert_eq!(
        find_task(&dag.run(&run)?, "a")?.attempts[0]
            .detail
            .as_deref(),
        Some(BUDGET_HOLD_DETAIL)
    );

    dag.stop_agent(&attempts["a"].agent.id)?;
    dag.application.runtime.stuck.lock().unwrap().clear();
    dag.reclaimed(&held)?;

    let snapshot = dag.run(&run)?;
    let task = find_task(&snapshot, "a")?;
    assert_eq!(task.attempts.len(), 1);
    assert_eq!(task.attempts[0].state, AttemptState::Interrupted);
    assert_eq!(task.attempts[0].detail.as_deref(), Some(STOP_DETAIL));
    assert!(
        dag.store
            .read(|db| db.queued_dispatches())?
            .iter()
            .all(|queued| queued.task != "a"),
        "a stopped worker's retry was queued"
    );
    assert!(!dag.application.has_record(&held)?);
    assert!(!dag.store.read(|db| db.stopping_sessions())?.contains(&held));
    Ok(())
}

#[test]
fn a_failed_queue_write_at_restart_keeps_the_worker_for_a_later_handoff() -> Result<()> {
    let (dag, run, attempts) = budget_restart(FakeWorkspace::default(), 1, None, 64, &["a", "b"])?;
    let _worktree = restorable(&dag)?;
    let over = session_of(&attempts["a"]);
    dag.store.0.lock().unwrap().fail_queue_writes = true;

    let restored = dag.restore()?;

    assert!(restored.contains(&session_of(&attempts["b"])));
    assert!(!restored.contains(&over));
    // Nothing was queued, so the worker keeps its record and its open attempt.
    assert!(dag.application.has_record(&over)?);
    let attempt = find_task(&dag.run(&run)?, "a")?.attempts[0].clone();
    assert_eq!(attempt.state, AttemptState::Active);
    assert_eq!(attempt.detail.as_deref(), Some(BUDGET_HOLD_DETAIL));

    // A reclaim retry while the store still fails keeps it too.
    assert!(dag.reclaimed(&over).is_err());
    assert!(dag.application.has_record(&over)?);
    assert_eq!(
        find_task(&dag.run(&run)?, "a")?.attempts[0].state,
        AttemptState::Active
    );

    // Once the store recovers, the handoff commits and only then is the record gone.
    dag.store.0.lock().unwrap().fail_queue_writes = false;
    dag.reclaimed(&over)?;
    assert!(!dag.application.has_record(&over)?);
    let snapshot = dag.run(&run)?;
    assert_eq!(tasks_in(&snapshot, TaskState::Queued), ["a"]);
    assert_eq!(
        find_task(&snapshot, "a")?.attempts[0].state,
        AttemptState::Interrupted
    );
    Ok(())
}

/// The memory DAG store with no talk store: a poster here writes nothing to talk.
impl JointStore for MemoryStore {
    fn write_joint<V>(
        &self,
        operation: impl FnOnce(&dyn DagTransaction, &dyn crate::talk::TalkTransaction) -> Result<V>,
    ) -> Result<V> {
        self.write(|db| operation(db, &NoTalk))
    }
}

struct NoTalk;

impl crate::talk::TalkReader for NoTalk {
    fn directory(&self) -> Result<domain::talk::Directory> {
        unreachable!()
    }
    fn message(&self, _: &domain::talk::MessageId) -> Result<Option<domain::talk::Message>> {
        unreachable!()
    }
    fn inbox(
        &self,
        _: &domain::talk::Recipient,
        _: Option<&str>,
        _: bool,
    ) -> Result<Vec<domain::talk::Message>> {
        unreachable!()
    }
    fn summary(&self, _: &str) -> Result<Vec<domain::talk::AgentTalkSummary>> {
        unreachable!()
    }
    fn pending_agents(&self, _: u64) -> Result<Vec<String>> {
        unreachable!()
    }
    fn queued(&self, _: &domain::talk::Recipient, _: &str) -> Result<Vec<domain::talk::Queued>> {
        unreachable!()
    }
    fn agent(&self, _: &str) -> Result<ManagedAgent> {
        unreachable!()
    }
    fn thread(&self, _: &domain::talk::ThreadId) -> Result<Vec<domain::talk::Message>> {
        unreachable!()
    }
    fn log(&self, _: &crate::talk::LogFilter) -> Result<Vec<domain::talk::Message>> {
        unreachable!()
    }
    fn recent_sends(
        &self,
        _: &domain::talk::Sender,
        _: &domain::talk::Recipient,
        _: u64,
    ) -> Result<Vec<u64>> {
        unreachable!()
    }
    fn decisions(&self, _: bool, _: u64) -> Result<Vec<domain::talk::Message>> {
        unreachable!()
    }
    fn cursor(&self) -> Result<u64> {
        unreachable!()
    }
    fn events(&self, _: u64, _: Option<&str>) -> Result<Vec<crate::talk::TalkEvent>> {
        unreachable!()
    }
}

impl crate::talk::TalkTransaction for NoTalk {
    fn insert(&self, _: &domain::talk::Message) -> Result<()> {
        unreachable!()
    }
    fn mark(
        &self,
        _: &[&domain::talk::MessageId],
        _: crate::talk::DeliveryChange,
        _: u64,
    ) -> Result<()> {
        unreachable!()
    }
    fn answer(&self, _: &domain::talk::MessageId, _: u64) -> Result<bool> {
        unreachable!()
    }
}

/// Fails the post, or records who it came from.
struct Poster {
    fail: bool,
    posted: Mutex<Vec<(String, String)>>,
}

impl DecisionPoster for Poster {
    fn check_decision(&self, _: &str, choices: &[String]) -> Result<()> {
        ensure!(!choices.is_empty(), "no choices");
        Ok(())
    }
    fn post_decision(
        &self,
        _: &dyn crate::talk::TalkTransaction,
        agent: &str,
        run: &str,
        body: &str,
        _: Vec<String>,
    ) -> Result<domain::talk::Message> {
        ensure!(!self.fail, "inbox is full");
        self.posted.lock().unwrap().push((agent.into(), run.into()));
        Ok(domain::talk::Message {
            id: domain::talk::MessageId("ask".into()),
            box_id: "box".into(),
            sender: domain::talk::Sender::System,
            recipient: domain::talk::Recipient::User,
            body: body.into(),
            thread: domain::talk::ThreadId("ask".into()),
            reply_to: None,
            broadcast: None,
            run: Some(run.into()),
            created_at: 0,
            delivery: domain::talk::Delivery::default(),
            urgent: false,
            decision: None,
        })
    }
}

#[test]
fn a_needs_input_decision_commits_with_its_report_or_not_at_all() -> Result<()> {
    let (dag, run) = setup()?;
    let started = dag.start(command(&run, false))?;
    let attempt = started.attempt.id.clone();
    let ask_as = |pid: u32, attempt: &str, choices: &[&str], fail: bool| {
        let poster = Poster {
            fail,
            posted: Mutex::new(Vec::new()),
        };
        let result = dag.report_decision(
            ReportDecision {
                run: run.clone(),
                task: "work".into(),
                attempt: attempt.into(),
                summary: "Ship it?".into(),
                choices: choices.iter().map(|c| (*c).into()).collect(),
                caller_pid: Some(pid),
            },
            &poster,
        );
        (result, poster.posted.into_inner().unwrap())
    };
    let ask = |attempt: &str, choices: &[&str], fail: bool| ask_as(7, attempt, choices, fail);
    let state = || dag.run(&run).map(|run| run.tasks[0].state);

    // Only a process in the attempt's session may ask in its worker's name.
    let (result, posted) = ask(&attempt, &["Yes"], false);
    assert!(format!("{:#}", result.unwrap_err()).contains("Only the worker"));
    assert!(posted.is_empty());
    *dag.application.runtime.worker_pid.lock().unwrap() = Some(7);
    let (result, posted) = ask_as(8, &attempt, &["Yes"], false);
    assert!(format!("{:#}", result.unwrap_err()).contains("Only the worker"));
    assert!(posted.is_empty());
    assert_eq!(state()?, TaskState::Working);

    // Invalid choices or a failed post leave the attempt working.
    let (result, posted) = ask(&attempt, &[], false);
    assert!(result.is_err() && posted.is_empty());
    let (result, _) = ask(&attempt, &["Yes"], true);
    assert!(format!("{:#}", result.unwrap_err()).contains("inbox is full"));
    assert_eq!(state()?, TaskState::Working);
    assert!(dag.run(&run)?.tasks[0].attempts[0].blocker.is_none());

    // An unknown attempt is rejected and posts nothing.
    let (result, posted) = ask("other", &["Yes"], false);
    assert!(result.is_err() && posted.is_empty());

    // The accepted report posts as the reported attempt's worker, in its run.
    let (result, posted) = ask(&attempt, &["Yes"], false);
    result?;
    assert_eq!(posted, [(started.agent.id.clone(), run.clone())]);
    assert_eq!(state()?, TaskState::NeedsInput);

    // A finished attempt cannot ask again.
    dag.report(&run, "work", &attempt, ReportStatus::Success, "done")?;
    let (result, posted) = ask(&attempt, &["Yes"], false);
    assert!(format!("{:#}", result.unwrap_err()).contains("did not apply"));
    assert!(posted.is_empty());
    Ok(())
}
