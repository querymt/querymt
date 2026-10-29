//! Tool argument normalisation.
//!
//! [`normalize_arguments`] turns the arguments a model emitted into *effective*
//! arguments before validation, scheduling and execution. It never touches the raw
//! model output: callers keep that for history and provider replay.
//!
//! Two kinds of change are made:
//!
//! * **Builtin-only, declared by the tool** ([`Tool::arg_aliases`],
//!   [`Tool::wrap_single_item`]): renaming a compatibility spelling to its canonical
//!   name, and wrapping flat item fields into a one-element array. Pass `tool = None`
//!   for MCP and provider tools; they get neither.
//! * **Generic, driven only by the JSON schema** (all tools): a JSON string where an
//!   array/object is declared, an integer string where an integer is declared, and a
//!   string where an array of strings is declared. Union schemas and string-typed
//!   values are never repaired, and broken JSON is never "fixed".
//!
//! Order of operations: top-level aliases, single-item wrapping, then a schema walk
//! that decodes a container before visiting its contents, so aliases inside a
//! decoded array apply to the decoded items. The result is idempotent: normalising a
//! normalised value changes nothing and reports no repairs. On a conflict the input
//! is left untouched.

use crate::tools::context::{AliasScope, ArgAlias, Tool, WrapRule};
use serde_json::{Map, Value};
use std::fmt;

/// The kind of change a [`Repair`] applied. Also the telemetry `rule` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepairRule {
    /// A compatibility spelling was renamed to the canonical name.
    Alias,
    /// Flat item fields were wrapped into a one-element array.
    Wrap,
    /// A JSON string was decoded into the declared array/object.
    JsonString,
    /// An integer string was parsed.
    IntString,
    /// A string was wrapped into a one-element array of strings.
    ScalarToArray,
}

impl RepairRule {
    pub fn as_str(self) -> &'static str {
        match self {
            RepairRule::Alias => "alias",
            RepairRule::Wrap => "wrap",
            RepairRule::JsonString => "json_string",
            RepairRule::IntString => "int_string",
            RepairRule::ScalarToArray => "scalar_to_array",
        }
    }
}

/// One change actually applied to the arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    pub rule: RepairRule,
    /// JSON Pointer to the changed value (the renamed key's object for aliases).
    pub path: String,
}

/// Several spellings of one parameter were supplied at the same level.
///
/// Raised for two aliases or an alias plus the canonical name, even when the values
/// are equal: no spelling is silently preferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgConflict {
    /// JSON Pointer to the object holding the keys (`""` for the top level).
    pub path: String,
    /// The canonical parameter name.
    pub canonical: String,
    /// Every spelling that was present.
    pub keys: Vec<String>,
}

impl fmt::Display for ArgConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys = self
            .keys
            .iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(" and ");
        let at = if self.path.is_empty() {
            "/"
        } else {
            &self.path
        };
        write!(
            f,
            "conflicting spellings of `{}` at {at}: {keys} were both given; send only `{}`",
            self.canonical, self.canonical
        )
    }
}

impl std::error::Error for ArgConflict {}

/// Normalise `args` in place and return the repairs applied.
///
/// * `tool`: `Some` for builtin tools (enables aliases and wrapping), `None` for MCP
///   and provider tools.
/// * `schema`: the tool's parameter schema, if known (enables value repairs).
/// * Non-object `args` are left alone: outer validation rejects them.
///
/// On `Err`, `args` is unchanged.
pub fn normalize_arguments(
    tool: Option<&dyn Tool>,
    schema: Option<&Value>,
    args: &mut Value,
) -> Result<Vec<Repair>, ArgConflict> {
    if !args.is_object() {
        return Ok(Vec::new());
    }

    let aliases: &'static [ArgAlias] = tool.map(|t| t.arg_aliases()).unwrap_or(&[]);
    let mut repairs = Vec::new();
    let mut work = args.clone();

    if let (Some(tool), Value::Object(obj)) = (tool, &mut work) {
        let top: Vec<&ArgAlias> = aliases
            .iter()
            .filter(|a| a.scope == AliasScope::TopLevel)
            .collect();
        apply_aliases(obj, &top, "", &mut repairs)?;
        if let Some(rule) = tool.wrap_single_item() {
            wrap_single_item(obj, &rule, aliases, &mut repairs)?;
        }
    }

    Walker {
        aliases,
        repairs: &mut repairs,
    }
    .visit(&mut work, schema, true, None, "")?;

    *args = work;
    Ok(repairs)
}

