mod common;

use common::write_config;
use infrastructure::{
    boxes::{Layout, SavedBox},
    client::protocol::{
        Attempt, AttemptState, BoxAttention, BoxInfo, Coordinator, ManagedAgent, Provider, Run,
        RunEvent, RunEventKind, Task, TaskState,
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::Barrier,
    thread,
    time::{Duration, Instant},
};

const BOX_ID: &str = "d8203461-69db-42a3-9c36-906c0e0cdc32";
const COORDINATOR: &str = "herdr:dag-test";
const TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

fn power(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_power"));
    command
        .current_dir(root)
        .env("POWER_SOCKET", root.join("power.sock"))
        .env("POWER_STATE_DIR", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("SHELL", "/bin/sh")
        .env("POWER_WORKER_SLICE", test_slice(root))
        .env_remove("POWER_BIN")
        .env_remove("POWER_PLUGIN_DIR")
        .env_remove("POWER_SESSION")
        .env_remove("POWER_BOX")
        .env_remove("POWER_AGENT")
        .env_remove("POWER_RUN")
        .env_remove("POWER_TASK")
        .env_remove("POWER_ATTEMPT");
    command
}

/// The systemd slice that holds this fixture's worker scopes, apart from the live
/// daemon's and every other test's.
fn test_slice(root: &Path) -> String {
    let name: String = root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    format!("app-powertest{name}.slice")
}

/// The cgroup directory of the fixture's slice.
fn slice_dir(root: &Path) -> PathBuf {
    let manager = Command::new("systemctl")
        .args(["--user", "show", "--property=ControlGroup", "--value"])
        .output()
        .unwrap();
    assert!(
        manager.status.success(),
        "these tests need a systemd user manager"
    );
    let manager = String::from_utf8(manager.stdout).unwrap();
    Path::new("/sys/fs/cgroup")
        .join(manager.trim().trim_start_matches('/'))
        .join("app.slice")
        .join(test_slice(root))
}

/// The session's scopes that still hold processes.
fn populated_scopes(root: &Path, session: &str) -> Vec<String> {
    let prefix = format!("power-{session}-");
    let Ok(entries) = fs::read_dir(slice_dir(root)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .filter(|entry| {
            fs::read_to_string(entry.path().join("cgroup.events"))
                .is_ok_and(|events| events.lines().any(|line| line == "populated 1"))
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

struct Daemon(Child);

impl Daemon {
    fn start_with(root: &Path, env: &[(&str, &str)]) -> Self {
        let mut daemon = Self(
            power(root)
                .arg("daemon")
                .envs(env.iter().copied())
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let socket = root.join("power.sock");
        poll("daemon socket", || {
            assert_eq!(
                daemon.0.try_wait().unwrap(),
                None,
                "Daemon exited on startup"
            );
            UnixStream::connect(&socket).ok()
        });
        daemon
    }

    fn stop(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }

    /// Stops the daemon the way a reboot or `systemctl stop` does.
    fn shut_down(&mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            child_finished(&mut self.0, TIMEOUT),
            "the daemon did not shut down"
        );
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn poll<T>(description: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(result) = probe() {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {description}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn child_finished(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[track_caller]
fn output(command: &mut Command) -> Output {
    command
        .output()
        .unwrap_or_else(|error| panic!("Could not run {command:?}: {error}"))
}

#[track_caller]
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "Command returned {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[track_caller]
fn assert_rejected(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "Expected a rejected operation\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!output.stderr.is_empty(), "{output:?}");
}

#[track_caller]
fn json_output(output: Output) -> Value {
    assert_success(&output);
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("Invalid JSON stdout: {error}\n{output:?}"))
}

fn git(directory: &Path, args: &[&str]) {
    assert_success(&output(
        Command::new("git")
            .current_dir(directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .args(args),
    ));
}

fn init_repo(directory: &Path) {
    fs::create_dir(directory).unwrap();
    git(directory, &["init", "--quiet"]);
    git(
        directory,
        &[
            "-c",
            "user.name=Power DAG Test",
            "-c",
            "user.email=power-dag@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Initial commit",
        ],
    );
}

struct Fixture {
    daemon: Daemon,
    /// Extra environment every daemon of this fixture starts with.
    env: Vec<(String, String)>,
    directory: tempfile::TempDir,
    checkout: PathBuf,
    worktrees: [PathBuf; 2],
}

impl Fixture {
    fn new() -> Self {
        Self::with_configure(|_| {})
    }

    fn with_configure(configure: impl FnOnce(&Path)) -> Self {
        Self::with_configure_env(configure, &[])
    }

    fn with_configure_env(configure: impl FnOnce(&Path), env: &[(&str, &str)]) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("pX.")
            .rand_bytes(4)
            .tempdir_in("/tmp")
            .unwrap();
        let root = directory.path();
        fs::create_dir(root.join("runtime")).unwrap();
        let checkout = root.join("box-repo");
        init_repo(&checkout);
        let checkout = checkout.canonicalize().unwrap();
        let worktrees = ["worker-one", "worker-two"].map(|name| {
            let path = root.join(name);
            git(
                &checkout,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    name,
                    path.to_str().unwrap(),
                ],
            );
            path.canonicalize().unwrap()
        });
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        let layout = Layout {
            boxes: vec![SavedBox {
                id: BOX_ID.into(),
                repository: checkout.join(".git"),
                checkout: checkout.clone(),
                shortcut: Some(7),
                views: Vec::new(),
                active_view: None,
                previous_view: None,
            }],
            active_box: Some(BOX_ID.into()),
            onboarded: true,
            color_scheme: None,
        };
        fs::write(
            state.join("boxes.json"),
            serde_json::to_vec(&layout).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("worker.sh"),
            include_str!("fixtures/dag-worker.sh"),
        )
        .unwrap();
        // Keep the host's free memory out of the tests. A fixture that needs its own config
        // writes it with `write_config` too.
        write_config(root, "");
        configure(root);
        let daemon = Daemon::start_with(root, env);
        Self {
            daemon,
            env: env
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
            directory,
            checkout,
            worktrees,
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn restart(&mut self) {
        self.restart_with(&[]);
    }

    fn restart_with(&mut self, env: &[(&str, &str)]) {
        self.daemon.stop();
        let mut all = self.env();
        all.extend_from_slice(env);
        self.daemon = Daemon::start_with(self.root(), &all);
    }

    fn restart_gracefully(&mut self) {
        self.daemon.shut_down();
        self.daemon = Daemon::start_with(self.root(), &self.env());
    }

    fn env(&self) -> Vec<(&str, &str)> {
        self.env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect()
    }

    fn command(&self, args: &[&str]) -> Output {
        output(power(self.root()).args(args))
    }

    #[track_caller]
    fn json(&self, args: &[&str]) -> Value {
        json_output(self.command(args))
    }

    fn create_run_json(&self, title: &str) -> Value {
        self.json(&[
            "dag",
            "create",
            "--title",
            title,
            "--box",
            "7",
            "--coordinator",
            COORDINATOR,
        ])
    }

    fn create_run(&self) -> Run {
        serde_json::from_value(self.create_run_json("Fixture run")["run"].take()).unwrap()
    }

    fn add_task(&self, run: &str, id: &str) {
        self.json(&["dag", "add-task", run, "--id", id, "--title", id]);
    }

    fn apply(&self, run: &str, tasks: Value) -> Output {
        let file = self.root().join("tasks.json");
        fs::write(&file, serde_json::to_vec(&tasks).unwrap()).unwrap();
        output(
            power(self.root())
                .args(["dag", "apply", run, "--file"])
                .arg(file),
        )
    }

    fn show(&self, run: &str) -> Run {
        serde_json::from_value(self.json(&["dag", "show", run])["run"].take()).unwrap()
    }

    fn agents(&self) -> Vec<ManagedAgent> {
        serde_json::from_value(
            self.json(&["agent", "list", "--box", BOX_ID, "--all"])["agents"].take(),
        )
        .unwrap()
    }

    fn dispatch(
        &self,
        run: &str,
        task: &str,
        worktree: &Path,
        retry: bool,
        worker: &[&str],
    ) -> Output {
        let mut command = power(self.root());
        command
            .args([
                "dag",
                "dispatch",
                run,
                task,
                "--provider",
                "command",
                "--worktree",
            ])
            .arg(worktree);
        if retry {
            command.arg("--retry");
        }
        output(
            command
                .args(["--", "/bin/sh"])
                .arg(self.root().join("worker.sh"))
                .arg(self.root().join("power.sock"))
                .arg(self.root().join("state"))
                .arg(worktree)
                .args(worker),
        )
    }

    fn release(&self, marker: &str) {
        fs::write(self.root().join(marker), []).unwrap();
    }

    fn wait_for_worker(&self, marker: &str) {
        let started = self.root().join(format!("{marker}.started"));
        poll("worker fixture to start", || {
            started.is_file().then_some(())
        });
    }

    fn wait_for_state(&self, run: &str, id: &str, state: TaskState) -> Task {
        poll(&format!("task {id} to reach {state:?}"), || {
            let task = self
                .show(run)
                .tasks
                .into_iter()
                .find(|task| task.id == id)
                .unwrap();
            (task.state == state).then_some(task)
        })
    }

    fn report_success(&self, run: &str, task: &str, attempt: &str, summary: &str) -> Value {
        self.json(&[
            "dag",
            "report",
            "success",
            "--summary",
            summary,
            "--run",
            run,
            "--task",
            task,
            "--attempt",
            attempt,
        ])
    }

    fn events(&self, run: &str, since: u64, kinds: &[&str]) -> EventPage {
        let mut command = power(self.root());
        command
            .args(["dag", "events", run, "--since"])
            .arg(since.to_string());
        for kind in kinds {
            command.args(["--kind", kind]);
        }
        EventPage::parse(json_output(output(&mut command)), since)
    }

    fn request(&self, request: Value) -> BufReader<UnixStream> {
        let mut socket = UnixStream::connect(self.root().join("power.sock")).unwrap();
        socket.set_read_timeout(Some(TIMEOUT)).unwrap();
        socket.set_write_timeout(Some(TIMEOUT)).unwrap();
        writeln!(socket, "{request}").unwrap();
        BufReader::new(socket)
    }

    fn subscribe(&self, session: &str) -> BufReader<UnixStream> {
        self.request(json!({"request": "subscribe", "session": session}))
    }

    fn attention(&self) -> Vec<BoxAttention> {
        // Attention is exposed on the daemon socket for the desktop.
        let mut reply = self.request(json!({"request": "dag", "dag": {"op": "attention"}}));
        let mut line = String::new();
        reply.read_line(&mut line).unwrap();
        let mut response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["event"], "dag", "{response}");
        assert_eq!(response["dag"]["kind"], "attention", "{response}");
        serde_json::from_value(response["dag"]["boxes"].take()).unwrap()
    }
}

impl Drop for Fixture {
    /// Stops the fixture's slice, which ends anything a failed test left in it.
    fn drop(&mut self) {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &test_slice(self.root())])
            .stderr(Stdio::null())
            .status();
    }
}

#[track_caller]
fn next_metadata(subscription: &mut BufReader<UnixStream>) -> Value {
    for line in subscription.lines() {
        let mut event: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if event["event"] == "metadata" {
            return event["metadata"].take();
        }
        assert!(
            !matches!(event["event"].as_str(), Some("error" | "exit" | "ok")),
            "Expected metadata before terminal exit, error, or keepalive. Received {event}"
        );
    }
    panic!("Terminal subscription ended before sending metadata");
}

struct EventPage {
    events: Vec<RunEvent>,
    cursor: u64,
}

impl EventPage {
    fn parse(mut json: Value, since: u64) -> Self {
        let events: Vec<RunEvent> = serde_json::from_value(json["events"].take()).unwrap();
        let cursor = json["cursor"].as_u64().unwrap();
        let mut previous = since;
        for event in &events {
            assert!(
                event.seq > previous,
                "Expected increasing event seq: {events:?}"
            );
            previous = event.seq;
        }
        assert_eq!(cursor, previous);
        Self { events, cursor }
    }
}

#[track_caller]
fn dispatched(output: Output) -> (Attempt, ManagedAgent) {
    let mut json = json_output(output);
    (
        serde_json::from_value(json["attempt"].take()).unwrap(),
        serde_json::from_value(json["agent"].take()).unwrap(),
    )
}

fn task<'a>(run: &'a Run, id: &str) -> &'a Task {
    run.tasks.iter().find(|task| task.id == id).unwrap()
}

fn ready_ids(fixture: &Fixture, run: &str) -> Vec<String> {
    let tasks: Vec<Task> =
        serde_json::from_value(fixture.json(&["dag", "ready", run])["tasks"].take()).unwrap();
    assert!(tasks.iter().all(|task| task.state == TaskState::Ready));
    let mut ids: Vec<_> = tasks.into_iter().map(|task| task.id).collect();
    ids.sort();
    ids
}

#[test]
fn create_show_and_list_include_links_for_each_run() {
    let fixture = Fixture::new();
    let ids = ["First run", "Second run"].map(|title| {
        let created = fixture.create_run_json(title);
        let id = created["run"]["id"].as_str().unwrap();
        assert!(!id.is_empty());
        let link = format!("pwr://dag/{id}");
        assert_eq!(created["link"], link);
        let shown = fixture.json(&["dag", "show", id]);
        assert_eq!(shown["run"]["id"], id);
        assert_eq!(shown["link"], link);
        id.to_owned()
    });
    assert_ne!(ids[0], ids[1]);

    let listed = fixture.json(&["dag", "list", "--box", BOX_ID]);
    let runs = listed["runs"].as_array().unwrap();
    let links = listed["links"].as_object().unwrap();
    assert_eq!(runs.len(), ids.len());
    assert_eq!(links.len(), ids.len());
    for id in &ids {
        assert!(runs.iter().any(|run| run["id"] == id.as_str()));
        assert_eq!(links[id], format!("pwr://dag/{id}"));
    }
}

#[test]
fn archive_hides_a_run_and_refuses_open_attempts_unless_forced() {
    let fixture = Fixture::new();
    let run = fixture.create_run().id;
    fixture.add_task(&run, "work");
    let listed = |all: bool| -> Vec<Value> {
        let mut args = vec!["dag", "list", "--box", BOX_ID];
        if all {
            args.push("--all");
        }
        fixture.json(&args)["runs"].as_array().unwrap().clone()
    };
    assert_eq!(listed(false).len(), 1);
    dispatched(fixture.dispatch(
        &run,
        "work",
        &fixture.worktrees[0],
        false,
        &["success", "go", "Done"],
    ));
    fixture.wait_for_state(&run, "work", TaskState::Working);

    let refused = fixture.command(&["dag", "archive", &run]);
    assert_rejected(&refused);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("open attempt") && stderr.contains("--force"),
        "{stderr}"
    );
    assert_eq!(listed(false).len(), 1);

    let archived = fixture.json(&["dag", "archive", &run, "--force"]);
    assert_eq!(archived["run"]["status"], "archived");
    assert!(archived["run"]["archived_at"].is_u64());
    assert!(listed(false).is_empty());
    let all = listed(true);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["status"], "archived");
    assert_eq!(
        fixture.show(&run).archived_at,
        archived["run"]["archived_at"].as_u64()
    );

    let restored = fixture.json(&["dag", "unarchive", &run]);
    assert_eq!(restored["run"]["status"], "active");
    assert!(restored["run"].get("archived_at").is_none());
    assert_eq!(listed(false).len(), 1);

    fixture.release("go");
    fixture.wait_for_state(&run, "work", TaskState::Completed);
}

#[test]
fn legacy_agent_aliases_use_the_configured_profiles() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_config(
        root,
        r#"
        agents.claude = {
            command = "/bin/echo",
            injection = { kind = "none" },
            model_callback = { kind = "none" },
            resume = { assign = {} },
        }
        agents.codex = { command = "/bin/echo", injection = { kind = "none" } }
        "#,
    );
    for prefix in [&[][..], &["agent"][..]] {
        for id in ["claude", "codex"] {
            let aliased = output(power(root).args(prefix).args([id, "--", "two words"]));
            assert_success(&aliased);
            assert_eq!(aliased.stdout, b"two words\n");
        }
    }
}

#[test]
fn configured_profile_works_in_a_shell_and_as_a_dag_worker() {
    let mut fixture = Fixture::with_configure(|root| {
        let program = root.join("fixture program");
        fs::write(
            &program,
            r#"#!/bin/sh
set -eu
test "$1" = "--instructions"
case "$2" in
    *'power skills get <name>'*) ;;
    *) exit 2 ;;
esac
shift 2
if [ "${POWER_RUN:-}" = "" ]; then
    printf '%s\n' "$@"
    exit
fi
case "$7" in
    *'Task: profile'*'power dag report success'*) ;;
    *) exit 3 ;;
esac
exec /bin/sh "${POWER_SOCKET%/*}/worker.sh" "$@"
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        write_config(
            root,
            &format!(
                r##"
                keys.prefix.tab = "ctrl-a"
                power_bar.agent_colors.Codex = "#62a0ea"
                agents["fixture-agent"] = {{
                    command = {:?},
                    injection = {{ kind = "flag", flag = "--instructions" }},
                }}
                "##,
                program.to_str().unwrap()
            ),
        );
    });

    let wrapped = output(
        Command::new("/bin/bash")
            .env("POWER_BIN", env!("CARGO_BIN_EXE_power"))
            .env("XDG_CONFIG_HOME", fixture.root().join("config"))
            .env_remove("POWER_RUN")
            .args([
                "--norc",
                "-c",
                r#"eval "$("$POWER_BIN" shell-init bash)"; fixture-agent "$@""#,
                "test-wrapper",
                "--custom-flag",
                "two words",
                "--",
            ]),
    );
    assert_success(&wrapped);
    assert_eq!(wrapped.stdout, b"--custom-flag\ntwo words\n--\n");

    let worktree = fixture.worktrees[0].to_str().unwrap();
    let started = fixture.command(&["session", "start", "fixture-agent", worktree]);
    assert_success(&started);
    let session = String::from_utf8(started.stdout).unwrap();
    let sessions = fixture.command(&["session", "list"]);
    assert_success(&sessions);
    assert!(
        String::from_utf8(sessions.stdout)
            .unwrap()
            .lines()
            .any(|line| line == format!("{}\tfixture-agent\t{worktree}", session.trim()))
    );

    let run = fixture.create_run();
    fixture.add_task(&run.id, "profile");
    let launch = |provider: &str| {
        output(
            power(fixture.root())
                .args([
                    "dag",
                    "dispatch",
                    &run.id,
                    "profile",
                    "--provider",
                    provider,
                    "--worktree",
                    worktree,
                ])
                .arg("--")
                .arg(fixture.root().join("power.sock"))
                .arg(fixture.root().join("state"))
                .args([worktree, "success", "profile-go", "Profile result"]),
        )
    };
    let rejected = launch("missing");
    assert_rejected(&rejected);
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        error.contains("Known profiles: claude, codex, grok, jcode, fixture-agent"),
        "{error}"
    );
    assert!(task(&fixture.show(&run.id), "profile").attempts.is_empty());

    let (attempt, agent) = dispatched(launch("fixture-agent"));
    assert_eq!(agent.provider, Provider::Agent("fixture-agent".into()));
    assert_eq!(attempt.provider, agent.provider);
    fixture.release("profile-go");
    let task = fixture.wait_for_state(&run.id, "profile", TaskState::Completed);
    assert_eq!(task.attempts[0].summary.as_deref(), Some("Profile result"));

    // Drop the profile. The attempt keeps the provider it ran with.
    write_config(fixture.root(), "");
    fixture.restart();
    let persisted = fixture.show(&run.id);
    assert_eq!(persisted.tasks[0].attempts[0].provider, agent.provider);
}

