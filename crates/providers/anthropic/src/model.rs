//! Claude model-ID parsing and capability classification.
//!
//! Anthropic's thinking, sampling and tool-choice rules depend on the model
//! generation. Rather than keeping a list of names, model IDs (including
//! Bedrock, Vertex and router forms) are normalised, parsed into
//! family and version, and classified from those.

/// Model family parsed from a Claude model ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
    Other,
}

/// A parsed Claude model ID. `major`/`minor` are absent for unversioned IDs
/// such as `claude-mythos-preview`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClaudeModel {
    pub family: Family,
    pub major: Option<u32>,
    pub minor: Option<u32>,
}

/// How a model takes part in extended thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingClass {
    /// Adaptive thinking is on unless the request says otherwise.
    DefaultOnAdaptive,
    /// Adaptive thinking exists but must be requested.
    OptInAdaptive,
    /// Only `thinking: {type: "enabled", budget_tokens}` is available.
    Budget,
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Strip a trailing Bedrock-style `-v<N>[:<M>]` revision suffix.
fn strip_revision_suffix(id: &str) -> &str {
    if let Some(pos) = id.rfind("-v") {
        let rest = &id[pos + 2..];
        let (major, minor) = match rest.split_once(':') {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };
        if is_digits(major) && minor.is_none_or(is_digits) {
            return &id[..pos];
        }
    }
    id
}

/// Strip a trailing `-YYYYMMDD` date.
fn strip_date_suffix(id: &str) -> &str {
    if let Some(pos) = id.rfind('-') {
        let rest = &id[pos + 1..];
        if rest.len() == 8 && is_digits(rest) {
            return &id[..pos];
        }
    }
    id
}

/// Normalise a model ID for parsing.
///
/// Lower-cases it, removes any routing/provider prefix up to and including
/// `anthropic.` or `anthropic/`, and removes `-vN:M`, `@...` and `-YYYYMMDD`
/// suffixes. Dots in versions (`4.6`) are treated as dashes.
pub(crate) fn normalize_model_id(id: &str) -> String {
    let mut s = id.trim().to_ascii_lowercase();
    if let Some(pos) = ["anthropic.", "anthropic/"]
        .iter()
        .filter_map(|marker| s.find(marker).map(|p| p + marker.len()))
        .min()
    {
        s = s[pos..].to_string();
    }
    if let Some(pos) = s.find('@') {
        s.truncate(pos);
    }
    let s = strip_revision_suffix(&s);
    let s = strip_date_suffix(s);
    s.replace('.', "-")
}

fn parse_version_number(token: &str) -> Option<u32> {
    // Version components are one or two digits; longer numbers are dates.
    (is_digits(token) && token.len() <= 2)
        .then(|| token.parse().ok())
        .flatten()
}

fn family_from_token(token: &str) -> Option<Family> {
    match token {
        "opus" => Some(Family::Opus),
        "sonnet" => Some(Family::Sonnet),
        "haiku" => Some(Family::Haiku),
        "fable" => Some(Family::Fable),
        "mythos" => Some(Family::Mythos),
        _ => None,
    }
}

/// Parse a model ID. Returns `None` when it is not a recognisable Claude ID.
///
/// Recognised forms: `claude-{family}-{major}[-{minor}]`,
/// `claude-{major}[-{minor}]-{family}`, and unversioned `claude-{family}-...`
/// such as `claude-mythos-preview`.
pub(crate) fn parse_claude_model(id: &str) -> Option<ClaudeModel> {
    let normalized = normalize_model_id(id);
    let mut tokens = normalized.split('-');
    if tokens.next()? != "claude" {
        return None;
    }
    let rest: Vec<&str> = tokens.collect();
    let first = *rest.first()?;

    if let Some(major) = parse_version_number(first) {
        // Legacy form: claude-{major}[-{minor}]-{family}
        let mut idx = 1;
        let minor = rest.get(idx).and_then(|t| parse_version_number(t));
        if minor.is_some() {
            idx += 1;
        }
        let family = rest
            .get(idx)
            .and_then(|t| family_from_token(t))
            .unwrap_or(Family::Other);
        return Some(ClaudeModel {
            family,
            major: Some(major),
            minor,
        });
    }

    if !first.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let family = family_from_token(first).unwrap_or(Family::Other);
    let major = rest.get(1).and_then(|t| parse_version_number(t));
    let minor = major.and_then(|_| rest.get(2).and_then(|t| parse_version_number(t)));
    if major.is_none() && family == Family::Other {
        return None;
    }
    Some(ClaudeModel {
        family,
        major,
        minor,
    })
}

impl ClaudeModel {
    /// Whether the parsed version is at least `major.minor`. Unversioned
    /// models never satisfy a version bound.
    fn version_at_least(&self, major: u32, minor: u32) -> bool {
        self.major
            .is_some_and(|m| (m, self.minor.unwrap_or(0)) >= (major, minor))
    }

