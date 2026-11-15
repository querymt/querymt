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

/// Current UTC time as `2026-11-07T23:26:14Z` (for diagnostic reports).
pub fn rfc3339_now() -> String {
    format_rfc3339((now_ms() / 1000.) as i64)
}

/// Epoch seconds as RFC 3339 UTC (civil from days, Howard Hinnant).
fn format_rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
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

/// Fuzzy match of `query` against `fields` (most important first); higher is
/// better, `None` when some query word matches no field. Each word must match
/// a field as a subsequence; contiguous runs, word starts and earlier fields
/// score higher. An empty query matches with score 0.
pub fn fuzzy_score(query: &str, fields: &[&str]) -> Option<u32> {
    let mut total = 0;
    for word in query.split_whitespace() {
        let word: Vec<char> = word.to_lowercase().chars().collect();
        let weights = (1..=fields.len() as u32).rev();
        total += fields
            .iter()
            .zip(weights)
            .filter_map(|(field, weight)| Some(word_score(&word, field)? * weight))
            .max()?;
    }
    Some(total)
}

fn word_score(word: &[char], text: &str) -> Option<u32> {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let (mut score, mut at, mut prev) = (0u32, 0usize, None::<usize>);
    for &c in word {
        let ix = at + text[at..].iter().position(|&t| t == c)?;
        score += 1;
        // A contiguous run outweighs letters scattered over word starts.
        if prev.is_some_and(|p| p + 1 == ix) {
            score += 10;
        } else if ix == 0 || !text[ix - 1].is_alphanumeric() {
            score += 6;
        }
        prev = Some(ix);
        at = ix + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::{format_rfc3339, fuzzy_score, parse_rfc3339_ms};

    #[test]
    fn fuzzy_score_ranks_contiguous_and_earlier_fields_higher() {
        assert_eq!(fuzzy_score("", &["anything"]), Some(0));
        assert_eq!(fuzzy_score("xyz", &["coder", "delegate"]), None);
        // Subsequence across a word boundary.
        assert!(fuzzy_score("cdl", &["Coder Delegate"]).is_some());
        // Every word must match some field.
        assert!(fuzzy_score("cod speech", &["Coder", "cdr", "Speech helper"]).is_some());
        assert_eq!(fuzzy_score("cod zzz", &["Coder", "cdr"]), None);
        // Contiguous beats scattered; a name hit beats a description hit.
        let tight = fuzzy_score("def", &["def-qmcp"]).unwrap();
        let loose = fuzzy_score("def", &["Default (Speech)"]).unwrap();
        assert!(fuzzy_score("def", &["d-e-f"]).unwrap() < tight);
        assert!(loose <= tight);
        let in_name = fuzzy_score("review", &["Review", "x", "y"]).unwrap();
        let in_desc = fuzzy_score("review", &["x", "y", "Review"]).unwrap();
        assert!(in_name > in_desc);
    }

    #[test]
    fn rfc3339_round_trips() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        let s = "2026-11-07T23:26:14Z";
        let secs = (parse_rfc3339_ms(s).unwrap() / 1000.) as i64;
        assert_eq!(format_rfc3339(secs), s);
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }
}
