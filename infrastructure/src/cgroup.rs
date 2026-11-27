//! Own each worker through a cgroup. Every launch runs in its own transient systemd
//! user scope, so everything it starts stays in that scope's cgroup however it
//! detaches: a new session, a clean environment and reparenting to init all leave
//! cgroup membership alone. Ending a worker kills its cgroup, and the worker has
//! exited only once that cgroup is empty.
//!
//! Scopes are named `power-<session>-<nonce>.scope`, so a daemon that restarts can
//! find what an earlier daemon left of a session and end it. The nonce keeps a
//! relaunch from colliding with a scope that systemd has not yet collected.
//!
//! The same cgroup is the primitive for pausing a worker (`cgroup.freeze`).

use anyhow::{Context, Result, bail, ensure};
use portable_pty::CommandBuilder;
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::process_tree::{Pinned, Sent};

/// The cgroup v2 mount.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// The slice that holds every worker scope, under the user manager's `app.slice`.
pub(crate) const DEFAULT_SLICE: &str = "app-power.slice";
/// Names another slice, so isolated tests never share the daemon's.
/// How many passes `signal_all` makes for processes forked while it signals.
const SIGNAL_ROUNDS: usize = 16;
pub(crate) const SLICE_ENV: &str = "POWER_WORKER_SLICE";
/// Set to `0` to run workers without cgroup ownership, under the subreaper fallback.
pub(crate) const DISABLE_ENV: &str = "POWER_WORKER_CGROUPS";
/// How often a teardown checks whether the cgroup is empty.
const POLL: Duration = Duration::from_millis(20);
/// How often a teardown logs a cgroup that outlives `cgroup.kill`.
const STUCK_LOG: Duration = Duration::from_secs(10);
/// How long a teardown keeps killing a cgroup that does not empty before it reports
/// the teardown as incomplete and leaves the scope for a later reclaim.
const KILL_LIMIT: Duration = Duration::from_secs(10);
/// How many `cgroup.kill` writes in a row may fail before a teardown stops retrying.
/// A write that keeps failing, such as on a read-only mount, will not start working.
const KILL_ERRORS: u32 = 5;
/// How long detection waits for the user manager to answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Starts worker processes in systemd user scopes and finds them again.
#[derive(Debug)]
pub(crate) struct WorkerScopes {
    systemd_run: PathBuf,
    /// systemd 254 and later expand `$NAME` in a command line unless told not to.
    no_expand: bool,
    slice: String,
    /// The slice's cgroup directory.
    slice_dir: PathBuf,
    /// The runtime directory that holds the user manager's socket. `systemd-run`
    /// gets it even when workers run with another `XDG_RUNTIME_DIR`.
    runtime_dir: PathBuf,
}

impl WorkerScopes {
    /// Checks that this system can own workers through cgroups: cgroup v2 alone, a
    /// reachable systemd user manager, `systemd-run`, and a kernel with `cgroup.kill`.
    /// The error says what is missing.
    pub(crate) fn detect(slice: &str) -> Result<Self> {
        let own =
            fs::read_to_string("/proc/self/cgroup").context("Could not read /proc/self/cgroup")?;
        ensure!(
            own.lines().count() == 1 && own.starts_with("0::"),
            "cgroup v2 is not the only cgroup hierarchy"
        );
        ensure!(
            Path::new(CGROUP_ROOT).join("cgroup.controllers").exists(),
            "cgroup v2 is not mounted at {CGROUP_ROOT}"
        );
        let systemd_run = find_program(OsStr::new("systemd-run"), std::env::var_os("PATH"), None)
            .context("systemd-run is not installed")?;
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .into_iter()
            .chain([PathBuf::from(format!("/run/user/{uid}"))])
            .find(|dir| dir.join("systemd/private").exists())
            .context("No systemd user manager socket was found")?;
        let manager = probe(
            Command::new("systemctl")
                .args(["--user", "show", "--property=ControlGroup", "--value"])
                .env("XDG_RUNTIME_DIR", &runtime_dir),
        )
        .context("No systemd user manager answered")?;
        let manager = manager.trim();
        ensure!(
            manager.starts_with('/'),
            "The systemd user manager reported no control group"
        );
        let manager_dir = Path::new(CGROUP_ROOT).join(manager.trim_start_matches('/'));
        ensure!(
            manager_dir.join("cgroup.kill").exists(),
            "This kernel has no cgroup.kill, which needs Linux 5.14"
        );
        let version = probe(Command::new(&systemd_run).arg("--version"))
            .context("Could not read the systemd version")?;
        let version: u32 = version
            .split_whitespace()
            .nth(1)
            .and_then(|version| version.parse().ok())
            .context("Could not parse the systemd version")?;
        Ok(Self {
            systemd_run,
            no_expand: version >= 254,
            slice_dir: manager_dir.join(slice_path(slice)?),
            slice: slice.to_owned(),
            runtime_dir,
        })
    }

