use infrastructure::boxes::{Layout, SavedBox, SavedView};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const BOX: &str = "e01a1101-4373-44f3-9356-eb412bc56904";
const OTHER_BOX: &str = "a257516f-64dd-4062-9314-7ef9f8c617c9";
const TIMEOUT: Duration = Duration::from_secs(15);
const REPORT_SUCCESS: &str = r#"exec "$POWER_BIN" dag report success --summary done"#;
const REPORT_AFTER_RELEASE: &str = r#"
    until test -f "${POWER_SOCKET%/*}/release"; do sleep 0.025; done
    pwd -P > "${POWER_SOCKET%/*}/$POWER_TASK.cwd"
    exec "$POWER_BIN" dag report success --summary done
"#;

struct Fixture {
    daemon: Child,
    temp: tempfile::TempDir,
    repo: PathBuf,
    layout: Layout,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let repo = root.join("repo");
        init_repo(&repo);
        let mut fixture = Self {
            daemon: power(root)
                .arg("daemon")
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
            temp,
            layout: Layout {
                boxes: vec![saved_box(&repo, BOX, 7)],
                active_box: Some(BOX.into()),
                ..Layout::default()
            },
            repo,
        };
        fixture.save_layout();
        let socket = fixture.temp.path().join("power.sock");
        wait_for("daemon socket", || {
            assert!(
                fixture.daemon.try_wait().unwrap().is_none(),
                "Daemon exited"
            );
            UnixStream::connect(&socket).is_ok().then_some(())
        });
        fixture
    }

    fn command(&self) -> Command {
        power(self.temp.path())
    }

    fn save_layout(&self) {
        let state = self.temp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("boxes.json"),
            serde_json::to_vec(&self.layout).unwrap(),
        )
        .unwrap();
    }

    fn start_view(&mut self, worktree: &Path) -> String {
        let session = run(self
            .command()
            .args(["session", "start", "shell"])
            .arg(worktree))
        .trim()
        .to_owned();
        self.layout.boxes[0].views.push(SavedView {
            session: session.clone(),
            worktree: worktree.to_owned(),
        });
        self.save_layout();
        session
    }

    fn end_session(&self, session: &str) {
        run(self.command().args(["session", "end", session]));
    }

    fn external_run(&self, box_selector: &str) -> String {
        self.json(&[
            "dag",
            "create",
            "--title",
            "Fixture",
            "--box",
            box_selector,
            "--coordinator",
            "outside",
        ])["run"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn add_task(&self, run: &str, task: &str, after: &[&str]) {
        let mut command = self.command();
        command.args(["dag", "add-task", run, "--id", task, "--title", task]);
        for dep in after {
            command.args(["--after", dep]);
        }
        parse(&mut command);
    }

    fn worker(&self, action: &str, run: &str, task: &str) -> Command {
        let mut command = self.command();
        command.args(["dag", action, run, task, "--provider", "command"]);
        command
    }

    fn spawn(&self, run: &str, task: &str, branch: &str, script: &str) -> Value {
        parse(
            self.worker("dispatch", run, task)
                .args(["--worktree", "new", "--branch", branch, "--base", "main"])
                .args(["--", "/bin/sh", "-ec", script]),
        )
    }

    fn json(&self, args: &[&str]) -> Value {
        parse(self.command().args(args))
    }

    fn wait_for_state(&self, run: &str, id: &str, state: &str) {
        wait_for(&format!("{id} to become {state}"), || {
            let shown = self.json(&["dag", "show", run]);
            shown["run"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|task| task["id"] == id && task["state"] == state)
                .then_some(())
        });
    }

    fn default_root(&self) -> PathBuf {
        self.temp.path().join("home/.power")
    }

    fn request(&self, request: Value) -> Value {
        let mut socket = UnixStream::connect(self.temp.path().join("power.sock")).unwrap();
        socket.set_read_timeout(Some(TIMEOUT)).unwrap();
        writeln!(socket, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn power(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_power"));
    command
        .current_dir(root)
        .env("POWER_SOCKET", root.join("power.sock"))
        .env("POWER_STATE_DIR", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root.join("home"))
        .env("SHELL", "/bin/sh")
        .env_remove("POWER_SESSION")
        .env_remove("POWER_BOX")
        .env_remove("POWER_RUN")
        .env_remove("POWER_TASK")
        .env_remove("POWER_ATTEMPT")
        .env_remove("POWER_AGENT")
        .env_remove("POWER_BIN")
        .env_remove("POWER_PLUGIN_DIR");
    isolated_git(&mut command);
    command
}

fn isolated_git(command: &mut Command) {
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR");
}

#[track_caller]
fn run(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[track_caller]
fn parse(command: &mut Command) -> Value {
    serde_json::from_str(&run(command)).unwrap()
}

#[track_caller]
fn rejected(output: Output, expected: &str) -> String {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains(expected), "{error}");
    error
}

fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for {what}");
        thread::sleep(Duration::from_millis(25));
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    isolated_git(&mut command);
    run(command.current_dir(repo).args(args)).trim().into()
}

fn init_repo(repo: &Path) {
    fs::create_dir(repo).unwrap();
    git(repo, &["init", "-q", "--initial-branch=main"]);
    git(repo, &["config", "user.name", "Power Worktree Test"]);
    git(
        repo,
        &["config", "user.email", "power-worktrees@example.invalid"],
    );
    git(
        repo,
        &["commit", "-q", "--allow-empty", "-m", "Initial commit"],
    );
}

fn saved_box(repo: &Path, id: &str, shortcut: u8) -> SavedBox {
    SavedBox {
        id: id.into(),
        repository: repo.join(".git"),
        checkout: repo.to_owned(),
        shortcut: Some(shortcut),
        views: Vec::new(),
        active_view: None,
        previous_view: None,
    }
}

fn checkout(dispatch: &Value, branch: &str) -> PathBuf {
    let path = dispatch["worktree"].as_str().unwrap();
    assert_eq!(dispatch["branch"], branch);
    assert_eq!(dispatch["attempt"]["worktree"], path);
    assert_eq!(dispatch["agent"]["worktree"], path);
    let path = PathBuf::from(path);
    assert_eq!(git(&path, &["branch", "--show-current"]), branch);
    path
}

#[test]
fn dispatches_from_a_view_and_an_explicit_base_and_records_creators() {
    let mut fixture = Fixture::new();
    let coordinator = fixture.temp.path().join("coordinator");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-b",
            "coordinator",
            coordinator.to_str().unwrap(),
        ],
    );
    git(
        &coordinator,
        &["commit", "-q", "--allow-empty", "-m", "Coordinator commit"],
    );
    let session = fixture.start_view(&coordinator);
    let current_head = git(&coordinator, &["rev-parse", "HEAD"]);
    let in_view = || {
        let mut command = fixture.command();
        command.env("POWER_SESSION", &session);
        command
    };
    let created = parse(in_view().args(["dag", "create", "--title", "Managed run"]));
    let run = created["run"]["id"].as_str().unwrap();
    fixture.add_task(run, "one", &[]);
    fixture.add_task(run, "two", &[]);
    fixture.add_task(run, "dependent", &["one", "two"]);

    let mut attempts = Vec::new();
    for task in ["one", "two"] {
        let branch = format!("worker/{task}");
        let result = parse(in_view().args([
            "dag",
            "dispatch",
            run,
            task,
            "--provider",
            "command",
            "--worktree",
            "new",
            "--branch",
            &branch,
            "--",
            "/bin/sh",
            "-ec",
            REPORT_AFTER_RELEASE,
        ]));
        let path = checkout(&result, &branch);
        assert_eq!(
            path,
            fixture.default_root().join(format!("repo/worker-{task}"))
        );
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), current_head);
        fixture.wait_for_state(run, task, "working");
        attempts.push((task, branch, path, result["attempt"]["id"].clone()));
    }
    let git_worktrees = git(&fixture.repo, &["worktree", "list", "--porcelain"]);
    for (task, branch, path, _) in &attempts {
        let checkout = format!("worktree {}", path.display());
        let branch = format!("branch refs/heads/{branch}");
        assert!(git_worktrees.split("\n\n").any(|entry| {
            entry.lines().any(|line| line == checkout) && entry.lines().any(|line| line == branch)
        }));
        fixture.wait_for_state(run, task, "working");
    }
    fs::write(fixture.temp.path().join("release"), []).unwrap();
    for (task, _, path, _) in &attempts {
        fixture.wait_for_state(run, task, "completed");
        let cwd = fs::read_to_string(fixture.temp.path().join(format!("{task}.cwd"))).unwrap();
        assert_eq!(Path::new(cwd.trim_end()), path);
    }

    let first_path = &attempts[0].2;
    git(
        first_path,
        &["commit", "-q", "--allow-empty", "-m", "Prerequisite"],
    );
    let dependent = parse(fixture.worker("dispatch", run, "dependent").args([
        "--worktree",
        "new",
        "--branch",
        "worker/dependent",
        "--base",
        "worker/one",
        "--",
        "/bin/sh",
        "-ec",
        REPORT_SUCCESS,
    ]));
    let dependent = checkout(&dependent, "worker/dependent");
    let first_head = git(first_path, &["rev-parse", "HEAD"]);
    assert_eq!(git(&dependent, &["rev-parse", "HEAD"]), first_head);
    fixture.wait_for_state(run, "dependent", "completed");

    let reply = fixture.request(json!({
        "request": "dag",
        "dag": {
            "op": "create_view_worktree", "box_sel": BOX, "from_session": session, "branch": "tab/work",
        }
    }));
    assert_eq!(reply["dag"]["kind"], "view_worktree_created", "{reply}");
    let view_path = Path::new(reply["dag"]["path"].as_str().unwrap());
    assert_eq!(git(view_path, &["rev-parse", "HEAD"]), current_head);
    assert_eq!(git(view_path, &["branch", "--show-current"]), "tab/work");

    let listed = parse(in_view().args(["worktree", "list"]));
    let records = listed["worktrees"].as_array().unwrap();
    assert_eq!(records.len(), 4);
    for (task, branch, path, attempt) in &attempts {
        let record = records
            .iter()
            .find(|record| record["branch"] == *branch)
            .unwrap();
        assert_eq!(record["path"], json!(path));
        assert_eq!(record["box_id"], BOX);
        assert_eq!(record["repository"], json!(fixture.repo.join(".git")));
        assert_eq!(record["base_commit"], current_head);
        assert!(record["created_at"].as_u64().unwrap() > 0);
        assert_eq!(
            record["creator"],
            json!({
                "kind": "attempt", "run": run, "task": task, "attempt": attempt,
            })
        );
    }
    let view = records
        .iter()
        .find(|record| record["branch"] == "tab/work")
        .unwrap();
    assert_eq!(
        view["creator"],
        json!({"kind": "view", "box_id": BOX, "session": session})
    );
    fixture.end_session(&session);
}

