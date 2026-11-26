use crate::{persistence::state_directory, wire};
use anyhow::{Context, Result};
use application::dag::{DagReader, DagStore, DagTransaction, EventScope, OpenAttempt, now};
use domain::{
    dag::{Attempt, ManagedAgent, Run, RunEvent, RunEventKind, Task, TaskSpec, TaskState},
    worktree::{ManagedWorktree, WorktreeCreator},
};
use rusqlite::{Connection, OptionalExtension, Params, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::IntoDeserializer};
use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub(crate) struct SqliteDagStore(Mutex<Connection>);

impl SqliteDagStore {
    pub(crate) fn open() -> Result<Self> {
        Self::initialize(Connection::open(state_directory()?.join("dag.sqlite3"))?)
    }

    fn initialize(db: Connection) -> Result<Self> {
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY, box_id TEXT NOT NULL, title TEXT NOT NULL,
                 coordinator TEXT NOT NULL, created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS agents (
                 id TEXT PRIMARY KEY, box_id TEXT NOT NULL, worktree TEXT NOT NULL,
                 provider TEXT NOT NULL, parent TEXT REFERENCES agents(id), session TEXT UNIQUE,
                 state TEXT NOT NULL, label TEXT NOT NULL, created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS tasks (
                 run_id TEXT NOT NULL REFERENCES runs(id), id TEXT NOT NULL,
                 title TEXT NOT NULL, instructions TEXT NOT NULL, PRIMARY KEY (run_id, id)
             );
             CREATE TABLE IF NOT EXISTS deps (
                 run_id TEXT NOT NULL, task_id TEXT NOT NULL, dep TEXT NOT NULL,
                 PRIMARY KEY (run_id, task_id, dep),
                 FOREIGN KEY (run_id, task_id) REFERENCES tasks(run_id, id),
                 FOREIGN KEY (run_id, dep) REFERENCES tasks(run_id, id)
             );
             CREATE TABLE IF NOT EXISTS attempts (
                 id TEXT PRIMARY KEY, run_id TEXT NOT NULL, task_id TEXT NOT NULL,
                 number INTEGER NOT NULL, agent TEXT NOT NULL UNIQUE REFERENCES agents(id),
                 state TEXT NOT NULL, summary TEXT, blocker TEXT, detail TEXT,
                 started_at INTEGER NOT NULL, ended_at INTEGER,
                 FOREIGN KEY (run_id, task_id) REFERENCES tasks(run_id, id),
                 UNIQUE (run_id, task_id, number)
             );
             CREATE UNIQUE INDEX IF NOT EXISTS active_attempt ON attempts(run_id, task_id)
                 WHERE state IN ('active', 'needs_input');
             CREATE TABLE IF NOT EXISTS events (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL REFERENCES runs(id),
                 kind TEXT NOT NULL, task_id TEXT, attempt_id TEXT, summary TEXT, at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS run_events ON events(run_id, seq);
             CREATE TABLE IF NOT EXISTS worktrees (
                 path BLOB NOT NULL PRIMARY KEY, box_id TEXT NOT NULL, repository BLOB NOT NULL,
                 branch TEXT NOT NULL, base_commit TEXT NOT NULL, created_at INTEGER NOT NULL,
                 creator_run TEXT, creator_task TEXT, creator_attempt TEXT UNIQUE,
                 creator_box TEXT, creator_session TEXT,
                 CHECK (
                     (creator_run IS NOT NULL AND creator_task IS NOT NULL
                      AND creator_attempt IS NOT NULL AND creator_box IS NULL
                      AND creator_session IS NULL)
                     OR
                     (creator_run IS NULL AND creator_task IS NULL AND creator_attempt IS NULL
                      AND creator_box IS NOT NULL AND creator_session IS NOT NULL)
                 )
             );",
        )?;
        Ok(Self(Mutex::new(db)))
    }
}

impl DagStore for SqliteDagStore {
    fn read<T>(&self, query: impl FnOnce(&dyn DagReader) -> Result<T>) -> Result<T> {
        let db = self.0.lock().unwrap();
        query(&DagDb(&db))
    }

