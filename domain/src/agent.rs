use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

/// A profile's stable name on the command line, on the wire, and in storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentId(String);

impl AgentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn display_name(id: &str) -> &str {
    match id {
        "claude" => "Claude",
        "codex" => "Codex",
        "grok" => "Grok",
        "jcode" => "jcode",
        id => id,
    }
}

impl From<String> for AgentId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for AgentId {
    fn from(id: &str) -> Self {
        id.to_owned().into()
    }
}

impl From<AgentId> for String {
    fn from(id: AgentId) -> Self {
        id.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentProfile {
    pub id: AgentId,
    pub command: String,
    pub injection: Injection,
    pub skip_subcommands: Vec<String>,
    pub skip_if_args_contain: Vec<String>,
    pub model_callback: ModelCallback,
    pub turn_hook: TurnHook,
    pub skills_dir: Option<PathBuf>,
    pub resume: Option<Resume>,
    /// Environment variables set for each launch. Values may contain `{session}`, the
    /// Power session ID, and `{runtime}`, the directory holding Power's socket.
    pub env: Vec<(String, String)>,
    /// Environment variable through which the agent exposes its conversation ID to the
    /// commands it runs. `power session task`, `power session model` and `power dag
    /// report` read it and report the conversation, so a restore can resume it.
    pub conversation_env: Option<String>,
    /// Environment variable that names a private server socket for the agent. Each
    /// launch gets its own short socket path, and a new launch of a session stops any
    /// server still listening on an earlier launch's socket, so a restore never reattaches
    /// to a server outside its process tree.
    pub server_socket_env: Option<String>,
}

impl AgentProfile {
    /// The launch environment, with `{session}` and `{runtime}` replaced.
    pub fn launch_env(&self, session: &str, runtime: &str) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(name, value)| {
                let value = value
                    .replace("{session}", session)
                    .replace("{runtime}", runtime);
                (name.clone(), value)
            })
            .collect()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Resume {
    pub assign: Vec<String>,
    pub resume: Vec<String>,
    pub resume_last: Vec<String>,
    pub conflicts: Vec<String>,
    /// Flags that make a resume open a new conversation branched from the one it names,
    /// such as Claude's `--fork-session`. The named ID is the parent's, not the fork's.
    pub forks: Vec<String>,
}

/// What a launch adds and what it is known to open.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Launch {
    /// Arguments to prepend.
    pub args: Vec<OsString>,
    /// The conversation the launch opens, when it is known.
    pub conversation: Option<String>,
    /// Whether the launch resumes `conversation`, an existing one, rather than starting
    /// a new conversation under that ID.
    pub resumed: bool,
}

impl Resume {
    /// Return arguments to prepend at launch and the conversation it opens.
    pub fn launch(&self, user_args: &[OsString], new_id: impl FnOnce() -> String) -> Launch {
        let conflicts = user_args
            .iter()
            .any(|arg| self.conflicts.iter().any(|token| arg == token.as_str()));
        if !conflicts && !self.assign.is_empty() {
            let id = new_id();
            return Launch {
                args: Self::substitute(&self.assign, &id),
                conversation: Some(id),
                resumed: false,
            };
        }
        let find = |flags: &[String]| {
            let flags: Vec<&str> = flags
                .windows(2)
                .filter(|pair| pair[1] == "{id}")
                .map(|pair| pair[0].as_str())
                .collect();
            user_args
                .windows(2)
                .filter(|pair| flags.iter().any(|flag| pair[0] == *flag))
                .find_map(|pair| pair[1].to_str().map(str::to_owned))
        };
        let assigned = find(&self.assign);
        if self.forks(user_args) {
            // A fork opens a new conversation. Only an ID assigned to it names that one.
            return Launch {
                conversation: assigned,
                ..Launch::default()
            };
        }
        if let Some(id) = assigned {
            return Launch {
                conversation: Some(id),
                ..Launch::default()
            };
        }
        let conversation = find(&self.resume);
        Launch {
            resumed: conversation.is_some(),
            conversation,
            ..Launch::default()
        }
    }

    /// Whether `args` fork the conversation they resume.
    pub fn forks(&self, args: &[OsString]) -> bool {
        args.iter().any(|arg| {
            let text = arg.to_string_lossy();
            self.forks.iter().any(|flag| {
                text == flag.as_str()
                    || text
                        .strip_prefix(flag.as_str())
                        .is_some_and(|rest| rest.starts_with('='))
            })
        })
    }

