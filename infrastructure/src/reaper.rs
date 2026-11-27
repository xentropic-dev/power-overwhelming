//! The fallback's whole-tree owner, for systems without cgroup ownership. Each launch
//! runs under `power reap`, which marks itself a child subreaper and starts the real
//! program. Whatever the program starts, however it detaches (a new session, a clean
//! environment, reparenting), is reparented to the reaper and not to init once its
//! parent exits. The reaper exits only after every process it adopted has exited, so
//! the launched process's exit means the whole tree is gone, as a cgroup's emptiness
//! does.
//!
//! When the program exits, or the reaper is asked to end with SIGTERM or SIGHUP, the
//! reaper asks each of its children to exit, waits up to a grace, then kills them.
//! Only its own unreaped children are signalled, by PID, which cannot be reused until
//! they are reaped. Grandchildren are reached as their parents die and they are
//! reparented to the reaper.

use crate::process_tree::Pinned;
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    fs,
    os::fd::OwnedFd,
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// The hidden subcommand that runs the reaper.
pub const SUBCOMMAND: &str = "reap";
/// How long the reaper's children have to exit after SIGTERM before they are killed.
pub(crate) const GRACE: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(20);

/// Runs `argv` under a reaper and exits with its status once every process it
/// started has exited.
pub fn run(argv: Vec<OsString>) -> Result<std::convert::Infallible> {
    let (program, args) = argv.split_first().context("Nothing to run")?;
    // SAFETY: prctl with PR_SET_CHILD_SUBREAPER takes an integer flag.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        bail!(
            "Could not become a child subreaper. {}",
            std::io::Error::last_os_error()
        );
    }
    // Handlers, not ignored dispositions, so the program starts with the defaults.
    // Terminal signals reach the program through its process group; the reaper only
    // survives them.
    let requested = Arc::new(AtomicI32::new(0));
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        let requested = requested.clone();
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            signal_hook::low_level::register(signal, move || {
                requested.store(signal, Ordering::Release);
            })
        }
        .context("Install the reaper's end handler")?;
    }
    for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
        // SAFETY: an empty handler.
        unsafe { signal_hook::low_level::register(signal, || {}) }
            .context("Install the reaper's terminal signal handlers")?;
    }
    let mut command = Command::new(program);
    command.args(args);
    // SAFETY: the hook only makes async-signal-safe calls. The program leads its own
    // process group in the foreground of the terminal, as it would without the reaper,
    // so job control and foreground tracking see it.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            let mut block: libc::sigset_t = std::mem::zeroed();
            let mut previous: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut block);
            libc::sigaddset(&mut block, libc::SIGTTOU);
            libc::sigprocmask(libc::SIG_BLOCK, &block, &mut previous);
            libc::tcsetpgrp(0, libc::getpid());
            libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("Could not start {}", program.to_string_lossy()))?;
    let main = child.id() as libc::pid_t;
    // The child is reaped through waitpid below, alongside the adopted processes.
    std::mem::forget(child);
    let mut status = None;
    let mut ending: Option<(libc::c_int, Instant)> = None;
    loop {
        // Reap everything that has exited.
        loop {
            let mut raw = 0;
            // SAFETY: waitpid writes the status of an exited child.
            let pid = unsafe { libc::waitpid(-1, &mut raw, libc::WNOHANG) };
            if pid > 0 {
                if pid == main {
                    status = Some(raw);
                }
                continue;
            }
            if pid < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                exit(status);
            }
            break;
        }
        let signal = requested.swap(0, Ordering::AcqRel);
        if ending.is_none() && (status.is_some() || signal != 0) {
            let signal = if signal == 0 { libc::SIGTERM } else { signal };
            ending = Some((signal, Instant::now()));
            signal_children(signal);
        }
        if let Some((_, started)) = ending {
            // Children adopted since the first signal are asked too, then killed.
            let signal = if started.elapsed() >= GRACE {
                libc::SIGKILL
            } else {
                libc::SIGTERM
            };
            signal_children(signal);
        }
        thread::sleep(POLL);
    }
}

