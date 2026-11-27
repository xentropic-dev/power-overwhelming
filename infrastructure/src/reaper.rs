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

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
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

/// Makes `command` run under the reaper. `executable` is the power binary.
pub(crate) fn wrap(command: &mut portable_pty::CommandBuilder, executable: &Path) {
    let argv = command.get_argv_mut();
    argv.splice(
        0..0,
        [
            executable.as_os_str().to_owned(),
            SUBCOMMAND.into(),
            "--".into(),
        ],
    );
}

/// Reapers an earlier daemon left for `session`, found by their owner marker.
fn reapers(marker: &[u8]) -> BTreeMap<u32, u64> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
                cmdline.split(|byte| *byte == 0).nth(1) == Some(SUBCOMMAND.as_bytes())
            }) && fs::read(format!("/proc/{pid}/environ"))
                .is_ok_and(|environ| environ.split(|byte| *byte == 0).any(|item| item == marker))
        })
        .filter_map(|pid| Some((pid, start_time(pid)?)))
        .collect()
}

fn start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Ends what reapers an earlier daemon left for the session marked `marker` still
/// run, waiting up to `timeout`. Each reaper ends its own tree. Returns how many
/// reapers were ended, or fails naming those that outlived the timeout.
pub(crate) fn reclaim(marker: &str, timeout: Duration) -> Result<usize> {
    let found = reapers(marker.as_bytes());
    let pinned: Vec<_> = found
        .iter()
        .filter_map(|(pid, start)| crate::process_tree::Pinned::pin_start(*pid, *start))
        .collect();
    for reaper in &pinned {
        reaper.signal(libc::SIGTERM);
    }
    let deadline = Instant::now() + timeout;
    while pinned.iter().any(|reaper| !reaper.exited()) {
        if Instant::now() >= deadline {
            let stuck: Vec<_> = pinned
                .iter()
                .filter(|reaper| !reaper.exited())
                .map(|reaper| reaper.pid())
                .collect();
            bail!("reapers {stuck:?} still run");
        }
        thread::sleep(POLL);
    }
    Ok(pinned.len())
}