    fn write<T>(&self, operation: impl FnOnce(&dyn DagTransaction) -> Result<T>) -> Result<T> {
        let mut db = self.0.lock().unwrap();
        let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&DagDb(&transaction))?;
        transaction.commit()?;
        Ok(result)
    }
}

struct DagDb<'a>(&'a Connection);

impl DagDb<'_> {
    fn require_run(&self, id: &str) -> Result<Run> {
        self.0
            .query_row(
                "SELECT id, box_id, title, coordinator, created_at FROM runs WHERE id = ?1",
                [id],
                run_from_row,
            )
            .optional()?
            .with_context(|| format!("Unknown run {id:?}"))
    }

    fn load_tasks(&self, run: &mut Run) -> Result<()> {
        let tasks = rows(
            self.0,
            "SELECT id, title, instructions FROM tasks WHERE run_id = ?1 ORDER BY rowid",
            [&run.id],
            |row| {
                Ok(Task {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    instructions: row.get(2)?,
                    deps: Vec::new(),
                    state: TaskState::Waiting,
                    current_attempt: None,
                    attempts: Vec::new(),
                })
            },
        )?;
        let mut deps = HashMap::<String, Vec<String>>::new();
        for (task, dep) in rows(
            self.0,
            "SELECT task_id, dep FROM deps WHERE run_id = ?1 ORDER BY rowid",
            [&run.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )? {
            deps.entry(task).or_default().push(dep);
        }
        let mut attempts = HashMap::<String, Vec<Attempt>>::new();
        for (task, attempt) in rows(
            self.0,
            "SELECT a.id, a.number, a.agent, g.session, g.worktree, g.provider,
                    a.state, a.summary, a.blocker, a.detail, a.started_at, a.ended_at, a.task_id
             FROM attempts a JOIN agents g ON g.id = a.agent
             WHERE a.run_id = ?1 ORDER BY a.number",
            [&run.id],
            |row| Ok((row.get(12)?, attempt_from_row(row)?)),
        )? {
            attempts.entry(task).or_default().push(attempt);
        }
        run.tasks = tasks
            .into_iter()
            .map(|mut task| {
                task.deps = deps.remove(&task.id).unwrap_or_default();
                task.attempts = attempts.remove(&task.id).unwrap_or_default();
                task
            })
            .collect();
        Ok(())
    }
}

