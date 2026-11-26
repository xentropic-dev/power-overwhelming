use crate::{
    skills::{parse_frontmatter, stub_hash},
    workspace::Checkout,
};
use anyhow::{Context, Result, ensure};
use application::skill_install::{
    ObservedStub, SkillFilesystem, SkillLocation, SkillMutation, SkillTargets,
};
use domain::session::Agent;
use fs2::FileExt;
use rustix::fs::{CWD, RenameFlags, renameat_with};
use std::{
    collections::BTreeSet,
    fs::{self, File, Metadata},
    io::{self, Read},
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _, symlink},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const SHARED_SKILLS: &str = ".agents/skills";
const CLAUDE_SKILLS: &str = ".claude/skills";
const SKILL_FILE: &str = "SKILL.md";
const MAX_STUB_BYTES: u64 = 64 * 1024;

fn home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is required to locate user skills")?;
    Ok(std::path::absolute(home)?)
}

pub fn install_targets(local: bool, claude: bool) -> Result<SkillTargets> {
    let home = home()?;
    let project = if local {
        Some(
            Checkout::discover(&std::env::current_dir()?)
                .context("--local requires a Git worktree")?
                .worktree,
        )
    } else {
        None
    };
    targets_at(&home, project.as_deref(), claude)
}

fn targets_at(home: &Path, project: Option<&Path>, claude: bool) -> Result<SkillTargets> {
    let scope = project.unwrap_or(home);
    let shared = scope.join(SHARED_SKILLS);
    if let Some(project) = project {
        ensure!(
            resolve_root(&shared)?.starts_with(project),
            "{} resolves outside the worktree",
            shared.display()
        );
    }

    let claude_root = scope.join(CLAUDE_SKILLS);
    let detected = if project.is_some() {
        claude_root.is_dir()
    } else {
        scope.join(".claude").is_dir()
    };
    let claude = if claude && detected {
        let resolved = resolve_root(&claude_root);
        let in_scope = project.is_none_or(|project| {
            resolved
                .as_ref()
                .is_ok_and(|root| root.starts_with(project))
        });
        let distinct = match (resolve_root(&shared), resolved) {
            (Ok(shared), Ok(root)) => shared != root,
            _ => true, // Let observation report errors for these locations.
        };
        (in_scope && distinct).then_some(claude_root)
    } else {
        None
    };
    Ok(SkillTargets {
        shared,
        claude,
        backups: home.join(".agents/.power-skill-backups"),
    })
}

// Resolve existing ancestors as well as roots that have already been created.
fn resolve_root(path: &Path) -> io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(resolved) => Ok(resolved),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::symlink_metadata(path) {
            Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or(error)?;
                Ok(resolve_root(parent)?.join(path.file_name().unwrap()))
            }
            _ => Err(error),
        },
        Err(error) => Err(error),
    }
}

pub struct LocalSkillFilesystem;

#[derive(Debug, Eq, PartialEq)]
pub struct SkillSnapshot {
    entry: Option<FileStamp>,
    stub: Option<(FileStamp, String)>,
}

#[derive(Debug, Eq, PartialEq)]
struct FileStamp {
    device: u64,
    inode: u64,
    mode: u32,
    size: u64,
    changed: (i64, i64),
    modified: (i64, i64),
}

impl From<&Metadata> for FileStamp {
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            size: metadata.size(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
        }
    }
}

impl SkillFilesystem for LocalSkillFilesystem {
    type Snapshot = SkillSnapshot;

    fn observe(
        &self,
        path: &Path,
        shared: Option<&Path>,
    ) -> Result<(SkillLocation, SkillSnapshot)> {
        inspect(path, shared).with_context(|| format!("Inspect {}", path.display()))
    }

    fn apply(
        &self,
        path: &Path,
        expected: SkillSnapshot,
        mutation: SkillMutation<'_>,
        backup_root: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        apply(path, expected, mutation, backup_root)
            .with_context(|| format!("Install at {}", path.display()))
    }
}

fn inspect(path: &Path, shared: Option<&Path>) -> Result<(SkillLocation, SkillSnapshot)> {
    let mut snapshot = SkillSnapshot {
        entry: None,
        stub: None,
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((SkillLocation::Missing, snapshot));
        }
        Err(error) => return Err(error.into()),
    };
    snapshot.entry = Some((&metadata).into());
    let location = if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?;
        let first = entries.next().transpose()?;
        let only_stub = first.is_some_and(|entry| entry.file_name() == SKILL_FILE)
            && entries.next().transpose()?.is_none();
        let stub = read_stub(&path.join(SKILL_FILE))?.map(|(stamp, stub)| {
            snapshot.stub = Some((stamp, stub.hash.clone()));
            stub
        });
        SkillLocation::Folder { stub, only_stub }
    } else if metadata.is_symlink()
        && let Some(shared) = shared
    {
        let target = path.parent().unwrap().join(fs::read_link(path)?);
        if target == shared
            || matches!((target.canonicalize(), shared.canonicalize()), (Ok(a), Ok(b)) if a == b)
        {
            SkillLocation::SharedLink
        } else {
            SkillLocation::Other
        }
    } else {
        SkillLocation::Other
    };
    Ok((location, snapshot))
}

