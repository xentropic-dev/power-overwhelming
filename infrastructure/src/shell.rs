//! Shell integration: agent wrappers that interactive shells in Power terminals pick up,
//! and the one-line rc hook that onboarding installs.

use crate::{skill_install::LocalInstalledSkills, skills::EmbeddedSkillCatalog};
use anyhow::{Context, Result, bail};
use application::{
    agents::AgentProfileRegistry,
    skills::{InstalledSkills, LaunchSkills},
};
use domain::agent::{AgentProfile, Injection};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
};

const MARKER: &str = "# Power Overwhelming shell integration";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    pub fn parse(name: &str) -> Option<Self> {
        match Path::new(name).file_name()?.to_str()? {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    /// The user's login shell, from `$SHELL`.
    pub fn detect() -> Option<Self> {
        Self::parse(&std::env::var("SHELL").ok()?)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }

    /// The file an interactive, non-login shell reads, which is how Power starts shells.
    pub fn rc_file(self) -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        Some(match self {
            Self::Bash => home.join(".bashrc"),
            Self::Zsh => std::env::var_os("ZDOTDIR")
                .map(PathBuf::from)
                .unwrap_or(home)
                .join(".zshrc"),
            Self::Fish => std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("fish/config.fish"),
        })
    }

    /// The hook line onboarding appends. It does nothing outside Power terminals.
    pub fn hook(self) -> &'static str {
        match self {
            Self::Bash => r#"[ -n "$POWER_BIN" ] && eval "$("$POWER_BIN" shell-init bash)""#,
            Self::Zsh => r#"[ -n "$POWER_BIN" ] && eval "$("$POWER_BIN" shell-init zsh)""#,
            Self::Fish => r#"test -n "$POWER_BIN"; and "$POWER_BIN" shell-init fish | source"#,
        }
    }

    /// Shell functions that start configured agents with Power's instructions.
    pub fn init_script(self, registry: &impl AgentProfileRegistry) -> String {
        registry.profiles().iter().map(|profile| {
            let name = profile.id.as_str();
            match self {
                Self::Bash | Self::Zsh => {
                    // `function name` keeps a user's alias from expanding in the definition.
                    format!("function {name} {{ \"$POWER_BIN\" exec-agent {name} -- \"$@\"; }}\n")
                }
                Self::Fish => format!(
                    "function {name} --wraps {name}\n    \"$POWER_BIN\" exec-agent {name} -- $argv\nend\n"
                ),
            }
        }).collect()
    }

    pub fn installed(self) -> bool {
        self.rc_file().is_some_and(|path| has_marker(&path))
    }

    /// Appends the hook to the rc file once and returns the file's path.
    pub fn install(self) -> Result<PathBuf> {
        let path = self
            .rc_file()
            .context("Set HOME to install shell integration")?;
        self.install_at(&path)?;
        Ok(path)
    }

    fn install_at(self, path: &Path) -> Result<()> {
        if has_marker(path) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existing = fs::read_to_string(path).unwrap_or_default();
        let separator = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Open {}", path.display()))?;
        write!(file, "{separator}\n{MARKER}\n{}\n", self.hook())?;
        Ok(())
    }
}

fn has_marker(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|contents| contents.contains(MARKER))
}

