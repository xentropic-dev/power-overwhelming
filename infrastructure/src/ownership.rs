//! What owns one launched worker process and everything it starts: its cgroup
//! where the system supports that, or process discovery as a logged fallback.
//! Ending, pausing and resuming a worker go through here.

use crate::{
    cgroup::Scope,
    process_tree::{Pinned, ProcessTree, TreeSignal},
};
use std::{
    sync::{Condvar, Mutex, MutexGuard},
    time::Duration,
};

pub(crate) enum Owner {
    /// The launch's own cgroup. `root` is the launched process, pinned because it
    /// runs as `systemd-run` outside the scope until the scope exists.
    Scope { root: Pinned, scope: Scope },
    /// Ancestry, session and owner-marker discovery.
    Tree(ProcessTree),
}

impl Owner {
    /// Owns the caller's unreaped child `pid`, which was started in `scope` when
    /// there is one. `marker` is the fallback's owner environment entry.
    pub(crate) fn new(pid: u32, scope: Option<Scope>, marker: Option<String>) -> Self {
        match scope {
            Some(scope) => Self::Scope {
                root: Pinned::child(pid),
                scope,
            },
            None => Self::Tree(ProcessTree::pin(pid, marker)),
        }
    }

    /// Whether the launched process has exited. The caller has not reaped it yet.
    pub(crate) fn root_exited(&self) -> bool {
        match self {
            Self::Scope { root, .. } => root.exited(),
            Self::Tree(tree) => tree.root_exited(),
        }
    }

    /// Asks everything to exit, waits up to `grace`, then kills what is left. With a
    /// cgroup, `Ok` means the cgroup is empty. The error names what could not be
    /// ended.
    pub(crate) fn terminate(&mut self, grace: Duration) -> Result<(), String> {
        match self {
            Self::Scope { root, scope } => scope.terminate(root, grace),
            Self::Tree(tree) => tree.terminate(grace),
        }
    }

    /// Pauses every process. A cgroup freezes them, which they cannot notice; the
    /// fallback sends SIGSTOP.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "pausing a worker comes with #79")
    )]
    pub(crate) fn pause(&mut self) -> std::io::Result<()> {
        match self {
            Self::Scope { scope, .. } => scope.freeze(),
            Self::Tree(tree) => {
                tree.signal(TreeSignal::Stop);
                Ok(())
            }
        }
    }

    /// Resumes a paused worker.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "resuming a worker comes with #79")
    )]
    pub(crate) fn resume(&mut self) -> std::io::Result<()> {
        match self {
            Self::Scope { scope, .. } => scope.thaw(),
            Self::Tree(tree) => {
                tree.signal(TreeSignal::Continue);
                Ok(())
            }
        }
    }
}

/// A teardown request, shared by whoever ends a session and the thread that owns
/// its processes. Its lock also serializes a teardown with a relaunch, so an end
/// either comes first and prevents the relaunch, or comes after it and ends the
/// relaunched process.
#[derive(Default)]
pub(crate) struct Stop {
    requested: Mutex<bool>,
    changed: Condvar,
}

impl Stop {
    pub(crate) fn request(&self) {
        *self.requested.lock().unwrap() = true;
        self.changed.notify_all();
    }

    pub(crate) fn requested(&self) -> bool {
        *self.requested.lock().unwrap()
    }

    /// Waits up to `timeout` for a request and says whether there is one.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let requested = self.requested.lock().unwrap();
        *self
            .changed
            .wait_timeout_while(requested, timeout, |requested| !*requested)
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
        let mut owner = Owner::new(child.id(), Some(scope), None);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !check.populated() {
            assert!(Instant::now() < deadline, "the scope never started");
            thread::sleep(Duration::from_millis(20));
        }
        let settle = |frozen: bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while check.frozen() != frozen {
                assert!(Instant::now() < deadline, "the scope never settled");
                thread::sleep(Duration::from_millis(20));
            }
        };
        owner.pause().unwrap();
        settle(true);
        assert!(!owner.root_exited());
        owner.resume().unwrap();
        settle(false);
        owner.pause().unwrap();
        settle(true);
        owner.terminate(Duration::from_millis(300)).unwrap();
        assert!(!check.populated());
        assert!(owner.root_exited());
        child.wait().unwrap();
    }
}
