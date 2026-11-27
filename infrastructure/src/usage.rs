//! Rate-limit usage for the models in use: Claude Code reports its limits through its
//! status line, and Codex records them in its session rollouts.

use anyhow::{Context, Result, bail, ensure};
use application::overview::UsageSource;
use domain::overview::{ModelInUse, ModelUsage, UsageWindow, window_label};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// A Claude report older than this is no longer shown.
const CLAUDE_STALE_AFTER: Duration = Duration::from_secs(10 * 60);
/// How long a Codex reading is reused before the rollouts are read again.
const CODEX_CACHE: Duration = Duration::from_secs(30);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Leaves out windows that have already reset, since their percentage no longer applies.
fn current(windows: &[UsageWindow], now: u64) -> Vec<UsageWindow> {
    windows
        .iter()
        .filter(|window| window.resets_at.is_none_or(|resets_at| resets_at > now))
        .cloned()
        .collect()
}

fn percent(value: &Value) -> Option<f32> {
    let value = value.as_f64()?;
    value.is_finite().then(|| value.clamp(0.0, 100.0) as f32)
}

/// The windows in a Claude Code status-line payload's `rate_limits`. Each window may be
/// absent, and so may `rate_limits` itself.
pub fn claude_status_line_windows(payload: &Value) -> Vec<UsageWindow> {
    let limits = &payload["rate_limits"];
    [
        ("five_hour", "5h"),
        ("seven_day", "7d"),
        ("spend_limit", "spend"),
    ]
    .into_iter()
    .filter_map(|(key, label)| {
        let window = &limits[key];
        Some(UsageWindow {
            label: label.into(),
            used_percent: percent(&window["used_percentage"])?,
            resets_at: window["resets_at"].as_u64(),
        })
    })
    .collect()
}

/// Encodes a usage report as one metadata line: each window as `label=percent@resets_at`,
/// then the model, which may contain spaces.
pub(crate) fn encode_usage(model: &str, windows: &[UsageWindow]) -> String {
    let mut value = String::new();
    for window in windows {
        value.push_str(&window.label);
        value.push('=');
        value.push_str(&format!("{:.1}", window.used_percent));
        if let Some(resets_at) = window.resets_at {
            value.push_str(&format!("@{resets_at}"));
        }
        value.push(' ');
    }
    value.push_str(model.trim());
    value
}

pub(crate) fn decode_usage(value: &str) -> Result<(String, Vec<UsageWindow>)> {
    let mut windows = Vec::new();
    let mut rest = value.trim();
    while let Some((head, tail)) = rest.split_once(' ') {
        let Some(window) = decode_window(head) else {
            break;
        };
        windows.push(window?);
        rest = tail.trim_start();
    }
    ensure!(
        decode_window(rest).is_none(),
        "A usage report must end with its model"
    );
    ensure!(!rest.is_empty(), "A usage report must name its model");
    Ok((rest.to_owned(), windows))
}

/// `None` when `token` is not a window at all, so it starts the model.
fn decode_window(token: &str) -> Option<Result<UsageWindow>> {
    let (label, value) = token.split_once('=')?;
    if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((|| {
        let (used, resets_at) = match value.split_once('@') {
            Some((used, resets_at)) => (used, Some(resets_at.parse().context("Reset time")?)),
            None => (value, None),
        };
        let used: f32 = used.parse().context("Used percentage")?;
        if !used.is_finite() {
            bail!("Used percentage must be a number");
        }
        Ok(UsageWindow {
            label: label.into(),
            used_percent: used.clamp(0.0, 100.0),
            resets_at,
        })
    })())
}

/// The latest Claude Code report per model, kept in daemon memory.
#[derive(Default)]
pub struct ClaudeReports {
    reports: Mutex<HashMap<String, (Instant, Vec<UsageWindow>)>>,
}

impl ClaudeReports {
    /// Records a report sent as a usage metadata line.
    pub(crate) fn record(&self, value: &str) -> Result<()> {
        let (model, windows) = decode_usage(value)?;
        self.reports
            .lock()
            .unwrap()
            .insert(model, (Instant::now(), windows));
        Ok(())
    }

    /// The model's latest fresh report. Limits belong to the account, so a model without
    /// its own report uses the newest report of any model.
    fn windows(&self, model: Option<&str>, now: u64) -> Vec<UsageWindow> {
        let mut reports = self.reports.lock().unwrap();
        reports.retain(|_, (at, _)| at.elapsed() < CLAUDE_STALE_AFTER);
        let report = model
            .and_then(|model| reports.get(model))
            .or_else(|| reports.values().max_by_key(|(at, _)| *at));
        report.map_or_else(Vec::new, |(_, windows)| current(windows, now))
    }
}

