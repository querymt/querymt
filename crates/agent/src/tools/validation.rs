//! Tool argument validation reports.
//!
//! [`report`] validates effective arguments against a tool's JSON schema and, on
//! failure, renders **every** violation as one line: the argument path, what was
//! expected, and (when useful) a bounded echo of the offending value.
//!
//! * Echoed values never exceed [`MAX_ECHO_CHARS`] characters; a cut value ends with
//!   the `…` marker, which counts towards the limit.
//! * A string where an array/object is declared (JSON that could not be repaired,
//!   typically because it is malformed) gets a hint that the value must be a real
//!   JSON container.
//! * An unknown key resembling a declared name (case style or a known short form
//!   such as `old_str`) gets a "did you mean" suggestion.
//!
//! Malformed *outer* arguments are not handled here: they are rejected as
//! `INVALID_JSON` before validation is reached.

use jsonschema::Validator;
use jsonschema::error::{TypeKind, ValidationErrorKind};
use serde_json::Value;

/// Maximum number of characters of an argument value echoed in a report,
/// including the shortening marker.
pub const MAX_ECHO_CHARS: usize = 80;

const ELLIPSIS: char = '…';

/// Shorten `text` to at most [`MAX_ECHO_CHARS`] characters, ending with `…` when cut.
fn shorten(text: &str) -> String {
    if text.chars().count() <= MAX_ECHO_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_ECHO_CHARS - 1).collect();
    out.push(ELLIPSIS);
    out
}

fn echo(value: &Value) -> String {
    shorten(&serde_json::to_string(value).unwrap_or_else(|_| "<unprintable>".to_string()))
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validate `args` against `validator` (compiled from `schema`). `None` when valid,
/// otherwise a multi-line description of all violations.
pub fn report(validator: &Validator, schema: &Value, args: &Value) -> Option<String> {
    let lines: Vec<String> = validator
        .iter_errors(args)
        .flat_map(|error| describe(schema, &error))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let noun = if lines.len() == 1 {
        "violation"
    } else {
        "violations"
    };
    let mut out = format!("{} {noun}:", lines.len());
    for line in lines {
        out.push_str("\n- ");
        out.push_str(&line);
    }
    Some(out)
}

/// One line per violation described by `error` (an `additionalProperties` error
/// may name several keys).
fn describe(schema: &Value, error: &jsonschema::ValidationError<'_>) -> Vec<String> {
    let path = error.instance_path().as_str();
    let shown_path = if path.is_empty() { "/" } else { path };
    let instance = error.instance();
    let at = |message: String| format!("{shown_path}: {message}");

    match error.kind() {
        ValidationErrorKind::Required { property } => {
            let name = property
                .as_str()
                .map(shorten)
                .unwrap_or_else(|| echo(property));
            vec![at(format!("missing required field `{name}`"))]
        }
        ValidationErrorKind::AdditionalProperties { unexpected }
        | ValidationErrorKind::UnevaluatedProperties { unexpected } => {
            let declared = declared_names(schema, path);
            unexpected
                .iter()
                .map(|key| {
                    let mut message = format!("property `{}` is not allowed", shorten(key));
                    if let Some(suggestion) = suggest(key, &declared) {
                        message.push_str(&format!("; did you mean `{suggestion}`?"));
                    }
                    at(message)
                })
                .collect()
        }
        ValidationErrorKind::Type { kind } => {
            let expected = match kind {
                TypeKind::Single(t) => t.to_string(),
                TypeKind::Multiple(set) => set
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(" or "),
            };
            let mut message = format!(
                "expected {expected}, got {}: {}",
                json_type_name(instance),
                echo(instance)
            );
            if let Some(hint) = container_hint(schema, path, &expected, instance) {
                message.push_str("; hint: ");
                message.push_str(&hint);
            }
            vec![at(message)]
        }
        ValidationErrorKind::Enum { options } => vec![at(format!(
            "{} is not one of {}",
            echo(instance),
            echo(options)
        ))],
        ValidationErrorKind::Constant { expected_value } => vec![at(format!(
            "expected {}, got {}",
            echo(expected_value),
            echo(instance)
        ))],
        ValidationErrorKind::Minimum { limit } => {
            vec![at(format!(
                "must be >= {}, got {}",
                echo(limit),
                echo(instance)
            ))]
        }
        ValidationErrorKind::Maximum { limit } => {
            vec![at(format!(
                "must be <= {}, got {}",
                echo(limit),
                echo(instance)
            ))]
        }
        ValidationErrorKind::ExclusiveMinimum { limit } => {
            vec![at(format!(
                "must be > {}, got {}",
                echo(limit),
                echo(instance)
            ))]
        }
        ValidationErrorKind::ExclusiveMaximum { limit } => {
            vec![at(format!(
                "must be < {}, got {}",
                echo(limit),
                echo(instance)
            ))]
        }
        ValidationErrorKind::MinLength { limit } => {
            vec![at(format!("must be at least {limit} characters long"))]
        }
        ValidationErrorKind::MaxLength { limit } => {
            vec![at(format!("must be at most {limit} characters long"))]
        }
        ValidationErrorKind::MinItems { limit } => {
            vec![at(format!("must contain at least {limit} items"))]
        }
        ValidationErrorKind::MaxItems { limit } => {
            vec![at(format!("must contain at most {limit} items"))]
        }
        ValidationErrorKind::MinProperties { limit } => {
            vec![at(format!("must have at least {limit} properties"))]
        }
        ValidationErrorKind::MaxProperties { limit } => {
            vec![at(format!("must have at most {limit} properties"))]
        }
        ValidationErrorKind::AnyOf { .. }
        | ValidationErrorKind::OneOfNotValid { .. }
        | ValidationErrorKind::OneOfMultipleValid { .. } => vec![at(format!(
            "does not match the allowed forms ({}), got {}",
            error.kind().keyword(),
            echo(instance)
        ))],
        other => vec![at(format!(
            "violates `{}` constraint, got {}",
            other.keyword(),
            echo(instance)
        ))],
    }
}

/// Schema node at a JSON-pointer instance path, following `properties` and `items`.
/// `None` when the path leaves the plain subset (references, unions, ...).
fn schema_at<'a>(schema: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = schema;
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        let key = segment.replace("~1", "/").replace("~0", "~");
        current = match current.get("properties").and_then(|p| p.get(&key)) {
            Some(next) => next,
            None => current.get("items").filter(|i| i.is_object())?,
        };
    }
    Some(current)
}

