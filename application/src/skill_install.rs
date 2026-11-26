use anyhow::{Result, bail};
use domain::agent::AgentProfile;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub struct SkillStub {
    pub name: String,
    pub description: String,
    pub markdown: &'static str,
    pub current_hash: String,
    pub shipped_hashes: &'static [&'static str],
}

/// Where an install writes: the shared folder, plus a link folder for each agent that does not
/// read the shared folder natively.
pub struct SkillTargets {
    pub shared: PathBuf,
    pub links: Vec<PathBuf>,
    pub backups: PathBuf,
}

/// The `--agent` value that selects the shared folder alone.
pub const UNIVERSAL_AGENT: &str = "universal";

/// Profiles an install may link for. No request selects every profile; `universal` adds none.
pub fn select_profiles<'a>(
    profiles: &'a [AgentProfile],
    requested: &[String],
) -> Result<Vec<&'a AgentProfile>> {
    if requested.is_empty() {
        return Ok(profiles.iter().collect());
    }
    for id in requested {
        if id != UNIVERSAL_AGENT && !profiles.iter().any(|profile| profile.id.as_str() == id) {
            let known = profiles
                .iter()
                .map(|profile| profile.id.as_str())
                .chain([UNIVERSAL_AGENT])
                .collect::<Vec<_>>()
                .join(", ");
            bail!("Unknown agent {id:?}. Known agents: {known}");
        }
    }
    Ok(profiles
        .iter()
        .filter(|profile| requested.iter().any(|id| id == profile.id.as_str()))
        .collect())
}

pub enum SkillLocation {
    Missing,
    Folder {
        stub: Option<ObservedStub>,
        only_stub: bool,
    },
    SharedLink,
    Other,
}

pub struct ObservedStub {
    pub hash: String,
    pub markdown: String,
}

pub trait SkillFilesystem {
    type Snapshot;

    fn observe(
        &self,
        path: &Path,
        shared: Option<&Path>,
    ) -> Result<(SkillLocation, Self::Snapshot)>;

    fn apply(
        &self,
        path: &Path,
        expected: Self::Snapshot,
        mutation: SkillMutation<'_>,
        backup_root: Option<&Path>,
    ) -> Result<Option<PathBuf>>;
}

pub enum SkillMutation<'a> {
    Write(&'a str),
    Link { target: &'a Path, hash: &'a str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillAction {
    NoChange,
    Write,
    Link,
    LeftAlone,
}

impl SkillAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoChange => "no-change",
            Self::Write => "write",
            Self::Link => "link",
            Self::LeftAlone => "left-alone",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaveReason {
    Changed,
    Foreign,
    SharedUnavailable,
}

impl LeaveReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::Foreign => "foreign",
            Self::SharedUnavailable => "shared-unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Decision {
    action: SkillAction,
    reason: Option<LeaveReason>,
    backup: bool,
}

impl Decision {
    fn action(action: SkillAction) -> Self {
        Self {
            action,
            reason: None,
            backup: false,
        }
    }

    fn protected(reason: LeaveReason, replacement: Option<SkillAction>) -> Self {
        Self {
            action: replacement.unwrap_or(SkillAction::LeftAlone),
            reason: Some(reason),
            backup: replacement.is_some(),
        }
    }
}

fn plan_location(
    stub: &SkillStub,
    location: &SkillLocation,
    shared_available: Option<bool>,
    options: &InstallSkills<'_>,
) -> Option<Decision> {
    use SkillAction::*;

    let replacement = match shared_available {
        None => Some(Write),
        Some(true) => Some(Link),
        Some(false) => None,
    };
    let unavailable = || Decision::protected(LeaveReason::SharedUnavailable, None);
    // An existing agent copy can be replaced in place when update cannot add the shared copy.
    let force_replacement = options.force.then_some(replacement.unwrap_or(Write));
    Some(match location {
        SkillLocation::Missing if options.update => return None,
        SkillLocation::Missing => replacement.map_or_else(unavailable, Decision::action),
        SkillLocation::SharedLink => match shared_available {
            Some(true) => Decision::action(NoChange),
            _ => unavailable(),
        },
        SkillLocation::Folder {
            stub: Some(found),
            only_stub,
        } if *only_stub && found.hash == stub.current_hash => Decision::action(NoChange),
        SkillLocation::Folder {
            stub: Some(found),
            only_stub,
        } if *only_stub && stub.shipped_hashes.contains(&found.hash.as_str()) => {
            // Update existing standalone copies in place, even when the shared copy is absent.
            Decision::action(Write)
        }
        SkillLocation::Folder {
            stub: Some(found), ..
        } if stub.recognizes(found) => Decision::protected(LeaveReason::Changed, force_replacement),
        _ => Decision::protected(LeaveReason::Foreign, force_replacement),
    })
}

impl SkillStub {
    fn recognizes(&self, found: &ObservedStub) -> bool {
        if found.hash == self.current_hash || self.shipped_hashes.contains(&found.hash.as_str()) {
            return true;
        }
        let loader = format!("skills get {}`", self.name);
        if found.markdown.contains(&loader) {
            return true;
        }
        let mut lines = found.markdown.lines();
        let Some("---") = lines.next() else {
            return false;
        };
        let header: Vec<_> = lines.by_ref().take_while(|line| *line != "---").collect();
        let field_matches = |key, value: &str| {
            header.iter().any(|line| {
                line.split_once(':')
                    .is_some_and(|(field, text)| field == key && text.trim() == value)
            })
        };
        // A package can reuse the name, so also require Power's description.
        field_matches("name", &self.name) && field_matches("description", &self.description)
    }
}

pub struct InstallSkills<'a> {
    pub names: &'a [String],
    pub update: bool,
    pub dry_run: bool,
    pub force: bool,
}

