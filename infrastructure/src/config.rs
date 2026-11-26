//! Load the Lua configuration into values that can be passed to plugin runtimes.

use crate::agents::AgentProfiles;
use anyhow::{Context, Result, bail};
use domain::{
    agent::{AgentProfile, Injection, ModelCallback, Resume},
    dag::WorkerBudget,
};
use mlua::{HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Value};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

pub type Sections = BTreeMap<String, JsonValue>;

pub const AGENTS_SECTION: &str = "agents";
pub(crate) const WORKTREES_SECTION: &str = "worktrees";
pub(crate) const DAG_SECTION: &str = "dag";

fn with_bundled_defaults(mut defaults: Sections) -> Sections {
    defaults.entry("power_bar".into()).or_insert_with(|| {
        serde_json::from_str(include_str!("../../plugins/power-bar/defaults.json"))
            .expect("bundled Power Bar defaults are valid JSON")
    });
    defaults
        .entry(AGENTS_SECTION.into())
        .or_insert_with(|| json!({}));
    defaults
        .entry(WORKTREES_SECTION.into())
        .or_insert_with(|| json!({}));
    defaults
        .entry(DAG_SECTION.into())
        .or_insert_with(|| json!({}));
    defaults
}

pub(crate) fn load_worker_budget(defaults: Sections) -> Result<WorkerBudget> {
    worker_budget(&load(with_bundled_defaults(defaults))?)
}

/// Reads the `dag` section over the default budget. A limit of 0 removes it.
pub(crate) fn worker_budget(sections: &Sections) -> Result<WorkerBudget> {
    let mut budget = WorkerBudget::default();
    let Some(value) = sections.get(DAG_SECTION) else {
        return Ok(budget);
    };
    let table = value.as_object().context("dag must be a table")?;
    for (name, value) in table {
        let count = || {
            value
                .as_u64()
                .and_then(|count| usize::try_from(count).ok())
                .map(|count| (count > 0).then_some(count))
                .with_context(|| format!("dag.{name} must be a whole number of workers"))
        };
        match name.as_str() {
            "max_active_workers" => budget.max_active_workers = count()?,
            "max_active_workers_per_run" => budget.max_active_workers_per_run = count()?,
            "max_active_workers_per_box" => budget.max_active_workers_per_box = count()?,
            "min_available_memory" => {
                let bytes = memory_size(value).with_context(|| {
                    format!("dag.{name} must be bytes or a size such as \"4 GiB\"")
                })?;
                budget.min_available_memory = (bytes > 0).then_some(bytes);
            }
            _ => bail!("dag.{name} is not a setting"),
        }
    }
    Ok(budget)
}

