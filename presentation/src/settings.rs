//! Desktop settings from `config.lua`: key bindings, preview behaviour and the terminal.
//!
//! ```lua
//! previews.auto_open = "always"   -- "when_pane_open" (default) or "never"
//! previews.history = 20           -- artifacts listed by the link search
//! terminal.font_family = "JetBrains Mono"
//! terminal.font_size = 14         -- logical pixels; the zoom keys change it per session
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
const TERMINAL: &str = "terminal";

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

/// Whether programs may set the clipboard with OSC 52. Reading it is never allowed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardWrite {
    Allow,
    Deny,
}

/// The `terminal` table: font, grid and behaviour of terminal views.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalSettings {
    /// A font family name; unset picks JetBrains Mono when installed, else `monospace`.
    pub font_family: Option<String>,
    /// In logical pixels, 6–72.
    pub font_size: f32,
    /// CSS weights, 100–900.
    pub font_weight: u16,
    pub font_weight_bold: u16,
    /// Cell height and width multipliers, 0.8–2.0, as Ghostty's `adjust-cell-*`.
    pub line_height: f32,
    pub cell_width: f32,
    /// Space around the grid in logical pixels, 0–64.
    pub padding: f32,
    pub ligatures: bool,
    /// Wheel scroll multiplier, 0.1–5.
    pub scroll_speed: f32,
    pub copy_on_select: bool,
    pub clipboard_write: ClipboardWrite,
    /// WCAG contrast ratio text keeps against its background, 1 (off) to 21.
    pub minimum_contrast: f32,
    /// Opacity of the default background, 0–1.
    pub opacity: f32,
}

pub const MIN_FONT_SIZE: f32 = 6.;
pub const MAX_FONT_SIZE: f32 = 72.;

impl Default for TerminalSettings {
    fn default() -> Self {
        Self::from_config(&terminal_defaults()).expect("default terminal settings are valid")
    }
}

/// Every `terminal` setting. `font_family` has no default value, so it is listed apart.
const TERMINAL_KEYS: &[&str] = &[
    "font_family",
    "font_size",
    "font_weight",
    "font_weight_bold",
    "line_height",
    "cell_width",
    "padding",
    "ligatures",
    "scroll_speed",
    "copy_on_select",
    "clipboard_write",
    "minimum_contrast",
    "opacity",
];

fn terminal_defaults() -> Value {
    json!({
        "font_size": 13,
        "font_weight": 400,
        "font_weight_bold": 700,
        "line_height": 1.0,
        "cell_width": 1.0,
        "padding": 6,
        "ligatures": false,
        "scroll_speed": 1.0,
        "copy_on_select": false,
        "clipboard_write": "allow",
        "minimum_contrast": 1.0,
        "opacity": 1.0,
    })
}

