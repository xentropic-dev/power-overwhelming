//! What the stall check sees of a worker: its terminal's idle time, and whether a
//! command it started, such as a build or test, is still alive.
//!
//! A session owns the processes in the cgroups its launches recorded, every process
//! that carries its `POWER_SESSION`, and every descendant of those. The oldest of them
//! outside the others' trees is the worker. A command is any other owned process that
//! runs under a shell, started after the worker's startup, or left the worker's tree.
//! Helpers an agent starts with itself, such as MCP servers, do not count. A command
//! counts while it is alive, however quiet: a test waiting on a timer or a build
//! waiting on a lock still runs. A helper started later counts too, which can only
//! hide a stall, never report a false one.

use crate::terminal::PtyRuntime;
use application::{Application, dag::WorkerProbe};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// A fresh process scan is reused for this long, so one stall check scans once. It is
/// shorter than the shortest stall check interval, so each check scans afresh.
const SNAPSHOT_AGE: Duration = Duration::from_millis(500);
/// Programs whose children are commands a worker runs.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "fish", "ksh", "mksh", "ash"];
/// Build and test programs. One of these, or anything below one, is a command whenever
/// it started, so a build an agent execs directly during its startup still counts.
const COMMANDS: &[&str] = &[
    "cargo",
    "rustc",
    "make",
    "gmake",
    "cmake",
    "ctest",
    "ninja",
    "meson",
    "go",
    "gcc",
    "g++",
    "cc",
    "c++",
    "clang",
    "clang++",
    "ld",
    "zig",
    "mise",
    "just",
    "bazel",
    "mvn",
    "gradle",
    "sbt",
    "dotnet",
    "pytest",
    "tox",
    "nox",
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "bun",
    "deno",
    "jest",
    "vitest",
    "tsc",
    "mix",
    "rake",
    "bundle",
    "swift",
    "xcodebuild",
];
/// Other processes that start within this long of the worker are its own helpers.
const STARTUP: Duration = Duration::from_secs(30);

pub(crate) struct LocalWorkerProbe {
    application: Arc<Application<PtyRuntime>>,
    /// The daemon's state directory, which holds each session's cgroup ledger.
    state: Option<PathBuf>,
    /// The last process scan. It is replaced on each scan, so it never holds a
    /// session that has gone.
    snapshot: Mutex<Option<(Instant, Arc<Processes>)>>,
}

/// One live process, as the scan read it.
struct Process {
    parent: u32,
    /// When it started, in clock ticks since boot.
    started: u64,
    shell: bool,
    /// Whether it runs a build or test program.
    command: bool,
    /// Its cgroup v2 path, as `/proc/<pid>/cgroup` names it.
    cgroup: String,
    /// The `POWER_SESSION` it started with.
    session: Option<String>,
}

#[derive(Default)]
struct Processes(BTreeMap<u32, Process>);

impl LocalWorkerProbe {
    pub(crate) fn new(application: Arc<Application<PtyRuntime>>) -> Self {
        Self {
            application,
            state: crate::persistence::state_directory().ok(),
            snapshot: Mutex::new(None),
        }
    }

    fn processes(&self) -> Arc<Processes> {
        let mut snapshot = self.snapshot.lock().unwrap();
        let now = Instant::now();
        match snapshot.as_ref() {
            Some((at, processes)) if now.duration_since(*at) < SNAPSHOT_AGE => processes.clone(),
            _ => {
                let processes = Arc::new(scan());
                *snapshot = Some((now, processes.clone()));
                processes
            }
        }
    }

    /// The cgroups `session`'s launches recorded, as `/proc/<pid>/cgroup` names them.
    fn scopes(&self, session: &str) -> Vec<String> {
        let Some(state) = &self.state else {
            return Vec::new();
        };
        crate::owned::scopes(state, session)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| {
                let relative = path.strip_prefix("/sys/fs/cgroup").ok()?;
                Some(format!("/{}", relative.display()))
            })
            .collect()
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

    fn commands_running(&self, session: &str) -> bool {
        self.processes()
            .commands_running(session, &self.scopes(session))
    }
}

