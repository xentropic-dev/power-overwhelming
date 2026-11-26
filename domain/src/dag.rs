use crate::agent::{AgentId, display_name};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    path::PathBuf,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSpec {
    /// Stable ID, unique within the run, matching `[a-z0-9][a-z0-9_-]{0,63}`.
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub deps: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    Waiting,
    Ready,
    Working,
    NeedsInput,
    Completed,
    Failed,
    Interrupted,
}

/// A run's overall progress, derived from its task states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStatus {
    /// Some task is ready, working or waiting on input, or the run has no tasks yet.
    Active,
    /// Every task completed.
    Completed,
    /// Nothing is open, but not every task completed.
    Stalled,
    /// Archived explicitly, whatever its task states.
    Archived,
}

impl RunStatus {
    /// Finished runs are hidden from default listings.
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Archived)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptState {
    Active,
    NeedsInput,
    Succeeded,
    Failed,
    Interrupted,
}

impl AttemptState {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Active | Self::NeedsInput)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportStatus {
    Success,
    Failed,
    NeedsInput,
    Working,
}

/// A saved box as the daemon sees it in the box registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoxInfo {
    pub id: String,
    /// Folder name of the repository, for display and `--box` selection.
    pub name: String,
    pub repository: PathBuf,
    pub checkout: PathBuf,
    pub shortcut: Option<u8>,
    /// False when the box is detached from the strip.
    pub open: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Provider {
    Agent(AgentId),
    /// Runs the given argv directly. Used for deterministic worker fixtures.
    Command,
}

impl Provider {
    pub fn name(&self) -> &str {
        match self {
            Self::Command => "Command",
            Self::Agent(id) => display_name(id.as_str()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    Running,
    Exited,
}

/// What an agent is doing, for display. Derived from the agent's state and its attempt when
/// agents are read; never persisted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStatus {
    /// Its attempt is active.
    Working,
    NeedsInput,
    /// Its attempt succeeded and its session is still alive.
    Done,
    /// Its attempt failed or was interrupted and its session is still alive.
    Failed,
    /// Running without an attempt, such as a coordinator.
    Idle,
    Exited,
}

impl AgentStatus {
    /// Finished agents are hidden from default listings.
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Exited | Self::Done)
    }