impl DagReader for DagDb<'_> {
    fn list_worktrees(&self, box_id: Option<&str>) -> Result<Vec<ManagedWorktree>> {
        Ok(rows(
            self.0,
            &format!("{WORKTREE_SELECT} WHERE (?1 IS NULL OR box_id = ?1) ORDER BY rowid"),
            [box_id],
            worktree_from_row,
        )?)
    }

    fn find_worktree_by_attempt(&self, attempt: &str) -> Result<Option<ManagedWorktree>> {
        Ok(self
            .0
            .query_row(
                &format!("{WORKTREE_SELECT} WHERE creator_attempt = ?1"),
                [attempt],
                worktree_from_row,
            )
            .optional()?)
    }

    fn find_worktree_by_path(&self, path: &Path) -> Result<Option<ManagedWorktree>> {
        Ok(self
            .0
            .query_row(
                &format!("{WORKTREE_SELECT} WHERE path = ?1"),
                [path.as_os_str().as_bytes()],
                worktree_from_row,
            )
            .optional()?)
    }

    fn list_agents(&self, box_id: Option<&str>) -> Result<Vec<ManagedAgent>> {
        Ok(rows(
            self.0,
            &format!("{AGENT_SELECT} WHERE (?1 IS NULL OR a.box_id = ?1) ORDER BY a.rowid"),
            [box_id],
            agent_from_row,
        )?)
    }

    fn agent(&self, id: &str) -> Result<ManagedAgent> {
        self.0
            .query_row(
                &format!("{AGENT_SELECT} WHERE a.id = ?1"),
                [id],
                agent_from_row,
            )
            .optional()?
            .with_context(|| format!("Unknown agent {id:?}"))
    }

    fn session_agent(&self, session: &str) -> Result<Option<ManagedAgent>> {
        Ok(self
            .0
            .query_row(
                &format!("{AGENT_SELECT} WHERE a.session = ?1"),
                [session],
                agent_from_row,
            )
            .optional()?)
    }

    fn run(&self, id: &str) -> Result<Run> {
        let mut run = self.require_run(id)?;
        self.load_tasks(&mut run)?;
        Ok(run)
    }

    fn run_box(&self, id: &str) -> Result<String> {
        Ok(self.require_run(id)?.box_id)
    }

    fn list_runs(&self, box_id: Option<&str>) -> Result<Vec<Run>> {
        let mut runs = rows(
            self.0,
            "SELECT id, box_id, title, coordinator, created_at FROM runs
             WHERE (?1 IS NULL OR box_id = ?1) ORDER BY rowid",
            [box_id],
            run_from_row,
        )?;
        for run in &mut runs {
            self.load_tasks(run)?;
        }
        Ok(runs)
    }

    fn open_attempts(&self, session: Option<&str>) -> Result<Vec<OpenAttempt>> {
        Ok(rows(
            self.0,
            "SELECT a.id, a.number, a.agent, g.session, g.worktree, g.provider,
                    a.state, a.summary, a.blocker, a.detail, a.started_at, a.ended_at,
                    a.run_id, g.box_id, a.task_id
             FROM attempts a JOIN agents g ON g.id = a.agent
             WHERE a.state IN ('active', 'needs_input') AND (?1 IS NULL OR g.session = ?1)
             ORDER BY g.rowid",
            [session],
            |row| {
                Ok(OpenAttempt {
                    attempt: attempt_from_row(row)?,
                    run: row.get(12)?,
                    box_id: row.get(13)?,
                    task: row.get(14)?,
                })
            },
        )?)
    }

    fn events(&self, scope: EventScope<'_>, since: u64) -> Result<Vec<RunEvent>> {
        let (run, box_id, kinds) = match scope {
            EventScope::Run(run, kinds) => {
                self.require_run(run)?;
                let kinds = (!kinds.is_empty())
                    .then(|| {
                        serde_json::to_string(
                            &kinds
                                .iter()
                                .copied()
                                .map(wire::RunEventKind::from)
                                .collect::<Vec<_>>(),
                        )
                    })
                    .transpose()?;
                (Some(run), None, kinds)
            }
            EventScope::Watch(box_id) => (None, box_id, None),
        };
        Ok(rows(
            self.0,
            "SELECT e.seq, e.run_id, r.box_id, e.kind, e.task_id, e.attempt_id, e.summary, e.at
             FROM events e JOIN runs r ON r.id = e.run_id
             WHERE e.seq > ?1 AND (?2 IS NULL OR e.run_id = ?2) AND (?3 IS NULL OR r.box_id = ?3)
               AND (?4 IS NULL OR e.kind IN (SELECT value FROM json_each(?4)))
             ORDER BY e.seq",
            params![i64::try_from(since).unwrap_or(i64::MAX), run, box_id, kinds],
            |row| {
                Ok(RunEvent {
                    seq: row.get(0)?,
                    run: row.get(1)?,
                    box_id: row.get(2)?,
                    kind: enum_column::<wire::RunEventKind>(row, 3)?.into(),
                    task: row.get(4)?,
                    attempt: row.get(5)?,
                    summary: row.get(6)?,
                    at: row.get(7)?,
                })
            },
        )?)
    }

    fn latest_seq(&self) -> Result<u64> {
        Ok(self
            .0
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
                row.get(0)
            })?)
    }
}

