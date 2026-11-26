use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorktree {
    pub box_id: String,
    pub repository: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base_commit: String,
    /// Unix seconds.
    pub created_at: u64,
    pub creator: WorktreeCreator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeCreator {
    Attempt {
        run: String,
        task: String,
        attempt: String,
    },
    View {
        box_id: String,
        session: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeRequest {
    Existing(PathBuf),
    RetryManaged,
    New {
        branch: String,
        base: Option<String>,
    },
}

/// Chooses a path for a repository folder name and a valid Git branch name.
/// When the unsuffixed path is taken, tries `-2`, `-3`, and so on.
pub fn default_worktree_path(
    root: &Path,
    repository_name: &str,
    branch: &str,
    mut exists: impl FnMut(&Path) -> bool,
) -> PathBuf {
    let repository = root.join(repository_name);
    let slug = branch.replace('/', "-");
    let mut path = repository.join(&slug);
    let mut suffix = 2;
    while exists(&path) {
        path = repository.join(format!("{slug}-{suffix}"));
        suffix += 1;
    }
    path
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PruneStatus {
    pub dirty: bool,
    pub merged_into_default: bool,
    pub in_use: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PruneVerdict {
    Remove,
    Keep(KeepReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeepReason {
    Dirty,
    Unmerged,
    InUse,
    NoLongerManaged,
}

/// Reports the first applicable keep reason in the order dirty, unmerged, in use.
pub fn prune_verdict(status: PruneStatus) -> PruneVerdict {
    if status.dirty {
        PruneVerdict::Keep(KeepReason::Dirty)
    } else if !status.merged_into_default {
        PruneVerdict::Keep(KeepReason::Unmerged)
    } else if status.in_use {
        PruneVerdict::Keep(KeepReason::InUse)
    } else {
        PruneVerdict::Remove
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_path_uses_the_first_available_slug() {
        let root = Path::new("/worktrees");
        let first = root.join("repo/feature-change");
        let second = root.join("repo/feature-change-2");
        assert_eq!(
            default_worktree_path(root, "repo", "feature/change", |_| false),
            first,
        );
        assert_eq!(
            default_worktree_path(root, "repo", "feature/change", |path| {
                path == first || path == second
            }),
            root.join("repo/feature-change-3"),
        );
    }

    #[test]
    fn prune_requires_a_clean_merged_unused_worktree() {
        let mut status = PruneStatus {
            dirty: true,
            merged_into_default: false,
            in_use: true,
        };
        assert_eq!(prune_verdict(status), PruneVerdict::Keep(KeepReason::Dirty));
        status.dirty = false;
        assert_eq!(
            prune_verdict(status),
            PruneVerdict::Keep(KeepReason::Unmerged)
        );
        status.merged_into_default = true;
        assert_eq!(prune_verdict(status), PruneVerdict::Keep(KeepReason::InUse));
        status.in_use = false;
        assert_eq!(prune_verdict(status), PruneVerdict::Remove);
    }
}
