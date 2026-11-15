//! Built-in tools that need the agent runtime.
//!
//! `tools::builtins` holds the tools that only need the `ToolContext`
//! contract. The tools here read runtime services (the agent registry) or
//! talk to routing actors, so they live with the runtime. `all_builtin_tools`
//! is the canonical list: the base tools with `delegate` in its original
//! position.

pub mod delegate;
pub mod route_delegation_to_peer;
pub mod use_remote_provider;

pub use delegate::DelegateTool;
pub use route_delegation_to_peer::RouteDelegationToPeerTool;
pub use use_remote_provider::UseRemoteProviderTool;

use crate::tools::Tool;
use std::sync::Arc;

/// Returns all builtin tools.
///
/// This is the canonical source of truth for which tools are built-in.
/// Used for capability inference and tool registration. `route_delegation_to_peer`
/// and `use_remote_provider` are registered separately by the quorum builder.
pub fn all_builtin_tools() -> Vec<Arc<dyn Tool>> {
    let mut tools = crate::tools::builtins::base_builtin_tools();
    // `delegate` sits right before `delete_file`, as it always has.
    let at = tools
        .iter()
        .position(|tool| tool.name() == "delete_file")
        .unwrap_or(tools.len());
    tools.insert(at, Arc::new(DelegateTool::new()));
    tools
}

/// The tool names baseline recorded before the crate split (task 0.1).
#[cfg(test)]
mod name_list_tests {
    use super::all_builtin_tools;

    #[test]
    fn builtin_tool_names_unchanged_after_split() {
        let expected: Vec<&str> =
            include_str!("../../../tests/fixtures/builtin_tool_names_baseline.txt")
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
        let actual: Vec<String> = all_builtin_tools()
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(actual, expected);
    }
}

#[cfg(test)]
mod schema_guard_tests {
    use crate::agent::builtin_tools::all_builtin_tools;
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
    use crate::agent::builtin_tools::all_builtin_tools;
    use crate::tools::builtins::DEFAULT_STRICT_TOOLS;

    #[test]
    fn default_strict_tool_names_exist_and_builtins_are_not_marked_strict() {
        assert!(!DEFAULT_STRICT_TOOLS.is_empty());
        let tools = all_builtin_tools();

        for name in DEFAULT_STRICT_TOOLS {
            assert!(
                tools.iter().any(|t| t.name() == *name),
                "default strict tool `{name}` is not a builtin"
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
mod read_only_tests {
    use crate::agent::builtin_tools::all_builtin_tools;
    use std::collections::BTreeSet;

    /// Built-ins that never change workspace files (they read, or only touch
    /// session / knowledge state); everything else takes a snapshot. The remote
    /// tools `use_remote_provider` and `route_delegation_to_peer` are also
    /// read-only but are registered outside `all_builtin_tools`.
    const READ_ONLY: &[&str] = &[
        "browse",
        "complete_task",
        "create_task",
        "delegate",
        "find_symbol_references",
        "get_function",
        "get_symbol",
        "glob",
        "index",
        "knowledge_consolidate",
        "knowledge_ingest",
        "knowledge_list_unconsolidated",
        "knowledge_query",
        "knowledge_stats",
        "language_query",
        "ls",
        "mdq",
        "question",
        "read_task",
        "read_tool",
        "search_text",
        "session_history",
        "todoread",
        "todowrite",
        "update_task",
        "web_fetch",
    ];

    #[test]
    fn read_only_set_is_exactly_the_documented_one() {
        let actual: BTreeSet<String> = all_builtin_tools()
            .iter()
            .filter(|tool| tool.is_read_only())
            .map(|tool| tool.name().to_string())
            .collect();
        let expected: BTreeSet<String> = READ_ONLY.iter().map(|s| s.to_string()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn only_accept_delegation_result_demands_approval_by_default() {
        use crate::config::ApprovalNeed;
        for tool in all_builtin_tools() {
            let expected = if tool.name() == "accept_delegation_result" {
                ApprovalNeed::Session
            } else {
                ApprovalNeed::None
            };
            assert_eq!(tool.default_approval(), expected, "{}", tool.name());
        }
    }

    #[test]
    fn mutating_builtins_are_not_read_only() {
        let tools = all_builtin_tools();
        for name in [
            "edit",
            "multiedit",
            "write_file",
            "delete_file",
            "replace_symbol",
            "shell",
            "accept_delegation_result",
        ] {
            let tool = tools
                .iter()
                .find(|t| t.name() == name)
                .unwrap_or_else(|| panic!("{name}"));
            assert!(!tool.is_read_only(), "{name}");
        }
    }
}
