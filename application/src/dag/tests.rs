use super::*;
use crate::{
    ArtifactStore, BranchStatus, CreatedWorktree, MemorySessions, RecentArtifact, SessionArtifact,
    Workspace, WorktreeStatus, agents::AgentProfileRegistry,
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
        Barrier,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
        agent.status = AgentStatus::derive(agent.state, attempt);
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
        self.0
            .borrow()
            .runs
            .iter()
            .find(|run| run.id == id)
            .cloned()
            .context("Unknown run")
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
                state: TaskState::Waiting,
                queued_at: None,
                current_attempt: None,
                attempts: Vec::new(),
            }));
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

    fn agent(&self, _: &Self::Handle) -> Option<Agent> {
        None
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
    static ID: AtomicU64 = AtomicU64::new(0);
    let profiles = ["claude", "codex"]
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
        })
        .collect();
    Arc::new(Application::new(
        FakeArtifacts::default(),
        Arc::new(MemorySessions::default()),
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
    }
}

fn command(run: &str, retry: bool) -> Dispatch {
    Dispatch {
        run: run.into(),
        task: "work".into(),
        provider: Provider::Command,
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
        provider: Provider::Agent("codex".into()),
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
        dag.session_exited(previous.session.as_deref().unwrap(), "interrupted")?;
        let retry = dag.start(Dispatch {
            provider: Provider::Agent("codex".into()),
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

    dag.session_exited(previous.session.as_deref().unwrap(), "interrupted")?;
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
    dag.session_exited(&first_session, "worker exited")?;
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
    dag.session_exited(&session, "worker exited with code 7")?;
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
        provider: Provider::Agent("claude".into()),
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
        provider: Provider::Agent("claude".into()),
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
    dag.session_exited(exited.agent.session.as_deref().unwrap(), "closed")?;

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
        tasks: states
            .iter()
            .enumerate()
            .map(|(index, state)| Task {
                id: format!("t{index}"),
                title: String::new(),
                instructions: String::new(),
                deps: Vec::new(),
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
    dag.session_exited(old.agent.session.as_deref().unwrap(), "exited")?;
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
