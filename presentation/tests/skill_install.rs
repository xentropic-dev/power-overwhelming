use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const TALK_STUB: &str = include_str!("../../skills/power-talk/SKILL.md");
const DAG_STUB: &str = include_str!("../../skills/power-dag/SKILL.md");
const ARTIFACTS_STUB: &str = include_str!("../../skills/power-artifacts/SKILL.md");
const FOREIGN: &str = "---\nname: power-dag\ndescription: A different DAG skill.\n---\n";

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&cwd).unwrap();
        Self {
            _root: root,
            home,
            cwd,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_power"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("POWER_SOCKET", self.home.join("absent/power.sock"))
            .args(["skills"])
            .args(args)
            .output()
            .unwrap()
    }

    #[track_caller]
    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn shared(&self, skill: &str) -> PathBuf {
        self.home.join(".agents/skills").join(skill)
    }

    fn claude(&self, skill: &str) -> PathBuf {
        self.home.join(".claude/skills").join(skill)
    }
}

fn write_skill(directory: &Path, stub: &str) {
    fs::create_dir_all(directory).unwrap();
    fs::write(directory.join("SKILL.md"), stub).unwrap();
}

#[test]
fn default_install_writes_all_stubs_and_reuses_them_on_rerun() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.home.join(".claude")).unwrap();
    fs::create_dir(fixture.home.join(".codex")).unwrap();

    let install = fixture.json(&["install", "--json"]);
    let again = fixture.json(&["install", "--json"]);
    let updates = fixture.json(&["update", "--json"]);
    assert_eq!(install["failed"], 0);
    assert_eq!(install["locations"].as_array().unwrap().len(), 6);
    assert_eq!(again, updates);
    for location in again["locations"].as_array().unwrap() {
        assert_eq!(location["action"], "no-change");
    }
    for (name, stub) in [
        ("power-artifacts", ARTIFACTS_STUB),
        ("power-dag", DAG_STUB),
        ("power-talk", TALK_STUB),
    ] {
        let shared = fixture.shared(name);
        assert!(!shared.is_symlink());
        assert_eq!(fs::read_dir(&shared).unwrap().count(), 1);
        assert_eq!(fs::read_to_string(shared.join("SKILL.md")).unwrap(), stub);
        assert_eq!(fs::read_link(fixture.claude(name)).unwrap(), shared);
    }
    assert!(!fixture.home.join(".codex/skills").exists());
}

#[test]
fn explicit_agent_and_skill_selections_control_targets() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.home.join(".claude")).unwrap();

    fixture.json(&[
        "install",
        "--skill",
        "power-dag",
        "--agent",
        "universal",
        "--json",
    ]);
    assert!(fixture.shared("power-dag").exists());
    assert!(!fixture.shared("power-artifacts").exists());
    assert!(!fixture.home.join(".claude/skills").exists());
    fixture.json(&["update", "--json"]);
    assert!(!fixture.claude("power-dag").exists());
    assert!(!fixture.shared("power-artifacts").exists());

    fixture.json(&[
        "install",
        "--skill",
        "power-dag",
        "--skill",
        "power-artifacts",
        "--agent",
        "claude,codex",
        "--json",
    ]);
    assert!(fixture.claude("power-dag").is_symlink());
    assert!(fixture.claude("power-artifacts").is_symlink());
}

#[test]
fn edited_and_foreign_copies_are_warnings_and_keep_their_contents() {
    let fixture = Fixture::new();
    let edited = format!("{ARTIFACTS_STUB}\nA local edit.\n");
    let artifacts = fixture.shared("power-artifacts");
    let dag = fixture.shared("power-dag");
    write_skill(&artifacts, &edited);
    write_skill(&dag, FOREIGN);
    fs::write(dag.join("extra.txt"), "extra").unwrap();

    let result = fixture.json(&["install", "--json"]);
    for (location, reason) in result["locations"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["changed", "foreign"])
    {
        assert_eq!(location["action"], "left-alone");
        assert_eq!(location["reason"], reason);
    }
    assert_eq!(result["failed"], 0);
    let human = fixture.run(&["update"]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stderr).contains("warning: left-alone"));
    assert_eq!(
        fs::read_to_string(artifacts.join("SKILL.md")).unwrap(),
        edited
    );
    assert_eq!(fs::read_to_string(dag.join("SKILL.md")).unwrap(), FOREIGN);
    assert_eq!(fs::read_to_string(dag.join("extra.txt")).unwrap(), "extra");
}

