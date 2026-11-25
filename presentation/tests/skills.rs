use serde_json::{Value, json};
use std::{
    fs,
    process::{Command, Output},
};

const DAG_GUIDE: &str = include_str!("../../skills/power-dag/guide.md");
const ARTIFACTS_GUIDE: &str = include_str!("../../skills/power-artifacts/guide.md");
const WORKER_CONTRACT: &str = include_str!("../../skills/power-dag/references/worker-contract.md");
const RECOVERY: &str = include_str!("../../skills/power-dag/references/recovery.md");
const EXAMPLE: &str = include_str!("../../skills/power-dag/references/example.md");
const REFERENCES: [&str; 3] = ["worker-contract", "recovery", "example"];

fn power(args: &[&str]) -> Output {
    let root = tempfile::tempdir().unwrap();
    let invalid_parent = root.path().join("file");
    fs::write(
        &invalid_parent,
        "A daemon socket cannot be created below this file.",
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_power"))
        .current_dir(root.path())
        .env("POWER_SOCKET", invalid_parent.join("power.sock"))
        .env_remove("POWER_BIN")
        .env_remove("POWER_SESSION")
        .args(args)
        .output()
        .unwrap()
}

#[track_caller]
fn stdout(args: &[&str]) -> String {
    let output = power(args);
    assert!(output.status.success(), "{args:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[track_caller]
fn stderr(args: &[&str]) -> String {
    let output = power(args);
    assert!(!output.status.success(), "{args:?}: {output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    String::from_utf8(output.stderr).unwrap()
}

#[track_caller]
fn json_stdout(args: &[&str]) -> Value {
    serde_json::from_str(&stdout(args)).unwrap()
}

#[test]
fn lists_skills_and_descriptions() {
    let output = stdout(&["skills", "list"]);
    let entries: Vec<_> = output
        .lines()
        .map(|line| line.split_once('\t').unwrap())
        .collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "power-artifacts");
    assert!(entries[0].1.contains("localhost preview"));
    assert_eq!(entries[1].0, "power-dag");
    assert!(entries[1].1.contains("dispatched worker"));
}

#[test]
fn get_and_show_print_the_kernel_by_default() {
    assert_eq!(stdout(&["skills", "get", "power-dag"]), DAG_GUIDE);
    assert_eq!(stdout(&["skills", "show", "power-dag"]), DAG_GUIDE);
    assert_eq!(
        stdout(&["skills", "get", "power-artifacts"]),
        ARTIFACTS_GUIDE
    );
}

#[test]
fn lists_reference_names() {
    let output = stdout(&["skills", "get", "power-dag", "--references"]);
    assert_eq!(output.lines().collect::<Vec<_>>(), REFERENCES);
    assert_eq!(
        stdout(&["skills", "get", "power-artifacts", "--references"]),
        ""
    );
}

#[test]
fn prints_a_reference_by_name_or_path() {
    for selector in ["recovery", "references/recovery.md"] {
        assert_eq!(
            stdout(&["skills", "get", "power-dag", "--reference", selector]),
            RECOVERY
        );
    }
}

#[test]
fn prints_the_full_guide() {
    let full = stdout(&["skills", "get", "power-dag", "--full"]);
    let rest = full.strip_prefix(DAG_GUIDE).unwrap();
    for reference in [WORKER_CONTRACT, RECOVERY, EXAMPLE] {
        assert!(rest.contains(reference.trim_end()));
    }
    assert_eq!(
        stdout(&["skills", "get", "power-artifacts", "--full"]),
        ARTIFACTS_GUIDE
    );
}

#[test]
fn rejects_unknown_names_and_lists_valid_names() {
    let error = stderr(&["skills", "get", "missing"]);
    for name in ["missing", "power-artifacts", "power-dag"] {
        assert!(error.contains(name), "{error}");
    }

    let error = stderr(&["skills", "get", "power-dag", "--reference", "missing"]);
    for name in ["missing", "power-dag"].into_iter().chain(REFERENCES) {
        assert!(error.contains(name), "{error}");
    }
}

#[test]
fn rejects_conflicting_selections() {
    for flags in [
        &["--references", "--full"][..],
        &["--reference", "recovery", "--references"],
        &["--reference", "recovery", "--full"],
    ] {
        let mut args = vec!["skills", "get", "power-dag"];
        args.extend_from_slice(flags);
        assert!(stderr(&args).contains("cannot be used with"));
    }
}

#[test]
fn emits_json_for_each_selection() {
    let output = stdout(&["skills", "list"]);
    let skills: Vec<_> = output
        .lines()
        .map(|line| {
            let (name, description) = line.split_once('\t').unwrap();
            json!({"name": name, "description": description})
        })
        .collect();
    assert_eq!(
        json_stdout(&["skills", "list", "--json"]),
        json!({"skills": skills})
    );
    assert_eq!(
        json_stdout(&["skills", "get", "power-dag", "--json"]),
        json!({"name": "power-dag", "full": false, "markdown": DAG_GUIDE})
    );
    assert_eq!(
        json_stdout(&["skills", "get", "power-dag", "--references", "--json"]),
        json!({"name": "power-dag", "references": REFERENCES})
    );
    assert_eq!(
        json_stdout(&[
            "skills",
            "get",
            "power-dag",
            "--reference",
            "references/recovery.md",
            "--json"
        ]),
        json!({"name": "power-dag", "reference": "recovery", "markdown": RECOVERY})
    );
    assert_eq!(
        json_stdout(&["skills", "get", "power-dag", "--full", "--json"]),
        json!({"name": "power-dag", "full": true, "markdown": stdout(&["skills", "get", "power-dag", "--full"])})
    );
}

#[test]
fn dag_guide_is_the_full_catalog_guide() {
    assert_eq!(
        stdout(&["dag", "guide"]),
        stdout(&["skills", "get", "power-dag", "--full"])
    );
}
