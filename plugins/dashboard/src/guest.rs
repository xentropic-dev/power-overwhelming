use crate::{bar_width, duration, gib, percent, ratio, trim};

wit_bindgen::generate!({
    path: ["../wit/terminal-ui.wit", "../wit/panel.wit"],
    world: "power:panel/panel-plugin",
    generate_all,
});

use power::{
    panel::types::{
        Bar, BoxOverview, CommandScope, Item, Line, ModelUsage, RunOverview, RunStatus, Section,
    },
    terminal_ui::{
        palette,
        types::{Overflow, Segment},
    },
};

const DEFAULTS: &str = include_str!("../defaults.json");
const OPEN: &str = "dashboard.open";
const PANEL: &str = "dashboard";

struct Dashboard;
export!(Dashboard);

impl Guest for Dashboard {
    fn settings() -> Option<PluginSettings> {
        Some(PluginSettings {
            key: "dashboard".into(),
            defaults_json: DEFAULTS.into(),
        })
    }

    fn configure(config_json: String) -> Result<Option<u32>, String> {
        crate::refresh_ms(&config_json).map(Some)
    }

    fn commands() -> Vec<Command> {
        vec![Command {
            id: OPEN.into(),
            title: "Open dashboard".into(),
            description: "Boxes, DAG runs, model usage and machine load at a glance".into(),
            scope: CommandScope::Box,
            default_keys: vec!["o".into()],
        }]
    }

    fn invoke(id: String) -> Result<Effect, String> {
        match id.as_str() {
            OPEN => Ok(Effect::OpenPanel(PANEL.into())),
            _ => Err(format!("Unknown dashboard command {id:?}")),
        }
    }

    fn render_panel(panel_id: String, data: Overview, width_cols: u32) -> Result<Panel, String> {
        if panel_id != PANEL {
            return Err(format!("Unknown dashboard panel {panel_id:?}"));
        }
        let style = Style::current(width_cols);
        Ok(Panel {
            title: "Dashboard".into(),
            sections: vec![
                summary(&data, &style),
                dags(&data, &style),
                usage(&data, &style),
                boxes(&data, &style),
            ],
        })
    }
}

/// Colors and sizes for one render.
struct Style {
    bar: u32,
    /// The palette's error color as hex, or the closest terminal color.
    error: String,
    /// Characters for a left-hand label.
    label: usize,
}

impl Style {
    fn current(width_cols: u32) -> Self {
        let error = palette::get_palette()
            .map_or_else(|| "red".into(), |colors| format!("#{:06x}", colors.error));
        Self {
            bar: bar_width(width_cols),
            error,
            label: (width_cols as usize / 3).clamp(12, 48),
        }
    }
}

fn text(text: impl Into<String>) -> Item {
    Item::Text(segment(text, None))
}

fn muted(text: impl Into<String>) -> Item {
    Item::Text(segment(text, Some("muted".into())))
}

fn segment(text: impl Into<String>, fg: Option<String>) -> Segment {
    Segment {
        text: text.into(),
        fg,
        bg: None,
        bold: false,
        italic: false,
        overflow: Overflow::Text,
    }
}

fn bar(ratio: f32, style: &Style, fg: Option<String>) -> Item {
    Item::Bar(Bar {
        ratio,
        width: style.bar,
        fg,
        bg: None,
    })
}

fn line(left: Vec<Item>, right: Vec<Item>) -> Line {
    Line { left, right }
}

fn empty(message: &str) -> Vec<Line> {
    vec![line(vec![muted(message)], Vec::new())]
}

