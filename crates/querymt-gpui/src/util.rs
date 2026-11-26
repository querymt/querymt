//! Small helpers: clock, timestamps, labels.

/// The app version: the published release or nightly version when CI set
/// `QMT_VERSION` at build time, else the crate version (see `build.rs`).
pub const APP_VERSION: &str = env!("QMT_BUILD_VERSION");

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

/// The system's IANA time zone, or `UTC` when it cannot be read.
pub fn system_time_zone() -> String {
    iana_time_zone::get_timezone()
        .ok()
        .filter(|zone| !zone.is_empty())
        .unwrap_or_else(|| "UTC".into())
}

/// IANA zone names for a picker, sorted; always has UTC and the system zone.
pub fn time_zones() -> Vec<String> {
    let mut zones = known_time_zones();
    zones.push("UTC".into());
    zones.push(system_time_zone());
    zones.sort();
    zones.dedup();
    zones
}

/// Zones of the tz database's `zone1970.tab` (one per region).
#[cfg(not(target_family = "wasm"))]
fn known_time_zones() -> Vec<String> {
    let dirs = std::env::var("TZDIR").into_iter().chain(
        [
            "/usr/share/zoneinfo",
            "/etc/zoneinfo",
            "/usr/share/lib/zoneinfo",
        ]
        .map(String::from),
    );
    dirs.filter_map(|dir| std::fs::read_to_string(format!("{dir}/zone1970.tab")).ok())
        .map(|tab| parse_zone_tab(&tab))
        .find(|zones| !zones.is_empty())
        .unwrap_or_default()
}

#[cfg(target_family = "wasm")]
fn known_time_zones() -> Vec<String> {
    js_sys::Intl::supported_values_of(js_sys::Intl::SupportedValuesKey::TimeZone)
        .iter()
        .map(String::from)
        .collect()
}

/// Zone names of a `zone.tab`-style file: the third column of each entry.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
fn parse_zone_tab(tab: &str) -> Vec<String> {
    tab.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split('\t').nth(2))
        .map(str::to_owned)
        .collect()
}

pub fn workspace_name(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

/// Full timestamp for tooltips: `Nov 7, 2026, 23:26 UTC` (the zone is named
/// only when the timestamp is in UTC; offsets are not converted).
pub fn absolute_time(timestamp: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let num = |range: std::ops::Range<usize>| timestamp.get(range)?.parse::<u32>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute) = (num(11..13)?, num(14..16)?);
    let name = MONTHS.get(month.checked_sub(1)? as usize)?;
    let utc = timestamp.ends_with('Z') || timestamp.ends_with("+00:00");
    Some(format!(
        "{name} {day}, {year}, {hour:02}:{minute:02}{}",
        if utc { " UTC" } else { "" }
    ))
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

/// YAML front matter leading a message (desktop `extractMessageFrontMatter`).
#[derive(Clone, Debug, PartialEq)]
pub struct FrontMatter {
    /// Exact prefix, delimiters and line endings included.
    pub source: String,
    /// The YAML between the delimiters.
    pub yaml: String,
}

/// Split `text` into its front matter and the body after it: a first line
/// `---`, then YAML up to a line `---` or `...`. Without a closing line the
/// text is ordinary Markdown (a leading rule) and is left whole.
pub fn split_front_matter(text: &str) -> Option<(FrontMatter, &str)> {
    let bom = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let is_delim = |line: &str, marks: &[&str]| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        marks.iter().any(|m| {
            line.strip_prefix(m)
                .is_some_and(|rest| rest.chars().all(|c| c == ' ' || c == '\t'))
        })
    };
    let first_end = text[bom..].find('\n')? + bom;
    if !is_delim(&text[bom..first_end], &["---"]) {
        return None;
    }
    let yaml_start = first_end + 1;
    let mut pos = yaml_start;
    for line in text[yaml_start..].split_inclusive('\n') {
        let next = pos + line.len();
        if is_delim(line.strip_suffix('\n').unwrap_or(line), &["---", "..."]) {
            let front = FrontMatter {
                source: text[..next].to_string(),
                yaml: text[yaml_start..pos].to_string(),
            };
            return Some((front, &text[next..]));
        }
        pos = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{format_rfc3339, parse_rfc3339_ms, parse_zone_tab, split_front_matter, time_zones};

    #[test]
    fn zone_tab_lists_the_zone_column() {
        let tab = "# comment\nAD\t+4230+00131\tEurope/Andorra\nUS\t+404251-0740023\tAmerica/New_York\tEastern (most areas)\n";
        assert_eq!(parse_zone_tab(tab), ["Europe/Andorra", "America/New_York"]);
        let zones = time_zones();
        assert!(zones.iter().any(|z| z == "UTC"));
        assert!(zones.is_sorted());
    }

    #[test]
    fn front_matter_is_split_from_the_body() {
        let text = "---\ntype: callback\nid: \"x\"\n---\nWorker turn returned.";
        let (fm, body) = split_front_matter(text).unwrap();
        assert_eq!(fm.yaml, "type: callback\nid: \"x\"\n");
        assert_eq!(fm.source, "---\ntype: callback\nid: \"x\"\n---\n");
        assert_eq!(body, "Worker turn returned.");
        // CRLF, BOM, `...` closing and trailing blanks on delimiters.
        let (fm, body) = split_front_matter("\u{feff}--- \r\na: 1\r\n...\t\r\nbody").unwrap();
        assert_eq!(fm.yaml, "a: 1\r\n");
        assert_eq!(body, "body");
        // Front matter only, closed at the end of the text.
        let (fm, body) = split_front_matter("---\na: 1\n---").unwrap();
        assert_eq!((fm.yaml.as_str(), body), ("a: 1\n", ""));
    }

    #[test]
    fn text_without_closed_front_matter_is_left_whole() {
        assert!(split_front_matter("--- \nnot closed\nstill text").is_none());
        assert!(split_front_matter("plain\n---\na: 1\n---\n").is_none());
        assert!(split_front_matter("---- \na\n---\n").is_none());
        assert!(split_front_matter("---").is_none());
        assert!(split_front_matter("").is_none());
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
