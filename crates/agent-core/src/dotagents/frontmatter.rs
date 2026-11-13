//! Shared frontmatter-plus-Markdown parsing for protocol content files.
//!
//! Protocol content files (`agents.md`, `system-prompt.md`, skill/agent/task/
//! memory documents) are YAML frontmatter followed by a Markdown body. The
//! protocol describes a deliberately small frontmatter surface: scalar values,
//! quoted values, comma-separated list values, and JSON-array list values.
//!
//! This module parses the frontmatter fence itself, then converts the raw YAML
//! map into `serde_json::Value`s while normalizing the protocol's list forms.
//! Unknown fields are retained so that forward-compatible documents remain
//! usable. Parsing never performs IO beyond the caller-supplied text.

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use std::collections::BTreeMap;

/// The result of parsing a frontmatter-plus-Markdown document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFrontmatter {
    /// Frontmatter fields as JSON values (lists normalized).
    pub fields: BTreeMap<String, serde_json::Value>,
    /// The Markdown body with the frontmatter fence removed.
    pub body: String,
    /// Whether a frontmatter fence was present.
    pub had_frontmatter: bool,
}

/// Parse a content document into frontmatter fields and a Markdown body.
///
/// A document without a leading `---` fence is treated as body-only and is not
/// an error. A document with an unterminated fence is an error.
pub fn parse_frontmatter_markdown(
    text: &str,
) -> Result<ParsedFrontmatter, Box<DotagentsDiagnostic>> {
    let normalized = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = strip_fence(normalized) else {
        return Ok(ParsedFrontmatter {
            fields: BTreeMap::new(),
            body: normalized.to_string(),
            had_frontmatter: false,
        });
    };

    let (frontmatter, body) = split_frontmatter(rest)?;
    let fields = parse_yaml_map(&frontmatter, 2)?;

    Ok(ParsedFrontmatter {
        fields,
        body: body.to_string(),
        had_frontmatter: true,
    })
}

/// Split off the body after an opening fence.
fn strip_fence(text: &str) -> Option<&str> {
    let trimmed_start = text.trim_start_matches(['\u{feff}', '\n', '\r']);
    trimmed_start
        .strip_prefix("---\n")
        .or_else(|| trimmed_start.strip_prefix("---\r\n"))
}

/// Find the closing fence and return `(frontmatter, body)`.
fn split_frontmatter(rest: &str) -> Result<(String, String), Box<DotagentsDiagnostic>> {
    let mut frontmatter = String::new();
    let mut lines = rest.split_inclusive('\n');
    let mut consumed = 0usize;

    for line in lines.by_ref() {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" || trimmed == "..." {
            consumed += line.len();
            let body = rest.get(consumed..).unwrap_or("").to_string();
            // Drop a single leading newline from the body for stable content.
            let body = body
                .strip_prefix('\n')
                .or_else(|| body.strip_prefix("\r\n"))
                .unwrap_or(&body)
                .to_string();
            return Ok((frontmatter, body));
        }
        frontmatter.push_str(line);
        consumed += line.len();
    }

    Err(Box::new(DotagentsDiagnostic::error(
        DotagentsDiagnosticCode::ParseError,
        "frontmatter opening `---` fence is not terminated by a closing `---`",
    )))
}

/// Parse a YAML frontmatter block into a JSON object, normalizing list forms.
///
/// `base_line` is the 1-based document line number of the first line of
/// `frontmatter`, used to produce document-relative diagnostics.
fn parse_yaml_map(
    frontmatter: &str,
    base_line: usize,
) -> Result<BTreeMap<String, serde_json::Value>, Box<DotagentsDiagnostic>> {
    let mut fields: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut current_key: Option<String> = None;
    let mut current_indent: Option<usize> = None;

    for (line_number, raw_line) in frontmatter.lines().enumerate() {
        let doc_line = base_line + line_number;
        let line = raw_line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        let indent = line.len() - line.trim_start().len();
        let content = line.trim_start();

        // A nested continuation line that is a YAML list item under the last
        // key (e.g. `tags:\n  - a\n  - b`).
        if let Some(stripped) = content.strip_prefix("- ") {
            if let (Some(key), Some(key_indent)) = (&current_key, current_indent)
                && indent > key_indent
            {
                let value = normalize_scalar(stripped.trim());
                // Upgrade a placeholder `Null` (from `key:`) into an array
                // so nested YAML list items are collected correctly.
                let entry = fields
                    .entry(key.clone())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                if entry.is_null() {
                    *entry = serde_json::Value::Array(Vec::new());
                }
                if let serde_json::Value::Array(items) = entry {
                    items.push(value);
                }
                continue;
            }
            return Err(Box::new(DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!("unexpected list item at frontmatter line {doc_line}"),
            )));
        }

        let Some((key, value)) = content.split_once(':') else {
            return Err(Box::new(DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!("expected `key: value` at frontmatter line {doc_line}"),
            )));
        };

        let key = unquote(key.trim());
        if key.is_empty() {
            return Err(Box::new(DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!("empty frontmatter key at line {doc_line}"),
            )));
        }

        let raw_value = value.trim();
        if raw_value.is_empty() {
            // Begin a (possibly nested) block; list items may follow.
            current_key = Some(key.clone());
            current_indent = Some(indent);
            fields.entry(key).or_insert(serde_json::Value::Null);
            continue;
        }

        let parsed = normalize_value(raw_value);
        fields.insert(key, parsed);
        current_key = None;
        current_indent = None;
    }

    Ok(fields)
}

