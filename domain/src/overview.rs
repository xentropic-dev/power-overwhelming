//! A high-level picture of every box, run, model and the machine, for the dashboard.

use crate::dag::{AttemptState, Run, RunStatus, TaskState};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct Overview {
    pub boxes: Vec<BoxOverview>,
    /// Active and stalled runs, live ones first.
    pub runs: Vec<RunOverview>,
    /// One entry per provider and model in use.
    pub usage: Vec<ModelUsage>,
    pub system: SystemStats,
    pub totals: Totals,
    /// Unix seconds when this overview was taken.
    pub at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoxOverview {
    pub id: String,
    pub name: String,
    /// Live terminals that belong to the box, attached or not.
    pub terminals: usize,
    /// Terminals open as a tab in the box.
    pub attached: usize,
    pub agents_working: usize,
    pub runs_active: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOverview {
    pub run: String,
    pub box_id: String,
    pub title: String,
    pub completed: usize,
    pub total: usize,
    /// Open attempts.
    pub workers: usize,
    /// Seconds until every task should be done, or `None` while unknown or paused.
    pub eta_secs: Option<u64>,
    pub status: RunStatus,
    pub paused: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Totals {
    pub agents_working: usize,
    pub terminals: usize,
    /// Terminals open as a tab in some box. The rest still run in the daemon, detached.
    pub attached: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SystemStats {
    /// Busy share of all CPUs since the last sample, from 0 to 100. `None` when unknown.
    pub cpu_percent: Option<f32>,
    /// Bytes. Both are 0 when unknown.
    pub mem_used: u64,
    pub mem_total: u64,
}

/// A provider and model that some live agent runs. The model is `None` until the agent
/// reports it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModelInUse {
    pub provider: String,
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelUsage {
    pub provider: String,
    pub model: Option<String>,
    /// Empty when the provider reported no limits.
    pub windows: Vec<UsageWindow>,
}

/// One rate-limit window, such as the 5-hour or 7-day limit.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    /// Short name, such as `5h` or `7d`.
    pub label: String,
    /// From 0 to 100.
    pub used_percent: f32,
    /// Unix seconds when the window resets.
    pub resets_at: Option<u64>,
}

/// A short label for a window of `minutes`: `5h`, `7d`, or `90m`.
pub fn window_label(minutes: u64) -> String {
    match minutes {
        0 => "?".into(),
        m if m.is_multiple_of(1440) => format!("{}d", m / 1440),
        m if m.is_multiple_of(60) => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// Estimates the seconds until every task in `run` is done, at Unix second `now`.
///
/// The typical task takes the median duration of the run's succeeded attempts. The
/// remaining tasks run in waves of as many tasks as are open now, but never in fewer
/// waves than their longest dependency chain. The open attempts' mean elapsed time is
/// already spent. `None` while no task has succeeded or the run is paused. Requires
/// `derive_task_states` to have run.
pub fn estimate(run: &Run, now: u64) -> Option<u64> {
    if run.paused_at.is_some() {
        return None;
    }
    let mut durations: Vec<u64> = run
        .tasks
        .iter()
        .flat_map(|task| &task.attempts)
        .filter(|attempt| attempt.state == AttemptState::Succeeded)
        .filter_map(|attempt| Some(attempt.ended_at?.saturating_sub(attempt.started_at)))
        .collect();
    if durations.is_empty() {
        return None;
    }
    durations.sort_unstable();
    let middle = durations.len() / 2;
    let median = if durations.len().is_multiple_of(2) {
        (durations[middle - 1] + durations[middle]) / 2
    } else {
        durations[middle]
    };
    let remaining = run.tasks.len() - run.completed_tasks();
    if remaining == 0 {
        return Some(0);
    }
    let open: Vec<u64> = run
        .open_attempts()
        .map(|attempt| now.saturating_sub(attempt.started_at))
        .collect();
    let parallel = open.len().max(1);
    let waves = remaining
        .div_ceil(parallel)
        .max(longest_remaining_chain(run));
    let elapsed = match open.len() {
        0 => 0,
        n => open.iter().sum::<u64>() / n as u64,
    };
    Some((waves as u64 * median).saturating_sub(elapsed))
}

/// The most tasks not yet completed on any one dependency path.
fn longest_remaining_chain(run: &Run) -> usize {
    let remaining: HashMap<&str, &[String]> = run
        .tasks
        .iter()
        .filter(|task| task.state != TaskState::Completed)
        .map(|task| (task.id.as_str(), task.deps.as_slice()))
        .collect();
    let mut depth = HashMap::new();
    remaining
        .keys()
        .map(|id| chain(id, &remaining, &mut depth, &mut HashSet::new()))
        .max()
        .unwrap_or(0)
}

fn chain<'a>(
    id: &'a str,
    remaining: &HashMap<&'a str, &'a [String]>,
    depth: &mut HashMap<&'a str, usize>,
    visiting: &mut HashSet<&'a str>,
) -> usize {
    if let Some(known) = depth.get(id) {
        return *known;
    }
    // A cycle cannot be applied, but a corrupt graph must not recurse forever.
    if !visiting.insert(id) {
        return 0;
    }
    let deps = remaining.get(id).copied().unwrap_or_default();
    let longest = deps
        .iter()
        .filter(|dep| remaining.contains_key(dep.as_str()))
        .map(|dep| chain(dep, remaining, depth, visiting))
        .max()
        .unwrap_or(0);
    visiting.remove(id);
    depth.insert(id, longest + 1);
    longest + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{Attempt, Coordinator, Provider, Task};
    use std::path::PathBuf;

    fn attempt(state: AttemptState, started_at: u64, ended_at: Option<u64>) -> Attempt {
        Attempt {
            id: format!("a{started_at}"),
            number: 1,
            agent: "agent".into(),
            session: None,
            worktree: PathBuf::new(),
            provider: Provider::Command,
            model: None,
            explicit_worktree: Some(false),
            state,
            summary: None,
            blocker: None,
            detail: None,
            started_at,
            ended_at,
            paused: false,
            stalled: false,
            artifacts: Vec::new(),
        }
    }

    fn task(id: &str, deps: &[&str], attempts: Vec<Attempt>) -> Task {
        Task {
            id: id.into(),
            title: id.into(),
            instructions: String::new(),
            deps: deps.iter().map(|dep| (*dep).into()).collect(),
            provider: None,
            model: None,
            state: TaskState::Waiting,
            queued_at: None,
            current_attempt: None,
            attempts,
        }
    }

    fn run(tasks: Vec<Task>) -> Run {
        let mut run = Run {
            id: "run".into(),
            box_id: "box".into(),
            title: "Run".into(),
            coordinator: Coordinator::External {
                label: "test".into(),
            },
            created_at: 0,
            archived_at: None,
            paused_at: None,
            tasks,
        };
        run.derive_task_states();
        run
    }

    fn done(started_at: u64, took: u64) -> Vec<Attempt> {
        vec![attempt(
            AttemptState::Succeeded,
            started_at,
            Some(started_at + took),
        )]
    }

    #[test]
    fn no_estimate_before_any_task_succeeds() {
        let run = run(vec![
            task("a", &[], vec![attempt(AttemptState::Active, 100, None)]),
            task("b", &[], vec![attempt(AttemptState::Failed, 0, Some(50))]),
        ]);
        assert_eq!(estimate(&run, 200), None);
    }

    #[test]
    fn no_estimate_while_paused() {
        let mut run = run(vec![task("a", &[], done(0, 60)), task("b", &[], vec![])]);
        assert_eq!(estimate(&run, 100), Some(60));
        run.paused_at = Some(90);
        assert_eq!(estimate(&run, 100), None);
    }

    #[test]
    fn finished_run_needs_no_more_time() {
        let run = run(vec![task("a", &[], done(0, 60))]);
        assert_eq!(estimate(&run, 1000), Some(0));
    }

    #[test]
    fn parallel_workers_share_the_remaining_waves() {
        // Median of 60, 100 and 200 is 100. Four tasks remain over two open workers, so two
        // waves, less the open workers' mean elapsed of 30.
        let run = run(vec![
            task("a", &[], done(0, 60)),
            task("b", &[], done(0, 100)),
            task("c", &[], done(0, 200)),
            task("d", &[], vec![attempt(AttemptState::Active, 980, None)]),
            task("e", &[], vec![attempt(AttemptState::NeedsInput, 960, None)]),
            task("f", &[], vec![]),
            task("g", &[], vec![]),
        ]);
        assert_eq!(estimate(&run, 1000), Some(2 * 100 - 30));
    }

    #[test]
    fn a_dependency_chain_longer_than_the_waves_sets_the_estimate() {
        // Three remaining tasks with two open workers would be two waves, but c waits on b
        // which waits on a, so three waves. Even-count median of 40 and 60 is 50.
        let run = run(vec![
            task("x", &[], done(0, 40)),
            task("y", &[], done(0, 60)),
            task("a", &[], vec![attempt(AttemptState::Active, 90, None)]),
            task("b", &["a"], vec![]),
            task("c", &["b", "x"], vec![]),
            task("z", &[], vec![attempt(AttemptState::Active, 70, None)]),
        ]);
        assert_eq!(longest_remaining_chain(&run), 3);
        assert_eq!(estimate(&run, 100), Some(3 * 50 - 20));
    }

    #[test]
    fn overrun_workers_clamp_the_estimate_at_zero() {
        let run = run(vec![
            task("a", &[], done(0, 10)),
            task("b", &[], vec![attempt(AttemptState::Active, 0, None)]),
        ]);
        assert_eq!(estimate(&run, 500), Some(0));
    }

    #[test]
    fn window_labels() {
        assert_eq!(window_label(300), "5h");
        assert_eq!(window_label(10080), "7d");
        assert_eq!(window_label(90), "90m");
    }
}
