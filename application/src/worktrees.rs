use super::{
    BoxIcons, BoxRegistry, BranchStatus, DagApplication, DagReader, DagStore, WorktreeCreator,
    default_worktree_path, now, select_box,
};
use crate::TerminalRuntime;
use anyhow::{Context, Result, bail, ensure};
use domain::{
    dag::{AgentState, BoxInfo},
    worktree::{KeepReason, ManagedWorktree, PruneStatus, PruneVerdict, prune_verdict},
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

impl<T: TerminalRuntime, S: DagStore, B: BoxRegistry, I: BoxIcons> DagApplication<T, S, B, I> {
    pub fn create_view_worktree(
        &self,
        box_selector: &str,
        from_session: &str,
        branch: &str,
    ) -> Result<PathBuf> {
        let boxes = self.registry.boxes()?;
        let entry = select_box(&boxes, box_selector)?;
        let box_id = &entry.info.id;
        ensure!(
            entry.sessions.iter().any(|session| session == from_session),
            "Power session {from_session:?} does not belong to box {box_id:?}"
        );
        let workspace = self.application.workspace.as_ref();
        let mut created_path = None;
        let result = self.store.write(|db| {
            let source = self.application.session(from_session)?;
            let path = self.new_worktree_path(&entry.info, branch)?;
            let base = workspace.head(&source.worktree)?;
            let created =
                workspace.create_worktree(&entry.info.repository, &path, branch, &base)?;
            created_path = Some(created.path.clone());
            db.insert_worktree(&ManagedWorktree {
                box_id: box_id.clone(),
                repository: entry.info.repository.clone(),
                path: created.path.clone(),
                branch: branch.into(),
                base_commit: created.base_commit,
                created_at: now(),
                creator: WorktreeCreator::View {
                    box_id: box_id.clone(),
                    session: from_session.into(),
                },
            })?;
            Ok(created.path)
        });
        if result.is_err()
            && let Some(path) = created_path
            && let Err(cleanup) = workspace.remove_worktree(&path, true)
        {
            return result
                .with_context(|| format!("Could not undo worktree creation: {cleanup:#}"));
        }
        result
    }

    pub(super) fn new_worktree_path(&self, info: &BoxInfo, branch: &str) -> Result<PathBuf> {
        ensure!(
            !branch.trim().is_empty() && !matches!(branch, "." | ".."),
            "Invalid branch name {branch:?}"
        );
        let root = self
            .worktree_root
            .as_ref()
            .context("Worktree provisioning is unavailable")?;
        if let BranchStatus::Exists { checked_out_at } = self
            .application
            .workspace
            .branch_exists(&info.repository, branch)?
        {
            if let Some(path) = checked_out_at {
                bail!(
                    "Branch {branch:?} already exists and is checked out at {}",
                    path.display()
                );
            }
            bail!("Branch {branch:?} already exists");
        }
        let root = root.worktree_root()?;
        Ok(default_worktree_path(&root, &info.name, branch, |path| {
            path.symlink_metadata().is_ok()
        }))
    }

    /// Lists ownership records for a box ID, or for all boxes when no ID is given.
    pub fn list_managed_worktrees(&self, box_id: Option<&str>) -> Result<Vec<ManagedWorktree>> {
        self.store.read(|db| db.list_worktrees(box_id))
    }

    pub fn prune(
        &self,
        box_id: Option<&str>,
        dry_run: bool,
        delete_branch: bool,
    ) -> Result<Vec<(ManagedWorktree, PruneVerdict)>> {
        if dry_run {
            return self.store.read(|db| {
                let in_use: BTreeSet<_> = self
                    .application
                    .sessions
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|live| self.application.runtime.is_running(&live.terminal))
                    .map(|live| live.session.worktree.clone())
                    .collect();
                db.list_worktrees(box_id)?
                    .into_iter()
                    .map(|worktree| {
                        let verdict = self.worktree_verdict(
                            &worktree,
                            has_open_attempt(db, &worktree.path)?,
                            in_use.contains(&worktree.path),
                        )?;
                        Ok((worktree, verdict))
                    })
                    .collect()
            });
        }

        let mut results = Vec::new();
        for worktree in self.list_managed_worktrees(box_id)? {
            // Commit each removal independently. A later Git error must leave
            // the earlier deletions recorded in the store.
            let verdict = self.store.write(|db| {
                if db.find_worktree_by_path(&worktree.path)?.as_ref() != Some(&worktree) {
                    return Ok(Ok(None));
                }
                let open_attempt = has_open_attempt(db, &worktree.path)?;
                let sessions = self.application.sessions.lock().unwrap();
                let in_use = sessions.values().any(|live| {
                    live.session.worktree == worktree.path
                        && self.application.runtime.is_running(&live.terminal)
                });
                let verdict = self.worktree_verdict(&worktree, open_attempt, in_use)?;
                if verdict == PruneVerdict::Remove {
                    let workspace = &self.application.workspace;
                    let removal = workspace
                        .remove_worktree(&worktree.path, delete_branch)
                        .with_context(|| format!("Remove worktree {}", worktree.path.display()));
                    // Git can remove the checkout and then fail to delete the branch.
                    if removal.is_ok() || !workspace.owns_worktree(&worktree) {
                        db.remove_worktree(&worktree.path)?;
                    }
                    return Ok(removal.map(|()| Some(verdict)));
                }
                Ok(Ok(Some(verdict)))
            })??;
            if let Some(verdict) = verdict {
                let removed = verdict == PruneVerdict::Remove;
                let path = worktree.path.clone();
                results.push((worktree, verdict));
                if removed {
                    self.retire_worktree_agents(&path)
                        .with_context(|| format!("Retire agents of {}", path.display()))?;
                }
            }
        }
        Ok(results)
    }

    /// Stops agents left running in a worktree Power removed. An agent with a session
    /// has it ended, and the session's exit marks the agent exited and interrupts its
    /// attempts. An agent without a session is marked exited directly.
    fn retire_worktree_agents(&self, path: &Path) -> Result<()> {
        let agents = self.store.read(|db| db.worktree_agents(path))?;
        let mut orphans = Vec::new();
        for agent in agents {
            if agent.state != AgentState::Running {
                continue;
            }
            match agent.session.as_deref() {
                Some(session) if self.application.session(session).is_ok() => {
                    self.application.end_session(session)?;
                }
                _ => orphans.push(agent.id),
            }
        }
        if !orphans.is_empty() {
            self.write(|db| {
                for id in &orphans {
                    db.set_agent_exited(id)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn worktree_verdict(
        &self,
        worktree: &ManagedWorktree,
        open_attempt: bool,
        in_use: bool,
    ) -> Result<PruneVerdict> {
        let workspace = &self.application.workspace;
        if !workspace.owns_worktree(worktree) {
            return Ok(PruneVerdict::Keep(KeepReason::NoLongerManaged));
        }
        let status = workspace
            .worktree_status(&worktree.path)
            .with_context(|| format!("Check worktree {}", worktree.path.display()))?;
        Ok(prune_verdict(PruneStatus {
            dirty: status.dirty,
            merged_into_default: status.merged_into_default,
            open_attempt,
            in_use,
        }))
    }
}

/// Whether an open attempt runs in this worktree, or belongs to an agent that works in it.
fn has_open_attempt(db: &dyn DagReader, path: &Path) -> Result<bool> {
    let agents: BTreeSet<_> = db
        .worktree_agents(path)?
        .into_iter()
        .map(|agent| agent.id)
        .collect();
    Ok(db
        .open_attempts(None)?
        .iter()
        .any(|open| open.attempt.worktree == path || agents.contains(&open.attempt.agent)))
}