    pub(crate) fn slice(&self) -> &str {
        &self.slice
    }

    /// Makes `command` start in a new scope of `session`, and returns that scope. Its
    /// program is resolved first, so a missing program fails here as an unwrapped
    /// spawn would. `systemd-run` execs the program in place, so the PID stays the
    /// program's own.
    pub(crate) fn wrap(&self, command: &mut CommandBuilder, session: &str) -> Result<Scope> {
        let program = command
            .get_argv()
            .first()
            .cloned()
            .context("The command has no program")?;
        let path = command
            .get_env("PATH")
            .map(OsStr::to_owned)
            .or_else(|| std::env::var_os("PATH"));
        let cwd = command.get_cwd().map(PathBuf::from);
        let resolved = find_program(&program, path, cwd.as_deref()).with_context(|| {
            format!(
                "Unable to spawn {} because it doesn't exist",
                program.to_string_lossy()
            )
        })?;
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let unit = format!("{}{}.scope", unit_prefix(session), &nonce[..12]);
        let mut wrapper: Vec<OsString> = vec![
            self.systemd_run.clone().into(),
            "--user".into(),
            "--scope".into(),
            "--quiet".into(),
            "--collect".into(),
        ];
        if self.no_expand {
            wrapper.push("--expand-environment=no".into());
        }
        // systemd-run finds the manager through XDG_RUNTIME_DIR, and hands the
        // program the one it was meant to have.
        let runtime = command.get_env("XDG_RUNTIME_DIR").map(OsStr::to_owned);
        if runtime.as_deref() != Some(self.runtime_dir.as_os_str()) {
            if let Some(runtime) = runtime {
                let mut setenv = OsString::from("--setenv=XDG_RUNTIME_DIR=");
                setenv.push(runtime);
                wrapper.push(setenv);
            }
            command.env("XDG_RUNTIME_DIR", &self.runtime_dir);
        }
        wrapper.push(format!("--slice={}", self.slice).into());
        wrapper.push(format!("--unit={unit}").into());
        wrapper.push("--".into());
        let argv = command.get_argv_mut();
        // systemd-run resolves the program against its own PATH and directory, so it
        // gets the one this launch resolved.
        argv[0] = resolved.into_os_string();
        argv.splice(0..0, wrapper);
        Ok(Scope::at(self.slice_dir.join(&unit)))
    }

    /// Scopes an earlier launch of `session` left behind, which may still hold
    /// processes after a daemon restart.
    /// A slice that cannot be read is an error, since its scopes may still run.
    pub(crate) fn leftovers(&self, session: &str) -> Result<Vec<Scope>> {
        let prefix = unit_prefix(session);
        Ok(self
            .scope_names()?
            .into_iter()
            .filter(|name| name.starts_with(&prefix))
            .map(|name| Scope::at(self.slice_dir.join(name)))
            .collect())
    }

