mod bridge;

#[cfg(feature = "wasm-plugins")]
mod clock;
#[cfg(feature = "wasm-plugins")]
mod host;
#[cfg(feature = "wasm-plugins")]
pub mod panel;
#[cfg(feature = "wasm-plugins")]
mod status;
#[cfg(feature = "wasm-plugins")]
pub mod ui;

pub(crate) use bridge::Plugins;
pub use bridge::StatusBarPlugins;
#[cfg(feature = "wasm-plugins")]
pub use host::run_host;
#[cfg(feature = "wasm-plugins")]
pub use status::run_status_bar_host;

use domain::artifact::{ResolvedArtifact, Target};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ActiveView {
    pub session: String,
    pub worktree_name: String,
    pub worktree_path: String,
    pub repository_path: Option<String>,
    pub agent_type: String,
    pub agent_task: Option<String>,
    pub agent_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dag_progress: Option<String>,
}

/// The reserved config section that carries panel-plugin commands to the keymap. The host
/// adds it after config.lua runs, so Lua cannot change it.
pub const PLUGIN_COMMANDS_SECTION: &str = "@plugin_commands";

/// Which key prefix a plugin command's keys follow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandScope {
    Box,
    Tab,
}

/// A command a panel plugin registers, with its default keys.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct PluginCommand {
    /// A namespaced id such as `dashboard.open`.
    pub id: String,
    pub title: String,
    pub description: String,
    pub scope: CommandScope,
    pub default_keys: Vec<String>,
}

/// The plugin commands recorded in evaluated config sections. Empty when there are none.
pub fn plugin_commands(
    sections: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<PluginCommand>, String> {
    match sections.get(PLUGIN_COMMANDS_SECTION) {
        None => Ok(Vec::new()),
        Some(value) => Vec::deserialize(value).map_err(|error| error.to_string()),
    }
}

/// Merges plugin command defaults into a `keys` config section, under each command's scope
/// table, so config.lua can read and override them like built-in keys.
pub fn merge_command_keys(keys: &mut serde_json::Value, commands: &[PluginCommand]) {
    if !keys.is_object() {
        *keys = serde_json::json!({});
    }
    for command in commands {
        let scope = match command.scope {
            CommandScope::Box => "box",
            CommandScope::Tab => "tab",
        };
        let table = keys
            .as_object_mut()
            .expect("keys is an object")
            .entry(scope)
            .or_insert_with(|| serde_json::json!({}));
        if let Some(table) = table.as_object_mut() {
            table
                .entry(command.id.clone())
                .or_insert_with(|| serde_json::json!(command.default_keys));
        }
    }
}

pub fn valid_status_field(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[derive(Deserialize, Serialize)]
enum HostMessage {
    ArtifactPublished { uri: String },
    ArtifactResolved(Result<PluginArtifact, String>),
    ActiveView(ActiveView),
}

#[derive(Deserialize, Serialize)]
enum WorkerReply {
    Ready,
    ResolvePublished,
    Finished,
    StatusFields(BTreeMap<String, String>),
}

#[derive(Deserialize, Serialize)]
struct PluginArtifact {
    title: String,
    kind: PluginTargetKind,
    location: String,
}

#[derive(Deserialize, Serialize)]
enum PluginTargetKind {
    Image,
    Video,
    Html,
    Loopback,
}

impl From<ResolvedArtifact> for PluginArtifact {
    fn from(artifact: ResolvedArtifact) -> Self {
        let (kind, location) = match artifact.target {
            Target::Image(path) => (PluginTargetKind::Image, path.to_string_lossy().into_owned()),
            Target::Video(path) => (PluginTargetKind::Video, path.to_string_lossy().into_owned()),
            Target::Html(path) => (PluginTargetKind::Html, path.to_string_lossy().into_owned()),
            Target::Loopback(url) => (PluginTargetKind::Loopback, url),
        };
        Self {
            title: artifact.title,
            kind,
            location,
        }
    }
}
