use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};
use url::{Host, Url};

pub const URI_PREFIX: &str = "pwr://artifact/";
pub const LEGACY_URI_PREFIX: &str = "power://artifact/";

pub fn artifact_id(uri: &str) -> Option<&str> {
    uri.strip_prefix(URI_PREFIX)
        .or_else(|| uri.strip_prefix(LEGACY_URI_PREFIX))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Image(PathBuf),
    Video(PathBuf),
    Html(PathBuf),
    Loopback(String),
}

impl Target {
    pub fn from_file(path: PathBuf) -> Result<Self, TargetError> {
        match path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "png" | "jpg" | "jpeg" | "webp" => Ok(Self::Image(path)),
            "mp4" | "webm" => Ok(Self::Video(path)),
            "htm" | "html" => Ok(Self::Html(path)),
            _ => Err(TargetError::UnsupportedFormat),
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Image(path) | Self::Video(path) | Self::Html(path) => path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            Self::Loopback(url) => url.clone(),
        }
    }
}

pub enum TargetLocation {
    File(PathBuf),
    Loopback(String),
}

impl TargetLocation {
    pub fn parse(value: &str, cwd: &Path) -> Result<Self, TargetError> {
        let Ok(url) = Url::parse(value) else {
            return Ok(Self::File(cwd.join(value)));
        };
        match url.scheme() {
            "file" => url
                .to_file_path()
                .map(Self::File)
                .map_err(|_| TargetError::RemoteFile),
            "http" | "https" if is_loopback(&url) => Ok(Self::Loopback(url.into())),
            _ => Err(TargetError::RemotePreview),
        }
    }
}

fn is_loopback(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.host() {
        Some(Host::Domain(host)) => host == "localhost",
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[derive(Debug)]
pub enum TargetError {
    RemoteFile,
    RemotePreview,
    UnsupportedFormat,
}

impl fmt::Display for TargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RemoteFile => "Expected a local file URL",
            Self::RemotePreview => "Preview links must point to a local file or a loopback URL",
            Self::UnsupportedFormat => "Unsupported artifact format",
        })
    }
}

impl Error for TargetError {}

#[derive(Debug)]
pub struct ResolvedArtifact {
    pub target: Target,
    pub title: String,
}

pub struct ArtifactRecord {
    pub id: String,
    pub session: String,
    pub worktree: PathBuf,
    pub artifact: ResolvedArtifact,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_the_artifact_target_rules() {
        for url in [
            "http://localhost:3000/",
            "https://127.1/",
            "http://[::1]:80/",
        ] {
            assert!(matches!(
                TargetLocation::parse(url, Path::new("/workspace")),
                Ok(TargetLocation::Loopback(_))
            ));
        }
        for url in ["http://example.com/", "http://user@localhost/"] {
            assert!(matches!(
                TargetLocation::parse(url, Path::new("/workspace")),
                Err(TargetError::RemotePreview)
            ));
        }
        let path = PathBuf::from("/workspace/report.HTML");
        assert_eq!(Target::from_file(path.clone()).unwrap(), Target::Html(path));
    }
}
