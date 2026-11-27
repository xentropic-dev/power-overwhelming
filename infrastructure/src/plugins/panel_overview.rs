//! Converts the daemon overview into the panel plugin interface's mirror of it.

use super::panel::{
    BoxOverview, ModelUsage, Overview, RunOverview, RunStatus, SystemStats, Totals, UsageWindow,
};
use domain::{dag, overview};

/// Counts saturate at `u32::MAX`, far past any real box.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

impl From<overview::Overview> for Overview {
    fn from(value: overview::Overview) -> Self {
        Self {
            boxes: value.boxes.into_iter().map(Into::into).collect(),
            runs: value.runs.into_iter().map(Into::into).collect(),
            usage: value.usage.into_iter().map(Into::into).collect(),
            system: value.system.into(),
            totals: value.totals.into(),
            at: value.at,
        }
    }
}

impl From<overview::BoxOverview> for BoxOverview {
    fn from(value: overview::BoxOverview) -> Self {
        Self {
            id: value.id,
            name: value.name,
            terminals: count(value.terminals),
            attached: count(value.attached),
            agents_working: count(value.agents_working),
            runs_active: count(value.runs_active),
        }
    }
}

impl From<overview::RunOverview> for RunOverview {
    fn from(value: overview::RunOverview) -> Self {
        Self {
            run: value.run,
            box_id: value.box_id,
            title: value.title,
            completed: count(value.completed),
            total: count(value.total),
            workers: count(value.workers),
            eta_secs: value.eta_secs,
            status: match value.status {
                dag::RunStatus::Active => RunStatus::Active,
                dag::RunStatus::Completed => RunStatus::Completed,
                dag::RunStatus::Stalled => RunStatus::Stalled,
                dag::RunStatus::Archived => RunStatus::Archived,
            },
            paused: value.paused,
        }
    }
}

impl From<overview::ModelUsage> for ModelUsage {
    fn from(value: overview::ModelUsage) -> Self {
        Self {
            provider: value.provider,
            model: value.model,
            windows: value
                .windows
                .into_iter()
                .map(|window| UsageWindow {
                    label: window.label,
                    used_percent: window.used_percent,
                    resets_at: window.resets_at,
                })
                .collect(),
        }
    }
}

impl From<overview::SystemStats> for SystemStats {
    fn from(value: overview::SystemStats) -> Self {
        Self {
            cpu_percent: value.cpu_percent,
            mem_used: value.mem_used,
            mem_total: value.mem_total,
        }
    }
}

impl From<overview::Totals> for Totals {
    fn from(value: overview::Totals) -> Self {
        Self {
            agents_working: count(value.agents_working),
            terminals: count(value.terminals),
            attached: count(value.attached),
        }
    }
}
