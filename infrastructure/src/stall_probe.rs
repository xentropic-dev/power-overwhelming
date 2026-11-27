//! What the stall check sees of a worker: its terminal's idle time, and whether the
//! processes its session started did any work since the last look.

use crate::terminal::PtyRuntime;
use application::{Application, dag::WorkerProbe};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// A fresh process scan is reused for this long, so one stall check scans once. It is
/// shorter than the shortest stall check interval, so each check scans afresh.
const SNAPSHOT_AGE: Duration = Duration::from_millis(500);
/// CPU use, as a share of one core, above which a session's processes count as working.
const BUSY_SHARE: f64 = 0.02;

pub(crate) struct LocalWorkerProbe {
    application: Arc<Application<PtyRuntime>>,
    state: Mutex<ProbeState>,
}

#[derive(Default)]
struct ProbeState {
    snapshot: Option<(Instant, BTreeMap<String, Usage>)>,
    previous: BTreeMap<String, (Instant, Usage)>,
}

/// The processes that carry one session's `POWER_SESSION`, with each one's CPU time.
#[derive(Clone, Default)]
struct Usage(BTreeMap<u32, u64>);

impl LocalWorkerProbe {
    pub(crate) fn new(application: Arc<Application<PtyRuntime>>) -> Self {
        Self {
            application,
            state: Mutex::new(ProbeState::default()),
        }
    }
}

impl WorkerProbe for LocalWorkerProbe {
    fn idle_for(&self, session: &str) -> Option<Duration> {
        let terminal = self.application.terminal(session).ok()?;
        if !terminal.is_running() {
            return None;
        }
        let activity = terminal.activity();
        let last = activity.last_output.max(activity.last_input);
        let now = crate::talk_adapter::now_ms();
        Some(Duration::from_millis(now.saturating_sub(last)))
    }

    fn children_working(&self, session: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        if state
            .snapshot
            .as_ref()
            .is_none_or(|(at, _)| now.duration_since(*at) >= SNAPSHOT_AGE)
        {
            state.snapshot = Some((now, scan()));
        }
        let current = state
            .snapshot
            .as_ref()
            .and_then(|(_, sessions)| sessions.get(session))
            .cloned()
            .unwrap_or_default();
        let previous = state
            .previous
            .insert(session.into(), (now, current.clone()));
        // Without an earlier sample there is nothing to compare, so assume work.
        let Some((then, before)) = previous else {
            return true;
        };
        if current.0.keys().any(|pid| !before.0.contains_key(pid)) {
            return true;
        }
        // Only processes seen both times, so an exited one cannot hide new work.
        let ticks: u64 = current
            .0
            .iter()
            .filter_map(|(pid, now)| Some(now.saturating_sub(*before.0.get(pid)?)))
            .sum();
        let elapsed = now.duration_since(then).as_secs_f64();
        let used = ticks as f64 / clock_ticks();
        elapsed > 0. && used / elapsed > BUSY_SHARE
    }
}

fn clock_ticks() -> f64 {
    // SAFETY: sysconf has no preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as f64 } else { 100. }
}

/// Every process this user can read, grouped by the `POWER_SESSION` it started with.
fn scan() -> BTreeMap<String, Usage> {
    let mut sessions: BTreeMap<String, Usage> = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return sessions;
    };
    for pid in entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    {
        let Some(session) = session_of(pid) else {
            continue;
        };
        let Some(ticks) = cpu_ticks(pid) else {
            continue;
        };
        sessions.entry(session).or_default().0.insert(pid, ticks);
    }
    sessions
}

fn session_of(pid: u32) -> Option<String> {
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ.split(|byte| *byte == 0).find_map(|item| {
        let value = item.strip_prefix(b"POWER_SESSION=")?;
        String::from_utf8(value.to_vec()).ok()
    })
}

/// User plus system CPU time, in clock ticks.
fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // proc_pid_stat(5): the name may contain spaces, so fields follow the last ')'.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_process_cpu_time() {
        assert!(cpu_ticks(std::process::id()).is_some());
    }
}