fn summary(data: &Overview, style: &Style) -> Section {
    let totals = &data.totals;
    let system = &data.system;
    let cpu = system
        .cpu_percent
        .map_or_else(|| "—".into(), |cpu| format!("{cpu:.0}%"));
    let mut lines = vec![line(
        vec![text(format!(
            "agents working {} · terminals {} · attached {}/{} · CPU {cpu}",
            totals.agents_working, totals.terminals, totals.attached, totals.terminals,
        ))],
        Vec::new(),
    )];
    if system.mem_total > 0 {
        let used = ratio(system.mem_used as f64, system.mem_total as f64);
        lines.push(line(
            vec![text("RAM")],
            vec![
                bar(used, style, None),
                text(format!(
                    "{}/{} GiB",
                    gib(system.mem_used),
                    gib(system.mem_total)
                )),
            ],
        ));
    }
    Section {
        heading: "Summary".into(),
        lines,
    }
}

fn box_name<'a>(data: &'a Overview, id: &'a str) -> &'a str {
    data.boxes
        .iter()
        .find(|entry| entry.id == id)
        .map_or(id, |entry| entry.name.as_str())
}

fn dags(data: &Overview, style: &Style) -> Section {
    let lines = data
        .runs
        .iter()
        .filter(|run| matches!(run.status, RunStatus::Active | RunStatus::Stalled))
        .map(|run| run_line(data, run, style))
        .collect::<Vec<_>>();
    Section {
        heading: "DAGs".into(),
        lines: if lines.is_empty() {
            empty("No active DAG runs")
        } else {
            lines
        },
    }
}

fn run_line(data: &Overview, run: &RunOverview, style: &Style) -> Line {
    let stalled = run.status == RunStatus::Stalled;
    let fg = stalled.then(|| style.error.clone());
    let colored = |value: String| Item::Text(segment(value, fg.clone()));
    let done = ratio(f64::from(run.completed), f64::from(run.total));
    let eta = if stalled {
        "stalled".into()
    } else if run.paused {
        "paused".into()
    } else {
        run.eta_secs
            .map_or_else(|| "ETA —".into(), |secs| format!("ETA {}", duration(secs)))
    };
    let label = format!("{} · {}", box_name(data, &run.box_id), run.title);
    line(
        vec![colored(trim(&label, style.label))],
        vec![
            colored(format!("{}/{}", run.completed, run.total)),
            bar(done, style, fg.clone()),
            colored(percent(done)),
            colored(format!("workers {}", run.workers)),
            colored(eta),
        ],
    )
}

fn usage(data: &Overview, style: &Style) -> Section {
    let lines = data
        .usage
        .iter()
        .map(|model| usage_line(model, data.at, style))
        .collect::<Vec<_>>();
    Section {
        heading: "Usage".into(),
        lines: if lines.is_empty() {
            empty("No models in use")
        } else {
            lines
        },
    }
}

fn usage_line(model: &ModelUsage, now: u64, style: &Style) -> Line {
    let name = match &model.model {
        Some(name) => format!("{} {name}", model.provider),
        None => model.provider.clone(),
    };
    let left = vec![text(trim(&name, style.label))];
    if model.windows.is_empty() {
        return line(left, vec![muted("no data")]);
    }
    let mut right = Vec::new();
    for window in &model.windows {
        if !right.is_empty() {
            right.push(Item::Spacer);
        }
        let used = (window.used_percent / 100.).clamp(0., 1.);
        right.push(muted(window.label.clone()));
        right.push(bar(used, style, None));
        right.push(text(percent(used)));
        if let Some(resets) = window.resets_at {
            right.push(muted(format!(
                "resets {}",
                duration(resets.saturating_sub(now))
            )));
        }
    }
    line(left, right)
}

fn boxes(data: &Overview, style: &Style) -> Section {
    let lines = data
        .boxes
        .iter()
        .map(|entry| box_line(entry, style))
        .collect::<Vec<_>>();
    Section {
        heading: "Boxes".into(),
        lines: if lines.is_empty() {
            empty("No boxes")
        } else {
            lines
        },
    }
}

fn box_line(entry: &BoxOverview, style: &Style) -> Line {
    line(
        vec![text(trim(&entry.name, style.label))],
        vec![
            text(format!("terminals {}/{}", entry.attached, entry.terminals)),
            text(format!("agents {}", entry.agents_working)),
            text(format!("DAGs {}", entry.runs_active)),
        ],
    )
}