#[test]
fn jcode_worker_gets_instructions_and_task_as_its_first_message_and_reports() {
    let fixture = Fixture::with_configure(|root| {
        let program = root.join("jcode");
        // A deterministic stand-in for jcode's TUI: it takes no arguments, reads the
        // typed first message, and reports from a command like jcode's bash tool runs.
        fs::write(
            &program,
            r#"#!/bin/sh
set -eu
dir="${POWER_SOCKET%/*}"
test "$#" -eq 0
test -n "$POWER_ATTEMPT"
printf '%s' "$JCODE_SOCKET" > "$dir/jcode.socket"
stty -echo
# Like a TUI in raw mode, announce that typed input is welcome.
printf '\033[?2004h'
end="$(printf '\033')[201~"
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$dir/jcode.message"
    case "$line" in *"$end"*) break ;; esac
done
# The second Enter arrives as an empty line.
IFS= read -r second
test -z "$second"
JCODE_SESSION_ID=session_fake_dag "$POWER_BIN" dag report success --summary "jcode done"
read -r finish
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        write_config(
            root,
            &format!(
                "agents.jcode = {{ command = {:?} }}",
                program.to_str().unwrap()
            ),
        );
    });
    let run = fixture.create_run().id;
    fixture.json(&[
        "dag",
        "add-task",
        &run,
        "--id",
        "light",
        "--title",
        "Use a lighter agent",
    ]);
    let worktree = &fixture.worktrees[0];
    let (attempt, agent) = dispatched(fixture.command(&[
        "dag",
        "dispatch",
        &run,
        "light",
        "--provider",
        "jcode",
        "--worktree",
        worktree.to_str().unwrap(),
    ]));
    assert_eq!(agent.provider, Provider::Agent("jcode".into()));
    let task = fixture.wait_for_state(&run, "light", TaskState::Completed);
    assert_eq!(task.attempts[0].summary.as_deref(), Some("jcode done"));
    assert!(attempt.session.is_some());
    let socket = fs::read_to_string(fixture.root().join("jcode.socket")).unwrap();
    assert_eq!(Path::new(&socket).parent(), Some(fixture.root()));
    assert!(socket.len() <= 107, "{socket}");
    let message = fs::read_to_string(fixture.root().join("jcode.message")).unwrap();
    assert!(
        message.starts_with("\x1b[200~You are running inside Power Overwhelming."),
        "{message:?}"
    );
    assert!(message.contains("Task: light"), "{message:?}");
    assert!(message.contains("power dag report success"), "{message:?}");
    assert!(message.trim_end().ends_with("\x1b[201~"), "{message:?}");
}

