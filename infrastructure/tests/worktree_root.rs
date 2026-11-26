#![cfg(feature = "lua-config")]

use anyhow::{Result, ensure};
use application::WorktreeRoot;
use infrastructure::workspace::LocalWorktreeRoot;
use serde_json::json;
use std::{env, fs, path::PathBuf, process::Command};

#[test]
fn worktree_root_defaults_to_power_home_and_rereads_lua() -> Result<()> {
    const TEST_DIRECTORY: &str = "POWER_TEST_WORKTREE_ROOT_DIRECTORY";
    if let Some(directory) = env::var_os(TEST_DIRECTORY) {
        let directory = PathBuf::from(directory);
        let home = directory.join("home");
        let config = directory.join("config/power-overwhelming/config.lua");
        let default = home.join(".power");
        let resolver = LocalWorktreeRoot::new(
            [("keys".into(), json!({ "prefix": { "tab": "ctrl-a" } }))].into(),
        );

        assert_eq!(resolver.worktree_root()?, default);
        assert!(default.is_dir());
        fs::remove_dir_all(&default)?;
        assert_eq!(resolver.worktree_root()?, default);
        fs::write(default.join("existing"), "existing worktree")?;

        fs::create_dir_all(config.parent().unwrap())?;
        fs::write(
            &config,
            r#"
            keys.prefix.tab = "ctrl-b"
            power_bar.enabled = false
            agents.custom = {}
            worktrees.root = "~/custom trees"
            "#,
        )?;
        let first = home.join("custom trees");
        assert_eq!(resolver.worktree_root()?, first);
        fs::write(first.join("existing"), "existing worktree")?;

        fs::write(&config, r#"return { worktrees = { root = "~/other" } }"#)?;
        let second = home.join("other");
        assert_eq!(resolver.worktree_root()?, second);
        assert!(second.is_dir());
        assert!(first.join("existing").exists());
        assert!(default.join("existing").exists());

        for (source, expected) in [
            ("worktrees = false", "worktrees must be a table"),
            ("worktrees.root = 42", "worktrees.root must be"),
            ("worktrees.root = {}", "worktrees.root must be"),
            (r#"worktrees.root = """#, "worktrees.root must be"),
            ("worktrees.roots = {}", "worktrees.roots"),
        ] {
            fs::write(&config, source)?;
            let error = resolver.worktree_root().unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }

        fs::write(&config, r#"worktrees.root = "~/custom trees/existing""#)?;
        let error = resolver.worktree_root().unwrap_err();
        assert!(error.to_string().contains("custom trees/existing"));
        fs::remove_file(&config)?;
        assert_eq!(resolver.worktree_root()?, default);
        return Ok(());
    }

    // Run the assertions in child processes so each test has an isolated environment.
    // The XDG data directory must not affect the default.
    for data_home in [Some("data"), None] {
        let directory = tempfile::tempdir()?;
        let mut test = Command::new(env::current_exe()?);
        test.args([
            "--exact",
            "worktree_root_defaults_to_power_home_and_rereads_lua",
        ])
        .current_dir(directory.path())
        .env(TEST_DIRECTORY, directory.path())
        .env("HOME", directory.path().join("home"))
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .env("POWER_STATE_DIR", directory.path().join("state"));
        match data_home {
            Some(value) => test.env("XDG_DATA_HOME", directory.path().join(value)),
            None => test.env_remove("XDG_DATA_HOME"),
        };
        let output = test.output()?;
        ensure!(
            output.status.success(),
            "Worktree root test failed with XDG_DATA_HOME case {data_home:?}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