fn exit(status: Option<libc::c_int>) -> ! {
    let code = match status {
        Some(raw) if libc::WIFEXITED(raw) => libc::WEXITSTATUS(raw),
        Some(raw) if libc::WIFSIGNALED(raw) => 128 + libc::WTERMSIG(raw),
        _ => 1,
    };
    std::process::exit(code)
}

/// Signals the reaper's own children, which are unreaped, so their PIDs still name them.
fn signal_children(signal: libc::c_int) {
    for pid in children(std::process::id()) {
        // SAFETY: kill has no memory effects.
        unsafe { libc::kill(pid as libc::pid_t, signal) };
        if signal == libc::SIGTERM {
            // SAFETY: as above. A stopped child handles SIGTERM only once it runs.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) };
        }
    }
}

/// The PIDs whose parent is `parent`.
pub(crate) fn children(parent: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| parent_of(*pid) == Some(parent))
        .collect()
}

fn parent_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The program a reaper at `reaper` started, once it has started, so the terminal
/// follows the program rather than the reaper.
pub(crate) fn program_of(reaper: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(pid) = children(reaper).into_iter().min() {
            return pid;
        }
        if Instant::now() >= deadline {
            return reaper;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Makes `command` run under the reaper. `executable` is the power binary. The
/// program is resolved first, as an unwrapped spawn would, so one that does not
/// exist fails the launch here rather than later inside the reaper.
pub(crate) fn wrap(command: &mut portable_pty::CommandBuilder, executable: &Path) -> Result<()> {
    let program = command
        .get_argv()
        .first()
        .cloned()
        .context("The command has no program")?;
    let path = command
        .get_env("PATH")
        .map(std::ffi::OsStr::to_owned)
        .or_else(|| std::env::var_os("PATH"));
    let cwd = command.get_cwd().map(std::path::PathBuf::from);
    let resolved =
        crate::cgroup::find_program(&program, path, cwd.as_deref()).with_context(|| {
            format!(
                "Unable to spawn {} because it doesn't exist",
                program.to_string_lossy()
            )
        })?;
    let argv = command.get_argv_mut();
    argv[0] = resolved.into_os_string();
    argv.splice(
        0..0,
        [
            executable.as_os_str().to_owned(),
            SUBCOMMAND.into(),
            "--".into(),
        ],
    );
    Ok(())
}

/// Whether `pid` runs `power reap` with `marker` in its environment.
fn is_reaper(pid: u32, marker: &[u8]) -> bool {
    fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|cmdline| cmdline.split(|byte| *byte == 0).nth(1) == Some(SUBCOMMAND.as_bytes()))
        && fs::read(format!("/proc/{pid}/environ"))
            .is_ok_and(|environ| environ.split(|byte| *byte == 0).any(|item| item == marker))
}

/// What a search for an earlier daemon's reapers found.
#[derive(Debug, Default)]
struct Found {
    /// Reapers pinned by a pidfd that was opened before their identity was read and
    /// was still alive after, so signalling the pin reaches the reaper.
    pinned: Vec<Pinned>,
    /// Reapers that could not be pinned, by PID. They still run, so nothing may
    /// treat them as gone, and they are never signalled by PID.
    unpinned: Vec<(u32, std::io::Error)>,
}

/// Reapers an earlier daemon left for `session`, found by their owner marker.
/// `open` opens a pidfd, so a kernel that refuses one can be tested.
fn reapers(marker: &[u8], open: impl Fn(u32) -> std::io::Result<OwnedFd>) -> Found {
    let mut found = Found::default();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    let candidates = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        // A cheap first look, so only likely reapers are pinned.
        .filter(|pid| is_reaper(*pid, marker));
    for pid in candidates {
        match Pinned::pin_now(pid, &open) {
            // The pin comes first, then identity is read again, then the pin is
            // checked alive: had the PID been reused before the pin, the reads would
            // describe the pinned replacement, and had it been reused after, the pin
            // would show its own process exited.
            Ok(Some(pin)) => {
                if is_reaper(pid, marker) && !pin.exited() {
                    found.pinned.push(pin);
                }
            }
            Ok(None) => {}
            Err(error) => found.unpinned.push((pid, error)),
        }
    }
    found
}

/// Ends what reapers an earlier daemon left for the session marked `marker` still
/// run, waiting up to `timeout`. Each reaper ends its own tree. Returns how many
/// reapers were ended, or fails naming those that outlived the timeout or that
/// could not be pinned, which are left alone rather than signalled by PID.
pub(crate) fn reclaim(marker: &str, timeout: Duration) -> Result<usize> {
    reclaim_with(marker, timeout, crate::process_tree::pidfd_open)
}

fn reclaim_with(
    marker: &str,
    timeout: Duration,
    open: impl Fn(u32) -> std::io::Result<OwnedFd>,
) -> Result<usize> {
    let Found { pinned, unpinned } = reapers(marker.as_bytes(), open);
    if !unpinned.is_empty() {
        let names: Vec<String> = unpinned
            .iter()
            .map(|(pid, error)| format!("{pid} ({error})"))
            .collect();
        bail!(
            "reapers {} could not be pinned, so they were left running",
            names.join(", ")
        );
    }
    for reaper in &pinned {
        reaper.signal(libc::SIGTERM);
    }
    let deadline = Instant::now() + timeout;
    while pinned.iter().any(|reaper| !reaper.exited()) {
        if Instant::now() >= deadline {
            let stuck: Vec<_> = pinned
                .iter()
                .filter(|reaper| !reaper.exited())
                .map(Pinned::pid)
                .collect();
            bail!("reapers {stuck:?} still run");
        }
        thread::sleep(POLL);
    }
    Ok(pinned.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::process::CommandExt,
        process::{Child, Stdio},
    };

    /// Ends the fake reaper's process group, its sleep included.
    fn end_group(pid: u32) {
        // SAFETY: kill has no memory effects.
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }

    /// A process whose command line and environment look like a reaper's: `sh reap`
    /// runs a script named `reap`, and stays `sh reap` while it sleeps.
    fn fake_reaper(directory: &Path, marker: &str) -> Child {
        fs::write(directory.join(SUBCOMMAND), "sleep 600; :\n").unwrap();
        let (name, value) = marker.split_once('=').unwrap();
        let child = Command::new("/bin/sh")
            .arg(SUBCOMMAND)
            .current_dir(directory)
            .env(name, value)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !is_reaper(child.id(), marker.as_bytes()) {
            assert!(Instant::now() < deadline, "the fake reaper never started");
            thread::sleep(POLL);
        }
        child
    }

    fn running(pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().next())
                .is_some_and(|state| !matches!(state, "Z" | "X"))
        })
    }

    #[test]
    fn a_reaper_that_cannot_be_pinned_is_deferred_not_gone() {
        let directory = tempfile::tempdir().unwrap();
        let marker = format!("REAPER_TEST=unpinnable-{}", std::process::id());
        let mut reaper = fake_reaper(directory.path(), &marker);
        for errno in [libc::EMFILE, libc::ENOSYS] {
            let refused = reclaim_with(&marker, Duration::from_millis(200), |_| {
                Err(std::io::Error::from_raw_os_error(errno))
            });
            let error = refused.expect_err("an unpinnable reaper was taken for gone");
            assert!(
                error.to_string().contains(&reaper.id().to_string()),
                "{error}"
            );
            assert!(running(reaper.id()), "an unpinned reaper was signalled");
        }
        // Once it can be pinned, it is ended.
        assert_eq!(reclaim(&marker, Duration::from_secs(5)).unwrap(), 1);
        end_group(reaper.id());
        reaper.wait().unwrap();
    }

    #[test]
    fn a_reaper_that_exits_after_its_pin_is_never_signalled() {
        let directory = tempfile::tempdir().unwrap();
        let marker = format!("REAPER_TEST=reused-{}", std::process::id());
        let reaper = std::sync::Mutex::new(fake_reaper(directory.path(), &marker));
        // The reaper exits between the first look and the identity reads made while
        // pinned, as one whose PID is about to be reused does. Its pin then shows it
        // exited, so whatever takes the PID is never sent anything.
        let ended = reclaim_with(&marker, Duration::from_secs(1), |pid| {
            let fd = crate::process_tree::pidfd_open(pid)?;
            let mut reaper = reaper.lock().unwrap();
            end_group(reaper.id());
            reaper.wait().unwrap();
            Ok(fd)
        })
        .unwrap();
        assert_eq!(ended, 0);
    }
}
