use infrastructure::{
    boxes::{Layout, SavedBox},
    client::protocol::{
        Attempt, AttemptState, BoxAttention, BoxInfo, Coordinator, ManagedAgent, Provider, Run,
        RunEvent, RunEventKind, Task, TaskState,
    },
};
use serde_json::{Value, json};
use std::{
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

struct Daemon(Child);

impl Daemon {
    fn start(root: &Path) -> Self {
        let mut daemon = Self(
            power(root)
                .arg("daemon")
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
    directory: tempfile::TempDir,
    checkout: PathBuf,
    worktrees: [PathBuf; 2],
}

impl Fixture {
    fn new() -> Self {
        Self::with_configure(|_| {})
    }

    fn with_configure(configure: impl FnOnce(&Path)) -> Self {
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
        configure(root);
        let daemon = Daemon::start(root);
        Self {
            daemon,
            directory,
            checkout,
            worktrees,
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn restart(&mut self) {
        self.daemon.stop();
        self.daemon = Daemon::start(self.root());
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
        serde_json::from_value(self.json(&["agent", "list", "--box", BOX_ID])["agents"].take())
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
fn legacy_agent_aliases_use_the_configured_profiles() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let config = root.join("config/power-overwhelming");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.lua"),
        r#"
        agents.claude = {
            command = "/bin/echo",
            injection = { kind = "none" },
            model_callback = { kind = "none" },
            resume = { assign = {} },
        }
        agents.codex = { command = "/bin/echo", injection = { kind = "none" } }
        "#,
    )
    .unwrap();
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
        let config = root.join("config/power-overwhelming");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.lua"),
            format!(
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
        )
        .unwrap();
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
        error.contains("Known profiles: claude, codex, grok, fixture-agent"),
        "{error}"
    );
    assert!(task(&fixture.show(&run.id), "profile").attempts.is_empty());

    let (attempt, agent) = dispatched(launch("fixture-agent"));
    assert_eq!(agent.provider, Provider::Agent("fixture-agent".into()));
    assert_eq!(attempt.provider, agent.provider);
    fixture.release("profile-go");
    let task = fixture.wait_for_state(&run.id, "profile", TaskState::Completed);
    assert_eq!(task.attempts[0].summary.as_deref(), Some("Profile result"));

    fs::remove_file(fixture.root().join("config/power-overwhelming/config.lua")).unwrap();
    fixture.restart();
    let persisted = fixture.show(&run.id);
    assert_eq!(persisted.tasks[0].attempts[0].provider, agent.provider);
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
        let config = root.join("config/power-overwhelming");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.lua"),
            format!(
                r#"agents.codex = {{ command = {:?}, injection = {{ kind = "none" }} }}"#,
                program.to_str().unwrap(),
            ),
        )
        .unwrap();
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
        &["success", "alpha-go", "Alpha result"],
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
