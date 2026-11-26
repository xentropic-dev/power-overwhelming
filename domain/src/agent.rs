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
    pub skills_dir: Option<PathBuf>,
    pub resume: Option<Resume>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Resume {
    pub assign: Vec<String>,
    pub resume: Vec<String>,
    pub resume_last: Vec<String>,
    pub conflicts: Vec<String>,
}

impl Resume {
    /// Return arguments to prepend at launch and the conversation ID to record.
    pub fn launch(
        &self,
        user_args: &[OsString],
        new_id: impl FnOnce() -> String,
    ) -> (Vec<OsString>, Option<String>) {
        let conflicts = user_args
            .iter()
            .any(|arg| self.conflicts.iter().any(|token| arg == token.as_str()));
        if !conflicts && !self.assign.is_empty() {
            let id = new_id();
            return (Self::substitute(&self.assign, &id), Some(id));
        }

        let id_flags = self
            .assign
            .windows(2)
            .chain(self.resume.windows(2))
            .filter(|pair| pair[1] == "{id}")
            .map(|pair| pair[0].as_str());
        let id = user_args
            .windows(2)
            .filter(|pair| id_flags.clone().any(|flag| pair[0] == flag))
            .find_map(|pair| pair[1].to_str().map(str::to_owned));
        (Vec::new(), id)
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
        "codex" | "grok" => Some(&["--model", "-m"]),
        "claude" => Some(&["--model"]),
        _ => None,
    }
}

/// The model that launch arguments select for the agent, if Power knows its model flag.
pub fn requested_model(
    agent: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Option<String> {
    let flags = model_flags(agent)?;
    let mut model = None;
    let mut expecting_value = false;
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--" {
            break;
        }
        if std::mem::take(&mut expecting_value) {
            model = arg
                .to_str()
                .filter(|value| !value.starts_with('-'))
                .and_then(model_name);
        }
        if flags.iter().any(|flag| arg == *flag) {
            expecting_value = true;
        } else if let Some((flag, value)) = arg.to_str().and_then(|arg| arg.split_once('='))
            && flags.contains(&flag)
        {
            model = model_name(value);
        }
    }
    if expecting_value { None } else { model }
}

/// True when the launch arguments pass a model flag the agent accepts, with or without a
/// usable value. Unknown agents report false.
pub fn passes_model_flag(agent: &str, args: &[impl AsRef<OsStr>]) -> bool {
    let Some(flags) = model_flags(agent) else {
        return false;
    };
    args.iter()
        .map(AsRef::as_ref)
        .take_while(|arg| *arg != "--")
        .filter_map(OsStr::to_str)
        .any(|arg| {
            flags
                .iter()
                .any(|flag| arg == *flag || arg.split_once('=').is_some_and(|(f, _)| f == *flag))
        })
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
        }
    }

    #[test]
    fn launch_assigns_an_id_unless_an_argument_matches_a_conflict_exactly() {
        let args = ["--resume-like", "mention --print"].map(OsString::from);
        assert_eq!(
            resume_spec().launch(&args, || "new-id".into()),
            (
                vec!["--session-id".into(), "new-id".into()],
                Some("new-id".into())
            )
        );
    }

    #[test]
    fn launch_captures_an_explicit_assignment_or_resume_id() {
        for flag in ["--session-id", "--resume"] {
            let args = [flag, "existing-id"].map(OsString::from);
            assert_eq!(
                resume_spec().launch(&args, || panic!("explicit ID")),
                (vec![], Some("existing-id".into()))
            );
        }
    }

    #[test]
    fn launch_skips_assignment_on_conflicts_without_an_explicit_id() {
        for flag in ["--continue", "--print", "--resume"] {
            assert_eq!(
                resume_spec().launch(&[flag.into()], || panic!("conflicting flag")),
                (vec![], None)
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
            (vec![], None)
        );
        assert_eq!(
            spec.launch(&["--resume".into(), "existing-id".into()], || panic!(
                "no assign args"
            )),
            (vec![], Some("existing-id".into()))
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
    fn model_flags_are_detected_and_built_for_known_agents() {
        assert_eq!(requested_model("codex", ["-m", "gpt"]), Some("gpt".into()));
        assert_eq!(requested_model("claude", ["-m", "opus"]), None);
        assert_eq!(requested_model("custom", ["--model", "x"]), None);
        assert!(passes_model_flag("claude", &["--model"]));
        assert!(passes_model_flag("codex", &["-m=gpt"]));
        assert!(!passes_model_flag("codex", &["--", "--model"]));
        assert!(!passes_model_flag("claude", &["--models"]));
        assert!(!passes_model_flag("custom", &["--model", "x"]));
        assert_eq!(
            model_args("grok", "grok-5"),
            Some(vec!["--model".into(), "grok-5".into()])
        );
        assert_eq!(model_args("custom", "x"), None);
    }
}