fn read_stub(path: &Path) -> Result<Option<(FileStamp, ObservedStub)>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "{} changed while reading", path.display());
    let mut bytes = Vec::new();
    (&file).take(MAX_STUB_BYTES + 1).read_to_end(&mut bytes)?;
    let stamp = FileStamp::from(&before);
    ensure!(
        stamp == FileStamp::from(&file.metadata()?),
        "{} changed while reading",
        path.display()
    );
    let hash = if bytes.len() as u64 <= MAX_STUB_BYTES {
        stub_hash(&bytes)
    } else {
        String::new()
    };
    Ok(Some((
        stamp,
        ObservedStub {
            hash,
            markdown: String::from_utf8_lossy(&bytes).into_owned(),
        },
    )))
}

fn apply(
    path: &Path,
    expected: SkillSnapshot,
    mutation: SkillMutation<'_>,
    backup_root: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let parent = path.parent().unwrap();
    if expected.entry.is_none() {
        fs::create_dir_all(parent)?;
    }
    // Serialize Power mutations of this directory without creating a lock file.
    let lock = File::open(parent)?;
    lock.lock_exclusive()?;

    let staging = tempfile::tempdir_in(parent)?;
    let staged = staging.path().join("skill");
    let update_in_place = expected.entry.is_some() && backup_root.is_none();
    match mutation {
        SkillMutation::Write(markdown) => {
            fs::create_dir(&staged)?;
            let file = staged.join(SKILL_FILE);
            fs::write(&file, markdown)?;
            if update_in_place {
                let (stamp, _) = expected.stub.as_ref().context("Expected a regular stub")?;
                fs::set_permissions(&file, fs::Permissions::from_mode(stamp.mode & 0o777))?;
            }
        }
        SkillMutation::Link { target, .. } => symlink(target, &staged)?,
    }

    let backup = backup_root
        .map(|root| {
            fs::create_dir_all(root)?;
            let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let name = path.file_name().unwrap().to_string_lossy();
            Ok::<_, anyhow::Error>(root.join(format!("{name}-{time}")))
        })
        .transpose()?;

    ensure!(
        inspect(path, None)?.1 == expected,
        "Location changed while preparing the install"
    );

    if let SkillMutation::Link { target, hash } = mutation {
        // Check that the shared stub is still the copy requested by the planner.
        ensure!(
            matches!(
                inspect(target, None)?.0,
                SkillLocation::Folder { stub: Some(stub), only_stub: true } if stub.hash == hash
            ),
            "Shared skill changed while preparing the link"
        );
    }

    if update_in_place {
        fs::rename(staged.join(SKILL_FILE), path.join(SKILL_FILE))?;
    } else if let Some(backup) = &backup {
        rename_new(path, backup).with_context(|| format!("Back up to {}", backup.display()))?;
        if let Err(error) = rename_new(&staged, path) {
            return match rename_new(backup, path) {
                Ok(()) => Err(error).context("Replacement failed and the backup was restored"),
                Err(restore) => Err(error).context(format!(
                    "Replacement failed and restoring the backup failed ({restore}); backup kept at {}",
                    backup.display()
                )),
            };
        }
    } else {
        rename_new(&staged, path)?;
    }
    Ok(backup)
}

fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(Into::into)
}

/// Discover user and project skills from the filesystem for the current directory.
pub fn installed_skills(agent: &Agent) -> Result<BTreeSet<String>> {
    installed_skills_in(agent, &std::env::current_dir()?)
}

/// Discover user and project skills as seen from an agent's launch directory.
pub fn installed_skills_in(agent: &Agent, cwd: &Path) -> Result<BTreeSet<String>> {
    if *agent == Agent::Shell {
        return Ok(BTreeSet::new());
    }
    let home = home()?;
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map_or_else(|| home.join(".codex"), PathBuf::from);
    installed_skills_at(agent, &home, &codex_home, cwd)
}

fn installed_skills_at(
    agent: &Agent,
    home: &Path,
    codex_home: &Path,
    cwd: &Path,
) -> Result<BTreeSet<String>> {
    // Agents other than Claude and Codex report nothing until profiles carry their skill folders.
    let relative = match agent.as_str() {
        "claude" => CLAUDE_SKILLS,
        "codex" => SHARED_SKILLS,
        _ => return Ok(BTreeSet::new()),
    };
    let codex = agent.as_str() == "codex";
    let cwd = cwd.canonicalize()?;
    let root = Checkout::discover(&cwd).map_or_else(|_| cwd.clone(), |checkout| checkout.worktree);
    let mut names = BTreeSet::new();
    scan_skills(&home.join(relative), codex, &mut names)?;
    if codex {
        // Current Codex still discovers existing skills in its legacy user directory.
        scan_skills(&codex_home.join("skills"), codex, &mut names)?;
    }
    for directory in cwd
        .ancestors()
        .take_while(|directory| directory.starts_with(&root))
    {
        scan_skills(&directory.join(relative), codex, &mut names)?;
    }
    Ok(names)
}

