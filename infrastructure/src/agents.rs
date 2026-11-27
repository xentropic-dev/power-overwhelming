use application::agents::AgentProfileRegistry;
use domain::agent::{AgentId, AgentProfile, Injection, ModelCallback, Resume, TurnHook};

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
                (ModelCallback::ClaudeStatusLine, TurnHook::ClaudeSettings),
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
                (ModelCallback::None, TurnHook::CodexNotify),
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
                (ModelCallback::None, TurnHook::None),
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
            jcode(),
        ])
    }
}

/// jcode takes no instructions flag, no prompt argument and no session ID of Power's
/// choosing, so Power types its instructions and the initial prompt as the first message.
/// The TUI runs its agents in a `jcode serve` daemon, and the bash tool runs there too
/// with the daemon's environment. A private socket per launch gives each agent its own
/// daemon, started by that agent's TUI, so its commands see the worker's `POWER_*`
/// variables. A restore gets a new daemon and stops the one its previous launch left. The bash tool exports
/// `JCODE_SESSION_ID`, which `power session task` reports for a restore to resume.
fn jcode() -> AgentProfile {
    let mut profile = builtin(
        "jcode",
        Injection::FirstMessage,
        &[
            "acp",
            "account",
            "ambient",
            "api-bridge",
            "auth",
            "auth-test",
            "browser",
            "cloud",
            "companion",
            "connect",
            "debug",
            "dictate",
            "help",
            "login",
            "mcp",
            "memory",
            "menubar",
            "mobile",
            "model",
            "pair",
            "permissions",
            "provider",
            "provider-doctor",
            "provider-test-coverage",
            "repl",
            "replay",
            "restart",
            "run",
            "self-dev",
            "serve",
            "server",
            "session",
            "setup-hotkey",
            "setup-launcher",
            "telemetry",
            "transcript",
            "update",
            "usage",
            "version",
        ],
        // A resumed conversation already holds the instructions.
        &["--resume"],
        (ModelCallback::None, TurnHook::None),
        // jcode reads ~/.agents/skills (as well as ~/.jcode/skills) natively.
        "~/.agents/skills",
        // jcode resumes only by ID. Without a known ID a restore starts a new
        // conversation rather than pick one from the session list.
        resume(&[], &["--resume", "{id}"], &[], &["--resume"]),
    );
    profile.server_socket_env = Some("JCODE_SOCKET".into());
    profile.conversation_env = Some("JCODE_SESSION_ID".into());
    profile
}

fn builtin(
    id: &str,
    injection: Injection,
    skip_subcommands: &[&str],
    skip_if_args_contain: &[&str],
    callbacks: (ModelCallback, TurnHook),
    skills_dir: &str,
    resume: Resume,
) -> AgentProfile {
    AgentProfile {
        id: AgentId::from(id),
        command: id.into(),
        injection,
        skip_subcommands: skip_subcommands.iter().map(|&arg| arg.into()).collect(),
        skip_if_args_contain: skip_if_args_contain.iter().map(|&arg| arg.into()).collect(),
        model_callback: callbacks.0,
        turn_hook: callbacks.1,
        skills_dir: Some(skills_dir.into()),
        resume: Some(resume),
        env: Vec::new(),
        conversation_env: None,
        server_socket_env: None,
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

/// The model a conversation currently uses, read from the agent's own session record.
/// jcode has no model callback and usually starts without `--model`, but every entry it
/// appends to `$JCODE_HOME/sessions/<id>.journal.jsonl` carries the session's current
/// model in `meta.model`, and the snapshot `<id>.json` carries it in `model`.
pub fn conversation_model(agent: &AgentId, conversation: &str) -> Option<String> {
    if agent.as_str() != "jcode"
        || conversation.is_empty()
        || conversation.contains(['/', '\\'])
        || conversation.starts_with('.')
    {
        return None;
    }
    let home = std::env::var_os("JCODE_HOME")
        .filter(|home| !home.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".jcode"))
        })?;
    jcode_session_model(&home.join("sessions"), conversation)
}

fn jcode_session_model(sessions: &std::path::Path, id: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 256 * 1024;

    let model = |value: &serde_json::Value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|model| !model.is_empty() && crate::terminal::valid_metadata(model))
            .map(str::to_owned)
    };
    if let Ok(mut journal) = std::fs::File::open(sessions.join(format!("{id}.journal.jsonl"))) {
        let len = journal.metadata().ok()?.len();
        let start = len.saturating_sub(TAIL);
        let mut tail = Vec::new();
        if journal.seek(SeekFrom::Start(start)).is_ok() && journal.read_to_end(&mut tail).is_ok() {
            // A tail that starts mid-file may start mid-line.
            let lines: Vec<&[u8]> = tail
                .split(|&byte| byte == b'\n')
                .skip(usize::from(start > 0))
                .collect();
            let found = lines
                .into_iter()
                .rev()
                .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                .find_map(|entry| model(&entry["meta"]["model"]));
            if found.is_some() {
                return found;
            }
        }
    }
    let snapshot = std::fs::read(sessions.join(format!("{id}.json"))).ok()?;
    model(&serde_json::from_slice::<serde_json::Value>(&snapshot).ok()?["model"])
}

#[cfg(test)]
mod model_tests {
    use super::*;

    #[test]
    fn jcode_model_comes_from_the_latest_journal_entry() {
        let dir = std::env::temp_dir().join(format!("power-jcode-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s1.json"), r#"{"model":"old-snapshot"}"#).unwrap();
        std::fs::write(
            dir.join("s1.journal.jsonl"),
            "{\"meta\":{\"model\":\"first\"}}\n{\"meta\":{\"model\":\"gpt-5.5\"}}\n",
        )
        .unwrap();
        assert_eq!(jcode_session_model(&dir, "s1").as_deref(), Some("gpt-5.5"));

        std::fs::write(dir.join("s2.json"), r#"{"model":"claude-opus-4-6"}"#).unwrap();
        assert_eq!(
            jcode_session_model(&dir, "s2").as_deref(),
            Some("claude-opus-4-6")
        );
        assert_eq!(jcode_session_model(&dir, "missing"), None);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(conversation_model(&"claude".into(), "s1"), None);
        assert_eq!(conversation_model(&"jcode".into(), "../x"), None);
    }
}