fn declared_names(schema: &Value, path: &str) -> Vec<String> {
    schema_at(schema, path)
        .and_then(|node| node.get("properties"))
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default()
}

/// Hint for a string where a JSON array/object is declared.
fn container_hint(schema: &Value, path: &str, expected: &str, instance: &Value) -> Option<String> {
    if !instance.is_string() {
        return None;
    }
    let node = schema_at(schema, path);
    let what = match expected {
        "array" => {
            let items_are_objects = node
                .and_then(|n| n.get("items"))
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str)
                == Some("object");
            if items_are_objects {
                "a JSON array of objects"
            } else {
                "a JSON array"
            }
        }
        "object" => "a JSON object",
        _ => return None,
    };
    let name = path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| format!("`{}`", shorten(s)))
        .unwrap_or_else(|| "this value".to_string());
    Some(format!(
        "{name} must be {what}, not a string containing JSON"
    ))
}

fn to_snake_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    let mut prev_lower = false;
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else {
            out.push(if c == '-' || c == ' ' { '_' } else { c });
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// A declared name that `key` is a case-style variant or known short form of.
fn suggest<'a>(key: &str, declared: &'a [String]) -> Option<&'a str> {
    let snake = to_snake_case(key);
    let long_form = snake
        .strip_suffix("_str")
        .map(|stem| format!("{stem}_string"));
    declared
        .iter()
        .find(|name| {
            name.as_str() != key && (**name == snake || long_form.as_deref() == Some(name.as_str()))
        })
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(schema: &Value, args: Value) -> Option<String> {
        let validator = jsonschema::validator_for(schema).unwrap();
        report(&validator, schema, &args)
    }

    fn multiedit_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["file_path", "edits"],
            "additionalProperties": false
        })
    }

    #[test]
    fn valid_arguments_produce_no_report() {
        let args = json!({"file_path": "a", "edits": [{"old_string": "x", "new_string": "y"}]});
        assert_eq!(run(&multiedit_schema(), args), None);
    }

    #[test]
    fn lists_every_missing_required_field() {
        let report = run(&multiedit_schema(), json!({})).unwrap();
        assert_eq!(
            report,
            "2 violations:\n\
             - /: missing required field `file_path`\n\
             - /: missing required field `edits`"
        );
    }

    #[test]
    fn collects_violations_across_levels_with_paths() {
        let report = run(
            &multiedit_schema(),
            json!({"file_path": 3, "edits": [{"old_string": "x"}]}),
        )
        .unwrap();
        assert!(report.starts_with("2 violations:"), "{report}");
        assert!(
            report.contains("- /file_path: expected string, got integer: 3"),
            "{report}"
        );
        assert!(
            report.contains("- /edits/0: missing required field `new_string`"),
            "{report}"
        );
    }

    #[test]
    fn echoed_value_is_bounded_to_80_characters_including_marker() {
        let long = "x".repeat(2000);
        let report = run(
            &multiedit_schema(),
            json!({"file_path": "a", "edits": long}),
        )
        .unwrap();
        let echoed = report
            .split("got string: ")
            .nth(1)
            .expect("echo present")
            .split("; hint:")
            .next()
            .unwrap();
        // serialised value: opening quote + content + marker
        assert_eq!(echoed.chars().count(), MAX_ECHO_CHARS, "{echoed}");
        assert!(echoed.ends_with(ELLIPSIS), "{echoed}");
        assert!(
            report.len() < 400,
            "report must stay small: {}",
            report.len()
        );
    }

    #[test]
    fn short_values_are_echoed_without_marker() {
        assert_eq!(shorten("abc"), "abc");
        let exact = "y".repeat(MAX_ECHO_CHARS);
        assert_eq!(shorten(&exact), exact);
        assert_eq!(
            shorten(&format!("{exact}z")).chars().count(),
            MAX_ECHO_CHARS
        );
    }

    #[test]
    fn shortening_is_unicode_safe() {
        let text = "é".repeat(500);
        let short = shorten(&text);
        assert_eq!(short.chars().count(), MAX_ECHO_CHARS);
        assert!(short.ends_with(ELLIPSIS));
    }

    #[test]
    fn unrepaired_stringified_array_gets_a_container_hint() {
        // Malformed nested JSON: the normaliser leaves it unchanged.
        let report = run(
            &multiedit_schema(),
            json!({"file_path": "a", "edits": "[{\"old_string\": \"x\", "}),
        )
        .unwrap();
        assert!(
            report.contains(
                "- /edits: expected array, got string: \"[{\\\"old_string\\\": \\\"x\\\", \"; \
                 hint: `edits` must be a JSON array of objects, not a string containing JSON"
            ),
            "{report}"
        );
    }

    #[test]
    fn stringified_object_gets_an_object_hint() {
        let schema = json!({
            "type": "object",
            "properties": {"options": {"type": "object"}}
        });
        let report = run(&schema, json!({"options": "{oops"})).unwrap();
        assert!(
            report.contains("`options` must be a JSON object, not a string containing JSON"),
            "{report}"
        );
    }

    #[test]
    fn no_container_hint_for_non_string_mismatch() {
        let report = run(&multiedit_schema(), json!({"file_path": "a", "edits": 5})).unwrap();
        assert!(
            report.contains("expected array, got integer: 5"),
            "{report}"
        );
        assert!(!report.contains("hint"), "{report}");
    }

    #[test]
    fn unknown_key_resembling_a_declared_name_gets_a_suggestion() {
        let report = run(
            &multiedit_schema(),
            json!({"file_path": "a", "edits": [{"old_str": "x", "newString": "y", "old_string": "x", "new_string": "y"}]}),
        )
        .unwrap();
        assert!(
            report.contains(
                "- /edits/0: property `old_str` is not allowed; did you mean `old_string`?"
            ),
            "{report}"
        );
        assert!(
            report.contains(
                "- /edits/0: property `newString` is not allowed; did you mean `new_string`?"
            ),
            "{report}"
        );
    }

    #[test]
    fn unrelated_unknown_key_has_no_suggestion() {
        let report = run(
            &multiedit_schema(),
            json!({"file_path": "a", "edits": [], "bogus": 1}),
        )
        .unwrap();
        assert_eq!(report, "1 violation:\n- /: property `bogus` is not allowed");
    }

    #[test]
    fn top_level_case_variant_is_suggested() {
        let report = run(
            &multiedit_schema(),
            json!({"file_path": "a", "edits": [], "filePath2": 1, "Edits": []}),
        )
        .unwrap();
        assert!(
            report.contains("property `Edits` is not allowed; did you mean `edits`?"),
            "{report}"
        );
        assert!(
            !report.contains("filePath2` is not allowed; did you mean"),
            "{report}"
        );
    }

    #[test]
    fn constraint_violations_are_described() {
        let schema = json!({
            "type": "object",
            "properties": {
                "n": {"type": "integer", "minimum": 1},
                "s": {"type": "string", "enum": ["a", "b"]}
            }
        });
        let report = run(&schema, json!({"n": 0, "s": "c"})).unwrap();
        assert!(report.contains("- /n: must be >= 1, got 0"), "{report}");
        assert!(
            report.contains("- /s: \"c\" is not one of [\"a\",\"b\"]"),
            "{report}"
        );
    }

    #[test]
    fn to_snake_case_handles_common_styles() {
        assert_eq!(to_snake_case("newString"), "new_string");
        assert_eq!(to_snake_case("replaceAll"), "replace_all");
        assert_eq!(to_snake_case("old-string"), "old_string");
        assert_eq!(to_snake_case("Edits"), "edits");
        assert_eq!(to_snake_case("already_snake"), "already_snake");
    }

    /// Real failure messages for the report fixtures in `scripts/testdata`.
    ///
    /// Each row is `tool<TAB>arguments<TAB>message<TAB>report cause`, where
    /// `message` is the stored result text with newlines written as `\n`. The
    /// test regenerates the message through the real normaliser and validator,
    /// so `scripts/tool-failure-report.sql` is tested against current wording.
    #[test]
    fn report_fixture_messages_match_the_real_pipeline() {
        use crate::tools::args::normalize_arguments;
        use crate::tools::builtins::all_builtin_tools;

        let rows = include_str!("../../../../scripts/testdata/new_format_failures.tsv");
        let tools = all_builtin_tools();
        let mut bad = Vec::new();
        let mut regenerated = Vec::new();
        for line in rows.lines().filter(|l| !l.trim().is_empty()) {
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 4, "malformed fixture row: {line}");
            let tool = tools
                .iter()
                .find(|t| t.name() == cols[0])
                .unwrap_or_else(|| panic!("unknown tool {}", cols[0]));
            let schema = tool.definition().function.parameters;
            let mut args: Value = serde_json::from_str(cols[1]).unwrap();
            let actual = match normalize_arguments(Some(tool.as_ref()), Some(&schema), &mut args) {
                Err(conflict) => format!("Error: invalid tool arguments: {conflict}"),
                Ok(_) => {
                    let validator = jsonschema::validator_for(&schema).unwrap();
                    let report = report(&validator, &schema, &args)
                        .unwrap_or_else(|| panic!("row unexpectedly valid: {line}"));
                    format!("Error: invalid tool arguments: {report}")
                }
            };
            let actual = actual.replace('\n', "\\n");
            regenerated.push(format!("{}\t{}\t{}\t{}", cols[0], cols[1], actual, cols[3]));
            if actual != cols[2] {
                bad.push(regenerated.last().unwrap().clone());
            }
        }
        // `REGEN_REPORT_FIXTURES=<path>` rewrites the fixture from the real pipeline.
        if let Ok(path) = std::env::var("REGEN_REPORT_FIXTURES") {
            std::fs::write(path, regenerated.join("\n") + "\n").unwrap();
            return;
        }
        assert!(
            bad.is_empty(),
            "fixture messages are stale; replace the rows with:\n{}",
            bad.join("\n")
        );
    }
}
