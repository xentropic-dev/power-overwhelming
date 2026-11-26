use crate::{
    skills::{parse_frontmatter, stub_hash},
    workspace::Checkout,
};
use anyhow::{Context, Result, ensure};
use application::{
    skill_install::{ObservedStub, SkillFilesystem, SkillLocation, SkillMutation, SkillTargets},
    skills::InstalledSkills,
};
use domain::agent::AgentProfile;
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
const SKILL_FILE: &str = "SKILL.md";
const MAX_STUB_BYTES: u64 = 64 * 1024;

fn home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is required to locate user skills")?;
    Ok(std::path::absolute(home)?)
}

/// Expand a leading `~` in a configured skill folder.
pub fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_owned(),
    }
}

/// A profile's user skill folder, and the same folder relative to a project when it lives
/// under the home directory.
fn skill_roots(profile: &AgentProfile, home: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let user = expand_home(profile.skills_dir.as_deref()?, home);
    let relative = user
        .strip_prefix(home)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(Path::to_owned);
    Some((user, relative))
}

pub fn install_targets(local: bool, profiles: &[&AgentProfile]) -> Result<SkillTargets> {
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
    targets_at(&home, project.as_deref(), profiles)
}

fn targets_at(
    home: &Path,
    project: Option<&Path>,
    profiles: &[&AgentProfile],
) -> Result<SkillTargets> {
    let scope = project.unwrap_or(home);
    let shared = scope.join(SHARED_SKILLS);
    if let Some(project) = project {
        ensure!(
            resolve_root(&shared)?.starts_with(project),
            "{} resolves outside the worktree",
            shared.display()
        );
    }
    let resolved_shared = resolve_root(&shared).ok();

    let mut links: Vec<PathBuf> = Vec::new();
    let mut seen = BTreeSet::new();
    for profile in profiles {
        let Some((user, relative)) = skill_roots(profile, home) else {
            continue;
        };
        // Agents that read the shared folder natively need no link.
        if relative.as_deref() == Some(Path::new(SHARED_SKILLS)) {
            continue;
        }
        let (root, detected) = match project {
            Some(project) => {
                let Some(relative) = relative else { continue };
                let root = project.join(relative);
                let detected = root.is_dir();
                (root, detected)
            }
            None => {
                let detected = user.parent().is_some_and(Path::is_dir);
                (user, detected)
            }
        };
        if !detected {
            continue;
        }
        let resolved = resolve_root(&root);
        let in_scope = project.is_none_or(|project| {
            resolved
                .as_ref()
                .is_ok_and(|root| root.starts_with(project))
        });
        let distinct = match (&resolved_shared, &resolved) {
            (Some(shared), Ok(root)) => shared != root,
            _ => true, // Let observation report errors for these locations.
        };
        let key = resolved.unwrap_or_else(|_| root.clone());
        if in_scope && distinct && seen.insert(key) {
            links.push(root);
        }
    }
    Ok(SkillTargets {
        shared,
        links,
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

/// Discovers the skills a profile's agent loads from its user and project folders, as seen
/// from the agent's launch directory.
pub struct LocalInstalledSkills<'a>(pub &'a Path);

impl InstalledSkills for LocalInstalledSkills<'_> {
    fn installed(&self, profile: &AgentProfile) -> Result<BTreeSet<String>> {
        installed_skills_in(profile, self.0)
    }
}

/// Discover user and project skills as seen from an agent's launch directory.
pub fn installed_skills_in(profile: &AgentProfile, cwd: &Path) -> Result<BTreeSet<String>> {
    installed_skills_at(profile, &home()?, cwd)
}

fn installed_skills_at(
    profile: &AgentProfile,
    home: &Path,
    cwd: &Path,
) -> Result<BTreeSet<String>> {
    let Some((user, relative)) = skill_roots(profile, home) else {
        return Ok(BTreeSet::new());
    };
    let mut names = BTreeSet::new();
    scan_skills(&user, &mut names)?;
    if let Some(relative) = relative {
        let cwd = cwd.canonicalize()?;
        let root =
            Checkout::discover(&cwd).map_or_else(|_| cwd.clone(), |checkout| checkout.worktree);
        for directory in cwd
            .ancestors()
            .take_while(|directory| directory.starts_with(&root))
        {
            scan_skills(&directory.join(&relative), &mut names)?;
        }
    }
    Ok(names)
}

/// Collect skill names in a folder, preferring the frontmatter name over the folder name.
fn scan_skills(directory: &Path, names: &mut BTreeSet<String>) -> Result<()> {
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
        let mut markdown = String::new();
        let frontmatter = File::open(path)?
            .take(MAX_STUB_BYTES)
            .read_to_string(&mut markdown)
            .ok()
            .and_then(|_| parse_frontmatter(&markdown).ok())
            .map(|summary| summary.name.trim_matches(['\'', '"']).to_owned())
            .filter(|name| !name.is_empty());
        if let Some(name) = frontmatter.or_else(|| entry.file_name().into_string().ok()) {
            names.insert(name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agents::AgentProfiles, skills::EmbeddedSkillCatalog};
    use application::{
        agents::AgentProfileRegistry,
        skill_install::{InstallSkills, SkillAction, SkillStub},
    };

    const CLAUDE_SKILLS: &str = ".claude/skills";

    fn builtin(id: &str) -> AgentProfile {
        AgentProfiles::default()
            .resolve(&id.into())
            .unwrap()
            .clone()
    }

    #[test]
    fn links_only_detected_agents_that_do_not_read_the_shared_folder() -> Result<()> {
        let home = tempfile::tempdir()?;
        let profiles = AgentProfiles::default();
        let mut custom = builtin("claude");
        custom.id = "custom".into();
        custom.skills_dir = Some("~/.custom/skills".into());
        let mut all: Vec<_> = profiles.profiles().iter().collect();
        all.push(&custom);

        assert!(targets_at(home.path(), None, &all)?.links.is_empty());
        fs::create_dir(home.path().join(".claude"))?;
        fs::create_dir(home.path().join(".custom"))?;
        let targets = targets_at(home.path(), None, &all)?;
        assert_eq!(
            targets.links,
            [
                home.path().join(CLAUDE_SKILLS),
                home.path().join(".custom/skills")
            ]
        );
        assert!(
            targets_at(home.path(), None, &[&custom, &custom])?
                .links
                .len()
                == 1
        );

        let project = tempfile::tempdir()?;
        assert!(
            targets_at(home.path(), Some(project.path()), &all)?
                .links
                .is_empty()
        );
        fs::create_dir_all(project.path().join(CLAUDE_SKILLS))?;
        assert_eq!(
            targets_at(home.path(), Some(project.path()), &all)?.links,
            [project.path().join(CLAUDE_SKILLS)]
        );
        Ok(())
    }

    const OLD_STUB: &str = "---\nname: power-dag\ndescription: Previous stub.\n---\n";
    const OLD_HASH: &str = "179d6c05537c0c5ccbc2ca0c9b62412a4e3a3ea9971f95e64cfd76485794b541";

    #[test]
    fn update_refreshes_a_shipped_stub_without_adding_missing_locations() -> Result<()> {
        let home = tempfile::tempdir()?;
        fs::create_dir_all(home.path().join(".claude"))?;
        let claude = builtin("claude");
        let targets = targets_at(home.path(), None, &[&claude])?;
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
    fn visibility_follows_profile_user_and_project_roots() -> Result<()> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        let shared = home.join(SHARED_SKILLS).join("power-dag");
        let claude = home.join(CLAUDE_SKILLS);
        let project_claude = cwd.join(CLAUDE_SKILLS).join("claude-project");
        let project_shared = cwd.join(SHARED_SKILLS).join("codex-folder");
        for dir in [&shared, &project_claude, &project_shared] {
            fs::create_dir_all(dir)?;
        }
        fs::write(shared.join(SKILL_FILE), OLD_STUB)?;
        fs::write(project_claude.join(SKILL_FILE), "Claude skill")?;
        fs::write(
            project_shared.join(SKILL_FILE),
            "---\nname: codex-project\ndescription: Test\n---\n",
        )?;
        fs::create_dir_all(&claude)?;
        symlink(&shared, claude.join("power-dag"))?;
        symlink(home.join("missing"), claude.join("missing"))?;
        assert_eq!(
            installed_skills_at(&builtin("claude"), &home, &cwd)?,
            BTreeSet::from(["claude-project".into(), "power-dag".into()])
        );
        let expected = BTreeSet::from(["codex-project".into(), "power-dag".into()]);
        assert_eq!(
            installed_skills_at(&builtin("codex"), &home, &cwd)?,
            expected
        );
        assert_eq!(
            installed_skills_at(&builtin("grok"), &home, &cwd)?,
            expected
        );

        let mut custom = builtin("claude");
        custom.skills_dir = None;
        assert!(installed_skills_at(&custom, &home, &cwd)?.is_empty());
        custom.skills_dir = Some(root.path().join("elsewhere"));
        assert!(installed_skills_at(&custom, &home, &cwd)?.is_empty());
        Ok(())
    }
}