#[test]
fn force_backs_up_entire_conflicting_directories_before_replacement() {
    let fixture = Fixture::new();
    let shared = fixture.shared("power-dag");
    let claude = fixture.claude("power-dag");
    for directory in [&shared, &claude] {
        write_skill(directory, FOREIGN);
        fs::write(directory.join("keep"), "preserve me").unwrap();
    }

    let result = fixture.json(&["update", "--skill", "power-dag", "--force", "--json"]);
    let locations = result["locations"].as_array().unwrap();
    assert_eq!(locations.len(), 2);
    assert_eq!(locations[0]["action"], "write");
    assert_eq!(locations[1]["action"], "link");
    let backup_root = fixture.home.join(".agents/.power-skill-backups");
    assert_eq!(fs::read_dir(&backup_root).unwrap().count(), 2);
    for location in locations {
        let backup = Path::new(location["backup"].as_str().unwrap());
        assert!(backup.starts_with(&backup_root));
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("power-dag-")
        );
        assert_eq!(
            fs::read_to_string(backup.join("SKILL.md")).unwrap(),
            FOREIGN
        );
        assert_eq!(
            fs::read_to_string(backup.join("keep")).unwrap(),
            "preserve me"
        );
    }
    assert_eq!(
        fs::read_to_string(shared.join("SKILL.md")).unwrap(),
        DAG_STUB
    );
    assert_eq!(fs::read_link(claude).unwrap(), shared);
}

#[test]
fn dry_run_leaves_missing_directories_and_pending_backups_untouched() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.home.join(".claude")).unwrap();
    let result = fixture.json(&["install", "--dry-run", "--json"]);
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["locations"][0]["action"], "write");
    assert_eq!(result["locations"][1]["action"], "link");
    assert!(!fixture.home.join(".agents").exists());
    assert!(!fixture.home.join(".claude/skills").exists());

    let shared = fixture.shared("power-dag");
    write_skill(&shared, FOREIGN);
    let result = fixture.json(&["install", "--force", "--dry-run", "--json"]);
    assert_eq!(result["locations"][2]["backup_required"], true);
    assert!(!fixture.home.join(".agents/.power-skill-backups").exists());
    assert_eq!(
        fs::read_to_string(shared.join("SKILL.md")).unwrap(),
        FOREIGN
    );
    assert!(!fixture.home.join(".claude/skills").exists());
}

#[test]
fn local_uses_the_git_root_and_only_an_existing_project_agent_skills_folder() {
    let mut fixture = Fixture::new();
    let git_root = fixture.cwd.clone();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&git_root)
            .status()
            .unwrap()
            .success()
    );
    fixture.cwd = git_root.join("nested");
    fs::create_dir(&fixture.cwd).unwrap();
    fs::create_dir(fixture.home.join(".claude")).unwrap();
    fs::create_dir(git_root.join(".claude")).unwrap();

    fixture.json(&["install", "--local", "--json"]);
    let shared = git_root.join(".agents/skills/power-dag");
    assert_eq!(
        fs::read_to_string(shared.join("SKILL.md")).unwrap(),
        DAG_STUB
    );
    assert!(!git_root.join(".claude/skills").exists());
    assert!(!fixture.cwd.join(".agents").exists());
    assert!(!fixture.home.join(".agents").exists());
    assert!(!fixture.home.join(".claude/skills").exists());

    fs::create_dir(git_root.join(".claude/skills")).unwrap();
    fixture.json(&["install", "--local", "--json"]);
    assert_eq!(
        fs::read_link(git_root.join(".claude/skills/power-dag")).unwrap(),
        shared
    );
}

#[test]
fn location_failures_return_one_and_other_locations_can_succeed() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.home.join(".claude")).unwrap();
    fs::write(fixture.home.join(".claude/skills"), "blocked").unwrap();

    let output = fixture.run(&["install", "--skill", "power-dag", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["failed"], 1);
    assert!(result["locations"][1]["error"].is_string());
    assert!(fixture.shared("power-dag").exists());
}
