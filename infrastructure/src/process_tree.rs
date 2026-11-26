//! Signal a session's whole process tree: its root, every descendant, and any
//! process left in the root's session after its parent died.
//!
//! Ending a session and pausing or resuming one share this. A process is named by
//! its PID and start time, so a reused PID is never signalled.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, thread,
    time::{Duration, Instant},
};

/// How often `terminate` checks whether the tree has exited.
const POLL: Duration = Duration::from_millis(50);
/// Bounds discovery when processes keep forking while being signalled.
const MAX_ROUNDS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TreeSignal {
    /// Asks every process to exit.
    Terminate,
    /// Ends every process at once.
    Kill,
    /// Pauses every process.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "pausing a session comes with #79")
    )]
    Stop,
    /// Resumes paused processes.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "resuming a session comes with #79")
    )]
    Continue,
}

impl TreeSignal {
    fn number(self) -> libc::c_int {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
            Self::Stop => libc::SIGSTOP,
            Self::Continue => libc::SIGCONT,
        }
    }
}

/// One process as `/proc/<pid>/stat` describes it.
#[derive(Clone, Copy, Debug)]
struct Stat {
    parent: u32,
    session: u32,
    start: u64,
    zombie: bool,
}

fn stat(pid: u32) -> Option<Stat> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // proc_pid_stat(5): the name may contain spaces, so fields follow the last ')'.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(Stat {
        zombie: matches!(*fields.first()?, "Z" | "X"),
        parent: fields.get(1)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

fn all_processes() -> BTreeMap<u32, Stat> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| Some((pid, stat(pid)?)))
        .collect()
}

/// A live process tree rooted at a session's launched process.
pub(crate) struct ProcessTree {
    root: u32,
    /// Session ID whose members belong to the tree, when the root leads a session.
    session: Option<u32>,
    /// Every process found so far, by PID, with its start time.
    members: BTreeMap<u32, u64>,
}

impl ProcessTree {
    /// Finds the tree under `root`. Power's own process and its ancestors are never
    /// members, so a stray PID cannot end the daemon.
    pub(crate) fn new(root: u32) -> Self {
        let session = stat(root).filter(|stat| stat.session == root).map(|_| root);
        let mut tree = Self {
            root,
            session,
            members: BTreeMap::new(),
        };
        tree.discover();
        tree
    }

    /// Adds processes that joined the tree since the last look, and forgets exited ones.
    fn discover(&mut self) -> Vec<u32> {
        let processes = all_processes();
        let protected = protected(&processes);
        self.members.retain(|pid, start| {
            processes
                .get(pid)
                .is_some_and(|stat| stat.start == *start && !stat.zombie)
        });
        let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (pid, stat) in &processes {
            children.entry(stat.parent).or_default().push(*pid);
        }
        let mut found = Vec::new();
        let mut queue: Vec<u32> = std::iter::once(self.root)
            .chain(self.members.keys().copied())
            .chain(
                processes
                    .iter()
                    .filter(|(_, stat)| Some(stat.session) == self.session)
                    .map(|(pid, _)| *pid),
            )
            .collect();
        let mut seen = BTreeSet::new();
        while let Some(pid) = queue.pop() {
            if !seen.insert(pid) || protected.contains(&pid) {
                continue;
            }
            let Some(stat) = processes.get(&pid).filter(|stat| !stat.zombie) else {
                continue;
            };
            if self.members.insert(pid, stat.start).is_none() {
                found.push(pid);
            }
            queue.extend(children.get(&pid).into_iter().flatten().copied());
        }
        found
    }

    /// Whether any member is still running. Zombies count as exited.
    pub(crate) fn alive(&mut self) -> bool {
        self.discover();
        !self.members.is_empty()
    }

    /// Sends `signal` to every member, including processes that appear while it is
    /// being sent, and returns how many processes received it.
    pub(crate) fn signal(&mut self, signal: TreeSignal) -> usize {
        let mut sent = BTreeSet::new();
        for _ in 0..MAX_ROUNDS {
            let pending: Vec<(u32, u64)> = self
                .members
                .iter()
                .filter(|(pid, _)| !sent.contains(*pid))
                .map(|(pid, start)| (*pid, *start))
                .collect();
            for (pid, start) in pending {
                // Recheck the start time so a reused PID is left alone.
                if stat(pid).is_some_and(|stat| stat.start == start) && send(pid, signal.number()) {
                    if signal == TreeSignal::Terminate {
                        // A paused process handles SIGTERM only once it runs again.
                        send(pid, libc::SIGCONT);
                    }
                    sent.insert(pid);
                }
            }
            if self.discover().is_empty() {
                break;
            }
        }
        sent.len()
    }