impl DagTransaction for DagDb<'_> {
    fn insert_worktree(&self, worktree: &ManagedWorktree) -> Result<()> {
        let (run, task, attempt, creator_box, session) = match &worktree.creator {
            WorktreeCreator::Attempt { run, task, attempt } => {
                (Some(run), Some(task), Some(attempt), None, None)
            }
            WorktreeCreator::View { box_id, session } => {
                (None, None, None, Some(box_id), Some(session))
            }
        };
        self.0.execute(
            "INSERT INTO worktrees (path, box_id, repository, branch, base_commit, created_at,
                                    creator_run, creator_task, creator_attempt, creator_box, creator_session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                worktree.path.as_os_str().as_bytes(),
                worktree.box_id,
                worktree.repository.as_os_str().as_bytes(),
                worktree.branch,
                worktree.base_commit,
                worktree.created_at,
                run,
                task,
                attempt,
                creator_box,
                session,
            ],
        )?;
        Ok(())
    }

    fn remove_worktree(&self, path: &Path) -> Result<()> {
        self.0.execute(
            "DELETE FROM worktrees WHERE path = ?1",
            [path.as_os_str().as_bytes()],
        )?;
        Ok(())
    }

    fn insert_agent(&self, agent: &ManagedAgent) -> Result<()> {
        self.0.execute(
            "INSERT INTO agents (id, box_id, worktree, provider, parent, session, state, label, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                agent.id, agent.box_id, agent.worktree.to_str().context("Worktree path must be UTF-8")?,
                enum_name(wire::Provider::from(agent.provider.clone()))?, agent.parent, agent.session, enum_name(wire::AgentState::from(agent.state))?,
                agent.label, agent.created_at,
            ],
        )?;
        Ok(())
    }

    fn set_agent_exited(&self, id: &str) -> Result<()> {
        self.0
            .execute("UPDATE agents SET state = 'exited' WHERE id = ?1", [id])?;
        Ok(())
    }

    fn insert_run(&self, run: &Run) -> Result<()> {
        self.0.execute(
            "INSERT INTO runs (id, box_id, title, coordinator, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run.id, run.box_id, run.title, serde_json::to_string(&wire::Coordinator::from(run.coordinator.clone()))?, run.created_at],
        )?;
        Ok(())
    }

    fn insert_tasks(&self, run: &str, tasks: &[TaskSpec]) -> Result<()> {
        // All task rows must exist before inserting edges with forward references.
        for task in tasks {
            self.0.execute(
                "INSERT INTO tasks (run_id, id, title, instructions) VALUES (?1, ?2, ?3, ?4)",
                params![run, task.id, task.title, task.instructions],
            )?;
        }
        for task in tasks {
            for dep in &task.deps {
                self.insert_dep(run, &task.id, dep)?;
            }
        }
        Ok(())
    }

    fn insert_dep(&self, run: &str, task: &str, dep: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO deps (run_id, task_id, dep) VALUES (?1, ?2, ?3)
             ON CONFLICT(run_id, task_id, dep) DO NOTHING",
            params![run, task, dep],
        )?;
        Ok(())
    }

    fn insert_attempt(&self, run: &str, task: &str, attempt: &Attempt) -> Result<()> {
        self.0.execute(
            "INSERT INTO attempts (id, run_id, task_id, number, agent, state, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                attempt.id,
                run,
                task,
                attempt.number,
                attempt.agent,
                enum_name(wire::AttemptState::from(attempt.state))?,
                attempt.started_at
            ],
        )?;
        Ok(())
    }

    fn update_attempt(&self, attempt: &Attempt) -> Result<()> {
        self.0.execute(
            "UPDATE attempts SET state = ?2, summary = ?3, blocker = ?4, detail = ?5, ended_at = ?6
             WHERE id = ?1",
            params![
                attempt.id,
                enum_name(wire::AttemptState::from(attempt.state))?,
                attempt.summary,
                attempt.blocker,
                attempt.detail,
                attempt.ended_at
            ],
        )?;
        Ok(())
    }

    fn event(
        &self,
        run: &str,
        kind: RunEventKind,
        task: Option<&str>,
        attempt: Option<&str>,
        summary: Option<&str>,
    ) -> Result<()> {
        self.0.execute(
            "INSERT INTO events (run_id, kind, task_id, attempt_id, summary, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run,
                enum_name(wire::RunEventKind::from(kind))?,
                task,
                attempt,
                summary,
                now()
            ],
        )?;
        Ok(())
    }
}

