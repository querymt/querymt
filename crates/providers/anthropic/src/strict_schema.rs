//! Conversion of tool schemas into the subset Anthropic accepts for strict
//! tool use.
//!
//! Conversion always works on a copy and is all-or-nothing: any construct that
//! cannot be expressed in strict mode produces [`UnsupportedSchema`] and the
//! caller sends the original schema untouched. Nothing is silently dropped:
//! constraints that strict mode cannot enforce are moved into the description,
//! and every other unknown keyword is rejected. The agent keeps validating
//! calls against the original schema.

use serde_json::{Map, Value};
use std::fmt;

/// Maximum number of strict tools in one request.
pub(crate) const MAX_STRICT_TOOLS: usize = 20;
/// Maximum number of optional properties across the strict tools of a request.
pub(crate) const MAX_OPTIONAL_PROPERTIES: usize = 24;
/// Maximum number of union-typed (`anyOf` / type array) properties across the
/// strict tools of a request.
pub(crate) const MAX_UNION_PROPERTIES: usize = 16;

/// Formats strict mode understands.
const SUPPORTED_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

/// Constraints strict mode cannot enforce; they are appended to the
/// description instead.
const RELAXED_KEYWORDS: &[&str] = &[
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "maxItems",
];

/// Keywords that carry no validation semantics and are copied unchanged.
const PASSTHROUGH_KEYWORDS: &[&str] = &[
    "title", "default", "examples", "$comment", "enum", "const", "required",
];

/// Maximum nesting followed while counting; converted schemas are acyclic so
/// this only guards against malformed input.
const MAX_DEPTH: usize = 64;

/// A schema construct strict mode cannot express. `path` locates the schema
/// node (never argument contents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnsupportedSchema {
    pub path: String,
    pub feature: String,
}

impl fmt::Display for UnsupportedSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.feature, self.path)
    }
}

impl std::error::Error for UnsupportedSchema {}

/// A request-wide strict limit that was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrictLimit {
    Tools(usize),
    OptionalProperties(usize),
    UnionProperties(usize),
}

impl fmt::Display for StrictLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StrictLimit::Tools(n) => {
                write!(
                    f,
                    "strict tool limit ({MAX_STRICT_TOOLS}) exceeded: {n} tools"
                )
            }
            StrictLimit::OptionalProperties(n) => write!(
                f,
                "optional-property limit ({MAX_OPTIONAL_PROPERTIES}) exceeded: {n} properties"
            ),
            StrictLimit::UnionProperties(n) => write!(
                f,
                "union-property limit ({MAX_UNION_PROPERTIES}) exceeded: {n} properties"
            ),
        }
    }
}

fn unsupported<T>(path: &str, feature: impl Into<String>) -> Result<T, UnsupportedSchema> {
    Err(UnsupportedSchema {
        path: path.to_string(),
        feature: feature.into(),
    })
}

/// Local reference targets (`#/$defs/Name`, `#/definitions/Name`) resolved
/// against the original root.
fn resolve_local_ref<'a>(
    root: &'a Value,
    reference: &str,
    path: &str,
) -> Result<&'a Value, UnsupportedSchema> {
    let Some(pointer) = reference.strip_prefix('#') else {
        return unsupported(path, format!("external reference `{reference}`"));
    };
    let target = ["/$defs/", "/definitions/"].iter().find_map(|prefix| {
        let name = pointer.strip_prefix(prefix)?;
        if name.is_empty() || name.contains('/') {
            return None;
        }
        root.get(&prefix[1..prefix.len() - 1])?.get(name)
    });
    match target {
        Some(target) => Ok(target),
        None => unsupported(path, format!("unresolvable reference `{reference}`")),
    }
}

struct Converter<'a> {
    root: &'a Value,
}