/// Configures a fake jcode at `root/jcode` running `script` after a common preamble that
/// counts launches and records each one's working directory, arguments and socket.
fn fake_jcode(root: &Path, script: &str) {
    let program = root.join("jcode");
    fs::write(
        &program,
        format!(
            r#"#!/bin/sh
set -eu
dir="${{POWER_SOCKET%/*}}"
n=$(($(cat "$dir/jcode.count" 2>/dev/null || echo 0) + 1))
echo "$n" > "$dir/jcode.count"
pwd -P > "$dir/jcode.$n.cwd"
printf '%s\n' "$@" > "$dir/jcode.$n.args"
printf '%s' "$JCODE_SOCKET" > "$dir/jcode.$n.socket"
stty -echo
# Like a TUI in raw mode, announce that typed input is welcome.
printf '\033[?2004h'
end="$(printf '\033')[201~"
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$dir/jcode.$n.message"
    case "$line" in *"$end"*) break ;; esac
done
# The second Enter arrives as an empty line.
IFS= read -r second
test -z "$second"
{script}
read -r finish
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    write_config(
        root,
        &format!(
            "agents.jcode = {{ command = {:?} }}",
            program.to_str().unwrap()
        ),
    );
}

#[test]
fn restart_resumes_an_active_jcode_worker_once_in_its_worktree_and_it_continues() {
    let mut fixture = Fixture::with_configure(|root| {
        // The first launch reports its conversation and works until the restart. The
        // resumed one reports the result.
        fake_jcode(
            root,
            r#"if test "$n" = 1; then
    JCODE_SESSION_ID=session_dag_1 "$POWER_BIN" session task working
    touch "$dir/jcode.reported"
else
    JCODE_SESSION_ID="$2" "$POWER_BIN" dag report success --summary "resumed $2"
fi"#,
        );
    });
    let run = fixture.create_run().id;
    fixture.add_task(&run, "long");
    let worktree = fixture.worktrees[0].clone();
    // A selector from the dispatch must not reach the restore as a second `--resume`.
    let (attempt, _) = dispatched(fixture.command(&[
        "dag",
        "dispatch",
        &run,
        "long",
        "--provider",
        "jcode",
        "--worktree",
        worktree.to_str().unwrap(),
        "--",
        "--resume",
        "session_stale",
    ]));
    let reported = fixture.root().join("jcode.reported");
    poll("jcode to report its conversation", || {
        reported.is_file().then_some(())
    });

    fixture.restart();

    let task = fixture.wait_for_state(&run, "long", TaskState::Completed);
    assert_eq!(task.attempts.len(), 1);
    assert_eq!(task.attempts[0].id, attempt.id);
    assert_eq!(
        task.attempts[0].summary.as_deref(),
        Some("resumed session_dag_1")
    );
    let read = |name: &str| fs::read_to_string(fixture.root().join(name)).unwrap();
    assert_eq!(read("jcode.count"), "2\n");
    assert_eq!(read("jcode.1.args"), "--resume\nsession_stale\n");
    assert_eq!(read("jcode.2.args"), "--resume\nsession_dag_1\n");
    for launch in 1..=2 {
        assert_eq!(
            read(&format!("jcode.{launch}.cwd")).trim_end(),
            worktree.to_str().unwrap()
        );
    }
    assert_ne!(read("jcode.1.socket"), read("jcode.2.socket"));
    let first = read("jcode.1.message");
    assert!(first.contains("Task: long"), "{first:?}");
    // The resumed worker is told to continue, and the initial prompt is not replayed.
    let resumed = read("jcode.2.message");
    assert!(
        resumed.contains("Power restarted and resumed this session."),
        "{resumed:?}"
    );
    assert!(
        resumed.contains(&format!("run {run}, task long")),
        "{resumed:?}"
    );
    assert!(!resumed.contains("Task: long"), "{resumed:?}");
}

#[test]
#[ignore = "enable when #83 lands: cgroup cleanup ends a worker's tree after a terminal report"]
fn jcode_worker_tree_ends_after_its_terminal_report() {
    let fixture = Fixture::with_configure(|root| {
        // Like jcode's server, the child leaves the worker's Unix session.
        fake_jcode(
            root,
            r#"setsid sleep 300 < /dev/null > /dev/null 2>&1 &
echo "$!" > "$dir/detached.pid"
"$POWER_BIN" dag report success --summary "done""#,
        );
    });
    let run = fixture.create_run().id;
    fixture.add_task(&run, "tree");
    dispatched(fixture.command(&[
        "dag",
        "dispatch",
        &run,
        "tree",
        "--provider",
        "jcode",
        "--worktree",
        fixture.worktrees[0].to_str().unwrap(),
    ]));
    fixture.wait_for_state(&run, "tree", TaskState::Completed);
    let pid = fs::read_to_string(fixture.root().join("detached.pid")).unwrap();
    let proc = PathBuf::from(format!("/proc/{}", pid.trim()));
    poll("the detached child to exit", || {
        (!proc.exists()).then_some(())
    });
}

#[test]
fn worker_metadata_starts_with_task_title_and_explicit_model() {
    let mut fixture = Fixture::with_configure(|root| {
        let program = root.join("codex");
        fs::write(
            &program,
            r#"#!/bin/sh
set -eu
test "$1" = "--model=launch-model"
fixture_dir="${POWER_SOCKET%/*}"
for phase in update finish; do
    for step in $(seq 400); do
        test ! -f "$fixture_dir/$phase" || break
        sleep 0.05
    done
    test -f "$fixture_dir/$phase"
    case "$phase" in
        update)
            "$POWER_BIN" session task "Revised task"
            printf '%s' '{"model":"actual-model"}' | "$POWER_BIN" session model --json
            ;;
        finish) "$POWER_BIN" dag report success --summary "Done" ;;
    esac
done
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        write_config(
            root,
            &format!(
                r#"agents.codex = {{ command = {:?}, injection = {{ kind = "none" }}, turn_hook = {{ kind = "none" }} }}"#,
                program.to_str().unwrap(),
            ),
        );
    });
    let run = fixture.create_run().id;
    fixture.json(&[
        "dag",
        "add-task",
        &run,
        "--id",
        "model",
        "--title",
        "Check model labels",
    ]);
    let (attempt, agent) = dispatched(fixture.command(&[
        "dag",
        "dispatch",
        &run,
        "model",
        "--provider",
        "codex",
        "--worktree",
        fixture.worktrees[0].to_str().unwrap(),
        "--",
        "--model=launch-model",
    ]));
    assert_eq!(attempt.model.as_deref(), Some("launch-model"));
    assert_eq!(agent.model, attempt.model);
    let mut attached = fixture.subscribe(attempt.session.as_deref().unwrap());
    assert_eq!(
        next_metadata(&mut attached),
        json!({"task": "Check model labels", "model": "launch-model"})
    );
    fixture.release("update");
    assert_eq!(
        next_metadata(&mut attached),
        json!({"task": "Revised task", "model": "launch-model"})
    );
    let updated = poll("the model change", || {
        fixture
            .events(&run, 0, &["agent_model_changed"])
            .events
            .pop()
    });
    assert_eq!(updated.attempt.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(updated.summary.as_deref(), Some("actual-model"));
    assert_eq!(
        task(&fixture.show(&run), "model").attempts[0]
            .model
            .as_deref(),
        Some("actual-model")
    );
    assert_eq!(fixture.agents()[0].model.as_deref(), Some("actual-model"));
    fixture.release("finish");
    fixture.wait_for_state(&run, "model", TaskState::Completed);
    fixture.restart();
    assert_eq!(
        task(&fixture.show(&run), "model").attempts[0]
            .model
            .as_deref(),
        Some("actual-model")
    );
    assert_eq!(fixture.agents()[0].model.as_deref(), Some("actual-model"));
}

#[test]
fn background_workers_unlock_dependent_task_only_after_both_succeed() {
    let fixture = Fixture::new();
    let boxes: Vec<BoxInfo> =
        serde_json::from_value(fixture.json(&["box", "list"])["boxes"].take()).unwrap();
    assert_eq!(
        boxes,
        [BoxInfo {
            id: BOX_ID.into(),
            name: "box-repo".into(),
            repository: fixture.checkout.join(".git"),
            checkout: fixture.checkout.clone(),
            shortcut: Some(7),
            open: true,
        }]
    );
    let run = fixture.create_run();
    assert_eq!(run.box_id, BOX_ID);
    assert_eq!(run.title, "Fixture run");
    assert_eq!(
        run.coordinator,
        Coordinator::External {
            label: COORDINATOR.into()
        }
    );
    let run = run.id.as_str();
    json_output(fixture.apply(
        run,
        json!([
            {"id": "join", "title": "Join results", "deps": ["alpha", "beta"]},
            {"id": "alpha", "title": "First workstream"},
            {"id": "beta", "title": "Second workstream"}
        ]),
    ));
    assert_eq!(ready_ids(&fixture, run), ["alpha", "beta"]);
    assert_eq!(task(&fixture.show(run), "join").state, TaskState::Waiting);

    let (alpha, alpha_agent) = dispatched(fixture.dispatch(
        run,
        "alpha",
        &fixture.worktrees[0],
        false,
        &["success", "alpha-go", "Alpha result", "alpha-exit"],
    ));
    let (beta, beta_agent) = dispatched(fixture.dispatch(
        run,
        "beta",
        &fixture.worktrees[1],
        false,
        &["success", "beta-go", "Beta result"],
    ));
    let listed = fixture.agents();
    assert_eq!(listed.len(), 2);
    for (id, attempt, agent, worktree) in [
        ("alpha", &alpha, &alpha_agent, &fixture.worktrees[0]),
        ("beta", &beta, &beta_agent, &fixture.worktrees[1]),
    ] {
        assert!(listed.contains(agent), "{listed:?}");
        assert_eq!(agent.box_id, BOX_ID);
        assert_eq!(agent.run.as_deref(), Some(run));
        assert_eq!(agent.task.as_deref(), Some(id));
        assert_eq!(agent.attempt.as_deref(), Some(attempt.id.as_str()));
        assert_eq!(agent.parent, None);
        assert_eq!(&agent.worktree, worktree);
        assert_eq!(agent.provider, Provider::Command);
        assert_eq!(json!(agent.state), "running");
        assert_eq!(json!(agent.status), "working");
        assert_eq!(attempt.agent, agent.id);
        assert_eq!(attempt.session, agent.session);
        assert!(
            agent
                .session
                .as_deref()
                .is_some_and(|session| !session.is_empty())
        );
        assert_eq!(attempt.number, 1);
        assert_eq!(attempt.state, AttemptState::Active);
    }
    assert_ne!(alpha_agent.session, beta_agent.session);
    let layout: Layout =
        serde_json::from_slice(&fs::read(fixture.root().join("state/boxes.json")).unwrap())
            .unwrap();
    assert!(layout.boxes[0].views.is_empty());

    fixture.release("alpha-go");
    let alpha = fixture.wait_for_state(run, "alpha", TaskState::Completed);
    assert_eq!(alpha.attempts[0].summary.as_deref(), Some("Alpha result"));
    let status = |id: &str| {
        let agents = fixture.json(&["agent", "list", "--box", BOX_ID, "--all"])["agents"].take();
        let agent = agents
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == id);
        agent.unwrap()["status"].clone()
    };
    assert_eq!(status(&alpha_agent.id), "done");
    assert_eq!(status(&beta_agent.id), "working");
    let live = fixture.json(&["agent", "list", "--box", BOX_ID])["agents"].take();
    let live: Vec<_> = live
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].clone())
        .collect();
    assert!(!live.contains(&alpha_agent.id.as_str().into()));
    assert!(live.contains(&beta_agent.id.as_str().into()));
    let everywhere = fixture.json(&["agent", "list", "--all-boxes"])["agents"].take();
    assert!(
        everywhere
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == beta_agent.id.as_str())
    );
    fixture.release("alpha-exit");
    poll("alpha agent to exit", || {
        (status(&alpha_agent.id) == "exited").then_some(())
    });
    let snapshot = fixture.show(run);
    assert_eq!(task(&snapshot, "beta").state, TaskState::Working);
    assert_eq!(task(&snapshot, "join").state, TaskState::Waiting);
    assert!(ready_ids(&fixture, run).is_empty());
    assert_rejected(&fixture.dispatch(
        run,
        "join",
        &fixture.worktrees[0],
        false,
        &["success", "join-go", "Joined"],
    ));
    assert!(task(&fixture.show(run), "join").attempts.is_empty());
    let before_beta = fixture.events(run, 0, &["task_ready"]);
    assert!(
        before_beta
            .events
            .iter()
            .all(|event| event.task.as_deref() != Some("join"))
    );

    fixture.release("beta-go");
    let beta = fixture.wait_for_state(run, "beta", TaskState::Completed);
    assert_eq!(beta.attempts[0].summary.as_deref(), Some("Beta result"));
    fixture.wait_for_state(run, "join", TaskState::Ready);
    assert_eq!(ready_ids(&fixture, run), ["join"]);
    let ready = fixture.events(run, before_beta.cursor, &["task_ready"]);
    assert_eq!(
        ready
            .events
            .iter()
            .filter(|event| event.task.as_deref() == Some("join"))
            .count(),
        1,
    );
}

