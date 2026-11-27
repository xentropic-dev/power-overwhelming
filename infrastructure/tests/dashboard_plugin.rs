#![cfg(feature = "wasm-plugins")]

//! The bundled dashboard component, through the panel plugin host.

use anyhow::{Result, bail};
use infrastructure::{
    plugins::{
        CommandScope,
        panel::{
            BoxOverview, ModelUsage, Overview, Panel, PanelItem, PanelPlugins, PanelUpdate,
            RunOverview, RunStatus, SystemStats, Totals, UsageWindow,
        },
    },
    theme,
};
use std::time::Duration;

fn next(updates: &async_channel::Receiver<PanelUpdate>) -> Result<PanelUpdate> {
    for _ in 0..500 {
        match updates.try_recv() {
            Ok(update) => return Ok(update),
            Err(async_channel::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => bail!("{error}"),
        }
    }
    bail!("No panel update arrived")
}

fn run(title: &str, status: RunStatus, completed: u32, eta_secs: Option<u64>) -> RunOverview {
    RunOverview {
        run: format!("run-{title}"),
        box_id: "b1".into(),
        title: title.into(),
        completed,
        total: 8,
        workers: 3,
        eta_secs,
        status,
        paused: false,
    }
}

fn overview() -> Overview {
    Overview {
        boxes: vec![BoxOverview {
            id: "b1".into(),
            name: "power".into(),
            terminals: 5,
            attached: 3,
            agents_working: 2,
            runs_active: 1,
        }],
        runs: vec![
            run("ship", RunStatus::Active, 5, Some(14 * 60)),
            run("stuck", RunStatus::Stalled, 2, None),
            run("done", RunStatus::Completed, 8, Some(0)),
        ],
        usage: vec![
            ModelUsage {
                provider: "Claude".into(),
                model: Some("Opus".into()),
                windows: vec![UsageWindow {
                    label: "5h".into(),
                    used_percent: 42.,
                    resets_at: Some(1_800_000_000 + 3600),
                }],
            },
            ModelUsage {
                provider: "Codex".into(),
                model: None,
                windows: Vec::new(),
            },
        ],
        system: SystemStats {
            cpu_percent: Some(12.5),
            mem_used: 6 << 30,
            mem_total: 32 << 30,
        },
        totals: Totals {
            agents_working: 2,
            terminals: 5,
            attached: 3,
        },
        at: 1_800_000_000,
    }
}

fn texts(items: &[PanelItem]) -> Vec<&str> {
    items
        .iter()
        .filter_map(|item| match item {
            PanelItem::Text(segment) => Some(segment.text.as_str()),
            _ => None,
        })
        .collect()
}

fn bars(items: &[PanelItem]) -> usize {
    items
        .iter()
        .filter(|item| matches!(item, PanelItem::Bar(_)))
        .count()
}

fn draw() -> Result<Panel> {
    let directory = tempfile::tempdir()?;
    let (panels, updates) = PanelPlugins::start(Some(directory.path().to_owned()));
    let command = panels
        .commands()
        .into_iter()
        .find(|command| command.id == "dashboard.open")
        .expect("the dashboard is bundled");
    assert_eq!(command.scope, CommandScope::Box);
    assert_eq!(command.default_keys, ["o"]);
    panels.invoke("dashboard.open");
    match next(&updates)? {
        PanelUpdate::OpenPanel(id) => assert_eq!(id, "dashboard"),
        other => bail!("Unexpected update {other:?}"),
    }
    panels.render("dashboard", overview(), 100, theme::default_scheme());
    match next(&updates)? {
        PanelUpdate::Panel { panel_id, panel } => {
            assert_eq!(panel_id, "dashboard");
            Ok(panel)
        }
        other => bail!("Unexpected update {other:?}"),
    }
}

#[test]
fn bundled_dashboard_draws_every_section() -> Result<()> {
    let panel = draw()?;
    assert_eq!(panel.title, "Dashboard");
    let headings: Vec<_> = panel.sections.iter().map(|s| s.heading.as_str()).collect();
    assert_eq!(headings, ["Summary", "DAGs", "Usage", "Boxes"]);
    let [summary, dags, usage, boxes] = &panel.sections[..] else {
        unreachable!()
    };

    let totals = texts(&summary.lines[0].left).join("");
    assert!(totals.contains("agents working 2"), "{totals}");
    assert!(totals.contains("attached 3/5"), "{totals}");
    assert!(
        totals.contains("CPU 12%") || totals.contains("CPU 13%"),
        "{totals}"
    );
    assert_eq!(bars(&summary.lines[1].right), 1);
    assert!(texts(&summary.lines[1].right).contains(&"6.0/32.0 GiB"));

    // Completed runs are left out; stalled ones are drawn in the error color.
    assert_eq!(dags.lines.len(), 2);
    let active = texts(&dags.lines[0].right);
    assert_eq!(texts(&dags.lines[0].left), ["power · ship"]);
    for expected in ["5/8", "62%", "workers 3", "ETA 14m"] {
        assert!(active.contains(&expected), "{active:?}");
    }
    assert_eq!(bars(&dags.lines[0].right), 1);
    let stalled = &dags.lines[1];
    assert!(texts(&stalled.right).contains(&"stalled"));
    let PanelItem::Text(label) = &stalled.left[0] else {
        bail!("Expected a label")
    };
    assert!(label.fg.as_deref().is_some_and(|fg| fg.starts_with('#')));

    let claude = texts(&usage.lines[0].right);
    for expected in ["5h", "42%", "resets 1h00m"] {
        assert!(claude.contains(&expected), "{claude:?}");
    }
    assert_eq!(texts(&usage.lines[1].left), ["Codex"]);
    assert_eq!(texts(&usage.lines[1].right), ["no data"]);

    assert_eq!(texts(&boxes.lines[0].left), ["power"]);
    assert_eq!(
        texts(&boxes.lines[0].right),
        ["terminals 3/5", "agents 2", "DAGs 1"]
    );
    Ok(())
}