fn scan_skills(directory: &Path, codex: bool, names: &mut BTreeSet<String>) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("Read {}", directory.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path().join(SKILL_FILE);
        if !path.is_file() {
            continue;
        }
        let name = if codex {
            let mut markdown = String::new();
            if File::open(path)?
                .take(MAX_STUB_BYTES)
                .read_to_string(&mut markdown)
                .is_err()
            {
                continue;
            }
            let Ok(summary) = parse_frontmatter(&markdown) else {
                continue;
            };
            summary.name.trim_matches(['\'', '"']).to_owned()
        } else {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            name
        };
        names.insert(name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::EmbeddedSkillCatalog;
    use application::skill_install::{InstallSkills, SkillAction, SkillStub};

    const OLD_STUB: &str = "---\nname: power-dag\ndescription: Previous stub.\n---\n";
    const OLD_HASH: &str = "179d6c05537c0c5ccbc2ca0c9b62412a4e3a3ea9971f95e64cfd76485794b541";

    #[test]
    fn update_refreshes_a_shipped_stub_without_adding_missing_locations() -> Result<()> {
        let home = tempfile::tempdir()?;
        fs::create_dir_all(home.path().join(".claude"))?;
        let targets = targets_at(home.path(), None, true)?;
        let skill = targets.shared.join("power-dag");
        fs::create_dir_all(&skill)?;
        fs::write(skill.join(SKILL_FILE), OLD_STUB)?;
        let inode = skill.metadata()?.ino();

        let stubs = EmbeddedSkillCatalog.stubs()?;
        let shipped = stubs.iter().find(|stub| stub.name == "power-dag").unwrap();
        let outcomes = InstallSkills {
            names: &[],
            update: true,
            dry_run: false,
            force: false,
        }
        .execute(
            &LocalSkillFilesystem,
            vec![SkillStub {
                name: shipped.name.clone(),
                description: shipped.description.clone(),
                markdown: shipped.markdown,
                current_hash: shipped.current_hash.clone(),
                shipped_hashes: &[OLD_HASH],
            }],
            &targets,
        )?;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].action, Some(SkillAction::Write));
        assert_eq!(outcomes[0].error, None);
        assert_eq!(skill.metadata()?.ino(), inode);
        assert_eq!(
            fs::read_to_string(skill.join(SKILL_FILE))?,
            shipped.markdown
        );
        assert!(!home.path().join(CLAUDE_SKILLS).exists());
        Ok(())
    }

    #[test]
    fn update_refuses_to_replace_a_file_that_changed_after_inspection() -> Result<()> {
        let home = tempfile::tempdir()?;
        let skill = home.path().join("power-dag");
        fs::create_dir(&skill)?;
        let file = skill.join(SKILL_FILE);
        fs::write(&file, OLD_STUB)?;
        let (_, snapshot) = LocalSkillFilesystem.observe(&skill, None)?;
        fs::write(&file, "edited after inspection")?;
        assert!(
            LocalSkillFilesystem
                .apply(&skill, snapshot, SkillMutation::Write("next stub"), None)
                .is_err()
        );
        assert_eq!(fs::read_to_string(file)?, "edited after inspection");
        Ok(())
    }

    #[test]
    fn visibility_follows_agent_specific_user_and_project_roots() -> Result<()> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        let codex_home = root.path().join("codex");
        let shared = home.join(SHARED_SKILLS).join("power-dag");
        let claude = home.join(CLAUDE_SKILLS);
        let project_claude = cwd.join(CLAUDE_SKILLS).join("claude-project");
        let project_shared = cwd.join(SHARED_SKILLS).join("codex-project");
        let codex_legacy = codex_home.join("skills/legacy-codex");
        for dir in [&shared, &project_claude, &project_shared, &codex_legacy] {
            fs::create_dir_all(dir)?;
        }
        fs::write(shared.join(SKILL_FILE), OLD_STUB)?;
        fs::write(project_claude.join(SKILL_FILE), "Claude skill")?;
        for (name, dir) in [
            ("codex-project", project_shared),
            ("legacy-codex", codex_legacy),
        ] {
            fs::write(
                dir.join(SKILL_FILE),
                format!("---\nname: {name}\ndescription: Test\n---\n"),
            )?;
        }
        fs::create_dir_all(&claude)?;
        symlink(&shared, claude.join("power-dag"))?;
        symlink(home.join("missing"), claude.join("missing"))?;
        assert_eq!(
            installed_skills_at(&Agent::Coding("claude".into()), &home, &codex_home, &cwd)?,
            BTreeSet::from(["claude-project".into(), "power-dag".into()])
        );
        assert_eq!(
            installed_skills_at(&Agent::Coding("codex".into()), &home, &codex_home, &cwd)?,
            BTreeSet::from([
                "codex-project".into(),
                "legacy-codex".into(),
                "power-dag".into()
            ])
        );
        Ok(())
    }
}
