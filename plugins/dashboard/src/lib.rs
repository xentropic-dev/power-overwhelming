//! The bundled dashboard panel: a high-level overview of every box, DAG run and model.
//!
//! The text helpers are plain Rust so they test natively; the guest bindings build only
//! for `wasm32`.

#[cfg(target_arch = "wasm32")]
mod guest;

/// A short duration such as `45s`, `14m`, `2h05m` or `3d04h`.
pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s.div_ceil(60).min(59)),
        s if s < 86_400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d{:02}h", s / 86_400, s % 86_400 / 3600),
    }
}

/// Bytes in binary gigabytes with one decimal, such as `6.1`.
pub fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 30) as f64)
}

/// `part / whole` clamped to 0–1, and 0 for an empty whole.
pub fn ratio(part: f64, whole: f64) -> f32 {
    if whole <= 0. {
        0.
    } else {
        (part / whole).clamp(0., 1.) as f32
    }
}

/// A whole percentage for a 0–1 ratio.
pub fn percent(ratio: f32) -> String {
    format!("{:.0}%", (ratio * 100.).clamp(0., 100.))
}

/// The bar width in columns for a panel `width_cols` wide.
pub fn bar_width(width_cols: u32) -> u32 {
    (width_cols / 6).clamp(8, 24)
}

/// Trims `text` to at most `max` characters, ending in `…` when cut.
pub fn trim(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The refresh interval from the `dashboard` settings: `refresh_ms`, at least 250.
pub fn refresh_ms(config_json: &str) -> Result<u32, String> {
    let value: serde_json::Value =
        serde_json::from_str(config_json).map_err(|error| format!("dashboard settings: {error}"))?;
    let Some(ms) = value.get("refresh_ms") else {
        return Ok(DEFAULT_REFRESH_MS);
    };
    let ms = ms
        .as_u64()
        .and_then(|ms| u32::try_from(ms).ok())
        .ok_or("dashboard.refresh_ms must be a whole number of milliseconds")?;
    Ok(ms.max(MIN_REFRESH_MS))
}

pub const DEFAULT_REFRESH_MS: u32 = 2000;
const MIN_REFRESH_MS: u32 = 250;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_stay_short() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(61), "2m");
        assert_eq!(duration(14 * 60), "14m");
        assert_eq!(duration(3599), "59m");
        assert_eq!(duration(2 * 3600 + 5 * 60), "2h05m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600), "3d04h");
    }

    #[test]
    fn ratios_clamp() {
        assert_eq!(ratio(5., 0.), 0.);
        assert_eq!(ratio(5., 10.), 0.5);
        assert_eq!(ratio(15., 10.), 1.);
        assert_eq!(percent(0.625), "62%");
        assert_eq!(percent(1.5), "100%");
    }

    #[test]
    fn bars_scale_with_width() {
        assert_eq!(bar_width(20), 8);
        assert_eq!(bar_width(90), 15);
        assert_eq!(bar_width(400), 24);
    }

    #[test]
    fn trims_on_characters() {
        assert_eq!(trim("dashboard", 20), "dashboard");
        assert_eq!(trim("dashboard", 5), "dash…");
        assert_eq!(trim("ééééé", 3), "éé…");
    }

    #[test]
    fn refresh_reads_settings() {
        assert_eq!(refresh_ms("{}"), Ok(2000));
        assert_eq!(refresh_ms(r#"{"refresh_ms": 5000}"#), Ok(5000));
        assert_eq!(refresh_ms(r#"{"refresh_ms": 10}"#), Ok(250));
        assert!(refresh_ms(r#"{"refresh_ms": "fast"}"#).is_err());
        assert!(refresh_ms("nope").is_err());
    }

    #[test]
    fn gigabytes_have_one_decimal() {
        assert_eq!(gib(0), "0.0");
        assert_eq!(gib(3 << 29), "1.5");
    }
}