#[test]
fn checks_creation_inputs_and_reads_config_again_for_the_next_dispatch() {
    let fixture = Fixture::new();
    let run_id = fixture.external_run("7");
    for task in ["first", "next", "after-reset", "duplicate"] {
        fixture.add_task(&run_id, task, &[]);
    }
    let first = fixture.spawn(&run_id, "first", "taken", REPORT_SUCCESS);
    let first = checkout(&first, "taken");
    fixture.wait_for_state(&run_id, "first", "completed");
    let dispatch = || fixture.worker("dispatch", &run_id, "duplicate");
    rejected(
        dispatch()
            .args(["--worktree", "new", "--base", "main"])
            .output()
            .unwrap(),
        "--branch",
    );
    rejected(
        dispatch()
            .args(["--worktree", "new", "--branch", "missing-base"])
            .output()
            .unwrap(),
        "--base",
    );
    let marker = fixture.temp.path().join("worker-launched");
    let error = rejected(
        dispatch()
            .args(["--worktree", "new", "--branch", "taken", "--base", "main"])
            .args(["--", "/usr/bin/touch"])
            .arg(&marker)
            .output()
            .unwrap(),
        "Branch \"taken\" already exists",
    );
    assert!(error.contains(first.to_str().unwrap()));
    assert!(!marker.exists());
    fixture.wait_for_state(&run_id, "duplicate", "ready");

    let config = fixture
        .temp
        .path()
        .join("config/power-overwhelming/config.lua");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let custom = fixture.temp.path().join("custom");
    fs::create_dir_all(custom.join("repo/worker-next")).unwrap();
    fs::write(
        &config,
        format!("worktrees.root = {:?}\n", custom.to_str().unwrap()),
    )
    .unwrap();
    let next = fixture.spawn(&run_id, "next", "worker/next", REPORT_SUCCESS);
    assert_eq!(
        checkout(&next, "worker/next"),
        custom.join("repo/worker-next-2")
    );
    fixture.wait_for_state(&run_id, "next", "completed");
    assert!(first.is_dir());

    fs::remove_file(&config).unwrap();
    fs::remove_dir_all(fixture.temp.path().join("home/.power")).unwrap();
    let result = fixture.spawn(&run_id, "after-reset", "restored", REPORT_SUCCESS);
    assert_eq!(
        checkout(&result, "restored"),
        fixture.default_root().join("repo/restored")
    );
    fixture.wait_for_state(&run_id, "after-reset", "completed");
    assert!(custom.join("repo/worker-next-2").is_dir());
}