#[test]
fn show_lists_artifacts_published_from_each_attempts_session() {
    let fixture = Fixture::new();
    let run = fixture.create_run().id;
    fixture.add_task(&run, "work");
    let (_, agent) = dispatched(fixture.dispatch(
        &run,
        "work",
        &fixture.worktrees[0],
        false,
        &["success", "finish", "done"],
    ));
    let session = agent.session.unwrap();
    let publish = |session: &str, file: &str, title: &str| {
        fs::write(fixture.worktrees[0].join(file), b"fixture").unwrap();
        let published = output(
            power(fixture.root())
                .current_dir(&fixture.worktrees[0])
                .args(["artifact", "publish", file, "--title", title])
                .env("POWER_SESSION", session),
        );
        assert_success(&published);
        String::from_utf8(published.stdout)
            .unwrap()
            .trim()
            .to_owned()
    };
    let first = publish(&session, "first.png", "First screenshot");
    let second = publish(&session, "second.png", "Second screenshot");
    let shown = fixture.json(&["dag", "show", &run]);
    let artifacts = &shown["run"]["tasks"][0]["attempts"][0]["artifacts"];
    let listed: Vec<_> = artifacts
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| {
            let id = artifact["id"].as_str().unwrap();
            assert_eq!(artifact["link"], format!("pwr://artifact/{id}"));
            (
                artifact["link"].as_str().unwrap().to_owned(),
                artifact["title"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (first, "First screenshot".to_owned()),
            (second, "Second screenshot".to_owned()),
        ]
    );
    fixture.release("finish");
    fixture.wait_for_state(&run, "work", TaskState::Completed);
}

#[test]
fn worker_input_request_appears_in_attention_until_work_resumes() {
    let fixture = Fixture::new();
    let run = fixture.create_run().id;
    fixture.add_task(&run, "ask");
    assert!(fixture.attention().is_empty());
    let (attempt, agent) = dispatched(fixture.dispatch(
        &run,
        "ask",
        &fixture.worktrees[0],
        false,
        &["input", "resume", "finish"],
    ));

    let blocked = fixture.wait_for_state(&run, "ask", TaskState::NeedsInput);
    assert_eq!(
        blocked.attempts[0].blocker.as_deref(),
        Some("Which option should I use?")
    );
    let listed = fixture.agents();
    assert_eq!(json!(listed[0].status), "needs_input");
    assert_eq!(
        fixture.attention(),
        [BoxAttention {
            box_id: BOX_ID.into(),
            sessions: vec![agent.session.unwrap()],
            agents: vec![agent.id],
        }]
    );

    fixture.release("resume");
    let resumed = fixture.wait_for_state(&run, "ask", TaskState::Working);
    assert_eq!(
        resumed.current_attempt.as_deref(),
        Some(attempt.id.as_str())
    );
    assert_eq!(resumed.attempts[0].blocker, None);
    assert!(fixture.attention().is_empty());
    let page = fixture.events(&run, 0, &["needs_input", "input_resolved"]);
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events[0].kind, RunEventKind::NeedsInput);
    assert_eq!(page.events[1].kind, RunEventKind::InputResolved);
    for event in page.events {
        assert_eq!(event.attempt.as_deref(), Some(attempt.id.as_str()));
    }

    fixture.release("finish");
    fixture.wait_for_state(&run, "ask", TaskState::Completed);
}

