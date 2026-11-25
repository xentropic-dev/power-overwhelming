use anyhow::{Context, Result, ensure};
use application::Workspace;
use std::{
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

pub struct LocalWorkspace;

pub struct Checkout {
    pub repository: PathBuf,
    pub worktree: PathBuf,
}

impl Checkout {
    pub fn discover(directory: &Path) -> Result<Self> {
        let git_path = |flag| {
            let mut command = git();
            command
                .arg("-C")
                .arg(directory)
                .args(["rev-parse", "--path-format=absolute", flag]);
            let output = git_output(command)?;
            let path = OsStr::from_bytes(output.strip_suffix(b"\n").unwrap_or(&output));
            Ok::<_, anyhow::Error>(Path::new(path).canonicalize()?)
        };
        Ok(Self {
            worktree: git_path("--show-toplevel")?,
            repository: git_path("--git-common-dir")?,
        })
    }

    pub fn in_repository(directory: &Path, repository: &Path) -> Result<Self> {
        let checkout = Self::discover(directory)?;
        ensure!(
            checkout.repository == repository,
            "Choose a worktree in this box's repository"
        );
        Ok(checkout)
    }
}

pub fn repository_worktrees(repository: &Path) -> Result<Vec<PathBuf>> {
    let mut command = git();
    command.arg("--git-dir").arg(repository);
    worktree_listing(command)
}

pub fn main_checkout(repository: &Path) -> Result<Option<PathBuf>> {
    let mut command = git();
    command.arg("--git-dir").arg(repository);
    let listing = worktree_output(command)?;
    let mut fields = listing
        .split(|byte| *byte == 0)
        .take_while(|field| !field.is_empty());
    let path = fields
        .next()
        .and_then(|field| field.strip_prefix(b"worktree "));
    if fields.any(|field| field == b"bare") {
        return Ok(None);
    }
    Ok(path
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .filter(|path| path.is_dir()))
}

impl Workspace for LocalWorkspace {
    fn open_worktree(&self, directory: &Path) -> Result<PathBuf> {
        let worktree = directory.canonicalize().context("Open worktree")?;
        ensure!(worktree.is_dir(), "Worktree must be a directory");
        Ok(worktree)
    }

    fn validate_box_worktree(&self, repository: &Path, worktree: &Path) -> Result<PathBuf> {
        let worktree = self.open_worktree(worktree)?;
        let repository = repository.canonicalize().context("Open box repository")?;
        // Desktop entries use the common Git directory. Also accept registry
        // entries that name the checkout itself.
        let repository =
            Checkout::discover(&repository).map_or(repository, |checkout| checkout.repository);
        let listed =
            repository_worktrees(&repository).context("List worktrees in the box repository")?;
        ensure!(
            listed
                .iter()
                .any(|path| path.canonicalize().is_ok_and(|path| path == worktree)),
            "Choose an existing worktree in the box repository"
        );
        let checkout = Checkout::in_repository(&worktree, &repository)
            .context("Choose an existing worktree in the box repository")?;
        ensure!(
            checkout.worktree == worktree,
            "Choose an existing worktree root"
        );
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
        let mut command = git();
        command.arg("-C").arg(directory);
        worktree_listing(command)
    }
}

fn git() -> Command {
    let mut command = Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR");
    command
}

fn worktree_listing(command: Command) -> Result<Vec<PathBuf>> {
    Ok(worktree_output(command)?
        .split(|byte| *byte == 0)
        .filter_map(|field| field.strip_prefix(b"worktree "))
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .collect())
}

fn worktree_output(mut command: Command) -> Result<Vec<u8>> {
    command.args(["worktree", "list", "--porcelain", "-z"]);
    git_output(command)
}

fn git_output(mut command: Command) -> Result<Vec<u8>> {
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_at(directory: &Path, args: &[&OsStr]) -> Result<()> {
        let mut command = git();
        command.arg("-C").arg(directory).args(args);
        git_output(command)?;
        Ok(())
    }

    #[test]
    fn main_checkout_is_shared_by_linked_worktrees_and_absent_for_bare_repos() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let run = |args: &[&str]| git_at(&root, &args.iter().map(OsStr::new).collect::<Vec<_>>());
        run(&["init", "-q", "main checkout"])?;
        run(&[
            "-C",
            "main checkout",
            "-c",
            "user.name=Power Test",
            "-c",
            "user.email=power@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Initial",
        ])?;
        run(&[
            "-C",
            "main checkout",
            "worktree",
            "add",
            "-q",
            "--detach",
            "../linked checkout",
        ])?;
        let linked = root.join("linked checkout");
        let checkout = Checkout::discover(&linked)?;
        assert_eq!(checkout.worktree, linked);
        assert_eq!(
            main_checkout(&checkout.repository)?,
            Some(root.join("main checkout"))
        );
        assert_eq!(
            repository_worktrees(&checkout.repository)?,
            [root.join("main checkout"), linked]
        );

        run(&["clone", "-q", "--bare", "main checkout", "bare.git"])?;
        run(&[
            "-C",
            "bare.git",
            "worktree",
            "add",
            "-q",
            "--detach",
            "../bare linked",
        ])?;
        let checkout = Checkout::discover(&root.join("bare linked"))?;
        assert_eq!(main_checkout(&checkout.repository)?, None);
        Ok(())
    }
}
