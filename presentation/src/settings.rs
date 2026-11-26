//! Desktop settings from `config.lua`: key bindings and preview behaviour.
//!
//! ```lua
//! previews.auto_open = "always"   -- "when_pane_open" (default) or "never"
//! previews.history = 20           -- artifacts listed by the link search
//! ```

use crate::keymap::{self, Keymap};
use anyhow::{Context as _, Result, bail};
use infrastructure::theme::{self, Scheme};
use infrastructure::{
    agents::AgentProfiles,
    config::{AGENTS_SECTION, Sections, agent_profiles},
};
use serde_json::{Value, json};

const PREVIEWS: &str = "previews";
const APPEARANCE: &str = "appearance";

/// When a newly published artifact opens in the preview pane on its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoOpen {
    /// Open it, even if the box has no preview showing.
    Always,
    /// Open it only when the box's preview pane is already showing.
    WhenPaneOpen,
    Never,
}

#[derive(Clone, Debug)]
pub struct Previews {
    pub auto_open: AutoOpen,
    /// How many published artifacts the link search lists.
    pub history: usize,
}

impl Default for Previews {
    fn default() -> Self {
        Self::from_config(&previews_defaults()).expect("default preview settings are valid")
    }
}

fn previews_defaults() -> Value {
    json!({ "auto_open": "when_pane_open", "history": 20 })
}

impl Previews {
    fn from_config(value: &Value) -> Result<Self> {
        let Some(table) = value.as_object() else {
            bail!("previews must be a table");
        };
        let defaults = previews_defaults();
        for name in table.keys() {
            if defaults.get(name).is_none() {
                bail!("previews.{name} is not a setting");
            }
        }
        let field = |name: &str| table.get(name).unwrap_or(&defaults[name]);
        let auto_open = match field("auto_open").as_str() {
            Some("always") => AutoOpen::Always,
            Some("when_pane_open") => AutoOpen::WhenPaneOpen,
            Some("never") => AutoOpen::Never,
            _ => bail!(r#"previews.auto_open must be "always", "when_pane_open", or "never""#),
        };
        let history = field("history")
            .as_u64()
            .filter(|count| (1..=500).contains(count))
            .context("previews.history must be a whole number from 1 to 500")?;
        Ok(Self {
            auto_open,
            history: history as usize,
        })
    }
}

#[derive(Clone, Default)]
pub struct Settings {
    pub keymap: Keymap,
    pub previews: Previews,
    pub scheme: Option<&'static Scheme>,
    pub agents: AgentProfiles,
}

impl Settings {
    /// The desktop's config sections and their defaults, for the shared config loader.
    pub fn defaults() -> Sections {
        [
            (keymap::SECTION.to_owned(), keymap::defaults()),
            (PREVIEWS.to_owned(), previews_defaults()),
            (APPEARANCE.to_owned(), json!({})),
            (AGENTS_SECTION.to_owned(), json!({})),
        ]
        .into()
    }

    /// Resolves the desktop sections of an evaluated config over their defaults.
    pub fn from_sections(config: &Sections) -> Result<Self> {
        Ok(Self {
            keymap: Keymap::from_sections(config).context("Read keys from config.lua")?,
            previews: match config.get(PREVIEWS) {
                None | Some(Value::Null) => Previews::default(),
                Some(value) => {
                    Previews::from_config(value).context("Read previews from config.lua")?
                }
            },
            scheme: read_scheme(config.get(APPEARANCE))?,
            agents: agent_profiles(config)?,
        })
    }
}

fn read_scheme(value: Option<&Value>) -> Result<Option<&'static Scheme>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let table = value.as_object().context("appearance must be a table")?;
    for name in table.keys() {
        if name != "scheme" {
            bail!("appearance.{name} is not a setting");
        }
    }
    let Some(value) = table.get("scheme").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let name = value
        .as_str()
        .context("appearance.scheme must be a color scheme name")?;
    theme::find(name)
        .map(Some)
        .with_context(|| format!("Unknown appearance.scheme {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_scheme_resolves_names_and_rejects_invalid_config() -> Result<()> {
        use infrastructure::config::load_from;
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("config.lua");
        std::fs::write(&config, r#"appearance.scheme = "Light 2026""#)?;
        let settings = Settings::from_sections(&load_from(&config, Settings::defaults())?)?;
        assert_eq!(settings.scheme.unwrap().name, theme::DEFAULT_LIGHT);
        let defaults = Settings::from_sections(&Settings::defaults())?;
        assert!(defaults.scheme.is_none());
        std::fs::write(&config, r#"appearance.scheme = "unknown""#)?;
        assert!(Settings::from_sections(&load_from(&config, Settings::defaults())?).is_err());
        Ok(())
    }

    #[test]
    fn preview_settings() {
        let previews = Previews::from_config(&json!({ "auto_open": "always" })).unwrap();
        assert_eq!(previews.auto_open, AutoOpen::Always);
        assert_eq!(previews.history, 20);
        assert_eq!(Previews::default().auto_open, AutoOpen::WhenPaneOpen);
        for bad in [
            json!({ "auto_open": "sometimes" }),
            json!({ "history": 0 }),
            json!({ "histroy": 5 }),
        ] {
            assert!(Previews::from_config(&bad).is_err(), "{bad}");
        }
    }
}
