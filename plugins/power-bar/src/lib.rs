#![cfg(target_arch = "wasm32")]

use serde::Deserialize;
use std::{cell::RefCell, collections::BTreeMap, path::Path};

wit_bindgen::generate!({
    path: "../wit/terminal-ui.wit",
    world: "terminal-bottom-with-palette",
});

use power::terminal_ui::{
    palette,
    types::{ActionMenu, Choice, Overflow, Segment},
};

const DEFAULTS: &str = include_str!("../defaults.json");

struct PowerBar;
export!(PowerBar);

thread_local! {
    static CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
}

impl Guest for PowerBar {
    fn settings() -> Option<PluginSettings> {
        Some(PluginSettings {
            key: "power_bar".into(),
            defaults_json: DEFAULTS.into(),
        })
    }

    fn configure(config_json: String) -> Result<Option<String>, String> {
        let config = Config::read(&config_json)?;
        let clock = config.clock.clone();
        CONFIG.set(Some(config));
        Ok(clock)
    }

    fn render(
        view: ActiveView,
        fields: Vec<Field>,
        local_time: Option<String>,
    ) -> Result<Option<Row>, String> {
        CONFIG.with_borrow(|config| {
            let config = config
                .as_ref()
                .ok_or("Configure Power-bar before rendering")?;
            Ok(config
                .enabled
                .then(|| config.render(view, fields, local_time)))
        })
    }
}

#[derive(Deserialize)]
struct Preferences {
    enabled: bool,
    theme: String,
    icons: bool,
    separator: String,
    clock: Clock,
    params: BTreeMap<String, String>,
    agent_colors: BTreeMap<String, String>,
    left: Vec<SegmentTemplate>,
    right: Vec<SegmentTemplate>,
}

#[derive(Deserialize)]
struct Clock {
    format: String,
}

#[derive(Deserialize)]
struct SegmentTemplate {
    text: String,
    icon: Option<String>,
    fg: Option<String>,
    bg: Option<String>,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    italic: bool,
}

struct Config {
    enabled: bool,
    icons: bool,
    separator: String,
    params: BTreeMap<String, String>,
    agent_colors: BTreeMap<String, String>,
    clock: Option<String>,
    uses_palette: bool,
    left: Vec<Template>,
    right: Vec<Template>,
}

impl Config {
    fn read(settings: &str) -> Result<Self, String> {
        use serde_json::{Value, from_str, from_value};

        let defaults = from_str::<Value>(DEFAULTS).map_err(|error| error.to_string())?;
        let mut settings = from_str::<Value>(settings).map_err(|error| error.to_string())?;
        let settings = settings
            .as_object_mut()
            .ok_or("power_bar must be a table")?;
        for (key, value) in defaults.as_object().expect("default object") {
            settings.entry(key.clone()).or_insert_with(|| value.clone());
        }
        for side in ["left", "right"] {
            if settings[side]
                .as_object()
                .is_some_and(|empty| empty.is_empty())
            {
                settings.insert(side.into(), Value::Array(Vec::new()));
            }
        }
        let prefs = from_value::<Preferences>(Value::Object(std::mem::take(settings)))
            .map_err(|error| error.to_string())?;
        if prefs.theme != "terminal" {
            return Err(format!("Unknown Power-bar theme {:?}", prefs.theme));
        }
        if prefs.separator.len() > 32 || prefs.separator.chars().any(char::is_control) {
            return Err("Invalid Power-bar separator".into());
        }
        for (name, value) in &prefs.params {
            if !custom_name(name) || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(format!("Invalid custom field {name:?}"));
            }
        }
        let compile = |segments: Vec<SegmentTemplate>| {
            segments
                .into_iter()
                .map(Template::new)
                .collect::<Result<Vec<_>, _>>()
        };
        let left = compile(prefs.left)?;
        let right = compile(prefs.right)?;
        let uses_field = |name| {
            left.iter().chain(&right).any(|segment| {
                segment
                    .pieces
                    .iter()
                    .any(|piece| matches!(piece, Piece::Field(field) if field == name))
            })
        };
        Ok(Self {
            enabled: prefs.enabled,
            icons: prefs.icons,
            separator: prefs.separator,
            params: prefs.params,
            agent_colors: prefs.agent_colors,
            clock: (prefs.enabled && uses_field("time")).then_some(prefs.clock.format),
            uses_palette: uses_field("theme.name"),
            left,
            right,
        })
    }

    fn render(&self, view: ActiveView, custom: Vec<Field>, local_time: Option<String>) -> Row {
        let agent_color = self
            .agent_colors
            .get(&view.agent_type)
            .map_or("accent", String::as_str);
        let display_path = |path: &str| display_path(path, view.home.as_deref());
        let mut fields = BTreeMap::from([
            ("worktree.name".to_owned(), one_line(&view.worktree_name)),
            (
                "worktree.path".to_owned(),
                display_path(&view.worktree_path),
            ),
            ("agent.type".to_owned(), view.agent_type),
        ]);
        if self.uses_palette
            && let Some(palette) = palette::get_palette()
        {
            fields.insert("theme.name".into(), palette.scheme);
        }
        if let Some(path) = &view.repository_path {
            fields.insert("repo.path".into(), display_path(path));
            let name = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path);
            fields.insert("repo.name".into(), one_line(name));
        }
        for (name, value) in [
            ("agent.task", view.agent_task),
            ("agent.model", view.agent_model),
            ("time", local_time),
        ] {
            if let Some(value) = value {
                fields.insert(name.into(), value);
            }
        }
        fields.extend(custom.into_iter().map(|field| {
            let name = match field.name.as_str() {
                "@dag.progress" => "dag.progress".into(),
                name => format!("custom.{name}"),
            };
            (name, field.value)
        }));
        fields.extend(
            self.params
                .iter()
                .map(|(name, value)| (format!("custom.{name}"), value.clone())),
        );
        let resolve = |segments: &[Template]| {
            segments
                .iter()
                .filter_map(|segment| segment.resolve(&fields, self.icons, agent_color))
                .collect()
        };

        let mut choices = vec![Choice {
            title: "Worktree path".into(),
            detail: one_line(&view.worktree_path),
            copy_text: view.worktree_path,
        }];
        if let Some(path) = view.repository_path {
            choices.push(Choice {
                title: "Main checkout path".into(),
                detail: one_line(&path),
                copy_text: path,
            });
        }
        Row {
            separator: self.separator.clone(),
            left: resolve(&self.left),
            right: resolve(&self.right),
            actions: Some(ActionMenu {
                title: "Copy a path".into(),
                choices,
            }),
        }
    }
}

