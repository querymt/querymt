//! Language query tool — access VS Code's language intelligence (LSP data).
//!
//! This tool enables the agent to query semantic information from the editor:
//! diagnostics (errors/warnings), find references, go-to-definition, document
//! symbols, workspace symbol search, hover info, and type definitions.
//!
//! Only available when connected to a client that supports workspace queries
//! (currently the VS Code extension). In CLI mode, returns an error message.

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{Tool as ToolTrait, ToolContext, ToolError};
use crate::workspace_query::WorkspaceQueryRequest;

pub struct LanguageQueryTool;

impl Default for LanguageQueryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageQueryTool {
    pub fn new() -> Self {
        Self
    }
}

/// Convert a file path (relative or absolute) to a `file://` URI.
///
/// Uses the tool context's `resolve_path()` for relative path resolution.
fn path_to_uri(context: &dyn ToolContext, path: &str) -> Result<String, ToolError> {
    // If already a file:// URI, pass through
    if path.starts_with("file://") {
        return Ok(path.to_string());
    }
    let resolved = context.resolve_path(path)?;
    Ok(format!("file://{}", resolved.display()))
}

/// Extract a required string field from the tool arguments.
fn require_string(args: &Value, field: &str) -> Result<String, ToolError> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidRequest(format!("Missing required field: {}", field)))
}

/// Extract a required u32 field from the tool arguments.
fn require_u32(args: &Value, field: &str) -> Result<u32, ToolError> {
    args.get(field)
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| ToolError::InvalidRequest(format!("Missing required field: {}", field)))
}

/// Build a `WorkspaceQueryRequest` from the tool arguments, resolving paths to URIs.
fn build_request(
    args: &Value,
    context: &dyn ToolContext,
) -> Result<WorkspaceQueryRequest, ToolError> {
    let action = require_string(args, "action")?;

    match action.as_str() {
        "diagnostics" => {
            let path = require_string(args, "uri")?;
            let uri = path_to_uri(context, &path)?;
            Ok(WorkspaceQueryRequest::Diagnostics { uri })
        }
        "references" => {
            let path = require_string(args, "uri")?;
            let uri = path_to_uri(context, &path)?;
            let line = require_u32(args, "line")?;
            let character = require_u32(args, "character")?;
            Ok(WorkspaceQueryRequest::References {
                uri,
                line,
                character,
            })
        }
        "definition" => {
            let path = require_string(args, "uri")?;
            let uri = path_to_uri(context, &path)?;
            let line = require_u32(args, "line")?;
            let character = require_u32(args, "character")?;
            Ok(WorkspaceQueryRequest::Definition {
                uri,
                line,
                character,
            })
        }
        "document_symbols" => {
            let path = require_string(args, "uri")?;
            let uri = path_to_uri(context, &path)?;
            Ok(WorkspaceQueryRequest::DocumentSymbols { uri })
        }
        "workspace_symbols" => {
            let query = require_string(args, "query")?;
            Ok(WorkspaceQueryRequest::WorkspaceSymbols { query })
        }
        "hover" => {
            let path = require_string(args, "uri")?;
            let uri = path_to_uri(context, &path)?;
            let line = require_u32(args, "line")?;
            let character = require_u32(args, "character")?;
            Ok(WorkspaceQueryRequest::Hover {
                uri,
                line,
                character,
            })
        }
        "type_definition" => {
            let path = require_string(args, "uri")?;
            let uri = path_to_uri(context, &path)?;
            let line = require_u32(args, "line")?;
            let character = require_u32(args, "character")?;
            Ok(WorkspaceQueryRequest::TypeDefinition {
                uri,
                line,
                character,
            })
        }
        other => Err(ToolError::InvalidRequest(format!(
            "Unknown action: {}. Valid actions: diagnostics, references, definition, \
             document_symbols, workspace_symbols, hover, type_definition",
            other
        ))),
    }
}

