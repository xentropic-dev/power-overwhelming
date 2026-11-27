//! What owns one launched worker process and everything it starts: its cgroup
//! where the system supports that, or process discovery as a logged fallback.
//! Ending, pausing and resuming a worker go through here.

use crate::{
    cgroup::Scope,
    process_tree::{Pinned, ProcessTree, Sent, TreeSignal},
};
use std::{
    sync::{
        Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub(crate) enum Owner {
    /// The launch's own cgroup. `root` is the launched process, pinned because it
    /// runs as `systemd-run` outside the scope until the scope exists.
    Scope { root: Pinned, scope: Scope },
    /// The fallback's subreaper (see `reaper`), the caller's unreaped child. It
    /// adopts and ends everything the program starts, and exits only once all of it
    /// has exited, so it alone is signalled and waited for. Killing it would orphan
    /// what it has not yet ended.
    Reaper(Pinned),
    /// Ancestry, session and owner-marker discovery, for a process started without
    /// a reaper.
    Tree(ProcessTree),
}

/// How long a reaper may take to end its tree beyond the caller's grace: its own
/// grace before SIGKILL, and time for the killed processes to be reaped.
const REAPER_SLACK: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

impl Owner {
    /// Owns the caller's unreaped child `pid`, which was started in `scope` when
    /// there is one. `marker` is the fallback's owner environment entry.
    /// `reaped` says the process is the fallback's reaper.
    pub(crate) fn new(
        pid: u32,
        scope: Option<Scope>,
        reaped: bool,
        marker: Option<String>,
    ) -> Self {
        match scope {
            Some(scope) => Self::Scope {
                root: Pinned::child(pid),
                scope,
            },
            None if reaped => Self::Reaper(Pinned::child(pid)),
            None => Self::Tree(ProcessTree::pin(pid, marker)),
        }
    }

    /// Whether the launched process has exited. The caller has not reaped it yet.
    pub(crate) fn root_exited(&self) -> bool {
        match self {
            Self::Scope { root, .. } => root.exited(),
            Self::Reaper(reaper) => reaper.exited(),
            Self::Tree(tree) => tree.root_exited(),
        }
    }

    /// Whether the caller may kill and reap the launched process once `terminate`
    /// has returned. A reaper that has not confirmed its tree is gone must keep
    /// running, since it still owns that tree, and a later reclaim ends it.
    pub(crate) fn releasable(&self, ended: &Result<(), String>) -> bool {
        !matches!(self, Self::Reaper(_)) || ended.is_ok()
    }

    /// Asks everything to exit, waits up to `grace`, then kills what is left. With a
    /// cgroup, `Ok` means the cgroup is empty. The error names what could not be
    /// ended.
    pub(crate) fn terminate(&mut self, grace: Duration) -> Result<(), String> {
        match self {
            Self::Scope { root, scope } => scope.terminate(root, grace),
            Self::Reaper(reaper) => end_reaper(reaper, grace + REAPER_SLACK),
            Self::Tree(tree) => tree.terminate(grace),
        }
    }

    /// Pauses every process with SIGSTOP, so each shows as stopped (state T) and a
    /// coordinator or `ps` sees it is not working. With a cgroup only the scope's
    /// members are signalled, and the fallbacks signal only the tree they own.
    pub(crate) fn pause(&mut self, marker: Option<&str>) -> usize {
        self.send(TreeSignal::Stop, marker)
    }

    /// Resumes a paused worker with SIGCONT.
    pub(crate) fn resume(&mut self, marker: Option<&str>) -> usize {
        self.send(TreeSignal::Continue, marker)
    }

    /// Asks every process of a paused worker to exit without resuming it first: each
    /// gets SIGTERM and only then SIGCONT, so it wakes with the end pending and runs
    /// only its handler, never ordinary work. A reaper's grandchildren are reached
    /// too, which the reaper itself, signalling only its children, would leave stopped.
    pub(crate) fn end_paused(&mut self, marker: Option<&str>) -> usize {
        self.send(TreeSignal::Terminate, marker)
    }

    fn send(&mut self, signal: TreeSignal, marker: Option<&str>) -> usize {
        let number = match signal {
            TreeSignal::Stop => libc::SIGSTOP,
            TreeSignal::Terminate => libc::SIGTERM,
            _ => libc::SIGCONT,
        };
        match self {
            Self::Scope { root, scope } => {
                // The root may still be systemd-run, outside the scope.
                let sent = usize::from(!root.exited() && root.signal(number) == Sent::Delivered);
                if number == libc::SIGTERM && !root.exited() {
                    root.signal(libc::SIGCONT);
                }
                sent + scope.signal_all(number)
            }
            // The reaper and everything it adopted carry the owner marker.
            Self::Reaper(reaper) => {
                if reaper.exited() {
                    return 0;
                }
                ProcessTree::pin(reaper.pid(), marker.map(str::to_owned)).signal(signal)
            }
            Self::Tree(tree) => tree.signal(signal),
        }
    }
}

/// Asks a reaper to end its tree and waits up to `limit` for it to exit, which it
/// does only once everything it adopted has exited. The reaper itself is never
/// killed, so a timeout leaves it owning what is left.
fn end_reaper(reaper: &Pinned, limit: Duration) -> Result<(), String> {
    if !reaper.exited() {
        reaper.signal(libc::SIGTERM);
        reaper.signal(libc::SIGCONT);
    }
    let deadline = std::time::Instant::now() + limit;
    while !reaper.exited() {
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "processes of reaper {}, which has not confirmed they exited",
                reaper.pid()
            ));
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

/// A teardown request, shared by whoever ends a session and the thread that owns
/// its processes. Its lock also serializes a teardown with a relaunch, so an end
/// either comes first and prevents the relaunch, or comes after it and ends the
/// relaunched process.
/// It also carries whether the session should be paused, which the owning thread
/// applies to whatever process tree it owns at the time, a relaunch included.
#[derive(Default)]
pub(crate) struct Stop {
    requested: Mutex<bool>,
    changed: Condvar,
    paused: AtomicBool,
}

impl Stop {
    pub(crate) fn request(&self) {
        *self.requested.lock().unwrap() = true;
        self.changed.notify_all();
    }

    /// Asks the owning thread to pause or resume the session's processes.
    pub(crate) fn set_paused(&self, paused: bool) {
        let _requested = self.requested.lock().unwrap();
        self.paused.store(paused, Ordering::Release);
        self.changed.notify_all();
    }

    pub(crate) fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub(crate) fn requested(&self) -> bool {
        *self.requested.lock().unwrap()
    }

    /// Waits up to `timeout` for an end request, or for the pause wish to differ
    /// from `applied`, and says whether an end is requested.
    pub(crate) fn wait(&self, timeout: Duration, applied: bool) -> bool {
        let requested = self.requested.lock().unwrap();
        *self
            .changed
            .wait_timeout_while(requested, timeout, |requested| {
                !*requested && self.paused.load(Ordering::Acquire) == applied
            })
            .unwrap()
            .0
    }

    /// Holds off requests while the caller acts on there being none.
    pub(crate) fn hold(&self) -> MutexGuard<'_, bool> {
        self.requested.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::tests::TestScopes;
    use portable_pty::CommandBuilder;
    use std::{
        process::{Command, Stdio},
        thread,
        time::Instant,
    };

    #[test]
    fn pause_and_resume_a_scoped_worker_then_end_it_while_paused() {
        let test = TestScopes::new();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "setsid -f sleep 600; exec sleep 600"]);
        let scope = test.scopes.wrap(&mut command, "pause-test").unwrap();
        let check = crate::cgroup::Scope::at_path(scope.path());
        let argv = command.get_argv();
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut owner = Owner::new(child.id(), Some(scope), false, None);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !check.populated() {
            assert!(Instant::now() < deadline, "the scope never started");
            thread::sleep(Duration::from_millis(20));
        }
        let states = || -> Vec<char> {
            check_pids(&check)
                .iter()
                .filter_map(|pid| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok())
                .filter_map(|stat| stat.rsplit_once(')')?.1.trim_start().chars().next())
                .collect()
        };
        let settle = |stopped: bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let states = states();
                if states.len() >= 2 && states.iter().all(|state| (*state == 'T') == stopped) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the scope never settled: {states:?}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        };
        let sleeping = || {
            let pids = check_pids(&check);
            pids.len() == 2
                && pids.iter().all(|pid| {
                    std::fs::read_to_string(format!("/proc/{pid}/comm"))
                        .is_ok_and(|comm| comm.trim() == "sleep")
                })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sleeping() {
            assert!(Instant::now() < deadline, "the sleeps never started");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(owner.pause(None) >= 2);
        settle(true);
        assert!(!owner.root_exited());
        owner.resume(None);
        settle(false);
        owner.pause(None);
        settle(true);
        owner.terminate(Duration::from_millis(300)).unwrap();
        assert!(!check.populated());
        assert!(owner.root_exited());
        child.wait().unwrap();
    }

    fn check_pids(scope: &crate::cgroup::Scope) -> Vec<u32> {
        std::fs::read_to_string(scope.path().join("cgroup.procs"))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|pid| pid.parse().ok())
            .collect()
    }
}
