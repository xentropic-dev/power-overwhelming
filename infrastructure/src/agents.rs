use application::agents::AgentProfileRegistry;
use domain::agent::{AgentId, AgentProfile, Injection};

#[derive(Clone)]
pub struct AgentProfiles(pub(crate) Vec<AgentProfile>);

impl AgentProfileRegistry for AgentProfiles {
    fn profiles(&self) -> &[AgentProfile] {
        &self.0
    }
}

impl Default for AgentProfiles {
    fn default() -> Self {
        Self(vec![
            builtin(
                "claude",
                Injection::Flag {
                    flag: "--append-system-prompt".into(),
                },
                &[
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
                ],
                &["--append-system-prompt"],
                "~/.claude/skills",
            ),
            builtin(
                "codex",
                Injection::ConfigValue {
                    flag: "-c".into(),
                    key: "developer_instructions".into(),
                },
                &[],
                &["developer_instructions"],
                // Codex reads the shared folder natively.
                "~/.agents/skills",
            ),
            builtin(
                "grok",
                Injection::Flag {
                    flag: "--rules".into(),
                },
                &[
                    "agent",
                    "clone",
                    "completions",
                    "dashboard",
                    "doctor",
                    "du",
                    "disk-usage",
                    "export",
                    "help",
                    "inspect",
                    "leader",
                    "login",
                    "logout",
                    "mcp",
                    "memory",
                    "models",
                    "plugin",
                    "sessions",
                    "setup",
                    "trace",
                    "update",
                    "usage",
                    "version",
                    "v",
                    "worker",
                    "worktree",
                    "wrap",
                ],
                &["--rules"],
                // Grok reads ~/.agents/skills (as well as ~/.grok and ~/.claude) natively.
                "~/.agents/skills",
            ),
        ])
    }
}

fn builtin(
    id: &str,
    injection: Injection,
    skip_subcommands: &[&str],
    skip_if_args_contain: &[&str],
    skills_dir: &str,
) -> AgentProfile {
    AgentProfile {
        id: AgentId::from(id),
        command: id.into(),
        injection,
        skip_subcommands: skip_subcommands.iter().map(|&arg| arg.into()).collect(),
        skip_if_args_contain: skip_if_args_contain.iter().map(|&arg| arg.into()).collect(),
        skills_dir: Some(skills_dir.into()),
    }
}
