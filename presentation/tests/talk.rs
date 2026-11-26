use infrastructure::{
    boxes::{Layout, SavedBox},
    client::protocol::{ManagedAgent, Run},
};
use serde_json::Value;
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const BOX: &str = "talk-box";
const REMOTE_BOX: &str = "talk-remote";

fn poll<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "Timed out: {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[track_caller]
fn success(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn json(output: Output) -> Value {
    serde_json::from_str(&success(output)).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    success(
        Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .args(args)
            .output()
            .unwrap(),
    );
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    _daemon: Option<Daemon>,
    root: tempfile::TempDir,
    coordinator: ManagedAgent,
    run: Run,
    a: ManagedAgent,
    b: ManagedAgent,
}

struct Sandbox {
    daemon: Option<Daemon>,
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("pTalk.")
            .tempdir_in("/tmp")
            .unwrap();
        let path = root.path();
        for name in ["state", "home", "config/power-overwhelming"] {
            fs::create_dir_all(path.join(name)).unwrap();
        }
        let script = path.join("worker.sh");
        fs::write(&script, include_str!("fixtures/talk-worker.sh")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("config/power-overwhelming/config.lua"), format!(
            "talk = {{ idle_after = 0.015, user_quiet = 0.025 }}\nagents.fixture = {{ command = {:?} }}\n",
            script.to_str().unwrap()
        )).unwrap();

        let mut boxes = Vec::new();
        for (id, names) in [
            (BOX, &["coordinator", "a", "b"][..]),
            (REMOTE_BOX, &["remote"][..]),
        ] {
            let checkout = path.join(id);
            fs::create_dir(&checkout).unwrap();
            git(&checkout, &["init", "--quiet"]);
            git(
                &checkout,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "initial",
                ],
            );
            for name in names {
                git(
                    &checkout,
                    &[
                        "worktree",
                        "add",
                        "--quiet",
                        "-b",
                        name,
                        path.join(name).to_str().unwrap(),
                    ],
                );
            }
            boxes.push(SavedBox {
                id: id.into(),
                repository: checkout.join(".git"),
                checkout,
                shortcut: None,
                views: Vec::new(),
                active_view: None,
                previous_view: None,
            });
        }
        let layout = Layout {
            boxes,
            active_box: Some(BOX.into()),
            onboarded: true,
            color_scheme: None,
        };
        fs::write(
            path.join("state/boxes.json"),
            serde_json::to_vec(&layout).unwrap(),
        )
        .unwrap();
        let daemon = Some(start(path));
        Self { root, daemon }
    }

    fn launch(&self, name: &str, box_id: &str) -> ManagedAgent {
        let path = self.root.path();
        let mut launch = power(path, None);
        launch
            .args([
                "agent",
                "start",
                "--box",
                box_id,
                "--provider",
                "fixture",
                "--worktree",
            ])
            .arg(path.join(name))
            .arg("--");
        let agent = json(launch.arg(name).output().unwrap())["agent"].take();
        poll("fixture worker startup", || {
            path.join(format!("ready-{name}")).exists().then_some(())
        });
        serde_json::from_value(agent).unwrap()
    }
}