    fn is_major_at_least(&self, major: u32) -> bool {
        self.major.is_some_and(|m| m >= major)
    }

    /// Class from the model ID alone.
    pub fn thinking_class(&self) -> ThinkingClass {
        if self.is_major_at_least(5) || matches!(self.family, Family::Fable | Family::Mythos) {
            return ThinkingClass::DefaultOnAdaptive;
        }
        if matches!(self.family, Family::Opus | Family::Sonnet)
            && self.major == Some(4)
            && matches!(self.minor, Some(6..=8))
        {
            return ThinkingClass::OptInAdaptive;
        }
        ThinkingClass::Budget
    }

    /// Strict tool use (`strict: true`) is available on Opus and Sonnet 4.5+,
    /// Haiku 4.5+, and every Fable and Mythos model. Unversioned Opus, Sonnet
    /// and Haiku IDs and unrecognised families do not qualify.
    pub fn supports_strict_tools(&self) -> bool {
        match self.family {
            Family::Opus | Family::Sonnet | Family::Haiku => self.version_at_least(4, 5),
            Family::Fable | Family::Mythos => true,
            Family::Other => false,
        }
    }

    /// Opus 4.7+, every Claude 5+, Fable and Mythos reject non-default
    /// `temperature`, `top_p` and `top_k`.
    pub fn rejects_sampling_params(&self) -> bool {
        self.is_major_at_least(5)
            || matches!(self.family, Family::Fable | Family::Mythos)
            || (self.family == Family::Opus && self.version_at_least(4, 7))
    }

    /// Opus 5.5+, Sonnet 5.5+, Fable 5.1+ and Mythos 5.1+ reject a forced
    /// `tool_choice` (`any` or a named tool).
    pub fn rejects_forced_tool_choice(&self) -> bool {
        match self.family {
            Family::Opus | Family::Sonnet => self.version_at_least(5, 5),
            Family::Fable | Family::Mythos => self.version_at_least(5, 1),
            _ => false,
        }
    }
}

