use crate::{persistence::state_directory, wire};
use anyhow::{Context, Result};
use application::dag::{
    DagReader, DagStore, DagTransaction, EventScope, OpenAttempt, QueuedDispatch, now,
};
use domain::{
    dag::{
        AgentStatus, Attempt, Coordinator, ManagedAgent, Run, RunEvent, RunEventKind, Task,
        TaskSpec, TaskState,
    },
    worktree::{ManagedWorktree, WorktreeCreator, WorktreeRequest},
};
use rusqlite::{Connection, OptionalExtension, Params, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::IntoDeserializer};
use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

mod talk;

#[derive(Clone)]
pub(crate) struct SqliteDagStore(Arc<Mutex<Connection>>, Arc<Condvar>);

impl SqliteDagStore {
    pub(crate) fn open() -> Result<Self> {
        Self::initialize(Connection::open(state_directory()?.join("dag.sqlite3"))?)
    }

    fn initialize(db: Connection) -> Result<Self> {
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY, box_id TEXT NOT NULL, title TEXT NOT NULL,
                 coordinator TEXT NOT NULL, created_at INTEGER NOT NULL,
                 archived_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS agents (
                 id TEXT PRIMARY KEY, box_id TEXT NOT NULL, worktree TEXT NOT NULL,
                 provider TEXT NOT NULL, parent TEXT REFERENCES agents(id), session TEXT UNIQUE,
                 state TEXT NOT NULL, label TEXT NOT NULL, created_at INTEGER NOT NULL,
                 model TEXT
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
             );
             CREATE TABLE IF NOT EXISTS stopping_sessions (session TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS queued_dispatches (
                 id TEXT NOT NULL UNIQUE,
                 run_id TEXT NOT NULL, task_id TEXT NOT NULL, provider TEXT NOT NULL,
                 worktree TEXT NOT NULL, allow_main_checkout INTEGER NOT NULL,
                 retry INTEGER NOT NULL, args TEXT NOT NULL, caller TEXT,
                 queued_at INTEGER NOT NULL,
                 PRIMARY KEY (run_id, task_id),
                 FOREIGN KEY (run_id, task_id) REFERENCES tasks(run_id, id)
             );",
        )?;
        let has_model: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('agents') WHERE name = 'model')",
            [],
            |row| row.get(0),
        )?;
        if !has_model {
            db.execute("ALTER TABLE agents ADD COLUMN model TEXT", [])?;
        }
        let has_archived_at: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runs') WHERE name = 'archived_at')",
            [],
            |row| row.get(0),
        )?;
        if !has_archived_at {
            db.execute("ALTER TABLE runs ADD COLUMN archived_at INTEGER", [])?;
        }
        let has_coordinator_session: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runs')
                 WHERE name = 'coordinator_session')",
            [],
            |row| row.get(0),
        )?;
        if !has_coordinator_session {
            // The session the coordinating agent held when it created the run. A later
            // registration of that session, after a restart, still coordinates the run.
            db.execute("ALTER TABLE runs ADD COLUMN coordinator_session TEXT", [])?;
        }
        let has_paused: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('agents') WHERE name = 'paused')",
            [],
            |row| row.get(0),
        )?;
        if !has_paused {
            db.execute(
                "ALTER TABLE agents ADD COLUMN paused INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        let has_paused_at: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runs') WHERE name = 'paused_at')",
            [],
            |row| row.get(0),
        )?;
        if !has_paused_at {
            db.execute("ALTER TABLE runs ADD COLUMN paused_at INTEGER", [])?;
        }
        talk::migrate(&db)?;
        Ok(Self(Arc::new(Mutex::new(db)), Arc::new(Condvar::new())))
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
                "SELECT id, box_id, title, coordinator, created_at, archived_at, paused_at FROM runs WHERE id = ?1",
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
                    queued_at: None,
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
        let mut queued: HashMap<String, u64> = rows(
            self.0,
            "SELECT task_id, queued_at FROM queued_dispatches WHERE run_id = ?1",
            [&run.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
        .into_iter()
        .collect();
        let mut attempts = HashMap::<String, Vec<Attempt>>::new();
        for (task, attempt) in rows(
            self.0,
            "SELECT a.id, a.number, a.agent, g.session, g.worktree, g.provider,
                    a.state, a.summary, a.blocker, a.detail, a.started_at, a.ended_at,
                    g.model, a.task_id, g.paused
             FROM attempts a JOIN agents g ON g.id = a.agent
             WHERE a.run_id = ?1 ORDER BY a.number",
            [&run.id],
            |row| {
                let mut attempt = attempt_from_row(row)?;
                attempt.paused = row.get(14)?;
                Ok((row.get(13)?, attempt))
            },
        )? {
            attempts.entry(task).or_default().push(attempt);
        }
        run.tasks = tasks
            .into_iter()
            .map(|mut task| {
                task.deps = deps.remove(&task.id).unwrap_or_default();
                task.attempts = attempts.remove(&task.id).unwrap_or_default();
                task.queued_at = queued.remove(&task.id);
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

    fn worktree_agents(&self, worktree: &Path) -> Result<Vec<ManagedAgent>> {
        // Agent worktrees are stored as UTF-8, so no agent can match any other path.
        let Some(worktree) = worktree.to_str() else {
            return Ok(Vec::new());
        };
        Ok(rows(
            self.0,
            &format!("{AGENT_SELECT} WHERE a.worktree = ?1 ORDER BY a.rowid"),
            [worktree],
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
            "SELECT id, box_id, title, coordinator, created_at, archived_at, paused_at FROM runs
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
                    g.model, a.run_id, g.box_id, a.task_id
             FROM attempts a JOIN agents g ON g.id = a.agent
             WHERE a.state IN ('active', 'needs_input') AND (?1 IS NULL OR g.session = ?1)
             ORDER BY g.rowid",
            [session],
            |row| {
                Ok(OpenAttempt {
                    attempt: attempt_from_row(row)?,
                    run: row.get(13)?,
                    box_id: row.get(14)?,
                    task: row.get(15)?,
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

    fn queued_dispatches(&self) -> Result<Vec<QueuedDispatch>> {
        rows(
            self.0,
            "SELECT run_id, task_id, provider, worktree, allow_main_checkout, retry, args, caller,
                    queued_at, id
             FROM queued_dispatches ORDER BY queued_at, rowid",
            [],
            |row| {
                let worktree: wire::WorktreeRequest =
                    serde_json::from_str(&row.get::<_, String>(3)?)
                        .map_err(|error| invalid_column(3, error))?;
                Ok(QueuedDispatch {
                    id: row.get(9)?,
                    run: row.get(0)?,
                    task: row.get(1)?,
                    provider: enum_column::<wire::Provider>(row, 2)?.into(),
                    worktree: worktree.into_domain(row.get(4)?),
                    retry: row.get(5)?,
                    args: serde_json::from_str(&row.get::<_, String>(6)?)
                        .map_err(|error| invalid_column(6, error))?,
                    caller: row.get(7)?,
                    queued_at: row.get(8)?,
                })
            },
        )
        .map_err(Into::into)
    }

    fn stopping_sessions(&self) -> Result<BTreeSet<String>> {
        Ok(
            rows(self.0, "SELECT session FROM stopping_sessions", [], |row| {
                row.get(0)
            })?
            .into_iter()
            .collect(),
        )
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

    fn insert_queued(&self, queued: &QueuedDispatch) -> Result<()> {
        let (worktree, allow_main_checkout) = match queued.worktree.clone() {
            WorktreeRequest::New { branch, base } => {
                (wire::WorktreeRequest::New { branch, base }, false)
            }
            WorktreeRequest::RetryManaged => (wire::WorktreeRequest::RetryManaged, false),
            WorktreeRequest::Existing {
                path,
                allow_main_checkout,
            } => (wire::WorktreeRequest::Existing(path), allow_main_checkout),
        };
        self.0.execute(
            "INSERT INTO queued_dispatches
                 (run_id, task_id, provider, worktree, allow_main_checkout, retry, args, caller,
                  queued_at, id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                queued.run,
                queued.task,
                enum_name(wire::Provider::from(queued.provider.clone()))?,
                serde_json::to_string(&worktree)?,
                allow_main_checkout,
                queued.retry,
                serde_json::to_string(&queued.args)?,
                queued.caller,
                queued.queued_at,
                queued.id,
            ],
        )?;
        Ok(())
    }

    fn remove_queued(&self, id: &str) -> Result<bool> {
        Ok(self
            .0
            .execute("DELETE FROM queued_dispatches WHERE id = ?1", [id])?
            > 0)
    }

    fn insert_agent(&self, agent: &ManagedAgent) -> Result<()> {
        self.0.execute(
            "INSERT INTO agents (id, box_id, worktree, provider, parent, session, state, label, created_at, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                agent.id, agent.box_id, agent.worktree.to_str().context("Worktree path must be UTF-8")?,
                enum_name(wire::Provider::from(agent.provider.clone()))?, agent.parent, agent.session, enum_name(wire::AgentState::from(agent.state))?,
                agent.label, agent.created_at, agent.model,
            ],
        )?;
        Ok(())
    }

    fn set_agent_exited(&self, id: &str) -> Result<()> {
        self.0.execute(
            "UPDATE agents SET state = 'exited', paused = 0 WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    fn set_agent_paused(&self, id: &str, paused: bool) -> Result<()> {
        self.0.execute(
            "UPDATE agents SET paused = ?2 WHERE id = ?1",
            params![id, paused],
        )?;
        Ok(())
    }

    fn insert_stopping(&self, session: &str) -> Result<bool> {
        Ok(self.0.execute(
            "INSERT OR IGNORE INTO stopping_sessions (session) VALUES (?1)",
            [session],
        )? > 0)
    }

    fn remove_stopping(&self, session: &str) -> Result<bool> {
        Ok(self.0.execute(
            "DELETE FROM stopping_sessions WHERE session = ?1",
            [session],
        )? > 0)
    }

    fn set_run_paused(&self, id: &str, paused_at: Option<u64>) -> Result<()> {
        self.0.execute(
            "UPDATE runs SET paused_at = ?2 WHERE id = ?1",
            params![id, paused_at],
        )?;
        Ok(())
    }

    fn adopt_coordinated_runs(&self, session: &str, agent: &str) -> Result<()> {
        let coordinator = serde_json::to_string(&wire::Coordinator::from(Coordinator::Agent {
            agent: agent.into(),
        }))?;
        self.0.execute(
            "UPDATE runs SET coordinator = ?2
             WHERE coordinator_session = ?1 AND json_extract(coordinator, '$.kind') = 'agent'",
            params![session, coordinator],
        )?;
        Ok(())
    }

    fn retire_agent(&self, id: &str) -> Result<()> {
        self.0.execute(
            "UPDATE agents SET state = 'exited', paused = 0, session = NULL WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    fn set_agent_model(&self, id: &str, model: Option<&str>) -> Result<()> {
        self.0.execute(
            "UPDATE agents SET model = ?2 WHERE id = ?1",
            params![id, model],
        )?;
        Ok(())
    }

    fn insert_run(&self, run: &Run) -> Result<()> {
        self.0.execute(
            "INSERT INTO runs (id, box_id, title, coordinator, created_at, archived_at, coordinator_session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, (SELECT session FROM agents WHERE id = ?7))",
            params![run.id, run.box_id, run.title, serde_json::to_string(&wire::Coordinator::from(run.coordinator.clone()))?, run.created_at, run.archived_at, match &run.coordinator { Coordinator::Agent { agent } => Some(agent.as_str()), Coordinator::External { .. } => None }],
        )?;
        Ok(())
    }

    fn set_run_archived(&self, id: &str, archived_at: Option<u64>) -> Result<()> {
        let updated = self.0.execute(
            "UPDATE runs SET archived_at = ?2 WHERE id = ?1",
            params![id, archived_at],
        )?;
        anyhow::ensure!(updated == 1, "Unknown run {id:?}");
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
            p.id, a.state, a.label, a.created_at, a.model, p.state, a.paused
     FROM agents a LEFT JOIN attempts p ON p.agent = a.id";

fn agent_from_row(row: &Row<'_>) -> rusqlite::Result<ManagedAgent> {
    let state = enum_column::<wire::AgentState>(row, 9)?.into();
    let paused: bool = row.get(14)?;
    let attempt_state = row
        .get::<_, Option<String>>(13)?
        .map(|_| enum_column::<wire::AttemptState>(row, 13).map(Into::into))
        .transpose()?;
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
        state,
        paused,
        status: AgentStatus::derive(state, paused, attempt_state),
        label: row.get(10)?,
        created_at: row.get(11)?,
        model: row.get(12)?,
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
        archived_at: row.get(5)?,
        paused_at: row.get(6)?,
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
        model: row.get(12)?,
        paused: false,
        artifacts: Vec::new(),
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
    use domain::dag::{AgentState, Provider};
    use std::ffi::OsStr;

    #[test]
    fn migrates_prior_agent_and_attempt_records() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("dag.sqlite3");
        let db = Connection::open(&path)?;
        db.execute_batch(
            "CREATE TABLE agents (
                id TEXT PRIMARY KEY, box_id TEXT NOT NULL, worktree TEXT NOT NULL,
                provider TEXT NOT NULL, parent TEXT REFERENCES agents(id), session TEXT UNIQUE,
                state TEXT NOT NULL, label TEXT NOT NULL, created_at INTEGER NOT NULL
             );
             INSERT INTO agents VALUES
                ('a', 'box', '/repo', 'codex', NULL, 'session-a', 'exited', 'Codex', 1),
                ('b', 'box', '/repo', 'claude', NULL, 'session-b', 'exited', 'Claude', 1),
                ('c', 'box', '/repo', 'command', NULL, 'session-c', 'exited', 'Command', 1);",
        )?;
        let store = SqliteDagStore::initialize(db)?;
        store.0.lock().unwrap().execute_batch(
            r#"
            DROP TABLE IF EXISTS worktrees;
            INSERT INTO runs (id, box_id, title, coordinator, created_at)
                VALUES ('run', 'box', 'Old run', '{"kind":"external","label":"old"}', 1);
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
                assert_eq!(attempt.model, None);
                let agent = db.agent(&attempt.agent)?;
                assert_eq!(agent.provider, expected);
                assert_eq!(agent.model, None);
            }
            Ok(())
        })
    }

    #[test]
    fn migrates_runs_without_archived_at_and_stores_it() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("dag.sqlite3");
        let db = Connection::open(&path)?;
        db.execute_batch(
            r#"CREATE TABLE runs (
                 id TEXT PRIMARY KEY, box_id TEXT NOT NULL, title TEXT NOT NULL,
                 coordinator TEXT NOT NULL, created_at INTEGER NOT NULL
             );
             INSERT INTO runs VALUES ('run', 'box', 'Old run', '{"kind":"external","label":"old"}', 1);"#,
        )?;
        let store = SqliteDagStore::initialize(db)?;
        assert_eq!(store.read(|db| db.run("run"))?.archived_at, None);
        store.write(|db| db.set_run_archived("run", Some(42)))?;
        drop(store);

        let store = SqliteDagStore::initialize(Connection::open(&path)?)?;
        store.read(|db| {
            assert_eq!(db.run("run")?.archived_at, Some(42));
            assert_eq!(db.list_runs(Some("box"))?[0].archived_at, Some(42));
            Ok(())
        })?;
        store.write(|db| db.set_run_archived("run", None))?;
        assert_eq!(store.read(|db| db.run("run"))?.archived_at, None);
        assert!(
            store
                .write(|db| db.set_run_archived("missing", Some(1)))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn a_coordinator_that_registers_again_coordinates_its_runs_and_pauses_persist() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store =
            SqliteDagStore::initialize(Connection::open(directory.path().join("dag.sqlite3"))?)?;
        let agent = |id: &str| ManagedAgent {
            id: id.into(),
            box_id: "box".into(),
            worktree: "/worktree".into(),
            provider: Provider::Agent("claude".into()),
            model: None,
            parent: None,
            session: Some("session".into()),
            run: None,
            task: None,
            attempt: None,
            state: AgentState::Running,
            paused: false,
            status: AgentStatus::Idle,
            label: id.into(),
            created_at: 1,
        };
        let run = Run {
            id: "run".into(),
            box_id: "box".into(),
            title: "Run".into(),
            coordinator: Coordinator::Agent {
                agent: "before".into(),
            },
            created_at: 1,
            archived_at: None,
            paused_at: None,
            tasks: Vec::new(),
        };
        store.write(|db| {
            db.insert_agent(&agent("before"))?;
            db.insert_run(&run)?;
            // A restart retires the old identity, and the session registers anew.
            db.retire_agent("before")?;
            db.insert_agent(&agent("after"))?;
            db.adopt_coordinated_runs("session", "after")?;
            db.set_agent_paused("after", true)?;
            db.set_run_paused("run", Some(7))
        })?;
        store.read(|db| {
            let run = db.run("run")?;
            assert_eq!(
                run.coordinator,
                Coordinator::Agent {
                    agent: "after".into()
                }
            );
            assert_eq!(run.paused_at, Some(7));
            let after = db.agent("after")?;
            assert!(after.paused);
            assert_eq!(after.status, AgentStatus::Paused);
            Ok(())
        })?;
        store.write(|db| db.set_agent_exited("after"))?;
        assert!(!store.read(|db| db.agent("after"))?.paused);
        Ok(())
    }

    #[test]
    fn finds_agents_by_exact_worktree() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store =
            SqliteDagStore::initialize(Connection::open(directory.path().join("dag.sqlite3"))?)?;
        let agent = |id: &str, worktree: &str| ManagedAgent {
            id: id.into(),
            box_id: "box".into(),
            worktree: worktree.into(),
            provider: Provider::Command,
            model: None,
            parent: None,
            session: None,
            run: None,
            task: None,
            attempt: None,
            state: AgentState::Running,
            paused: false,
            status: AgentStatus::Idle,
            label: id.into(),
            created_at: 1,
        };
        let first = agent("first", "/worktrees/feature");
        let second = agent("second", "/worktrees/feature");
        store.write(|db| {
            db.insert_agent(&first)?;
            db.insert_agent(&agent("nested", "/worktrees/feature/sub"))?;
            db.insert_agent(&second)
        })?;
        store.read(|db| {
            assert_eq!(
                db.worktree_agents(Path::new("/worktrees/feature"))?,
                [first.clone(), second.clone()]
            );
            assert!(db.worktree_agents(Path::new("/worktrees"))?.is_empty());
            let invalid = OsStr::from_bytes(b"/worktrees/\xff");
            assert!(db.worktree_agents(Path::new(invalid))?.is_empty());
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