fn child_path(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}

/// Rename aliases to their canonical names in `obj`; reject multiple spellings.
fn apply_aliases(
    obj: &mut Map<String, Value>,
    aliases: &[&ArgAlias],
    path: &str,
    repairs: &mut Vec<Repair>,
) -> Result<(), ArgConflict> {
    let mut targets: Vec<&str> = Vec::new();
    for alias in aliases {
        if !targets.contains(&alias.to) {
            targets.push(alias.to);
        }
    }

    // Detect every conflict before renaming anything.
    let mut renames: Vec<(&str, &str)> = Vec::new();
    for to in targets {
        let mut present: Vec<&str> = Vec::new();
        if obj.contains_key(to) {
            present.push(to);
        }
        for alias in aliases.iter().filter(|a| a.to == to) {
            if obj.contains_key(alias.from) && !present.contains(&alias.from) {
                present.push(alias.from);
            }
        }
        match present.as_slice() {
            [] => {}
            [only] => {
                if *only != to {
                    renames.push((only, to));
                }
            }
            _ => {
                return Err(ArgConflict {
                    path: path.to_string(),
                    canonical: to.to_string(),
                    keys: present.iter().map(|k| k.to_string()).collect(),
                });
            }
        }
    }

    for (from, to) in renames {
        if let Some(value) = obj.remove(from) {
            obj.insert(to.to_string(), value);
            repairs.push(Repair {
                rule: RepairRule::Alias,
                path: path.to_string(),
            });
        }
    }
    Ok(())
}

/// Move flat item fields into a one-element array when the array is absent and the
/// required fields are present (after item aliases).
fn wrap_single_item(
    obj: &mut Map<String, Value>,
    rule: &WrapRule,
    aliases: &[ArgAlias],
    repairs: &mut Vec<Repair>,
) -> Result<(), ArgConflict> {
    if obj.contains_key(rule.array) {
        return Ok(());
    }

    let item_aliases: Vec<&ArgAlias> = aliases
        .iter()
        .filter(|a| matches!(a.scope, AliasScope::ArrayItems(n) if n == rule.array))
        .collect();

    let keys: Vec<String> = obj
        .keys()
        .filter(|k| {
            rule.fields.contains(&k.as_str())
                || item_aliases
                    .iter()
                    .any(|a| a.from == k.as_str() && rule.fields.contains(&a.to))
        })
        .cloned()
        .collect();

    let mut item = Map::new();
    for key in &keys {
        if let Some(value) = obj.get(key) {
            item.insert(key.clone(), value.clone());
        }
    }

    let item_path = format!("{}/0", child_path("", rule.array));
    let mut item_repairs = Vec::new();
    apply_aliases(&mut item, &item_aliases, &item_path, &mut item_repairs)?;

    if !rule.required.iter().all(|r| item.contains_key(*r)) {
        return Ok(());
    }

    for key in keys {
        obj.remove(&key);
    }
    repairs.extend(item_repairs);
    repairs.push(Repair {
        rule: RepairRule::Wrap,
        path: child_path("", rule.array),
    });
    obj.insert(
        rule.array.to_string(),
        Value::Array(vec![Value::Object(item)]),
    );
    Ok(())
}

/// A schema we are willing to reason about: not a union, composition or reference.
fn plain(schema: &Value) -> Option<&Value> {
    let obj = schema.as_object()?;
    if ["anyOf", "oneOf", "allOf", "$ref"]
        .iter()
        .any(|k| obj.contains_key(*k))
    {
        return None;
    }
    if obj.get("type").is_some_and(Value::is_array) {
        return None;
    }
    Some(schema)
}

