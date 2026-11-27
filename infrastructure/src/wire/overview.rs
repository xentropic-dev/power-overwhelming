//! The dashboard overview on the daemon socket.

use super::RunStatus;
use domain::overview;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Overview {
    pub boxes: Vec<BoxOverview>,
    pub runs: Vec<RunOverview>,
    pub usage: Vec<ModelUsage>,
    pub system: SystemStats,
    pub totals: Totals,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct BoxOverview {
    pub id: String,
    pub name: String,
    pub terminals: usize,
    pub attached: usize,
    pub agents_working: usize,
    pub runs_active: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct RunOverview {
    pub run: String,
    pub box_id: String,
    pub title: String,
    pub completed: usize,
    pub total: usize,
    pub workers: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
    pub status: RunStatus,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct Totals {
    pub agents_working: usize,
    pub terminals: usize,
    pub attached: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct SystemStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ModelUsage {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Empty when the provider reported no limits.
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

/// Converts between a DTO and its domain type, field by field, with lists element-wise.
macro_rules! convert {
    ($name:ident { $($field:ident),+ $(,)? } $(lists { $($list:ident),+ $(,)? })?) => {
        impl From<overview::$name> for $name {
            fn from(value: overview::$name) -> Self {
                Self {
                    $($field: value.$field.into(),)+
                    $($($list: value.$list.into_iter().map(Into::into).collect(),)+)?
                }
            }
        }
        impl From<$name> for overview::$name {
            fn from(value: $name) -> Self {
                Self {
                    $($field: value.$field.into(),)+
                    $($($list: value.$list.into_iter().map(Into::into).collect(),)+)?
                }
            }
        }
    };
}

convert!(Overview { system, totals, at } lists { boxes, runs, usage });
convert!(BoxOverview {
    id,
    name,
    terminals,
    attached,
    agents_working,
    runs_active
});
convert!(RunOverview {
    run,
    box_id,
    title,
    completed,
    total,
    workers,
    eta_secs,
    status,
    paused
});
convert!(Totals {
    agents_working,
    terminals,
    attached
});
convert!(SystemStats {
    cpu_percent,
    mem_used,
    mem_total
});
convert!(ModelUsage { provider, model } lists { windows });
convert!(UsageWindow {
    label,
    used_percent,
    resets_at
});

#[cfg(test)]
mod tests {
    use super::*;
    use domain::dag;

    #[test]
    fn overview_round_trips_through_json() {
        let domain = overview::Overview {
            boxes: vec![overview::BoxOverview {
                id: "box".into(),
                name: "repo".into(),
                terminals: 3,
                attached: 2,
                agents_working: 1,
                runs_active: 1,
            }],
            runs: vec![overview::RunOverview {
                run: "run".into(),
                box_id: "box".into(),
                title: "Run".into(),
                completed: 1,
                total: 4,
                workers: 2,
                eta_secs: Some(600),
                status: dag::RunStatus::Stalled,
                paused: false,
            }],
            usage: vec![overview::ModelUsage {
                provider: "claude".into(),
                model: Some("claude-opus-5-5".into()),
                windows: vec![overview::UsageWindow {
                    label: "5h".into(),
                    used_percent: 23.5,
                    resets_at: Some(1738425600),
                }],
            }],
            system: overview::SystemStats {
                cpu_percent: Some(12.5),
                mem_used: 1024,
                mem_total: 4096,
            },
            totals: overview::Totals {
                agents_working: 1,
                terminals: 3,
                attached: 2,
            },
            at: 42,
        };
        let json = serde_json::to_value(Overview::from(domain.clone())).unwrap();
        assert_eq!(json["runs"][0]["status"], "stalled");
        assert_eq!(json["usage"][0]["windows"][0]["label"], "5h");
        let back: Overview = serde_json::from_value(json).unwrap();
        assert_eq!(overview::Overview::from(back), domain);
    }
}