const WORKTREE_SELECT: &str = "SELECT path, box_id, repository, branch, base_commit, created_at,
                                     creator_run, creator_task, creator_attempt, creator_box, creator_session
                              FROM worktrees";

fn worktree_from_row(row: &Row<'_>) -> rusqlite::Result<ManagedWorktree> {
    let creator = match row.get::<_, Option<String>>(8)? {
        Some(attempt) => WorktreeCreator::Attempt {
            run: row.get(6)?,
            task: row.get(7)?,
            attempt,
        },
        None => WorktreeCreator::View {
            box_id: row.get(9)?,
            session: row.get(10)?,
        },
    };
    Ok(ManagedWorktree {
        path: PathBuf::from(OsString::from_vec(row.get(0)?)),
        box_id: row.get(1)?,
        repository: PathBuf::from(OsString::from_vec(row.get(2)?)),
        branch: row.get(3)?,
        base_commit: row.get(4)?,
        created_at: row.get(5)?,
        creator,
    })
}

const AGENT_SELECT: &str =
    "SELECT a.id, a.box_id, a.worktree, a.provider, a.parent, a.session, p.run_id, p.task_id,
            p.id, a.state, a.label, a.created_at
     FROM agents a LEFT JOIN attempts p ON p.agent = a.id";

fn agent_from_row(row: &Row<'_>) -> rusqlite::Result<ManagedAgent> {
    Ok(ManagedAgent {
        id: row.get(0)?,
        box_id: row.get(1)?,
        worktree: PathBuf::from(row.get::<_, String>(2)?),
        provider: enum_column::<wire::Provider>(row, 3)?.into(),
        parent: row.get(4)?,
        session: row.get(5)?,
        run: row.get(6)?,
        task: row.get(7)?,
        attempt: row.get(8)?,
        state: enum_column::<wire::AgentState>(row, 9)?.into(),
        label: row.get(10)?,
        created_at: row.get(11)?,
    })
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get(0)?,
        box_id: row.get(1)?,
        title: row.get(2)?,
        coordinator: serde_json::from_str::<wire::Coordinator>(&row.get::<_, String>(3)?)
            .map_err(|error| invalid_column(3, error))?
            .into(),
        created_at: row.get(4)?,
        tasks: Vec::new(),
    })
}

fn attempt_from_row(row: &Row<'_>) -> rusqlite::Result<Attempt> {
    Ok(Attempt {
        id: row.get(0)?,
        number: row.get(1)?,
        agent: row.get(2)?,
        session: row.get(3)?,
        worktree: PathBuf::from(row.get::<_, String>(4)?),
        provider: enum_column::<wire::Provider>(row, 5)?.into(),
        state: enum_column::<wire::AttemptState>(row, 6)?.into(),
        summary: row.get(7)?,
        blocker: row.get(8)?,
        detail: row.get(9)?,
        started_at: row.get(10)?,
        ended_at: row.get(11)?,
    })
}

fn rows<T>(
    db: &Connection,
    sql: &str,
    args: impl Params,
    read: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    db.prepare(sql)?.query_map(args, read)?.collect()
}

fn enum_name(value: impl Serialize) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(name) => Ok(name),
        _ => anyhow::bail!("Expected a string enum in the DAG store"),
    }
}

fn enum_column<T: for<'de> Deserialize<'de>>(row: &Row<'_>, column: usize) -> rusqlite::Result<T> {
    T::deserialize(row.get::<_, String>(column)?.into_deserializer())
        .map_err(|error: serde::de::value::Error| invalid_column(column, error))
}

