use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

struct Daemon(Child, String);

/// Gives the daemon `command` starts a systemd slice of its own for its worker
/// scopes, and returns the slice's name, which the caller stops when done.
fn own_slice(command: &mut Command) -> String {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let slice = format!(
        "app-powertest{}x{}.slice",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    command.env("POWER_WORKER_SLICE", &slice);
    slice
}

/// Stops a test slice, which ends anything a failed test left in it.
fn stop_slice(slice: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", slice])
        .stderr(std::process::Stdio::null())
        .status();
}

impl Daemon {
    fn start(root: &Path) -> (Self, PathBuf) {
        Self::start_with(
            root,
            Command::new(env!("CARGO_BIN_EXE_power")).env("SHELL", "/bin/sh"),
        )
    }

    fn start_with(root: &Path, command: &mut Command) -> (Self, PathBuf) {
        let socket = root.join("power.sock");
        let slice = own_slice(command);
        let daemon = Self(
            command
                .arg("daemon")
                .env("POWER_SOCKET", &socket)
                .env("POWER_STATE_DIR", root.join("state"))
                .env("XDG_RUNTIME_DIR", root.join("runtime"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .spawn()
                .unwrap(),
            slice,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "Daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
        (daemon, socket)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        stop_slice(&self.1);
    }
}

fn request(socket: &Path, message: Value) -> BufReader<UnixStream> {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
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

#[test]
fn changing_theme_recolors_existing_terminal_and_preserves_its_session() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ghostty_vt::snapshot::Decoder;
    use infrastructure::theme;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let (_daemon, socket) = Daemon::start(root);
    let launched = event(&mut request(
        &socket,
        json!({"request":"launch", "worktree":root, "agent":"shell"}),
    ));
    let id = &launched["session"]["id"];
    let mut attached = request(&socket, json!({"request":"subscribe", "session":id}));
    assert_eq!(event(&mut attached)["event"], "snapshot");
    let input = |command: &str| {
        assert_eq!(
            event(&mut request(
                &socket,
                json!({
                    "request":"input", "session":id, "data":STANDARD.encode(command),
                })
            ))["event"],
            "ok"
        );
    };
    let wait_for = |attached: &mut BufReader<UnixStream>, marker: &[u8]| {
        let mut output = Vec::new();
        loop {
            let message = event(attached);
            if message["event"] == "output" {
                output.extend(STANDARD.decode(message["data"].as_str().unwrap()).unwrap());
                if output.windows(marker.len()).any(|part| part == marker) {
                    break;
                }
            }
        }
        output
    };
    input("saved=still_here; printf '\\033[31mBEFORE\\033[0m\\n'\n");
    wait_for(&mut attached, b"\x1b[31mBEFORE");

    let scheme = theme::find("Catppuccin Latte").unwrap();
    assert_eq!(
        event(&mut request(
            &socket,
            json!({
                "request":"set_color_scheme", "name":scheme.name,
            })
        ))["event"],
        "ok"
    );
    input("printf '\\033[0m%s\\n' \"$saved\"\n");
    wait_for(&mut attached, b"\x1b[0mstill_here");
    let snapshot = event(&mut request(
        &socket,
        json!({"request":"subscribe", "session":id}),
    ));
    assert_eq!(snapshot["exited"], false);
    let data = STANDARD.decode(snapshot["data"].as_str().unwrap()).unwrap();
    let terminal = Decoder::from_slice(&data).decode().unwrap();
    let background = terminal.bg_color().unwrap().unwrap();
    assert_eq!(
        u32::from_be_bytes([0, background.r, background.g, background.b]),
        scheme.colors.background
    );
    assert_eq!(
        u32::from_be_bytes({
            let c = terminal.color_palette().unwrap().0[1];
            [0, c.r, c.g, c.b]
        }),
        scheme.colors.indexed[1]
    );
    event(&mut request(
        &socket,
        json!({"request":"end", "session":id}),
    ));
}

#[test]
fn reattachment_preserves_terminal_and_published_artifact() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ghostty_vt::{
        snapshot::Decoder,
        terminal::{Point, PointCoordinate},
    };

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let (_daemon, socket) = Daemon::start(root);
    let binary = env!("CARGO_BIN_EXE_power");
    let launched = event(&mut request(
        &socket,
        json!({
            "request":"launch", "worktree":root, "agent":"shell",
        }),
    ));
    let id = launched["session"]["id"].as_str().unwrap();
    let asset = root.join("my clip.mp4");
    std::fs::write(&asset, b"fixture").unwrap();
    let published = Command::new(binary)
        .args(["artifact", "publish", "my clip.mp4"])
        .current_dir(root)
        .env("POWER_SOCKET", &socket)
        .env("POWER_SESSION", id)
        .output()
        .unwrap();
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let uri = String::from_utf8(published.stdout).unwrap();
    let uri = uri.trim();
    let artifact_id = uri.strip_prefix("pwr://artifact/").unwrap();
    let mut attached = request(&socket, json!({"request":"subscribe", "session":id}));
    assert_eq!(event(&mut attached)["event"], "snapshot");
    let command = format!("printf '\\033[2J\\033[H\\033]8;;{uri}\\aLINK\\033]8;;\\a\\n'\n");
    event(&mut request(
        &socket,
        json!({
            "request":"input", "session":id, "data":STANDARD.encode(command),
        }),
    ));
    loop {
        let message = event(&mut attached);
        if let Some(part) = message["data"].as_str() {
            let bytes = STANDARD.decode(part).unwrap();
            if bytes.windows(3).any(|part| part == b"\x1b]8") {
                break;
            }
        }
    }
    drop(attached);

    let snapshot = event(&mut request(
        &socket,
        json!({"request":"subscribe", "session":id}),
    ));
    assert_eq!(snapshot["exited"], false);
    let data = STANDARD.decode(snapshot["data"].as_str().unwrap()).unwrap();
    let terminal = Decoder::from_slice(&data).decode().unwrap();
    let cell = terminal
        .grid_ref(Point::Viewport(PointCoordinate { x: 0, y: 0 }))
        .unwrap();
    let mut link = [0; 512];
    let count = cell.hyperlink_uri(&mut link).unwrap();
    assert_eq!(&link[..count], uri.as_bytes());

    let direct = event(&mut request(
        &socket,
        json!({
            "request":"resolve", "session":id, "target":"my clip.mp4",
        }),
    ));
    let resolved = event(&mut request(
        &socket,
        json!({
            "request":"resolve", "session":id, "target":uri,
        }),
    ));
    assert_eq!(direct["target"], resolved["target"]);
    assert_eq!(resolved["target"]["value"].as_str(), asset.to_str());
    let legacy = event(&mut request(
        &socket,
        json!({
            "request":"resolve", "session":id, "target":format!("power://artifact/{artifact_id}"),
        }),
    ));
    assert_eq!(legacy["target"], resolved["target"]);
    event(&mut request(
        &socket,
        json!({"request":"end", "session":id}),
    ));
}

#[test]
fn live_directory_does_not_change_the_session_worktree() {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let directory = tempfile::tempdir().unwrap();
    let worktree = directory.path().canonicalize().unwrap();
    let subdirectory = worktree.join("live cwd");
    std::fs::create_dir(&subdirectory).unwrap();
    let (_daemon, socket) = Daemon::start(&worktree);
    let launched = event(&mut request(
        &socket,
        json!({"request":"launch", "worktree":worktree, "agent":"shell"}),
    ));
    let session = &launched["session"]["id"];
    event(&mut request(
        &socket,
        json!({
            "request":"input", "session":session,
            "data":STANDARD.encode("cd 'live cwd'\n"),
        }),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = event(&mut request(
            &socket,
            json!({"request":"working_directory", "session":session}),
        ));
        if response["directory"] == json!(subdirectory) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Live directory remained {response}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let listed = event(&mut request(&socket, json!({"request":"list"})));
    assert_eq!(listed["sessions"][0]["worktree"], json!(worktree));
    event(&mut request(
        &socket,
        json!({"request":"end", "session":session}),
    ));
}

#[test]
fn shell_tracks_an_agent_started_inside_it() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let agent = root.join("claude");
    std::fs::write(&agent, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_daemon, socket) = Daemon::start(&root);
    let launched = event(&mut request(
        &socket,
        json!({"request":"launch", "worktree":root, "agent":"shell"}),
    ));
    let session = &launched["session"]["id"];
    let mut attached = request(&socket, json!({"request":"subscribe", "session":session}));
    assert_eq!(event(&mut attached)["event"], "snapshot");
    assert_eq!(event(&mut attached)["agent"], "shell");
    let input = |data: &str| {
        event(&mut request(
            &socket,
            json!({"request":"input", "session":session, "data":STANDARD.encode(data)}),
        ));
    };
    let wait_for = |attached: &mut BufReader<UnixStream>, agent: &str| loop {
        let message = event(attached);
        if message["event"] == "agent" {
            assert_eq!(message["agent"], agent);
            break;
        }
    };

    input("./claude\n");
    wait_for(&mut attached, "claude");
    let listed = event(&mut request(&socket, json!({"request":"list"})));
    assert_eq!(listed["sessions"][0]["agent"], "claude");

    input("\x03");
    wait_for(&mut attached, "shell");
    event(&mut request(
        &socket,
        json!({"request":"end", "session":session}),
    ));
}

#[test]
fn restored_shell_resumes_agent_in_foreground_and_accepts_its_task() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use infrastructure::shell::Shell;
    use std::{fs, os::unix::fs::PermissionsExt};

    for (shell, rc) in [
        (Shell::Bash, ".bashrc"),
        (Shell::Zsh, ".zshrc"),
        (Shell::Fish, "config/fish/config.fish"),
    ] {
        let path = std::env::var_os("PATH").unwrap();
        let Some(shell_path) = std::env::split_paths(&path)
            .map(|dir| dir.join(shell.name()))
            .find(|path| path.is_file())
        else {
            continue;
        };
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let rc = root.join(rc);
        fs::create_dir_all(rc.parent().unwrap()).unwrap();
        fs::write(rc, shell.hook()).unwrap();
        let agent = root.join("claude");
        fs::write(
            &agent,
            r#"#!/bin/sh
set -e
task=initial
for arg do
    case "$arg" in
        --continue|--resume)
            set -- $(ps -o pgid=,tpgid=,sid= -p "$$")
            task=resumed
            if [ "$1" != "$2" ] || [ "$1" = "$3" ]; then
                task="unexpected groups: pgid=$1 tpgid=$2 sid=$3"
            fi
            break
            ;;
    esac
done
"$POWER_BIN" session task "$task"
read -r finish
"#,
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        let (daemon, socket) = Daemon::start(root);
        let session = event(&mut request(
            &socket,
            json!({"request":"launch", "worktree":root, "agent":"shell"}),
        ))["session"]["id"]
            .clone();
        let wait_for_task = |task: &str| {
            let mut attached = request(&socket, json!({"request":"subscribe", "session":session}));
            loop {
                let message = event(&mut attached);
                if let Some(reported) = message["metadata"]["task"].as_str() {
                    assert_eq!(reported, task, "{shell:?}");
                    break;
                }
            }
            let listed = event(&mut request(&socket, json!({"request":"list"})));
            assert_eq!(listed["sessions"][0]["agent"], "claude", "{shell:?}");
        };
        // Reproduce the shared process group from a shell without job control.
        event(&mut request(
            &socket,
            json!({"request":"input", "session":session, "data":STANDARD.encode("set +m; ./claude\n")}),
        ));
        wait_for_task("initial");
        drop(daemon);

        let path = std::env::join_paths(
            std::iter::once(root.to_owned()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        let (_restored, _) = Daemon::start_with(
            root,
            Command::new(env!("CARGO_BIN_EXE_power"))
                .env("HOME", root)
                .env("ZDOTDIR", root)
                .env("SHELL", shell_path)
                .env("PATH", path),
        );
        wait_for_task("resumed");
        event(&mut request(
            &socket,
            json!({"request":"end", "session":session}),
        ));
    }
}

#[test]
fn jcode_gets_typed_instructions_a_private_socket_and_resumes_its_own_session() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // A deterministic stand-in for jcode's TUI. Each launch records its arguments and
    // socket. A fresh one reads the typed first message (a bracketed paste ended by
    // Enter), then reports a task the way jcode's bash tool runs commands, with
    // `JCODE_SESSION_ID` set. Marker files make a resume fail or a fresh start stay quiet.
    fs::write(
        root.join("jcode"),
        r#"#!/bin/sh
set -eu
dir="${POWER_SOCKET%/*}"
n=$(($(cat "$dir/count" 2>/dev/null || echo 0) + 1))
echo "$n" > "$dir/count"
printf '%s\n' "$@" > "$dir/launch.$n.args"
printf '%s' "$JCODE_SOCKET" > "$dir/launch.$n.socket"
if test "${1:-}" = --resume; then
    test -e "$dir/reject-resume" && exit 1
    JCODE_SESSION_ID="$2" "$POWER_BIN" session task "resumed-$n"
    read -r finish
    exit
fi
stty -echo
# Like a TUI in raw mode, announce that typed input is welcome.
printf '\033[?2004h'
end="$(printf '\033')[201~"
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$dir/launch.$n.message"
    case "$line" in *"$end"*) break ;; esac
done
# The second Enter arrives as an empty line.
IFS= read -r second
test -z "$second"
if test -e "$dir/quiet"; then
    "$POWER_BIN" session task "quiet-$n"
else
    JCODE_SESSION_ID=session_fake_1 "$POWER_BIN" session task "started-$n"
fi
read -r finish
"#,
    )
    .unwrap();
    fs::set_permissions(root.join("jcode"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(root.to_owned())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let start = || {
        Daemon::start_with(
            root,
            Command::new(env!("CARGO_BIN_EXE_power"))
                .env("SHELL", "/bin/sh")
                .env("PATH", &path)
                // A daemon started from inside jcode inherits its conversation, which
                // no session it launches may report as its own.
                .env("JCODE_SESSION_ID", "session_of_the_outer_jcode"),
        )
    };
    let (daemon, socket) = start();
    let session = event(&mut request(
        &socket,
        json!({"request":"launch", "worktree":root, "agent":"jcode"}),
    ))["session"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let wait_for_task = |task: &str| {
        let mut attached = request(&socket, json!({"request":"subscribe", "session":session}));
        while event(&mut attached)["metadata"]["task"].as_str() != Some(task) {}
    };
    let read = |name: &str| fs::read_to_string(root.join(name)).unwrap();
    wait_for_task("started-1");
    // No arguments: printf still writes one empty line.
    assert_eq!(read("launch.1.args"), "\n");
    let first_socket = read("launch.1.socket");
    assert!(first_socket.len() <= 107, "{first_socket}");
    assert_eq!(Path::new(&first_socket).parent(), Some(root));
    let message = read("launch.1.message");
    assert!(
        message.starts_with("\x1b[200~You are running inside Power Overwhelming."),
        "{message:?}"
    );
    assert!(message.contains("`power session task 'Short task label'`"));
    assert!(message.trim_end().ends_with("\x1b[201~"), "{message:?}");
    drop(daemon);

    // The restore resumes the session jcode reported, on a socket of its own launch.
    fs::write(root.join("reject-resume"), "").unwrap();
    fs::write(root.join("quiet"), "").unwrap();
    let (daemon, _) = start();
    // The resume fails and a fresh start replaces it.
    wait_for_task("quiet-3");
    assert_eq!(read("launch.2.args"), "--resume\nsession_fake_1\n");
    assert_eq!(read("launch.3.args"), "\n");
    let sockets = [
        first_socket,
        read("launch.2.socket"),
        read("launch.3.socket"),
    ];
    assert!(
        sockets[0] != sockets[1] && sockets[1] != sockets[2],
        "{sockets:?}"
    );
    drop(daemon);

    // The fresh start never reported a conversation, so the rejected one is gone and the
    // next restore starts fresh instead of returning to it.
    fs::remove_file(root.join("reject-resume")).unwrap();
    let (_restored, _) = start();
    wait_for_task("quiet-4");
    assert_eq!(read("launch.4.args"), "\n");
    event(&mut request(
        &socket,
        json!({"request":"end", "session":session}),
    ));
}

#[test]
fn agent_task_and_model_follow_the_run_and_survive_reattachment() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let agent = root.join("codex");
    std::fs::write(
        &agent,
        r#"#!/bin/sh
set -e
power session task "Initial task"
power session model "Agent-reported model"
printf '%s' '{"model":"codex-hook-model"}' | power session model --json
printf '%s' '{"model":{"id":"status-model","display_name":"Status model"}}' | power session model --json
printf '%s' '{"model":{"id":"status-model","display_name":"Status model"}}' | power session model --json
printf '%s' '{"model":{"display_name":"Next model"}}' | power session model --json
printf '%s' '{}' | power session model --json
power session task "Revised task"
read finish
"#,
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_daemon, socket) = Daemon::start(root);
    let launched = event(&mut request(
        &socket,
        json!({"request":"launch", "worktree":root, "agent":"shell"}),
    ));
    let session = &launched["session"]["id"];
    let subscribe = || request(&socket, json!({"request":"subscribe", "session":session}));
    let mut attached = subscribe();
    assert_eq!(event(&mut attached)["event"], "snapshot");
    assert_eq!(event(&mut attached)["agent"], "shell");
    let input = |data: &str| {
        event(&mut request(
            &socket,
            json!({"request":"input", "session":session, "data":STANDARD.encode(data)}),
        ));
    };
    let wait_for_metadata = |attached: &mut BufReader<UnixStream>| loop {
        let message = event(attached);
        if message["event"] == "metadata" {
            break message["metadata"].clone();
        }
    };
    input("./codex --model launch-model\n");
    assert_eq!(wait_for_metadata(&mut attached)["model"], "launch-model");
    assert_eq!(wait_for_metadata(&mut attached)["task"], "Initial task");
    assert_eq!(
        wait_for_metadata(&mut attached)["model"],
        "Agent-reported model"
    );
    assert_eq!(
        wait_for_metadata(&mut attached)["model"],
        "codex-hook-model"
    );
    assert_eq!(wait_for_metadata(&mut attached)["model"], "status-model");
    assert_eq!(wait_for_metadata(&mut attached)["model"], "Next model");
    assert_eq!(wait_for_metadata(&mut attached)["task"], "Revised task");

    let mut reattached = subscribe();
    assert_eq!(event(&mut reattached)["event"], "snapshot");
    assert_eq!(event(&mut reattached)["agent"], "codex");
    assert_eq!(
        wait_for_metadata(&mut reattached),
        json!({
            "task":"Revised task", "model":"Next model",
        })
    );
    let mut outsider = request(
        &socket,
        json!({
            "request":"set_metadata", "session":session, "field":"task", "value":"Wrong run",
        }),
    );
    let mut response = String::new();
    outsider.read_line(&mut response).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["event"],
        "error"
    );

    input("\n");
    assert_eq!(
        wait_for_metadata(&mut reattached),
        json!({"task":null, "model":null})
    );
    event(&mut request(
        &socket,
        json!({"request":"end", "session":session}),
    ));
}

#[test]
fn shell_hook_gives_hand_started_agents_instructions() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\necho \"ARGS:$1\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let setup = Command::new(env!("CARGO_BIN_EXE_power"))
        .arg("setup")
        .env("HOME", &root)
        .env("SHELL", "/bin/bash")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );

    let socket = root.join("power.sock");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut command = Command::new(env!("CARGO_BIN_EXE_power"));
    command
        .arg("daemon")
        .env("POWER_SOCKET", &socket)
        .env("POWER_STATE_DIR", root.join("state"))
        .env("HOME", &root)
        .env("SHELL", "/bin/bash")
        .env("PATH", path);
    let slice = own_slice(&mut command);
    let _daemon = Daemon(command.spawn().unwrap(), slice);
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "Daemon did not start");
        thread::sleep(Duration::from_millis(20));
    }
    let launched = event(&mut request(
        &socket,
        json!({"request":"launch", "worktree":root, "agent":"shell"}),
    ));
    let session = &launched["session"]["id"];
    let mut attached = request(&socket, json!({"request":"subscribe", "session":session}));
    assert_eq!(event(&mut attached)["event"], "snapshot");
    event(&mut request(
        &socket,
        json!({"request":"input", "session":session, "data":STANDARD.encode("claude hi\n")}),
    ));
    let mut output = Vec::new();
    while !String::from_utf8_lossy(&output).contains("ARGS:--append-system-prompt") {
        let message = event(&mut attached);
        if let Some(data) = message["data"].as_str() {
            output.extend(STANDARD.decode(data).unwrap());
        }
    }
    event(&mut request(
        &socket,
        json!({"request":"end", "session":session}),
    ));
}