impl Processes {
    /// Every process `session` owns: those in its cgroups or carrying its
    /// `POWER_SESSION`, and their descendants, which may have cleared their environment.
    fn owned(&self, session: &str, scopes: &[String]) -> BTreeSet<u32> {
        let in_scope = |cgroup: &str| {
            scopes.iter().any(|scope| {
                cgroup
                    .strip_prefix(scope.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
        };
        let mut owned: BTreeSet<u32> = self
            .0
            .iter()
            .filter(|(_, process)| {
                process.session.as_deref() == Some(session) || in_scope(&process.cgroup)
            })
            .map(|(pid, _)| *pid)
            .collect();
        loop {
            let before = owned.len();
            for (pid, process) in &self.0 {
                if owned.contains(&process.parent) {
                    owned.insert(*pid);
                }
            }
            if owned.len() == before {
                return owned;
            }
        }
    }

    /// Whether a command the worker of `session` started is alive: an owned process
    /// below the worker with a shell between them, or below a worker that is a shell,
    /// or one that left the worker's tree, such as a command reparented to init.
    fn commands_running(&self, session: &str, scopes: &[String]) -> bool {
        let owned = self.owned(session, scopes);
        // Owned processes whose parent is not owned. The oldest is the worker; any
        // other detached from it.
        let roots: Vec<u32> = owned
            .iter()
            .copied()
            .filter(|pid| !owned.contains(&self.0[pid].parent))
            .collect();
        let worker = roots
            .iter()
            .copied()
            .min_by_key(|pid| (self.0[pid].started, *pid));
        let Some(worker) = worker else { return false };
        if roots.len() > 1 {
            return true;
        }
        let startup = self.0[&worker].started + STARTUP.as_secs() * clock_ticks();
        owned.iter().any(|pid| {
            if *pid != worker && self.0[pid].started > startup {
                return true;
            }
            let mut current = *pid;
            let mut below_shell = false;
            loop {
                let Some(process) = self.0.get(&current) else {
                    return false;
                };
                if !owned.contains(&process.parent) {
                    // `current` is the worker itself. It is no command of its own.
                    return current != *pid && (below_shell || process.shell);
                }
                if process.command {
                    return true;
                }
                if current != *pid && process.shell {
                    below_shell = true;
                }
                current = process.parent;
            }
        })
    }
}

fn clock_ticks() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks)
        .ok()
        .filter(|ticks| *ticks > 0)
        .unwrap_or(100)
}

/// Every live process this user can read.
fn scan() -> Processes {
    let mut processes = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Processes(processes);
    };
    for pid in entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    {
        if let Some(process) = read(pid) {
            processes.insert(pid, process);
        }
    }
    Processes(processes)
}

