pub mod accept_delegation_result;
pub mod browse;
pub mod create_task;
pub mod delegate;
pub mod delete_file;
pub mod edit;
pub mod edit_output;
pub mod find_references;
pub mod get_function;
pub mod get_symbol;
pub mod glob;
pub mod helpers;
pub mod index;
pub mod knowledge_consolidate;
pub mod knowledge_ingest;
pub mod knowledge_list;
pub mod knowledge_query;
pub mod knowledge_stats;
pub mod language_query;
pub mod ls;
pub mod mdq;
pub mod multiedit;
pub mod question;
pub mod read_shared;
pub mod read_tool;
pub mod replace_symbol;
pub mod search_text;
pub mod shell;
pub mod task;
pub mod todo;
pub mod web_fetch;
pub mod write_file;

pub mod route_delegation_to_peer;
pub mod use_remote_provider;

pub use route_delegation_to_peer::RouteDelegationToPeerTool;
pub use use_remote_provider::UseRemoteProviderTool;

pub use accept_delegation_result::AcceptDelegationResultTool;
pub use browse::BrowseTool;
pub use create_task::CreateTaskTool;
pub use delegate::DelegateTool;
pub use delete_file::DeleteFileTool;
pub use edit::EditTool;
pub use find_references::FindSymbolReferencesTool;
pub use get_function::GetFunctionTool;
pub use get_symbol::GetSymbolTool;
pub use glob::GlobTool;
pub use index::IndexTool;
pub use knowledge_consolidate::KnowledgeConsolidateTool;
pub use knowledge_ingest::KnowledgeIngestTool;
pub use knowledge_list::KnowledgeListTool;
pub use knowledge_query::KnowledgeQueryTool;
pub use knowledge_stats::KnowledgeStatsTool;
pub use language_query::LanguageQueryTool;
pub use ls::ListTool;
pub use mdq::MdqTool;
pub use multiedit::MultiEditTool;
pub use question::QuestionTool;
pub use read_tool::ReadTool;
pub use replace_symbol::ReplaceSymbolTool;
pub use search_text::SearchTextTool;
pub use shell::ShellTool;
pub use task::{CompleteTaskTool, ReadTaskTool, UpdateTaskTool};
pub use todo::{TodoReadTool, TodoWriteTool};
pub use web_fetch::WebFetchTool;
pub use write_file::WriteFileTool;

use crate::tools::Tool;
use std::sync::Arc;

/// Returns all builtin tools.
///
/// This is the canonical source of truth for which tools are built-in.
/// Used for capability inference and tool registration.
pub fn all_builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(AcceptDelegationResultTool::new()),
        Arc::new(BrowseTool::new()),
        Arc::new(CreateTaskTool::new()),
        Arc::new(ReadTaskTool::new()),
        Arc::new(UpdateTaskTool::new()),
        Arc::new(CompleteTaskTool::new()),
        Arc::new(DelegateTool::new()),
        Arc::new(DeleteFileTool::new()),
        Arc::new(EditTool::new()),
        Arc::new(GetFunctionTool::new()),
        Arc::new(GetSymbolTool::new()),
        Arc::new(GlobTool::new()),
        Arc::new(FindSymbolReferencesTool::new()),
        Arc::new(IndexTool::new()),
        Arc::new(LanguageQueryTool::new()),
        Arc::new(KnowledgeConsolidateTool::new()),
        Arc::new(KnowledgeIngestTool::new()),
        Arc::new(KnowledgeListTool::new()),
        Arc::new(KnowledgeQueryTool::new()),
        Arc::new(KnowledgeStatsTool::new()),
        Arc::new(ListTool::new()),
        Arc::new(MdqTool::new()),
        Arc::new(MultiEditTool::new()),
        Arc::new(QuestionTool::new()),
        Arc::new(ReadTool::new()),
        Arc::new(ReplaceSymbolTool::new()),
        Arc::new(SearchTextTool::new()),
        Arc::new(ShellTool::new()),
        Arc::new(TodoReadTool::new()),
        Arc::new(TodoWriteTool::new()),
        Arc::new(WebFetchTool::new()),
        Arc::new(WriteFileTool::new()),
    ]
}