impl Converter<'_> {
    fn node(
        &self,
        node: &Value,
        path: &str,
        stack: &mut Vec<String>,
        is_root: bool,
    ) -> Result<Value, UnsupportedSchema> {
        let Value::Object(obj) = node else {
            return unsupported(path, "non-object schema");
        };
        let mut out = Map::new();
        let mut notes: Vec<String> = Vec::new();
        let mut has_properties = false;

        for (key, value) in obj {
            let kpath = format!("{path}/{key}");
            match key.as_str() {
                "type" => {
                    let ok = value.is_string()
                        || value
                            .as_array()
                            .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_string));
                    if !ok {
                        return unsupported(&kpath, "malformed `type`");
                    }
                    out.insert(key.clone(), value.clone());
                }
                "description" => {
                    if !value.is_string() {
                        return unsupported(&kpath, "malformed `description`");
                    }
                    out.insert(key.clone(), value.clone());
                }
                k if PASSTHROUGH_KEYWORDS.contains(&k) => {
                    out.insert(key.clone(), value.clone());
                }
                "properties" => {
                    let Value::Object(props) = value else {
                        return unsupported(&kpath, "malformed `properties`");
                    };
                    has_properties = true;
                    let mut converted = Map::new();
                    for (name, sub) in props {
                        let sub_path = format!("{kpath}/{name}");
                        converted.insert(name.clone(), self.node(sub, &sub_path, stack, false)?);
                    }
                    out.insert(key.clone(), Value::Object(converted));
                }
                "additionalProperties" => {
                    // Checked below once we know whether fixed properties exist.
                    if value != &Value::Bool(false) {
                        return unsupported(
                            &kpath,
                            "extensible object (`additionalProperties` other than false)",
                        );
                    }
                }
                "items" => {
                    if !value.is_object() {
                        return unsupported(&kpath, "tuple or boolean `items`");
                    }
                    out.insert(key.clone(), self.node(value, &kpath, stack, false)?);
                }
                "anyOf" => {
                    let Some(branches) = value.as_array().filter(|a| !a.is_empty()) else {
                        return unsupported(&kpath, "malformed `anyOf`");
                    };
                    let mut converted = Vec::with_capacity(branches.len());
                    for (i, branch) in branches.iter().enumerate() {
                        let branch_path = format!("{kpath}/{i}");
                        converted.push(self.node(branch, &branch_path, stack, false)?);
                    }
                    out.insert(key.clone(), Value::Array(converted));
                }
                "format" => match value.as_str() {
                    Some(f) if SUPPORTED_FORMATS.contains(&f) => {
                        out.insert(key.clone(), value.clone());
                    }
                    Some(f) => return unsupported(&kpath, format!("format `{f}`")),
                    None => return unsupported(&kpath, "malformed `format`"),
                },
                "pattern" => {
                    return unsupported(&kpath, "`pattern` (regular-expression subset unchecked)");
                }
                "$ref" => {
                    let Some(reference) = value.as_str() else {
                        return unsupported(&kpath, "malformed `$ref`");
                    };
                    if stack.iter().any(|r| r == reference) {
                        return unsupported(&kpath, format!("recursive reference `{reference}`"));
                    }
                    let target = resolve_local_ref(self.root, reference, &kpath)?;
                    stack.push(reference.to_string());
                    let checked = self.node(target, &kpath, stack, false);
                    stack.pop();
                    checked?;
                    out.insert(key.clone(), value.clone());
                }
                "$defs" | "definitions" if is_root => {
                    let Value::Object(defs) = value else {
                        return unsupported(&kpath, "malformed definitions");
                    };
                    let mut converted = Map::new();
                    for (name, def) in defs {
                        let def_path = format!("{kpath}/{name}");
                        converted.insert(
                            name.clone(),
                            self.node(def, &def_path, &mut Vec::new(), false)?,
                        );
                    }
                    out.insert(key.clone(), Value::Object(converted));
                }
                "$schema" if is_root => {
                    out.insert(key.clone(), value.clone());
                }
                "minItems" => {
                    let Some(n) = value.as_u64() else {
                        return unsupported(&kpath, "malformed `minItems`");
                    };
                    if n > 1 {
                        notes.push(format!("minItems: {n}"));
                    } else {
                        out.insert(key.clone(), value.clone());
                    }
                }
                k if RELAXED_KEYWORDS.contains(&k) => {
                    if !value.is_number() {
                        return unsupported(&kpath, format!("malformed `{k}`"));
                    }
                    notes.push(format!("{k}: {value}"));
                }
                other => return unsupported(&kpath, format!("keyword `{other}`")),
            }
        }

        if has_properties {
            out.insert("additionalProperties".into(), Value::Bool(false));
        } else if obj.contains_key("additionalProperties") {
            return unsupported(path, "dictionary object (arbitrary keys)");
        } else if declares_object(obj) {
            return unsupported(path, "object without fixed properties (arbitrary keys)");
        }

        if !notes.is_empty() {
            let suffix = format!("({})", notes.join(", "));
            let description = match out.get("description").and_then(Value::as_str) {
                Some(existing) if !existing.is_empty() => format!("{existing} {suffix}"),
                _ => suffix,
            };
            out.insert("description".into(), Value::String(description));
        }
        Ok(Value::Object(out))
    }
}