#[test]
fn publishing_streams_an_event_and_lists_history_newest_first() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (_daemon, socket) = Daemon::start(&root);
    let launch = || {
        event(&mut request(
            &socket,
            json!({"request":"launch", "worktree":root, "agent":"shell"}),
        ))["session"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (first, other) = (launch(), launch());
    let mut watch = request(&socket, json!({"request":"watch_artifacts"}));
    // The watch registers before the first publish once the daemon has read the request.
    thread::sleep(Duration::from_millis(200));
    let publish = |session: &str, file: &str| {
        std::fs::write(root.join(file), b"fixture").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_power"))
            .args(["artifact", "publish", file, "--title", file])
            .current_dir(&root)
            .env("POWER_SOCKET", &socket)
            .env("POWER_SESSION", session)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let older = publish(&first, "older.png");
    let published = loop {
        let message = event(&mut watch);
        if message["event"] == "artifact_published" {
            break message;
        }
    };
    assert_eq!(published["artifact"]["uri"], older);
    assert_eq!(published["artifact"]["session"], first);
    assert_eq!(published["artifact"]["title"], "older.png");
    let newer = publish(&first, "newer.png");
    publish(&other, "elsewhere.png");

    let recent = event(&mut request(
        &socket,
        json!({"request":"recent_artifacts", "sessions":[first], "limit":10}),
    ));
    let uris: Vec<_> = recent["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| artifact["uri"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(uris, [newer, older]);
}
