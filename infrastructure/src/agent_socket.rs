//! Private server sockets for agents whose profile sets `server_socket_env`.
//!
//! Each launch of a session gets its own socket, so a restored agent never reattaches to
//! a server its previous launch left behind. Before Power launches a session again, it
//! stops any server still listening on one of that session's earlier sockets.

use std::{
    ffi::OsStr,
    fs,
    os::unix::{ffi::OsStrExt, fs::DirBuilderExt, fs::MetadataExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// The longest socket pathname `sun_path` holds, leaving room for the NUL.
const MAX_SOCKET_PATH: usize = 107;
const PREFIX: &str = "ps-";

/// A new socket path for one launch of `session`, beside Power's socket when the path
/// fits there and in a short private directory otherwise.
pub(crate) fn launch_socket(session: &str, power_socket: &Path) -> PathBuf {
    let launch = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("{}{}.sock", session_prefix(session), &launch[..8]);
    for directory in directories(power_socket) {
        let path = directory.join(&name);
        if path.as_os_str().len() <= MAX_SOCKET_PATH && prepare(&directory) {
            return path;
        }
    }
    // Neither directory works. The agent reports its own error for this path.
    std::env::temp_dir().join(name)
}

/// Stops every server still listening on a socket from an earlier launch of `session`
/// and removes those sockets. Call it only when none of the session's processes from
/// this daemon run, so every such listener is an orphan.
pub(crate) fn reclaim(session: &str, power_socket: &Path) {
    let prefix = session_prefix(session);
    for directory in directories(power_socket) {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && name.ends_with(".sock") {
                stop_listeners(&entry.path());
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Stops the processes holding a socket bound at `path`. A server that started its own
/// process group, as jcode's does, is stopped with its whole group.
pub(crate) fn stop_listeners(path: &Path) {
    let pids = socket_holders(path);
    if pids.is_empty() {
        return;
    }
    let own_group = unsafe { libc::getpgid(0) };
    let targets: Vec<libc::pid_t> = pids
        .iter()
        .map(|&pid| {
            let group = unsafe { libc::getpgid(pid) };
            if group == pid && group != own_group {
                -pid
            } else {
                pid
            }
        })
        .collect();
    for &target in &targets {
        unsafe { libc::kill(target, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && pids.iter().any(|&pid| alive(pid)) {
        thread::sleep(Duration::from_millis(20));
    }
    for (&pid, &target) in pids.iter().zip(&targets) {
        if alive(pid) {
            unsafe { libc::kill(target, libc::SIGKILL) };
        }
    }
}

fn alive(pid: libc::pid_t) -> bool {
    // A zombie no longer listens.
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

/// PIDs of processes with a descriptor for a Unix socket bound at `path`.
fn socket_holders(path: &Path) -> Vec<libc::pid_t> {
    let Ok(table) = fs::read("/proc/net/unix") else {
        return Vec::new();
    };
    let path = path.as_os_str().as_bytes();
    let inodes: Vec<String> = table
        .split(|&byte| byte == b'\n')
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&[u8]> = line
                .split(|&byte| byte == b' ')
                .filter(|field| !field.is_empty())
                .collect();
            (fields.len() >= 8 && fields[7..].join(&b' ') == path)
                .then(|| format!("socket:[{}]", String::from_utf8_lossy(fields[6])))
        })
        .collect();
    if inodes.is_empty() {
        return Vec::new();
    }
    let own = std::process::id() as libc::pid_t;
    let Ok(processes) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    processes
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .filter(|&pid| pid != own)
        .filter(|pid| {
            fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|fds| {
                fds.flatten().any(|fd| {
                    fs::read_link(fd.path()).is_ok_and(|target| {
                        inodes
                            .iter()
                            .any(|inode| target.as_os_str() == OsStr::new(inode))
                    })
                })
            })
        })
        .collect()
}

/// The directory of Power's socket, then a short private directory for when a socket
/// path there would be too long.
fn directories(power_socket: &Path) -> [PathBuf; 2] {
    // A relative socket from `POWER_SOCKET` would leave the directory empty.
    let socket = std::path::absolute(power_socket).unwrap_or_else(|_| power_socket.to_owned());
    let runtime = socket.parent().unwrap_or(Path::new("/")).to_owned();
    let uid = unsafe { libc::getuid() };
    [runtime, PathBuf::from(format!("/tmp/power-{uid}"))]
}

/// Whether `directory` exists, or was created, as a directory this user owns.
fn prepare(directory: &Path) -> bool {
    let _ = fs::DirBuilder::new().mode(0o700).create(directory);
    fs::symlink_metadata(directory)
        .is_ok_and(|metadata| metadata.is_dir() && metadata.uid() == unsafe { libc::getuid() })
}

/// The name prefix shared by every socket of `session`: a stable FNV-1a hash, so the
/// name stays short whatever the session ID.
fn session_prefix(session: &str) -> String {
    let hash = session
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("{PREFIX}{:012x}-", hash >> 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    #[test]
    fn a_socket_under_a_long_power_directory_still_binds() {
        let root = tempfile::tempdir().unwrap();
        // A 99-byte Power socket path, valid itself, leaves no room for a long name.
        let base = root.path().as_os_str().len();
        let directory = root
            .path()
            .join("d".repeat(99 - base - "/power.sock".len() - 1));
        fs::create_dir_all(&directory).unwrap();
        let power = directory.join("power.sock");
        assert_eq!(power.as_os_str().len(), 99, "{power:?}");
        UnixListener::bind(&power).unwrap();

        let session = "0123456789abcdef-0123-4567-89ab-cdef01234567-long-session-id";
        let socket = launch_socket(session, &power);
        assert!(socket.as_os_str().len() <= MAX_SOCKET_PATH, "{socket:?}");
        let _listener = UnixListener::bind(&socket).unwrap();
        // Each launch gets its own socket.
        assert_ne!(launch_socket(session, &power), socket);
        reclaim(session, &power);
        assert!(!socket.exists());

        // A short directory keeps the socket beside Power's.
        let short = root.path().join("power.sock");
        assert_eq!(launch_socket(session, &short).parent(), Some(root.path()));
    }

    #[test]
    fn reclaim_stops_an_orphaned_server_and_spares_other_sessions() {
        let root = tempfile::tempdir().unwrap();
        let power = root.path().join("power.sock");
        let serve = |socket: &Path| {
            // A detached server in its own session, like `jcode serve`.
            let mut child = Command::new("setsid")
                .args(["python3", "-c"])
                .arg(
                    "import socket,sys,time\n\
                     s=socket.socket(socket.AF_UNIX)\n\
                     s.bind(sys.argv[1]); s.listen()\n\
                     open(sys.argv[1]+'.ready','w').close()\n\
                     time.sleep(60)",
                )
                .arg(socket)
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let ready = PathBuf::from(format!("{}.ready", socket.display()));
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "server never listened");
                assert!(child.try_wait().unwrap().is_none(), "server exited");
                thread::sleep(Duration::from_millis(20));
            }
            child
        };
        let old = launch_socket("restored", &power);
        let other = launch_socket("another", &power);
        let mut orphan = serve(&old);
        let mut neighbour = serve(&other);

        reclaim("restored", &power);
        let deadline = Instant::now() + Duration::from_secs(5);
        while orphan.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the orphaned server survived");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!old.exists());
        assert!(neighbour.try_wait().unwrap().is_none());
        assert!(other.exists());
        stop_listeners(&other);
        neighbour.wait().unwrap();
    }
}