fn declares_object(obj: &Map<String, Value>) -> bool {
    match obj.get("type") {
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(types)) => types.iter().any(|t| t == "object"),
        _ => false,
    }
}

/// Converts a tool input schema into the strict-mode subset.
///
/// Works on a copy; `schema` is never modified. Fixed-property objects are
/// closed, numeric, string-length, `maxItems` and `minItems > 1` constraints
/// are moved into descriptions (`(minimum: 0)`), and required/optional
/// semantics are preserved. Returns [`UnsupportedSchema`] for anything that
/// cannot be converted without losing meaning.
pub(crate) fn convert(schema: &Value) -> Result<Value, UnsupportedSchema> {
    let converter = Converter { root: schema };
    let converted = converter.node(schema, "#", &mut Vec::new(), true)?;
    if converted.get("properties").is_none() {
        return unsupported("#", "root schema without fixed properties");
    }
    Ok(converted)
}

#[derive(Default)]
struct Stats {
    optional: usize,
    union: usize,
}

fn resolve_converted<'a>(root: &'a Value, mut node: &'a Value) -> &'a Value {
    for _ in 0..MAX_DEPTH {
        let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
            return node;
        };
        match resolve_local_ref(root, reference, "#") {
            Ok(target) => node = target,
            Err(_) => return node,
        }
    }
    node
}

fn is_union(root: &Value, node: &Value) -> bool {
    let node = resolve_converted(root, node);
    node.get("anyOf").is_some() || node.get("type").is_some_and(Value::is_array)
}

fn count(root: &Value, node: &Value, depth: usize, stats: &mut Stats) {
    if depth > MAX_DEPTH {
        return;
    }
    let node = resolve_converted(root, node);
    if let Some(props) = node.get("properties").and_then(Value::as_object) {
        let required: Vec<&str> = node
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for (name, sub) in props {
            if !required.contains(&name.as_str()) {
                stats.optional += 1;
            }
            if is_union(root, sub) {
                stats.union += 1;
            }
            count(root, sub, depth + 1, stats);
        }
    }
    if let Some(items) = node.get("items") {
        count(root, items, depth + 1, stats);
    }
    if let Some(branches) = node.get("anyOf").and_then(Value::as_array) {
        for branch in branches {
            count(root, branch, depth + 1, stats);
        }
    }
}

