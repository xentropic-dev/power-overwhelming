use super::*;
use crate::{MemorySessions, WorktreeRoot};
use domain::worktree::{KeepReason, PruneVerdict, WorktreeCreator};
use std::fs;

#[derive(Default)]
struct WorktreeState {
    branch: Option<BranchStatus>,
    create_error: Option<&'static str>,
    created: Vec<PathBuf>,
    statuses: BTreeMap<PathBuf, WorktreeStatus>,
    removals: Vec<(PathBuf, bool)>,
    fail_removal: Option<PathBuf>,
}

#[derive(Clone, Default)]
struct WorktreeWorkspace(Arc<Mutex<WorktreeState>>);

impl Workspace for WorktreeWorkspace {
    fn open_worktree(&self, path: &Path) -> Result<PathBuf> {
        Ok(path.to_owned())
    }

    fn list_worktrees(&self, _: &Path) -> Result<Vec<PathBuf>> {
        bail!("Unexpected lookup of unmanaged worktrees")
    }

    fn validate_box_worktree(&self, _: &Path, _: &Path) -> Result<PathBuf> {
        bail!("Unexpected worktree validation")
    }
    fn main_checkout(&self, _: &Path) -> Result<Option<PathBuf>> {
        bail!("Unexpected main checkout lookup")
    }

    fn head(&self, path: &Path) -> Result<String> {
        ensure!(path == Path::new("/view-source"), "Wrong source worktree");
        Ok("view-head".into())
    }

    fn resolve_commit(&self, _: &Path, commitish: &str) -> Result<String> {
        Ok(commitish.into())
    }

    fn branch_exists(&self, repository: &Path, _: &str) -> Result<BranchStatus> {
        ensure!(repository == Path::new("/repo"), "Wrong repository");
        Ok(self
            .0
            .lock()
            .unwrap()
            .branch
            .clone()
            .unwrap_or(BranchStatus::Missing))
    }

    fn create_worktree(
        &self,
        repository: &Path,
        path: &Path,
        _: &str,
        base: &str,
    ) -> Result<CreatedWorktree> {
        ensure!(repository == Path::new("/repo"), "Wrong repository");
        ensure!(base == "view-head", "Wrong base commit");
        let mut state = self.0.lock().unwrap();
        if let Some(error) = state.create_error {
            bail!(error);
        }
        state.created.push(path.into());
        Ok(CreatedWorktree {
            path: Path::new("/canonical").join(path.strip_prefix("/")?),
            base_commit: base.into(),
        })
    }

    fn owns_worktree(&self, worktree: &ManagedWorktree) -> bool {
        self.0.lock().unwrap().statuses.contains_key(&worktree.path)
    }

    fn worktree_status(&self, path: &Path) -> Result<WorktreeStatus> {
        self.0
            .lock()
            .unwrap()
            .statuses
            .get(path)
            .copied()
            .context("Missing status")
    }

    fn remove_worktree(&self, path: &Path, delete_branch: bool) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        ensure!(
            state.fail_removal.as_deref() != Some(path),
            "Git refused removal"
        );
        state.removals.push((path.into(), delete_branch));
        state.statuses.remove(path);
        Ok(())
    }

    fn worktree_exists(&self, path: &Path) -> bool {
        !self
            .0
            .lock()
            .unwrap()
            .removals
            .iter()
            .any(|(removed, _)| removed == path)
    }

    fn open_artifact_file(&self, _: &Path) -> Result<PathBuf> {
        bail!("Unexpected artifact operation")
    }
}

struct FixedRoot(PathBuf);

impl WorktreeRoot for FixedRoot {
    fn worktree_root(&self) -> Result<PathBuf> {
        Ok(self.0.clone())
    }
}

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Result<Self> {
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "power-view-worktree-test-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture(workspace: WorktreeWorkspace) -> Result<(TestDag, Session)> {
    static ID: AtomicU64 = AtomicU64::new(0);
    let application = Arc::new(Application::new(
        FakeArtifacts::default(),
        Arc::new(MemorySessions::default()),
        workspace,
        FakeTerminal::default(),
        Arc::new(FakeProfiles(Vec::new())),
        || format!("view-{}", ID.fetch_add(1, Ordering::Relaxed)),
    ));
    let source = start_shell(&application, Path::new("/view-source"))?;
    let dag = TestDag::new(
        application,
        MemoryStore::default(),
        FakeRegistry(vec![source.id.clone()]),
        FakeIcons::default(),
    );
    Ok((dag, source))
}