fn invalid_column(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::dag::Provider;
    use std::ffi::OsStr;

    #[test]
    fn migrates_prior_agent_and_attempt_records() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("dag.sqlite3");
        let store = SqliteDagStore::initialize(Connection::open(&path)?)?;
        store.0.lock().unwrap().execute_batch(
            r#"
            DROP TABLE IF EXISTS worktrees;
            INSERT INTO runs VALUES ('run', 'box', 'Old run', '{"kind":"external","label":"old"}', 1);
            INSERT INTO agents VALUES
                ('a', 'box', '/repo', 'codex', NULL, 'session-a', 'exited', 'Codex', 1),
                ('b', 'box', '/repo', 'claude', NULL, 'session-b', 'exited', 'Claude', 1),
                ('c', 'box', '/repo', 'command', NULL, 'session-c', 'exited', 'Command', 1);
            INSERT INTO tasks VALUES ('run', 'task', 'Task', 'Old instructions');
            INSERT INTO attempts VALUES
                ('attempt-a', 'run', 'task', 1, 'a', 'failed', NULL, NULL, NULL, 1, 2),
                ('attempt-b', 'run', 'task', 2, 'b', 'failed', NULL, NULL, NULL, 3, 4),
                ('attempt-c', 'run', 'task', 3, 'c', 'succeeded', NULL, NULL, NULL, 5, 6);
            "#,
        )?;
        drop(store);

        let store = SqliteDagStore::initialize(Connection::open(&path)?)?;
        store.read(|db| {
            assert!(db.list_worktrees(None)?.is_empty());
            let run = db.run("run")?;
            let attempts = &run.tasks[0].attempts;
            assert_eq!(attempts.len(), 3);
            for (attempt, expected) in attempts.iter().zip([
                Provider::Agent("codex".into()),
                Provider::Agent("claude".into()),
                Provider::Command,
            ]) {
                assert_eq!(attempt.provider, expected);
                assert_eq!(db.agent(&attempt.agent)?.provider, expected);
            }
            Ok(())
        })
    }

    #[test]
    fn stores_and_removes_worktree_ownership_records() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("dag.sqlite3");
        let store = SqliteDagStore::initialize(Connection::open(&path)?)?;
        let worker = ManagedWorktree {
            box_id: "worker-box".into(),
            repository: "/repos/worker".into(),
            path: "/worktrees/worker/feature".into(),
            branch: "feature/worker".into(),
            base_commit: "abc123".into(),
            created_at: 10,
            creator: WorktreeCreator::Attempt {
                run: "run".into(),
                task: "task".into(),
                attempt: "attempt".into(),
            },
        };
        let tab = ManagedWorktree {
            box_id: "tab-box".into(),
            repository: OsStr::from_bytes(b"/repos/tab-\xff").into(),
            path: OsStr::from_bytes(b"/worktrees/tab-\xff/feature").into(),
            branch: "feature/tab".into(),
            base_commit: "def456".into(),
            created_at: 20,
            creator: WorktreeCreator::View {
                box_id: "tab-box".into(),
                session: "view-session".into(),
            },
        };

        store.write(|db| {
            db.insert_worktree(&worker)?;
            db.insert_worktree(&tab)
        })?;
        let duplicate_path = ManagedWorktree {
            path: worker.path.clone(),
            ..tab.clone()
        };
        assert!(
            store
                .write(|db| db.insert_worktree(&duplicate_path))
                .is_err()
        );
        drop(store);

        let store = SqliteDagStore::initialize(Connection::open(&path)?)?;
        store.read(|db| {
            assert_eq!(
                db.find_worktree_by_attempt("attempt")?,
                Some(worker.clone())
            );
            assert_eq!(db.find_worktree_by_attempt("unknown")?, None);
            assert_eq!(db.find_worktree_by_path(&tab.path)?, Some(tab.clone()));
            assert_eq!(db.find_worktree_by_path(Path::new("/unknown"))?, None);
            assert_eq!(
                db.list_worktrees(Some("worker-box"))?.as_slice(),
                std::slice::from_ref(&worker)
            );
            assert!(db.list_worktrees(Some("unknown"))?.is_empty());
            assert_eq!(db.list_worktrees(None)?, [worker.clone(), tab.clone()]);
            Ok(())
        })?;

        store.write(|db| db.remove_worktree(&worker.path))?;
        store.read(|db| {
            assert_eq!(db.find_worktree_by_path(&worker.path)?, None);
            assert_eq!(db.find_worktree_by_attempt("attempt")?, None);
            assert_eq!(db.list_worktrees(None)?, [tab]);
            Ok(())
        })
    }
}
