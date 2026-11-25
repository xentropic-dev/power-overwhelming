//! Load the Lua configuration into values that can be passed to plugin runtimes.

use anyhow::{Context, Result, bail};
use mlua::{HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Value};
use serde_json::Value as JsonValue;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

pub type Sections = BTreeMap<String, JsonValue>;

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
    use serde_json::json;

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