fn start_shell(application: &Application<FakeTerminal>, path: &Path) -> Result<Session> {
    application.start_session(StartSession {
        agent: Agent::Shell,
        worktree: path.into(),
    })
}

#[test]
fn creates_from_the_view_head_and_records_ownership() -> Result<()> {
    let workspace = WorktreeWorkspace::default();
    let (dag, view) = fixture(workspace.clone())?;
    let root = TempRoot::new()?;
    let dag = dag.with_worktree_root(FixedRoot(root.0.clone()));
    fs::create_dir_all(root.0.join("test-repo/feature-branch"))?;
    let before = now();

    let path = dag.create_view_worktree("2", &view.id, "feature/branch")?;

    let requested = root.0.join("test-repo/feature-branch-2");
    assert_eq!(
        path,
        Path::new("/canonical").join(requested.strip_prefix("/")?)
    );
    assert_eq!(workspace.0.lock().unwrap().created, [requested]);
    let records = dag.list_managed_worktrees(Some("box-id"))?;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.box_id, "box-id");
    assert_eq!(record.repository, Path::new("/repo"));
    assert_eq!(record.path, path);
    assert_eq!(record.branch, "feature/branch");
    assert_eq!(record.base_commit, "view-head");
    assert!(record.created_at >= before);
    assert_eq!(
        record.creator,
        WorktreeCreator::View {
            box_id: "box-id".into(),
            session: view.id
        }
    );
    assert_eq!(dag.application.list_sessions().len(), 1);
    Ok(())
}