/// Arguments Power adds in front of the user's arguments when it starts an agent.
/// Skills the agent already discovers natively are left out of the list.
pub(crate) fn instruction_args(
    profile: &AgentProfile,
    user: &[OsString],
    installed: &dyn InstalledSkills,
) -> Result<Vec<OsString>> {
    if profile.injection == Injection::None
        || user
            .first()
            .and_then(|arg| arg.to_str())
            .is_some_and(|arg| profile.skip_subcommands.iter().any(|skip| skip == arg))
        || user.iter().any(|arg| {
            let arg = arg.to_string_lossy();
            profile
                .skip_if_args_contain
                .iter()
                .any(|skip| arg.contains(skip))
        })
    {
        return Ok(Vec::new());
    }

    let mut instructions = String::from("You are running inside Power Overwhelming.");
    let skills = LaunchSkills::execute(&EmbeddedSkillCatalog, installed, profile)?;
    if !skills.is_empty() {
        instructions.push_str(" Power skills, load with `power skills get <name>` when relevant:");
    }
    instructions.push('\n');
    for skill in skills {
        instructions.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    instructions.push_str(concat!(
        "\nWhen you understand the user's objective, run ",
        "`power session task 'Short task label'` from this terminal. ",
        "Use a short, concrete label and update it when the objective changes.\n\n",
        "To give this box a custom icon, run `power box icon <image-path>`.",
    ));
    Ok(match &profile.injection {
        Injection::Flag { flag } => vec![flag.into(), instructions.into()],
        Injection::ConfigValue { flag, key } => vec![
            flag.into(),
            format!("{key}={}", serde_json::to_string(&instructions)?).into(),
        ],
        Injection::None => Vec::new(),
    })
}

/// Replaces this process with the real agent binary, adding Power's instructions.
pub fn exec_agent(profile: &AgentProfile, args: Vec<OsString>) -> Result<()> {
    let program = find_program(&profile.command)
        .with_context(|| format!("{}: command not found", profile.command))?;
    let mut command = std::process::Command::new(&program);
    command
        .args(instruction_args(
            profile,
            &args,
            &LocalInstalledSkills(&std::env::current_dir()?),
        )?)
        .args(&args);
    let error = command.exec();
    bail!("Start {}. {error}", program.display())
}

/// Finds `name` on PATH, skipping the Power binary so a wrapper cannot call itself.
fn find_program(name: &str) -> Option<PathBuf> {
    let current = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let is_program =
        |path: &Path| is_executable(path) && path.canonicalize().ok().as_ref() != current.as_ref();
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_program(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| is_program(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Whether an agent's binary is on PATH, for onboarding to report.
pub fn agent_available(profile: &AgentProfile) -> bool {
    find_program(&profile.command).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentProfiles;
    use application::skills::ListSkills;
    use std::collections::BTreeSet;

    struct Installed(BTreeSet<String>);

    impl InstalledSkills for Installed {
        fn installed(&self, _: &AgentProfile) -> Result<BTreeSet<String>> {
            Ok(self.0.clone())
        }
    }

    const NONE: Installed = Installed(BTreeSet::new());

    const TASK_LINE: &str = concat!(
        "When you understand the user's objective, run ",
        "`power session task 'Short task label'` from this terminal. ",
        "Use a short, concrete label and update it when the objective changes.",
    );

    const ICON_LINE: &str = "To give this box a custom icon, run `power box icon <image-path>`.";

    fn launch_text(installed: &Installed) -> String {
        let profiles = AgentProfiles::default();
        let args = instruction_args(
            profiles.resolve(&"claude".into()).unwrap(),
            &["fix the bug".into()],
            installed,
        )
        .unwrap();
        args[1].to_str().unwrap().to_owned()
    }

    #[test]
    fn launch_instructions_omit_natively_installed_skills() {
        let text = launch_text(&Installed(BTreeSet::from(["power-dag".into()])));
        assert!(text.contains("- power-artifacts: "));
        assert!(!text.contains("- power-dag: "));

        let all = ListSkills::execute(&EmbeddedSkillCatalog).unwrap();
        let text = launch_text(&Installed(
            all.into_iter().map(|skill| skill.name).collect(),
        ));
        assert_eq!(
            text,
            format!("You are running inside Power Overwhelming.\n\n{TASK_LINE}\n\n{ICON_LINE}")
        );
    }

    #[test]
    fn skips_instructions_for_management_commands() {
        let profiles = AgentProfiles::default();
        assert!(
            instruction_args(
                profiles.resolve(&"claude".into()).unwrap(),
                &["mcp".into(), "list".into()],
                &NONE
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            instruction_args(
                profiles.resolve(&"codex".into()).unwrap(),
                &["-c".into(), "developer_instructions=\"x\"".into()],
                &NONE
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn respects_no_injection_and_existing_flag_values() -> Result<()> {
        let profiles = AgentProfiles::default();
        let grok = profiles.resolve(&"grok".into())?;
        assert_eq!(instruction_args(grok, &[], &NONE)?[0], "--rules");
        assert!(instruction_args(grok, &["--rules=custom".into()], &NONE)?.is_empty());
        assert!(instruction_args(grok, &["mcp".into(), "list".into()], &NONE)?.is_empty());

        let mut profile = grok.clone();
        profile.injection = Injection::None;
        assert!(instruction_args(&profile, &["prompt".into()], &NONE)?.is_empty());
        Ok(())
    }

    #[test]
    fn launch_instructions_list_every_catalog_skill() {
        let profiles = AgentProfiles::default();
        let claude = instruction_args(
            profiles.resolve(&"claude".into()).unwrap(),
            &["fix the bug".into()],
            &NONE,
        )
        .unwrap();
        assert_eq!(claude[0], "--append-system-prompt");
        let instructions = claude[1].to_str().unwrap();
        let mut lines = instructions.lines();
        assert_eq!(
            lines.next(),
            Some(
                "You are running inside Power Overwhelming. Power skills, load with `power skills get <name>` when relevant:"
            )
        );
        for skill in ListSkills::execute(&EmbeddedSkillCatalog).unwrap() {
            let entry = format!("- {}: {}", skill.name, skill.description);
            assert_eq!(lines.next(), Some(entry.as_str()));
        }
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some(TASK_LINE));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some(ICON_LINE));
        assert_eq!(lines.next(), None);

        let codex = instruction_args(
            profiles.resolve(&"codex".into()).unwrap(),
            &["fix the bug".into()],
            &NONE,
        )
        .unwrap();
        assert_eq!(codex[0], "-c");
        let encoded = codex[1]
            .to_str()
            .unwrap()
            .strip_prefix("developer_instructions=")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<String>(encoded).unwrap(),
            instructions
        );
    }

    #[test]
    fn installs_the_hook_once() {
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".bashrc");
        fs::write(&rc, "export A=1").unwrap();
        Shell::Bash.install_at(&rc).unwrap();
        Shell::Bash.install_at(&rc).unwrap();
        let contents = fs::read_to_string(&rc).unwrap();
        assert_eq!(contents.matches(MARKER).count(), 1);
        assert!(contents.starts_with("export A=1\n\n"));
        assert!(has_marker(&rc));
    }
}
