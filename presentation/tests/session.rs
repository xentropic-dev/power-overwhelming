use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command},
    thread,
    time::Duration,
};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
fn reattachment_preserves_terminal_and_published_artifact() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ghostty_vt::{
        snapshot::Decoder,
        terminal::{Point, PointCoordinate},
    };

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let socket = root.join("power.sock");
    let binary = env!("CARGO_BIN_EXE_power");
    let _daemon = Daemon(
        Command::new(binary)
            .arg("daemon")
            .env("POWER_SOCKET", &socket)
            .env("POWER_STATE_DIR", root.join("state"))
            .env("SHELL", "/bin/sh")
            .spawn()
            .unwrap(),
    );
    for _ in 0..100 {
        if UnixStream::connect(&socket).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
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
