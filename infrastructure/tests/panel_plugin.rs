#![cfg(feature = "wasm-plugins")]

use anyhow::{Context, Result, bail};
use domain::overview::{Overview, SystemStats, Totals};
use infrastructure::{
    plugins::{
        CommandScope, PluginCommand,
        panel::{PanelItem, PanelPlugins, PanelSettings, PanelUpdate, validate_panel},
    },
    theme,
};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path, time::Duration};

const FIXTURE: &str = include_str!("fixtures/panel-plugin.wat");

fn install(directory: &Path, source: &str) -> Result<()> {
    fs::create_dir_all(directory.join("plugins"))?;
    fs::write(
        directory.join("plugins/fixture.wasm"),
        wat::parse_str(source)?,
    )?;
    Ok(())
}

fn overview(terminals: usize, attached: usize) -> Overview {
    Overview {
        boxes: Vec::new(),
        runs: Vec::new(),
        usage: Vec::new(),
        system: SystemStats {
            cpu_percent: Some(12.5),
            mem_used: 1 << 30,
            mem_total: 4 << 30,
        },
        totals: Totals {
            agents_working: 1,
            terminals,
            attached,
        },
        at: 1_800_000_000,
    }
}

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

#[test]
fn panel_plugins_register_commands_open_and_draw_panels() -> Result<()> {
    let directory = tempfile::tempdir()?;
    install(directory.path(), FIXTURE)?;
    let (panels, updates) = PanelPlugins::start(Some(directory.path().to_owned()));
    assert_eq!(
        panels.commands(),
        [PluginCommand {
            id: "fixture.open".into(),
            title: "Open fixture".into(),
            description: "Open the fixture panel".into(),
            scope: CommandScope::Box,
            default_keys: vec!["o".into()],
        }]
    );

    assert_eq!(
        panels.setup().settings,
        [PanelSettings {
            key: "fixture".into(),
            defaults: json!({"refresh_ms": 2000}),
        }]
    );

    // Before configure, the host picks the pace.
    panels.invoke("fixture.open");
    match next(&updates)? {
        PanelUpdate::OpenPanel {
            panel_id,
            refresh_ms,
        } => assert_eq!((panel_id.as_str(), refresh_ms), ("fixture", None)),
        other => bail!("Unexpected update {other:?}"),
    }
    // The fixture asks for 100 ms per byte of its settings JSON.
    panels.configure(BTreeMap::from([(
        "fixture".into(),
        json!({"refresh_ms": 500}),
    )]));
    panels.invoke("fixture.open");
    match next(&updates)? {
        PanelUpdate::OpenPanel { refresh_ms, .. } => assert_eq!(refresh_ms, Some(1800)),
        other => bail!("Unexpected update {other:?}"),
    }
    // Without its table the plugin gets "{}", and the host holds it to the fastest pace.
    panels.configure(BTreeMap::new());
    panels.invoke("fixture.open");
    match next(&updates)? {
        PanelUpdate::OpenPanel { refresh_ms, .. } => assert_eq!(refresh_ms, Some(250)),
        other => bail!("Unexpected update {other:?}"),
    }

    panels.render("fixture", &overview(4, 2), 24, theme::default_scheme());
    let PanelUpdate::Panel { panel_id, panel } = next(&updates)? else {
        bail!("Expected a panel");
    };
    assert_eq!(panel_id, "fixture");
    assert_eq!(panel.title, "fixture");
    let section = panel.sections.first().context("Missing section")?;
    assert_eq!(section.heading, "Totals");
    let line = &section.lines[0];
    let PanelItem::Text(text) = &line.left[0] else {
        bail!("Expected text on the left");
    };
    assert_eq!(text.text, "terminals");
    assert_eq!(text.fg.as_deref(), Some("accent"));
    assert!(text.bold);
    let PanelItem::Bar(bar) = &line.right[0] else {
        bail!("Expected a bar on the right");
    };
    assert_eq!((bar.ratio, bar.width), (0.5, 24));

    // Another plugin's panel id is not drawn.
    panels.render("other", &overview(4, 2), 24, theme::default_scheme());
    assert!(matches!(next(&updates)?, PanelUpdate::Error(error) if error.contains("other")));
    panels.invoke("missing.command");
    assert!(matches!(next(&updates)?, PanelUpdate::Error(error) if error.contains("missing")));
    Ok(())
}

#[test]
fn a_spinning_plugin_is_dropped_with_an_error_and_reload_restores_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let spinning = FIXTURE.replace(
        "(func (export \"invoke\") (param i32 i32) (result i32) (i32.const 160))",
        "(func (export \"invoke\") (param i32 i32) (result i32) (loop $spin (br $spin)) (i32.const 160))",
    );
    assert_ne!(spinning, FIXTURE);
    install(directory.path(), &spinning)?;
    let (panels, updates) = PanelPlugins::start(Some(directory.path().to_owned()));
    assert_eq!(panels.commands().len(), 1);
    panels.invoke("fixture.open");
    match next(&updates)? {
        PanelUpdate::Error(error) => assert!(error.contains("Disabled plugin"), "{error}"),
        other => bail!("Unexpected update {other:?}"),
    }
    assert!(panels.commands().is_empty());

    install(directory.path(), FIXTURE)?;
    panels.reload(None);
    assert_eq!(panels.commands().len(), 1);
    Ok(())
}

#[test]
fn invalid_plugins_and_bad_output_are_rejected() -> Result<()> {
    let directory = tempfile::tempdir()?;
    // A command id without a namespace is refused, and the plugin does not load.
    install(
        directory.path(),
        &FIXTURE.replace(
            "\"\\00\\02\\00\\00\\0c\\00\\00\\00\"   ;; id",
            "\"\\00\\02\\00\\00\\07\\00\\00\\00\"   ;; id",
        ),
    )?;
    let (panels, updates) = PanelPlugins::start(Some(directory.path().to_owned()));
    assert!(panels.commands().is_empty());
    match next(&updates)? {
        PanelUpdate::Error(error) => assert!(error.contains("Invalid command id"), "{error}"),
        other => bail!("Unexpected update {other:?}"),
    }

    let mut panel = infrastructure::plugins::panel::Panel {
        title: "ok".into(),
        sections: vec![infrastructure::plugins::panel::PanelSection {
            heading: "bad\nheading".into(),
            lines: Vec::new(),
        }],
    };
    assert!(validate_panel(&mut panel).is_err());
    panel.sections[0].heading = "Heading".into();
    panel.sections[0]
        .lines
        .push(infrastructure::plugins::panel::PanelLine {
            left: vec![PanelItem::Bar(infrastructure::plugins::panel::PanelBar {
                ratio: f32::NAN,
                width: 10,
                fg: Some("no-such-color".into()),
                bg: None,
            })],
            right: Vec::new(),
        });
    assert!(validate_panel(&mut panel).is_err());
    let PanelItem::Bar(bar) = &mut panel.sections[0].lines[0].left[0] else {
        unreachable!()
    };
    bar.fg = None;
    bar.ratio = 7.;
    validate_panel(&mut panel)?;
    let PanelItem::Bar(bar) = &panel.sections[0].lines[0].left[0] else {
        unreachable!()
    };
    assert_eq!(bar.ratio, 1.);
    Ok(())
}
