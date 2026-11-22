//! Agent log (desktop `AgentLogEntry` / `SidecarLogList`): what this app
//! notes about an agent (connects, failures) and what the agent process
//! writes to stderr. The agent's lines are `tracing` output, so each is
//! read for its time, level and target, with terminal colors removed.

use std::collections::VecDeque;

/// Severity of an entry; agent lines carry their own, app notes say theirs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }

    fn parse(token: &str) -> Option<Self> {
        Some(match token {
            "ERROR" => Self::Error,
            "WARN" => Self::Warn,
            "INFO" => Self::Info,
            "DEBUG" => Self::Debug,
            "TRACE" => Self::Trace,
            _ => return None,
        })
    }
}

/// Who wrote an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// This app: connecting, failed requests, transport notes.
    App,
    /// The agent process's stderr.
    Agent,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogEntry {
    /// Increases by one per entry, across clears.
    pub seq: u64,
    /// Epoch ms: the agent's own timestamp when its line has one.
    pub at_ms: f64,
    pub source: LogSource,
    pub level: Level,
    /// `tracing` target (module path) of an agent line.
    pub target: Option<String>,
    pub text: String,
}

/// The most recent entries of one agent.
#[derive(Debug)]
pub struct AgentLog {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
    /// Level of the last agent line: continuation lines (a panic's
    /// backtrace, multi-line messages) belong to it.
    agent_level: Level,
}

impl Default for AgentLog {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 1,
            agent_level: Level::Info,
        }
    }
}

impl AgentLog {
    /// Entries kept; older ones drop off.
    pub const CAP: usize = 5000;

    pub fn entries(&self) -> &VecDeque<LogEntry> {
        &self.entries
    }

    /// Sequence number of the newest entry ever added (0: none yet).
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn push_app(&mut self, level: Level, text: impl Into<String>) {
        self.push(
            crate::util::now_ms(),
            LogSource::App,
            level,
            None,
            text.into(),
        );
    }

    /// One stderr line of the agent process.
    pub fn push_agent(&mut self, raw: &str) {
        let line = strip_ansi(raw);
        let line = line.trim_end();
        if line.trim().is_empty() {
            return;
        }
        let (at, level, target, text) = match parse_tracing(line) {
            Some(parsed) => {
                self.agent_level = parsed.level;
                (
                    parsed.at_ms,
                    parsed.level,
                    parsed.target,
                    parsed.text.to_string(),
                )
            }
            None => (None, self.agent_level, None, line.to_string()),
        };
        let at = at.unwrap_or_else(crate::util::now_ms);
        self.push(at, LogSource::Agent, level, target, text);
    }

    fn push(
        &mut self,
        at_ms: f64,
        source: LogSource,
        level: Level,
        target: Option<String>,
        text: String,
    ) {
        self.entries.push_back(LogEntry {
            seq: self.next_seq,
            at_ms,
            source,
            level,
            target,
            text,
        });
        self.next_seq += 1;
        while self.entries.len() > Self::CAP {
            self.entries.pop_front();
        }
    }
}

impl super::AgentsStore {
    /// Forget the log lines of `agent` so far.
    pub fn clear_agent_log(&mut self, agent: &str, cx: &mut gpui_kit::Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(agent) {
            rt.log.clear();
            cx.notify();
        }
    }
}

/// Remove ANSI escape sequences (colors, cursor moves).
pub fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\x1b') {
        return s.into();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: parameters, then one final byte in `@`..=`~`.
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC `\`.
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out.into()
}

struct Tracing<'a> {
    at_ms: Option<f64>,
    level: Level,
    target: Option<String>,
    text: &'a str,
}

/// `tracing_subscriber::fmt` output: `[TIMESTAMP] LEVEL [target: ]message`.
fn parse_tracing(line: &str) -> Option<Tracing<'_>> {
    let rest = line.trim_start();
    let (first, after) = split_token(rest);
    let (at_ms, level, rest) = match Level::parse(first) {
        Some(level) => (None, level, after),
        None => {
            let at = parse_timestamp(first)?;
            let (token, after) = split_token(after);
            (Some(at), Level::parse(token)?, after)
        }
    };
    // `target: message`, possibly behind span context (`span{a=1}:target:`).
    let (target, text) = match rest.find(": ") {
        Some(ix) if !rest[..ix].contains(' ') || rest[..ix].contains('{') => {
            let head = &rest[..ix];
            let target = head.rfind("}:").map_or(head, |span| &head[span + 2..]);
            (Some(target.to_string()), &rest[ix + 2..])
        }
        _ => (None, rest),
    };
    Some(Tracing {
        at_ms,
        level,
        target,
        text,
    })
}

fn split_token(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(ix) => (&s[..ix], s[ix..].trim_start()),
        None => (s, ""),
    }
}

/// `2026-11-20T10:42:07.123456Z` to epoch ms, keeping milliseconds.
fn parse_timestamp(token: &str) -> Option<f64> {
    if token.len() < 19 || token.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let secs = crate::util::parse_rfc3339_ms(token)?;
    let fraction = token[19..]
        .strip_prefix('.')
        .map(|f| {
            f.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap_or_default();
    let ms = format!("0.{fraction}").parse::<f64>().unwrap_or(0.) * 1000.;
    Some(secs + ms.floor())
}

/// `10:42:07.123` (UTC) of epoch ms.
pub fn clock(ms: f64) -> String {
    let ms = ms.max(0.) as u64;
    let day = ms % 86_400_000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        day / 3_600_000,
        day / 60_000 % 60,
        day / 1000 % 60,
        day % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tracing_lines_and_drops_colors() {
        let mut log = AgentLog::default();
        log.push_agent(
            "\x1b[2m2026-11-20T10:42:07.123456Z\x1b[0m \x1b[31mERROR\x1b[0m \
             \x1b[2mquerymt_agent::session\x1b[0m\x1b[2m:\x1b[0m prompt failed: boom",
        );
        let e = &log.entries()[0];
        assert_eq!(e.level, Level::Error);
        assert_eq!(e.source, LogSource::Agent);
        assert_eq!(e.target.as_deref(), Some("querymt_agent::session"));
        assert_eq!(e.text, "prompt failed: boom");
        assert_eq!(clock(e.at_ms), "10:42:07.123");

        // A backtrace line continues the error.
        log.push_agent("   0: std::panicking::begin_panic");
        assert_eq!(log.entries()[1].level, Level::Error);
        assert_eq!(log.entries()[1].text, "   0: std::panicking::begin_panic");

        // Without a timestamp; span context before the target.
        log.push_agent(" WARN conn{id=3}:querymt::mesh: peer gone");
        let e = &log.entries()[2];
        assert_eq!(
            (e.level, e.target.as_deref()),
            (Level::Warn, Some("querymt::mesh"))
        );
        assert_eq!(e.text, "peer gone");
        log.push_agent("   ");
        assert_eq!(log.entries().len(), 3);
    }

    #[test]
    fn keeps_the_newest_entries() {
        let mut log = AgentLog::default();
        for i in 0..AgentLog::CAP + 5 {
            log.push_app(Level::Info, format!("line {i}"));
        }
        assert_eq!(log.entries().len(), AgentLog::CAP);
        assert_eq!(log.entries()[0].text, "line 5");
        assert_eq!(log.last_seq(), (AgentLog::CAP + 5) as u64);
        log.clear();
        assert_eq!(log.last_seq(), (AgentLog::CAP + 5) as u64);
    }
}