struct Walker<'a> {
    aliases: &'static [ArgAlias],
    repairs: &'a mut Vec<Repair>,
}

impl Walker<'_> {
    /// `top`: `value` is the top-level argument object. `array_key`: `value` is the
    /// array stored under that top-level key, so its items get `ArrayItems` aliases.
    fn visit(
        &mut self,
        value: &mut Value,
        schema: Option<&Value>,
        top: bool,
        array_key: Option<&str>,
        path: &str,
    ) -> Result<(), ArgConflict> {
        let schema = schema.and_then(plain);
        if let Some(schema) = schema {
            self.decode(value, schema, path);
        }

        match value {
            Value::Object(map) => {
                let props = schema
                    .and_then(|s| s.get("properties"))
                    .and_then(Value::as_object);
                let keys: Vec<String> = map.keys().cloned().collect();
                for key in keys {
                    let child_schema = props.and_then(|p| p.get(&key));
                    let child_key = top.then_some(key.as_str());
                    let child_path = child_path(path, &key);
                    if let Some(child) = map.get_mut(&key) {
                        self.visit(child, child_schema, false, child_key, &child_path)?;
                    }
                }
            }
            Value::Array(items) => {
                let items_schema = schema.and_then(|s| s.get("items"));
                let aliases = self.aliases;
                for (index, item) in items.iter_mut().enumerate() {
                    let item_path = format!("{path}/{index}");
                    if let Some(s) = items_schema.and_then(plain) {
                        self.decode(item, s, &item_path);
                    }
                    if let (Some(key), Value::Object(map)) = (array_key, &mut *item) {
                        let scoped: Vec<&ArgAlias> = aliases
                            .iter()
                            .filter(|a| matches!(a.scope, AliasScope::ArrayItems(n) if n == key))
                            .collect();
                        apply_aliases(map, &scoped, &item_path, self.repairs)?;
                    }
                    self.visit(item, items_schema, false, None, &item_path)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Repair `value` if it is a string where the schema declares another type.
    fn decode(&mut self, value: &mut Value, schema: &Value, path: &str) {
        let Value::String(text) = value else {
            return;
        };
        let text = text.clone();
        let Some(ty) = schema.get("type").and_then(Value::as_str) else {
            return;
        };

        match ty {
            "array" | "object" => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                    let matches = if ty == "array" {
                        parsed.is_array()
                    } else {
                        parsed.is_object()
                    };
                    if matches {
                        *value = parsed;
                        self.record(RepairRule::JsonString, path);
                        return;
                    }
                }
                let strings = schema
                    .get("items")
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    == Some("string");
                if ty == "array" && strings {
                    *value = Value::Array(vec![Value::String(text)]);
                    self.record(RepairRule::ScalarToArray, path);
                }
            }
            "integer" => {
                let trimmed = text
                    .trim_end_matches(|c: char| c.is_whitespace() || c == ',')
                    .trim_start();
                if let Ok(n) = trimmed.parse::<i64>() {
                    *value = Value::from(n);
                    self.record(RepairRule::IntString, path);
                }
            }
            _ => {}
        }
    }

    fn record(&mut self, rule: RepairRule, path: &str) {
        self.repairs.push(Repair {
            rule,
            path: path.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::context::{ToolContext, ToolError};
    use async_trait::async_trait;
    use serde_json::json;

    const EDIT_ALIASES: &[ArgAlias] = &[
        top("filePath", "file_path"),
        top("oldString", "old_string"),
        top("old_str", "old_string"),
        top("newString", "new_string"),
        top("new_str", "new_string"),
        top("replaceAll", "replace_all"),
    ];
    const MULTIEDIT_ALIASES: &[ArgAlias] = &[
        top("filePath", "file_path"),
        item("edits", "oldString", "old_string"),
        item("edits", "old_str", "old_string"),
        item("edits", "newString", "new_string"),
        item("edits", "new_str", "new_string"),
        item("edits", "replaceAll", "replace_all"),
    ];
    const REPLACE_SYMBOL_ALIASES: &[ArgAlias] = &[
        item("replacements", "newText", "new_text"),
        item("replacements", "expectedHash", "expected_hash"),
    ];

    const fn top(from: &'static str, to: &'static str) -> ArgAlias {
        ArgAlias {
            scope: AliasScope::TopLevel,
            from,
            to,
        }
    }
    const fn item(array: &'static str, from: &'static str, to: &'static str) -> ArgAlias {
        ArgAlias {
            scope: AliasScope::ArrayItems(array),
            from,
            to,
        }
    }

    /// Test double declaring aliases/wrapper the way the real builtins will.
    struct Fixture {
        aliases: &'static [ArgAlias],
        wrap: Option<WrapRule>,
    }

    #[async_trait]
    impl Tool for Fixture {
        fn name(&self) -> &str {
            "fixture"
        }
        fn definition(&self) -> querymt::chat::Tool {
            querymt::chat::Tool {
                tool_type: "function".to_string(),
                function: querymt::chat::FunctionTool {
                    name: "fixture".to_string(),
                    description: String::new(),
                    parameters: json!({}),
                    strict: None,
                },
            }
        }
        fn arg_aliases(&self) -> &'static [ArgAlias] {
            self.aliases
        }
        fn wrap_single_item(&self) -> Option<WrapRule> {
            self.wrap
        }
        async fn call(
            &self,
            _args: Value,
            _context: &dyn ToolContext,
        ) -> Result<Vec<querymt::chat::ToolResultPart>, ToolError> {
            Err(ToolError::InvalidRequest("not callable".into()))
        }
    }

    fn edit_tool() -> Fixture {
        Fixture {
            aliases: EDIT_ALIASES,
            wrap: None,
        }
    }
    fn multiedit_tool() -> Fixture {
        Fixture {
            aliases: MULTIEDIT_ALIASES,
            wrap: Some(WrapRule {
                array: "edits",
                fields: &["old_string", "new_string", "replace_all"],
                required: &["old_string", "new_string"],
            }),
        }
    }
    fn replace_symbol_tool() -> Fixture {
        Fixture {
            aliases: REPLACE_SYMBOL_ALIASES,
            wrap: Some(WrapRule {
                array: "replacements",
                fields: &["path", "symbol", "new_text", "expected_hash"],
                required: &["path", "symbol", "new_text"],
            }),
        }
    }

    fn edit_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": false
        })
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
    fn replace_symbol_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "replacements": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "symbol": {"type": "string"},
                            "new_text": {"type": "string"},
                            "expected_hash": {"type": "string"}
                        },
                        "required": ["path", "symbol", "new_text"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["replacements"],
            "additionalProperties": false
        })
    }
    fn search_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "offset": {"type": "integer"},
                "exclude": {"type": "array", "items": {"type": "string"}}
            }
        })
    }

    fn norm(
        tool: Option<&dyn Tool>,
        schema: &Value,
        mut args: Value,
    ) -> Result<(Value, Vec<Repair>), ArgConflict> {
        let repairs = normalize_arguments(tool, Some(schema), &mut args)?;
        Ok((args, repairs))
    }

    fn rules(repairs: &[Repair]) -> Vec<&'static str> {
        repairs.iter().map(|r| r.rule.as_str()).collect()
    }

    // ---- 2.2: aliases, conflicts, wrapper ------------------------------------

    #[test]
    fn top_level_camel_case_aliases_are_renamed() {
        let tool = edit_tool();
        let (args, repairs) = norm(
            Some(&tool),
            &edit_schema(),
            json!({"filePath": "a.rs", "oldString": "x", "newString": "y", "replaceAll": true}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"file_path": "a.rs", "old_string": "x", "new_string": "y", "replace_all": true})
        );
        assert_eq!(rules(&repairs), vec!["alias"; 4]);
    }

    #[test]
    fn short_editor_names_are_renamed() {
        let tool = edit_tool();
        let (args, _) = norm(
            Some(&tool),
            &edit_schema(),
            json!({"file_path": "a.rs", "old_str": "x", "new_str": "y"}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"})
        );
    }

    #[test]
    fn canonical_names_are_untouched_and_unrepaired() {
        let tool = edit_tool();
        let input = json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"});
        let (args, repairs) = norm(Some(&tool), &edit_schema(), input.clone()).unwrap();
        assert_eq!(args, input);
        assert!(repairs.is_empty());
    }

    #[test]
    fn alias_and_canonical_conflict_even_with_equal_values() {
        let tool = edit_tool();
        let input =
            json!({"file_path": "a.rs", "old_string": "x", "new_string": "y", "newString": "y"});
        let mut args = input.clone();
        let err = normalize_arguments(Some(&tool), Some(&edit_schema()), &mut args).unwrap_err();
        assert_eq!(err.canonical, "new_string");
        assert!(err.keys.contains(&"new_string".to_string()));
        assert!(err.keys.contains(&"newString".to_string()));
        let message = err.to_string();
        assert!(message.contains("`new_string`") && message.contains("`newString`"));
        assert_eq!(args, input, "input is untouched on conflict");
    }

    #[test]
    fn two_aliases_for_one_name_conflict() {
        let tool = edit_tool();
        let err = norm(
            Some(&tool),
            &edit_schema(),
            json!({"file_path": "a.rs", "oldString": "x", "old_str": "x", "new_string": "y"}),
        )
        .unwrap_err();
        assert_eq!(err.canonical, "old_string");
        assert_eq!(err.keys.len(), 2);
    }

    #[test]
    fn aliases_apply_inside_nested_items() {
        let tool = multiedit_tool();
        let (args, repairs) = norm(
            Some(&tool),
            &multiedit_schema(),
            json!({"filePath": "a.rs", "edits": [{"oldString": "a", "newString": "b", "replaceAll": false}]}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"file_path": "a.rs", "edits": [{"old_string": "a", "new_string": "b", "replace_all": false}]})
        );
        assert_eq!(rules(&repairs), vec!["alias"; 4]);
    }

    #[test]
    fn item_aliases_are_scoped_to_their_array() {
        // `newString` at the top level of multiedit is not a TopLevel alias, and a
        // `newString` in an item of another array is not renamed.
        let tool = multiedit_tool();
        let (args, _) = norm(
            Some(&tool),
            &json!({"type": "object"}),
            json!({"other": [{"newString": "b"}]}),
        )
        .unwrap();
        assert_eq!(args, json!({"other": [{"newString": "b"}]}));
    }

    #[test]
    fn conflict_inside_nested_item_is_rejected_with_its_path() {
        let tool = multiedit_tool();
        let err = norm(
            Some(&tool),
            &multiedit_schema(),
            json!({"file_path": "a.rs", "edits": [{"old_string": "a", "new_string": "b"}, {"old_string": "a", "newString": "b", "new_string": "b"}]}),
        )
        .unwrap_err();
        assert_eq!(err.path, "/edits/1");
        assert_eq!(err.canonical, "new_string");
    }

    #[test]
    fn single_edit_is_wrapped_into_edits() {
        let tool = multiedit_tool();
        let (args, repairs) = norm(
            Some(&tool),
            &multiedit_schema(),
            json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"file_path": "a.rs", "edits": [{"old_string": "x", "new_string": "y"}]})
        );
        assert_eq!(rules(&repairs), vec!["wrap"]);
    }

    #[test]
    fn flat_aliased_edit_is_aliased_then_wrapped() {
        let tool = multiedit_tool();
        let (args, repairs) = norm(
            Some(&tool),
            &multiedit_schema(),
            json!({"filePath": "a.rs", "oldString": "x", "new_str": "y", "replaceAll": true}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"file_path": "a.rs", "edits": [{"old_string": "x", "new_string": "y", "replace_all": true}]})
        );
        assert_eq!(
            rules(&repairs),
            vec!["alias", "alias", "alias", "alias", "wrap"]
        );
    }

    #[test]
    fn wrapper_requires_the_required_fields() {
        let tool = multiedit_tool();
        let input = json!({"file_path": "a.rs", "old_string": "x"});
        let (args, repairs) = norm(Some(&tool), &multiedit_schema(), input.clone()).unwrap();
        assert_eq!(
            args, input,
            "missing new_string: nothing is wrapped or renamed"
        );
        assert!(repairs.is_empty());
    }

    #[test]
    fn no_wrapping_when_the_array_key_exists() {
        let tool = multiedit_tool();
        let input = json!({"file_path": "a.rs", "edits": [], "old_string": "x", "new_string": "y"});
        let (args, repairs) = norm(Some(&tool), &multiedit_schema(), input.clone()).unwrap();
        assert_eq!(args, input);
        assert!(repairs.is_empty());
    }

    #[test]
    fn flat_conflict_is_rejected_even_when_wrapping_would_not_happen() {
        let tool = multiedit_tool();
        let err = norm(
            Some(&tool),
            &multiedit_schema(),
            json!({"file_path": "a.rs", "new_string": "y", "newString": "y"}),
        )
        .unwrap_err();
        assert_eq!(err.canonical, "new_string");
    }

    #[test]
    fn single_replacement_with_alias_is_wrapped() {
        let tool = replace_symbol_tool();
        let (args, repairs) = norm(
            Some(&tool),
            &replace_symbol_schema(),
            json!({"path": "a.rs", "symbol": "f", "newText": "fn f() {}", "expectedHash": "h"}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"replacements": [{"path": "a.rs", "symbol": "f", "new_text": "fn f() {}", "expected_hash": "h"}]})
        );
        assert_eq!(rules(&repairs), vec!["alias", "alias", "wrap"]);
    }

    #[test]
    fn non_object_arguments_are_left_for_outer_validation() {
        let tool = edit_tool();
        let mut args = json!(["a"]);
        let repairs = normalize_arguments(Some(&tool), Some(&edit_schema()), &mut args).unwrap();
        assert!(repairs.is_empty());
        assert_eq!(args, json!(["a"]));
    }

    // ---- 2.3: generic value repairs ------------------------------------------

    #[test]
    fn integer_string_is_trimmed_and_parsed() {
        for (text, expected) in [("466, ", 466), ("1,", 1), (" 7 ", 7), ("-3", -3)] {
            let (args, repairs) = norm(None, &search_schema(), json!({"offset": text})).unwrap();
            assert_eq!(args, json!({"offset": expected}), "{text:?}");
            assert_eq!(rules(&repairs), vec!["int_string"]);
        }
    }

    #[test]
    fn unparseable_integer_string_is_left_alone() {
        for text in ["", "abc", "1.5", "4 6", ",", "0x10"] {
            let input = json!({"offset": text});
            let (args, repairs) = norm(None, &search_schema(), input.clone()).unwrap();
            assert_eq!(args, input, "{text:?}");
            assert!(repairs.is_empty());
        }
    }

    #[test]
    fn scalar_string_becomes_array_of_strings() {
        let (args, repairs) =
            norm(None, &search_schema(), json!({"exclude": "target/**"})).unwrap();
        assert_eq!(args, json!({"exclude": ["target/**"]}));
        assert_eq!(rules(&repairs), vec!["scalar_to_array"]);
    }

    #[test]
    fn existing_array_of_strings_is_not_repaired() {
        let input = json!({"exclude": ["a", "b"]});
        let (args, repairs) = norm(None, &search_schema(), input.clone()).unwrap();
        assert_eq!(args, input);
        assert!(repairs.is_empty());
    }

    #[test]
    fn json_string_decodes_to_declared_array() {
        let (args, repairs) =
            norm(None, &search_schema(), json!({"exclude": "[\"a\",\"b\"]"})).unwrap();
        assert_eq!(args, json!({"exclude": ["a", "b"]}));
        assert_eq!(rules(&repairs), vec!["json_string"]);
    }

    #[test]
    fn scalar_to_array_only_applies_to_arrays_of_strings() {
        let schema = json!({"type": "object", "properties": {
            "numbers": {"type": "array", "items": {"type": "integer"}},
            "objs": {"type": "array", "items": {"type": "object"}}
        }});
        let input = json!({"numbers": "5", "objs": "x"});
        let (args, repairs) = norm(None, &schema, input.clone()).unwrap();
        assert_eq!(args, input);
        assert!(repairs.is_empty());
    }

    #[test]
    fn decoded_value_of_the_wrong_type_is_not_kept() {
        // An object where an array is declared, and an array where an object is.
        let schema = json!({"type": "object", "properties": {
            "list": {"type": "array", "items": {"type": "object"}},
            "obj": {"type": "object"}
        }});
        let input = json!({"list": "{\"a\":1}", "obj": "[1]"});
        let (args, repairs) = norm(None, &schema, input.clone()).unwrap();
        assert_eq!(args, input);
        assert!(repairs.is_empty());
    }

    #[test]
    fn broken_nested_json_is_not_repaired() {
        let tool = multiedit_tool();
        let input = json!({"file_path": "a.rs", "edits": "[{\"old_string\": \"a\", "});
        let (args, repairs) = norm(Some(&tool), &multiedit_schema(), input.clone()).unwrap();
        assert_eq!(args, input);
        assert!(repairs.is_empty());
    }

    #[test]
    fn string_typed_content_is_never_touched() {
        let tool = edit_tool();
        let json_like = "[{\"a\": 1}]";
        let (args, repairs) = norm(
            Some(&tool),
            &edit_schema(),
            json!({"file_path": "a.rs", "old_string": "123", "new_string": json_like}),
        )
        .unwrap();
        assert_eq!(args["new_string"], json!(json_like));
        assert_eq!(args["old_string"], json!("123"));
        assert!(repairs.is_empty());
    }

    #[test]
    fn edit_content_inside_decoded_items_stays_a_string() {
        let tool = multiedit_tool();
        let inner = "[\"x\", 1]";
        let edits =
            serde_json::to_string(&json!([{"old_string": "a", "new_string": inner}])).unwrap();
        let (args, _) = norm(
            Some(&tool),
            &multiedit_schema(),
            json!({"file_path": "a.rs", "edits": edits}),
        )
        .unwrap();
        assert_eq!(args["edits"][0]["new_string"], json!(inner));
    }

    #[test]
    fn unions_are_never_repaired() {
        let schema = json!({"type": "object", "properties": {
            "any": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
            "one": {"oneOf": [{"type": "array", "items": {"type": "string"}}, {"type": "null"}]},
            "multi": {"type": ["integer", "null"]},
            "list": {"type": ["array", "null"], "items": {"type": "string"}}
        }});
        let input = json!({"any": "5", "one": "x", "multi": "5", "list": "x"});
        let (args, repairs) = norm(None, &schema, input.clone()).unwrap();
        assert_eq!(args, input);
        assert!(repairs.is_empty());
    }

    #[test]
    fn mcp_tools_get_value_repairs_but_no_aliases_or_wrapping() {
        // tool = None models an MCP/provider tool whose schema declares `edits`.
        let input = json!({
            "filePath": "a.rs", "oldString": "x", "newString": "y",
            "edits": "[{\"oldString\":\"a\",\"newString\":\"b\"}]"
        });
        let (args, repairs) = norm(None, &multiedit_schema(), input).unwrap();
        assert_eq!(args["filePath"], json!("a.rs"));
        assert_eq!(args["oldString"], json!("x"));
        assert!(args.get("file_path").is_none());
        // The string was decoded, but the keys inside are the server's own.
        assert_eq!(args["edits"], json!([{"oldString": "a", "newString": "b"}]));
        assert_eq!(rules(&repairs), vec!["json_string"]);
    }

    #[test]
    fn mcp_tools_are_not_wrapped() {
        let input = json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"});
        let (args, repairs) = norm(None, &multiedit_schema(), input.clone()).unwrap();
        assert_eq!(args, input);
        assert!(repairs.is_empty());
    }

    #[test]
    fn without_a_schema_only_declared_aliases_apply() {
        let tool = multiedit_tool();
        let mut args = json!({"filePath": "a.rs", "edits": [{"oldString": "a", "newString": "b"}]});
        let repairs = normalize_arguments(Some(&tool), None, &mut args).unwrap();
        assert_eq!(
            args,
            json!({"file_path": "a.rs", "edits": [{"old_string": "a", "new_string": "b"}]})
        );
        assert_eq!(rules(&repairs), vec!["alias"; 3]);
    }

    // ---- 2.4: combined repairs and idempotence --------------------------------

    #[test]
    fn stringified_edits_with_aliases_become_canonical_items() {
        let tool = multiedit_tool();
        let (args, repairs) = norm(
            Some(&tool),
            &multiedit_schema(),
            json!({"filePath": "a.rs", "edits": "[{\"oldString\":\"a\",\"newString\":\"b\"}]"}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"file_path": "a.rs", "edits": [{"old_string": "a", "new_string": "b"}]})
        );
        assert_eq!(
            rules(&repairs),
            vec!["alias", "json_string", "alias", "alias"]
        );
    }

    #[test]
    fn flat_new_text_wraps_into_a_canonical_replacement() {
        let tool = replace_symbol_tool();
        let (args, _) = norm(
            Some(&tool),
            &replace_symbol_schema(),
            json!({"path": "a.rs", "symbol": "f", "newText": "x"}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"replacements": [{"path": "a.rs", "symbol": "f", "new_text": "x"}]})
        );
    }

    #[test]
    fn stringified_replacements_with_aliases_normalise() {
        let tool = replace_symbol_tool();
        let (args, _) = norm(
            Some(&tool),
            &replace_symbol_schema(),
            json!({"replacements": "[{\"path\":\"a.rs\",\"symbol\":\"f\",\"newText\":\"x\"}]"}),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({"replacements": [{"path": "a.rs", "symbol": "f", "new_text": "x"}]})
        );
    }

    #[test]
    fn conflicts_inside_decoded_items_reject() {
        let tool = multiedit_tool();
        let input = json!({
            "file_path": "a.rs",
            "edits": "[{\"old_string\":\"a\",\"newString\":\"b\",\"new_string\":\"b\"}]"
        });
        let mut args = input.clone();
        let err =
            normalize_arguments(Some(&tool), Some(&multiedit_schema()), &mut args).unwrap_err();
        assert_eq!(err.path, "/edits/0");
        assert!(err.keys.contains(&"newString".to_string()));
        assert!(err.keys.contains(&"new_string".to_string()));
        assert_eq!(args, input, "a rejected call is not partially normalised");
    }

    #[test]
    fn normalisation_is_idempotent() {
        let multi = multiedit_tool();
        let repl = replace_symbol_tool();
        let search = search_schema();
        let cases: Vec<(Option<&dyn Tool>, Value, Value)> = vec![
            (
                Some(&multi),
                multiedit_schema(),
                json!({"filePath": "a.rs", "edits": "[{\"oldString\":\"a\",\"newString\":\"b\"}]"}),
            ),
            (
                Some(&multi),
                multiedit_schema(),
                json!({"filePath": "a.rs", "oldString": "x", "newString": "y"}),
            ),
            (
                Some(&repl),
                replace_symbol_schema(),
                json!({"path": "a.rs", "symbol": "f", "newText": "x"}),
            ),
            (
                None,
                search.clone(),
                json!({"offset": "466, ", "exclude": "target/**"}),
            ),
        ];
        for (tool, schema, input) in cases {
            let (first, repairs) = norm(tool, &schema, input).unwrap();
            assert!(!repairs.is_empty());
            let (second, again) = norm(tool, &schema, first.clone()).unwrap();
            assert_eq!(second, first);
            assert!(again.is_empty(), "second pass reported {again:?}");
        }
    }

    #[test]
    fn repair_paths_point_at_the_changed_values() {
        let (_, repairs) = norm(
            None,
            &search_schema(),
            json!({"offset": "5", "exclude": "x"}),
        )
        .unwrap();
        let mut paths: Vec<_> = repairs.iter().map(|r| r.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["/exclude", "/offset"]);
    }
}
