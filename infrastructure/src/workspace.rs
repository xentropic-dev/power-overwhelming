use anyhow::{Context, Result, ensure};
use application::{BranchStatus, CreatedWorktree, Workspace, WorktreeRoot, WorktreeStatus};
use domain::worktree::ManagedWorktree;
use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::ffi::OsStrExt,
    path::{self, Path, PathBuf},
    process::{Command, Output},
};

pub struct LocalWorkspace;

#[derive(Default)]
pub struct LocalWorktreeRoot {
    #[cfg(feature = "lua-config")]
    defaults: crate::config::Sections,
}

impl LocalWorktreeRoot {
    /// Supply the desktop defaults used to seed Lua globals in the user's config.
    #[cfg(feature = "lua-config")]
    pub fn new(defaults: crate::config::Sections) -> Self {
        Self { defaults }
    }
}

impl WorktreeRoot for LocalWorktreeRoot {
    fn worktree_root(&self) -> Result<PathBuf> {
        // Read config at each request so edits apply to subsequent creations.
        #[cfg(feature = "lua-config")]
        let configured = crate::config::load_worktree_root(self.defaults.clone())?;
        #[cfg(not(feature = "lua-config"))]
        let configured = None::<PathBuf>;
        let root = match configured {
            Some(root) => std::path::absolute(root).context("Resolve worktrees.root")?,
            None => power_home()?,
        };
        std::fs::create_dir_all(&root)
            .with_context(|| format!("Create worktree root at {}", root.display()))?;
        Ok(root)
    }
}

/// `~/.power`, the default home for Power-created worktrees.
fn power_home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .context("Set HOME or worktrees.root to choose where worktrees go")?;
    Ok(PathBuf::from(home).join(".power"))
}

/// The common Git directory of a box. Desktop entries use the common Git directory, and
/// registry entries may name the checkout itself.
fn box_repository(repository: &Path) -> Result<PathBuf> {
    let repository = repository.canonicalize().context("Open box repository")?;
    Ok(Checkout::discover(&repository).map_or(repository, |checkout| checkout.repository))
}

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
            Ok::<_, anyhow::Error>(Path::new(git_line(&output)).canonicalize()?)
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

pub struct ListedWorktree {
    pub path: PathBuf,
    pub head: Option<WorktreeHead>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum WorktreeHead {
    Branch(String),
    Detached,
}

pub fn repository_worktree_details(repository: &Path) -> Result<Vec<ListedWorktree>> {
    let mut command = git();
    command.arg("--git-dir").arg(repository);
    worktree_details(command)
}

fn worktree_listing(command: Command) -> Result<Vec<PathBuf>> {
    Ok(worktree_details(command)?
        .into_iter()
        .map(|worktree| worktree.path)
        .collect())
}

fn worktree_details(command: Command) -> Result<Vec<ListedWorktree>> {
    let listing = worktree_output(command)?;
    let mut worktrees = Vec::<ListedWorktree>::new();
    for field in listing.split(|&byte| byte == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            worktrees.push(ListedWorktree {
                path: PathBuf::from(OsStr::from_bytes(path)),
                head: None,
            });
        } else if let Some(worktree) = worktrees.last_mut() {
            worktree.head = if let Some(branch) = field.strip_prefix(b"branch refs/heads/") {
                Some(WorktreeHead::Branch(
                    String::from_utf8_lossy(branch).into_owned(),
                ))
            } else if field == b"detached" {
                Some(WorktreeHead::Detached)
            } else {
                continue;
            };
        }
    }
    Ok(worktrees)
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
        let repository = box_repository(repository)?;
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

    fn main_checkout(&self, repository: &Path) -> Result<Option<PathBuf>> {
        let repository = box_repository(repository)?;
        let mut bare = git();
        bare.arg("--git-dir")
            .arg(&repository)
            .args(["rev-parse", "--is-bare-repository"]);
        if git_line(&git_output(bare)?) == OsStr::new("true") {
            return Ok(None);
        }
        // Git lists the main worktree first.
        let listed =
            repository_worktrees(&repository).context("List worktrees in the box repository")?;
        Ok(listed.first().and_then(|path| path.canonicalize().ok()))
    }