    pub fn derive(state: AgentState, attempt: Option<AttemptState>) -> Self {
        match (state, attempt) {
            (AgentState::Exited, _) => Self::Exited,
            (AgentState::Running, None) => Self::Idle,
            (AgentState::Running, Some(AttemptState::Active)) => Self::Working,
            (AgentState::Running, Some(AttemptState::NeedsInput)) => Self::NeedsInput,
            (AgentState::Running, Some(AttemptState::Succeeded)) => Self::Done,
            (AgentState::Running, Some(AttemptState::Failed | AttemptState::Interrupted)) => {
                Self::Failed
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsInput => "needs input",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Idle => "idle",
            Self::Exited => "exited",
        }
    }
}

/// A coding agent or fixture process managed by Power. Tab attachment is tracked by the
/// desktop, independently of this record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedAgent {
    pub id: String,
    pub box_id: String,
    pub worktree: PathBuf,
    pub provider: Provider,
    pub model: Option<String>,
    /// The delegating Power agent, if any.
    pub parent: Option<String>,
    /// The daemon terminal session, while it has one.
    pub session: Option<String>,
    pub run: Option<String>,
    pub task: Option<String>,
    pub attempt: Option<String>,
    pub state: AgentState,
    /// Derived from `state` and the attempt's state when read; never persisted.
    pub status: AgentStatus,
    /// Short label, the task title for DAG workers.
    pub label: String,
    /// Unix seconds.
    pub created_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Coordinator {
    /// A Power agent, such as a managed Claude Code or Codex session in a box.
    Agent { agent: String },
    /// A coordinator outside Power, such as a Herdr pane. The label is opaque.
    External { label: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attempt {
    pub id: String,
    /// 1 for the first dispatch of a task.
    pub number: u32,
    pub agent: String,
    pub session: Option<String>,
    pub worktree: PathBuf,
    pub provider: Provider,
    pub model: Option<String>,
    pub state: AttemptState,
    /// Result summary from the worker's final report.
    pub summary: Option<String>,
    /// Outstanding request for input, while state is `needs_input`.
    pub blocker: Option<String>,
    /// Daemon-supplied context, such as why an attempt was interrupted.
    pub detail: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    /// Artifacts published from the attempt's session, in publish order, newest last.
    /// Derived from the artifact store when a run is read; never persisted with the run.
    pub artifacts: Vec<AttemptArtifact>,
}

/// An artifact published from an attempt's session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptArtifact {
    pub id: String,
    pub title: String,
}

impl AttemptArtifact {
    /// The `pwr://artifact/<id>` link that opens this artifact.
    pub fn link(&self) -> String {
        format!("{}{}", crate::artifact::URI_PREFIX, self.id)
    }
}

impl Attempt {
    pub fn interrupt(&mut self, detail: &str, ended_at: u64) -> bool {
        if !self.state.is_open() {
            return false;
        }
        self.state = AttemptState::Interrupted;
        self.blocker = None;
        self.detail = Some(detail.into());
        self.ended_at = Some(ended_at);
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub deps: Vec<String>,
    /// Derived by the daemon from persisted attempts and prerequisite states.
    pub state: TaskState,
    pub current_attempt: Option<String>,
    /// Oldest first.
    pub attempts: Vec<Attempt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Run {
    pub id: String,
    pub box_id: String,
    pub title: String,
    pub coordinator: Coordinator,
    pub created_at: u64,
    /// Unix seconds, while the run is archived.
    pub archived_at: Option<u64>,
    /// In insertion order.
    pub tasks: Vec<Task>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunEventKind {
    RunCreated,
    TaskAdded,
    TaskReady,
    AttemptStarted,
    AgentModelChanged,
    AttemptSucceeded,
    AttemptFailed,
    NeedsInput,
    InputResolved,
    /// The worker exited without a result, or its session could not be recovered.
    AttemptInterrupted,
    /// A report from an attempt that is no longer current. Recorded, not applied.
    StaleReport,
    RunArchived,
    RunUnarchived,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunEvent {
    /// Daemon-wide, strictly increasing, persisted.
    pub seq: u64,
    pub run: String,
    pub box_id: String,
    pub kind: RunEventKind,
    pub task: Option<String>,
    pub attempt: Option<String>,
    /// Result summary, blocker text, interruption context, or updated model name.
    pub summary: Option<String>,
    pub at: u64,
}

/// A box with at least one outstanding request for input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoxAttention {
    pub box_id: String,
    /// Sessions of workers that need input, for tab highlights.
    pub sessions: Vec<String>,
    /// Agents that need input, including those without a session view.
    pub agents: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum GraphError {
    InvalidId(String),
    DuplicateTask(String),
    UnknownDependency { task: String, dep: String },
    SelfDependency(String),
    Cycle,
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(id) => {
                write!(f, "Invalid task ID {id:?}. Use [a-z0-9][a-z0-9_-]{{0,63}}")
            }
            Self::DuplicateTask(id) => write!(f, "Duplicate task ID {id:?}"),
            Self::UnknownDependency { task, dep } => {
                write!(f, "Task {task:?} depends on unknown task {dep:?}")
            }
            Self::SelfDependency(id) => write!(f, "Task {id:?} cannot depend on itself"),
            Self::Cycle => write!(f, "Task dependencies contain a cycle"),
        }
    }
}

impl std::error::Error for GraphError {}

pub fn valid_task_id(id: &str) -> bool {
    let is_alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    id.len() <= 64
        && id
            .as_bytes()
            .first()
            .is_some_and(|&byte| is_alphanumeric(byte))
        && id
            .bytes()
            .all(|byte| is_alphanumeric(byte) || matches!(byte, b'_' | b'-'))
}

/// Batch references can point forward and can refer to tasks already in the run.
pub fn validate_batch(existing: &[TaskSpec], batch: &[TaskSpec]) -> Result<(), GraphError> {
    let mut remaining = HashMap::new();
    for task in existing.iter().chain(batch) {
        let id = task.id.as_str();
        if !valid_task_id(id) {
            return Err(GraphError::InvalidId(id.into()));
        }
        if remaining.insert(id, 0).is_some() {
            return Err(GraphError::DuplicateTask(id.into()));
        }
    }

    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in existing.iter().chain(batch) {
        let id = task.id.as_str();
        let mut deps = HashSet::new();
        for dep in &task.deps {
            if id == dep {
                return Err(GraphError::SelfDependency(id.into()));
            }
            if !remaining.contains_key(dep.as_str()) {
                return Err(GraphError::UnknownDependency {
                    task: id.into(),
                    dep: dep.clone(),
                });
            }
            if deps.insert(dep) {
                dependents.entry(dep).or_default().push(id);
            }
        }
        remaining.insert(id, deps.len());
    }

    let mut ready: VecDeque<_> = remaining
        .iter()
        .filter_map(|(&id, &count)| (count == 0).then_some(id))
        .collect();
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        if let Some(tasks) = dependents.get(id) {
            for &task in tasks {
                let count = remaining.get_mut(task).unwrap();
                *count -= 1;
                if *count == 0 {
                    ready.push_back(task);
                }
            }
        }
    }
    if visited == remaining.len() {
        Ok(())
    } else {
        Err(GraphError::Cycle)
    }
}

pub fn task_state(
    latest: Option<AttemptState>,
    prerequisites: impl IntoIterator<Item = Option<AttemptState>>,
) -> TaskState {
    match latest {
        Some(AttemptState::Active) => TaskState::Working,
        Some(AttemptState::NeedsInput) => TaskState::NeedsInput,
        Some(AttemptState::Succeeded) => TaskState::Completed,
        Some(AttemptState::Failed) => TaskState::Failed,
        Some(AttemptState::Interrupted) => TaskState::Interrupted,
        None if prerequisites
            .into_iter()
            .all(|state| state == Some(AttemptState::Succeeded)) =>
        {
            TaskState::Ready
        }
        None => TaskState::Waiting,
    }
}

impl Run {
    pub fn derive_task_states(&mut self) {
        let latest: HashMap<_, _> = self
            .tasks
            .iter()
            .filter_map(|task| Some((task.id.clone(), task.attempts.last()?.state)))
            .collect();
        for task in &mut self.tasks {
            task.current_attempt = task.attempts.last().map(|attempt| attempt.id.clone());
            task.state = task_state(
                latest.get(&task.id).copied(),
                task.deps.iter().map(|dep| latest.get(dep).copied()),
            );
        }
    }

    /// Requires `derive_task_states` to have run. An archived run is `Archived` whatever its
    /// task states.
    pub fn status(&self) -> RunStatus {
        if self.archived_at.is_some() {
            return RunStatus::Archived;
        }
        run_status(self.tasks.iter().map(|task| task.state))
    }

    /// Attempts that are still active or waiting on input.
    pub fn open_attempts(&self) -> impl Iterator<Item = &Attempt> {
        self.tasks
            .iter()
            .flat_map(|task| &task.attempts)
            .filter(|attempt| attempt.state.is_open())
    }

    pub fn completed_tasks(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.state == TaskState::Completed)
            .count()
    }
}

pub fn run_status(states: impl IntoIterator<Item = TaskState>) -> RunStatus {
    let mut any = false;
    let mut all_completed = true;
    for state in states {
        any = true;
        match state {
            TaskState::Ready | TaskState::Working | TaskState::NeedsInput => {
                return RunStatus::Active;
            }
            TaskState::Completed => {}
            TaskState::Waiting | TaskState::Failed | TaskState::Interrupted => {
                all_completed = false;
            }
        }
    }
    match (any, all_completed) {
        (false, _) => RunStatus::Active,
        (true, true) => RunStatus::Completed,
        (true, false) => RunStatus::Stalled,
    }
}

pub fn can_dispatch(state: TaskState, retry: bool) -> bool {
    state == TaskState::Ready
        || (retry && matches!(state, TaskState::Failed | TaskState::Interrupted))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportTransition {
    Stale,
    AlreadyReported,
    Unchanged,
    Update {
        state: AttemptState,
        resolves_input: bool,
    },
}

/// The caller has already checked that the reported attempt belongs to the task.
pub fn report_transition(
    current: bool,
    previous: AttemptState,
    status: ReportStatus,
) -> ReportTransition {
    use AttemptState as A;
    use ReportStatus as S;
    if !current {
        return ReportTransition::Stale;
    }
    if !previous.is_open() {
        return ReportTransition::AlreadyReported;
    }
    let state = match status {
        S::Success => A::Succeeded,
        S::Failed => A::Failed,
        S::NeedsInput => A::NeedsInput,
        S::Working if previous == A::Active => return ReportTransition::Unchanged,
        S::Working => A::Active,
    };
    ReportTransition::Update {
        state,
        resolves_input: previous == A::NeedsInput && state != A::NeedsInput,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, deps: &[&str]) -> TaskSpec {
        TaskSpec {
            id: id.into(),
            title: id.into(),
            instructions: String::new(),
            deps: deps.iter().map(|dep| (*dep).into()).collect(),
        }
    }

    #[test]
    fn id_syntax_and_boundaries() {
        for id in ["a", "0", "5_a-b09", &"a".repeat(64)] {
            assert!(valid_task_id(id), "{id:?}");
        }
        for id in [
            "",
            "-",
            "_a",
            "A",
            "aB",
            "a.b",
            "a b",
            "a\n",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!valid_task_id(id), "{id:?}");
        }
    }

    #[test]
    fn forward_references_and_existing_dependencies() {
        let existing = [task("base", &[])];
        let batch = [
            task("last", &["left", "right"]),
            task("right", &["base", "base"]),
            task("left", &[]),
        ];
        assert_eq!(validate_batch(&existing, &batch), Ok(()));
        assert_eq!(validate_batch(&[], &[]), Ok(()));
    }

    #[test]
    fn rejects_invalid_ids_duplicates_missing_references_and_cycles() {
        let cases = [
            (vec![task("A", &[])], GraphError::InvalidId("A".into())),
            (
                vec![task("a", &[]), task("a", &[])],
                GraphError::DuplicateTask("a".into()),
            ),
            (
                vec![task("a", &["missing"])],
                GraphError::UnknownDependency {
                    task: "a".into(),
                    dep: "missing".into(),
                },
            ),
            (
                vec![task("a", &["a"])],
                GraphError::SelfDependency("a".into()),
            ),
            (
                vec![task("a", &["b"]), task("b", &["c"]), task("c", &["a"])],
                GraphError::Cycle,
            ),
        ];
        for (batch, expected) in cases {
            assert_eq!(validate_batch(&[], &batch), Err(expected));
        }
        assert_eq!(
            validate_batch(&[task("a", &[])], &[task("a", &[])]),
            Err(GraphError::DuplicateTask("a".into()))
        );
    }

    #[test]
    fn readiness_and_dispatch_follow_attempt_state() {
        use AttemptState as A;
        use TaskState as T;
        assert_eq!(task_state(None, []), T::Ready);
        assert_eq!(
            task_state(None, [Some(A::Succeeded), Some(A::Succeeded)]),
            T::Ready
        );
        for dep in [
            None,
            Some(A::Active),
            Some(A::NeedsInput),
            Some(A::Failed),
            Some(A::Interrupted),
        ] {
            assert_eq!(task_state(None, [Some(A::Succeeded), dep]), T::Waiting);
        }
        for (attempt, expected) in [
            (A::Active, T::Working),
            (A::NeedsInput, T::NeedsInput),
            (A::Succeeded, T::Completed),
            (A::Failed, T::Failed),
            (A::Interrupted, T::Interrupted),
        ] {
            assert_eq!(task_state(Some(attempt), [None]), expected);
            assert!(!can_dispatch(expected, false));
            assert_eq!(
                can_dispatch(expected, true),
                matches!(expected, T::Failed | T::Interrupted)
            );
        }
        assert!(can_dispatch(T::Ready, false));
        assert!(can_dispatch(T::Ready, true));
        assert!(!can_dispatch(T::Waiting, true));
    }

    #[test]
    fn run_status_follows_task_states() {
        use RunStatus as R;
        use TaskState as T;
        assert_eq!(run_status([]), R::Active);
        for open in [T::Ready, T::Working, T::NeedsInput] {
            assert_eq!(run_status([T::Completed, T::Failed, open]), R::Active);
        }
        assert_eq!(run_status([T::Completed, T::Completed]), R::Completed);
        for closed in [T::Waiting, T::Failed, T::Interrupted] {
            assert_eq!(run_status([T::Completed, closed]), R::Stalled);
            assert_eq!(run_status([closed]), R::Stalled);
        }
    }

    #[test]
    fn archived_runs_are_archived_and_finished() {
        let mut run = Run {
            id: "run".into(),
            box_id: "box".into(),
            title: "Run".into(),
            coordinator: Coordinator::External { label: "x".into() },
            created_at: 1,
            archived_at: None,
            tasks: Vec::new(),
        };
        assert_eq!(run.status(), RunStatus::Active);
        assert!(!run.status().is_finished());
        run.archived_at = Some(2);
        assert_eq!(run.status(), RunStatus::Archived);
        assert!(run.status().is_finished());
        assert!(RunStatus::Completed.is_finished());
        assert!(!RunStatus::Stalled.is_finished());
    }

    #[test]
    fn stale_reports_take_precedence_over_replay_and_terminal_rejection() {
        use AttemptState as A;
        use ReportStatus as S;
        use ReportTransition as R;
        for terminal in [A::Succeeded, A::Failed, A::Interrupted] {
            for status in [S::Success, S::Failed, S::NeedsInput, S::Working] {
                assert_eq!(report_transition(false, terminal, status), R::Stale);
                assert_eq!(
                    report_transition(true, terminal, status),
                    R::AlreadyReported
                );
            }
        }
        assert_eq!(report_transition(false, A::Active, S::NeedsInput), R::Stale);
    }

    #[test]
    fn input_resolution_and_final_reports() {
        use AttemptState as A;
        use ReportStatus as S;
        use ReportTransition as R;
        assert_eq!(report_transition(true, A::Active, S::Working), R::Unchanged);
        for (status, state) in [
            (S::Success, A::Succeeded),
            (S::Failed, A::Failed),
            (S::NeedsInput, A::NeedsInput),
            (S::Working, A::Active),
        ] {
            assert_eq!(
                report_transition(true, A::NeedsInput, status),
                R::Update {
                    state,
                    resolves_input: state != A::NeedsInput
                }
            );
            if status != S::Working {
                assert_eq!(
                    report_transition(true, A::Active, status),
                    R::Update {
                        state,
                        resolves_input: false
                    }
                );
            }
        }
    }
}
