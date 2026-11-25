use anyhow::{Context, Result, ensure};
use application::Workspace;
use std::{
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

pub struct LocalWorkspace;

impl Workspace for LocalWorkspace {
    fn open_worktree(&self, directory: &Path) -> Result<PathBuf> {
        let worktree = directory.canonicalize().context("Open worktree")?;
        ensure!(worktree.is_dir(), "Worktree must be a directory");
        Ok(worktree)
    }

    fn open_artifact_file(&self, path: &Path) -> Result<PathBuf> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Artifact is unavailable at {}", path.display()))?;
        ensure!(path.is_file(), "Artifact is not a regular file");
        Ok(path)
    }

    fn list_worktrees(&self, directory: &Path) -> Result<Vec<PathBuf>> {
        let listing = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["worktree", "list", "--porcelain", "-z"])
            .output()?;
        ensure!(
            listing.status.success(),
            "{}",
            String::from_utf8_lossy(&listing.stderr).trim()
        );
        Ok(listing
            .stdout
            .split(|byte| *byte == 0)
            .filter_map(|field| field.strip_prefix(b"worktree "))
            .map(|path| PathBuf::from(OsStr::from_bytes(path)))
            .collect())
    }
}