    /// Every scope name in the slice. A slice systemd has not created holds none.
    fn scope_names(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.slice_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Could not read {}", self.slice_dir.display()));
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry =
                entry.with_context(|| format!("Could not read {}", self.slice_dir.display()))?;
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with("power-")
                && name.ends_with(".scope")
            {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    }

    /// Ends everything left in `session`'s scopes, waiting up to `timeout` for them
    /// to empty. Returns how many scopes still held processes.
    pub(crate) fn reclaim(&self, session: &str, timeout: Duration) -> Result<usize> {
        reclaim_scopes(self.leftovers(session)?, timeout)
    }
}

/// Ends everything left in the scopes at `paths`, which an earlier daemon recorded,
/// in whatever slice it used. This needs no systemd, only `cgroup.kill`, so a daemon
/// that now runs without cgroup ownership still ends them.
pub(crate) fn reclaim_paths(paths: &[PathBuf], timeout: Duration) -> Result<usize> {
    reclaim_scopes(paths.iter().cloned().map(Scope::at).collect(), timeout)
}

fn reclaim_scopes(scopes: Vec<Scope>, timeout: Duration) -> Result<usize> {
    let scopes: Vec<Scope> = scopes.into_iter().filter(Scope::populated).collect();
    for scope in &scopes {
        eprintln!(
            "Ending processes that an earlier daemon left in {}.",
            scope.name()
        );
        let _ = scope.thaw();
        scope
            .kill()
            .with_context(|| format!("Could not kill {}", scope.name()))?;
    }
    let deadline = Instant::now() + timeout;
    while scopes.iter().any(Scope::populated) {
        if Instant::now() >= deadline {
            let stuck: Vec<_> = scopes
                .iter()
                .filter(|scope| scope.populated())
                .map(Scope::name)
                .collect();
            bail!("{} still hold processes after SIGKILL", stuck.join(", "));
        }
        let _ = scopes.iter().try_for_each(Scope::kill);
        thread::sleep(POLL);
    }
    Ok(scopes.len())
}

/// The start of every unit name for `session`. Characters a unit name cannot hold
/// become `_`.
fn unit_prefix(session: &str) -> String {
    let session: String = session
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || char == '-' {
                char
            } else {
                '_'
            }
        })
        .collect();
    format!("power-{session}-")
}

/// The cgroup directory of `slice` relative to the user manager's: systemd nests
/// `a-b.slice` inside `a.slice`.
fn slice_path(slice: &str) -> Result<PathBuf> {
    let name = slice
        .strip_suffix(".slice")
        .filter(|name| {
            !name.is_empty()
                && !name.starts_with('-')
                && !name.ends_with('-')
                && !name.contains("--")
                && name
                    .chars()
                    .all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '_'))
        })
        .with_context(|| format!("{slice:?} is not a slice name like app-power.slice"))?;
    let mut path = PathBuf::new();
    let mut prefix = String::new();
    for part in name.split('-') {
        if !prefix.is_empty() {
            prefix.push('-');
        }
        prefix.push_str(part);
        path.push(format!("{prefix}.slice"));
    }
    Ok(path)
}

/// Runs a short probe and returns its standard output, or fails if it does not
/// succeed within `PROBE_TIMEOUT`.
fn probe(command: &mut Command) -> Result<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{command:?} did not answer within {PROBE_TIMEOUT:?}");
        }
        thread::sleep(POLL);
    };
    ensure!(status.success(), "{command:?} failed ({status})");
    let mut output = String::new();
    io::Read::read_to_string(&mut child.stdout.take().context("no output")?, &mut output)?;
    Ok(output)
}