#[test]
fn retry_reuses_partial_work_and_can_create_a_fresh_branch() {
    let fixture = Fixture::new();
    let run = fixture.external_run("7");
    fixture.add_task(&run, "retry-me", &[]);
    let first = fixture.spawn(
        &run,
        "retry-me",
        "retry/first",
        "echo partial > progress; exit 2",
    );
    let first = checkout(&first, "retry/first");
    fixture.wait_for_state(&run, "retry-me", "interrupted");
    let resumed = parse(fixture.worker("retry", &run, "retry-me").args([
        "--",
        "/bin/sh",
        "-ec",
        "test \"$(cat progress)\" = partial; echo resumed >> progress",
    ]));
    assert_eq!(checkout(&resumed, "retry/first"), first);
    assert_eq!(resumed["attempt"]["number"], 2);
    fixture.wait_for_state(&run, "retry-me", "interrupted");
    assert_eq!(
        fs::read_to_string(first.join("progress")).unwrap(),
        "partial\nresumed\n"
    );

    let fresh = parse(fixture.worker("retry", &run, "retry-me").args([
        "--worktree",
        "new",
        "--branch",
        "retry/fresh",
        "--base",
        "main",
        "--",
        "/bin/sh",
        "-ec",
        "test ! -e progress; exec \"$POWER_BIN\" dag report success --summary fresh",
    ]));
    assert_ne!(checkout(&fresh, "retry/fresh"), first);
    assert_eq!(fresh["attempt"]["number"], 3);
    fixture.wait_for_state(&run, "retry-me", "completed");
    let listed = fixture.json(&["worktree", "list", "--all"]);
    assert_eq!(listed["worktrees"].as_array().unwrap().len(), 2);
    assert!(first.join("progress").exists());
}

