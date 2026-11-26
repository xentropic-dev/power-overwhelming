//! Configurable key bindings. One table names every bindable command with its default keys
//! and description; it drives config parsing, key matching, and the shortcut help.
//!
//! Users override bindings in `config.lua`, for example:
//!
//! ```lua
//! keys.tab.next = { "n", "j" }
//! keys.prefix.tab = "ctrl-a"
//! ```

use anyhow::{Context as _, Result, bail};
use gpui::Keystroke;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The config section the keymap reads.
pub const SECTION: &str = "keys";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    BoxPrefix,
    TabPrefix,
    BoxNew,
    BoxPick,
    BoxDetach,
    BoxColorScheme,
    BoxSetup,
    BoxRuns,
    BoxAgents,
    Reload,
    Quit,
    Help,
    TabNew,
    TabWorktree,
    TabPick,
    TabRecover,
    TabEnd,
    TabDetach,
    TabPaths,
    TabNext,
    TabPrevious,
    TabLast,
    PickerUp,
    PickerDown,
    PickerSelect,
    PickerCancel,
    PickerComplete,
    DialogUp,
    DialogDown,
    AgentCollapse,
    AgentExpand,
    DagUp,
    DagDown,
    DagLeft,
    DagRight,
    DagZoomIn,
    DagZoomOut,
    DagOpen,
    DagClose,
    TabOpenLink,
    TabPreview,
    PreviewPrevious,
    PreviewNext,
    PreviewClose,
    PreviewPlay,
    PreviewBack,
    PreviewForward,
    PreviewScrollDown,
    PreviewScrollUp,
    PreviewReload,
    PreviewInteract,
    PreviewExit,
    HintSearch,
    HintCancel,
}

/// Where a key press is interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    /// Any time no picker is open: the prefixes.
    Prefix,
    /// After the box prefix.
    Box,
    /// After the tab prefix.
    Tab,
    /// In every picker.
    Picker,
    /// In pickers without a text field, in addition to `Picker`.
    Dialog,
    /// In the searchable agent tree, in addition to `Picker`.
    AgentTree,
    /// On the task graph.
    Dag,
    /// While the preview pane has focus.
    Preview,
    /// While links on screen show number hints. Digits 1–9 always open a hint.
    Hints,
}

impl Scope {
    fn section(self) -> &'static str {
        match self {
            Self::Prefix => "prefix",
            Self::Box => "box",
            Self::Tab => "tab",
            Self::Picker => "picker",
            Self::Dialog => "dialog",
            Self::AgentTree => "agent_tree",
            Self::Dag => "dag",
            Self::Preview => "preview",
            Self::Hints => "hints",
        }
    }
}

struct Entry {
    scope: Scope,
    name: &'static str,
    command: Command,
    keys: &'static [&'static str],
    description: &'static str,
}

const fn entry(
    scope: Scope,
    name: &'static str,
    command: Command,
    keys: &'static [&'static str],
    description: &'static str,
) -> Entry {
    Entry {
        scope,
        name,
        command,
        keys,
        description,
    }
}

use Command as C;
use Scope as S;

pub struct Shortcut {
    pub description: &'static str,
    pub binding: String,
}

