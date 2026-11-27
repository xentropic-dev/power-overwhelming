//! Helpers shared by the integration tests.

use std::{fs, path::Path};

/// Writes `config.lua` in the test's isolated `XDG_CONFIG_HOME`, `root/config`. A first line
/// turns off the worker budget's memory gate, so the free memory of the machine running the
/// tests cannot queue a dispatch. `lua` can assign `dag` again to test the budget.
pub fn write_config(root: &Path, lua: &str) {
    let config = root.join("config/power-overwhelming/config.lua");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        format!("dag = {{ min_available_memory = 0 }}\n{lua}"),
    )
    .unwrap();
}