    /// Return arguments for a known conversation or the profile's fallback.
    pub fn resume_args(&self, conversation: Option<&str>) -> Vec<OsString> {
        match conversation {
            Some(id) if !self.resume.is_empty() => Self::substitute(&self.resume, id),
            _ => self.resume_last.iter().map(OsString::from).collect(),
        }
    }

    /// Return arguments that start a new conversation in place of a failed resume.
    /// A profile that can pin IDs reuses the conversation ID, which is free because
    /// the failed resume found no transcript under it. Otherwise nothing is added,
    /// so the agent starts fresh with no resume arguments.
    pub fn fresh_args(&self, conversation: Option<&str>) -> Vec<OsString> {
        match conversation {
            Some(id) if !self.assign.is_empty() => Self::substitute(&self.assign, id),
            _ => Vec::new(),
        }
    }

    /// `args` without the options that select a conversation by ID, such as
    /// `--resume <id>` or `--resume=<id>`, so a restore can pass its own selector instead
    /// of a second, conflicting one.
    /// Fork flags go too, since a restore continues the saved conversation itself.
    pub fn without_selectors(&self, args: &[OsString]) -> Vec<OsString> {
        let args: Vec<OsString> = args
            .iter()
            .filter(|arg| !self.forks(std::slice::from_ref(*arg)))
            .cloned()
            .collect();
        let flags: Vec<&str> = self
            .assign
            .windows(2)
            .chain(self.resume.windows(2))
            .filter(|pair| pair[1] == "{id}")
            .map(|pair| pair[0].as_str())
            .collect();
        let mut kept = Vec::with_capacity(args.len());
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let text = arg.to_string_lossy();
            if flags.iter().any(|flag| text == *flag) {
                args.next();
            } else if !flags.iter().any(|flag| {
                text.strip_prefix(flag)
                    .is_some_and(|rest| rest.starts_with('='))
            }) {
                kept.push(arg.clone());
            }
        }
        kept
    }

    fn substitute(args: &[String], id: &str) -> Vec<OsString> {
        args.iter()
            .map(|arg| arg.replace("{id}", id).into())
            .collect()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Injection {
    Flag {
        flag: String,
    },
    ConfigValue {
        flag: String,
        key: String,
    },
    /// Power types the instructions into the agent's terminal as its first message, for
    /// agents that take neither an instructions flag nor a prompt argument. The initial
    /// prompt is typed in the same message instead of being passed as an argument.
    FirstMessage,
    #[default]
    None,
}

/// How Power registers `power session model --json` with an agent for one launch, so the
/// agent reports its model without changes to the user's config files.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ModelCallback {
    /// Pass Claude Code `--settings` with a `statusLine` that runs the callback, unless the
    /// user's settings already define a status line or the launch passes `--settings`.
    ClaudeStatusLine,
    /// Pass these arguments, with `{command}` replaced by the callback command.
    Args { args: Vec<String> },
    #[default]
    None,
}

/// The model flags of the agents whose command lines Power understands.
fn model_flags(agent: &str) -> Option<&'static [&'static str]> {
    match agent {
        "codex" | "grok" | "jcode" => Some(&["--model", "-m"]),
        "claude" => Some(&["--model"]),
        _ => None,
    }
}

/// How launch arguments select a model.
#[derive(Default)]
struct ModelSelection {
    /// A model flag appears before any `--`, with or without a usable value.
    flagged: bool,
    /// The model the last model flag selects, if its value is usable.
    model: Option<String>,
}

/// Parses model flags as `--model VALUE`, `--model=VALUE`, `-m VALUE`, `-m=VALUE` and
/// `-mVALUE`. Returns `None` for agents whose model flag Power does not know.
fn model_selection(
    agent: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Option<ModelSelection> {
    let flags = model_flags(agent)?;
    let mut selection = ModelSelection::default();
    let mut expecting_value = false;
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--" {
            break;
        }
        if std::mem::take(&mut expecting_value) {
            selection.model = arg
                .to_str()
                .filter(|value| !value.starts_with('-'))
                .and_then(model_name);
        }
        if flags.iter().any(|flag| arg == *flag) {
            selection.flagged = true;
            expecting_value = true;
        } else if let Some(value) = arg.to_str().and_then(|arg| attached_value(flags, arg)) {
            selection.flagged = true;
            selection.model = model_name(value);
        }
    }
    if expecting_value {
        selection.model = None;
    }
    Some(selection)
}