    fn head(&self, worktree: &Path) -> Result<String> {
        resolve_commit(worktree, "HEAD")
    }

    fn branch_exists(&self, repository: &Path, branch: &str) -> Result<BranchStatus> {
        let reference = format!("refs/heads/{branch}");
        let branch_field = format!("branch {reference}");
        let listing = worktree_output(git_in(repository))?;
        let mut checked_out_at = None;
        for field in listing.split(|byte| *byte == 0) {
            if let Some(path) = field.strip_prefix(b"worktree ") {
                checked_out_at = Some(PathBuf::from(OsStr::from_bytes(path)));
            } else if field == branch_field.as_bytes() {
                return Ok(BranchStatus::Exists { checked_out_at });
            }
        }
        if ref_exists(repository, OsStr::new(&reference))? {
            Ok(BranchStatus::Exists {
                checked_out_at: None,
            })
        } else {
            Ok(BranchStatus::Missing)
        }
    }

    fn create_worktree(
        &self,
        repository: &Path,
        path: &Path,
        branch: &str,
        base: &str,
    ) -> Result<CreatedWorktree> {
        let base_commit = resolve_commit(repository, base)?;
        let path = path::absolute(path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Create worktree parent directory {}", parent.display())
            })?;
        }
        let mut command = git_in(repository);
        command
            .args(["worktree", "add", "-b", branch, "--"])
            .arg(&path)
            .arg(&base_commit);
        git_output(command)?;
        let created = (|| {
            let path = path.canonicalize()?;
            fs::write(
                worktree_marker(&path)?,
                marker_contents(branch, &base_commit),
            )
            .context("Record Power worktree ownership in Git metadata")?;
            Ok(CreatedWorktree { path, base_commit })
        })();
        if created.is_err()
            && let Err(cleanup) = self.remove_worktree(&path, true)
        {
            return created
                .with_context(|| format!("Could not undo worktree creation: {cleanup:#}"));
        }
        created
    }

    fn owns_worktree(&self, worktree: &ManagedWorktree) -> bool {
        let Ok(path) = self.validate_box_worktree(&worktree.repository, &worktree.path) else {
            return false;
        };
        let branch = format!("refs/heads/{}", worktree.branch);
        path == worktree.path
            && matches!(symbolic_ref(&path, "HEAD"), Ok(Some(head)) if head == branch.as_str())
            && worktree_marker(&path)
                .and_then(|marker| Ok(fs::read_to_string(marker)?))
                .is_ok_and(|contents| {
                    contents == marker_contents(&worktree.branch, &worktree.base_commit)
                })
    }

    fn worktree_status(&self, path: &Path) -> Result<WorktreeStatus> {
        let checkout = Checkout::discover(path)?;
        let mut command = git_in(&checkout.worktree);
        command.args([
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ]);
        let dirty = !git_output(command)?.is_empty();
        let merged_into_default = match (
            symbolic_ref(&checkout.worktree, "HEAD")?,
            default_branch(&checkout.repository)?,
        ) {
            (Some(branch), Some(default)) => {
                let mut command = git_in(&checkout.worktree);
                command
                    .args(["merge-base", "--is-ancestor"])
                    .arg(branch)
                    .arg(default);
                git_query(command)?.is_some()
            }
            _ => false,
        };
        Ok(WorktreeStatus {
            dirty,
            merged_into_default,
        })
    }

    fn worktree_exists(&self, path: &Path) -> bool {
        // Only a definite absence counts. A permission or I/O error keeps the agent.
        !matches!(path.try_exists(), Ok(false))
    }

    fn remove_worktree(&self, path: &Path, delete_branch: bool) -> Result<()> {
        let checkout = Checkout::discover(path)?;
        ensure!(
            path.canonicalize()? == checkout.worktree,
            "Choose an existing worktree root"
        );
        let branch = if delete_branch {
            symbolic_ref(&checkout.worktree, "HEAD")?
        } else {
            None
        };
        let mut command = git_in(&checkout.repository);
        command
            .args(["worktree", "remove", "--"])
            .arg(&checkout.worktree);
        git_output(command)?;
        if let Some(branch) = branch
            .as_deref()
            .and_then(|reference| reference.as_bytes().strip_prefix(b"refs/heads/"))
        {
            let mut command = git_in(&checkout.repository);
            command
                .args(["branch", "-d", "--"])
                .arg(OsStr::from_bytes(branch));
            git_output(command).map_err(|error| {
                anyhow::anyhow!(
                    "Removed worktree {}. Branch deletion failed. {error:#}",
                    checkout.worktree.display()
                )
            })?;
        }
        Ok(())
    }

    fn open_artifact_file(&self, path: &Path) -> Result<PathBuf> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Artifact is unavailable at {}", path.display()))?;
        ensure!(path.is_file(), "Artifact is not a regular file");
        Ok(path)
    }

    fn list_worktrees(&self, directory: &Path) -> Result<Vec<PathBuf>> {
        worktree_listing(git_in(directory))
    }
}

