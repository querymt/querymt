//! Session usage model for the details popover, ported from the desktop app's
//! `session-usage.ts`: per-request / session token breakdowns, API-cost
//! accounting, and active-work time.

use serde_json::Value;

/// One token category of a request breakdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKey {
    Input,
    CacheRead,
    CacheWrite,
    Output,
    Reasoning,
}

impl TokenKey {
    pub const INPUT: [TokenKey; 3] = [TokenKey::Input, TokenKey::CacheRead, TokenKey::CacheWrite];
    pub const OUTPUT: [TokenKey; 2] = [TokenKey::Output, TokenKey::Reasoning];

    pub fn short(self) -> &'static str {
        match self {
            Self::Input => "Uncached",
            Self::CacheRead => "Read",
            Self::CacheWrite => "Write",
            Self::Output => "Generated",
            Self::Reasoning => "Reasoning",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Input => "Input not served from or written to the cache",
            Self::CacheRead => "Input reused from a cached prefix",
            Self::CacheWrite => "Input used to create a cache entry",
            Self::Output => "Generated output, excluding reasoning",
            Self::Reasoning => "Thinking tokens, separate from generated output",
        }
    }

    /// Desktop `--mode-tone-*` colors (dark, light) for each category.
    pub fn color(self, dark: bool) -> u32 {
        let (d, l) = match self {
            Self::Input => (0x93c5fd, 0x1a5fa8),
            Self::CacheRead => (0x86efac, 0x1a7f47),
            Self::CacheWrite => (0x5eead4, 0x0f766e),
            Self::Output => (0xfda36b, 0xc05621),
            Self::Reasoning => (0xc4b5fd, 0x7c3aad),
        };
        if dark { d } else { l }
    }
}

/// Disjoint token counts; `None` means the provider did not report it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

impl TokenUsage {
    pub fn get(&self, key: TokenKey) -> Option<u64> {
        match key {
            TokenKey::Input => self.input,
            TokenKey::CacheRead => self.cache_read,
            TokenKey::CacheWrite => self.cache_write,
            TokenKey::Output => self.output,
            TokenKey::Reasoning => self.reasoning,
        }
    }

    fn all(&self) -> [Option<u64>; 5] {
        [
            self.input,
            self.output,
            self.reasoning,
            self.cache_read,
            self.cache_write,
        ]
    }