#[cfg(test)]
mod schema_guard_tests {
    use super::all_builtin_tools;
    use serde_json::Value;

    /// Walk every schema node, reporting `(json_pointer, problem)` pairs.
    fn walk(schema: &Value, path: &str, out: &mut Vec<(String, String)>) {
        let Some(obj) = schema.as_object() else {
            return;
        };
        let is_object = obj.get("type").and_then(Value::as_str) == Some("object")
            || obj.contains_key("properties");
        if is_object && obj.get("additionalProperties") != Some(&Value::Bool(false)) {
            out.push((path.to_string(), "object is not closed".to_string()));
        }
        if let Some(props) = obj.get("properties").and_then(Value::as_object) {
            for (name, child) in props {
                let valid = name.starts_with(|c: char| c.is_ascii_lowercase())
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if !valid {
                    out.push((format!("{path}/properties/{name}"), "not snake_case".into()));
                }
                walk(child, &format!("{path}/properties/{name}"), out);
            }
        }
        if let Some(items) = obj.get("items") {
            walk(items, &format!("{path}/items"), out);
        }
        for key in ["anyOf", "oneOf", "allOf"] {
            if let Some(arr) = obj.get(key).and_then(Value::as_array) {
                for (i, child) in arr.iter().enumerate() {
                    walk(child, &format!("{path}/{key}/{i}"), out);
                }
            }
        }
    }

    #[test]
    fn builtin_schemas_are_snake_case_and_closed() {
        let mut problems = Vec::new();
        for tool in all_builtin_tools() {
            let mut found = Vec::new();
            walk(&tool.definition().function.parameters, "", &mut found);
            for (path, problem) in found {
                let at = if path.is_empty() {
                    "<root>"
                } else {
                    path.as_str()
                };
                problems.push(format!("{}: {at}: {problem}", tool.name()));
            }
        }
        assert!(
            problems.is_empty(),
            "builtin schema violations:\n{}",
            problems.join("\n")
        );
    }
}

#[cfg(test)]
mod anthropic_strict_defaults_tests {
    use super::all_builtin_tools;
    use serde_json::Value;

    /// Fixture shared with the Anthropic provider; its keys are the provider's
    /// default `strict_tools` list (the provider asserts that equivalence).
    const FIXTURE: &str = include_str!("../../../../providers/anthropic/src/builtin_schemas.json");

    /// Drop prose (`description` strings) so structure can be compared.
    fn without_prose(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(k, v)| !(k.as_str() == "description" && v.is_string()))
                    .map(|(k, v)| (k.clone(), without_prose(v)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(without_prose).collect()),
            other => other.clone(),
        }
    }

    #[test]
    fn default_strict_tool_names_exist_and_builtins_are_not_marked_strict() {
        let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
        let fixture = fixture.as_object().unwrap();
        assert!(!fixture.is_empty());
        let tools = all_builtin_tools();

        for (name, expected) in fixture {
            let tool = tools
                .iter()
                .find(|t| t.name() == name)
                .unwrap_or_else(|| panic!("default strict tool `{name}` is not a builtin"));
            let definition = tool.definition().function;
            assert_eq!(definition.strict, None, "{name} must not be marked strict");
            assert_eq!(
                without_prose(&definition.parameters),
                without_prose(expected),
                "{name}: provider fixture drifted from the builtin schema"
            );
        }
        // OpenAI Responses rejects strict schemas with optional properties,
        // so no builtin may opt in to strictness.
        for tool in &tools {
            assert_eq!(tool.definition().function.strict, None, "{}", tool.name());
        }
    }
}