#[test]
fn invalid_dependencies_leave_the_graph_unchanged() {
    let fixture = Fixture::new();
    let run = fixture.create_run().id;
    json_output(fixture.apply(
        &run,
        json!([
            {"id": "root", "title": "Root"},
            {"id": "leaf", "title": "Leaf", "deps": ["root"]}
        ]),
    ));
    let original = fixture.show(&run);
    let cursor = fixture.events(&run, 0, &[]).cursor;

    assert_rejected(&fixture.apply(
        &run,
        json!({"tasks": [
            {"id": "valid", "title": "Would otherwise be ready"},
            {"id": "a", "title": "A", "deps": ["b"]},
            {"id": "b", "title": "B", "deps": ["a"]}
        ]}),
    ));
    assert_eq!(fixture.show(&run), original);

    for (task, dep) in [("root", "leaf"), ("root", "root"), ("leaf", "missing")] {
        assert_rejected(&fixture.command(&["dag", "add-dep", &run, task, dep]));
        assert_eq!(fixture.show(&run), original);
    }
    assert!(fixture.events(&run, cursor, &[]).events.is_empty());
}

#[test]
fn simultaneous_dispatches_create_exactly_one_attempt() {
    let fixture = Fixture::new();
    let run = fixture.create_run().id;
    fixture.add_task(&run, "race");
    let start = Barrier::new(2);
    let dispatch = |index: usize| {
        start.wait();
        fixture.dispatch(
            &run,
            "race",
            &fixture.worktrees[index],
            false,
            &["success", "race-go", "Won"],
        )
    };
    let [one, two] = thread::scope(|scope| {
        let one = scope.spawn(|| dispatch(0));
        let two = scope.spawn(|| dispatch(1));
        [one.join().unwrap(), two.join().unwrap()]
    });
    let (winner, loser) = if one.status.success() {
        (one, two)
    } else {
        (two, one)
    };
    let (attempt, agent) = dispatched(winner);
    assert_rejected(&loser);
    let snapshot = fixture.show(&run);
    let task = task(&snapshot, "race");
    assert_eq!(task.state, TaskState::Working);
    assert_eq!(task.current_attempt.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(task.attempts, [attempt]);
    assert_eq!(fixture.agents(), [agent]);
    assert_eq!(
        fixture.events(&run, 0, &["attempt_started"]).events.len(),
        1
    );

    fixture.release("race-go");
    fixture.wait_for_state(&run, "race", TaskState::Completed);
}

#[test]
fn interrupted_worker_requires_retry_and_old_reports_cannot_change_new_attempt() {
    let fixture = Fixture::new();
    let run = fixture.create_run().id;
    fixture.json(&[
        "dag",
        "add-task",
        &run,
        "--id",
        "retry-me",
        "--title",
        "Retry the worker",
    ]);
    let (first, agent) = dispatched(fixture.dispatch(
        &run,
        "retry-me",
        &fixture.worktrees[0],
        false,
        &["label", "update-task", "First attempt label", "exit-now"],
    ));
    assert_eq!(first.number, 1);
    let mut attached = fixture.subscribe(first.session.as_deref().unwrap());
    assert_eq!(next_metadata(&mut attached)["task"], "Retry the worker");
    fixture.release("update-task");
    assert_eq!(next_metadata(&mut attached)["task"], "First attempt label");
    fixture.release("exit-now");
    let interrupted = fixture.wait_for_state(&run, "retry-me", TaskState::Interrupted);
    let attempt = &interrupted.attempts[0];
    assert_eq!(attempt.id, first.id);
    assert_eq!(attempt.state, AttemptState::Interrupted);
    assert!(
        attempt
            .detail
            .as_deref()
            .is_some_and(|detail| !detail.trim().is_empty()),
        "Missing worker exit detail: {attempt:?}",
    );
    poll("worker agent to exit", || {
        fixture
            .agents()
            .into_iter()
            .find(|listed| listed.id == agent.id && json!(listed.state) == "exited")
    });

    let worker = ["success", "retry-go", "Retried result"];
    assert_rejected(&fixture.dispatch(&run, "retry-me", &fixture.worktrees[0], false, &worker));
    assert_eq!(task(&fixture.show(&run), "retry-me"), &interrupted);
    let (second, _) =
        dispatched(fixture.dispatch(&run, "retry-me", &fixture.worktrees[0], true, &worker));
    assert_eq!(second.number, 2);
    assert_ne!(second.id, first.id);
    let mut reattached = fixture.subscribe(second.session.as_deref().unwrap());
    assert_eq!(next_metadata(&mut reattached)["task"], "Retry the worker");
    let current = fixture.wait_for_state(&run, "retry-me", TaskState::Working);
    assert_eq!(current.current_attempt.as_deref(), Some(second.id.as_str()));
    assert_eq!(current.attempts, [attempt.clone(), second.clone()]);

    let before_stale = fixture.events(&run, 0, &[]).cursor;
    let stale = fixture.report_success(&run, "retry-me", &first.id, "Late first result");
    assert_eq!(stale, json!({"applied": false, "reason": "stale attempt"}));
    assert_eq!(task(&fixture.show(&run), "retry-me"), &current);
    let page = fixture.events(&run, before_stale, &[]);
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, RunEventKind::StaleReport);
    assert_eq!(page.events[0].task.as_deref(), Some("retry-me"));
    assert_eq!(page.events[0].attempt.as_deref(), Some(first.id.as_str()));

    fixture.release("retry-go");
    let completed = fixture.wait_for_state(&run, "retry-me", TaskState::Completed);
    assert_eq!(completed.attempts[1].state, AttemptState::Succeeded);
    assert_eq!(
        completed.attempts[1].summary.as_deref(),
        Some("Retried result")
    );
    let before_replay = fixture.events(&run, 0, &[]).cursor;
    let replay = fixture.report_success(&run, "retry-me", &second.id, "Retried result");
    assert_eq!(
        replay,
        json!({"applied": false, "reason": "already reported"})
    );
    assert_eq!(task(&fixture.show(&run), "retry-me"), &completed);
    assert!(fixture.events(&run, before_replay, &[]).events.is_empty());
}

#[test]
fn agent_end_interrupts_a_working_worker() {
    let fixture = Fixture::new();
    let run = fixture.create_run().id;
    fixture.json(&[
        "dag", "add-task", &run, "--id", "stop-me", "--title", "Stop me",
    ]);
    let (attempt, agent) = dispatched(fixture.dispatch(
        &run,
        "stop-me",
        &fixture.worktrees[0],
        false,
        &["success", "never-released", "Unused"],
    ));
    fixture.wait_for_state(&run, "stop-me", TaskState::Working);
    let ended = fixture.command(&["agent", "end", &agent.id, "--box", BOX_ID]);
    assert_success(&ended);
    let interrupted = fixture.wait_for_state(&run, "stop-me", TaskState::Interrupted);
    assert_eq!(interrupted.attempts[0].id, attempt.id);
    assert_eq!(interrupted.attempts[0].state, AttemptState::Interrupted);
    poll("ended agent to exit", || {
        fixture
            .agents()
            .into_iter()
            .find(|listed| listed.id == agent.id && json!(listed.state) == "exited")
    });
    let missing = fixture.command(&["agent", "end", "no-such-agent", "--box", BOX_ID]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("No agent no-such-agent"));
}

/// Whether the process runs. A zombie has exited.
fn process_running(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().next())
            .is_some_and(|state| !matches!(state, "Z" | "X"))
    })
}

fn with_grace(grace: &str) -> Fixture {
    Fixture::with_configure(|root| {
        write_config(root, &format!("dag.end_finished_workers_after = {grace}\n"))
    })
}

fn read_pids(path: &Path, count: usize) -> Vec<u32> {
    poll("worker to record its processes", || {
        let pids: Vec<u32> = fs::read_to_string(path)
            .ok()?
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        (pids.len() == count).then_some(pids)
    })
}

/// Polls until the agent is exited, and fails if it is ever exited while any of
/// `pids` still runs.
fn wait_for_exit_after(fixture: &Fixture, agent: &str, pids: &[u32]) {
    poll("finished agent to exit", || {
        let exited = fixture
            .agents()
            .into_iter()
            .any(|listed| listed.id == agent && json!(listed.state) == "exited");
        if exited {
            let alive: Vec<_> = pids.iter().filter(|pid| process_running(**pid)).collect();
            assert!(alive.is_empty(), "agent exited while {alive:?} still ran");
        }
        exited.then_some(())
    });
}

#[test]
fn a_finished_worker_and_its_descendants_end_after_the_configured_grace() {
    let fixture = with_grace("0.5");
    let run = fixture.create_run().id;
    fixture.add_task(&run, "linger");
    let (attempt, agent) = dispatched(fixture.dispatch(
        &run,
        "linger",
        &fixture.worktrees[0],
        false,
        &["linger", "linger-go", "Done, still running", "linger-pids"],
    ));
    fixture.release("linger-go");
    let pids = read_pids(&fixture.root().join("linger-pids"), 3);
    let completed = fixture.wait_for_state(&run, "linger", TaskState::Completed);
    let reported = Instant::now();

    // Half a second of grace, then SIGTERM, which these processes obey. The
    // default grace is ten seconds, so ending well before that proves the setting.
    wait_for_exit_after(&fixture, &agent.id, &pids);
    let ended = reported.elapsed();
    assert!(ended < Duration::from_secs(4), "took {ended:?}");
    for pid in &pids {
        assert!(!process_running(*pid), "{pid} survived");
    }
    let task = task(&fixture.show(&run), "linger").clone();
    assert_eq!(task.state, TaskState::Completed);
    assert_eq!(task.attempts, completed.attempts);
    assert_eq!(task.attempts[0].id, attempt.id);
    assert_eq!(task.attempts[0].state, AttemptState::Succeeded);
    assert_eq!(
        task.attempts[0].summary.as_deref(),
        Some("Done, still running")
    );
}