/// Normalize a frontmatter value, expanding protocol list forms.
fn normalize_value(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();

    // JSON-array list form: `tags: ["a", "b"]`.
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        if let Ok(serde_json::Value::Array(items)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            return serde_json::Value::Array(items.into_iter().map(json_scalar_to_value).collect());
        }
        // YAML flow sequence without JSON quoting: `tags: [a, b]`. This is not
        // valid JSON, but the protocol's JSON-array list form is commonly
        // written this way, so split the inner elements on commas.
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.trim().is_empty() {
            return serde_json::Value::Array(Vec::new());
        }
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.iter().all(|part| !part.is_empty()) {
            return serde_json::Value::Array(
                parts.iter().map(|part| normalize_scalar(part)).collect(),
            );
        }
    }

    // Preserve scalar text verbatim. List-aware accessors split CSV values so
    // string fields such as descriptions can safely contain commas.
    normalize_scalar(trimmed)
}

/// Normalize a single scalar component into a JSON value.
fn normalize_scalar(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if is_quoted(trimmed) {
        return serde_json::Value::String(unquote(trimmed));
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "true" => return serde_json::Value::Bool(true),
        "false" => return serde_json::Value::Bool(false),
        "null" | "~" => return serde_json::Value::Null,
        _ => {}
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        return serde_json::Value::from(number);
    }
    if let Ok(number) = trimmed.parse::<u64>() {
        return serde_json::Value::from(number);
    }
    if let Ok(number) = trimmed.parse::<f64>()
        && number.is_finite()
    {
        return serde_json::Value::from(number);
    }
    serde_json::Value::String(trimmed.to_string())
}

fn json_scalar_to_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => value,
        other => other,
    }
}

fn is_quoted(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
}