#[test]
fn prune_reports_kept_reasons_and_only_removes_managed_worktrees_in_scope() {
    let mut fixture = Fixture::new();
    let other_repo = fixture.temp.path().join("other");
    init_repo(&other_repo);
    fixture
        .layout
        .boxes
        .push(saved_box(&other_repo, OTHER_BOX, 8));
    fixture.save_layout();
    let run_id = fixture.external_run("7");
    let other_run = fixture.external_run("8");
    for task in ["merged", "unmerged", "dirty", "active"] {
        fixture.add_task(&run_id, task, &[]);
    }
    fixture.add_task(&other_run, "outside-scope", &[]);
    let work = |task, script| checkout(&fixture.spawn(&run_id, task, task, script), task);
    let merged = work("merged", REPORT_SUCCESS);
    let unmerged = work("unmerged", REPORT_SUCCESS);
    let dirty = work("dirty", REPORT_SUCCESS);
    let active = fixture.spawn(&run_id, "active", "active", "exec sleep 60");
    let active_path = checkout(&active, "active");
    let outside = checkout(
        &fixture.spawn(&other_run, "outside-scope", "outside-scope", REPORT_SUCCESS),
        "outside-scope",
    );
    for task in ["merged", "unmerged", "dirty"] {
        fixture.wait_for_state(&run_id, task, "completed");
    }
    fixture.wait_for_state(&other_run, "outside-scope", "completed");
    git(
        &merged,
        &["commit", "-q", "--allow-empty", "-m", "Merged work"],
    );
    git(&fixture.repo, &["merge", "--ff-only", "merged"]);
    git(
        &unmerged,
        &["commit", "-q", "--allow-empty", "-m", "Unmerged work"],
    );
    fs::write(dirty.join("untracked"), b"keep me").unwrap();
    let unmanaged = fixture.temp.path().join("unmanaged");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-b",
            "manual",
            unmanaged.to_str().unwrap(),
        ],
    );

    let listed = fixture.json(&["worktree", "list", "--all"]);
    assert_eq!(listed["worktrees"].as_array().unwrap().len(), 5);
    let plan = run(fixture
        .command()
        .args(["worktree", "prune", "--box", "repo", "--dry-run"]));
    assert!(
        plan.contains(&format!("Would remove {} (merged)", merged.display())),
        "{plan}"
    );
    assert!(
        plan.contains("branch has not merged into the default branch"),
        "{plan}"
    );
    assert!(plan.contains("uncommitted or untracked changes"), "{plan}");
    assert!(
        plan.contains("in use by a live session or active attempt"),
        "{plan}"
    );
    assert!(merged.is_dir());
    let removed = run(fixture.command().args(["worktree", "prune", "--box", "7"]));
    assert_eq!(removed, plan.replace("Would remove", "Removed"));
    assert!(!merged.exists());
    assert_eq!(
        git(&fixture.repo, &["branch", "--list", "merged"]),
        "merged"
    );
    for path in [&unmerged, &dirty, &active_path, &unmanaged, &outside] {
        assert!(path.is_dir(), "Removed {}", path.display());
    }

    let listed = fixture.json(&["worktree", "list", "--box", "7"]);
    assert_eq!(listed["worktrees"].as_array().unwrap().len(), 3);
    let removed = run(fixture
        .command()
        .args(["worktree", "prune", "--all", "--delete-branch"]));
    assert!(removed.contains(&format!("Removed {} (outside-scope)", outside.display())));
    assert!(!outside.exists());
    assert!(git(&other_repo, &["branch", "--list", "outside-scope"]).is_empty());
    fixture.end_session(active["agent"]["session"].as_str().unwrap());
}