#[test]
fn a_worker_whose_root_exits_first_is_exited_only_once_its_detached_child_ends() {
    // A long grace, so only the root's own exit can end the detached child in time.
    let fixture = with_grace("600");
    let run = fixture.create_run().id;
    fixture.add_task(&run, "detach");
    let (attempt, agent) = dispatched(fixture.dispatch(
        &run,
        "detach",
        &fixture.worktrees[0],
        false,
        &["detach", "detach-go", "Left a child", "detach-pids"],
    ));
    fixture.release("detach-go");
    let pids = read_pids(&fixture.root().join("detach-pids"), 2);
    wait_for_exit_after(&fixture, &agent.id, &pids);
    for pid in &pids {
        assert!(!process_running(*pid), "{pid} survived");
    }
    let task = task(&fixture.show(&run), "detach").clone();
    assert_eq!(task.attempts[0].id, attempt.id);
    assert_eq!(task.attempts[0].state, AttemptState::Succeeded);
}

/// Polls until the agent is exited, and fails if it is ever exited while any of
/// `pids` runs or while its session's cgroup still holds a process.
fn wait_for_exit_with_empty_scopes(fixture: &Fixture, agent: &str, session: &str, pids: &[u32]) {
    poll("finished agent to exit", || {
        let exited = fixture
            .agents()
            .into_iter()
            .any(|listed| listed.id == agent && json!(listed.state) == "exited");
        if exited {
            let alive: Vec<_> = pids.iter().filter(|pid| process_running(**pid)).collect();
            assert!(alive.is_empty(), "agent exited while {alive:?} still ran");
            let scopes = populated_scopes(fixture.root(), session);
            assert!(
                scopes.is_empty(),
                "agent exited while {scopes:?} held processes"
            );
        }
        exited.then_some(())
    });
}

/// Starts the escape fixture and returns its attempt, agent, root PID and helper PID
/// once the helper has left the session, its environment and its parent.
fn dispatch_escape(fixture: &Fixture, run: &str, mode: &str) -> (Attempt, ManagedAgent, u32, u32) {
    fixture.add_task(run, mode);
    let go = format!("{mode}-go");
    let pids = format!("{mode}-pids");
    let (attempt, agent) = dispatched(fixture.dispatch(
        run,
        mode,
        &fixture.worktrees[0],
        false,
        &[mode, &go, "Left a helper", &pids],
    ));
    fixture.release(&go);
    let [root, helper] = read_pids(&fixture.root().join(pids), 2)[..] else {
        unreachable!()
    };
    // An "escape" worker exits at once, so its helper may already be ended here.
    let Ok(environment) = fs::read(format!("/proc/{helper}/environ")) else {
        return (attempt, agent, root, helper);
    };
    assert!(
        !environment
            .windows(14)
            .any(|window| window == b"POWER_SESSION="),
        "the helper kept POWER_SESSION"
    );
    let Ok(parent) = fs::read_to_string(format!("/proc/{helper}/stat")) else {
        return (attempt, agent, root, helper);
    };
    let parent = parent
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    assert_ne!(parent, root.to_string(), "the helper was not reparented");
    (attempt, agent, root, helper)
}

#[test]
fn a_detached_helper_with_a_clean_environment_ends_before_its_worker_is_exited() {
    // A long grace, so only the root's own exit ends the helper. Discovery by
    // ancestry, session or POWER_SESSION finds none of it, so only the cgroup does.
    let fixture = with_grace("600");
    let run = fixture.create_run().id;
    let (attempt, agent, root, helper) = dispatch_escape(&fixture, &run, "escape");
    let session = agent.session.clone().unwrap();
    wait_for_exit_with_empty_scopes(&fixture, &agent.id, &session, &[root, helper]);
    assert!(!process_running(helper), "{helper} survived");
    let task = task(&fixture.show(&run), "escape").clone();
    assert_eq!(task.attempts[0].id, attempt.id);
    assert_eq!(task.attempts[0].state, AttemptState::Succeeded);
}

/// The cgroup directories of the session's scopes.
fn scope_dirs(root: &Path, session: &str) -> Vec<PathBuf> {
    let prefix = format!("power-{session}-");
    fs::read_dir(slice_dir(root))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.path())
        .collect()
}

fn agent_state(fixture: &Fixture, agent: &str) -> Value {
    let listed = fixture.agents();
    json!(
        listed
            .iter()
            .find(|listed| listed.id == agent)
            .unwrap()
            .state
    )
}

#[test]
fn a_restart_during_a_teardown_keeps_the_record_until_the_scope_is_empty() {
    // Half a second of grace, then a teardown whose helper ignores SIGTERM, so the
    // teardown is still waiting out its grace when the daemon restarts.
    let mut fixture = with_grace("0.5");
    let run = fixture.create_run().id;
    let (_, agent, root, helper) = dispatch_escape(&fixture, &run, "stubborn");
    let session = agent.session.clone().unwrap();
    fixture.wait_for_state(&run, "stubborn", TaskState::Completed);
    thread::sleep(Duration::from_millis(900));
    assert!(process_running(helper), "the teardown ended early");

    fixture.restart_gracefully();

    wait_for_exit_with_empty_scopes(&fixture, &agent.id, &session, &[root, helper]);
    assert!(!process_running(helper), "{helper} survived the restart");
    let listed = fixture.command(&["session", "list"]);
    assert!(!String::from_utf8(listed.stdout).unwrap().contains(&session));
}

#[test]
fn a_failed_reclaim_keeps_the_agent_running_until_a_retry_ends_its_scope() {
    let mut fixture = with_grace("600");
    let run = fixture.create_run().id;
    let (_, agent, _, helper) = dispatch_escape(&fixture, &run, "escape-stay");
    let session = agent.session.clone().unwrap();
    fixture.wait_for_state(&run, "escape-stay", TaskState::Completed);
    let kills: Vec<_> = scope_dirs(fixture.root(), &session)
        .into_iter()
        .map(|scope| scope.join("cgroup.kill"))
        .collect();
    assert!(!kills.is_empty());
    for kill in &kills {
        fs::set_permissions(kill, fs::Permissions::from_mode(0o444)).unwrap();
    }

    // The old daemon dies without a teardown, and the new one cannot kill the scope.
    fixture.restart();
    thread::sleep(Duration::from_secs(2));
    assert!(process_running(helper));
    assert_eq!(agent_state(&fixture, &agent.id), "running");
    let listed = fixture.command(&["session", "list"]);
    assert!(
        !String::from_utf8(listed.stdout).unwrap().contains(&session),
        "a session whose processes could not be ended was relaunched"
    );

    for kill in &kills {
        fs::set_permissions(kill, fs::Permissions::from_mode(0o644)).unwrap();
    }
    wait_for_exit_with_empty_scopes(&fixture, &agent.id, &session, &[helper]);
}

#[test]
fn without_cgroups_a_detached_helper_ends_before_its_worker_is_exited() {
    let fixture = Fixture::with_configure_env(
        |root| write_config(root, "dag.end_finished_workers_after = 600\n"),
        &[("POWER_WORKER_CGROUPS", "0")],
    );
    let run = fixture.create_run().id;
    let (attempt, agent, root, helper) = dispatch_escape(&fixture, &run, "escape");
    let session = agent.session.clone().unwrap();
    assert!(
        populated_scopes(fixture.root(), &session).is_empty(),
        "the fallback used a scope"
    );
    wait_for_exit_after(&fixture, &agent.id, &[root, helper]);
    assert!(!process_running(helper), "{helper} survived");
    let task = task(&fixture.show(&run), "escape").clone();
    assert_eq!(task.attempts[0].id, attempt.id);
    assert_eq!(task.attempts[0].state, AttemptState::Succeeded);
}

#[test]
fn without_cgroups_a_restart_ends_what_a_finished_worker_left() {
    let mut fixture = Fixture::with_configure_env(
        |root| write_config(root, "dag.end_finished_workers_after = 600\n"),
        &[("POWER_WORKER_CGROUPS", "0")],
    );
    let run = fixture.create_run().id;
    let (_, agent, root, helper) = dispatch_escape(&fixture, &run, "escape-stay");
    fixture.wait_for_state(&run, "escape-stay", TaskState::Completed);
    fixture.restart();
    wait_for_exit_after(&fixture, &agent.id, &[root, helper]);
    assert!(!process_running(helper), "{helper} survived the restart");
}

#[test]
fn a_finished_worker_keeps_running_for_the_whole_grace() {
    let fixture = with_grace("1.5");
    let run = fixture.create_run().id;
    fixture.add_task(&run, "graced");
    let (_, agent) = dispatched(fixture.dispatch(
        &run,
        "graced",
        &fixture.worktrees[0],
        false,
        &["graced", "graced-go", "Done", "graced-marks"],
    ));
    fixture.release("graced-go");
    wait_for_exit_after(&fixture, &agent.id, &[]);
    let marks: Vec<u128> = fs::read_to_string(fixture.root().join("graced-marks"))
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect();
    let ran = Duration::from_nanos((marks.last().unwrap() - marks[0]) as u64);
    assert!(ran >= Duration::from_millis(1200), "ended after {ran:?}");
}