/// Resolve the thinking class for a model ID, honouring the configuration
/// override (`Some(true)` = opt-in adaptive, `Some(false)` = budget).
/// IDs that cannot be parsed are budget models.
pub(crate) fn resolve_thinking_class(
    model: Option<&ClaudeModel>,
    adaptive_override: Option<bool>,
) -> ThinkingClass {
    match adaptive_override {
        Some(true) => ThinkingClass::OptInAdaptive,
        Some(false) => ThinkingClass::Budget,
        None => model.map_or(ThinkingClass::Budget, ClaudeModel::thinking_class),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> ClaudeModel {
        parse_claude_model(id).unwrap_or_else(|| panic!("{id} should parse"))
    }

    #[test]
    fn normalizes_router_and_version_suffixes() {
        assert_eq!(
            normalize_model_id("us.anthropic.claude-opus-4-7-v1:0"),
            "claude-opus-4-7"
        );
        assert_eq!(
            normalize_model_id("claude-sonnet-4-6@20260101"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_model_id("anthropic/claude-sonnet-5-5"),
            "claude-sonnet-5-5"
        );
        assert_eq!(
            normalize_model_id("claude-3-7-sonnet-20250219"),
            "claude-3-7-sonnet"
        );
        assert_eq!(
            normalize_model_id("anthropic.claude-3-7-sonnet-20250219-v1:0"),
            "claude-3-7-sonnet"
        );
        assert_eq!(normalize_model_id("Claude-Opus-4.6"), "claude-opus-4-6");
    }

    #[test]
    fn parses_current_and_legacy_forms() {
        assert_eq!(
            model("us.anthropic.claude-opus-4-7-v1:0"),
            ClaudeModel {
                family: Family::Opus,
                major: Some(4),
                minor: Some(7)
            }
        );
        assert_eq!(
            model("claude-sonnet-4-6@20260101"),
            model("claude-sonnet-4-6")
        );
        assert_eq!(
            model("anthropic/claude-sonnet-5-5"),
            ClaudeModel {
                family: Family::Sonnet,
                major: Some(5),
                minor: Some(5)
            }
        );
        assert_eq!(
            model("claude-3-7-sonnet-20250219"),
            ClaudeModel {
                family: Family::Sonnet,
                major: Some(3),
                minor: Some(7)
            }
        );
        assert_eq!(
            model("claude-3-opus-20240229"),
            ClaudeModel {
                family: Family::Opus,
                major: Some(3),
                minor: None
            }
        );
        assert_eq!(
            model("claude-sonnet-4-20250514"),
            ClaudeModel {
                family: Family::Sonnet,
                major: Some(4),
                minor: None
            }
        );
        assert_eq!(
            model("claude-mythos-preview"),
            ClaudeModel {
                family: Family::Mythos,
                major: None,
                minor: None
            }
        );
    }

    #[test]
    fn unparseable_ids_return_none() {
        assert_eq!(parse_claude_model("my-proxy-model"), None);
        assert_eq!(parse_claude_model("gpt-4o"), None);
        assert_eq!(parse_claude_model("claude"), None);
        assert_eq!(parse_claude_model(""), None);
    }

    #[test]
    fn class_table() {
        use ThinkingClass::*;
        let cases = [
            ("claude-opus-4-5", Budget),
            ("claude-sonnet-4-5", Budget),
            ("claude-3-7-sonnet-20250219", Budget),
            ("claude-opus-4-1", Budget),
            ("claude-sonnet-4-6", OptInAdaptive),
            ("claude-sonnet-4-6-20260101", OptInAdaptive),
            ("claude-opus-4-6", OptInAdaptive),
            ("claude-opus-4-7", OptInAdaptive),
            ("claude-opus-4-8", OptInAdaptive),
            ("claude-haiku-4-6", Budget),
            ("claude-opus-5", DefaultOnAdaptive),
            ("claude-sonnet-5-5", DefaultOnAdaptive),
            ("claude-haiku-5-5", DefaultOnAdaptive),
            ("claude-fable-5-1", DefaultOnAdaptive),
            ("claude-mythos-preview", DefaultOnAdaptive),
        ];
        for (id, expected) in cases {
            assert_eq!(model(id).thinking_class(), expected, "{id}");
        }
    }

    #[test]
    fn unparseable_is_budget_and_override_applies() {
        assert_eq!(
            resolve_thinking_class(parse_claude_model("my-proxy-model").as_ref(), None),
            ThinkingClass::Budget
        );
        assert_eq!(
            resolve_thinking_class(parse_claude_model("my-proxy-model").as_ref(), Some(true)),
            ThinkingClass::OptInAdaptive
        );
        let sonnet5 = parse_claude_model("claude-sonnet-5-5");
        assert_eq!(
            resolve_thinking_class(sonnet5.as_ref(), Some(true)),
            ThinkingClass::OptInAdaptive
        );
        assert_eq!(
            resolve_thinking_class(sonnet5.as_ref(), Some(false)),
            ThinkingClass::Budget
        );
        let sonnet45 = parse_claude_model("claude-sonnet-4-5");
        assert_eq!(
            resolve_thinking_class(sonnet45.as_ref(), Some(true)),
            ThinkingClass::OptInAdaptive
        );
    }

    #[test]
    fn strict_tool_support_by_family_and_version() {
        let cases = [
            ("claude-opus-4-5", true),
            ("claude-opus-4-8", true),
            ("claude-sonnet-4-5-20250929", true),
            ("claude-sonnet-5-5", true),
            ("claude-haiku-4-5", true),
            ("claude-haiku-5", true),
            ("claude-fable-5", true),
            ("claude-mythos-preview", true),
            ("claude-sonnet-4-0", false),
            ("claude-opus-4-1", false),
            ("claude-haiku-4-0", false),
            ("claude-3-7-sonnet-20250219", false),
            ("claude-3-5-haiku-20241022", false),
            ("claude-instant-1", false),
        ];
        for (id, expected) in cases {
            let parsed = parse_claude_model(id);
            assert_eq!(
                parsed.is_some_and(|m| m.supports_strict_tools()),
                expected,
                "{id}"
            );
        }
        // Unrecognisable IDs never parse, so they never qualify.
        assert!(parse_claude_model("my-proxy-model").is_none());
    }

    #[test]
    fn sampling_and_forced_tool_predicates() {
        let rejects_sampling = [
            ("claude-opus-4-6", false),
            ("claude-sonnet-4-6", false),
            ("claude-sonnet-4-8", false),
            ("claude-opus-4-7", true),
            ("claude-opus-4-8", true),
            ("claude-opus-5", true),
            ("claude-sonnet-5-5", true),
            ("claude-haiku-5-5", true),
            ("claude-fable-5-1", true),
            ("claude-mythos-preview", true),
            ("claude-sonnet-4-5", false),
        ];
        for (id, expected) in rejects_sampling {
            assert_eq!(model(id).rejects_sampling_params(), expected, "{id}");
        }

        let rejects_forced = [
            ("claude-opus-5", false),
            ("claude-opus-5-4", false),
            ("claude-opus-5-5", true),
            ("claude-sonnet-5-5", true),
            ("claude-sonnet-5-4", false),
            ("claude-haiku-5-5", false),
            ("claude-fable-5", false),
            ("claude-fable-5-1", true),
            ("claude-mythos-5-1", true),
            ("claude-opus-4-8", false),
            ("claude-sonnet-4-5", false),
        ];
        for (id, expected) in rejects_forced {
            assert_eq!(model(id).rejects_forced_tool_choice(), expected, "{id}");
        }
    }
}