/// Every bindable command. The defaults follow the tmux-style map in the planning docs.
#[rustfmt::skip]
const TABLE: &[Entry] = &[
    entry(S::Prefix, "box", C::BoxPrefix, &["ctrl-b"], "Box prefix"),
    entry(S::Prefix, "tab", C::TabPrefix, &["ctrl-s"], "Tab prefix"),
    entry(S::Box, "new", C::BoxNew, &["c"], "New box"),
    entry(S::Box, "pick", C::BoxPick, &["s"], "Pick or reopen a box"),
    entry(S::Box, "detach", C::BoxDetach, &["x"], "Detach box"),
    entry(S::Box, "runs", C::BoxRuns, &["d"], "Open DAG runs"),
    entry(S::Box, "agents", C::BoxAgents, &["a"], "Open agent tree"),
    entry(S::Box, "reload", C::Reload, &["r"], "Reload configuration"),
    entry(S::Box, "color_scheme", C::BoxColorScheme, &["t"], "Choose color scheme"),
    entry(S::Box, "setup", C::BoxSetup, &["i"], "Shell integration setup"),
    entry(S::Box, "quit", C::Quit, &["q"], "Quit"),
    entry(S::Box, "help", C::Help, &["?"], "Keyboard shortcuts"),
    entry(S::Tab, "new", C::TabNew, &["c"], "New tab"),
    entry(
        S::Tab,
        "worktree",
        C::TabWorktree,
        &["t"],
        "New tab in another worktree",
    ),
    entry(S::Tab, "pick", C::TabPick, &["w"], "Pick tab"),
    entry(S::Tab, "recover", C::TabRecover, &["r"], "Recover session"),
    entry(S::Tab, "end", C::TabEnd, &["x"], "End session"),
    entry(S::Tab, "detach", C::TabDetach, &["d"], "Detach tab and keep session running"),
    entry(
        S::Tab,
        "paths",
        C::TabPaths,
        &["y"],
        "Open status bar actions",
    ),
    entry(S::Tab, "next", C::TabNext, &["n"], "Next tab"),
    entry(S::Tab, "previous", C::TabPrevious, &["p"], "Previous tab"),
    entry(S::Tab, "last", C::TabLast, &["l"], "Previously active tab"),
    entry(S::Tab, "open_link", C::TabOpenLink, &["o"], "Number the links on screen"),
    entry(S::Tab, "preview", C::TabPreview, &["v"], "Focus the preview pane"),
    entry(S::Tab, "help", C::Help, &["?"], "Keyboard shortcuts"),
    entry(
        S::Picker,
        "up",
        C::PickerUp,
        &["up", "ctrl-k", "ctrl-p"],
        "Move up",
    ),
    entry(
        S::Picker,
        "down",
        C::PickerDown,
        &["down", "ctrl-j", "ctrl-n"],
        "Move down",
    ),
    entry(S::Picker, "select", C::PickerSelect, &["enter"], "Choose"),
    entry(S::Picker, "cancel", C::PickerCancel, &["escape"], "Close"),
    entry(
        S::Picker,
        "complete",
        C::PickerComplete,
        &["tab"],
        "Complete directory",
    ),
    entry(S::Dialog, "up", C::DialogUp, &["k", "h", "left"], "Move up"),
    entry(
        S::Dialog,
        "down",
        C::DialogDown,
        &["j", "l", "right"],
        "Move down",
    ),
    entry(S::AgentTree, "collapse", C::AgentCollapse, &["ctrl-h"], "Collapse branch or select parent"),
    entry(S::AgentTree, "expand", C::AgentExpand, &["ctrl-l"], "Expand branch or select child"),
    entry(S::Dag, "up", C::DagUp, &["up", "k"], "Select task above"),
    entry(S::Dag, "down", C::DagDown, &["down", "j"], "Select task below"),
    entry(S::Dag, "left", C::DagLeft, &["left", "h"], "Select task to the left"),
    entry(S::Dag, "right", C::DagRight, &["right", "l"], "Select task to the right"),
    entry(S::Dag, "zoom_in", C::DagZoomIn, &["+"], "Zoom in"),
    entry(S::Dag, "zoom_out", C::DagZoomOut, &["-"], "Zoom out"),
    entry(S::Dag, "open", C::DagOpen, &["enter", "o"], "Open assigned worker terminal"),
    entry(S::Dag, "close", C::DagClose, &["escape"], "Close task graph"),
    entry(S::Preview, "previous", C::PreviewPrevious, &["h", "left"], "Previous preview"),
    entry(S::Preview, "next", C::PreviewNext, &["l", "right"], "Next preview"),
    entry(S::Preview, "close", C::PreviewClose, &["escape", "x"], "Close preview"),
    entry(S::Preview, "play", C::PreviewPlay, &["space"], "Play or pause video"),
    entry(S::Preview, "back", C::PreviewBack, &[","], "Back 5 seconds"),
    entry(S::Preview, "forward", C::PreviewForward, &["."], "Forward 5 seconds"),
    entry(S::Preview, "scroll_down", C::PreviewScrollDown, &["j", "down"], "Scroll page down"),
    entry(S::Preview, "scroll_up", C::PreviewScrollUp, &["k", "up"], "Scroll page up"),
    entry(S::Preview, "reload", C::PreviewReload, &["r"], "Reload page"),
    entry(S::Preview, "interact", C::PreviewInteract, &["i"], "Type into the page"),
    entry(S::Preview, "exit", C::PreviewExit, &["tab"], "Back to the terminal"),
    entry(S::Hints, "search", C::HintSearch, &["s"], "Search links and published artifacts"),
    entry(S::Hints, "cancel", C::HintCancel, &["escape"], "Hide the numbers"),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Key {
    key: String,
    control: bool,
    alt: bool,
    shift: bool,
    platform: bool,
}

impl Key {
    pub fn parse(text: &str) -> Result<Self> {
        let mut key = Self {
            key: String::new(),
            control: false,
            alt: false,
            shift: false,
            platform: false,
        };
        // A trailing "-" is the minus key itself, as in "ctrl--".
        let (modifiers, name) = match text.strip_suffix('-') {
            Some(rest) if rest.is_empty() || rest.ends_with('-') => {
                (rest.trim_end_matches('-'), "-")
            }
            _ => text.rsplit_once('-').unwrap_or(("", text)),
        };
        for modifier in modifiers.split('-').filter(|part| !part.is_empty()) {
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => key.control = true,
                "alt" => key.alt = true,
                "shift" => key.shift = true,
                "super" | "cmd" | "platform" => key.platform = true,
                other => bail!("Unknown modifier {other:?} in key {text:?}"),
            }
        }
        if name.is_empty() {
            bail!("Key {text:?} has no key name");
        }
        key.key = match name.to_ascii_lowercase().as_str() {
            "esc" => "escape".into(),
            "return" => "enter".into(),
            "spacebar" => "space".into(),
            _ if name.chars().count() == 1 => name.into(),
            lower => lower.into(),
        };
        Ok(key)
    }

    pub fn matches(&self, stroke: &Keystroke) -> bool {
        let mods = stroke.modifiers;
        if mods.control != self.control || mods.alt != self.alt || mods.platform != self.platform {
            return false;
        }
        // Symbols such as "?" arrive as shifted keys, so match the typed character directly.
        if !self.control && !self.alt && stroke.key_char.as_deref() == Some(self.key.as_str()) {
            return true;
        }
        mods.shift == self.shift && stroke.key.eq_ignore_ascii_case(&self.key)
    }

    /// A label such as `Ctrl+B`, `↑` or `?`.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        for (on, name) in [
            (self.control, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.platform, "Super"),
        ] {
            if on {
                parts.push(name.to_owned());
            }
        }
        let name = match self.key.as_str() {
            "up" => "↑".to_owned(),
            "down" => "↓".to_owned(),
            "left" => "←".to_owned(),
            "right" => "→".to_owned(),
            "enter" => "Enter".to_owned(),
            "escape" => "Esc".to_owned(),
            "tab" => "Tab".to_owned(),
            "space" => "Space".to_owned(),
            key if self.control || self.alt || self.platform => key.to_uppercase(),
            key => key.to_owned(),
        };
        parts.push(name);
        parts.join("+")
    }
}