/// Checks the request-wide strict limits over the converted schemas of the
/// tools that will be sent with `strict: true`.
pub(crate) fn budget(converted: &[Value]) -> Result<(), StrictLimit> {
    if converted.len() > MAX_STRICT_TOOLS {
        return Err(StrictLimit::Tools(converted.len()));
    }
    let mut stats = Stats::default();
    for schema in converted {
        count(schema, schema, 0, &mut stats);
    }
    if stats.optional > MAX_OPTIONAL_PROPERTIES {
        return Err(StrictLimit::OptionalProperties(stats.optional));
    }
    if stats.union > MAX_UNION_PROPERTIES {
        return Err(StrictLimit::UnionProperties(stats.union));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Shaped like the agent's `multiedit` builtin: nested closed-able item
    /// objects, optional properties and numeric/array limits.
    fn multiedit_like() -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string", "description": "File to edit."},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Edits to apply.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old_string", "new_string"]
                    }
                },
                "occurrence": {"type": "integer", "minimum": 0, "description": "0-based occurrence."}
            },
            "required": ["file_path", "edits"]
        })
    }

    #[test]
    fn closes_nested_objects_and_moves_limits_into_descriptions() {
        let original = multiedit_like();
        let converted = convert(&original).unwrap();

        assert_eq!(converted["additionalProperties"], json!(false));
        assert_eq!(
            converted["properties"]["edits"]["items"]["additionalProperties"],
            json!(false)
        );
        let occurrence = &converted["properties"]["occurrence"];
        assert!(occurrence.get("minimum").is_none());
        assert_eq!(
            occurrence["description"],
            "0-based occurrence. (minimum: 0)"
        );
        // minItems of 1 is supported and stays a real constraint.
        assert_eq!(converted["properties"]["edits"]["minItems"], json!(1));
        // Optionality is untouched.
        assert_eq!(converted["required"], original["required"]);
        assert_eq!(
            converted["properties"]["edits"]["items"]["required"],
            original["properties"]["edits"]["items"]["required"]
        );
    }

    #[test]
    fn golden_conversion_of_a_multiedit_like_schema() {
        let converted = convert(&multiedit_like()).unwrap();
        assert_eq!(
            converted,
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "file_path": {"type": "string", "description": "File to edit."},
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Edits to apply.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    },
                    "occurrence": {
                        "type": "integer",
                        "description": "0-based occurrence. (minimum: 0)"
                    }
                },
                "required": ["file_path", "edits"]
            })
        );
    }

    /// Copies of the agent's builtin `edit`, `multiedit` and `replace_symbol`
    /// parameter schemas (descriptions shortened). An agent-side test asserts
    /// the structure stays in sync with the real definitions.
    fn builtin_fixtures() -> Vec<(String, Value)> {
        let all: Value = serde_json::from_str(include_str!("builtin_schemas.json")).unwrap();
        all.as_object()
            .unwrap()
            .iter()
            .map(|(name, schema)| (name.clone(), schema.clone()))
            .collect()
    }

    #[test]
    fn builtin_default_strict_tools_convert_with_only_the_documented_change() {
        for (name, original) in builtin_fixtures() {
            let snapshot = original.clone();
            let converted = convert(&original).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(original, snapshot, "{name}: original mutated");
            if name == "replace_symbol" {
                // Only the minimum moved into the description; all else identical.
                let mut expected = original.clone();
                let occurrence = &mut expected["properties"]["replacements"]["items"]["properties"]
                    ["occurrence"];
                occurrence.as_object_mut().unwrap().remove("minimum");
                occurrence["description"] =
                    json!("0-based occurrence when multiple symbols match. (minimum: 0)");
                assert_eq!(converted, expected);
            } else {
                assert_eq!(converted, original, "{name}");
            }
        }
    }

    #[test]
    fn relaxed_constraints_without_description_create_one() {
        let converted = convert(&json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "maxLength": 12, "minLength": 1},
                "list": {"type": "array", "items": {"type": "string"}, "maxItems": 5, "minItems": 2},
                "n": {"type": "number", "exclusiveMaximum": 10, "multipleOf": 2}
            }
        }))
        .unwrap();
        let name = converted["properties"]["name"]["description"]
            .as_str()
            .unwrap();
        assert!(name.contains("maxLength: 12") && name.contains("minLength: 1"));
        assert!(converted["properties"]["name"].get("maxLength").is_none());
        let list = &converted["properties"]["list"];
        assert!(list.get("maxItems").is_none() && list.get("minItems").is_none());
        let desc = list["description"].as_str().unwrap();
        assert!(desc.contains("maxItems: 5") && desc.contains("minItems: 2"));
        let n = converted["properties"]["n"]["description"]
            .as_str()
            .unwrap();
        assert!(n.contains("exclusiveMaximum: 10") && n.contains("multipleOf: 2"));
    }

    #[test]
    fn original_schema_is_never_modified() {
        let original = multiedit_like();
        let snapshot = original.clone();
        convert(&original).unwrap();
        assert_eq!(original, snapshot);
    }

    #[test]
    fn supported_local_references_are_kept_and_defs_converted() {
        let original = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "target": {"$ref": "#/$defs/Target"},
                "more": {"type": "array", "items": {"$ref": "#/$defs/Target"}}
            },
            "required": ["target"],
            "$defs": {
                "Target": {
                    "type": "object",
                    "properties": {"line": {"type": "integer", "minimum": 1}},
                    "required": ["line"]
                }
            }
        });
        let converted = convert(&original).unwrap();
        assert_eq!(converted["properties"]["target"]["$ref"], "#/$defs/Target");
        let def = &converted["$defs"]["Target"];
        assert_eq!(def["additionalProperties"], json!(false));
        assert!(def["properties"]["line"].get("minimum").is_none());
        assert_eq!(def["properties"]["line"]["description"], "(minimum: 1)");
        assert_eq!(converted["$schema"], original["$schema"]);
        // The legacy `definitions` spelling resolves too.
        convert(&json!({
            "type": "object",
            "properties": {"a": {"$ref": "#/definitions/A"}},
            "definitions": {"A": {"type": "string"}}
        }))
        .unwrap();
    }

    #[test]
    fn anyof_and_type_arrays_are_supported() {
        convert(&json!({
            "type": "object",
            "properties": {
                "a": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]},
                "b": {"type": ["string", "null"], "format": "date-time"}
            }
        }))
        .unwrap();
    }

    fn feature(schema: Value) -> UnsupportedSchema {
        convert(&schema).expect_err("must be unsupported")
    }

    #[test]
    fn rejects_external_and_unresolvable_references() {
        let err = feature(json!({
            "type": "object",
            "properties": {"a": {"$ref": "https://example.com/schema.json"}}
        }));
        assert!(err.feature.contains("external reference"), "{err}");
        assert_eq!(err.path, "#/properties/a/$ref");

        let err = feature(json!({
            "type": "object",
            "properties": {"a": {"$ref": "#/$defs/Missing"}}
        }));
        assert!(err.feature.contains("unresolvable"), "{err}");
    }

    #[test]
    fn rejects_recursive_references() {
        let direct = feature(json!({
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/Node"}},
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {"child": {"$ref": "#/$defs/Node"}}
                }
            }
        }));
        assert!(direct.feature.contains("recursive reference"), "{direct}");

        let mutual = feature(json!({
            "type": "object",
            "properties": {"a": {"$ref": "#/$defs/A"}},
            "$defs": {
                "A": {"type": "object", "properties": {"b": {"$ref": "#/$defs/B"}}},
                "B": {"type": "object", "properties": {"a": {"$ref": "#/$defs/A"}}}
            }
        }));
        assert!(mutual.feature.contains("recursive reference"), "{mutual}");
    }

    #[test]
    fn rejects_dictionaries_and_open_objects() {
        for schema in [
            json!({"type": "object", "properties": {"m": {"type": "object", "additionalProperties": {"type": "string"}}}}),
            json!({"type": "object", "properties": {"m": {"type": "object", "additionalProperties": true}}}),
            json!({"type": "object", "properties": {"m": {"type": "object"}}}),
            json!({"type": "object", "properties": {"k": {"type": "string"}}, "additionalProperties": true}),
        ] {
            let err = feature(schema);
            assert!(
                err.feature.contains("object") || err.feature.contains("extensible"),
                "{err}"
            );
        }
        let err = feature(json!({"type": "object"}));
        assert!(err.feature.contains("arbitrary keys"), "{err}");
    }

    #[test]
    fn rejects_unsupported_constructs_instead_of_dropping_them() {
        for (schema, expected) in [
            (json!({"type": "string", "pattern": "^a+$"}), "pattern"),
            (
                json!({"type": "string", "format": "color"}),
                "format `color`",
            ),
            (json!({"oneOf": [{"type": "string"}]}), "oneOf"),
            (json!({"allOf": [{"type": "string"}]}), "allOf"),
            (json!({"not": {"type": "string"}}), "not"),
            (json!({"type": "array", "uniqueItems": true}), "uniqueItems"),
            (
                json!({"type": "object", "patternProperties": {}}),
                "patternProperties",
            ),
            (
                json!({"type": "array", "items": [{"type": "string"}]}),
                "tuple",
            ),
            (json!(true), "non-object schema"),
        ] {
            let wrapped = json!({"type": "object", "properties": {"p": schema}});
            let err = feature(wrapped);
            assert!(err.feature.contains(expected), "{expected}: {err}");
            assert!(err.path.starts_with("#/properties/p"), "{err}");
        }
    }

    #[test]
    fn rejects_roots_without_fixed_properties() {
        assert!(feature(json!({"type": "string"})).feature.contains("root"));
    }

    #[test]
    fn failure_after_partial_conversion_leaks_nothing() {
        // The first property is relaxed before the second property fails.
        let original = json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer", "minimum": 0},
                "tree": {"$ref": "#/$defs/Tree"}
            },
            "$defs": {"Tree": {"type": "object", "properties": {"kids": {"type": "array", "items": {"$ref": "#/$defs/Tree"}}}}}
        });
        let snapshot = original.clone();
        assert!(convert(&original).is_err());
        assert_eq!(original, snapshot);
        assert_eq!(original["properties"]["count"]["minimum"], json!(0));
    }

    fn tool_with(optional: usize, union: usize) -> Value {
        let mut props = Map::new();
        for i in 0..optional {
            props.insert(format!("opt{i}"), json!({"type": "string"}));
        }
        for i in 0..union {
            props.insert(
                format!("union{i}"),
                json!({"anyOf": [{"type": "string"}, {"type": "integer"}]}),
            );
        }
        // Union properties are listed as required so only `optional` count
        // towards the optional budget.
        let required: Vec<String> = (0..union).map(|i| format!("union{i}")).collect();
        convert(&json!({"type": "object", "properties": props, "required": required})).unwrap()
    }

    #[test]
    fn budget_tool_count_boundary() {
        let tool = tool_with(0, 0);
        assert_eq!(budget(&vec![tool.clone(); MAX_STRICT_TOOLS]), Ok(()));
        assert_eq!(
            budget(&vec![tool; MAX_STRICT_TOOLS + 1]),
            Err(StrictLimit::Tools(MAX_STRICT_TOOLS + 1))
        );
    }

    #[test]
    fn budget_optional_property_boundary_is_summed_across_tools() {
        assert_eq!(budget(&[tool_with(12, 0), tool_with(12, 0)]), Ok(()));
        assert_eq!(
            budget(&[tool_with(12, 0), tool_with(13, 0)]),
            Err(StrictLimit::OptionalProperties(25))
        );
    }

    #[test]
    fn budget_union_property_boundary() {
        assert_eq!(budget(&[tool_with(0, MAX_UNION_PROPERTIES)]), Ok(()));
        assert_eq!(
            budget(&[tool_with(0, MAX_UNION_PROPERTIES + 1)]),
            Err(StrictLimit::UnionProperties(MAX_UNION_PROPERTIES + 1))
        );
    }

    #[test]
    fn budget_counts_nested_optional_properties_and_type_array_unions() {
        let nested = convert(&json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "a": {"type": ["string", "null"]},
                            "b": {"type": "string"}
                        },
                        "required": ["b"]
                    }
                }
            },
            "required": ["items"]
        }))
        .unwrap();
        let mut stats = Stats::default();
        count(&nested, &nested, 0, &mut stats);
        assert_eq!((stats.optional, stats.union), (1, 1));
    }

    #[test]
    fn budget_expands_references_for_each_use() {
        let converted = convert(&json!({
            "type": "object",
            "properties": {
                "x": {"$ref": "#/$defs/D"},
                "y": {"$ref": "#/$defs/D"}
            },
            "required": ["x", "y"],
            "$defs": {"D": {"type": "object", "properties": {"o": {"type": "string"}}}}
        }))
        .unwrap();
        let mut stats = Stats::default();
        count(&converted, &converted, 0, &mut stats);
        assert_eq!(stats.optional, 2);
    }
}