pub struct SkillOutcome {
    pub name: String,
    pub path: PathBuf,
    pub action: Option<SkillAction>,
    pub reason: Option<LeaveReason>,
    pub needs_backup: bool,
    pub backup: Option<PathBuf>,
    pub error: Option<String>,
}

impl InstallSkills<'_> {
    pub fn execute<F: SkillFilesystem>(
        &self,
        files: &F,
        stubs: Vec<SkillStub>,
        targets: &SkillTargets,
    ) -> Result<Vec<SkillOutcome>> {
        let stubs: BTreeMap<_, _> = stubs
            .into_iter()
            .map(|stub| (stub.name.clone(), stub))
            .collect();
        let names: BTreeSet<_> = if self.names.is_empty() {
            stubs.keys().collect()
        } else {
            self.names.iter().collect()
        };
        for name in &names {
            if !stubs.contains_key(*name) {
                let available = stubs.keys().cloned().collect::<Vec<_>>().join(", ");
                bail!("Unknown skill {name:?}. Available skills: {available}");
            }
        }

        let mut outcomes = Vec::new();
        for name in names {
            let stub = &stubs[name];
            let shared = targets.shared.join(name);
            let shared_outcome = self.run_location(files, stub, targets, &shared, None);
            let shared_available = shared_outcome.as_ref().is_some_and(|outcome| {
                outcome.error.is_none()
                    && matches!(
                        outcome.action,
                        Some(SkillAction::NoChange | SkillAction::Write)
                    )
            });
            outcomes.extend(shared_outcome);
            for links in &targets.links {
                outcomes.extend(self.run_location(
                    files,
                    stub,
                    targets,
                    &links.join(name),
                    Some((&shared, shared_available)),
                ));
            }
        }
        Ok(outcomes)
    }

    fn run_location<F: SkillFilesystem>(
        &self,
        files: &F,
        stub: &SkillStub,
        targets: &SkillTargets,
        path: &Path,
        shared: Option<(&Path, bool)>,
    ) -> Option<SkillOutcome> {
        let mut outcome = SkillOutcome {
            name: stub.name.clone(),
            path: path.to_owned(),
            action: None,
            reason: None,
            needs_backup: false,
            backup: None,
            error: None,
        };
        let (location, snapshot) = match files.observe(path, shared.map(|(path, _)| path)) {
            Ok(observed) => observed,
            Err(error) => {
                outcome.error = Some(format!("{error:#}"));
                return Some(outcome);
            }
        };
        let decision = plan_location(
            stub,
            &location,
            shared.map(|(_, available)| available),
            self,
        )?;
        outcome.action = Some(decision.action);
        outcome.reason = decision.reason;
        outcome.needs_backup = decision.backup;
        if self.dry_run {
            return Some(outcome);
        }

        let mutation = match decision.action {
            SkillAction::Write => SkillMutation::Write(stub.markdown),
            SkillAction::Link => SkillMutation::Link {
                target: shared.expect("link has a shared target").0,
                hash: &stub.current_hash,
            },
            _ => return Some(outcome),
        };
        let backup_root = decision.backup.then_some(targets.backups.as_path());
        match files.apply(path, snapshot, mutation, backup_root) {
            Ok(backup) => outcome.backup = backup,
            Err(error) => outcome.error = Some(format!("{error:#}")),
        }
        Some(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::agent::Injection;

    fn profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.into(),
            command: id.into(),
            injection: Injection::None,
            skip_subcommands: Vec::new(),
            skip_if_args_contain: Vec::new(),
            model_callback: domain::agent::ModelCallback::None,
            skills_dir: None,
            resume: None,
            env: Vec::new(),
            conversation_env: None,
            server_socket_env: None,
        }
    }

    #[test]
    fn agent_selection_uses_profile_ids_and_universal() -> Result<()> {
        let profiles = [profile("claude"), profile("grok")];
        let ids = |selected: Vec<&AgentProfile>| -> Vec<String> {
            selected
                .into_iter()
                .map(|profile| profile.id.as_str().to_owned())
                .collect()
        };
        assert_eq!(ids(select_profiles(&profiles, &[])?), ["claude", "grok"]);
        assert_eq!(ids(select_profiles(&profiles, &["grok".into()])?), ["grok"]);
        assert!(select_profiles(&profiles, &["universal".into()])?.is_empty());
        assert!(select_profiles(&profiles, &["cursor".into()]).is_err());
        Ok(())
    }

    #[test]
    fn planner_requires_an_exact_hash_and_a_single_file() {
        let stub = SkillStub {
            name: "power-test".into(),
            description: "Test".into(),
            markdown: "",
            current_hash: "current".into(),
            shipped_hashes: &["old"],
        };
        let options = InstallSkills {
            names: &[],
            update: false,
            force: false,
            dry_run: true,
        };
        for (hash, only_stub, action, reason) in [
            ("current", true, SkillAction::NoChange, None),
            ("old", true, SkillAction::Write, None),
            (
                "current",
                false,
                SkillAction::LeftAlone,
                Some(LeaveReason::Changed),
            ),
            (
                "unknown",
                true,
                SkillAction::LeftAlone,
                Some(LeaveReason::Foreign),
            ),
        ] {
            let found = SkillLocation::Folder {
                stub: Some(ObservedStub {
                    hash: hash.into(),
                    markdown: String::new(),
                }),
                only_stub,
            };
            let decision = plan_location(&stub, &found, None, &options).unwrap();
            assert_eq!(decision.action, action);
            assert_eq!(decision.reason, reason);
            assert!(!decision.backup);
        }
    }

    #[test]
    fn planner_never_adds_an_empty_location_during_update() {
        let stub = SkillStub {
            name: "power-test".into(),
            description: "Test".into(),
            markdown: "",
            current_hash: "current".into(),
            shipped_hashes: &[],
        };
        let options = InstallSkills {
            names: &[],
            update: true,
            force: true,
            dry_run: true,
        };
        assert_eq!(
            plan_location(&stub, &SkillLocation::Missing, None, &options),
            None
        );
        assert_eq!(
            plan_location(&stub, &SkillLocation::Missing, Some(true), &options),
            None
        );
        assert_eq!(
            plan_location(&stub, &SkillLocation::Other, Some(true), &options),
            Some(Decision {
                action: SkillAction::Link,
                reason: Some(LeaveReason::Foreign),
                backup: true
            })
        );
    }
}