fn unquote(value: &str) -> String {
    if is_quoted(value) {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// Read a string field from parsed frontmatter, trimming and ignoring empties.
pub fn field_string(fields: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<String> {
    match fields.get(key) {
        Some(serde_json::Value::String(value)) => {
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }
        Some(serde_json::Value::Number(number)) => Some(number.to_string()),
        Some(serde_json::Value::Bool(value)) => Some(value.to_string()),
        _ => None,
    }
}

/// Read a boolean field, accepting YAML booleans and `"true"`/`"false"`.
pub fn field_bool(fields: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<bool> {
    match fields.get(key) {
        Some(serde_json::Value::Bool(value)) => Some(*value),
        Some(serde_json::Value::String(value)) => {
            match value.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" => Some(true),
                "false" | "no" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Read an unsigned integer field.
pub fn field_u64(fields: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<u64> {
    match fields.get(key) {
        Some(serde_json::Value::Number(number)) => number.as_u64(),
        Some(serde_json::Value::String(value)) => value.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// Read a list field, accepting arrays and CSV strings.
pub fn field_list(fields: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<Vec<String>> {
    match fields.get(key) {
        Some(serde_json::Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|item| match item {
                    serde_json::Value::String(value) => Some(value.clone()),
                    serde_json::Value::Number(number) => Some(number.to_string()),
                    serde_json::Value::Bool(value) => Some(value.to_string()),
                    _ => None,
                })
                .collect(),
        ),
        Some(serde_json::Value::String(value)) => {
            if value.trim().is_empty() {
                return Some(Vec::new());
            }
            Some(
                value
                    .split(',')
                    .map(|part| part.trim().to_string())
                    .filter(|part| !part.is_empty())
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Whether a field is present and not null.
pub fn field_present(fields: &BTreeMap<String, serde_json::Value>, key: &str) -> bool {
    fields
        .get(key)
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_frontmatter_and_body() {
        let doc = "---\nname: review\ndescription: Review code\n---\n# Body\n\nInstructions.\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert!(parsed.had_frontmatter);
        assert_eq!(
            field_string(&parsed.fields, "name").as_deref(),
            Some("review")
        );
        assert_eq!(
            field_string(&parsed.fields, "description").as_deref(),
            Some("Review code")
        );
        assert!(parsed.body.contains("# Body"));
        assert!(parsed.body.contains("Instructions."));
        // Frontmatter must not leak into the body.
        assert!(!parsed.body.contains("name: review"));
    }

    #[test]
    fn treats_document_without_frontmatter_as_body() {
        let doc = "# Just a body\n\nNo frontmatter here.\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert!(!parsed.had_frontmatter);
        assert!(parsed.fields.is_empty());
        assert_eq!(parsed.body, doc);
    }

    #[test]
    fn accepts_quoted_scalars() {
        let doc = "---\nname: \"quoted name\"\nother: 'single'\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(
            field_string(&parsed.fields, "name").as_deref(),
            Some("quoted name")
        );
        assert_eq!(
            field_string(&parsed.fields, "other").as_deref(),
            Some("single")
        );
    }

    #[test]
    fn accepts_csv_list_values() {
        let doc = "---\ntags: alpha, beta, gamma\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(
            field_list(&parsed.fields, "tags"),
            Some(vec![
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string()
            ])
        );
    }

    #[test]
    fn preserves_commas_in_unquoted_string_fields() {
        let doc = "---\ntitle: Plan, build, ship\ndescription: Fast, safe delivery\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(
            field_string(&parsed.fields, "title").as_deref(),
            Some("Plan, build, ship")
        );
        assert_eq!(
            field_string(&parsed.fields, "description").as_deref(),
            Some("Fast, safe delivery")
        );
    }

    #[test]
    fn accepts_json_array_list_values() {
        let doc = "---\ntags: [\"one\", \"two\"]\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(
            field_list(&parsed.fields, "tags"),
            Some(vec!["one".to_string(), "two".to_string()])
        );
    }

    #[test]
    fn accepts_nested_yaml_list_values() {
        let doc = "---\ntags:\n  - one\n  - two\nname: x\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(
            field_list(&parsed.fields, "tags"),
            Some(vec!["one".to_string(), "two".to_string()])
        );
        assert_eq!(field_string(&parsed.fields, "name").as_deref(), Some("x"));
    }

    #[test]
    fn retains_unknown_extension_fields() {
        let doc = "---\nname: x\nfutureField: whatever\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert!(parsed.fields.contains_key("futureField"));
        assert_eq!(
            field_string(&parsed.fields, "futureField").as_deref(),
            Some("whatever")
        );
    }

    #[test]
    fn parses_booleans_and_numbers() {
        let doc = "---\nenabled: false\nintervalMinutes: 60\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(field_bool(&parsed.fields, "enabled"), Some(false));
        assert_eq!(field_u64(&parsed.fields, "intervalMinutes"), Some(60));
    }

    #[test]
    fn unterminated_fence_is_parse_error() {
        let doc = "---\nname: x\n\nbody without closing fence\n";
        let err = parse_frontmatter_markdown(doc).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
        assert!(err.message.contains("not terminated"));
    }

    #[test]
    fn malformed_line_is_parse_error_with_line_number() {
        let doc = "---\nname: x\nthis line has no colon\n---\nbody\n";
        let err = parse_frontmatter_markdown(doc).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
        assert!(err.message.contains("line 3"));
    }

    #[test]
    fn empty_body_is_allowed() {
        let doc = "---\nname: x\n---\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let doc = "---\r\nname: x\r\n---\r\nbody\r\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(field_string(&parsed.fields, "name").as_deref(), Some("x"));
        assert!(parsed.body.contains("body"));
    }

    #[test]
    fn dots_terminator_is_accepted() {
        let doc = "---\nname: x\n...\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(field_string(&parsed.fields, "name").as_deref(), Some("x"));
        assert!(parsed.body.contains("body"));
    }

    #[test]
    fn field_present_distinguishes_null() {
        let doc = "---\nempty:\nset: yes\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert!(!field_present(&parsed.fields, "empty"));
        assert!(field_present(&parsed.fields, "set"));
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let doc = "---\n# a comment\n\nname: x\n---\nbody\n";
        let parsed = parse_frontmatter_markdown(doc).unwrap();
        assert_eq!(field_string(&parsed.fields, "name").as_deref(), Some("x"));
    }
}