#[derive(Clone, Debug)]
pub struct Keymap {
    bindings: Vec<(Scope, Command, Vec<Key>)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::from_config(&defaults()).expect("default key bindings are valid")
    }
}

/// The default `keys` section, used to seed `config.lua`.
pub fn defaults() -> Value {
    let mut sections = Map::new();
    for entry in TABLE {
        let section = sections
            .entry(entry.scope.section())
            .or_insert_with(|| json!({}));
        section[entry.name] = json!(entry.keys);
    }
    Value::Object(sections)
}

impl Keymap {
    pub fn from_sections(config: &infrastructure::config::Sections) -> Result<Self> {
        Self::from_config(config.get(SECTION).unwrap_or(&Value::Null))
            .context("Read keys from config.lua")
    }

    /// Resolve a `keys` value. Missing entries keep their defaults; unknown ones are errors.
    pub fn from_config(value: &Value) -> Result<Self> {
        let empty = Map::new();
        let sections = match value {
            Value::Null => &empty,
            Value::Object(sections) => sections,
            _ => bail!("keys must be a table"),
        };
        for (section, names) in sections {
            let Some(names) = names.as_object() else {
                bail!("keys.{section} must be a table");
            };
            for name in names.keys() {
                if !TABLE
                    .iter()
                    .any(|entry| entry.scope.section() == section && entry.name == name)
                {
                    bail!("keys.{section}.{name} is not a command");
                }
            }
        }
        let mut bindings = Vec::new();
        let mut seen = BTreeMap::new();
        for entry in TABLE {
            let path = format!("keys.{}.{}", entry.scope.section(), entry.name);
            let keys: Result<Vec<_>> = match sections
                .get(entry.scope.section())
                .and_then(|section| section.get(entry.name))
            {
                None => entry.keys.iter().map(|key| Key::parse(key)).collect(),
                Some(Value::String(key)) => vec![Key::parse(key)].into_iter().collect(),
                Some(Value::Array(keys)) => keys
                    .iter()
                    .map(|key| match key {
                        Value::String(key) => Key::parse(key),
                        _ => bail!("must list key names"),
                    })
                    .collect(),
                Some(_) => bail!("{path} must be a key name or a list of key names"),
            };
            let keys = keys.with_context(|| path.clone())?;
            for key in &keys {
                let scopes: &[Scope] = match entry.scope {
                    Scope::Picker => &[Scope::Picker, Scope::Dialog, Scope::AgentTree],
                    _ => std::slice::from_ref(&entry.scope),
                };
                for scope in scopes {
                    if let Some((other, command)) = seen.insert(
                        (scope.section(), key.label()),
                        (path.clone(), entry.command),
                    ) && command != entry.command
                    {
                        bail!("{} is bound to both {other} and {path}", key.label());
                    }
                }
            }
            bindings.push((entry.scope, entry.command, keys));
        }
        Ok(Self { bindings })
    }