/// A byte count, or a string such as "4 GiB", "512MiB" or "2G" in binary units.
fn memory_size(value: &JsonValue) -> Option<u64> {
    if let Some(bytes) = value.as_u64() {
        return Some(bytes);
    }
    let text = value.as_str()?.trim();
    let split = text
        .find(|character: char| !(character.is_ascii_digit() || character == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let scale: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        _ => return None,
    };
    let bytes = number * scale as f64;
    (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

/// Load profiles from the same config as the desktop, with the bundled plugin defaults.
pub fn load_agent_profiles(defaults: Sections) -> Result<AgentProfiles> {
    agent_profiles(&load(with_bundled_defaults(defaults))?)
}

pub(crate) fn load_worktree_root(defaults: Sections) -> Result<Option<PathBuf>> {
    worktree_root(&load(with_bundled_defaults(defaults))?)
}

/// Validate and expand the configured root without touching the filesystem.
pub(crate) fn worktree_root(sections: &Sections) -> Result<Option<PathBuf>> {
    let Some(value) = sections.get(WORKTREES_SECTION) else {
        return Ok(None);
    };
    let table = value.as_object().context("worktrees must be a table")?;
    for name in table.keys() {
        if name != "root" {
            bail!("worktrees.{name} is not a setting");
        }
    }
    let Some(value) = table.get("root") else {
        return Ok(None);
    };
    let root = value
        .as_str()
        .filter(|root| !root.is_empty() && !root.contains('\0'))
        .context("worktrees.root must be a non-empty path string")?;
    let path = Path::new(root);
    if let Ok(relative) = path.strip_prefix("~") {
        let home = env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .context("Set HOME to expand worktrees.root")?;
        return Ok(Some(PathBuf::from(home).join(relative)));
    }
    Ok(Some(path.to_owned()))
}

/// Apply Lua profile fields over the built-in profiles.
pub fn agent_profiles(sections: &Sections) -> Result<AgentProfiles> {
    let mut profiles = AgentProfiles::default();
    let Some(section) = sections.get(AGENTS_SECTION) else {
        return Ok(profiles);
    };
    let entries = section
        .as_object()
        .context("agents must be a table keyed by profile ID")?;
    // Lua tables have no order, so new profiles follow the built-in ones by ID.
    let mut entries: Vec<_> = entries.iter().collect();
    entries.sort_by_key(|(id, _)| *id);
    for (id, value) in entries {
        let profile = read_agent_profile(id, value.clone(), &profiles)
            .with_context(|| format!("Read agents[{id:?}] from config.lua"))?;
        if let Some(index) = profiles
            .0
            .iter()
            .position(|existing| existing.id == profile.id)
        {
            profiles.0[index] = profile;
        } else {
            profiles.0.push(profile);
        }
    }
    Ok(profiles)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileConfig {
    command: Option<String>,
    injection: Option<InjectionConfig>,
    skip_subcommands: Option<Vec<String>>,
    skip_if_args_contain: Option<Vec<String>>,
    model_callback: Option<ModelCallbackConfig>,
    skills_dir: Option<PathBuf>,
    resume: Option<ResumeConfig>,
    env: Option<std::collections::BTreeMap<String, String>>,
    conversation_env: Option<String>,
    server_socket_env: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeConfig {
    assign: Option<Vec<String>>,
    resume: Option<Vec<String>>,
    resume_last: Option<Vec<String>>,
    conflicts: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum InjectionConfig {
    Flag { flag: String },
    ConfigValue { flag: String, key: String },
    FirstMessage,
    None,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ModelCallbackConfig {
    ClaudeStatusLine,
    Args { args: Vec<String> },
    None,
}

fn read_agent_profile(
    id: &str,
    mut value: JsonValue,
    profiles: &AgentProfiles,
) -> Result<AgentProfile> {
    let mut bytes = id.bytes();
    let name_char = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    anyhow::ensure!(
        bytes.next().is_some_and(name_char)
            && bytes.all(|byte| name_char(byte) || matches!(byte, b'-' | b'_'))
            && !matches!(id, "shell" | "command" | "power"),
        "Use a lowercase shell function name starting with a letter or digit, excluding shell, command and power"
    );
    normalize_empty_lists(&mut value, &["skip_subcommands", "skip_if_args_contain"]);
    if let Some(callback) = value.get_mut("model_callback") {
        normalize_empty_lists(callback, &["args"]);
    }
    if let Some(resume) = value.get_mut("resume") {
        normalize_empty_lists(resume, &["assign", "resume", "resume_last", "conflicts"]);
    }
    // Lua cannot tell an empty table from an empty list.
    if let Some(env) = value.get_mut("env")
        && env.as_array().is_some_and(Vec::is_empty)
    {
        *env = JsonValue::Object(Default::default());
    }
    let config: ProfileConfig = serde_json::from_value(value)?;
    let mut profile = profiles
        .0
        .iter()
        .find(|profile| profile.id.as_str() == id)
        .cloned()
        .unwrap_or_else(|| AgentProfile {
            id: id.into(),
            command: id.into(),
            injection: Injection::None,
            skip_subcommands: Vec::new(),
            skip_if_args_contain: Vec::new(),
            model_callback: ModelCallback::None,
            skills_dir: None,
            resume: None,
            env: Vec::new(),
            conversation_env: None,
            server_socket_env: None,
        });
    if let Some(command) = config.command {
        anyhow::ensure!(
            !command.trim().is_empty() && !command.contains('\0'),
            "command must name an executable"
        );
        profile.command = command;
    }
    if let Some(injection) = config.injection {
        profile.injection = match injection {
            InjectionConfig::Flag { flag } => Injection::Flag { flag },
            InjectionConfig::ConfigValue { flag, key } => Injection::ConfigValue { flag, key },
            InjectionConfig::FirstMessage => Injection::FirstMessage,
            InjectionConfig::None => Injection::None,
        };
    }
    if let Some(callback) = config.model_callback {
        profile.model_callback = match callback {
            ModelCallbackConfig::ClaudeStatusLine => ModelCallback::ClaudeStatusLine,
            ModelCallbackConfig::Args { args } => ModelCallback::Args { args },
            ModelCallbackConfig::None => ModelCallback::None,
        };
    }
    if let Some(skip) = config.skip_subcommands {
        profile.skip_subcommands = skip;
    }
    if let Some(skip) = config.skip_if_args_contain {
        profile.skip_if_args_contain = skip;
    }
    if let Some(directory) = config.skills_dir {
        profile.skills_dir = Some(directory);
    }
    if let Some(env) = config.env {
        anyhow::ensure!(
            env.keys()
                .all(|name| !name.is_empty() && !name.contains(['=', '\0'])),
            "env names must be non-empty and contain no = or NUL"
        );
        profile.env = env.into_iter().collect();
    }
    if let Some(name) = config.conversation_env {
        profile.conversation_env = (!name.is_empty()).then_some(name);
    }
    if let Some(name) = config.server_socket_env {
        anyhow::ensure!(
            !name.contains(['=', '\0']),
            "server_socket_env must contain no = or NUL"
        );
        profile.server_socket_env = (!name.is_empty()).then_some(name);
    }
    if let Some(config) = config.resume {
        let defaults = profile.resume.take().unwrap_or_default();
        profile.resume = Some(Resume {
            assign: config.assign.unwrap_or(defaults.assign),
            resume: config.resume.unwrap_or(defaults.resume),
            resume_last: config.resume_last.unwrap_or(defaults.resume_last),
            conflicts: config.conflicts.unwrap_or(defaults.conflicts),
        });
    }
    Ok(profile)
}

fn normalize_empty_lists(value: &mut JsonValue, keys: &[&str]) {
    // Lua's empty table becomes an empty JSON object, including when used as an empty list.
    for &key in keys {
        if let Some(list) = value.get_mut(key)
            && list.as_object().is_some_and(|table| table.is_empty())
        {
            *list = json!([]);
        }
    }
}

/// Power's configuration directory, following the XDG base directory convention.
pub fn directory() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|path| path.join("power-overwhelming"))
}

/// Load the registered configuration sections from Power's usual config.lua.
pub fn load(
    defaults: impl IntoIterator<Item = (String, JsonValue)>,
) -> Result<BTreeMap<String, JsonValue>> {
    match directory() {
        Some(directory) => load_from(&directory.join("config.lua"), defaults),
        None => Ok(defaults.into_iter().collect()),
    }
}

/// Seed named Lua globals with defaults, evaluate a file, and return those sections as JSON.
///
/// A returned table can also supply sections. Its entries take precedence over globals.
/// A missing file returns the defaults. Evaluate on a background thread in desktop callers.
pub fn load_from(
    path: &Path,
    defaults: impl IntoIterator<Item = (String, JsonValue)>,
) -> Result<BTreeMap<String, JsonValue>> {
    let defaults = defaults.into_iter().collect();
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(defaults),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    evaluate(&source, path, defaults).with_context(|| format!("Load {}", path.display()))
}

fn evaluate(
    source: &str,
    path: &Path,
    defaults: BTreeMap<String, JsonValue>,
) -> Result<BTreeMap<String, JsonValue>> {
    let libs = StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8;
    let lua = Lua::new_with(libs, LuaOptions::default())?;
    lua.set_memory_limit(8 * 1024 * 1024)?;
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(100_000),
        |_, _| {
            Err(mlua::Error::RuntimeError(
                "Power configuration took too long".into(),
            ))
        },
    )?;
    let globals = lua.globals();
    for (name, value) in &defaults {
        globals.raw_set(name.as_str(), lua.to_value(value)?)?;
    }
    let returned = match lua.load(source).set_name(path.to_string_lossy()).eval()? {
        Value::Nil => None,
        Value::Table(table) => Some(table),
        _ => bail!("Configuration must return a table or assign settings directly"),
    };
    defaults
        .into_iter()
        .map(|(name, default)| {
            let returned = returned
                .as_ref()
                .map(|table| table.raw_get::<Value>(name.as_str()))
                .transpose()?;
            let value = match returned {
                Some(value) if !value.is_nil() => value,
                _ => globals.raw_get(name.as_str())?,
            };
            let value = if value.is_nil() {
                default
            } else {
                lua.from_value(value)
                    .with_context(|| format!("Read configuration section {name:?}"))?
            };
            Ok((name, value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use application::agents::AgentProfileRegistry;
    use domain::dag::GIB;
    use serde_json::json;

    #[test]
    fn worker_budget_reads_the_dag_section() -> Result<()> {
        let sections = |dag: JsonValue| Sections::from([(DAG_SECTION.into(), dag)]);
        assert_eq!(worker_budget(&Sections::new())?, WorkerBudget::default());
        assert_eq!(
            worker_budget(&sections(json!({})))?,
            WorkerBudget::default()
        );
        assert_eq!(
            worker_budget(&sections(json!({
                "max_active_workers": 2,
                "max_active_workers_per_run": 1,
                "max_active_workers_per_box": 0,
                "min_available_memory": "1.5 GiB",
            })))?,
            WorkerBudget {
                max_active_workers: Some(2),
                max_active_workers_per_run: Some(1),
                max_active_workers_per_box: None,
                min_available_memory: Some(GIB + GIB / 2),
            }
        );
        for (size, bytes) in [
            (json!(1024), Some(1024)),
            (json!("512MiB"), Some(512 << 20)),
            (json!("2G"), Some(2 * GIB)),
            (json!(0), None),
        ] {
            assert_eq!(
                worker_budget(&sections(json!({ "min_available_memory": size })))?
                    .min_available_memory,
                bytes
            );
        }
        for invalid in [
            json!({ "max_active_workers": -1 }),
            json!({ "max_active_workers": "four" }),
            json!({ "min_available_memory": "4 parsecs" }),
            json!({ "max_workers": 2 }),
            json!(3),
        ] {
            assert!(
                worker_budget(&sections(invalid.clone())).is_err(),
                "{invalid}"
            );
        }
        Ok(())
    }

    #[test]
    fn lua_extends_and_overrides_builtin_profiles() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.lua");
        fs::write(
            &path,
            r#"
            agents.claude = { command = "/opt/claude", skip_subcommands = {} }
            agents.codex = { injection = { kind = "none" } }
            agents.custom = {
                injection = { kind = "config_value", flag = "-o", key = "instructions" },
                skip_subcommands = { "login" },
                skip_if_args_contain = { "instructions=" },
                skills_dir = "~/.custom/skills",
                model_callback = { kind = "args", args = { "--hook", "{command}" } },
            }
            agents.grok = { model_callback = { kind = "claude_status_line" } }
            agents.bare = { model_callback = { kind = "args", args = {} } }
            "#,
        )?;
        let profiles = agent_profiles(&load_from(&path, [(AGENTS_SECTION.into(), json!({}))])?)?;

        let claude = profiles.resolve(&"claude".into())?;
        assert_eq!(claude.command, "/opt/claude");
        assert_eq!(
            claude.injection,
            Injection::Flag {
                flag: "--append-system-prompt".into()
            }
        );
        assert!(claude.skip_subcommands.is_empty());
        assert_eq!(claude.skip_if_args_contain, ["--append-system-prompt"]);
        assert_eq!(claude.model_callback, ModelCallback::ClaudeStatusLine);
        assert_eq!(
            profiles.resolve(&"grok".into())?.model_callback,
            ModelCallback::ClaudeStatusLine
        );
        assert_eq!(
            profiles.resolve(&"bare".into())?.model_callback,
            ModelCallback::Args { args: Vec::new() }
        );
        assert_eq!(
            profiles.resolve(&"codex".into())?.injection,
            Injection::None
        );
        assert_eq!(
            profiles.resolve(&"grok".into())?.injection,
            Injection::Flag {
                flag: "--rules".into()
            }
        );

        let custom = profiles.resolve(&"custom".into())?;
        assert_eq!(custom.command, "custom");
        assert_eq!(
            custom.injection,
            Injection::ConfigValue {
                flag: "-o".into(),
                key: "instructions".into(),
            }
        );
        assert_eq!(custom.skip_subcommands, ["login"]);
        assert_eq!(custom.skip_if_args_contain, ["instructions="]);
        assert_eq!(custom.skills_dir, Some("~/.custom/skills".into()));
        assert_eq!(
            custom.model_callback,
            ModelCallback::Args {
                args: vec!["--hook".into(), "{command}".into()]
            }
        );
        assert_eq!(custom.resume, None);
        let error = profiles.resolve(&"unknown".into()).unwrap_err().to_string();
        assert_eq!(
            error,
            "Unknown agent profile \"unknown\". Known profiles: claude, codex, grok, jcode, bare, custom"
        );
        Ok(())
    }

    #[test]
    fn lua_resume_fields_override_builtin_lists_individually() -> Result<()> {
        let sections = evaluate(
            r#"
            agents.claude = { resume = { assign = {}, resume_last = { "--latest" } } }
            agents.codex = { resume = { resume_last = {}, conflicts = {} } }
            "#,
            Path::new("config.lua"),
            [(AGENTS_SECTION.into(), json!({}))].into(),
        )?;
        let profiles = agent_profiles(&sections)?;
        let defaults = AgentProfiles::default();
        let claude = profiles.resolve(&"claude".into())?;
        let mut expected = defaults.resolve(&claude.id)?.resume.clone().unwrap();
        expected.assign.clear();
        expected.resume_last = vec!["--latest".into()];
        assert_eq!(claude.resume, Some(expected));
        assert_eq!(
            profiles.resolve(&"codex".into())?.resume,
            Some(Resume::default())
        );
        Ok(())
    }

    #[test]
    fn lua_sets_first_message_injection_and_launch_env() -> Result<()> {
        let sections = evaluate(
            r#"
            agents.custom = {
                injection = { kind = "first_message" },
                env = { AGENT_SOCKET = "{runtime}/agent-{session}.sock" },
                conversation_env = "AGENT_SESSION",
                server_socket_env = "AGENT_SERVER",
            }
            agents.jcode = { env = {}, conversation_env = "", server_socket_env = "" }
            "#,
            Path::new("config.lua"),
            [(AGENTS_SECTION.into(), json!({}))].into(),
        )?;
        let profiles = agent_profiles(&sections)?;
        let custom = profiles.resolve(&"custom".into())?;
        assert_eq!(custom.injection, Injection::FirstMessage);
        assert_eq!(
            custom.env,
            [(
                "AGENT_SOCKET".into(),
                "{runtime}/agent-{session}.sock".into()
            )]
        );
        assert_eq!(custom.conversation_env.as_deref(), Some("AGENT_SESSION"));
        assert_eq!(custom.server_socket_env.as_deref(), Some("AGENT_SERVER"));
        let jcode = profiles.resolve(&"jcode".into())?;
        assert!(jcode.env.is_empty());
        assert_eq!(jcode.conversation_env, None);
        assert_eq!(jcode.server_socket_env, None);
        assert_eq!(jcode.injection, Injection::FirstMessage);

        let builtin = AgentProfiles::default();
        let jcode = builtin.resolve(&"jcode".into())?;
        assert!(jcode.env.is_empty());
        assert_eq!(jcode.server_socket_env.as_deref(), Some("JCODE_SOCKET"));
        assert_eq!(jcode.conversation_env.as_deref(), Some("JCODE_SESSION_ID"));
        Ok(())
    }

    #[test]
    fn lua_parses_resume_fields_for_a_custom_agent() -> Result<()> {
        let sections = evaluate(
            r#"
            agents.custom = {
                resume = {
                    assign = { "--id", "{id}" },
                    resume = { "reopen", "{id}" },
                    resume_last = { "reopen", "--last" },
                    conflicts = { "--id", "reopen" },
                },
            }
            "#,
            Path::new("config.lua"),
            [(AGENTS_SECTION.into(), json!({}))].into(),
        )?;
        let profiles = agent_profiles(&sections)?;
        assert_eq!(
            profiles.resolve(&"custom".into())?.resume,
            Some(Resume {
                assign: vec!["--id".into(), "{id}".into()],
                resume: vec!["reopen".into(), "{id}".into()],
                resume_last: vec!["reopen".into(), "--last".into()],
                conflicts: vec!["--id".into(), "reopen".into()],
            })
        );
        Ok(())
    }

    #[test]
    fn rejects_profile_names_that_cannot_be_wrapped() {
        for id in ["shell", "command", "power", "-flag", "a; echo surprise"] {
            let sections = [(AGENTS_SECTION.into(), json!({ (id): {} }))].into();
            let error = agent_profiles(&sections).err().unwrap();
            assert!(error.to_string().contains(id), "{error:#}");
        }
    }

    #[test]
    fn defaults_only_and_lua_overrides() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.lua");
        let defaults = || {
            [
                ("power_bar".into(), json!({ "left": [], "icons": true })),
                ("another_plugin".into(), json!({ "enabled": true })),
            ]
        };
        assert_eq!(
            load_from(&path, defaults())?["power_bar"]["left"],
            json!([])
        );

        fs::write(
            &path,
            r#"
            table.insert(power_bar.left, { text = "worktree" })
            another_plugin.enabled = true
            return { another_plugin = { enabled = false } }
            "#,
        )?;
        let config = load_from(&path, defaults())?;
        assert_eq!(config["power_bar"]["left"], json!([{ "text": "worktree" }]));
        assert_eq!(config["power_bar"]["icons"], true);
        assert_eq!(config["another_plugin"]["enabled"], false);
        Ok(())
    }

    #[test]
    fn errors_include_the_config_path() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.lua");
        for source in [
            "return }",
            "while true do end",
            "return { power_bar = function() end }",
        ] {
            fs::write(&path, source)?;
            let error = load_from(&path, [("power_bar".into(), json!({}))]).unwrap_err();
            assert!(format!("{error:#}").contains(path.to_str().unwrap()));
        }
        Ok(())
    }
}
