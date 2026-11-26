use application::agents::AgentProfileRegistry;
use domain::agent::{AgentId, AgentProfile, Injection, ModelCallback, Resume};

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
                ModelCallback::ClaudeStatusLine,
                "~/.claude/skills",
                resume(
                    &["--session-id", "{id}"],
                    &["--resume", "{id}"],
                    &["--continue"],
                    &[
                        "--session-id",
                        "--resume",
                        "-r",
                        "--continue",
                        "-c",
                        "--fork-session",
                        "--from-pr",
                        "--teleport",
                        "-p",
                        "--print",
                    ],
                ),
            ),
            builtin(
                "codex",
                Injection::ConfigValue {
                    flag: "-c".into(),
                    key: "developer_instructions".into(),
                },
                &[],
                &["developer_instructions"],
                // Codex hooks from any source, `-c` included, run only after the user
                // reviews and trusts them in `/hooks`, so Power cannot register one per launch.
                ModelCallback::None,
                // Codex reads the shared folder natively.
                "~/.agents/skills",
                resume(&[], &[], &["resume", "--last"], &["resume", "fork"]),
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
                // The `GROK_CONFIG` overlay drops `[ui]`, so `ui.status_line` can only be
                // set in the user's config file.
                ModelCallback::None,
                // Grok reads ~/.agents/skills (as well as ~/.grok and ~/.claude) natively.
                "~/.agents/skills",
                resume(
                    &["--session-id", "{id}"],
                    &["--resume", "{id}"],
                    &["--continue"],
                    &[
                        "--session-id",
                        "-s",
                        "--resume",
                        "-r",
                        "--continue",
                        "-c",
                        "--fork-session",
                    ],
                ),
            ),
        ])
    }
}

fn builtin(
    id: &str,
    injection: Injection,
    skip_subcommands: &[&str],
    skip_if_args_contain: &[&str],
    model_callback: ModelCallback,
    skills_dir: &str,
    resume: Resume,
) -> AgentProfile {
    AgentProfile {
        id: AgentId::from(id),
        command: id.into(),
        injection,
        skip_subcommands: skip_subcommands.iter().map(|&arg| arg.into()).collect(),
        skip_if_args_contain: skip_if_args_contain.iter().map(|&arg| arg.into()).collect(),
        model_callback,
        skills_dir: Some(skills_dir.into()),
        resume: Some(resume),
    }
}

fn resume(assign: &[&str], resume: &[&str], resume_last: &[&str], conflicts: &[&str]) -> Resume {
    let strings = |args: &[&str]| args.iter().map(|&arg| arg.into()).collect();
    Resume {
        assign: strings(assign),
        resume: strings(resume),
        resume_last: strings(resume_last),
        conflicts: strings(conflicts),
    }
}