    /// The command a key press runs in `scope`, including shared picker bindings.
    pub fn command(&self, scope: Scope, stroke: &Keystroke) -> Option<Command> {
        let find = |scope: Scope| {
            self.bindings
                .iter()
                .filter(|(entry, ..)| *entry == scope)
                .find(|(.., keys)| keys.iter().any(|key| key.matches(stroke)))
                .map(|(_, command, _)| *command)
        };
        match scope {
            Scope::Dialog | Scope::AgentTree => find(Scope::Picker).or_else(|| find(scope)),
            scope => find(scope),
        }
    }

    /// Labels for a command's keys, such as `j/↓`.
    pub fn keys(&self, scope: Scope, command: Command) -> String {
        self.bindings
            .iter()
            .filter(|(entry, bound, _)| *entry == scope && *bound == command)
            .flat_map(|(.., keys)| keys.iter().map(Key::label))
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The label of a command's first key, for short hints.
    pub fn first(&self, scope: Scope, command: Command) -> String {
        self.bindings
            .iter()
            .find(|(entry, bound, _)| *entry == scope && *bound == command)
            .and_then(|(.., keys)| keys.first())
            .map_or_else(|| "(unbound)".to_owned(), Key::label)
    }

    /// Shortcut help generated from the current bindings.
    pub fn help(&self) -> Vec<Shortcut> {
        let prefix = |command| self.keys(Scope::Prefix, command);
        let (box_prefix, tab_prefix) = (prefix(C::BoxPrefix), prefix(C::TabPrefix));
        let open_links = self.keys(Scope::Tab, C::TabOpenLink);
        let hints = format!("{tab_prefix} then {open_links}, then");
        let mut shortcuts = Vec::new();
        let mut add = |description, binding| {
            shortcuts.push(Shortcut {
                description,
                binding,
            })
        };
        if !box_prefix.is_empty() {
            add("Switch box", format!("{box_prefix} then 1–9, 0"));
        }
        if !tab_prefix.is_empty() {
            add("Select tab", format!("{tab_prefix} then 1–9"));
        }
        if !tab_prefix.is_empty() && !open_links.is_empty() {
            add("Open the numbered link", format!("{hints} 1–9"));
        }
        for entry in TABLE {
            let keys = self.keys(entry.scope, entry.command);
            if keys.is_empty() {
                continue;
            }
            let binding = match entry.scope {
                Scope::Box if !box_prefix.is_empty() => format!("{box_prefix} then {keys}"),
                Scope::Tab if !tab_prefix.is_empty() => format!("{tab_prefix} then {keys}"),
                Scope::Picker => format!("In pickers: {keys}"),
                Scope::Dialog => format!("In dialogs without search: {keys}"),
                Scope::AgentTree => format!("In the agent tree: {keys}"),
                Scope::Dag => format!("In the task graph: {keys}"),
                Scope::Preview => format!("In the preview pane: {keys}"),
                Scope::Hints if !tab_prefix.is_empty() && !open_links.is_empty() => {
                    format!("{hints} {keys}")
                }
                _ => continue,
            };
            add(entry.description, binding);
        }
        shortcuts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(text: &str) -> Keystroke {
        Keystroke::parse(text).unwrap()
    }

    #[test]
    fn defaults_follow_the_documented_map() {
        let keymap = Keymap::default();
        assert_eq!(
            keymap.command(Scope::Prefix, &stroke("ctrl-s")),
            Some(C::TabPrefix)
        );
        assert_eq!(keymap.command(Scope::Tab, &stroke("l")), Some(C::TabLast));
        assert_eq!(keymap.command(Scope::Picker, &stroke("j")), None);
        assert_eq!(
            keymap.command(Scope::Dialog, &stroke("j")),
            Some(C::DialogDown)
        );
        assert_eq!(
            keymap.command(Scope::Dialog, &stroke("down")),
            Some(C::PickerDown)
        );
        assert_eq!(keymap.command(Scope::Prefix, &stroke("ctrl-shift-s")), None);
        for (key, command) in [
            ("escape", C::PreviewClose),
            ("x", C::PreviewClose),
            ("tab", C::PreviewExit),
        ] {
            assert_eq!(keymap.command(Scope::Preview, &stroke(key)), Some(command));
        }
    }

    #[test]
    fn orchestration_bindings_are_scoped_and_listed_in_help() {
        let keymap = Keymap::default();
        for (scope, key, command) in [
            (Scope::Box, "d", C::BoxRuns),
            (Scope::Box, "a", C::BoxAgents),
            (Scope::Tab, "d", C::TabDetach),
            (Scope::AgentTree, "ctrl-h", C::AgentCollapse),
            (Scope::AgentTree, "ctrl-l", C::AgentExpand),
            (Scope::AgentTree, "enter", C::PickerSelect),
            (Scope::AgentTree, "escape", C::PickerCancel),
            (Scope::Dag, "up", C::DagUp),
            (Scope::Dag, "k", C::DagUp),
            (Scope::Dag, "down", C::DagDown),
            (Scope::Dag, "j", C::DagDown),
            (Scope::Dag, "left", C::DagLeft),
            (Scope::Dag, "h", C::DagLeft),
            (Scope::Dag, "right", C::DagRight),
            (Scope::Dag, "l", C::DagRight),
            (Scope::Dag, "+", C::DagZoomIn),
            (Scope::Dag, "-", C::DagZoomOut),
            (Scope::Dag, "enter", C::DagOpen),
            (Scope::Dag, "o", C::DagOpen),
            (Scope::Dag, "escape", C::DagClose),
        ] {
            assert_eq!(keymap.command(scope, &stroke(key)), Some(command));
        }
        assert_eq!(keymap.command(Scope::AgentTree, &stroke("h")), None);
        assert_eq!(keymap.command(Scope::AgentTree, &stroke("left")), None);
        let help = keymap.help();
        for (description, binding) in [
            ("Open DAG runs", "Ctrl+B then d"),
            ("Open agent tree", "Ctrl+B then a"),
            ("Detach tab and keep session running", "Ctrl+S then d"),
            (
                "Open assigned worker terminal",
                "In the task graph: Enter/o",
            ),
        ] {
            assert!(
                help.iter()
                    .any(|row| row.description == description && row.binding == binding)
            );
        }
    }

    #[test]
    fn config_overrides_merge_over_defaults() {
        let keymap = Keymap::from_config(&json!({
            "tab": { "next": ["n", "j"] },
            "box": { "runs": "g" },
            "dag": { "open": "space" },
            "agent_tree": { "expand": "ctrl-f" },
            "prefix": { "tab": "ctrl-a" },
        }))
        .unwrap();
        assert_eq!(keymap.command(Scope::Tab, &stroke("j")), Some(C::TabNext));
        assert_eq!(
            keymap.command(Scope::Tab, &stroke("p")),
            Some(C::TabPrevious)
        );
        assert_eq!(
            keymap.command(Scope::Prefix, &stroke("ctrl-a")),
            Some(C::TabPrefix)
        );
        assert_eq!(keymap.command(Scope::Prefix, &stroke("ctrl-s")), None);
        assert_eq!(keymap.command(Scope::Box, &stroke("g")), Some(C::BoxRuns));
        assert_eq!(
            keymap.command(Scope::Dag, &stroke("space")),
            Some(C::DagOpen)
        );
        assert_eq!(keymap.command(Scope::Dag, &stroke("o")), None);
        assert_eq!(
            keymap.command(Scope::AgentTree, &stroke("ctrl-f")),
            Some(C::AgentExpand)
        );
        assert!(
            keymap
                .help()
                .iter()
                .any(|row| row.description == "Next tab" && row.binding == "Ctrl+A then n/j")
        );
    }

    #[test]
    fn rejects_typos_and_conflicts() {
        for (config, message) in [
            (
                json!({ "tab": { "nxt": "j" } }),
                "keys.tab.nxt is not a command",
            ),
            (json!({ "tab": { "next": "p" } }), "bound to both"),
            (
                json!({ "agent_tree": { "expand": "down" } }),
                "bound to both",
            ),
            (json!({ "tab": { "next": "hyper-j" } }), "Unknown modifier"),
            (json!({ "tab": { "next": 3 } }), "must be a key name"),
        ] {
            let error = format!("{:#}", Keymap::from_config(&config).unwrap_err());
            assert!(error.contains(message), "{error}");
        }
    }

    #[test]
    fn labels_and_symbols() {
        let help = Key::parse("?").unwrap();
        // Typing "?" arrives as shifted "/" with the typed character attached.
        let typed = Keystroke {
            modifiers: gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            key: "/".into(),
            key_char: Some("?".into()),
        };
        assert!(help.matches(&typed));
        assert!(!Key::parse("k").unwrap().matches(&stroke("shift-k")));
        assert_eq!(Key::parse("ctrl-b").unwrap().label(), "Ctrl+B");
        assert_eq!(Key::parse("ctrl--").unwrap().key, "-");
    }
}
