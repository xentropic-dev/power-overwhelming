use crate::wire;
use anyhow::{Context, Result};
use application::ArtifactStore;
use domain::artifact::{ArtifactRecord, ResolvedArtifact};
use rusqlite::{Connection, params};
use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf, sync::Mutex};

pub struct SqliteArtifacts(Mutex<Connection>);

impl SqliteArtifacts {
    pub fn open() -> Result<Self> {
        let db_path = state_path()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(db_path.parent().context("Invalid database path")?)?;
        let db = Connection::open(db_path)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS artifacts (
                id TEXT PRIMARY KEY, session TEXT NOT NULL, worktree TEXT NOT NULL,
                title TEXT NOT NULL, target TEXT NOT NULL
            );",
        )?;
        Ok(Self(Mutex::new(db)))
    }
}

impl ArtifactStore for SqliteArtifacts {
    fn insert(&self, record: ArtifactRecord) -> Result<()> {
        let target = serde_json::to_string(&wire::Target::from(record.artifact.target))?;
        self.0.lock().unwrap().execute(
            "INSERT INTO artifacts VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.id,
                record.session,
                record.worktree.to_string_lossy(),
                record.artifact.title,
                target,
            ],
        )?;
        Ok(())
    }

    fn find(&self, id: &str) -> Result<ResolvedArtifact> {
        let (title, json): (String, String) = self
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT title, target FROM artifacts WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("Published artifact is unavailable")?;
        Ok(ResolvedArtifact {
            title,
            target: serde_json::from_str::<wire::Target>(&json)?.into(),
        })
    }
}

fn state_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("POWER_STATE_DIR") {
        return Ok(PathBuf::from(path).join("artifacts.sqlite3"));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("Set XDG_STATE_HOME or HOME")?;
    Ok(base.join("power-overwhelming/artifacts.sqlite3"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::artifact::Target;

    #[test]
    fn reads_a_pre_refactor_artifact_record() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE artifacts (
                id TEXT PRIMARY KEY, session TEXT NOT NULL, worktree TEXT NOT NULL,
                title TEXT NOT NULL, target TEXT NOT NULL
            );",
        )?;
        db.execute(
            "INSERT INTO artifacts VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "legacy-id",
                "old-session",
                "/tmp/worktree",
                "Existing preview",
                r#"{"kind":"html","value":"/tmp/worktree/old report.html"}"#,
            ],
        )?;
        let store = SqliteArtifacts(Mutex::new(db));
        let artifact = store.find("legacy-id")?;
        assert_eq!(artifact.title, "Existing preview");
        assert_eq!(
            artifact.target,
            Target::Html("/tmp/worktree/old report.html".into())
        );
        Ok(())
    }
}