#[async_trait]
impl ToolTrait for LanguageQueryTool {
    fn name(&self) -> &str {
        "language_query"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Query language intelligence from the editor (VS Code). \
                     Returns semantic information like diagnostics (errors/warnings), \
                     find references, go-to-definition, document symbols, workspace \
                     symbol search, hover docs, and type definitions. Only available \
                     when connected to a VS Code client with workspace query support."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "required": ["action"],
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": [
                                "diagnostics",
                                "references",
                                "definition",
                                "document_symbols",
                                "workspace_symbols",
                                "hover",
                                "type_definition"
                            ],
                            "description": "The type of language query to perform."
                        },
                        "uri": {
                            "type": "string",
                            "description": "File path (relative or absolute; will be converted to file:// URI). Required for all actions except workspace_symbols."
                        },
                        "line": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "0-based line number. Required for: references, definition, hover, type_definition."
                        },
                        "character": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "0-based character offset. Required for: references, definition, hover, type_definition."
                        },
                        "query": {
                            "type": "string",
                            "description": "Search string. Required for workspace_symbols."
                        }
                    }
                }),
            },
        }
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
        // Check if workspace query bridge is available
        let bridge = match context.workspace_query_bridge() {
            Some(bridge) => bridge.clone(),
            None => {
                return Ok(vec![Content::text(
                    "language_query is not available in this mode. \
                     It requires a VS Code client with workspace query support. \
                     Use file reading and search tools instead.",
                )]);
            }
        };

        // Build the request from tool arguments
        let request = build_request(&args, context)?;

        // Send the query through the bridge and await the response
        let response = bridge.workspace_query(request).await.map_err(|e| {
            ToolError::ProviderError(format!("Workspace query failed: {}", e.message))
        })?;

        // Format the response as a string for the LLM
        Ok(vec![Content::text(response.to_string())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_is_language_query() {
        let tool = LanguageQueryTool::new();
        assert_eq!(tool.name(), "language_query");
    }

    #[test]
    fn tool_definition_has_correct_schema() {
        let tool = LanguageQueryTool::new();
        let def = tool.definition();
        assert_eq!(def.function.name, "language_query");
        assert!(!def.function.description.is_empty());
        let params = &def.function.parameters;
        assert_eq!(params["type"], "object");
        let props = &params["properties"];
        assert!(props.get("action").is_some());
        assert!(props.get("uri").is_some());
        assert!(props.get("line").is_some());
        assert!(props.get("character").is_some());
        assert!(props.get("query").is_some());
    }

    #[test]
    fn build_request_diagnostics() {
        let args = json!({"action": "diagnostics", "uri": "/src/main.rs"});
        let ctx = crate::tools::context_impl::AgentToolContext::basic(
            "s1".into(),
            Some("/project".into()),
        );
        let req = build_request(&args, &ctx).unwrap();
        assert!(
            matches!(req, WorkspaceQueryRequest::Diagnostics { uri } if uri.contains("main.rs"))
        );
    }

    #[test]
    fn build_request_references() {
        let args =
            json!({"action": "references", "uri": "/src/lib.rs", "line": 10, "character": 5});
        let ctx = crate::tools::context_impl::AgentToolContext::basic(
            "s1".into(),
            Some("/project".into()),
        );
        let req = build_request(&args, &ctx).unwrap();
        assert!(matches!(
            req,
            WorkspaceQueryRequest::References {
                line: 10,
                character: 5,
                ..
            }
        ));
    }

    #[test]
    fn build_request_workspace_symbols() {
        let args = json!({"action": "workspace_symbols", "query": "MyStruct"});
        let ctx = crate::tools::context_impl::AgentToolContext::basic(
            "s1".into(),
            Some("/project".into()),
        );
        let req = build_request(&args, &ctx).unwrap();
        assert!(
            matches!(req, WorkspaceQueryRequest::WorkspaceSymbols { query } if query == "MyStruct")
        );
    }

    #[test]
    fn build_request_unknown_action_errors() {
        let args = json!({"action": "unknown_thing"});
        let ctx = crate::tools::context_impl::AgentToolContext::basic(
            "s1".into(),
            Some("/project".into()),
        );
        let result = build_request(&args, &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn build_request_missing_uri_errors() {
        let args = json!({"action": "diagnostics"});
        let ctx = crate::tools::context_impl::AgentToolContext::basic(
            "s1".into(),
            Some("/project".into()),
        );
        let result = build_request(&args, &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn path_to_uri_passthrough() {
        let ctx = crate::tools::context_impl::AgentToolContext::basic(
            "s1".into(),
            Some("/project".into()),
        );
        let uri = path_to_uri(&ctx, "file:///already/a/uri").unwrap();
        assert_eq!(uri, "file:///already/a/uri");
    }

    #[test]
    fn path_to_uri_absolute() {
        let ctx = crate::tools::context_impl::AgentToolContext::basic(
            "s1".into(),
            Some("/project".into()),
        );
        let uri = path_to_uri(&ctx, "/src/main.rs").unwrap();
        assert_eq!(uri, "file:///src/main.rs");
    }

    #[test]
    fn path_to_uri_relative() {
        let ctx = crate::tools::context_impl::AgentToolContext::basic(
            "s1".into(),
            Some("/project".into()),
        );
        let uri = path_to_uri(&ctx, "src/main.rs").unwrap();
        assert_eq!(uri, "file:///project/src/main.rs");
    }
}
