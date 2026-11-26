use std::{ffi::OsString, path::PathBuf};

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
}