impl Fixture {
    fn new() -> Self {
        let Sandbox { root, daemon } = Sandbox::new();
        let sandbox = Sandbox { root, daemon };
        let path = sandbox.root.path();
        let coordinator = sandbox.launch("coordinator", BOX);
        let run = json(
            power(path, Some(&coordinator))
                .args(["dag", "create", "--title", "Talk"])
                .output()
                .unwrap(),
        )["run"]
            .take();
        let run: Run = serde_json::from_value(run).unwrap();
        let dispatch = |name: &str| {
            json(
                power(path, None)
                    .args(["dag", "add-task", &run.id, "--id", name, "--title", name])
                    .output()
                    .unwrap(),
            );
            let output = power(path, Some(&coordinator))
                .args([
                    "dag",
                    "dispatch",
                    &run.id,
                    name,
                    "--provider",
                    "command",
                    "--worktree",
                ])
                .arg(path.join(name))
                .arg("--")
                .arg(path.join("worker.sh"))
                .output()
                .unwrap();
            let agent = serde_json::from_value(json(output)["agent"].take()).unwrap();
            poll("worker startup", || {
                path.join(format!("ready-{name}")).exists().then_some(())
            });
            agent
        };
        let a = dispatch("a");
        let b = dispatch("b");
        Self {
            root: sandbox.root,
            _daemon: sandbox.daemon,
            coordinator,
            run,
            a,
            b,
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn talk(&self, caller: Option<&ManagedAgent>, args: &[&str]) -> Output {
        talk(self.path(), caller, args)
    }

    fn messages(&self, caller: Option<&ManagedAgent>, args: &[&str]) -> Vec<Value> {
        messages(self.talk(caller, args))
    }
}

fn power(path: &Path, caller: Option<&ManagedAgent>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_power"));
    command
        .current_dir(path)
        .env("HOME", path.join("home"))
        .env("POWER_SOCKET", path.join("power.sock"))
        .env("POWER_STATE_DIR", path.join("state"))
        .env("XDG_RUNTIME_DIR", path)
        .env("XDG_CONFIG_HOME", path.join("config"))
        .env("SHELL", "/bin/sh")
        .env("TALK_TEST_ROOT", path)
        .env_remove("POWER_BIN")
        .env_remove("POWER_PLUGIN_DIR");
    for env in [
        "POWER_SESSION",
        "POWER_BOX",
        "POWER_AGENT",
        "POWER_RUN",
        "POWER_TASK",
        "POWER_ATTEMPT",
    ] {
        command.env_remove(env);
    }
    if let Some(agent) = caller {
        command.env("POWER_SESSION", agent.session.as_ref().unwrap());
    }
    command
}

fn start(path: &Path) -> Daemon {
    let mut daemon = Daemon(
        power(path, None)
            .arg("daemon")
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    poll("test daemon socket", || {
        assert!(daemon.0.try_wait().unwrap().is_none(), "Test daemon exited");
        UnixStream::connect(path.join("power.sock")).ok()
    });
    daemon
}

fn talk(path: &Path, caller: Option<&ManagedAgent>, args: &[&str]) -> Output {
    power(path, caller).arg("talk").args(args).output().unwrap()
}

fn messages(output: Output) -> Vec<Value> {
    serde_json::from_value(json(output)["messages"].take()).unwrap()
}

fn file(path: &Path, name: &str) -> String {
    fs::read_to_string(path.join(name)).unwrap_or_default()
}

fn notice_count(path: &Path, name: &str) -> usize {
    file(path, &format!("notice-{name}")).lines().count()
}

fn field<'a>(message: &'a Value, key: &str) -> &'a str {
    message[key].as_str().unwrap()
}

#[test]
fn queues_one_notice_and_reads_the_worker_envelope() {
    let fixture = Fixture::new();
    let sent = fixture.messages(
        Some(&fixture.a),
        &["send", "coordinator", "question", "--json"],
    );
    let id = field(&sent[0], "id");
    poll("coordinator notice", || {
        (notice_count(fixture.path(), "coordinator") > 0).then_some(())
    });
    let line = file(fixture.path(), "notice-coordinator");
    assert!(
        line.contains("1 new message")
            && line.contains("Not from the user.")
            && !line.contains("question")
    );
    let printed = success(fixture.talk(Some(&fixture.coordinator), &["inbox"]));
    for expected in [
        id,
        &fixture.a.id,
        "Command",
        "worker on task a",
        "Not from the user.",
        "question",
    ] {
        assert!(printed.contains(expected), "{printed}");
    }
    assert!(
        fixture
            .messages(Some(&fixture.coordinator), &["inbox", "--json"])
            .is_empty()
    );
    let history = fixture.messages(None, &["log", "--agent", &fixture.a.id, "--json"]);
    assert!(history[0]["delivery"]["notified_at"].is_u64());
    assert!(history[0]["delivery"]["read_at"].is_u64());
    thread::sleep(Duration::from_millis(125));
    assert_eq!(notice_count(fixture.path(), "coordinator"), 1);
}

#[test]
fn waits_for_reply_and_resumes_a_timed_out_thread() {
    let fixture = Fixture::new();
    for mark in ["read-b", "reply-b"] {
        fs::write(fixture.path().join(mark), "").unwrap();
    }
    let output = json(fixture.talk(
        Some(&fixture.coordinator),
        &[
            "send",
            "task:b",
            "do X",
            "--wait",
            "--timeout",
            "30",
            "--json",
        ],
    ));
    assert_eq!(output["reply"]["body"], "fixture reply");
    assert_eq!(output["reply"]["thread"], output["messages"][0]["thread"]);
    assert_eq!(output["reply"]["reply_to"], output["messages"][0]["id"]);
    assert_eq!(
        fixture
            .messages(
                Some(&fixture.coordinator),
                &["show", field(&output["reply"], "id"), "--json"]
            )
            .len(),
        2
    );

    let timed_out = fixture.talk(
        Some(&fixture.coordinator),
        &[
            "send",
            "task:b",
            "no immediate reply",
            "--wait",
            "--timeout",
            "0",
            "--json",
        ],
    );
    assert_eq!(timed_out.status.code(), Some(3), "{timed_out:?}");
    let timed_out: Value = serde_json::from_slice(&timed_out.stdout).unwrap();
    let thread = timed_out["threads"][0].as_str().unwrap();
    success(fixture.talk(Some(&fixture.b), &["reply", thread, "later"]));
    let reply = json(fixture.talk(
        Some(&fixture.coordinator),
        &["wait", thread, "--timeout", "2", "--json"],
    ));
    assert_eq!(reply["reply"]["body"], "later");
}

#[test]
fn broadcasts_to_active_workers_with_one_broadcast_id() {
    let fixture = Fixture::new();
    let sent = fixture.messages(
        Some(&fixture.coordinator),
        &["send", "--run", &fixture.run.id, "status?", "--json"],
    );
    assert_eq!(sent.len(), 2);
    assert!(sent[0]["broadcast"].is_string());
    assert_eq!(sent[0]["broadcast"], sent[1]["broadcast"]);
    assert_ne!(sent[0]["thread"], sent[1]["thread"]);
    for (name, worker) in [("a", &fixture.a), ("b", &fixture.b)] {
        poll("broadcast notice", || {
            (notice_count(fixture.path(), name) > 0).then_some(())
        });
        let inbox = fixture.messages(Some(worker), &["inbox", "--json"]);
        assert_eq!(inbox[0]["broadcast"], sent[0]["broadcast"]);
        assert_eq!(inbox[0]["sender"]["id"], fixture.coordinator.id);
    }
    assert_eq!(notice_count(fixture.path(), "coordinator"), 0);
}

#[test]
fn holds_busy_and_needs_input_peer_notices_and_delivers_user_messages() {
    let fixture = Fixture::new();
    success(fixture.talk(Some(&fixture.b), &["busy"]));
    success(fixture.talk(Some(&fixture.a), &["send", "task:b", "queued while busy"]));
    thread::sleep(Duration::from_millis(150));
    assert_eq!(notice_count(fixture.path(), "b"), 0);
    success(fixture.talk(Some(&fixture.b), &["ready"]));
    poll("first worker notice", || {
        (notice_count(fixture.path(), "b") == 1).then_some(())
    });
    success(fixture.talk(Some(&fixture.b), &["inbox"]));
    success(fixture.talk(Some(&fixture.b), &["ready"]));
    let report = |status| {
        let mut command = power(fixture.path(), Some(&fixture.b));
        command
            .env("POWER_RUN", &fixture.run.id)
            .env("POWER_TASK", "b")
            .env("POWER_ATTEMPT", fixture.b.attempt.as_ref().unwrap());
        success(
            command
                .args(["dag", "report", status, "--summary", "test"])
                .output()
                .unwrap(),
        );
    };
    report("needs-input");
    success(fixture.talk(Some(&fixture.a), &["send", "task:b", "queued for working"]));
    thread::sleep(Duration::from_millis(150));
    assert_eq!(notice_count(fixture.path(), "b"), 1);
    success(fixture.talk(None, &["send", "--box", BOX, &fixture.b.id, "user answer"]));
    poll("user message notice", || {
        (notice_count(fixture.path(), "b") == 2).then_some(())
    });
    assert!(
        file(fixture.path(), "notice-b")
            .lines()
            .last()
            .unwrap()
            .contains("from the user")
    );
    let inbox = fixture.messages(Some(&fixture.b), &["inbox", "--peek", "--json"]);
    assert!(inbox[0]["delivery"]["notified_at"].is_null());
    assert!(inbox[1]["delivery"]["notified_at"].is_u64());
    success(fixture.talk(Some(&fixture.b), &["ready"]));
    report("working");
    poll("notice after working", || {
        (notice_count(fixture.path(), "b") == 3).then_some(())
    });
    let inbox = fixture.messages(Some(&fixture.b), &["inbox", "--json"]);
    assert!(inbox[0]["delivery"]["notified_at"].is_u64());
}

#[test]
fn rejects_bad_targets_and_rate_limits_without_queueing() {
    let fixture = Fixture::new();
    // Generated agent IDs are random. Seed two known prefixes in the isolated test database.
    let db = rusqlite::Connection::open(fixture.path().join("state/dag.sqlite3")).unwrap();
    for (id, box_id) in [
        ("fixture-a", BOX),
        ("fixture-b", BOX),
        ("elsewhere", REMOTE_BOX),
    ] {
        db.execute(
            "INSERT INTO agents(id,box_id,worktree,provider,state,label,created_at)
                    SELECT ?1,?2,worktree,provider,'exited',label,created_at
                    FROM agents WHERE id=?3",
            (id, box_id, &fixture.a.id),
        )
        .unwrap();
    }
    for (to, why) in [
        ("task:missing", "Unknown"),
        ("fixture-", "ambiguous"),
        ("fixture-a", "exited"),
        ("elsewhere", "another box"),
    ] {
        let result = fixture.talk(Some(&fixture.a), &["send", to, "rejected"]);
        assert_eq!(result.status.code(), Some(1), "{result:?}");
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains(to) && error.contains(why), "{error}");
    }
    assert!(
        fixture
            .messages(None, &["log", "--box", BOX, "--json"])
            .is_empty()
    );
    for _ in 0..20 {
        success(fixture.talk(Some(&fixture.a), &["send", "task:b", "test rate"]));
    }
    let result = fixture.talk(Some(&fixture.a), &["send", "task:b", "21st"]);
    assert_eq!(result.status.code(), Some(1), "{result:?}");
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("rate limit") && error.contains("Retry in"),
        "{error}"
    );
    assert_eq!(
        fixture
            .messages(None, &["log", "--agent", &fixture.b.id, "--json"])
            .len(),
        20
    );
}

