//! Shell integration: agent wrappers that interactive shells in Power terminals pick up,
//! and the one-line rc hook that onboarding installs.

use crate::skills::ARTIFACTS_GUIDE;
use anyhow::{Context, Result, bail};
use domain::session::Agent;
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
};

const MARKER: &str = "# Power Overwhelming shell integration";
const WRAPPED: [Agent; 2] = [Agent::Claude, Agent::Codex];

/// Claude subcommands that manage the installation instead of starting a conversation.
const CLAUDE_SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "config",
    "doctor",
    "gateway",
    "import",
    "install",
    "kill",
    "logs",
    "mcp",
    "migrate-installer",
    "plugin",
    "plugins",
    "purge",
    "respawn",
    "rm",
    "setup-token",
    "stop",
    "ultrareview",
    "update",
    "upgrade",
];

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

    /// Shell code that wraps `claude` and `codex` so they receive Power's instructions.
    pub fn init_script(self) -> String {
        WRAPPED
            .iter()
            .map(|agent| {
                let name = agent.command();
                match self {
                    Self::Bash | Self::Zsh => {
                        // `function name` keeps a user's alias from expanding in the definition.
                        format!("function {name} {{ \"$POWER_BIN\" agent {name} \"$@\"; }}\n")
                    }
                    Self::Fish => format!(
                        "function {name} --wraps {name}\n    \"$POWER_BIN\" agent {name} $argv\nend\n"
                    ),
                }
            })
            .collect()
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
pub(crate) fn instruction_args(agent: Agent, user: &[OsString]) -> Result<Vec<OsString>> {
    let artifact_instructions = ARTIFACTS_GUIDE.trim();
    let instructions = format!(
        "{artifact_instructions}\n\n{}",
        concat!(
            "When you understand the user's objective, run ",
            "`power session task 'Short task label'` from this terminal. ",
            "Use a short, concrete label and update it when the objective changes.\n\n",
            "Power can coordinate a DAG of background workers. ",
            "Run `power dag guide` to load the full guide before coordinating. ",
            "If `POWER_TASK` is set, you are a DAG worker and must report with `power dag report`.",
        )
    );
    let has = |needle: &str| {
        user.iter()
            .any(|arg| arg.to_string_lossy().contains(needle))
    };
    Ok(match agent {
        Agent::Shell => Vec::new(),
        Agent::Claude => {
            let subcommand = user
                .first()
                .and_then(|arg| arg.to_str())
                .is_some_and(|arg| CLAUDE_SUBCOMMANDS.contains(&arg));
            if subcommand || has("--append-system-prompt") {
                Vec::new()
            } else {
                vec!["--append-system-prompt".into(), instructions.into()]
            }
        }
        Agent::Codex => {
            if has("developer_instructions") {
                Vec::new()
            } else {
                vec![
                    "-c".into(),
                    format!(
                        "developer_instructions={}",
                        serde_json::to_string(&instructions)?
                    )
                    .into(),
                ]
            }
        }
    })
}

/// Replaces this process with the real agent binary, adding Power's instructions.
pub fn exec_agent(agent: Agent, args: Vec<OsString>) -> Result<()> {
    let program = find_program(agent.command())
        .with_context(|| format!("{}: command not found", agent.command()))?;
    let mut command = std::process::Command::new(&program);
    command.args(instruction_args(agent, &args)?).args(&args);
    let error = command.exec();
    bail!("Start {}. {error}", program.display())
}

/// Finds `name` on PATH, skipping the Power binary so a wrapper cannot call itself.
fn find_program(name: &str) -> Option<PathBuf> {
    let current = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            is_executable(candidate) && candidate.canonicalize().ok().as_ref() != current.as_ref()
        })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Whether an agent's binary is on PATH, for onboarding to report.
pub fn agent_available(agent: Agent) -> bool {
    find_program(agent.command()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_instructions_for_management_commands() {
        assert!(
            instruction_args(Agent::Claude, &["mcp".into(), "list".into()])
                .unwrap()
                .is_empty()
        );
        let claude = instruction_args(Agent::Claude, &["fix the bug".into()]).unwrap();
        assert_eq!(claude[0], "--append-system-prompt");
        let instructions = claude[1].to_str().unwrap();
        assert!(instructions.starts_with(ARTIFACTS_GUIDE.trim()));
        assert!(instructions.contains("run `power session task 'Short task label'`"));
        assert!(instructions.contains("Run `power dag guide`"));
        assert!(instructions.contains("must report with `power dag report`"));
        assert!(
            instruction_args(
                Agent::Codex,
                &["-c".into(), "developer_instructions=\"x\"".into()]
            )
            .unwrap()
            .is_empty()
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
