//! The worker budget from `config.lua` and the machine's available memory.

use anyhow::Result;
use application::dag::{MemoryProbe, WorkerBudgetSource};
use domain::dag::WorkerBudget;

#[derive(Default)]
pub struct LocalWorkerBudget {
    #[cfg(feature = "lua-config")]
    defaults: crate::config::Sections,
}

impl LocalWorkerBudget {
    /// Supply the desktop defaults used to seed Lua globals in the user's config.
    #[cfg(feature = "lua-config")]
    pub fn new(defaults: crate::config::Sections) -> Self {
        Self { defaults }
    }
}

impl WorkerBudgetSource for LocalWorkerBudget {
    fn worker_budget(&self) -> Result<WorkerBudget> {
        // Read config at each dispatch so edits apply to the next one.
        #[cfg(feature = "lua-config")]
        return crate::config::load_worker_budget(self.defaults.clone());
        #[cfg(not(feature = "lua-config"))]
        Ok(WorkerBudget::default())
    }
}

/// Reads `MemAvailable` plus `SwapFree` from `/proc/meminfo`.
pub(crate) struct ProcMemory;

impl MemoryProbe for ProcMemory {
    fn available_memory(&self) -> Option<u64> {
        meminfo_available(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }
}

fn meminfo_available(meminfo: &str) -> Option<u64> {
    let field = |name: &str| {
        meminfo.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.strip_prefix(':')?;
            let kib: u64 = value.trim().strip_suffix("kB")?.trim().parse().ok()?;
            kib.checked_mul(1024)
        })
    };
    Some(field("MemAvailable")? + field("SwapFree").unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_memory_adds_free_swap() {
        let meminfo = "MemTotal:       32779628 kB\nMemFree:  100 kB\nMemAvailable:   2000 kB\n\
                       SwapTotal: 9000 kB\nSwapFree:        1000 kB\n";
        assert_eq!(meminfo_available(meminfo), Some(3000 * 1024));
        assert_eq!(meminfo_available("MemAvailable: 5 kB\n"), Some(5 * 1024));
        assert_eq!(meminfo_available("MemTotal: 5 kB\n"), None);
    }
}
