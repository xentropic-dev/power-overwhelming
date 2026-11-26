use super::*;
use crate::{
    ArtifactStore, BranchStatus, CreatedWorktree, RecentArtifact, Workspace, WorktreeStatus,
    agents::AgentProfileRegistry,
};
use anyhow::bail;
use domain::{
    agent::{AgentProfile, Injection},
    artifact::{ArtifactRecord, ResolvedArtifact},
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

    fn find_worktree_by_attempt(&self, _attempt: &str) -> Result<Option<ManagedWorktree>> {
        bail!("Unexpected managed worktree lookup")
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
            .context("Unknown agent")
    }

    fn session_agent(&self, session: &str) -> Result<Option<ManagedAgent>> {
        Ok(self
            .0
            .borrow()
            .agents
            .iter()
            .find(|agent| agent.session.as_deref() == Some(session))
            .cloned())
    }

    fn list_agents(&self, box_id: Option<&str>) -> Result<Vec<ManagedAgent>> {
        Ok(self
            .0
            .borrow()
            .agents
            .iter()
            .filter(|agent| box_id.is_none_or(|id| agent.box_id == id))
            .cloned()
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
            .push(attempt.clone());
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

struct FakeWorkspace;

impl Workspace for FakeWorkspace {
    fn open_worktree(&self, directory: &Path) -> Result<PathBuf> {
        Ok(directory.to_owned())
    }

    fn list_worktrees(&self, directory: &Path) -> Result<Vec<PathBuf>> {
        Ok(vec![directory.to_owned()])
    }

    fn validate_box_worktree(&self, repository: &Path, worktree: &Path) -> Result<PathBuf> {
        ensure!(
            repository == Path::new("/repo") && worktree == Path::new("/worktree"),
            "Wrong worktree"
        );
        Ok(worktree.to_owned())
    }

    fn head(&self, _worktree: &Path) -> Result<String> {
        bail!("Unexpected HEAD lookup")
    }

    fn branch_exists(&self, _repository: &Path, _branch: &str) -> Result<BranchStatus> {
        bail!("Unexpected branch lookup")
    }

    fn create_worktree(
        &self,
        _repository: &Path,
        _path: &Path,
        _branch: &str,
        _base: &str,
    ) -> Result<CreatedWorktree> {
        bail!("Unexpected worktree creation")
    }

    fn worktree_status(&self, _path: &Path) -> Result<WorktreeStatus> {
        bail!("Unexpected worktree status lookup")
    }

    fn remove_worktree(&self, _path: &Path, _delete_branch: bool) -> Result<()> {
        bail!("Unexpected worktree removal")
    }

    fn open_artifact_file(&self, _: &Path) -> Result<PathBuf> {
        bail!("Unexpected artifact operation")
    }
}

struct FakeArtifacts;

impl ArtifactStore for FakeArtifacts {
    fn insert(&self, _: ArtifactRecord) -> Result<()> {
        bail!("Unexpected artifact operation")
    }

    fn find(&self, _: &str) -> Result<ResolvedArtifact> {
        bail!("Unexpected artifact operation")
    }

    fn recent(&self, _: &[String], _: usize) -> Result<Vec<RecentArtifact>> {
        bail!("Unexpected artifact operation")
    }
}

#[derive(Default)]
struct FakeTerminal(Mutex<Vec<(Session, SessionOptions)>>);

impl TerminalRuntime for FakeTerminal {
    type Handle = Arc<AtomicBool>;

    fn start(
        &self,
        session: &Session,
        options: SessionOptions,
        _: Option<&AgentProfile>,
    ) -> Result<Self::Handle> {
        self.0.lock().unwrap().push((session.clone(), options));
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

type TestDag = DagApplication<FakeTerminal, MemoryStore, FakeRegistry>;

struct FakeProfiles(Vec<AgentProfile>);

impl AgentProfileRegistry for FakeProfiles {
    fn profiles(&self) -> &[AgentProfile] {
        &self.0
    }
}

fn application() -> Arc<Application<FakeTerminal>> {
    static ID: AtomicU64 = AtomicU64::new(0);
    let profiles = ["claude", "codex"]
        .into_iter()
        .map(|id| AgentProfile {
            id: id.into(),
            command: id.into(),
            injection: Injection::None,
            skip_subcommands: Vec::new(),
            skip_if_args_contain: Vec::new(),
            skills_dir: None,
        })
        .collect();
    Arc::new(Application::new(
        FakeArtifacts,
        FakeWorkspace,
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
    );
    let run = dag.create_run(
        BoxSelection {
            selector: Some("2".into()),
            caller: None,
        },
        "Fixture".into(),
        Some("external".into()),
    )?;
    dag.add_tasks(&run.id, &[spec("work", &[])])?;
    Ok((dag, run.id))
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
        worktree: WorktreeRequest::Existing("/worktree".into()),
        retry,
        args: vec!["worker".into()],
        caller: None,
    }
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
    assert_eq!(dag.application.runtime.0.lock().unwrap().len(), 1);
    let task = &dag.run(&run)?.tasks[0];
    assert_eq!(task.state, TaskState::Working);
    assert_eq!(task.attempts.len(), 1);
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
    dag.reconcile()?;
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
    dag.reconcile()?;
    assert_eq!(dag.latest_seq()?, page.cursor);
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
    assert_eq!(dag.list_agents(selection())?.len(), 1);
    dag.add_tasks(&run.id, &[spec("join", &["work"]), spec("work", &[])])?;
    let first = dag.dispatch(command(&run.id, false))?;
    dag.report(
        &run.id,
        "work",
        &first.attempt.id,
        ReportStatus::Success,
        "Prerequisite result",
    )?;
    let worker = dag.dispatch(Dispatch {
        task: "join".into(),
        provider: Provider::Agent("claude".into()),
        caller: Some(caller.id),
        args: vec!["--flag".into()],
        ..command(&run.id, false)
    })?;
    assert_eq!(worker.agent.parent.as_deref(), Some(coordinator.as_str()));
    let launches = dag.application.runtime.0.lock().unwrap();
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
        worktree: WorktreeRequest::Existing("/worktree".into()),
        parent: None,
        label: "coordinator".into(),
        prompt: None,
        args: Vec::new(),
    })?;
    assert_eq!(agent.parent, None);
    assert!(agent.session.is_some());
    Ok(())
}
