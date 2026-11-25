mod bridge;

#[cfg(feature = "wasm-plugins")]
mod clock;
#[cfg(feature = "wasm-plugins")]
mod host;
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