    /// Asks the tree to exit, waits up to `grace`, then kills whatever is left.
    /// Returns once no member runs, or after the kill if some never exit.
    pub(crate) fn terminate(&mut self, grace: Duration) {
        self.signal(TreeSignal::Terminate);
        let deadline = Instant::now() + grace;
        while self.alive() {
            if Instant::now() >= deadline {
                self.signal(TreeSignal::Kill);
                let deadline = Instant::now() + grace.max(POLL * 10);
                while self.alive() && Instant::now() < deadline {
                    thread::sleep(POLL);
                }
                return;
            }
            // Children forked after the first signal are asked too.
            self.signal(TreeSignal::Terminate);
            thread::sleep(POLL);
        }
    }
}

/// Power's own PID and its ancestors.
fn protected(processes: &BTreeMap<u32, Stat>) -> BTreeSet<u32> {
    let mut protected = BTreeSet::from([0, 1]);
    let mut pid = std::process::id();
    while protected.insert(pid) {
        match processes.get(&pid) {
            Some(stat) => pid = stat.parent,
            None => break,
        }
    }
    protected
}

fn send(pid: u32, signal: libc::c_int) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill has no memory effects. The PID was checked against its start time.
    unsafe { libc::kill(pid, signal) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };

    fn running(pid: u32) -> bool {
        stat(pid).is_some_and(|stat| !stat.zombie)
    }

    fn read_pids(path: &std::path::Path) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                let pids: Vec<u32> = text
                    .split_whitespace()
                    .filter_map(|pid| pid.parse().ok())
                    .collect();
                if pids.len() == 2 {
                    return pids;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the fixture never wrote its PIDs"
            );
            thread::sleep(POLL);
        }
    }

    /// A session leader with a child that ignores SIGTERM and a grandchild in a new
    /// session, which a process group signal would miss.
    fn spawn_tree(directory: &std::path::Path) -> (std::process::Child, Vec<u32>) {
        let pids = directory.join("pids");
        let script = format!(
            "trap '' TERM; sleep 600 & a=$!; setsid sleep 600 & b=$!; \
             echo \"$a $b\" > {}.tmp; mv {0}.tmp {0}; wait",
            pids.display()
        );
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let descendants = read_pids(&pids);
        (child, descendants)
    }

    #[test]
    fn terminate_ends_descendants_that_ignore_sigterm_or_left_the_session() {
        let directory = tempfile::tempdir().unwrap();
        let (mut child, descendants) = spawn_tree(directory.path());
        let mut tree = ProcessTree::new(child.id());
        for pid in &descendants {
            assert!(tree.members.contains_key(pid), "{pid} not discovered");
        }
        assert!(!tree.members.contains_key(&std::process::id()));

        let started = Instant::now();
        tree.terminate(Duration::from_millis(300));
        child.wait().unwrap();

        assert!(started.elapsed() < Duration::from_secs(5));
        for pid in descendants {
            assert!(!running(pid), "{pid} survived");
        }
    }

    #[test]
    fn stop_and_continue_pause_the_whole_tree() {
        let directory = tempfile::tempdir().unwrap();
        let (mut child, descendants) = spawn_tree(directory.path());
        let state = |pid: u32| {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            stat.rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned()
        };
        let settle = |expect_stopped: bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while descendants
                .iter()
                .any(|pid| (state(*pid) == "T") != expect_stopped)
            {
                assert!(Instant::now() < deadline, "the tree never settled");
                thread::sleep(POLL);
            }
        };
        let mut tree = ProcessTree::new(child.id());
        assert!(tree.signal(TreeSignal::Stop) >= 3);
        settle(true);
        tree.signal(TreeSignal::Continue);
        settle(false);
        // Terminating a paused tree still ends it.
        tree.signal(TreeSignal::Stop);
        tree.terminate(Duration::from_millis(300));
        child.wait().unwrap();
        for pid in descendants {
            assert!(!running(pid), "{pid} survived");
        }
    }
}
