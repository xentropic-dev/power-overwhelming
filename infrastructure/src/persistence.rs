use crate::wire;
use anyhow::{Context, Result};
use application::{
    ArtifactStore, Labels, RecentArtifact, SessionArtifact, SessionRecord, SessionStore,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use domain::{
    artifact::{ArtifactRecord, ResolvedArtifact, URI_PREFIX},
    dag::AttemptArtifact,
    session::Agent,
};
use rusqlite::{Connection, Row, params};
use serde::{Deserialize, de::value::StringDeserializer};
use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::DirBuilderExt,
    },
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct SqliteArtifacts(Mutex<Connection>);

impl SqliteArtifacts {
    pub fn open() -> Result<Self> {
        let db_path = state_directory()?.join("artifacts.sqlite3");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(db_path.parent().context("Invalid database path")?)?;
        Self::initialize(Connection::open(db_path)?)
    }

    fn initialize(db: Connection) -> Result<Self> {
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

    fn recent(&self, sessions: &[String], limit: usize) -> Result<Vec<RecentArtifact>> {
        if sessions.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; sessions.len()].join(", ");
        // Rows are never updated, so rowid order is publish order.
        let query = format!(
            "SELECT id, session, title FROM artifacts WHERE session IN ({placeholders})
             ORDER BY rowid DESC LIMIT {limit}"
        );
        let db = self.0.lock().unwrap();
        let mut statement = db.prepare(&query)?;
        let rows = statement.query_map(rusqlite::params_from_iter(sessions), |row| {
            Ok(RecentArtifact {
                uri: format!("{URI_PREFIX}{}", row.get::<_, String>(0)?),
                session: row.get(1)?,
                title: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn published(&self, sessions: &[String]) -> Result<Vec<SessionArtifact>> {
        if sessions.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; sessions.len()].join(", ");
        // Rows are never updated, so rowid order is publish order.
        let query = format!(
            "SELECT id, session, title FROM artifacts WHERE session IN ({placeholders})
             ORDER BY rowid ASC"
        );
        let db = self.0.lock().unwrap();
        let mut statement = db.prepare(&query)?;
        let rows = statement.query_map(rusqlite::params_from_iter(sessions), |row| {
            Ok(SessionArtifact {
                session: row.get(1)?,
                artifact: AttemptArtifact {
                    id: row.get(0)?,
                    title: row.get(2)?,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

pub struct SqliteSessions(Mutex<Connection>);

#[allow(dead_code, reason = "Daemon integration follows the session registry")]
impl SqliteSessions {
    pub fn open() -> Result<Self> {
        Self::open_at(&state_directory()?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn open_at(directory: &Path) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        Self::initialize(Connection::open(directory.join("sessions.sqlite3"))?)
    }

    fn initialize(db: Connection) -> Result<Self> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, worktree BLOB NOT NULL, agent TEXT NOT NULL,
                args TEXT NOT NULL, env TEXT NOT NULL, foreground TEXT NOT NULL,
                conversation TEXT, cwd BLOB, created INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )?;
        // Older databases predate the reported labels.
        for column in ["task", "model", "model_reported"] {
            let present: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = ?1)",
                [column],
                |row| row.get(0),
            )?;
            if !present {
                db.execute(
                    &format!(
                        "ALTER TABLE sessions ADD COLUMN {column} {}",
                        if column == "model_reported" {
                            "INTEGER NOT NULL DEFAULT 0"
                        } else {
                            "TEXT"
                        }
                    ),
                    [],
                )?;
            }
        }
        Ok(Self(Mutex::new(db)))
    }
}

#[allow(dead_code, reason = "Daemon integration follows the session registry")]
impl SessionStore for SqliteSessions {
    fn save(&self, record: &SessionRecord) -> Result<()> {
        let args: Vec<_> = record
            .args
            .iter()
            .map(|arg| encode_os_string(arg))
            .collect();
        let env: Vec<_> = record
            .env
            .iter()
            .map(|(key, value)| {
                (
                    encode_os_string(key),
                    value.as_deref().map(encode_os_string),
                )
            })
            .collect();
        // Keep the original rowid when saving an existing session.
        self.0.lock().unwrap().execute(
            "INSERT INTO sessions
                (id, worktree, agent, args, env, foreground, conversation, cwd,
                 task, model, model_reported)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                worktree = excluded.worktree, agent = excluded.agent, args = excluded.args,
                env = excluded.env, foreground = excluded.foreground,
                conversation = excluded.conversation, cwd = excluded.cwd,
                task = excluded.task, model = excluded.model,
                model_reported = excluded.model_reported",
            params![
                record.id,
                path_bytes(&record.worktree),
                record.agent.as_str(),
                serde_json::to_string(&args)?,
                serde_json::to_string(&env)?,
                record.foreground.as_str(),
                record.conversation,
                record.cwd.as_deref().map(path_bytes),
                record.labels.task,
                record.labels.model,
                record.labels.model_reported,
            ],
        )?;
        Ok(())
    }

    fn update_labels(&self, id: &str, labels: &Labels) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE sessions SET task = ?2, model = ?3, model_reported = ?4 WHERE id = ?1",
            params![id, labels.task, labels.model, labels.model_reported],
        )?;
        Ok(())
    }

    fn update_foreground(
        &self,
        id: &str,
        foreground: &Agent,
        conversation: Option<&str>,
        cwd: Option<&Path>,
        labels: Option<&Labels>,
    ) -> Result<()> {
        let db = self.0.lock().unwrap();
        let (foreground, cwd) = (foreground.as_str(), cwd.map(path_bytes));
        // One statement, so the saved agent and its labels never come from different owners.
        match labels {
            Some(labels) => db.execute(
                "UPDATE sessions SET foreground = ?2, conversation = ?3, cwd = ?4,
                    task = ?5, model = ?6, model_reported = ?7
                 WHERE id = ?1",
                params![
                    id,
                    foreground,
                    conversation,
                    cwd,
                    labels.task,
                    labels.model,
                    labels.model_reported
                ],
            )?,
            None => db.execute(
                "UPDATE sessions SET foreground = ?2, conversation = ?3, cwd = ?4 WHERE id = ?1",
                params![id, foreground, conversation, cwd],
            )?,
        };
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<SessionRecord>> {
        let db = self.0.lock().unwrap();
        let mut statement = db.prepare(
            "SELECT id, worktree, agent, args, env, foreground, conversation, cwd, task, model,
                    model_reported
             FROM sessions ORDER BY rowid",
        )?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(read_session(row)?);
        }
        Ok(records)
    }
}

fn read_session(row: &Row<'_>) -> Result<SessionRecord> {
    let args: Vec<String> = serde_json::from_str(row.get_ref(3)?.as_str()?)?;
    let env: Vec<(String, Option<String>)> = serde_json::from_str(row.get_ref(4)?.as_str()?)?;
    Ok(SessionRecord {
        id: row.get(0)?,
        worktree: path_from_bytes(row.get(1)?),
        agent: session_agent(row.get(2)?)?,
        args: args
            .into_iter()
            .map(decode_os_string)
            .collect::<Result<_>>()?,
        env: env
            .into_iter()
            .map(|(key, value)| {
                Ok((
                    decode_os_string(key)?,
                    value.map(decode_os_string).transpose()?,
                ))
            })
            .collect::<Result<_>>()?,
        foreground: session_agent(row.get(5)?)?,
        conversation: row.get(6)?,
        cwd: row.get::<_, Option<Vec<u8>>>(7)?.map(path_from_bytes),
        labels: Labels {
            task: row.get(8)?,
            model: row.get(9)?,
            model_reported: row.get(10)?,
        },
    })
}

fn session_agent(name: String) -> Result<Agent> {
    let name = StringDeserializer::<serde::de::value::Error>::new(name);
    Ok(wire::Agent::deserialize(name)?.into())
}

fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    OsString::from_vec(bytes).into()
}

fn encode_os_string(value: &OsStr) -> String {
    STANDARD.encode(value.as_bytes())
}

fn decode_os_string(value: String) -> Result<OsString> {
    Ok(OsString::from_vec(
        STANDARD
            .decode(value)
            .context("Decode session launch data")?,
    ))
}

pub(crate) fn state_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("POWER_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("Set XDG_STATE_HOME or HOME")?;
    Ok(base.join("power-overwhelming"))
}

pub(crate) fn data_directory() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .context("Set XDG_DATA_HOME or HOME")?;
    Ok(base.join("power-overwhelming"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::artifact::Target;
    use std::os::unix::fs::PermissionsExt;

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

    #[test]
    fn lists_published_artifacts_for_sessions_in_publish_order() -> Result<()> {
        let store = SqliteArtifacts::initialize(Connection::open_in_memory()?)?;
        // IDs sort opposite to publish order, so only rowid order passes.
        for (id, session, title) in [
            ("z-first", "worker", "First"),
            ("m-other", "other", "Other session"),
            ("a-second", "worker", "Second"),
            ("b-third", "reviewer", "Third"),
        ] {
            store.insert(ArtifactRecord {
                id: id.into(),
                session: session.into(),
                worktree: "/repo".into(),
                artifact: ResolvedArtifact {
                    title: title.into(),
                    target: Target::Loopback("http://localhost:3000/".into()),
                },
            })?;
        }
        let published = store.published(&["worker".into(), "reviewer".into()])?;
        let listed: Vec<_> = published
            .iter()
            .map(|entry| {
                (
                    entry.session.as_str(),
                    entry.artifact.id.as_str(),
                    entry.artifact.title.as_str(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("worker", "z-first", "First"),
                ("worker", "a-second", "Second"),
                ("reviewer", "b-third", "Third"),
            ]
        );
        assert!(store.published(&[])?.is_empty());
        Ok(())
    }

    fn session(id: &str) -> SessionRecord {
        SessionRecord {
            id: id.to_owned(),
            worktree: "/repo".into(),
            agent: Agent::Shell,
            args: vec!["sh".into(), "-c".into(), "echo ready".into()],
            env: vec![
                ("POWER_BOX".into(), Some("box-id".into())),
                ("UNSET".into(), None),
                ("EMPTY".into(), Some(OsString::new())),
            ],
            foreground: Agent::Coding("custom".into()),
            conversation: Some("conversation-id".to_owned()),
            cwd: Some("/repo/subdir".into()),
            labels: Labels {
                task: Some("Fix the build".to_owned()),
                model: Some("opus".to_owned()),
                model_reported: true,
            },
        }
    }

    #[test]
    fn saves_and_replaces_sessions_in_creation_order() -> Result<()> {
        let store = SqliteSessions::open_in_memory()?;
        let mut first = session("z");
        let second = session("a");
        store.save(&first)?;
        store.save(&second)?;
        assert_eq!(store.list()?, [first.clone(), second.clone()]);

        first.worktree = "/other".into();
        first.agent = Agent::Coding("claude".into());
        first.args = vec!["--resume".into(), "new-conversation".into()];
        first.env.clear();
        first.foreground = first.agent.clone();
        first.conversation = Some("new-conversation".to_owned());
        first.cwd = None;
        store.save(&first)?;
        assert_eq!(store.list()?, [first, second]);
        Ok(())
    }

    #[test]
    fn updates_and_clears_foreground_state() -> Result<()> {
        let store = SqliteSessions::open_in_memory()?;
        let mut record = session("id");
        store.save(&record)?;

        record.foreground = Agent::Coding("codex".into());
        record.conversation = Some("resumable".to_owned());
        record.cwd = Some("/repo/current".into());
        store.update_foreground(
            &record.id,
            &record.foreground,
            record.conversation.as_deref(),
            record.cwd.as_deref(),
            None,
        )?;
        // Without labels the saved ones are kept.
        assert_eq!(store.list()?, [record.clone()]);

        store.update_foreground(
            &record.id,
            &Agent::Shell,
            None,
            None,
            Some(&Labels::default()),
        )?;
        record.foreground = Agent::Shell;
        record.conversation = None;
        record.cwd = None;
        record.labels = Labels::default();
        assert_eq!(store.list()?, [record]);
        Ok(())
    }

    #[test]
    fn updates_and_clears_labels() -> Result<()> {
        let store = SqliteSessions::open_in_memory()?;
        let mut record = session("id");
        store.save(&record)?;
        let cleared = Labels {
            task: Some("New task".to_owned()),
            model: None,
            model_reported: true,
        };
        store.update_labels("id", &cleared)?;
        record.labels = cleared;
        assert_eq!(store.list()?, [record]);
        Ok(())
    }

    #[test]
    fn loads_records_saved_before_labels_existed() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sessions.sqlite3");
        let db = Connection::open(&path)?;
        db.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, worktree BLOB NOT NULL, agent TEXT NOT NULL,
                args TEXT NOT NULL, env TEXT NOT NULL, foreground TEXT NOT NULL,
                conversation TEXT, cwd BLOB, created INTEGER NOT NULL DEFAULT (unixepoch())
            );
            INSERT INTO sessions (id, worktree, agent, args, env, foreground)
                VALUES ('old', x'2f72', 'shell', '[]', '[]', 'shell');",
        )?;
        drop(db);
        let store = SqliteSessions::open_at(directory.path())?;
        let records = store.list()?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].labels, Labels::default());
        let labels = Labels {
            task: Some("t".into()),
            model: Some("m".into()),
            model_reported: true,
        };
        store.update_labels("old", &labels)?;
        assert_eq!(store.list()?[0].labels, labels);
        Ok(())
    }

    #[test]
    fn removes_only_the_requested_session() -> Result<()> {
        let store = SqliteSessions::open_in_memory()?;
        let other = session("other");
        store.save(&session("id"))?;
        store.save(&other)?;

        store.remove("id")?;
        store.remove("id")?;
        store.update_foreground("id", &Agent::Shell, None, None, None)?;
        assert_eq!(store.list()?, [other]);
        Ok(())
    }

    #[test]
    fn retains_non_utf8_launch_data_after_reopening() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let state = directory.path().join("state");
        let mut record = session("id");
        record.args.push(OsString::from_vec(vec![b'a', 0xff]));
        record.env.push((
            OsString::from_vec(vec![b'K', 0xfe]),
            Some(OsString::from_vec(vec![b'v', 0xfd])),
        ));
        record.worktree = path_from_bytes(b"/repo\xff".to_vec());
        record.cwd = Some(path_from_bytes(b"/repo\xff/subdir\xfe".to_vec()));

        SqliteSessions::open_at(&state)?.save(&record)?;
        assert_eq!(fs::metadata(&state)?.permissions().mode() & 0o077, 0);
        let reopened = SqliteSessions::open_at(&state)?;
        assert_eq!(reopened.list()?, [record]);
        Ok(())
    }
}