#[test]
fn a_restart_finishes_a_finished_workers_pending_teardown() {
    // The grace outlasts the test, so the teardown is still pending at the restart.
    let mut fixture = with_grace("600");
    let run = fixture.create_run().id;
    let (_, agent, root, helper) = dispatch_escape(&fixture, &run, "escape-stay");
    let session = agent.session.clone().unwrap();
    fixture.wait_for_state(&run, "escape-stay", TaskState::Completed);
    assert!(process_running(helper));
    assert_eq!(populated_scopes(fixture.root(), &session).len(), 1);

    // The old daemon's terminals close, but the helper ignores that.
    fixture.restart_gracefully();

    wait_for_exit_with_empty_scopes(&fixture, &agent.id, &session, &[root, helper]);
    assert!(!process_running(helper), "{helper} survived the restart");
    let listed = fixture.command(&["session", "list"]);
    assert_success(&listed);
    assert!(!String::from_utf8(listed.stdout).unwrap().contains(&session));
}

#[test]
fn a_restart_skips_finished_workers_and_resumes_active_ones_without_a_fresh_start() {
    // A coding-agent profile backed by a shell script. It logs each launch, reports
    // success when it finishes, and keeps running afterwards as agents do.
    let profile = |root: &Path, grace: &str| {
        format!(
            r#"
            dag.end_finished_workers_after = {grace}
            agents["fixture-resumable"] = {{
                command = {:?},
                injection = {{ kind = "flag", flag = "--instructions" }},
                resume = {{
                    assign = {{ "--id", "{{id}}" }},
                    resume = {{ "--resume", "{{id}}" }},
                    resume_last = {{ "--continue" }},
                    conflicts = {{ "--id", "--resume", "--continue" }},
                }},
            }}
            "#,
            root.join("resumable").to_str().unwrap()
        )
    };
    let mut fixture = Fixture::with_configure(|root| {
        let program = root.join("resumable");
        fs::write(
            &program,
            r#"#!/bin/sh
set -eu
fixture_dir="${POWER_SOCKET%/*}"
test "$1" = "--instructions"
shift 2
case "$1" in
    --id) mode=fresh ;;
    --resume) mode=resume ;;
    *) exit 2 ;;
esac
printf '%s %s\n' "$mode" "$POWER_TASK" >> "$fixture_dir/launches"
case "$mode $POWER_TASK" in
    "fresh finished") "$POWER_BIN" dag report success --summary "Finished before the restart" ;;
    "resume active") "$POWER_BIN" dag report success --summary "Finished after the resume" ;;
esac
exec sleep 600
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        // Finished workers keep running, so the finished one is saved at the restart.
        write_config(root, &profile(root, "false"));
    });
    let launches = fixture.root().join("launches");
    let launched = || {
        let mut lines: Vec<String> = fs::read_to_string(&launches)
            .unwrap_or_default()
            .lines()
            .map(Into::into)
            .collect();
        lines.sort();
        lines
    };
    let run = fixture.create_run().id;
    let mut agents = BTreeMap::new();
    for (index, id) in ["finished", "active"].into_iter().enumerate() {
        fixture.add_task(&run, id);
        let (_, agent) = dispatched(output(
            power(fixture.root())
                .args(["dag", "dispatch", &run, id])
                .args(["--provider", "fixture-resumable", "--worktree"])
                .arg(&fixture.worktrees[index]),
        ));
        agents.insert(id, agent);
    }
    fixture.wait_for_state(&run, "finished", TaskState::Completed);
    poll("both workers to start", || {
        (launched().len() == 2).then_some(())
    });
    let sessions = |fixture: &Fixture| {
        let listed = fixture.command(&["session", "list"]);
        assert_success(&listed);
        String::from_utf8(listed.stdout).unwrap()
    };
    let finished_session = agents["finished"].session.clone().unwrap();
    let active_session = agents["active"].session.clone().unwrap();
    assert!(sessions(&fixture).contains(&finished_session));

    // The second daemon ends finished workers half a second after they report,
    // well inside the window in which a failed resume starts a fresh conversation.
    write_config(fixture.root(), &profile(fixture.root(), "0.5"));
    fixture.restart();

    fixture.wait_for_state(&run, "active", TaskState::Completed);
    wait_for_exit_after(&fixture, &agents["active"].id, &[]);
    // Give a mistaken fresh start time to appear.
    thread::sleep(Duration::from_secs(1));
    assert_eq!(
        launched(),
        ["fresh active", "fresh finished", "resume active"],
        "the finished worker was relaunched or the resumed one restarted fresh"
    );
    let listed = sessions(&fixture);
    assert!(!listed.contains(&finished_session), "{listed}");
    assert!(!listed.contains(&active_session), "{listed}");
    let snapshot = fixture.show(&run);
    let finished = task(&snapshot, "finished");
    assert_eq!(finished.attempts.len(), 1);
    assert_eq!(finished.attempts[0].state, AttemptState::Succeeded);
    let active = task(&snapshot, "active");
    assert_eq!(active.attempts.len(), 1);
    assert_eq!(active.attempts[0].state, AttemptState::Succeeded);
    assert_eq!(
        active.attempts[0].summary.as_deref(),
        Some("Finished after the resume")
    );
    assert!(
        fixture.agents().iter().any(|listed| {
            listed.id == agents["finished"].id && json!(listed.state) == "exited"
        })
    );
}

#[test]
fn dispatch_rejects_a_worktree_from_another_repository() {
    let fixture = Fixture::new();
    let foreign = fixture.root().join("foreign-repo");
    init_repo(&foreign);
    let run = fixture.create_run().id;
    fixture.add_task(&run, "local-only");
    assert_rejected(&fixture.dispatch(
        &run,
        "local-only",
        &foreign,
        false,
        &["success", "foreign-go", "Wrong repository"],
    ));
    let snapshot = fixture.show(&run);
    let task = task(&snapshot, "local-only");
    assert_eq!(task.state, TaskState::Ready);
    assert!(task.attempts.is_empty());
    assert!(fixture.agents().is_empty());
}

