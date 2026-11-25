mod bridge;

#[cfg(feature = "wasm-plugins")]
mod host;

pub(crate) use bridge::Plugins;
#[cfg(feature = "wasm-plugins")]
pub use host::run_host;

use domain::artifact::{ResolvedArtifact, Target};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
enum HostMessage {
    ArtifactPublished { uri: String },
    ArtifactResolved(Result<PluginArtifact, String>),
}

#[derive(Deserialize, Serialize)]
enum WorkerReply {
    Ready,
    ResolvePublished,
    Finished,
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
