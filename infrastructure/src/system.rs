//! The machine's CPU and memory load from `/proc`.

use application::overview::SystemProbe;
use domain::overview::SystemStats;
use std::sync::Mutex;

/// Reads `/proc/meminfo`, and `/proc/stat` CPU time since the previous call.
#[derive(Default)]
pub struct ProcSystem {
    /// The last aggregate CPU sample, so each call measures the load since the one before.
    previous: Mutex<Option<CpuTimes>>,
}

impl SystemProbe for ProcSystem {
    fn stats(&self) -> SystemStats {
        let (mem_used, mem_total) = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|meminfo| memory(&meminfo))
            .unwrap_or_default();
        let cpu_percent = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|stat| CpuTimes::parse(&stat))
            .and_then(|now| {
                let before = self.previous.lock().unwrap().replace(now);
                now.busy_percent_since(before.unwrap_or_default())
            });
        SystemStats {
            cpu_percent,
            mem_used,
            mem_total,
        }
    }
}

/// A `/proc/meminfo` field in bytes.
pub(crate) fn meminfo_field(meminfo: &str, name: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let value = line.strip_prefix(name)?.strip_prefix(':')?;
        let kib: u64 = value.trim().strip_suffix("kB")?.trim().parse().ok()?;
        kib.checked_mul(1024)
    })
}

/// Used and total bytes: `MemTotal` less `MemAvailable`, and `MemTotal`.
fn memory(meminfo: &str) -> Option<(u64, u64)> {
    let total = meminfo_field(meminfo, "MemTotal")?;
    let available = meminfo_field(meminfo, "MemAvailable")?;
    Some((total.saturating_sub(available), total))
}

/// Aggregate jiffies from the `cpu` line of `/proc/stat`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

impl CpuTimes {
    fn parse(stat: &str) -> Option<Self> {
        let line = stat.lines().find(|line| line.starts_with("cpu "))?;
        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        // user nice system idle iowait irq softirq steal. Guest time is already counted
        // in user and nice.
        let fields = fields.get(..8.min(fields.len()))?;
        if fields.len() < 4 {
            return None;
        }
        Some(Self {
            total: fields.iter().sum(),
            idle: fields[3] + fields.get(4).copied().unwrap_or(0),
        })
    }

    fn busy_percent_since(self, before: Self) -> Option<f32> {
        let total = self.total.checked_sub(before.total)?;
        let idle = self.idle.checked_sub(before.idle)?;
        if total == 0 {
            return None;
        }
        Some((total.saturating_sub(idle) as f64 / total as f64 * 100.0) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_is_total_less_available() {
        let meminfo = "MemTotal:       16000 kB\nMemFree:  100 kB\nMemAvailable:   6000 kB\n";
        assert_eq!(memory(meminfo), Some((10000 * 1024, 16000 * 1024)));
        assert_eq!(memory("MemTotal: 5 kB\n"), None);
    }

    #[test]
    fn cpu_load_is_the_busy_share_between_samples() {
        let first = CpuTimes::parse(
            "cpu  100 0 100 700 100 0 0 0 50 0\ncpu0 1 2 3 4 5 6 7 8 9 10\nintr 5\n",
        )
        .unwrap();
        assert_eq!(
            first,
            CpuTimes {
                total: 1000,
                idle: 800
            }
        );
        let second = CpuTimes::parse("cpu  400 0 200 1300 100 0 0 0 0 0\n").unwrap();
        // 1000 jiffies passed, 600 of them idle.
        assert_eq!(second.busy_percent_since(first), Some(40.0));
        assert_eq!(first.busy_percent_since(second), None);
        assert_eq!(first.busy_percent_since(first), None);
        assert_eq!(CpuTimes::parse("intr 5\n"), None);
        assert_eq!(CpuTimes::parse("cpu  1 2\n"), None);
    }
}
