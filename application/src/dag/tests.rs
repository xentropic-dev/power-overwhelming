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
}

impl DagTransaction for MemoryDb {
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
    creation_error: Option<&'static str>,
    missing: BTreeSet<PathBuf>,
    removed: Vec<PathBuf>,
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
        if let Some(error) = worktrees.creation_error {
            bail!("{error}");
        }
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
            skills_dir: None,
            resume: None,
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

    let first = dag.dispatch(Dispatch {
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
    dag.dispatch(Dispatch {
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
            .dispatch(dispatch)
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
    workspace.0.lock().unwrap().creation_error = Some(git_error);
    let before = dag.latest_seq()?;
    let error = dag
        .dispatch(new_worktree(&run, "fresh", Some("missing-base")))
        .err()
        .context("Dispatch unexpectedly succeeded")?;
    assert_eq!(error.to_string(), git_error);
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
    let retry = dag.dispatch(command(&run, true))?;
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
        .dispatch(new_worktree(&run, "fresh", Some("main")))
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
    let retry = dag.dispatch(Dispatch {
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
    let first = dag.dispatch(new_worktree(&run, "feature/original", Some("main")))?;
    let mut previous = first.attempt.clone();
    for number in 2..=3 {
        dag.session_exited(previous.session.as_deref().unwrap(), "interrupted")?;
        let retry = dag.dispatch(Dispatch {
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
    let fresh = dag.dispatch(Dispatch {
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
            dag.dispatch(command(&run, false))
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
    let first = dag.dispatch(command(&run, false))?;
    let first_session = first.agent.session.unwrap();
    dag.session_exited(&first_session, "worker exited")?;
    let second = dag.dispatch(command(&run, true))?;
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
    let first = dag.dispatch(command(&run, false))?;
    let first_id = first.attempt.id;
    let session = first.agent.session.unwrap();
    dag.session_exited(&session, "worker exited with code 7")?;
    assert_eq!(dag.run(&run)?.tasks[0].state, TaskState::Interrupted);
    let agent = dag.store.read(|db| db.agent(&first.agent.id))?;
    assert_eq!(agent.state, AgentState::Exited);
    assert!(dag.dispatch(command(&run, false)).is_err());

    let second = dag.dispatch(command(&run, true))?;
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
    let active = dag.dispatch(command(&run, false))?;
    let blocked = dag.dispatch(Dispatch {
        task: "blocked".into(),
        ..command(&run, false)
    })?;
    let done = dag.dispatch(Dispatch {
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
    let restored = dag.dispatch(command(&run, false))?;
    let lost = dag.dispatch(Dispatch {
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
    let working = dag.dispatch(command(&run, false))?;
    let finished = dag.dispatch(Dispatch {
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
    let first = dag.dispatch(command(&run.id, false))?;
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
    let worker = dag.dispatch(Dispatch {
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
    let working = dag.dispatch(command(&run, false))?;
    assert_eq!(working.agent.status, AgentStatus::Working);
    let [blocked, done, failed, exited] = tasks.map(|task| {
        dag.dispatch(Dispatch {
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
    let worker = dag.dispatch(Dispatch {
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
        dag.dispatch(Dispatch {
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

    let worker = dag.dispatch(Dispatch {
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
    let worker = dag.dispatch(command(&run, false))?;
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
    let worker = dag.dispatch(command(&run, false))?;
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