/// Resolves a program the way `execvp` does: a name with a slash is a path, which
/// is relative to `cwd`, and any other name is looked up in `path`.
pub(crate) fn find_program(
    program: &OsStr,
    path: Option<OsString>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    let executable = |candidate: &Path| {
        fs::metadata(candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    let program = Path::new(program);
    if program.as_os_str().as_encoded_bytes().contains(&b'/') {
        let candidate = match cwd {
            Some(cwd) if program.is_relative() => cwd.join(program),
            _ => program.to_owned(),
        };
        return executable(&candidate).then_some(candidate);
    }
    std::env::split_paths(&path?)
        .map(|directory| {
            // execvp searches a relative entry, the empty one included, from the
            // directory the program starts in.
            let directory = match cwd {
                Some(cwd) if directory.is_relative() => cwd.join(directory),
                _ => directory,
            };
            directory.join(program)
        })
        .find(|candidate| executable(candidate))
}

/// One worker launch's cgroup. A scope that systemd has removed is empty.
#[derive(Debug)]
pub(crate) struct Scope {
    path: PathBuf,
    /// The cgroup path as `/proc/<pid>/cgroup` names it.
    relative: String,
}

impl Scope {
    fn at(path: PathBuf) -> Self {
        let relative = path
            .strip_prefix(CGROUP_ROOT)
            .map(|relative| format!("/{}", relative.display()))
            .unwrap_or_default();
        Self { path, relative }
    }

    /// The scope's unit name.
    pub(crate) fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Another handle on the scope at `path`.
    #[cfg(test)]
    pub(crate) fn at_path(path: &Path) -> Self {
        Self::at(path.to_owned())
    }

    /// Whether systemd has created the scope and not yet removed it.
    #[cfg(test)]
    pub(crate) fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Reads a `key value` line from `cgroup.events`.
    fn event(&self, key: &str) -> io::Result<Option<bool>> {
        match fs::read_to_string(self.path.join("cgroup.events")) {
            Ok(events) => Ok(events
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
                .map(|value| value.trim() == "1")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether any process is in the scope or a cgroup below it. A scope systemd has
    /// already removed is empty. One that cannot be read counts as populated, so a
    /// teardown keeps waiting rather than reporting a live worker as gone.
    pub(crate) fn populated(&self) -> bool {
        match self.event("populated") {
            Ok(populated) => populated.unwrap_or(false),
            Err(error) => {
                eprintln!("Could not read {}. {error}", self.path.display());
                true
            }
        }
    }

    fn write(&self, file: &str, value: &str) -> io::Result<()> {
        match fs::write(self.path.join(file), value) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    /// Kills every process in the scope and below it at once, including processes
    /// forked while the kill runs, frozen ones and ones that changed user.
    pub(crate) fn kill(&self) -> io::Result<()> {
        self.write("cgroup.kill", "1")
    }

    /// Pauses every process in the scope. They cannot catch or notice it.
    #[cfg(test)]
    pub(crate) fn freeze(&self) -> io::Result<()> {
        self.write("cgroup.freeze", "1")
    }

    /// Resumes a frozen scope.
    pub(crate) fn thaw(&self) -> io::Result<()> {
        self.write("cgroup.freeze", "0")
    }

    /// Whether every process in the scope is frozen.
    #[cfg(test)]
    pub(crate) fn frozen(&self) -> bool {
        self.event("frozen").ok().flatten().unwrap_or(false)
    }

    /// The PIDs in the scope and every cgroup below it.
    fn pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        let mut directories = vec![self.path.clone()];
        while let Some(directory) = directories.pop() {
            if let Ok(procs) = fs::read_to_string(directory.join("cgroup.procs")) {
                pids.extend(
                    procs
                        .split_whitespace()
                        .filter_map(|pid| pid.parse::<u32>().ok()),
                );
            }
            if let Ok(entries) = fs::read_dir(&directory) {
                directories.extend(
                    entries
                        .flatten()
                        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                        .map(|entry| entry.path()),
                );
            }
        }
        pids
    }

    /// Whether `pid` is in this scope right now.
    fn contains(&self, pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/cgroup")).is_ok_and(|cgroup| {
            cgroup.lines().any(|line| {
                line.strip_prefix("0::").is_some_and(|path| {
                    path.strip_prefix(self.relative.as_str())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
                })
            })
        })
    }

    /// Sends `signal` to each process in the scope that is not in `sent`, and adds it
    /// there. Each process is pinned by a pidfd and checked for membership after the
    /// pin, so a PID reused by a process outside the scope is never signalled. A
    /// process that cannot be pinned is left to `kill`, which needs no PID.
    fn signal(&self, signal: libc::c_int, sent: &mut BTreeSet<u32>) {
        for pid in self.pids() {
            if sent.contains(&pid) {
                continue;
            }
            let Some(pinned) = Pinned::member(pid) else {
                continue;
            };
            // The pin is alive after the membership check, so the PID still named
            // the pinned process when the check ran.
            if !self.contains(pid) || pinned.exited() {
                continue;
            }
            if pinned.signal(signal) == Sent::Delivered {
                if signal == libc::SIGTERM {
                    pinned.signal(libc::SIGCONT);
                }
                sent.insert(pid);
            }
        }
    }

    /// Sends `signal` to every process in the scope, including ones forked while it
    /// is sent, and returns how many received it. Only scope members are signalled.
    pub(crate) fn signal_all(&self, signal: libc::c_int) -> usize {
        let mut sent = BTreeSet::new();
        for _ in 0..SIGNAL_ROUNDS {
            let before = sent.len();
            self.signal(signal, &mut sent);
            if sent.len() == before {
                break;
            }
        }
        sent.len()
    }

    /// Asks every process to exit, waits up to `grace` for the scope to empty, then
    /// kills the cgroup. `root` is the launched process, which may still be
    /// `systemd-run` and outside the scope. `Ok` means the root has exited and the
    /// scope is empty: after the root has exited, nothing new can join the scope.
    /// Killing is retried for a bounded time, and stops early when `cgroup.kill` keeps
    /// failing. The error then names the scope, which stays for a later reclaim.
    pub(crate) fn terminate(&self, root: &Pinned, grace: Duration) -> Result<(), String> {
        let gone = || root.exited() && !self.populated();
        let mut sent = BTreeSet::new();
        if !root.exited() && root.signal(libc::SIGTERM) == Sent::Delivered {
            root.signal(libc::SIGCONT);
        }
        self.signal(libc::SIGTERM, &mut sent);
        // A frozen process handles SIGTERM only once it runs again. It is thawed
        // only after the end is pending, so it never resumes ordinary work first.
        if let Err(error) = self.thaw() {
            eprintln!("Could not thaw {}. {error}", self.name());
        }
        let deadline = Instant::now() + grace;
        while !gone() && Instant::now() < deadline {
            // Processes forked since the last pass are asked too.
            self.signal(libc::SIGTERM, &mut sent);
            thread::sleep(POLL);
        }
        let killing = Instant::now();
        let mut logged = killing;
        let mut failures = 0;
        while !gone() {
            if !root.exited() {
                root.signal(libc::SIGKILL);
            }
            match self.kill() {
                Ok(()) => failures = 0,
                Err(error) => {
                    failures += 1;
                    if failures == 1 {
                        eprintln!("Could not kill {}. {error}", self.name());
                    }
                    if failures >= KILL_ERRORS {
                        return Err(self.stuck(&format!("cgroup.kill keeps failing ({error})")));
                    }
                }
            }
            if killing.elapsed() >= KILL_LIMIT {
                return Err(self.stuck("it outlived cgroup.kill"));
            }
            thread::sleep(POLL);
            if logged.elapsed() >= STUCK_LOG {
                logged = Instant::now();
                eprintln!(
                    "Still waiting for {} to empty after cgroup.kill. Processes {:?} remain.",
                    self.name(),
                    self.pids()
                );
            }
        }
        Ok(())
    }

    fn stuck(&self, why: &str) -> String {
        format!(
            "processes {:?} in {}, kept for a later reclaim because {why}",
            self.pids(),
            self.name()
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Child;

    /// Scopes in a slice of their own, which the guard stops when dropped.
    pub(crate) struct TestScopes {
        pub(crate) scopes: WorkerScopes,
    }

    impl TestScopes {
        pub(crate) fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let slice = format!(
                "app-powertest{}x{}.slice",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let scopes = WorkerScopes::detect(&slice).expect(
                "the cgroup tests need cgroup v2 and a systemd user manager, which own workers on Linux",
            );
            Self { scopes }
        }
    }

    impl Drop for TestScopes {
        fn drop(&mut self) {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", &self.scopes.slice])
                .stderr(Stdio::null())
                .status();
        }
    }

    fn running(pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().next())
                .is_some_and(|state| !matches!(state, "Z" | "X"))
        })
    }

    fn read_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "the fixture never wrote its PID");
            thread::sleep(POLL);
        }
    }

    /// Runs `script` through `wrap`, as the terminal runtime does.
    fn spawn(scopes: &WorkerScopes, session: &str, script: &str) -> (Child, Scope) {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        let scope = scopes.wrap(&mut command, session).unwrap();
        let argv = command.get_argv();
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        (child, scope)
    }

    fn wait_populated(scope: &Scope) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !scope.populated() {
            assert!(Instant::now() < deadline, "{} never started", scope.name());
            thread::sleep(POLL);
        }
    }

    #[test]
    fn a_detached_helper_with_a_clean_environment_stays_in_the_scope_until_terminate() {
        let test = TestScopes::new();
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("helper");
        // The helper leaves the session, drops its whole environment and is
        // reparented when the root exits, which is what process discovery missed.
        let script = format!(
            "setsid -f env -i /bin/sh -c 'echo $$ > {0}.tmp; mv {0}.tmp {0}; exec sleep 600'",
            pid_file.display()
        );
        let (mut child, scope) = spawn(&test.scopes, "detach-test", &script);
        let root = Pinned::child(child.id());
        let helper = read_pid(&pid_file);
        child.wait().unwrap();
        assert!(running(helper));
        assert_ne!(
            fs::read_to_string(format!("/proc/{helper}/stat"))
                .unwrap()
                .split_whitespace()
                .nth(3),
            Some(child.id().to_string().as_str()),
            "the helper was reparented"
        );
        assert!(
            !fs::read(format!("/proc/{helper}/environ"))
                .unwrap()
                .windows(14)
                .any(|window| window == b"POWER_SESSION=")
        );
        assert!(scope.populated(), "the helper left the scope");
        assert!(scope.contains(helper));
        assert_eq!(test.scopes.leftovers("detach-test").unwrap().len(), 1);

        scope.terminate(&root, Duration::from_millis(300)).unwrap();
        assert!(!scope.populated());
        assert!(!running(helper), "{helper} survived");
    }

    #[test]
    fn terminate_kills_what_ignores_sigterm_after_the_grace() {
        let test = TestScopes::new();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        // SIGTERM stays ignored across exec, so the child ignores it too.
        let script = format!(
            "trap '' TERM; sleep 600 & echo $$ > {0}.tmp; mv {0}.tmp {0}; wait",
            ready.display()
        );
        let (mut child, scope) = spawn(&test.scopes, "stubborn-test", &script);
        let root = Pinned::child(child.id());
        read_pid(&ready);
        let started = Instant::now();
        scope.terminate(&root, Duration::from_millis(300)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!scope.populated());
        child.wait().unwrap();
    }

    #[test]
    fn reclaim_ends_what_an_earlier_launch_of_the_session_left() {
        let test = TestScopes::new();
        let (mut first, first_scope) = spawn(&test.scopes, "restart-test", "setsid -f sleep 600");
        let (mut other, other_scope) = spawn(&test.scopes, "other-test", "sleep 600");
        first.wait().unwrap();
        wait_populated(&first_scope);
        wait_populated(&other_scope);
        // The daemon that launched it is gone, so only the name finds it.
        assert_eq!(
            test.scopes
                .reclaim("restart-test", Duration::from_secs(5))
                .unwrap(),
            1
        );
        assert!(!first_scope.populated());
        assert!(
            other_scope.populated(),
            "another session's scope was touched"
        );
        other.kill().unwrap();
        other.wait().unwrap();
    }

    #[test]
    fn freeze_and_thaw_pause_the_whole_scope_and_terminate_ends_a_frozen_one() {
        let test = TestScopes::new();
        let (mut child, scope) = spawn(
            &test.scopes,
            "freeze-test",
            "setsid -f sleep 600; sleep 600",
        );
        let root = Pinned::child(child.id());
        wait_populated(&scope);
        let settle = |frozen: bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while scope.frozen() != frozen {
                assert!(Instant::now() < deadline, "the scope never settled");
                thread::sleep(POLL);
            }
        };
        scope.freeze().unwrap();
        settle(true);
        scope.thaw().unwrap();
        settle(false);
        scope.freeze().unwrap();
        settle(true);
        scope.terminate(&root, Duration::from_millis(300)).unwrap();
        assert!(!scope.populated());
        child.wait().unwrap();
    }

    #[test]
    fn a_failing_cgroup_kill_ends_the_teardown_with_an_error_and_keeps_the_scope() {
        use std::os::unix::fs::PermissionsExt;
        let test = TestScopes::new();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        // Both the root and its child ignore SIGTERM, so only cgroup.kill could end them.
        let script = format!(
            "trap '' TERM; setsid -f sleep 600; echo $$ > {0}.tmp; mv {0}.tmp {0}; exec sleep 600",
            ready.display()
        );
        let (mut child, scope) = spawn(&test.scopes, "readonly-test", &script);
        let root = Pinned::child(child.id());
        read_pid(&ready);
        let kill = scope.path().join("cgroup.kill");
        fs::set_permissions(&kill, fs::Permissions::from_mode(0o444)).unwrap();
        let started = Instant::now();
        // The root dies by its pidfd, but the scope keeps the detached child.
        let ended = scope.terminate(&root, Duration::from_millis(100));
        let took = started.elapsed();
        fs::set_permissions(&kill, fs::Permissions::from_mode(0o644)).unwrap();
        let error = ended.expect_err("an incomplete teardown reported success");
        assert!(error.contains("readonly-test"), "{error}");
        assert!(took < Duration::from_secs(2), "kept retrying for {took:?}");
        assert!(scope.populated(), "the scope was not kept");
        assert_eq!(
            test.scopes
                .reclaim("readonly-test", Duration::from_secs(5))
                .unwrap(),
            1
        );
        child.wait().unwrap();
    }

    #[test]
    fn an_unreadable_slice_is_a_reclaim_error() {
        use std::os::unix::fs::PermissionsExt;
        let test = TestScopes::new();
        let (mut child, scope) = spawn(&test.scopes, "hidden-test", "sleep 600");
        wait_populated(&scope);
        let slice = &test.scopes.slice_dir;
        let mode = fs::metadata(slice).unwrap().permissions().mode();
        fs::set_permissions(slice, fs::Permissions::from_mode(0o300)).unwrap();
        let reclaimed = test.scopes.reclaim("hidden-test", Duration::from_secs(1));
        fs::set_permissions(slice, fs::Permissions::from_mode(mode)).unwrap();
        assert!(reclaimed.is_err(), "an unreadable slice reclaimed nothing");
        assert_eq!(test.scopes.leftovers("hidden-test").unwrap().len(), 1);
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_relative_path_entry_resolves_against_the_working_directory() {
        use std::os::unix::fs::PermissionsExt;
        let test = TestScopes::new();
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("local-bin");
        fs::create_dir(&bin).unwrap();
        let program = bin.join("power-test-local");
        let marker = directory.path().join("ran");
        fs::write(&program, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = CommandBuilder::new("power-test-local");
        command.env("PATH", "local-bin:/usr/bin:/bin");
        command.cwd(directory.path());
        // The daemon's own directory has no local-bin.
        assert_ne!(std::env::current_dir().unwrap(), directory.path());
        test.scopes.wrap(&mut command, "relative-test").unwrap();
        assert!(
            command
                .get_argv()
                .iter()
                .any(|arg| arg == program.as_os_str())
        );
        let argv = command.get_argv();
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .envs([("PATH", "local-bin:/usr/bin:/bin")])
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(marker.exists(), "the local program did not run");
    }

    #[test]
    fn a_missing_program_fails_before_anything_is_spawned() {
        let test = TestScopes::new();
        let mut command = CommandBuilder::new("power-test-no-such-program");
        assert!(test.scopes.wrap(&mut command, "missing-test").is_err());
    }

    #[test]
    fn slices_nest_by_dash() {
        assert_eq!(
            slice_path("app-power.slice").unwrap(),
            PathBuf::from("app.slice/app-power.slice")
        );
        assert!(slice_path("app--x.slice").is_err());
        assert!(slice_path("../x.slice").is_err());
        assert!(slice_path("app-power").is_err());
    }
}