/// The value attached to a model flag in one argument: `--model=VALUE`, `-m=VALUE` or
/// `-mVALUE`.
fn attached_value<'a>(flags: &[&str], arg: &'a str) -> Option<&'a str> {
    flags.iter().find_map(|flag| {
        let rest = arg.strip_prefix(flag)?;
        if let Some(value) = rest.strip_prefix('=') {
            Some(value)
        } else {
            // Only a short flag takes its value without a separator.
            (!flag.starts_with("--") && !rest.is_empty()).then_some(rest)
        }
    })
}

/// The model that launch arguments select for the agent, if Power knows its model flag.
pub fn requested_model(
    agent: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Option<String> {
    model_selection(agent, args)?.model
}

/// True when the launch arguments pass a model flag the agent accepts, with or without a
/// usable value. Unknown agents report false.
pub fn passes_model_flag(agent: &str, args: &[impl AsRef<OsStr>]) -> bool {
    model_selection(agent, args).is_some_and(|selection| selection.flagged)
}

/// Launch arguments that select `model`, for agents whose model flag Power knows.
pub fn model_args(agent: &str, model: &str) -> Option<Vec<String>> {
    model_flags(agent)?;
    Some(vec!["--model".into(), model.into()])
}

/// A trimmed model name, if it is usable as session metadata.
pub fn model_name(value: &str) -> Option<String> {
    let value = value.trim();
    (valid_metadata(value) && !value.is_empty()).then(|| value.to_owned())
}

pub fn valid_metadata(value: &str) -> bool {
    value.chars().count() <= 120 && !value.chars().any(char::is_control)
}

/// How a profile reports the end of a turn to Power.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TurnHook {
    ClaudeSettings,
    CodexNotify,
    #[default]
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resume_spec() -> Resume {
        Resume {
            assign: vec!["--session-id".into(), "{id}".into()],
            resume: vec!["--resume".into(), "{id}".into()],
            resume_last: vec!["--continue".into()],
            conflicts: ["--session-id", "--resume", "--continue", "--print"]
                .map(String::from)
                .into(),
            forks: vec!["--fork-session".into()],
        }
    }

    fn opened(conversation: Option<&str>, resumed: bool) -> Launch {
        Launch {
            args: Vec::new(),
            conversation: conversation.map(Into::into),
            resumed,
        }
    }

    #[test]
    fn launch_assigns_an_id_unless_an_argument_matches_a_conflict_exactly() {
        let args = ["--resume-like", "mention --print"].map(OsString::from);
        assert_eq!(
            resume_spec().launch(&args, || "new-id".into()),
            Launch {
                args: vec!["--session-id".into(), "new-id".into()],
                conversation: Some("new-id".into()),
                resumed: false,
            }
        );
    }

    #[test]
    fn launch_tells_an_assigned_id_from_a_resumed_one() {
        let launch = |args: &[&str]| {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            resume_spec().launch(&args, || panic!("explicit ID"))
        };
        // `--session-id <id>` starts a new conversation, even under a saved ID.
        assert_eq!(
            launch(&["--session-id", "existing-id"]),
            opened(Some("existing-id"), false)
        );
        assert_eq!(
            launch(&["--resume", "existing-id"]),
            opened(Some("existing-id"), true)
        );
    }

    #[test]
    fn a_forked_resume_never_reports_the_parent_conversation() {
        let launch = |args: &[&str]| {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            resume_spec().launch(&args, || panic!("explicit ID"))
        };
        assert_eq!(
            launch(&["--resume", "parent", "--fork-session"]),
            opened(None, false)
        );
        assert_eq!(
            launch(&["--fork-session=true", "--resume", "parent"]),
            opened(None, false)
        );
        // An ID assigned to the fork names the fork, which is new.
        assert_eq!(
            launch(&[
                "--resume",
                "parent",
                "--fork-session",
                "--session-id",
                "fork"
            ]),
            opened(Some("fork"), false)
        );
    }

    #[test]
    fn launch_skips_assignment_on_conflicts_without_an_explicit_id() {
        for flag in ["--continue", "--print", "--resume"] {
            assert_eq!(
                resume_spec().launch(&[flag.into()], || panic!("conflicting flag")),
                opened(None, false)
            );
        }
    }

    #[test]
    fn launch_with_no_assign_args_can_still_capture_a_resume_id() {
        let spec = Resume {
            assign: Vec::new(),
            ..resume_spec()
        };
        assert_eq!(
            spec.launch(&[], || panic!("no assign args")),
            opened(None, false)
        );
        assert_eq!(
            spec.launch(&["--resume".into(), "existing-id".into()], || panic!(
                "no assign args"
            )),
            opened(Some("existing-id"), true)
        );
    }

    #[test]
    fn fresh_args_pin_the_same_id_only_when_the_profile_can_assign() {
        let spec = resume_spec();
        assert_eq!(
            spec.fresh_args(Some("existing-id")),
            ["--session-id", "existing-id"].map(OsString::from)
        );
        assert!(spec.fresh_args(None).is_empty());
        let codex = Resume {
            resume_last: vec!["resume".into(), "--last".into()],
            ..Resume::default()
        };
        assert!(codex.fresh_args(Some("existing-id")).is_empty());
        assert!(codex.fresh_args(None).is_empty());
    }

    #[test]
    fn launch_env_substitutes_the_session_and_runtime_directory() {
        let profile = AgentProfile {
            id: "jcode".into(),
            command: "jcode".into(),
            injection: Injection::FirstMessage,
            skip_subcommands: Vec::new(),
            skip_if_args_contain: Vec::new(),
            model_callback: ModelCallback::None,
            skills_dir: None,
            resume: None,
            env: vec![("SOCKET".into(), "{runtime}/agent-{session}.sock".into())],
            conversation_env: None,
            server_socket_env: None,
            turn_hook: TurnHook::None,
        };
        assert_eq!(
            profile.launch_env("abc", "/run/user/1"),
            [("SOCKET".to_owned(), "/run/user/1/agent-abc.sock".to_owned())]
        );
    }

    #[test]
    fn without_selectors_drops_id_options_in_both_spellings() {
        let args: Vec<OsString> = [
            "--model",
            "m",
            "--resume",
            "old",
            "--session-id=x",
            "--resume=other",
            "--resumes",
            "--fork-session",
        ]
        .map(Into::into)
        .into();
        assert_eq!(
            resume_spec().without_selectors(&args),
            ["--model", "m", "--resumes"].map(OsString::from)
        );
    }

    #[test]
    fn resume_args_substitutes_the_id_or_uses_the_fallback() {
        let mut spec = resume_spec();
        spec.resume.push("--label={id}".into());
        assert_eq!(
            spec.resume_args(Some("existing-id")),
            ["--resume", "existing-id", "--label=existing-id"].map(OsString::from)
        );
        assert_eq!(spec.resume_args(None), ["--continue"].map(OsString::from));

        spec.resume.clear();
        assert_eq!(
            spec.resume_args(Some("existing-id")),
            ["--continue"].map(OsString::from)
        );
    }

    #[test]
    fn attached_model_values_select_the_model() {
        for agent in ["codex", "grok"] {
            for args in [
                ["-mother-model"],
                ["-m=other-model"],
                ["--model=other-model"],
            ] {
                assert!(passes_model_flag(agent, &args), "{agent} {args:?}");
                assert_eq!(requested_model(agent, args), Some("other-model".into()));
            }
        }
        assert!(passes_model_flag("claude", &["--model=opus"]));
        assert_eq!(
            requested_model("claude", ["--model=opus"]),
            Some("opus".into())
        );
        assert_eq!(
            requested_model("codex", ["--model", "gpt", "-mmini"]),
            Some("mini".into())
        );
    }

    #[test]
    fn model_flags_are_detected_and_built_for_known_agents() {
        assert_eq!(requested_model("codex", ["-m", "gpt"]), Some("gpt".into()));
        assert_eq!(requested_model("claude", ["-m", "opus"]), None);
        assert_eq!(requested_model("custom", ["--model", "x"]), None);
        assert!(passes_model_flag("claude", &["--model"]));
        assert!(passes_model_flag("codex", &["-m=gpt"]));
        assert!(!passes_model_flag("codex", &["--", "--model"]));
        assert!(!passes_model_flag("claude", &["--models"]));
        assert!(!passes_model_flag("custom", &["--model", "x"]));
        assert!(!passes_model_flag("codex", &["--modelx"]));
        assert!(!passes_model_flag("claude", &["-mopus"]));
        assert_eq!(
            model_args("grok", "grok-5"),
            Some(vec!["--model".into(), "grok-5".into()])
        );
        assert_eq!(model_args("custom", "x"), None);
    }
}