/// Reads the limits from the newest Codex session rollouts.
pub struct CodexRollouts {
    sessions: PathBuf,
    cache: Mutex<Option<(Instant, Vec<UsageWindow>)>>,
}

impl CodexRollouts {
    pub fn new(sessions: PathBuf) -> Self {
        Self {
            sessions,
            cache: Mutex::new(None),
        }
    }

    /// `$CODEX_HOME/sessions`, or `~/.codex/sessions`.
    pub fn from_env() -> Option<Self> {
        let home = std::env::var_os("CODEX_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?;
        Some(Self::new(home.join("sessions")))
    }

    fn windows(&self, now: u64) -> Vec<UsageWindow> {
        let mut cache = self.cache.lock().unwrap();
        if let Some((at, windows)) = &*cache
            && at.elapsed() < CODEX_CACHE
        {
            return current(windows, now);
        }
        let windows = newest_rollouts(&self.sessions, 5)
            .iter()
            .find_map(|path| rollout_windows(path, now))
            .unwrap_or_default();
        *cache = Some((Instant::now(), windows.clone()));
        current(&windows, now)
    }
}

/// Up to `limit` rollout files under `sessions` (laid out as `YYYY/MM/DD/`), newest first.
fn newest_rollouts(sessions: &Path, limit: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![(sessions.to_path_buf(), 0)];
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() && depth < 4 {
                pending.push((entry.path(), depth + 1));
            } else if kind.is_file() && name.starts_with("rollout-") && name.ends_with(".jsonl") {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(UNIX_EPOCH);
                found.push((modified, entry.path()));
            }
        }
    }
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

/// How much of a rollout's end is read first. Rate limits come with every turn, so the
/// last one is nearly always near the end.
const ROLLOUT_TAIL: u64 = 256 * 1024;
/// Never read more of a rollout than this.
const ROLLOUT_LIMIT: u64 = 64 * 1024 * 1024;

/// The limits in the rollout's last `token_count` event that has them.
fn rollout_windows(path: &Path, now: u64) -> Option<Vec<UsageWindow>> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    for window in [ROLLOUT_TAIL, ROLLOUT_LIMIT] {
        let start = length.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut data = Vec::new();
        file.by_ref().take(window).read_to_end(&mut data).ok()?;
        let text = String::from_utf8_lossy(&data);
        // A partial first line is skipped by the parser.
        if let Some(windows) = text
            .lines()
            .rev()
            .find_map(|line| token_count_windows(line, now))
        {
            return Some(windows);
        }
        if start == 0 {
            break;
        }
    }
    None
}

/// The `primary` and `secondary` limits of a `token_count` event line, if it has any.
fn token_count_windows(line: &str, now: u64) -> Option<Vec<UsageWindow>> {
    if !line.contains("\"token_count\"") {
        return None;
    }
    let event: Value = serde_json::from_str(line).ok()?;
    let payload = &event["payload"];
    if payload["type"] != "token_count" {
        return None;
    }
    let limits = &payload["rate_limits"];
    let windows: Vec<_> = ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let window = &limits[key];
            let used_percent = percent(&window["used_percent"])?;
            let label = window["window_minutes"]
                .as_u64()
                .map_or_else(|| key.to_owned(), window_label);
            let resets_at = window["resets_at"].as_u64().or_else(|| {
                window["resets_in_seconds"]
                    .as_u64()
                    .map(|seconds| event_time(&event).unwrap_or(now) + seconds)
            });
            Some(UsageWindow {
                label,
                used_percent,
                resets_at,
            })
        })
        .collect();
    (!windows.is_empty()).then_some(windows)
}