impl TerminalSettings {
    fn from_config(value: &Value) -> Result<Self> {
        let Some(table) = value.as_object() else {
            bail!("terminal must be a table");
        };
        for name in table.keys() {
            if !TERMINAL_KEYS.contains(&name.as_str()) {
                bail!("terminal.{name} is not a setting");
            }
        }
        let defaults = terminal_defaults();
        let field = |name: &str| table.get(name).unwrap_or(&defaults[name]);
        let number = |name: &str, low: f32, high: f32| {
            field(name)
                .as_f64()
                .map(|value| value as f32)
                .filter(|value| (low..=high).contains(value))
                .with_context(|| format!("terminal.{name} must be a number from {low} to {high}"))
        };
        let weight = |name: &str| {
            field(name)
                .as_u64()
                .filter(|value| (100..=900).contains(value))
                .map(|value| value as u16)
                .with_context(|| format!("terminal.{name} must be a whole number from 100 to 900"))
        };
        let flag = |name: &str| {
            field(name)
                .as_bool()
                .with_context(|| format!("terminal.{name} must be true or false"))
        };
        let font_family = match table.get("font_family") {
            None | Some(Value::Null) => None,
            Some(Value::String(name)) if !name.trim().is_empty() => Some(name.trim().to_owned()),
            Some(_) => bail!("terminal.font_family must be a font family name"),
        };
        let clipboard_write = match field("clipboard_write").as_str() {
            Some("allow") => ClipboardWrite::Allow,
            Some("deny") => ClipboardWrite::Deny,
            _ => bail!(r#"terminal.clipboard_write must be "allow" or "deny""#),
        };
        Ok(Self {
            font_family,
            font_size: number("font_size", MIN_FONT_SIZE, MAX_FONT_SIZE)?,
            font_weight: weight("font_weight")?,
            font_weight_bold: weight("font_weight_bold")?,
            line_height: number("line_height", 0.8, 2.)?,
            cell_width: number("cell_width", 0.8, 2.)?,
            padding: number("padding", 0., 64.)?,
            ligatures: flag("ligatures")?,
            scroll_speed: number("scroll_speed", 0.1, 5.)?,
            copy_on_select: flag("copy_on_select")?,
            clipboard_write,
            minimum_contrast: number("minimum_contrast", 1., 21.)?,
            opacity: number("opacity", 0., 1.)?,
        })
    }
}

#[derive(Clone, Default)]
pub struct Settings {
    pub keymap: Keymap,
    pub previews: Previews,
    pub terminal: TerminalSettings,
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
            (TERMINAL.to_owned(), terminal_defaults()),
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
            terminal: match config.get(TERMINAL) {
                None | Some(Value::Null) => TerminalSettings::default(),
                Some(value) => {
                    TerminalSettings::from_config(value).context("Read terminal from config.lua")?
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

    #[test]
    fn terminal_settings_from_config_lua() -> Result<()> {
        use infrastructure::config::load_from;
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("config.lua");
        std::fs::write(
            &config,
            r#"
terminal.font_family = "Iosevka Term"
terminal.font_size = 15.5
terminal.line_height = 1.2
terminal.ligatures = true
terminal.clipboard_write = "deny"
"#,
        )?;
        let terminal =
            Settings::from_sections(&load_from(&config, Settings::defaults())?)?.terminal;
        assert_eq!(terminal.font_family.as_deref(), Some("Iosevka Term"));
        assert_eq!(terminal.font_size, 15.5);
        assert_eq!(terminal.line_height, 1.2);
        assert!(terminal.ligatures);
        assert_eq!(terminal.clipboard_write, ClipboardWrite::Deny);
        assert_eq!(terminal.font_weight, 400);
        assert_eq!(terminal.padding, 6.);

        // Replacing the whole table keeps the defaults for what it leaves out.
        std::fs::write(&config, "terminal = { cell_width = 1.1 }")?;
        let terminal =
            Settings::from_sections(&load_from(&config, Settings::defaults())?)?.terminal;
        assert_eq!(terminal.cell_width, 1.1);
        assert_eq!(terminal.font_size, 13.);
        assert_eq!(terminal.font_family, None);

        let defaults = Settings::from_sections(&Settings::defaults())?.terminal;
        assert_eq!(defaults, TerminalSettings::default());
        assert!(!defaults.copy_on_select);
        assert_eq!(defaults.clipboard_write, ClipboardWrite::Allow);
        Ok(())
    }

    #[test]
    fn terminal_settings_reject_unknown_keys_and_bad_values() {
        for (bad, message) in [
            (
                json!({ "font_sise": 14 }),
                "terminal.font_sise is not a setting",
            ),
            (json!({ "font_size": 5 }), "terminal.font_size"),
            (json!({ "font_size": "big" }), "terminal.font_size"),
            (json!({ "line_height": 2.5 }), "terminal.line_height"),
            (json!({ "cell_width": 0.5 }), "terminal.cell_width"),
            (json!({ "font_weight": 950 }), "terminal.font_weight"),
            (
                json!({ "font_weight_bold": 700.5 }),
                "terminal.font_weight_bold",
            ),
            (json!({ "padding": -1 }), "terminal.padding"),
            (json!({ "ligatures": "yes" }), "terminal.ligatures"),
            (
                json!({ "clipboard_write": "ask" }),
                "terminal.clipboard_write",
            ),
            (
                json!({ "minimum_contrast": 0.5 }),
                "terminal.minimum_contrast",
            ),
            (json!({ "opacity": 1.5 }), "terminal.opacity"),
            (json!({ "font_family": 3 }), "terminal.font_family"),
            (json!(4), "terminal must be a table"),
        ] {
            let error = format!("{:#}", TerminalSettings::from_config(&bad).unwrap_err());
            assert!(error.contains(message), "{bad}: {error}");
        }
    }
}