struct Template {
    segment: SegmentTemplate,
    pieces: Vec<Piece>,
    overflow: Overflow,
}

enum Piece {
    Literal(String),
    Field(String),
}

impl Template {
    fn new(segment: SegmentTemplate) -> Result<Self, String> {
        if segment.text.len() > 512
            || segment.text.chars().any(char::is_control)
            || segment
                .icon
                .as_ref()
                .is_some_and(|icon| icon.len() > 16 || icon.chars().any(char::is_control))
        {
            return Err("Invalid Power-bar segment".into());
        }
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut chars = segment.text.chars().peekable();
        let mut overflow = Overflow::Text;
        while let Some(ch) = chars.next() {
            match ch {
                '{' | '}' if chars.next_if_eq(&ch).is_some() => literal.push(ch),
                '{' => {
                    if !literal.is_empty() {
                        pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                    }
                    let mut field = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(ch) => field.push(ch),
                            None => return Err("Unclosed Power-bar field".into()),
                        }
                    }
                    match field.as_str() {
                        "worktree.name" | "repo.name" | "agent.type" => overflow = Overflow::Keep,
                        "worktree.path" | "repo.path" => {
                            if !matches!(overflow, Overflow::Keep) {
                                overflow = Overflow::Path;
                            }
                        }
                        "agent.task" | "agent.model" | "dag.progress" | "time" | "theme.name" => {}
                        value if value.strip_prefix("custom.").is_some_and(custom_name) => {}
                        _ => return Err(format!("Unknown Power-bar field {field:?}")),
                    }
                    pieces.push(Piece::Field(field));
                }
                '}' => return Err("Unexpected closing brace in Power-bar template".into()),
                _ => literal.push(ch),
            }
        }
        if !literal.is_empty() {
            pieces.push(Piece::Literal(literal));
        }
        Ok(Self {
            segment,
            pieces,
            overflow,
        })
    }

    fn resolve(
        &self,
        fields: &BTreeMap<String, String>,
        icons: bool,
        agent_color: &str,
    ) -> Option<Segment> {
        let mut text = String::new();
        if let Some(icon) = self.segment.icon.as_ref().filter(|_| icons) {
            text.push_str(icon);
            text.push(' ');
        }
        for piece in &self.pieces {
            match piece {
                Piece::Literal(literal) => text.push_str(literal),
                Piece::Field(name) => {
                    text.push_str(fields.get(name).filter(|value| !value.is_empty())?)
                }
            }
        }
        let color = |value: &Option<String>| {
            value.as_deref().map(|value| {
                if value == "agent" {
                    agent_color.to_owned()
                } else {
                    value.to_owned()
                }
            })
        };
        (!text.trim().is_empty()).then(|| Segment {
            text,
            fg: color(&self.segment.fg),
            bg: color(&self.segment.bg),
            bold: self.segment.bold,
            italic: self.segment.italic,
            overflow: self.overflow,
        })
    }
}

fn custom_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn display_path(path: &str, home: Option<&str>) -> String {
    if let Some(relative) = home.and_then(|home| Path::new(path).strip_prefix(home).ok()) {
        if relative.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", one_line(&relative.to_string_lossy()))
        }
    } else {
        one_line(path)
    }
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect()
}