/// The event's `timestamp` (UTC RFC 3339, as Codex writes it) in Unix seconds, for limits
/// given relative to it.
fn event_time(event: &Value) -> Option<u64> {
    let stamp = event["timestamp"].as_str()?;
    let number = |range: std::ops::Range<usize>| -> Option<i64> { stamp.get(range)?.parse().ok() };
    if stamp.get(4..5)? != "-" || stamp.get(10..11)? != "T" {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date, after Howard Hinnant's `days_from_civil`.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

/// Usage from every provider Power knows how to read. Other providers report no data.
pub struct ProviderUsage {
    pub claude: ClaudeReports,
    pub codex: Option<CodexRollouts>,
}

impl ProviderUsage {
    pub fn from_env() -> Self {
        Self {
            claude: ClaudeReports::default(),
            codex: CodexRollouts::from_env(),
        }
    }
}

impl UsageSource for ProviderUsage {
    fn usage(&self, models: &[ModelInUse]) -> Vec<ModelUsage> {
        let now = unix_now();
        models
            .iter()
            .map(|entry| ModelUsage {
                provider: entry.provider.clone(),
                model: entry.model.clone(),
                windows: match entry.provider.as_str() {
                    "claude" => self.claude.windows(entry.model.as_deref(), now),
                    "codex" => self
                        .codex
                        .as_ref()
                        .map_or_else(Vec::new, |codex| codex.windows(now)),
                    _ => Vec::new(),
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn window(label: &str, used_percent: f32, resets_at: Option<u64>) -> UsageWindow {
        UsageWindow {
            label: label.into(),
            used_percent,
            resets_at,
        }
    }

    #[test]
    fn claude_status_line_limits_are_read_when_present() {
        let payload = json!({
            "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
            "rate_limits": {
                "five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
                "seven_day": {"used_percentage": 41.2},
                "spend_limit": {"used_percentage": "lots"}
            }
        });
        assert_eq!(
            claude_status_line_windows(&payload),
            [
                window("5h", 23.5, Some(1738425600)),
                window("7d", 41.2, None)
            ]
        );
        assert!(claude_status_line_windows(&json!({"model": "x"})).is_empty());
        assert!(claude_status_line_windows(&json!({"rate_limits": null})).is_empty());
    }

    #[test]
    fn usage_reports_round_trip_with_spaced_models() {
        let windows = [
            window("5h", 23.5, Some(1738425600)),
            window("7d", 41.0, None),
        ];
        let value = encode_usage("Opus 5.5", &windows);
        assert_eq!(value, "5h=23.5@1738425600 7d=41.0 Opus 5.5");
        assert_eq!(
            decode_usage(&value).unwrap(),
            ("Opus 5.5".into(), windows.to_vec())
        );
        assert_eq!(
            decode_usage("claude-opus-5-5").unwrap(),
            ("claude-opus-5-5".into(), Vec::new())
        );
        assert!(decode_usage("5h=23.5").is_err());
        assert!(decode_usage("5h=lots claude").is_err());
        assert!(decode_usage("").is_err());
    }

    #[test]
    fn claude_reports_fall_back_to_the_newest_model_and_drop_reset_windows() {
        let reports = ClaudeReports::default();
        assert!(reports.windows(Some("claude-opus-5-5"), 0).is_empty());
        reports
            .record("5h=10.0@100 7d=20.0@900 claude-sonnet-5-5")
            .unwrap();
        reports.record("5h=30.0@100 claude-opus-5-5").unwrap();
        assert_eq!(
            reports.windows(Some("claude-sonnet-5-5"), 50),
            [window("5h", 10.0, Some(100)), window("7d", 20.0, Some(900))]
        );
        assert_eq!(reports.windows(None, 50), [window("5h", 30.0, Some(100))]);
        assert_eq!(
            reports.windows(Some("claude-sonnet-5-5"), 200),
            [window("7d", 20.0, Some(900))]
        );
    }

    #[test]
    fn codex_limits_come_from_the_newest_rollouts_last_token_count() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let older = sessions.join("2026/11/25");
        let newer = sessions.join("2026/11/26");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::write(
            older.join("rollout-old.jsonl"),
            include_str!("../tests/fixtures/codex-rollout.jsonl"),
        )
        .unwrap();
        // The newest rollout has no limits yet, so the one before it is read.
        std::fs::write(
            newer.join("rollout-new.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"rate_limits\":null}}\n",
        )
        .unwrap();
        let old = File::options()
            .write(true)
            .open(older.join("rollout-old.jsonl"))
            .unwrap();
        old.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        std::fs::write(sessions.join("notes.txt"), "ignored").unwrap();

        let codex = CodexRollouts::new(sessions);
        let expected = [
            window("5h", 12.5, Some(1795900000)),
            window("7d", 64.0, Some(1795966484)),
        ];
        assert_eq!(codex.windows(1795000000), expected);
        // A window past its reset is left out, even from the cache.
        assert_eq!(codex.windows(1795950000), expected[1..]);

        let usage = ProviderUsage {
            claude: ClaudeReports::default(),
            codex: Some(codex),
        };
        let models = [
            ModelInUse {
                provider: "codex".into(),
                model: None,
            },
            ModelInUse {
                provider: "grok".into(),
                model: Some("grok-5".into()),
            },
        ];
        let reported = usage.usage(&models);
        assert_eq!(reported.len(), 2);
        assert_eq!(reported[0].provider, "codex");
        assert!(!reported[0].windows.is_empty());
        assert_eq!(reported[1].model.as_deref(), Some("grok-5"));
        assert!(reported[1].windows.is_empty());
    }

    #[test]
    fn codex_limits_relative_to_the_event_time_are_made_absolute() {
        let line = r#"{"timestamp":"2026-11-26T23:17:54.994Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":5,"resets_in_seconds":60}}}}"#;
        let stamp = 1_795_735_074;
        assert_eq!(
            token_count_windows(line, 0),
            Some(vec![window("primary", 5.0, Some(stamp + 60))])
        );
        assert_eq!(token_count_windows("not json token_count", 0), None);
    }
}
