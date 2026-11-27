//! A durable list of the cgroups each session's launches run in, kept beside the
//! session records. A restarted daemon reclaims through it, so a scope an earlier
//! daemon started is still ended when this daemon runs without cgroup ownership or
//! in another slice.

use anyhow::{Context, Result};
use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

fn directory(state: &Path) -> PathBuf {
    state.join("owned-scopes")
}

/// The ledger file of `session`. Session IDs are generated, but anything outside
/// `[A-Za-z0-9-]` is replaced so an ID can never name another path.
fn file(state: &Path, session: &str) -> PathBuf {
    let name: String = session
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || char == '-' {
                char
            } else {
                '_'
            }
        })
        .collect();
    directory(state).join(name)
}

/// Records that a launch of `session` runs in the cgroup at `scope`, before the
/// launch starts, so no crash can leave a scope that nothing records.
pub(crate) fn record(state: &Path, session: &str, scope: &Path) -> Result<()> {
    let directory = directory(state);
    fs::create_dir_all(&directory)
        .with_context(|| format!("Could not create {}", directory.display()))?;
    let path = file(state, session);
    let mut ledger = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Could not open {}", path.display()))?;
    let mut line = scope.as_os_str().as_encoded_bytes().to_vec();
    line.push(b'\n');
    ledger
        .write_all(&line)
        .and_then(|()| ledger.sync_data())
        .with_context(|| format!("Could not record a scope in {}", path.display()))
}

/// The cgroups recorded for `session`. A ledger that exists but cannot be read is
/// an error, since what it names may still run.
pub(crate) fn scopes(state: &Path, session: &str) -> Result<Vec<PathBuf>> {
    let path = file(state, session);
    match fs::read(&path) {
        Ok(bytes) => Ok(bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            // SAFETY: the bytes were written from an OsStr on this platform.
            .map(|line| {
                PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(line) })
            })
            .collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}

/// Forgets `session`'s ledger once nothing it names runs.
pub(crate) fn forget(state: &Path, session: &str) -> Result<()> {
    let path = file(state, session);
    match fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("Could not remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ledger_lists_each_recorded_scope_until_forgotten() -> Result<()> {
        let state = tempfile::tempdir()?;
        assert!(scopes(state.path(), "s-1")?.is_empty());
        record(state.path(), "s-1", Path::new("/sys/fs/cgroup/a.scope"))?;
        record(state.path(), "s-1", Path::new("/sys/fs/cgroup/b.scope"))?;
        record(state.path(), "s/2", Path::new("/sys/fs/cgroup/c.scope"))?;
        assert_eq!(
            scopes(state.path(), "s-1")?,
            [
                PathBuf::from("/sys/fs/cgroup/a.scope"),
                PathBuf::from("/sys/fs/cgroup/b.scope")
            ]
        );
        forget(state.path(), "s-1")?;
        assert!(scopes(state.path(), "s-1")?.is_empty());
        assert_eq!(scopes(state.path(), "s/2")?.len(), 1);
        Ok(())
    }
}