#[test]
fn rejects_a_branch_that_already_exists() -> Result<()> {
    let workspace = WorktreeWorkspace::default();
    let (dag, view) = fixture(workspace.clone())?;
    let dag = dag.with_worktree_root(FixedRoot("/managed".into()));
    for (checked_out_at, expected) in [
        (
            Some("/elsewhere".into()),
            "Branch \"feature\" already exists and is checked out at /elsewhere",
        ),
        (None, "Branch \"feature\" already exists"),
    ] {
        workspace.0.lock().unwrap().branch = Some(BranchStatus::Exists { checked_out_at });
        let error = dag
            .create_view_worktree("box-id", &view.id, "feature")
            .unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
    assert!(workspace.0.lock().unwrap().created.is_empty());
    assert!(dag.list_managed_worktrees(None)?.is_empty());
    Ok(())
}

#[test]
fn reports_git_creation_errors_without_recording_ownership() -> Result<()> {
    let workspace = WorktreeWorkspace::default();
    workspace.0.lock().unwrap().create_error = Some("fatal: invalid branch name");
    let (dag, view) = fixture(workspace.clone())?;
    let dag = dag.with_worktree_root(FixedRoot("/managed".into()));

    let error = dag
        .create_view_worktree("box-id", &view.id, "invalid name")
        .unwrap_err();

    assert_eq!(error.to_string(), "fatal: invalid branch name");
    assert!(dag.list_managed_worktrees(None)?.is_empty());
    assert!(workspace.0.lock().unwrap().removals.is_empty());
    Ok(())
}

fn record(box_id: &str, branch: &str) -> ManagedWorktree {
    ManagedWorktree {
        box_id: box_id.into(),
        repository: "/repo".into(),
        path: Path::new("/managed").join(branch),
        branch: branch.into(),
        base_commit: "base".into(),
        created_at: 1,
        creator: WorktreeCreator::View {
            box_id: box_id.into(),
            session: "view".into(),
        },
    }
}

fn record_owners(dag: &TestDag, records: &[&ManagedWorktree]) -> Result<()> {
    dag.store.write(|db| {
        for record in records {
            db.insert_worktree(record)?;
        }
        Ok(())
    })
}

fn add_attempt(dag: &TestDag, worktree: &ManagedWorktree, state: AttemptState) -> Result<()> {
    dag.store.write(|db| {
        db.insert_run(&Run {
            id: worktree.branch.clone(),
            box_id: worktree.box_id.clone(),
            title: "Fixture".into(),
            coordinator: Coordinator::External {
                label: "fixture".into(),
            },
            created_at: 0,
            archived_at: None,
            paused_at: None,
            tasks: Vec::new(),
        })?;
        db.insert_tasks(&worktree.branch, &[spec("work", &[])])?;
        db.insert_attempt(
            &worktree.branch,
            "work",
            &Attempt {
                id: worktree.branch.clone(),
                number: 1,
                agent: "agent".into(),
                session: None,
                worktree: worktree.path.clone(),
                provider: Provider::Command,
                model: None,
                state,
                summary: None,
                blocker: None,
                detail: None,
                started_at: 0,
                ended_at: None,
                paused: false,
                stalled: false,
                explicit_worktree: Some(false),
                artifacts: Vec::new(),
            },
        )
    })
}

const CLEAN_MERGED: WorktreeStatus = WorktreeStatus {
    dirty: false,
    merged_into_default: true,
};

#[test]
fn lists_and_prunes_only_owned_worktrees_in_scope() -> Result<()> {
    let dirty = record("box-id", "dirty");
    let unmerged = record("box-id", "unmerged");
    let live = record("box-id", "live");
    let open = record("box-id", "open-attempt");
    let removable = record("box-id", "removable");
    let other_box = record("other", "outside-scope");
    let unmanaged = PathBuf::from("/managed/unmanaged");
    let workspace = WorktreeWorkspace::default();
    workspace.0.lock().unwrap().statuses.extend([
        (
            dirty.path.clone(),
            WorktreeStatus {
                dirty: true,
                merged_into_default: true,
            },
        ),
        (
            unmerged.path.clone(),
            WorktreeStatus {
                dirty: false,
                merged_into_default: false,
            },
        ),
        (live.path.clone(), CLEAN_MERGED),
        (open.path.clone(), CLEAN_MERGED),
        (removable.path.clone(), CLEAN_MERGED),
        (other_box.path.clone(), CLEAN_MERGED),
        (unmanaged.clone(), CLEAN_MERGED),
    ]);
    let (dag, _) = fixture(workspace.clone())?;
    record_owners(
        &dag,
        &[&dirty, &unmerged, &live, &open, &removable, &other_box],
    )?;
    start_shell(&dag.application, &live.path)?;
    add_attempt(&dag, &open, AttemptState::NeedsInput)?;
    add_attempt(&dag, &removable, AttemptState::Succeeded)?;
    let finished = start_shell(&dag.application, &removable.path)?;
    dag.application
        .terminal(&finished.id)?
        .store(false, Ordering::Release);
    assert_eq!(dag.list_managed_worktrees(None)?.len(), 6);
    assert_eq!(dag.list_managed_worktrees(Some("box-id"))?.len(), 5);

    let expected = vec![
        (dirty.clone(), PruneVerdict::Keep(KeepReason::Dirty)),
        (unmerged.clone(), PruneVerdict::Keep(KeepReason::Unmerged)),
        (live.clone(), PruneVerdict::Keep(KeepReason::InUse)),
        (open.clone(), PruneVerdict::Keep(KeepReason::OpenAttempt)),
        (removable.clone(), PruneVerdict::Remove),
    ];
    assert_eq!(dag.prune(Some("box-id"), true, true)?, expected);
    assert!(workspace.0.lock().unwrap().removals.is_empty());
    assert_eq!(dag.list_managed_worktrees(None)?.len(), 6);

    assert_eq!(dag.prune(Some("box-id"), false, false)?, expected);
    assert_eq!(
        dag.list_managed_worktrees(None)?,
        [dirty, unmerged, live, open, other_box.clone()]
    );
    assert_eq!(
        dag.prune(None, false, true)?.last(),
        Some(&(other_box.clone(), PruneVerdict::Remove))
    );
    let state = workspace.0.lock().unwrap();
    assert_eq!(
        state.removals,
        [(removable.path, false), (other_box.path, true)]
    );
    assert!(state.statuses.contains_key(&unmanaged));
    Ok(())
}

#[test]
fn retains_ownership_when_git_refuses_to_remove_a_worktree() -> Result<()> {
    let removed = record("box-id", "first");
    let kept = record("box-id", "second");
    let workspace = WorktreeWorkspace::default();
    {
        let mut state = workspace.0.lock().unwrap();
        state.statuses.extend([
            (removed.path.clone(), CLEAN_MERGED),
            (kept.path.clone(), CLEAN_MERGED),
        ]);
        state.fail_removal = Some(kept.path.clone());
    }
    let (dag, _) = fixture(workspace.clone())?;
    record_owners(&dag, &[&removed, &kept])?;

    let error = dag.prune(None, false, false).unwrap_err();

    assert!(format!("{error:#}").contains("Git refused removal"));
    assert_eq!(dag.list_managed_worktrees(None)?, [kept]);
    assert_eq!(
        workspace.0.lock().unwrap().removals,
        [(removed.path, false)]
    );
    Ok(())
}

fn add_agent(
    dag: &TestDag,
    id: &str,
    worktree: &Path,
    session: Option<&str>,
    state: AgentState,
) -> Result<()> {
    dag.store.write(|db| {
        db.insert_agent(&ManagedAgent {
            id: id.into(),
            box_id: "box-id".into(),
            worktree: worktree.into(),
            provider: Provider::Command,
            model: None,
            parent: None,
            session: session.map(str::to_owned),
            run: None,
            task: None,
            attempt: None,
            state,
            paused: false,
            status: domain::dag::AgentStatus::derive(state, false, None),
            label: id.into(),
            created_at: 0,
        })
    })
}

fn agent_state(dag: &TestDag, id: &str) -> Result<AgentState> {
    dag.store.read(|db| db.agent(id)).map(|agent| agent.state)
}

#[test]
fn removing_a_worktree_retires_the_agents_left_in_it() -> Result<()> {
    let removed = record("box-id", "removed");
    let other = record("box-id", "other");
    let workspace = WorktreeWorkspace::default();
    workspace
        .0
        .lock()
        .unwrap()
        .statuses
        .insert(removed.path.clone(), CLEAN_MERGED);
    let (dag, _) = fixture(workspace.clone())?;
    record_owners(&dag, &[&removed])?;
    // The terminal has stopped, but the exit has not been recorded yet.
    let stale = start_shell(&dag.application, &removed.path)?;
    dag.application
        .terminal(&stale.id)?
        .store(false, Ordering::Release);
    add_agent(
        &dag,
        "with-session",
        &removed.path,
        Some(&stale.id),
        AgentState::Running,
    )?;
    add_agent(
        &dag,
        "orphan",
        &removed.path,
        Some("gone"),
        AgentState::Running,
    )?;
    add_agent(&dag, "no-session", &removed.path, None, AgentState::Running)?;
    add_agent(&dag, "elsewhere", &other.path, None, AgentState::Running)?;

    assert_eq!(
        dag.prune(None, false, false)?,
        [(removed.clone(), PruneVerdict::Remove)]
    );

    assert!(dag.application.session(&stale.id).is_err());
    assert_eq!(agent_state(&dag, "orphan")?, AgentState::Exited);
    assert_eq!(agent_state(&dag, "no-session")?, AgentState::Exited);
    assert_eq!(agent_state(&dag, "elsewhere")?, AgentState::Running);
    // The session's exit report marks this one, as for any other exit.
    assert_eq!(agent_state(&dag, "with-session")?, AgentState::Running);
    dag.session_exited(&stale.id, "ended", true)?;
    assert_eq!(agent_state(&dag, "with-session")?, AgentState::Exited);
    Ok(())
}

#[test]
fn keeps_a_worktree_whose_agent_has_an_open_attempt_elsewhere() -> Result<()> {
    let worktree = record("box-id", "agent-home");
    let attempt_path = record("box-id", "attempt-path");
    let workspace = WorktreeWorkspace::default();
    workspace
        .0
        .lock()
        .unwrap()
        .statuses
        .insert(worktree.path.clone(), CLEAN_MERGED);
    let (dag, _) = fixture(workspace.clone())?;
    record_owners(&dag, &[&worktree])?;
    // The attempt records a different path, but its agent works in this worktree.
    add_attempt(&dag, &attempt_path, AttemptState::Active)?;
    add_agent(&dag, "agent", &worktree.path, None, AgentState::Running)?;
    let expected = [(
        worktree.clone(),
        PruneVerdict::Keep(KeepReason::OpenAttempt),
    )];

    assert_eq!(dag.prune(None, true, false)?, expected);
    assert_eq!(dag.prune(None, false, false)?, expected);

    assert!(workspace.0.lock().unwrap().removals.is_empty());
    assert_eq!(agent_state(&dag, "agent")?, AgentState::Running);
    Ok(())
}
