#![cfg(feature = "wasm-plugins")]

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

#[test]
fn status_plugins_receive_view_metadata_and_publish_template_fields() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let component = wat::parse_str(include_str!("fixtures/status-bar-plugin.wat"))?;
    fs::write(directory.path().join("status.wasm"), component)?;
    let mut worker = Command::new(env!("CARGO_BIN_EXE_power-plugin-host"))
        .arg(directory.path())
        .arg("--status-bar")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut input = worker.stdin.take().context("Missing plugin input")?;
    let mut replies = BufReader::new(worker.stdout.take().context("Missing plugin output")?);
    let mut line = String::new();
    replies.read_line(&mut line)?;
    assert_eq!(serde_json::from_str::<Value>(&line)?, "Ready");

    writeln!(
        input,
        "{}",
        json!({"ActiveView": {
            "session": "linked-view", "worktree_name": "linked", "worktree_path": "/trees/linked",
            "repository_path": "/repo", "agent_type": "Codex", "agent_task": "Check the bar",
            "agent_model": null
        }})
    )?;
    drop(input);

    line.clear();
    replies.read_line(&mut line)?;
    assert_eq!(
        serde_json::from_str::<Value>(&line)?,
        json!({
            "StatusFields": {"fixture": "linked-view"}
        })
    );
    assert!(worker.wait()?.success());
    Ok(())
}