#[test]
fn restart_preserves_unread_messages_and_delivers_one_reminder() {
    let mut sandbox = Sandbox::new();
    // Command sessions are intentionally never replayed after restart. The same deterministic
    // fixture script runs as a configured profile for this restorable recipient.
    let worker = sandbox.launch("b", BOX);
    let path = sandbox.root.path();
    let sent = messages(talk(
        path,
        None,
        &["send", "--box", BOX, &worker.id, "survive", "--json"],
    ));
    let id = field(&sent[0], "id");
    poll("first notice", || {
        (notice_count(path, "b") == 1).then_some(())
    });
    poll("durable notice state", || {
        let rows = messages(talk(path, Some(&worker), &["inbox", "--peek", "--json"]));
        rows[0]["delivery"]["notified_at"].is_u64().then_some(())
    });

    drop(sandbox.daemon.take());
    let db = rusqlite::Connection::open(path.join("state/dag.sqlite3")).unwrap();
    // Simulate the two-minute reminder interval elapsing while the daemon is down.
    db.execute(
        "UPDATE talk_messages SET notified_at = notified_at - 120000 WHERE id = ?1",
        [id],
    )
    .unwrap();
    drop(db);
    sandbox.daemon = Some(start(path));
    poll("restored worker reminder", || {
        (notice_count(path, "b") == 2).then_some(())
    });
    let inbox = messages(talk(path, Some(&worker), &["inbox", "--peek", "--json"]));
    assert_eq!(inbox[0]["id"], id);
    assert!(inbox[0]["delivery"]["reminded_at"].is_u64());
    assert!(inbox[0]["delivery"]["read_at"].is_null());
    thread::sleep(Duration::from_millis(125));
    assert_eq!(notice_count(path, "b"), 2);
    let inbox = messages(talk(path, Some(&worker), &["inbox", "--json"]));
    assert_eq!(inbox[0]["id"], id);
    assert!(inbox[0]["delivery"]["read_at"].is_u64());
}
