//! The fallback for systems without cgroup ownership (see `cgroup`): find a
//! session's processes by ancestry, session ID and the session's owner marker.
//! This cannot find a process that leaves the session, clears its environment and
//! is reparented, so the daemon logs that it runs without cgroups.
//!
//! Identity is pinned, so a reused PID is never signalled or used to find a tree:
//!
//! - The root is the caller's child, which the caller leaves unreaped until the
//!   tree is gone. Until then its PID, and the session ID it leads, cannot be reused.
//! - Every other member is held by a pidfd, opened only after its start time was
//!   checked, and signalled through it. A member whose pidfd cannot be opened, on a
//!   kernel without pidfds too, is never signalled by PID, and an end that leaves it
//!   running says so. Only the root, which cannot be reused while unreaped, may be
//!   signalled by PID.
//! - A member's PID seeds discovery of its children only while its pidfd shows it
//!   alive, which also means its PID still names it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    thread,
    time::{Duration, Instant},
};

/// How often `terminate` checks whether the tree has exited.
const POLL: Duration = Duration::from_millis(50);
/// Bounds discovery when processes keep forking while being signalled.
const MAX_ROUNDS: usize = 16;
/// How often `terminate` logs members that outlive SIGKILL.
const STUCK_LOG: Duration = Duration::from_secs(10);
/// How long `terminate` keeps killing members that outlive SIGKILL before it
/// reports the teardown as incomplete.
const KILL_LIMIT: Duration = Duration::from_secs(30);

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

/// Whether the process's initial environment holds exactly `entry`.
fn carries(pid: u32, entry: &[u8]) -> bool {
    fs::read(format!("/proc/{pid}/environ"))
        .is_ok_and(|environ| environ.split(|byte| *byte == 0).any(|item| item == entry))
}

/// What a signal to a pinned process did.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Sent {
    Delivered,
    /// The process has exited, so it is no longer a member.
    Gone,
    /// Another user's process, which Power cannot own.
    Denied,
}

/// A process pinned by a pidfd, or by its start time where pidfds are unavailable.
#[derive(Debug)]
pub(crate) struct Pinned {
    pid: u32,
    start: u64,
    fd: Option<OwnedFd>,
}

impl Pinned {
    /// Pins `pid` if it is still the process that started at `start`. The pidfd is
    /// opened before the recheck, so a match means the pidfd names that process.
    /// `Err` means the process could not be pinned at all, so it must not be
    /// signalled by PID.
    fn open(pid: u32, start: u64) -> Result<Option<Self>, io::Error> {
        Self::open_with(pid, start, pidfd_open)
    }