#[cfg(test)]
mod canonical_naming_tests {
    use super::{EditTool, MultiEditTool, ReplaceSymbolTool};
    use crate::tools::Tool;
    use crate::tools::args::normalize_arguments;
    use serde_json::{Value, json};

    /// Normalise with the real tool and schema, then validate against that schema.
    fn run(tool: &dyn Tool, mut args: Value) -> Result<Value, String> {
        let schema = tool.definition().function.parameters;
        normalize_arguments(Some(tool), Some(&schema), &mut args).map_err(|e| e.to_string())?;
        let validator = jsonschema::validator_for(&schema).unwrap();
        match validator.validate(&args) {
            Ok(()) => Ok(args),
            Err(e) => Err(e.to_string()),
        }
    }

    fn property_names(tool: &dyn Tool) -> Vec<String> {
        let schema = tool.definition().function.parameters;
        schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn edit_advertises_only_canonical_names() {
        let mut names = property_names(&EditTool::new());
        names.sort();
        assert_eq!(
            names,
            ["file_path", "new_string", "old_string", "replace_all"]
        );
        let schema = EditTool::new().definition().function.parameters.to_string();
        for legacy in [
            "filePath",
            "oldString",
            "newString",
            "replaceAll",
            "old_str\"",
            "new_str\"",
        ] {
            assert!(!schema.contains(legacy), "schema leaks {legacy}: {schema}");
        }
    }

    #[test]
    fn edit_accepts_every_spelling() {
        let canonical =
            json!({"file_path": "a.rs", "old_string": "x", "new_string": "y", "replace_all": true});
        let spellings = [
            json!({"file_path": "a.rs", "old_string": "x", "new_string": "y", "replace_all": true}),
            json!({"filePath": "a.rs", "oldString": "x", "newString": "y", "replaceAll": true}),
            json!({"file_path": "a.rs", "old_str": "x", "new_str": "y", "replaceAll": true}),
            json!({"filePath": "a.rs", "old_str": "x", "newString": "y", "replace_all": true}),
        ];
        for input in spellings {
            assert_eq!(
                run(&EditTool::new(), input.clone()).unwrap(),
                canonical,
                "{input}"
            );
        }
    }

    #[test]
    fn edit_rejects_conflicts_and_unknown_keys_with_the_key_named() {
        let tool = EditTool::new();
        let err = run(
            &tool,
            json!({"file_path": "a", "old_string": "x", "new_string": "y", "newString": "y"}),
        )
        .unwrap_err();
        assert!(
            err.contains("new_string") && err.contains("newString"),
            "{err}"
        );
        let err = run(
            &tool,
            json!({"file_path": "a", "old_string": "x", "old_str": "x", "new_string": "y"}),
        )
        .unwrap_err();
        assert!(
            err.contains("old_string") && err.contains("old_str"),
            "{err}"
        );
        let err = run(&tool, json!({"file_path": "a", "old_string": "x", "new_string": "y", "replace_everything": true})).unwrap_err();
        assert!(err.contains("replace_everything"), "{err}");
    }

    #[test]
    fn multiedit_schema_is_canonical_at_both_levels() {
        let tool = MultiEditTool::new();
        let schema = tool.definition().function.parameters;
        let mut top = property_names(&tool);
        top.sort();
        assert_eq!(top, ["edits", "file_path"]);
        let mut item: Vec<_> = schema["properties"]["edits"]["items"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        item.sort();
        assert_eq!(item, ["new_string", "old_string", "replace_all"]);
    }

    #[test]
    fn multiedit_canonical_and_legacy_forms_normalise_and_validate() {
        let canonical =
            json!({"file_path": "a.rs", "edits": [{"old_string": "a", "new_string": "b"}]});
        let forms = [
            // canonical
            json!({"file_path": "a.rs", "edits": [{"old_string": "a", "new_string": "b"}]}),
            // legacy nested
            json!({"filePath": "a.rs", "edits": [{"oldString": "a", "newString": "b"}]}),
            // stringified and aliased
            json!({"filePath": "a.rs", "edits": "[{\"oldString\":\"a\",\"newString\":\"b\"}]"}),
            // flat, canonical
            json!({"file_path": "a.rs", "old_string": "a", "new_string": "b"}),
            // flat and aliased
            json!({"filePath": "a.rs", "old_str": "a", "newString": "b"}),
        ];
        for input in forms {
            assert_eq!(
                run(&MultiEditTool::new(), input.clone()).unwrap(),
                canonical,
                "{input}"
            );
        }
    }

    #[test]
    fn multiedit_rejects_unknown_item_keys_with_their_path() {
        let err = run(
            &MultiEditTool::new(),
            json!({"file_path": "a", "edits": [{"old_string": "a", "new_string": "b", "oops": 1}]}),
        )
        .unwrap_err();
        assert!(err.contains("oops"), "{err}");
        let err = run(&MultiEditTool::new(), json!({"file_path": "a", "edits": [{"old_string": "a", "newString": "b", "new_string": "b"}]})).unwrap_err();
        assert!(err.contains("/edits/0"), "{err}");
    }

    #[test]
    fn replace_symbol_schema_is_canonical() {
        let tool = ReplaceSymbolTool::new();
        let schema = tool.definition().function.parameters;
        let mut item: Vec<_> = schema["properties"]["replacements"]["items"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        item.sort();
        assert_eq!(
            item,
            [
                "expected_hash",
                "kind",
                "new_text",
                "occurrence",
                "path",
                "symbol"
            ]
        );
    }

    #[test]
    fn replace_symbol_canonical_legacy_decoded_and_flat_forms_normalise() {
        let canonical = json!({"replacements": [{"path": "a.rs", "symbol": "f", "new_text": "x", "expected_hash": "h"}]});
        let forms = [
            json!({"replacements": [{"path": "a.rs", "symbol": "f", "new_text": "x", "expected_hash": "h"}]}),
            json!({"replacements": [{"path": "a.rs", "symbol": "f", "newText": "x", "expectedHash": "h"}]}),
            json!({"replacements": "[{\"path\":\"a.rs\",\"symbol\":\"f\",\"newText\":\"x\",\"expectedHash\":\"h\"}]"}),
            json!({"path": "a.rs", "symbol": "f", "newText": "x", "expectedHash": "h"}),
            json!({"path": "a.rs", "symbol": "f", "new_text": "x", "expected_hash": "h"}),
        ];
        for input in forms {
            assert_eq!(
                run(&ReplaceSymbolTool::new(), input.clone()).unwrap(),
                canonical,
                "{input}"
            );
        }
    }

    #[test]
    fn replace_symbol_flat_form_needs_its_required_fields() {
        // Missing `new_text`: nothing is wrapped, so the stray top-level fields stay
        // in place and the closed schema rejects the call. (The validator currently
        // reports only the first violation; section 5 lists them all.)
        let tool = ReplaceSymbolTool::new();
        let input = json!({"path": "a.rs", "symbol": "f"});
        let mut args = input.clone();
        let schema = tool.definition().function.parameters;
        let repairs = normalize_arguments(Some(&tool), Some(&schema), &mut args).unwrap();
        assert_eq!(args, input, "must not be wrapped");
        assert!(repairs.is_empty());
        assert!(run(&tool, input).is_err());
    }

    #[test]
    fn wrapper_does_not_swallow_root() {
        // `root` is a real top-level property of replace_symbol, not an item field.
        let args = run(
            &ReplaceSymbolTool::new(),
            json!({"root": ".", "path": "a.rs", "symbol": "f", "newText": "x"}),
        )
        .unwrap();
        assert_eq!(args["root"], json!("."));
        assert_eq!(args["replacements"][0]["new_text"], json!("x"));
    }
}