    /// Live (`inputTokens`, …) or historical audit (`input_tokens`, `cache_read`, …) shape.
    pub fn read(value: &Value, historical: bool) -> Option<Self> {
        let obj = value.as_object()?;
        let n = |live: &str, hist: &str| {
            obj.get(if historical { hist } else { live })
                .and_then(Value::as_u64)
        };
        let usage = Self {
            input: n("inputTokens", "input_tokens"),
            output: n("outputTokens", "output_tokens"),
            reasoning: n("reasoningTokens", "reasoning_tokens"),
            cache_read: n("cacheReadTokens", "cache_read"),
            cache_write: n("cacheWriteTokens", "cache_write"),
        };
        usage.all().iter().any(Option::is_some).then_some(usage)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Breakdown {
    pub latest: Option<TokenUsage>,
    pub totals: Option<TokenUsage>,
    /// Some requests did not report complete usage.
    pub partial: bool,
}

impl Breakdown {
}

// ---- charts -----------------------------------------------------------------

pub struct Segment {
    pub key: TokenKey,
    pub count: Option<u64>,
    /// Share of the chart total, `0..=1`.
    pub share: f32,
}

pub struct Chart {
    pub label: &'static str,
    pub segments: Vec<Segment>,
    pub total: u64,
    /// Every category reported (and not a partial session total).
    pub complete: bool,
    /// `99% reused` / `0% reasoning` headline under the ring.
    pub takeaway: String,
}

impl Chart {
    pub fn segment(&self, key: TokenKey) -> Option<&Segment> {
        self.segments.iter().find(|s| s.key == key)
    }
}

/// Input (uncached / read / write) and Output (generated / reasoning) rings.
pub fn charts(counts: &TokenUsage, partial: bool) -> [Chart; 2] {
    let build = |label: &'static str, keys: &[TokenKey], headline: TokenKey, word: &str| {
        let total: u64 = keys.iter().map(|k| counts.get(*k).unwrap_or(0)).sum();
        let complete = !partial && keys.iter().all(|k| counts.get(*k).is_some());
        let segments = keys
            .iter()
            .map(|&key| {
                let count = counts.get(key);
                let share = if total > 0 {
                    count.unwrap_or(0) as f32 / total as f32
                } else {
                    0.
                };
                Segment { key, count, share }
            })
            .collect();
        let takeaway = if !complete {
            "Reported tokens only".to_string()
        } else if total == 0 {
            "No tokens reported".to_string()
        } else {
            let pct = (counts.get(headline).unwrap_or(0) as f64 / total as f64 * 100.).round();
            format!("{pct}% {word}")
        };
        Chart {
            label,
            segments,
            total,
            complete,
            takeaway,
        }
    };
    [
        build("Input", &TokenKey::INPUT, TokenKey::CacheRead, "reused"),
        build(
            "Output",
            &TokenKey::OUTPUT,
            TokenKey::Reasoning,
            "reasoning",
        ),
    ]
}

// ---- accounting -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub known_subtotal_usd: f64,
    pub complete_total_usd: Option<f64>,
    pub complete: bool,
    /// `empty`, `metered`, `excluded` or `mixed`.
    pub billing: String,
    pub reasons: Vec<Value>,
    pub unresolved: u64,
    pub unaccounted: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Budget {
    pub known_subtotal_usd: f64,
    pub complete_total_usd: Option<f64>,
    pub reasons: Vec<Value>,
    pub best_effort: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Accounting {
    pub session: Scope,
    pub budget: Option<Budget>,
}

fn amount(v: Option<&Value>) -> Option<f64> {
    v?.as_f64().filter(|a| a.is_finite() && *a >= 0.)
}

/// `completeTotalUsd` must be present: explicit `null` clears, omission is malformed.
fn complete_total(obj: &serde_json::Map<String, Value>) -> Option<Option<f64>> {
    match obj.get("completeTotalUsd") {
        None => None,
        Some(Value::Null) => Some(None),
        v => amount(v).map(Some),
    }
}

fn read_scope(v: Option<&Value>) -> Option<Scope> {
    let o = v?.as_object()?;
    let count = |k: &str| o.get(k).and_then(Value::as_u64);
    let billing = o
        .get("billing")
        .and_then(Value::as_str)
        .filter(|b| ["empty", "metered", "excluded", "mixed"].contains(b))?;
    let complete_total_usd = complete_total(o)?;
    for k in ["pricedAttempts", "excludedAttempts", "incompleteAttempts"] {
        count(k)?;
    }
    Some(Scope {
        known_subtotal_usd: amount(o.get("knownSubtotalUsd"))?,
        complete_total_usd,
        // A complete flag without a total cannot be shown as complete.
        complete: o.get("complete").and_then(Value::as_bool)? && complete_total_usd.is_some(),
        billing: billing.to_string(),
        reasons: o.get("reasons").and_then(Value::as_array)?.clone(),
        unresolved: count("unresolvedAttempts")?,
        unaccounted: count("unaccountedGenerations")?,
    })
}

fn read_budget(v: Option<&Value>) -> Option<Budget> {
    let o = v?.as_object()?;
    o.get("incompleteAttempts").and_then(Value::as_u64)?;
    Some(Budget {
        known_subtotal_usd: amount(o.get("knownSubtotalUsd"))?,
        complete_total_usd: complete_total(o)?,
        reasons: o.get("reasons").and_then(Value::as_array)?.clone(),
        best_effort: o.get("bestEffort").and_then(Value::as_bool)?,
    })
}

impl Accounting {
    /// `AccountingSnapshot` v1; `None` for an absent or malformed payload.
    pub fn read(v: Option<&Value>) -> Option<Self> {
        let o = v?.as_object()?;
        if o.get("version").and_then(Value::as_u64) != Some(1) || !o.contains_key("cycle") {
            return None;
        }
        let session = read_scope(o.get("session"))?;
        let budget = match o.get("cycle") {
            Some(Value::Null) => None,
            Some(cycle) => {
                cycle.get("cycleId").and_then(Value::as_str)?;
                read_scope(cycle.get("scope"))?;
                Some(read_budget(cycle.get("budget"))?)
            }
            None => None,
        };
        Some(Self { session, budget })
    }
}

fn describe_reason(reason: &Value) -> String {
    let category = |c: &str| {
        match c {
            "input" => "input",
            "output" => "output",
            "cache_read" => "cache read",
            "cache_write" => "cache write",
            "cache_write_1h" => "one-hour cache write",
            other => other,
        }
        .to_string()
    };
    match reason.get("kind").and_then(Value::as_str) {
        Some("unresolved_attempt") => "A request has no recorded outcome".into(),
        Some("partial_usage") => "An interrupted request reported only partial usage".into(),
        Some("usage_unavailable") => "A request reported no usage".into(),
        Some("invalid_settlement") => "A recorded charge could not be trusted".into(),
        Some("unaccounted_history") => "Earlier history has no recorded pricing basis".into(),
        Some("valuation") => {
            let d = reason.get("reason");
            let s = |k: &str| d.and_then(|d| d.get(k)).and_then(Value::as_str);
            let cat = s("category").map(category).unwrap_or_else(|| "a".into());
            match s("kind") {
                Some("usage_unavailable") => "A request reported no usage".into(),
                Some("pricing_unavailable") => "No price is available for the model".into(),
                Some("missing_rate") => format!("No {cat} rate is configured"),
                Some("invalid_rate") => format!("The {cat} rate is invalid"),
                Some("contradictory_write_detail") => {
                    "Cache-write usage details are contradictory".into()
                }
                Some("unsupported_dimension") => format!(
                    "Pricing dimension '{}' is not supported",
                    s("dimension").unwrap_or("unknown")
                ),
                Some("amount_overflow") => "A charge was not representable".into(),
                Some("pricing_inapplicable") => format!(
                    "No price applies to the served model '{}'",
                    s("effective_model").unwrap_or("unknown")
                ),
                _ => "A charge could not be priced".into(),
            }
        }
        _ => "Accounting is incomplete".into(),
    }
}

fn describe_reasons(reasons: &[Value], unresolved: u64, unaccounted: u64) -> String {
    let has = |k: &str| {
        reasons
            .iter()
            .any(|r| r.get("kind").and_then(Value::as_str) == Some(k))
    };
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for r in reasons {
        push(describe_reason(r));
    }
    if unresolved > 0 && !has("unresolved_attempt") {
        push(describe_reason(
            &serde_json::json!({ "kind": "unresolved_attempt" }),
        ));
    }
    if unaccounted > 0 && !has("unaccounted_history") {
        push(describe_reason(
            &serde_json::json!({ "kind": "unaccounted_history" }),
        ));
    }
    if out.is_empty() {
        "some charges are unknown".into()
    } else {
        out.join("; ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Warn,
}

/// One aligned label/value row of the cost section.
#[derive(Clone, Debug, PartialEq)]
pub struct CostLine {
    pub label: &'static str,
    pub value: String,
    /// `≥` for a lower bound, `~` for an approximation.
    pub prefix: Option<&'static str>,
    pub tone: Tone,
    /// Full explanation, shown as the tooltip.
    pub title: String,
}

const SUBSCRIPTION_NOTE: &str = "Subscription usage is excluded from this figure";

/// Cost rows: accounting is authoritative; agents without it only report a standard ACP cost.
pub fn cost_lines(accounting: Option<&Accounting>, standard_usd: Option<f64>) -> Vec<CostLine> {
    let mut lines = Vec::new();
    match accounting {
        None => {
            if let Some(usd) = standard_usd {
                lines.push(CostLine {
                    label: "Agent-reported cost",
                    value: format_cost(usd),
                    prefix: None,
                    tone: Tone::Normal,
                    title: "Cost reported by the agent; completeness is not indicated".into(),
                });
            }
        }
        Some(acc) => {
            let s = &acc.session;
            let clean = s.reasons.is_empty() && s.unresolved == 0;
            let excl = s.billing == "mixed" || s.billing == "excluded";
            let label = if excl {
                "API cost (excl. subscription)"
            } else {
                "API cost"
            };
            let note = if excl {
                format!(". {SUBSCRIPTION_NOTE}")
            } else {
                String::new()
            };
            if s.billing == "empty" && clean {
            } else if s.billing == "excluded" && clean {
                // All-excluded usage is never a known zero-dollar invoice.
                lines.push(CostLine {
                    label: "API cost",
                    value: "Subscription".into(),
                    prefix: None,
                    tone: Tone::Muted,
                    title: "Subscription usage excluded: no metered API charges are reported"
                        .into(),
                });
            } else if let Some(total) = s.complete_total_usd.filter(|_| s.complete) {
                lines.push(CostLine {
                    label,
                    value: format_cost(total),
                    prefix: None,
                    tone: Tone::Normal,
                    title: format!("Estimated API cost for metered requests, independent of the selected token scope{note}"),
                });
            } else {
                let known = format_cost(s.known_subtotal_usd);
                lines.push(CostLine {
                    label,
                    title: format!(
                        "At least {known} - incomplete: {}{note}",
                        describe_reasons(&s.reasons, s.unresolved, s.unaccounted)
                    ),
                    value: known,
                    prefix: Some("≥"),
                    tone: Tone::Warn,
                });
            }
            if let Some(b) = acc
                .budget
                .as_ref()
                .filter(|b| b.known_subtotal_usd > 0. || b.best_effort)
            {
                lines.push(CostLine {
                    label: "Cycle budget",
                    value: format_cost(b.complete_total_usd.unwrap_or(b.known_subtotal_usd)),
                    prefix: b.best_effort.then_some("~"),
                    tone: Tone::Normal,
                    title: if b.best_effort {
                        format!("Cycle budget valuation is best-effort: {}", describe_reasons(&b.reasons, 0, 0))
                    } else {
                        "Spend counted against this cycle's price limit (manual rates when configured)".into()
                    },
                });
            }
        }
    }
    lines
}

// ---- formatting -------------------------------------------------------------

fn trim_decimal(v: f64) -> String {
    if v >= 10. {
        format!("{v:.0}")
    } else {
        let s = format!("{v:.1}");
        s.strip_suffix(".0").map(str::to_owned).unwrap_or(s)
    }
}

/// `115k`, `1.2m`, `116`.
pub fn format_tokens(n: u64) -> String {
    match n {
        1_000_000.. => format!("{}m", trim_decimal(n as f64 / 1e6)),
        1_000.. => format!("{}k", trim_decimal(n as f64 / 1e3)),
        _ => n.to_string(),
    }
}

/// `114,712` (en-US grouping).
pub fn format_exact(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn format_cost(usd: f64) -> String {
    if usd == 0. {
        "$0.00".into()
    } else if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// `17m 15s`, `2h 4m`, `9s`.
pub fn format_duration(ms: f64) -> String {
    let secs = (ms.max(0.) / 1000.).floor() as u64;
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn charts_match_desktop_takeaways() {
        let counts = TokenUsage {
            input: Some(1_000),
            cache_read: Some(99_000),
            cache_write: Some(0),
            output: Some(116),
            reasoning: Some(0),
        };
        let [input, output] = charts(&counts, false);
        assert_eq!(input.total, 100_000);
        assert_eq!(input.takeaway, "99% reused");
        assert_eq!(output.takeaway, "0% reasoning");
        assert!((input.segment(TokenKey::CacheRead).unwrap().share - 0.99).abs() < 1e-6);
        let [partial, _] = charts(&counts, true);
        assert_eq!(partial.takeaway, "Reported tokens only");
    }

    fn scope(billing: &str, complete: bool, total: Value) -> Value {
        json!({ "knownSubtotalUsd": 1.5, "completeTotalUsd": total, "complete": complete, "billing": billing, "reasons": [],
            "pricedAttempts": 1, "excludedAttempts": 0, "unresolvedAttempts": 0, "incompleteAttempts": 0, "unaccountedGenerations": 0 })
    }

    #[test]
    fn cost_lines_follow_accounting_states() {
        let read = |s: Value| {
            Accounting::read(Some(&json!({ "version": 1, "session": s, "cycle": null }))).unwrap()
        };
        let sub = cost_lines(Some(&read(scope("excluded", false, Value::Null))), None);
        assert_eq!(
            (sub[0].label, sub[0].value.as_str(), sub[0].tone),
            ("API cost", "Subscription", Tone::Muted)
        );
        let est = cost_lines(Some(&read(scope("metered", true, json!(2.345)))), Some(9.));
        assert_eq!((est[0].value.as_str(), est[0].prefix), ("$2.35", None));
        let inc = cost_lines(Some(&read(scope("mixed", false, Value::Null))), None);
        assert_eq!(
            (inc[0].label, inc[0].prefix, inc[0].tone),
            ("API cost (excl. subscription)", Some("≥"), Tone::Warn)
        );
        assert!(cost_lines(Some(&read(scope("empty", false, Value::Null))), None).is_empty());
        assert_eq!(cost_lines(None, Some(0.004))[0].value, "$0.0040");
        // Omitting `completeTotalUsd` is malformed.
        let mut s = scope("metered", true, json!(1));
        s.as_object_mut().unwrap().remove("completeTotalUsd");
        assert!(
            Accounting::read(Some(&json!({ "version": 1, "session": s, "cycle": null }))).is_none()
        );
    }

    #[test]
    fn formats_like_desktop() {
        assert_eq!(format_tokens(114_712), "115k");
        assert_eq!(format_tokens(885_288), "885k");
        assert_eq!(format_tokens(1_234), "1.2k");
        assert_eq!(format_tokens(2_000_000), "2m");
        assert_eq!(format_tokens(116), "116");
        assert_eq!(format_exact(1_234_567), "1,234,567");
        assert_eq!(format_exact(999), "999");
        assert_eq!(format_duration(1_035_000.), "17m 15s");
        assert_eq!(format_duration(7_440_000.), "2h 4m");
        assert_eq!(format_duration(9_500.), "9s");
    }
}