    /// `open` with the pidfd source given, so a kernel without pidfds can be tested.
    /// Every error but `ESRCH` leaves the process unpinned, `ENOSYS` included: a start
    /// time checked apart from the signal cannot stop a reused PID from receiving it.
    fn open_with(
        pid: u32,
        start: u64,
        open: impl Fn(u32) -> io::Result<OwnedFd>,
    ) -> Result<Option<Self>, io::Error> {
        let fd = match open(pid) {
            Ok(fd) => fd,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(stat(pid)
            .filter(|stat| stat.start == start && !stat.zombie)
            .map(|_| Self {
                pid,
                start,
                fd: Some(fd),
            }))
    }

    /// Pins `pid` by a pidfd if it is still the process that started at `start`.
    pub(crate) fn pin_start(pid: u32, start: u64) -> Option<Self> {
        Self::open(pid, start).ok().flatten()
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Pins a process that is not the caller's child by a pidfd. Without one, the
    /// process is not pinned, since only a pidfd keeps a reused PID from being
    /// signalled.
    pub(crate) fn member(pid: u32) -> Option<Self> {
        let fd = pidfd_open(pid).ok()?;
        Some(Self {
            pid,
            start: stat(pid).map_or(0, |stat| stat.start),
            fd: Some(fd),
        })
    }

    /// Pins the caller's unreaped child, whose PID cannot be reused until it is reaped.
    pub(crate) fn child(pid: u32) -> Self {
        Self {
            pid,
            start: stat(pid).map_or(0, |stat| stat.start),
            fd: pidfd_open(pid).ok(),
        }
    }

    /// Whether the process has exited. A zombie has exited.
    pub(crate) fn exited(&self) -> bool {
        match &self.fd {
            Some(fd) => {
                let mut poll = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd and a zero timeout.
                let ready = unsafe { libc::poll(&mut poll, 1, 0) };
                ready != 0
            }
            None => !stat(self.pid).is_some_and(|stat| stat.start == self.start && !stat.zombie),
        }
    }

    pub(crate) fn signal(&self, signal: libc::c_int) -> Sent {
        let result = match &self.fd {
            // SAFETY: the pidfd is open, and pidfd_send_signal reads no memory when
            // info is null.
            Some(fd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            None => {
                // Recheck the start time so a reused PID is left alone.
                if self.exited() {
                    return Sent::Gone;
                }
                let Ok(pid) = libc::pid_t::try_from(self.pid) else {
                    return Sent::Gone;
                };
                // SAFETY: kill has no memory effects.
                unsafe { libc::kill(pid, signal) }.into()
            }
        };
        if result == 0 {
            return Sent::Delivered;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) => Sent::Denied,
            _ => Sent::Gone,
        }
    }
}

fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?;
    // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = libc::c_int::try_from(fd).map_err(|_| io::Error::from_raw_os_error(libc::EBADF))?;
    // SAFETY: the kernel returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A launched process and everything it started, owned until all of it has exited.
pub(crate) struct ProcessTree {
    root: Pinned,
    /// Session ID whose members belong to the tree, when the root leads a session.
    session: Option<u32>,
    /// `NAME=value` that every process started in the tree inherits, so a process
    /// that left both the root's descendants and its session is still found.
    owner: Option<Vec<u8>>,
    /// Every process found so far besides the root, by PID.
    members: BTreeMap<u32, Pinned>,
    /// Processes already checked for the owner marker and found without it.
    strangers: BTreeMap<u32, u64>,
    /// Members that belong to another user, by PID and start time. Power cannot
    /// signal them, so they are not waited for, and an end reports them.
    denied: BTreeMap<u32, u64>,
    /// Processes found in the last look that could not be pinned, so they were
    /// never signalled.
    unpinned: BTreeSet<u32>,
}

impl ProcessTree {
    /// Pins `root`, which must be the caller's child. The caller reaps it only after
    /// `alive` returns false, or after dropping the tree. `owner` is an environment
    /// entry like `POWER_SESSION=<id>` that the root was started with.
    pub(crate) fn pin(root: u32, owner: Option<String>) -> Self {
        let session = stat(root).filter(|stat| stat.session == root).map(|_| root);
        Self {
            root: Pinned::child(root),
            session,
            owner: owner.map(String::into_bytes),
            members: BTreeMap::new(),
            strangers: BTreeMap::new(),
            denied: BTreeMap::new(),
            unpinned: BTreeSet::new(),
        }
    }

    /// Whether the root has exited. It stays unreaped, so its PID stays pinned.
    pub(crate) fn root_exited(&self) -> bool {
        self.root.exited()
    }

    /// Adds processes that joined the tree since the last look, and forgets exited
    /// ones. Returns how many joined.
    fn discover(&mut self) -> usize {
        let processes = all_processes();
        let protected = protected(&processes, self.root.pid);
        // After the snapshot, so a member that is alive now was alive in it, under
        // the same PID.
        self.members.retain(|_, member| !member.exited());
        let same = |pid: &u32, start: &mut u64| {
            processes
                .get(pid)
                .is_some_and(|stat| stat.start == *start && !stat.zombie)
        };
        self.strangers.retain(same);
        self.denied.retain(same);
        self.unpinned.clear();
        let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (pid, stat) in &processes {
            children.entry(stat.parent).or_default().push(*pid);
        }
        let mut queue: Vec<u32> = std::iter::once(self.root.pid)
            .chain(self.members.keys().copied())
            .collect();
        for (pid, stat) in &processes {
            if self.members.contains_key(pid)
                || self.denied.contains_key(pid)
                || protected.contains(pid)
                || stat.zombie
            {
                continue;
            }
            if Some(stat.session) == self.session {
                queue.push(*pid);
                continue;
            }
            let Some(owner) = &self.owner else { continue };
            if self.strangers.get(pid) == Some(&stat.start) {
                continue;
            }
            if carries(*pid, owner) {
                queue.push(*pid);
            } else {
                self.strangers.insert(*pid, stat.start);
            }
        }
        let mut joined = 0;
        let mut seen = BTreeSet::new();
        while let Some(pid) = queue.pop() {
            if !seen.insert(pid) || protected.contains(&pid) || self.denied.contains_key(&pid) {
                continue;
            }
            let known = pid == self.root.pid || self.members.contains_key(&pid);
            if !known {
                let Some(stat) = processes.get(&pid).filter(|stat| !stat.zombie) else {
                    continue;
                };
                let pinned = match Pinned::open(pid, stat.start) {
                    Ok(Some(pinned)) => pinned,
                    Ok(None) => continue,
                    Err(error) => {
                        // Its PID could be reused before a signal, so it is never sent
                        // one, and neither are children found through it.
                        eprintln!("Could not pin process {pid}, so it is not signalled. {error}");
                        self.unpinned.insert(pid);
                        continue;
                    }
                };
                self.members.insert(pid, pinned);
                joined += 1;
            }
            queue.extend(children.get(&pid).into_iter().flatten().copied());
        }
        joined
    }

    /// Whether any process in the tree, the root included, is still running.
    pub(crate) fn alive(&mut self) -> bool {
        self.discover();
        !self.root.exited() || !self.members.is_empty()
    }

    /// Sends `signal` to the whole tree, including processes that appear while it is
    /// being sent, and returns how many processes received it.
    pub(crate) fn signal(&mut self, signal: TreeSignal) -> usize {
        let mut sent = BTreeSet::new();
        self.discover();
        for _ in 0..MAX_ROUNDS {
            if !sent.contains(&self.root.pid)
                && !self.root.exited()
                && deliver(&self.root, signal) == Sent::Delivered
            {
                sent.insert(self.root.pid);
            }
            let mut gone = Vec::new();
            for (pid, member) in &self.members {
                if sent.contains(pid) {
                    continue;
                }
                match deliver(member, signal) {
                    Sent::Delivered => {
                        sent.insert(*pid);
                    }
                    Sent::Gone => gone.push(*pid),
                    Sent::Denied => {
                        eprintln!("Process {pid} belongs to another user, so it is left running.");
                        self.denied.insert(*pid, member.start);
                        gone.push(*pid);
                    }
                }
            }
            for pid in gone {
                self.members.remove(&pid);
            }
            if self.discover() == 0 {
                break;
            }
        }
        sent.len()
    }

    /// Asks the tree to exit, waits up to `grace`, then kills whatever is left.
    /// Returns once every member Power can signal has exited, so the caller may then
    /// reap the root. The error names processes still running that Power could not
    /// signal or kill, so the end is incomplete.
    pub(crate) fn terminate(&mut self, grace: Duration) -> Result<(), String> {
        self.signal(TreeSignal::Terminate);
        let deadline = Instant::now() + grace;
        while self.alive() {
            if Instant::now() >= deadline {
                break;
            }
            // Children forked after the first signal are asked too.
            self.signal(TreeSignal::Terminate);
            thread::sleep(POLL);
        }
        let mut logged = Instant::now();
        let killing = Instant::now();
        while self.alive() {
            if killing.elapsed() >= KILL_LIMIT {
                break;
            }
            self.signal(TreeSignal::Kill);
            thread::sleep(POLL);
            if logged.elapsed() >= STUCK_LOG {
                logged = Instant::now();
                eprintln!(
                    "Still waiting for processes {:?} of tree {} to exit after SIGKILL.",
                    self.members.keys().collect::<Vec<_>>(),
                    self.root.pid
                );
            }
        }
        let mut left: Vec<String> = Vec::new();
        if !self.members.is_empty() {
            left.push(format!(
                "{:?} outlived SIGKILL",
                self.members.keys().collect::<Vec<_>>()
            ));
        }
        if !self.denied.is_empty() {
            left.push(format!(
                "{:?} belong to another user",
                self.denied.keys().collect::<Vec<_>>()
            ));
        }
        if !self.unpinned.is_empty() {
            left.push(format!("{:?} could not be pinned", self.unpinned));
        }
        if left.is_empty() {
            Ok(())
        } else {
            Err(format!("processes {}", left.join(", and ")))
        }
    }
}

fn deliver(process: &Pinned, signal: TreeSignal) -> Sent {
    let sent = process.signal(signal.number());
    if sent == Sent::Delivered && signal == TreeSignal::Terminate {
        // A paused process handles SIGTERM only once it runs again. The same pin
        // carries the SIGCONT, so it reaches the same process.
        process.signal(libc::SIGCONT);
    }
    sent
}

/// Power's own PID, its ancestors, and its other children, which are other
/// sessions' roots and Power's own helpers. None of them is ever a member.
fn protected(processes: &BTreeMap<u32, Stat>, root: u32) -> BTreeSet<u32> {
    let own = std::process::id();
    let mut protected = BTreeSet::from([0, 1]);
    let mut pid = own;
    while protected.insert(pid) {
        match processes.get(&pid) {
            Some(stat) => pid = stat.parent,
            None => break,
        }
    }
    protected.extend(
        processes
            .iter()
            .filter(|(pid, stat)| stat.parent == own && **pid != root)
            .map(|(pid, _)| *pid),
    );
    protected
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::process::CommandExt,
        process::{Child, Command, Stdio},
    };

    fn running(pid: u32) -> bool {
        stat(pid).is_some_and(|stat| !stat.zombie)
    }

    fn read_pids(path: &std::path::Path, count: usize) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                let pids: Vec<u32> = text
                    .split_whitespace()
                    .filter_map(|pid| pid.parse().ok())
                    .collect();
                if pids.len() == count {
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

    /// Runs `script` in a new session with `marker` in its environment.
    fn spawn(script: &str, marker: &str) -> Child {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .env("PROCESS_TREE_TEST", marker)
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
        command.spawn().unwrap()
    }

    fn owner(marker: &str) -> Option<String> {
        Some(format!("PROCESS_TREE_TEST={marker}"))
    }

    /// A session leader with a child that ignores SIGTERM and a grandchild in a new
    /// session, which a process group signal would miss.
    fn spawn_tree(directory: &std::path::Path) -> (Child, ProcessTree, Vec<u32>) {
        let pids = directory.join("pids");
        let script = format!(
            "trap '' TERM; sleep 600 & a=$!; setsid sleep 600 & b=$!; \
             echo \"$a $b\" > {}.tmp; mv {0}.tmp {0}; wait",
            pids.display()
        );
        let child = spawn(&script, "tree");
        let tree = ProcessTree::pin(child.id(), None);
        let descendants = read_pids(&pids, 2);
        (child, tree, descendants)
    }

    #[test]
    fn terminate_ends_descendants_that_ignore_sigterm_or_left_the_session() {
        let directory = tempfile::tempdir().unwrap();
        let (mut child, mut tree, descendants) = spawn_tree(directory.path());
        tree.discover();
        for pid in &descendants {
            assert!(tree.members.contains_key(pid), "{pid} not discovered");
        }
        assert!(!tree.members.contains_key(&std::process::id()));

        let started = Instant::now();
        tree.terminate(Duration::from_millis(300)).unwrap();
        assert!(!tree.alive());
        child.wait().unwrap();

        assert!(started.elapsed() < Duration::from_secs(5));
        for pid in descendants {
            assert!(!running(pid), "{pid} survived");
        }
    }

    #[test]
    fn a_detached_child_is_still_owned_after_the_root_exits_first() {
        let directory = tempfile::tempdir().unwrap();
        let pids = directory.path().join("pids");
        // The root starts a child in its own session, then exits before anything
        // looks at its tree, so neither parentage nor session leads to the child.
        let script = format!(
            "setsid sleep 600 & echo $! > {}.tmp; mv {0}.tmp {0}",
            pids.display()
        );
        let marker = format!("detached-{}", std::process::id());
        let mut child = spawn(&script, &marker);
        let mut tree = ProcessTree::pin(child.id(), owner(&marker));
        let detached = read_pids(&pids, 1)[0];
        let deadline = Instant::now() + Duration::from_secs(10);
        while !tree.root_exited() {
            assert!(Instant::now() < deadline, "the root never exited");
            thread::sleep(POLL);
        }
        // The unreaped root still pins its PID.
        assert!(stat(child.id()).is_some_and(|stat| stat.zombie));
        assert!(tree.alive(), "the detached child was not found");
        assert!(tree.members.contains_key(&detached));

        tree.terminate(Duration::from_millis(300)).unwrap();
        assert!(!running(detached), "{detached} survived");
        assert!(!tree.alive());
        child.wait().unwrap();
    }

    #[test]
    fn a_process_without_the_owner_marker_is_left_alone() {
        let marker = format!("ours-{}", std::process::id());
        let mut stranger = spawn("sleep 600", &format!("theirs-{}", std::process::id()));
        let mut child = spawn("sleep 600", &marker);
        let mut tree = ProcessTree::pin(child.id(), owner(&marker));
        tree.terminate(Duration::from_millis(300)).unwrap();
        child.wait().unwrap();
        assert!(running(stranger.id()));
        stranger.kill().unwrap();
        stranger.wait().unwrap();
    }

    #[test]
    fn without_pidfds_a_member_is_never_pinned_so_never_signalled_by_pid() {
        let mut child = Command::new("sleep").arg("600").spawn().unwrap();
        let pid = child.id();
        let start = stat(pid).unwrap().start;
        let enosys = |_| Err(io::Error::from_raw_os_error(libc::ENOSYS));
        let pinned = Pinned::open_with(pid, start, enosys);
        assert_eq!(
            pinned
                .map(|pinned| pinned.is_some())
                .map_err(|error| error.raw_os_error()),
            Err(Some(libc::ENOSYS))
        );
        // A live pidfd still pins it.
        assert!(Pinned::open(pid, start).unwrap().is_some());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_pid_whose_start_time_changed_is_never_pinned() {
        let mut child = spawn("sleep 600", "reuse");
        let start = stat(child.id()).unwrap().start;
        // The same PID with another start time is what a reused PID looks like.
        assert!(Pinned::open(child.id(), start + 1).unwrap().is_none());
        let pinned = Pinned::open(child.id(), start).unwrap().unwrap();
        assert!(pinned.fd.is_some(), "this kernel has pidfds");
        child.kill().unwrap();
        child.wait().unwrap();
        // Once reaped, the pin reports the exit and delivers nothing, whatever
        // process takes the PID next.
        assert!(pinned.exited());
        assert_eq!(pinned.signal(libc::SIGTERM), Sent::Gone);
        assert_eq!(pinned.signal(libc::SIGCONT), Sent::Gone);
    }

    #[test]
    fn stop_and_continue_pause_the_whole_tree() {
        let directory = tempfile::tempdir().unwrap();
        let (mut child, mut tree, descendants) = spawn_tree(directory.path());
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
        assert!(tree.signal(TreeSignal::Stop) >= 3);
        settle(true);
        tree.signal(TreeSignal::Continue);
        settle(false);
        // Terminating a paused tree still ends it.
        tree.signal(TreeSignal::Stop);
        tree.terminate(Duration::from_millis(300)).unwrap();
        child.wait().unwrap();
        for pid in descendants {
            assert!(!running(pid), "{pid} survived");
        }
    }
}