fn read(pid: u32) -> Option<Process> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // proc_pid_stat(5): the name may contain spaces, so fields follow the last ')'.
    let (head, tail) = stat.rsplit_once(')')?;
    let name = head.split_once('(')?.1;
    let mut fields = tail.split_whitespace();
    if matches!(fields.next()?, "Z" | "X") {
        return None;
    }
    let parent = fields.next()?.parse().ok()?;
    // Fields after the state: ppid is the first, starttime the 19th.
    let started = fields.nth(17)?.parse().ok()?;
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()
        .and_then(|cgroup| {
            cgroup
                .lines()
                .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
        })
        .unwrap_or_default();
    let session = fs::read(format!("/proc/{pid}/environ"))
        .ok()
        .and_then(|environ| {
            environ.split(|byte| *byte == 0).find_map(|item| {
                let value = item.strip_prefix(b"POWER_SESSION=")?;
                String::from_utf8(value.to_vec()).ok()
            })
        });
    Some(Process {
        parent,
        started,
        shell: SHELLS.contains(&name),
        command: COMMANDS.contains(&name),
        cgroup,
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(parent: u32, name: &str, session: Option<&str>) -> Process {
        Process {
            parent,
            started: 0,
            shell: SHELLS.contains(&name),
            command: COMMANDS.contains(&name),
            cgroup: String::new(),
            session: session.map(Into::into),
        }
    }

    #[test]
    fn reads_this_process() {
        let me = read(std::process::id()).unwrap();
        assert_eq!(me.parent, std::os::unix::process::parent_id());
    }

    #[test]
    fn a_quiet_command_under_a_shell_counts_and_an_agent_helper_does_not() {
        // agent (10) -> mcp server (11), agent -> bash (12) -> cargo (13) -> test (14).
        let mut processes = Processes(BTreeMap::from([
            (1, process(0, "systemd", None)),
            (10, process(1, "jcode", Some("s"))),
            (11, process(10, "node", Some("s"))),
        ]));
        assert!(!processes.commands_running("s", &[]));
        processes.0.insert(12, process(10, "bash", Some("s")));
        // A shell the agent holds open runs no command yet.
        assert!(!processes.commands_running("s", &[]));
        processes.0.insert(13, process(12, "cargo", Some("s")));
        // The test cleared its environment, and is still owned through its parent.
        processes
            .0
            .insert(14, process(13, "integration-test", None));
        processes.0.remove(&13);
        processes.0.get_mut(&14).unwrap().parent = 12;
        assert!(processes.commands_running("s", &[]));
        assert!(!processes.commands_running("other", &[]));
        // `bash -c` execs a lone command, so it runs right under the agent. Started
        // after the agent's startup, it counts too.
        processes.0.remove(&12);
        processes.0.remove(&14);
        assert!(!processes.commands_running("s", &[]));
        let mut exec = process(10, "cargo", Some("s"));
        exec.started = 31 * clock_ticks();
        processes.0.insert(15, exec);
        assert!(processes.commands_running("s", &[]));
    }

    #[test]
    fn a_build_execed_during_startup_counts_while_it_runs() {
        // agent (10) execs `cargo test` (11) two seconds in, which runs a test binary
        // (12) with a hashed name. Both are quiet and well inside the startup window.
        let mut processes = Processes(BTreeMap::from([
            (1, process(0, "systemd", None)),
            (10, process(1, "jcode", Some("s"))),
        ]));
        let mut cargo = process(10, "cargo", Some("s"));
        cargo.started = 2 * clock_ticks();
        processes.0.insert(11, cargo);
        assert!(processes.commands_running("s", &[]));
        let mut test = process(11, "power-1a2b3c4d", None);
        test.started = 3 * clock_ticks();
        processes.0.insert(12, test);
        assert!(processes.commands_running("s", &[]));
        // Cargo exited and the test binary was reparented to init: still owned through
        // the cgroup, and detached from the worker, so it counts.
        processes.0.remove(&11);
        processes.0.get_mut(&12).unwrap().parent = 1;
        for pid in [10, 12] {
            processes.0.get_mut(&pid).unwrap().cgroup = "/power-s.scope".into();
        }
        assert!(processes.commands_running("s", &["/power-s.scope".into()]));
        // Once it ends, an early helper alone does not hide the stall.
        processes.0.remove(&12);
        let mut helper = process(10, "node", Some("s"));
        helper.started = 2 * clock_ticks();
        processes.0.insert(13, helper);
        assert!(!processes.commands_running("s", &["/power-s.scope".into()]));
    }

    #[test]
    fn a_shell_worker_counts_any_child_and_cgroups_own_reparented_processes() {
        let mut processes = Processes(BTreeMap::from([
            (1, process(0, "systemd", None)),
            (20, process(1, "sh", Some("w"))),
        ]));
        assert!(!processes.commands_running("w", &[]));
        processes.0.insert(21, process(20, "sleep", Some("w")));
        assert!(processes.commands_running("w", &[]));
        // A detached command that cleared its environment, reparented to init, is
        // still owned through the session's cgroup.
        processes.0.remove(&21);
        let mut detached = process(1, "make", None);
        detached.started = 5;
        detached.cgroup = "/app.slice/power-w-1.scope".into();
        processes.0.insert(22, detached);
        processes.0.get_mut(&20).unwrap().cgroup = "/app.slice/power-w-1.scope".into();
        let mut child = process(22, "cc", None);
        child.cgroup = "/app.slice/power-w-1.scope".into();
        processes.0.insert(23, child);
        assert!(processes.commands_running("w", &["/app.slice/power-w-1.scope".into()]));
    }
}
