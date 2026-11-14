//! Small helpers: clock, timestamps, labels.

pub fn now_ms() -> f64 {
    #[cfg(target_family = "wasm")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }
}

/// Parse `2026-11-07T23:26:14.731Z` (UTC, optional fraction / offset ignored) to epoch ms.
pub fn parse_rfc3339_ms(s: &str) -> Option<f64> {
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + h * 3600 + mi * 60 + sec) * 1000) as f64)
}

pub fn workspace_name(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

/// Compact age like the Svelte app: `now`, `5m`, `2h`, `3d`, then `Oct 31`.
pub fn short_time(timestamp: &str) -> String {
    let Some(ms) = parse_rfc3339_ms(timestamp) else {
        return String::new();
    };
    let secs = ((now_ms() - ms) / 1000.0).max(0.0) as u64;
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        86400..604800 => format!("{}d", secs / 86400),
        _ => {
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let month = timestamp
                .get(5..7)
                .and_then(|m| m.parse::<usize>().ok())
                .unwrap_or(1);
            let day = timestamp
                .get(8..10)
                .and_then(|d| d.parse::<u32>().ok())
                .unwrap_or(1);
            format!("{} {day}", MONTHS[(month.clamp(1, 12)) - 1])
        }
    }
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1048576 => format!("{:.0} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1048576.0),
    }
}
