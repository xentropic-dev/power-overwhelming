use std::path::PathBuf;

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
