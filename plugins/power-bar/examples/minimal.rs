#![cfg(target_arch = "wasm32")]

wit_bindgen::generate!({
    path: "../wit/terminal-ui.wit",
    world: "terminal-bottom",
});

use power::terminal_ui::types::{Overflow, Segment};

struct Example;
export!(Example);

impl Guest for Example {
    fn settings() -> Option<PluginSettings> {
        None
    }

    fn configure(_: String) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn render(view: ActiveView, _: Vec<Field>, _: Option<String>) -> Result<Option<Row>, String> {
        Ok(Some(Row {
            separator: String::new(),
            left: vec![Segment {
                text: format!("Example plugin · {}", view.agent_type),
                fg: Some("accent".into()),
                bg: None,
                bold: true,
                italic: false,
                overflow: Overflow::Keep,
            }],
            right: Vec::new(),
            actions: None,
        }))
    }
}