fn worktree_marker(path: &Path) -> Result<PathBuf> {
    let mut command = git_in(path);
    command.args(["rev-parse", "--absolute-git-dir"]);
    let output = git_output(command)?;
    Ok(Path::new(git_line(&output)).join("power-managed-worktree"))
}

fn marker_contents(branch: &str, base_commit: &str) -> String {
    format!("{branch}\n{base_commit}\n")
}

fn resolve_commit(directory: &Path, revision: &str) -> Result<String> {
    let mut command = git_in(directory);
    command
        .args(["rev-parse", "--verify", "--end-of-options"])
        .arg(format!("{revision}^{{commit}}"));
    Ok(git_line(&git_output(command)?)
        .to_str()
        .context("Read Git commit ID")?
        .to_owned())
}

fn default_branch(repository: &Path) -> Result<Option<OsString>> {
    // Use origin/HEAD to name the default branch. Prefer its local ref so local
    // merges count, then its remote-tracking ref. With no usable origin/HEAD,
    // fall back to the main Git directory's symbolic HEAD, including in a bare repo.
    let origin_head = "refs/remotes/origin/HEAD";
    if let Some(target) = symbolic_ref(repository, origin_head)? {
        if let Some(name) = target.as_bytes().strip_prefix(b"refs/remotes/origin/") {
            let mut local = OsString::from("refs/heads/");
            local.push(OsStr::from_bytes(name));
            if ref_exists(repository, &local)? {
                return Ok(Some(local));
            }
        }
        if ref_exists(repository, OsStr::new(origin_head))? {
            return Ok(Some(OsString::from(origin_head)));
        }
    }
    if let Some(main_branch) = symbolic_ref(repository, "HEAD")?
        && ref_exists(repository, &main_branch)?
    {
        return Ok(Some(main_branch));
    }
    Ok(None)
}

fn symbolic_ref(directory: &Path, name: &str) -> Result<Option<OsString>> {
    let mut command = git_in(directory);
    command.args(["symbolic-ref", "--quiet", "--", name]);
    Ok(git_query(command)?.map(|output| git_line(&output).to_owned()))
}

fn ref_exists(directory: &Path, reference: &OsStr) -> Result<bool> {
    let mut command = git_in(directory);
    command
        .args(["show-ref", "--verify", "--quiet", "--"])
        .arg(reference);
    Ok(git_query(command)?.is_some())
}

fn git() -> Command {
    let mut command = Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR");
    command
}

fn git_in(directory: &Path) -> Command {
    let mut command = git();
    command.arg("-C").arg(directory);
    command
}

fn worktree_output(mut command: Command) -> Result<Vec<u8>> {
    command.args(["worktree", "list", "--porcelain", "-z"]);
    git_output(command)
}