#[test]
fn wait_blocks_for_matching_result_and_advances_cursor() {
    let fixture = Fixture::new();
    let run = fixture.create_run().id;
    fixture.add_task(&run, "result");
    let baseline = fixture.events(&run, 0, &[]).cursor;
    let (attempt, _) = dispatched(fixture.dispatch(
        &run,
        "result",
        &fixture.worktrees[0],
        false,
        &["success", "result-go", "Waited result"],
    ));
    fixture.wait_for_worker("result-go");
    let pending = fixture.events(&run, baseline, &[]);
    assert!(
        pending.cursor > baseline,
        "Expected an attempt_started event"
    );
    let filtered = fixture.events(&run, baseline, &["attempt_succeeded"]);
    assert!(filtered.events.is_empty());
    assert_eq!(filtered.cursor, baseline);

    let mut waiter = power(fixture.root())
        .args(["dag", "wait", &run, "--since"])
        .arg(baseline.to_string())
        .args(["--timeout", "10", "--kind", "attempt_succeeded"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if child_finished(&mut waiter, Duration::from_millis(300)) {
        let output = waiter.wait_with_output().unwrap();
        panic!("Wait returned before the worker could report: {output:?}");
    }
    fixture.release("result-go");
    if !child_finished(&mut waiter, TIMEOUT) {
        let _ = waiter.kill();
        let output = waiter.wait_with_output().unwrap();
        panic!("Wait did not return after the worker reported: {output:?}");
    }
    let page = EventPage::parse(json_output(waiter.wait_with_output().unwrap()), baseline);
    assert_eq!(page.events.len(), 1);
    let result = &page.events[0];
    assert_eq!(result.kind, RunEventKind::AttemptSucceeded);
    assert_eq!(result.run, run);
    assert_eq!(result.box_id, BOX_ID);
    assert_eq!(result.task.as_deref(), Some("result"));
    assert_eq!(result.attempt.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(result.summary.as_deref(), Some("Waited result"));
    assert!(page.cursor > pending.cursor);

    let fetched = fixture.events(&run, baseline, &["attempt_succeeded"]);
    assert_eq!(fetched.events, page.events);
    assert_eq!(fetched.cursor, page.cursor);
    let next = fixture.events(&run, page.cursor, &["attempt_succeeded"]);
    assert!(next.events.is_empty());
    assert_eq!(next.cursor, page.cursor);
}

#[test]
fn daemon_restart_preserves_graph_and_reconciles_active_attempt() {
    let mut fixture = Fixture::new();
    let run = fixture.create_run().id;
    json_output(fixture.apply(
        &run,
        json!([
            {"id": "running", "title": "Running", "instructions": "Leave a durable result"},
            {"id": "next", "title": "Next", "deps": ["running"]}
        ]),
    ));
    let (first, agent) = dispatched(fixture.dispatch(
        &run,
        "running",
        &fixture.worktrees[0],
        false,
        &["success", "never-release", "Old result"],
    ));
    fixture.wait_for_worker("never-release");
    let before = fixture.show(&run);
    assert_eq!(task(&before, "running").state, TaskState::Working);
    assert_eq!(task(&before, "next").state, TaskState::Waiting);
    let saved_events = fixture.events(&run, 0, &[]);

    fixture.restart();

    let after = fixture.show(&run);
    assert_eq!(after.id, before.id);
    assert_eq!(after.box_id, before.box_id);
    assert_eq!(after.title, before.title);
    assert_eq!(after.coordinator, before.coordinator);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.tasks.len(), before.tasks.len());
    assert_eq!(task(&after, "next"), task(&before, "next"));
    let interrupted = task(&after, "running");
    assert_eq!(interrupted.title, "Running");
    assert_eq!(interrupted.instructions, "Leave a durable result");
    assert_eq!(interrupted.state, TaskState::Interrupted);
    assert_eq!(
        interrupted.current_attempt.as_deref(),
        Some(first.id.as_str())
    );
    assert_eq!(interrupted.attempts.len(), 1);
    assert_eq!(interrupted.attempts[0].state, AttemptState::Interrupted);
    assert_eq!(
        interrupted.attempts[0].detail.as_deref(),
        Some("daemon restarted; session unavailable"),
    );
    assert!(
        fixture
            .agents()
            .iter()
            .any(|listed| { listed.id == agent.id && json!(listed.state) == "exited" })
    );

    let history = fixture.events(&run, 0, &[]);
    assert!(history.events.starts_with(&saved_events.events));
    let reconciled = fixture.events(&run, saved_events.cursor, &["attempt_interrupted"]);
    assert_eq!(reconciled.events.len(), 1);
    assert_eq!(
        reconciled.events[0].attempt.as_deref(),
        Some(first.id.as_str())
    );

    let worker = ["success", "restarted-go", "Recovered result"];
    assert_rejected(&fixture.dispatch(&run, "running", &fixture.worktrees[1], false, &worker));
    let (retried, _) =
        dispatched(fixture.dispatch(&run, "running", &fixture.worktrees[1], true, &worker));
    assert_eq!(retried.number, 2);
    assert_ne!(first.id, retried.id);
    fixture.release("restarted-go");
    fixture.wait_for_state(&run, "running", TaskState::Completed);
    assert_eq!(ready_ids(&fixture, &run), ["next"]);
}

#[test]
fn worker_budget_queues_past_max_active_workers_and_starts_on_report() {
    let fixture = Fixture::with_configure(|root| {
        write_config(root, "dag.max_active_workers = 2\n");
    });
    let run = fixture.create_run();
    for id in ["one", "two", "three"] {
        fixture.add_task(&run.id, id);
    }
    let worktree = &fixture.worktrees[0];
    let dispatch = |id: &str| {
        json_output(fixture.dispatch(
            &run.id,
            id,
            worktree,
            false,
            &["success", &format!("{id}.go"), &format!("{id} done")],
        ))
    };
    for id in ["one", "two"] {
        let started = dispatch(id);
        assert!(started["attempt"]["id"].is_string(), "{started}");
        fixture.wait_for_worker(&format!("{id}.go"));
    }
    let queued = dispatch("three");
    assert_eq!(queued["queued"], true, "{queued}");
    assert_eq!(queued["task"], "three", "{queued}");
    assert!(
        queued["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("2 of 2 workers")),
        "{queued}"
    );
    let three = fixture.wait_for_state(&run.id, "three", TaskState::Queued);
    assert!(three.attempts.is_empty());
    assert!(three.queued_at.is_some());

    let ready = fixture.json(&["dag", "ready", &run.id]);
    assert_eq!(ready["queued"], json!(["three"]), "{ready}");
    assert!(
        ready["budget"]
            .as_str()
            .is_some_and(|note| note.contains("dag.max_active_workers")),
        "{ready}"
    );
    let again = fixture.dispatch(&run.id, "three", worktree, false, &["success", "x", "x"]);
    assert_rejected(&again);

    // The third worker starts once one of the first two reports.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        fixture.show(&run.id).tasks[2].state,
        TaskState::Queued,
        "The queued task started while the budget was full"
    );
    fixture.release("one.go");
    fixture.wait_for_state(&run.id, "one", TaskState::Completed);
    fixture.wait_for_state(&run.id, "three", TaskState::Working);
    fixture.wait_for_worker("three.go");
    let kinds: Vec<_> = fixture
        .events(&run.id, 0, &["task_queued"])
        .events
        .into_iter()
        .map(|event| (event.kind, event.task))
        .collect();
    assert_eq!(kinds, [(RunEventKind::TaskQueued, Some("three".into()))]);

    for id in ["two", "three"] {
        fixture.release(&format!("{id}.go"));
        fixture.wait_for_state(&run.id, id, TaskState::Completed);
    }
}

#[test]
fn a_persisted_queued_worker_that_reports_at_once_starts_after_the_socket_listens() {
    let mut fixture = Fixture::with_configure(|root| {
        write_config(root, "dag.max_active_workers = 1\n");
    });
    let run = fixture.create_run();
    fixture.add_task(&run.id, "held");
    fixture.add_task(&run.id, "queued");
    let worktree = &fixture.worktrees[0];
    dispatched(fixture.dispatch(
        &run.id,
        "held",
        worktree,
        false,
        &["success", "held.go", "x"],
    ));
    fixture.wait_for_worker("held.go");
    // The queued worker reports as soon as it starts.
    fixture.release("queued.go");
    let queued = json_output(fixture.dispatch(
        &run.id,
        "queued",
        worktree,
        false,
        &["success", "queued.go", "Reported at once"],
    ));
    assert_eq!(queued["queued"], true, "{queued}");

    // The restart interrupts the held worker, which frees the budget for the queued one.
    // The bind waits longer than a report retries for, so a worker started before the
    // socket listens could not report.
    fixture.restart_with(&[("POWER_TEST_BIND_DELAY_MS", "4000")]);
    let task = fixture.wait_for_state(&run.id, "queued", TaskState::Completed);
    assert_eq!(task.attempts.len(), 1);
    assert_eq!(
        task.attempts[0].summary.as_deref(),
        Some("Reported at once")
    );
    assert_eq!(fixture.show(&run.id).tasks[0].state, TaskState::Interrupted);
}

#[test]
fn an_incomplete_teardown_is_retried_by_the_live_daemon_once_the_fault_clears() {
    // A short grace, then a teardown whose helper ignores SIGTERM and whose
    // cgroup.kill fails, so the teardown ends incomplete without any restart.
    let fixture = with_grace("1");
    let run = fixture.create_run().id;
    let (_, agent, _, helper) = dispatch_escape(&fixture, &run, "stubborn");
    let session = agent.session.clone().unwrap();
    let kills: Vec<_> = scope_dirs(fixture.root(), &session)
        .into_iter()
        .map(|scope| scope.join("cgroup.kill"))
        .collect();
    assert!(!kills.is_empty());
    for kill in &kills {
        fs::set_permissions(kill, fs::Permissions::from_mode(0o444)).unwrap();
    }
    fixture.wait_for_state(&run, "stubborn", TaskState::Completed);
    // The grace, the end grace and the failing kills pass.
    thread::sleep(Duration::from_secs(6));
    assert!(
        process_running(helper),
        "the helper ended without cgroup.kill"
    );
    assert_eq!(agent_state(&fixture, &agent.id), "running");

    for kill in &kills {
        fs::set_permissions(kill, fs::Permissions::from_mode(0o644)).unwrap();
    }
    wait_for_exit_with_empty_scopes(&fixture, &agent.id, &session, &[helper]);
    assert!(!process_running(helper), "{helper} survived the retry");
}

#[test]
fn a_restart_without_cgroups_still_ends_what_a_scoped_worker_left() {
    let mut fixture = with_grace("600");
    let run = fixture.create_run().id;
    let (_, agent, root, helper) = dispatch_escape(&fixture, &run, "escape-stay");
    let session = agent.session.clone().unwrap();
    fixture.wait_for_state(&run, "escape-stay", TaskState::Completed);
    assert_eq!(populated_scopes(fixture.root(), &session).len(), 1);

    fixture.restart_with(&[("POWER_WORKER_CGROUPS", "0")]);

    wait_for_exit_with_empty_scopes(&fixture, &agent.id, &session, &[root, helper]);
    assert!(!process_running(helper), "{helper} survived the restart");
}

#[test]
fn a_restart_with_cgroups_still_ends_what_a_fallback_worker_left() {
    let mut fixture = Fixture::with_configure_env(
        |root| write_config(root, "dag.end_finished_workers_after = 600\n"),
        &[("POWER_WORKER_CGROUPS", "0")],
    );
    let run = fixture.create_run().id;
    let (_, agent, root, helper) = dispatch_escape(&fixture, &run, "escape-stay");
    fixture.wait_for_state(&run, "escape-stay", TaskState::Completed);

    fixture.restart_with(&[("POWER_WORKER_CGROUPS", "1")]);

    wait_for_exit_after(&fixture, &agent.id, &[root, helper]);
    assert!(!process_running(helper), "{helper} survived the restart");
}

#[test]
fn without_cgroups_a_stubborn_detached_helper_ends_before_its_worker_is_exited() {
    // The helper ignores SIGTERM and has neither POWER_SESSION nor its parent, so
    // only the reaper that adopted it can end it.
    let fixture = Fixture::with_configure_env(
        |root| write_config(root, "dag.end_finished_workers_after = 0.5\n"),
        &[("POWER_WORKER_CGROUPS", "0")],
    );
    let run = fixture.create_run().id;
    let (_, agent, root, helper) = dispatch_escape(&fixture, &run, "stubborn");
    wait_for_exit_after(&fixture, &agent.id, &[root, helper]);
    assert!(!process_running(helper), "{helper} survived");
}

#[test]
fn without_cgroups_a_missing_program_fails_the_dispatch() {
    let fixture = Fixture::with_configure_env(|_| {}, &[("POWER_WORKER_CGROUPS", "0")]);
    let run = fixture.create_run().id;
    fixture.add_task(&run, "missing");
    let output = output(
        power(fixture.root())
            .args([
                "dag",
                "dispatch",
                &run,
                "missing",
                "--provider",
                "command",
                "--worktree",
            ])
            .arg(&fixture.worktrees[0])
            .args(["--", "power-test-no-such-program"]),
    );
    assert!(!output.status.success(), "a missing program was dispatched");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("doesn't exist"), "{stderr}");
}
