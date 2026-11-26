//! Task and model labels across a daemon restart, through the PTY runtime.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

struct Daemon(Child);

impl Daemon {
    fn start(root: &Path) -> Self {
        let socket = root.join("power.sock");
        let path = format!(
            "{}:{}",
            root.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let daemon = Self(
            Command::new(env!("CARGO_BIN_EXE_power"))
                .arg("daemon")
                .env("POWER_SOCKET", &socket)
                .env("POWER_STATE_DIR", root.join("state"))
                .env("HOME", root)
                .env("SHELL", "/bin/bash")
                .env("PATH", path)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "Daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    /// Stops the daemon the way a reboot does and waits for its shutdown save.
    fn terminate(mut self) {
        Command::new("kill")
            .arg(self.0.id().to_string())
            .status()
            .unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(socket: &Path, message: Value) -> BufReader<UnixStream> {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writeln!(stream, "{message}").unwrap();
    BufReader::new(stream)
}

fn event(stream: &mut impl BufRead) -> Value {
    let mut data = String::new();
    assert_ne!(stream.read_line(&mut data).unwrap(), 0);
    let event: Value = serde_json::from_str(&data).unwrap();
    assert_ne!(event["event"], "error", "{event}");
    event
}

fn metadata_until(stream: &mut impl BufRead, wanted: Value) {
    let mut last = Value::Null;
    for _ in 0..40 {
        let message = event(stream);
        if message["event"] == "metadata" {
            last = message["metadata"].clone();
            if last == wanted {
                return;
            }
        }
    }
    panic!("Metadata never became {wanted}. Last {last}");
}

fn setup(root: &Path) {
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_power"), bin.join("power")).unwrap();
    // Reports labels once. A resumed run does not repeat them, so any label it shows
    // came from the saved record.
    let agent = bin.join("claude");
    std::fs::write(
        &agent,
        format!(
            "#!/bin/sh\nif [ ! -f {marker} ]; then\n  : > {marker}\n  power session task 'Saved task'\n  power session model reported-model\nfi\n# With its input redirected, the agent stays until it is killed.\nread finish || sleep 30\n",
            marker = root.join("reported").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Never reports anything. A resume would leave a marker and hold the foreground.
    let quiet = bin.join("grok");
    std::fs::write(
        &quiet,
        format!(
            "#!/bin/sh\n[ $# -eq 0 ] || : > {marker}\nsleep 30\n",
            marker = root.join("resumed").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&quiet, std::fs::Permissions::from_mode(0o755)).unwrap();
    let setup = Command::new(env!("CARGO_BIN_EXE_power"))
        .arg("setup")
        .env("HOME", root)
        .env("SHELL", "/bin/bash")
        .output()
        .unwrap();
    assert!(setup.status.success());
}

fn start_shell(socket: &Path) -> (Value, BufReader<UnixStream>) {
    let launched = event(&mut request(
        socket,
        json!({"request":"launch", "worktree":socket.parent().unwrap(), "agent":"shell"}),
    ));
    let session = launched["session"]["id"].clone();
    let mut attached = request(socket, json!({"request":"subscribe", "session":session}));
    assert_eq!(event(&mut attached)["event"], "snapshot");
    (session, attached)
}

fn input(socket: &Path, session: &Value, text: &str) {
    event(&mut request(
        socket,
        json!({"request":"input", "session":session, "data":STANDARD.encode(text)}),
    ));
}

fn start_agent(socket: &Path) -> (Value, BufReader<UnixStream>) {
    start_agent_with(socket, "claude\n")
}

fn start_agent_with(socket: &Path, command: &str) -> (Value, BufReader<UnixStream>) {
    let (session, mut attached) = start_shell(socket);
    input(socket, &session, command);
    metadata_until(
        &mut attached,
        json!({"task":"Saved task", "model":"reported-model"}),
    );
    (session, attached)
}

fn sandbox() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    setup(&root);
    let socket = root.join("power.sock");
    (directory, socket)
}

#[test]
fn labels_survive_a_restart_through_the_bash_resume_hook() {
    let (directory, socket) = sandbox();
    let root = directory.path();
    let daemon = Daemon::start(root);
    let (session, _attached) = start_agent(&socket);
    daemon.terminate();

    let _daemon = Daemon::start(root);
    // Resumed from Bash's prompt hook, the agent runs in the shell's own process group.
    let mut attached = request(&socket, json!({"request":"subscribe", "session":session}));
    metadata_until(
        &mut attached,
        json!({"task":"Saved task", "model":"reported-model"}),
    );
}

#[test]
fn labels_stay_saved_through_the_shell_restore_grace_period() {
    let (directory, socket) = sandbox();
    let root = directory.path();
    let daemon = Daemon::start(root);
    let (session, mut attached) = start_agent(&socket);
    // The agent exits, so the live labels clear at once.
    event(&mut request(
        &socket,
        json!({"request":"input", "session":session, "data":STANDARD.encode("\n")}),
    ));
    metadata_until(&mut attached, json!({"task":null, "model":null}));
    // A reboot arrives while the saved agent and conversation still stand.
    daemon.terminate();

    let _daemon = Daemon::start(root);
    let mut attached = request(&socket, json!({"request":"subscribe", "session":session}));
    metadata_until(
        &mut attached,
        json!({"task":"Saved task", "model":"reported-model"}),
    );
}

#[test]
fn a_foreground_agent_with_redirected_input_keeps_its_labels() {
    let (directory, socket) = sandbox();
    let root = directory.path();
    let daemon = Daemon::start(root);
    // Started without the wrapper and with its input redirected, the agent still runs in
    // the terminal's foreground job and owns the labels it reports.
    let (session, _attached) = start_agent_with(&socket, "command claude </dev/null\n");
    daemon.terminate();

    let _daemon = Daemon::start(root);
    let mut attached = request(&socket, json!({"request":"subscribe", "session":session}));
    metadata_until(
        &mut attached,
        json!({"task":"Saved task", "model":"reported-model"}),
    );
}

#[test]
fn a_background_agent_that_never_reported_is_not_resumed() {
    let (directory, socket) = sandbox();
    let root = directory.path();
    let daemon = Daemon::start(root);
    let (session, mut attached) = start_shell(&socket);
    // Without job control the job shares the shell's group, so it is seen as the
    // foreground agent. It never reports, so it is not what a restore resumes.
    input(&socket, &session, "set +m; command grok </dev/null &\n");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "The job was never seen");
        let message = event(&mut attached);
        if message["event"] == "agent" && message["agent"] == "grok" {
            break;
        }
    }
    daemon.terminate();

    let _daemon = Daemon::start(root);
    // The restored shell reads commands, so no resumed agent holds the foreground.
    let prompted = root.join("prompted");
    input(&socket, &session, &format!(": > {}\n", prompted.display()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !prompted.exists() {
        assert!(
            Instant::now() < deadline,
            "The restored shell never ran a command"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!root.join("resumed").exists());
}