fn git_output(mut command: Command) -> Result<Vec<u8>> {
    checked_git_stdout(command.output()?)
}

// The Git queries used here signal a negative answer with exit code 1 and no stderr.
fn git_query(mut command: Command) -> Result<Option<Vec<u8>>> {
    let output = command.output()?;
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        return Ok(None);
    }
    checked_git_stdout(output).map(Some)
}

fn checked_git_stdout(output: Output) -> Result<Vec<u8>> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        ensure!(!message.is_empty(), "Git exited with {}", output.status);
        anyhow::bail!("{message}");
    }
    Ok(output.stdout)
}

fn git_line(output: &[u8]) -> &OsStr {
    OsStr::from_bytes(output.strip_suffix(b"\n").unwrap_or(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_at(directory: &Path, args: &[&str]) -> Result<()> {
        let mut command = git_in(directory);
        command.args(args);
        git_output(command)?;
        Ok(())
    }

    fn init_repository(root: &Path) -> Result<PathBuf> {
        let repository = root.join("main checkout");
        fs::create_dir(&repository)?;
        git_at(&repository, &["init", "-q", "--initial-branch=main"])?;
        git_at(&repository, &["config", "user.name", "Power Test"])?;
        git_at(
            &repository,
            &["config", "user.email", "power@example.invalid"],
        )?;
        git_at(&repository, &["config", "commit.gpgSign", "false"])?;
        fs::write(repository.join("tracked"), "Initial\n")?;
        git_at(&repository, &["add", "tracked"])?;
        git_at(&repository, &["commit", "-qm", "Initial"])?;
        Ok(repository)
    }

    #[test]
    fn main_checkout_is_shared_by_linked_worktrees_and_absent_for_bare_repos() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let main = init_repository(&root)?;
        let run = |args: &[&str]| git_at(&root, args);
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
        assert_eq!(main_checkout(&checkout.repository)?, Some(main.clone()));
        let worktrees = repository_worktree_details(&checkout.repository)?;
        let heads: Vec<_> = worktrees
            .into_iter()
            .map(|worktree| worktree.head)
            .collect();
        assert_eq!(
            heads,
            [
                Some(WorktreeHead::Branch("main".into())),
                Some(WorktreeHead::Detached)
            ]
        );
        assert_eq!(repository_worktrees(&checkout.repository)?, [main, linked]);

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

    #[test]
    fn a_recreated_checkout_with_the_same_path_and_branch_is_not_managed() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let main = init_repository(&root)?;
        let repository = Checkout::discover(&main)?.repository;
        let created =
            LocalWorkspace.create_worktree(&repository, &root.join("worker"), "feature", "main")?;
        let worktree = ManagedWorktree {
            box_id: "test".into(),
            repository,
            path: created.path,
            branch: "feature".into(),
            base_commit: created.base_commit,
            created_at: 0,
            creator: domain::worktree::WorktreeCreator::View {
                box_id: "test".into(),
                session: "source".into(),
            },
        };
        assert!(LocalWorkspace.owns_worktree(&worktree));

        LocalWorkspace.remove_worktree(&worktree.path, false)?;
        let mut command = git_in(&main);
        command
            .args(["worktree", "add", "--"])
            .arg(&worktree.path)
            .arg("feature");
        git_output(command)?;
        assert!(!LocalWorkspace.owns_worktree(&worktree));
        Ok(())
    }

    #[test]
    fn creates_worktrees_from_explicit_bases_and_finds_local_branches() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let main = init_repository(&root)?;
        let repository = Checkout::discover(&main)?.repository;
        let initial = LocalWorkspace.head(&main)?;
        let provisioned = root.join("provisioned");
        let alias = root.join("alias");
        fs::create_dir(&provisioned)?;
        std::os::unix::fs::symlink(&provisioned, &alias)?;
        let relative = "nested/first\ncheckout";
        let first = provisioned.join(relative);
        assert_eq!(
            LocalWorkspace.create_worktree(
                &repository,
                &alias.join(relative),
                "worker/first",
                "main"
            )?,
            CreatedWorktree {
                path: first.clone(),
                base_commit: initial.clone()
            }
        );
        assert_eq!(LocalWorkspace.head(&first)?, initial);
        git_at(&first, &["commit", "-q", "--allow-empty", "-m", "Work"])?;
        let first_head = LocalWorkspace.head(&first)?;
        assert_ne!(first_head, initial);
        let dependent = root.join("provisioned/dependent checkout");
        let created =
            LocalWorkspace.create_worktree(&first, &dependent, "worker/dependent", "HEAD")?;
        assert_eq!(created.base_commit, first_head);
        assert_eq!(LocalWorkspace.head(&dependent)?, first_head);

        assert_eq!(
            LocalWorkspace.branch_exists(&repository, "worker/first")?,
            BranchStatus::Exists {
                checked_out_at: Some(first)
            }
        );
        git_at(&main, &["branch", "idle"])?;
        assert_eq!(
            LocalWorkspace.branch_exists(&dependent, "idle")?,
            BranchStatus::Exists {
                checked_out_at: None
            }
        );
        git_at(&main, &["tag", "tag-only"])?;
        assert_eq!(
            LocalWorkspace.branch_exists(&repository, "tag-only")?,
            BranchStatus::Missing
        );

        let unused = root.join("duplicate");
        let error = LocalWorkspace
            .create_worktree(&repository, &unused, "worker/first", "main")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("fatal:") && error.contains("worker/first"),
            "{error}"
        );
        assert!(!unused.exists());
        let error = LocalWorkspace
            .create_worktree(&repository, &unused, "worker/invalid", "missing-base")
            .unwrap_err()
            .to_string();
        assert!(error.contains("fatal:"), "{error}");
        assert_eq!(
            LocalWorkspace.branch_exists(&repository, "worker/invalid")?,
            BranchStatus::Missing
        );
        Ok(())
    }

    #[test]
    fn status_checks_tracked_and_untracked_changes_and_git_ancestry() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let main = init_repository(temp.path())?;
        let path = temp.path().join("worker");
        LocalWorkspace.create_worktree(&main, &path, "worker/status", "main")?;
        let clean_and_merged = WorktreeStatus {
            dirty: false,
            merged_into_default: true,
        };
        assert_eq!(LocalWorkspace.worktree_status(&path)?, clean_and_merged);

        git_at(&main, &["config", "status.showUntrackedFiles", "no"])?;
        let untracked = path.join("untracked");
        fs::write(&untracked, "Untracked\n")?;
        assert!(LocalWorkspace.worktree_status(&path)?.dirty);
        fs::remove_file(untracked)?;
        fs::write(path.join("tracked"), "Edited\n")?;
        assert!(LocalWorkspace.worktree_status(&path)?.dirty);
        git_at(&path, &["add", "tracked"])?;
        assert!(LocalWorkspace.worktree_status(&path)?.dirty);
        git_at(&path, &["commit", "-qm", "Work"])?;
        assert_eq!(
            LocalWorkspace.worktree_status(&path)?,
            WorktreeStatus {
                dirty: false,
                merged_into_default: false
            }
        );

        git_at(&main, &["merge", "--ff-only", "worker/status"])?;
        assert_eq!(LocalWorkspace.worktree_status(&path)?, clean_and_merged);
        git_at(&path, &["switch", "--detach"])?;
        assert!(!LocalWorkspace.worktree_status(&path)?.merged_into_default);
        Ok(())
    }

    #[test]
    fn origin_head_selects_the_default_branch_when_main_checkout_moves() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let main = init_repository(temp.path())?;
        git_at(&main, &["update-ref", "refs/remotes/origin/main", "HEAD"])?;
        git_at(
            &main,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        )?;
        git_at(&main, &["branch", "other"])?;
        let path = temp.path().join("worker");
        LocalWorkspace.create_worktree(&main, &path, "worker/origin", "main")?;
        git_at(&path, &["commit", "-q", "--allow-empty", "-m", "Work"])?;
        git_at(&main, &["merge", "--ff-only", "worker/origin"])?;
        git_at(&main, &["switch", "other"])?;
        assert!(LocalWorkspace.worktree_status(&path)?.merged_into_default);

        git_at(&main, &["branch", "-m", "main", "renamed"])?;
        assert!(!LocalWorkspace.worktree_status(&path)?.merged_into_default);
        git_at(
            &main,
            &["update-ref", "refs/remotes/origin/main", "worker/origin"],
        )?;
        git_at(&main, &["switch", "--detach"])?;
        assert!(LocalWorkspace.worktree_status(&path)?.merged_into_default);
        git_at(
            &main,
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        )?;
        assert!(!LocalWorkspace.worktree_status(&path)?.merged_into_default);
        Ok(())
    }

    #[test]
    fn removes_clean_worktrees_and_only_deletes_branches_on_request() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let main = init_repository(temp.path())?;
        let path = temp.path().join("worker");
        LocalWorkspace.create_worktree(&main, &path, "worker/keep", "main")?;
        let subdirectory = path.join("subdirectory");
        fs::create_dir(&subdirectory)?;
        assert!(LocalWorkspace.remove_worktree(&subdirectory, true).is_err());
        assert!(subdirectory.exists());
        let untracked = path.join("untracked");
        fs::write(&untracked, "Untracked\n")?;
        let error = LocalWorkspace
            .remove_worktree(&path, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fatal:"), "{error}");
        assert!(untracked.exists());
        fs::remove_file(untracked)?;
        LocalWorkspace.remove_worktree(&path, false)?;
        assert!(!path.exists());
        assert_eq!(
            LocalWorkspace.branch_exists(&main, "worker/keep")?,
            BranchStatus::Exists {
                checked_out_at: None
            }
        );

        LocalWorkspace.create_worktree(&main, &path, "worker/delete", "main")?;
        LocalWorkspace.remove_worktree(&path, true)?;
        assert!(!path.exists());
        assert_eq!(
            LocalWorkspace.branch_exists(&main, "worker/delete")?,
            BranchStatus::Missing
        );

        LocalWorkspace.create_worktree(&main, &path, "worker/unmerged", "main")?;
        git_at(&path, &["commit", "-q", "--allow-empty", "-m", "Work"])?;
        let error = LocalWorkspace
            .remove_worktree(&path, true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Removed worktree") && error.contains("worker/unmerged"),
            "{error}"
        );
        assert!(!path.exists());
        assert_eq!(
            LocalWorkspace.branch_exists(&main, "worker/unmerged")?,
            BranchStatus::Exists {
                checked_out_at: None
            }
        );
        Ok(())
    }

    #[test]
    fn creates_and_removes_worktrees_from_a_bare_repository() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let main = init_repository(&root)?;
        git_at(
            &root,
            &["clone", "-q", "--bare", "main checkout", "bare.git"],
        )?;
        let bare = root.join("bare.git");
        let path = root.join("bare linked");
        let created = LocalWorkspace.create_worktree(&bare, &path, "worker/bare", "main")?;
        assert_eq!(created.path, path);
        assert_eq!(created.base_commit, LocalWorkspace.head(&main)?);
        assert_eq!(LocalWorkspace.head(&path)?, created.base_commit);
        assert_eq!(
            LocalWorkspace.branch_exists(&bare, "worker/bare")?,
            BranchStatus::Exists {
                checked_out_at: Some(path.clone())
            }
        );
        assert_eq!(
            LocalWorkspace.worktree_status(&path)?,
            WorktreeStatus {
                dirty: false,
                merged_into_default: true
            }
        );
        LocalWorkspace.remove_worktree(&path, true)?;
        assert!(!path.exists());
        assert_eq!(
            LocalWorkspace.branch_exists(&bare, "worker/bare")?,
            BranchStatus::Missing
        );
        Ok(())
    }
}
