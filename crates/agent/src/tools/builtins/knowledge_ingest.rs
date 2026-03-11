//! Knowledge ingest tool implementation.

use crate::knowledge::IngestRequest;
use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

pub struct KnowledgeIngestTool;

impl Default for KnowledgeIngestTool {
    fn default() -> Self {
        Self::new()
    }
}

impl KnowledgeIngestTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for KnowledgeIngestTool {
    fn name(&self) -> &str {
        "knowledge_ingest"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "knowledge_ingest".to_string(),
                description: "Ingest raw information into the knowledge store. Use this to save important facts, insights, or patterns for later retrieval and consolidation.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "The raw text to ingest"
                        },
                        "source": {
                            "type": "string",
                            "description": "Where this information came from (e.g. 'user_message', 'tool_output', 'session:abc')"
                        },
                        "summary": {
                            "type": "string",
                            "description": "A concise summary of the information (optional, defaults to truncated text)"
                        },
                        "entities": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Named entities extracted from the text (optional)"
                        },
                        "topics": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Topics/tags for categorization (optional)"
                        },
                        "importance": {
                            "type": "number",
                            "minimum": 0.0,
                            "maximum": 1.0,
                            "description": "Importance score from 0.0 (trivial) to 1.0 (critical), defaults to 0.5"
                        },
                        "scope": {
                            "type": "string",
                            "description": "Knowledge scope (defaults to current session)"
                        }
                    },
                    "required": ["text", "source"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        // Extract required fields
        let raw_text = args["text"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'text' field".into()))?
            .to_string();

        let source = args["source"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'source' field".into()))?
            .to_string();

        // Extract optional fields
        let summary = args["summary"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                // Default summary: truncate to 200 chars
                if raw_text.len() > 200 {
                    format!("{}...", &raw_text[..200])
                } else {
                    raw_text.clone()
                }
            });

        let entities = args["entities"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let topics = args["topics"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let importance = args["importance"].as_f64().unwrap_or(0.5);
        if !(0.0..=1.0).contains(&importance) {
            return Err(ToolError::InvalidRequest(
                "importance must be between 0.0 and 1.0".into(),
            ));
        }

        // Determine scope (default to session) with policy validation
        let session_public_id = context
            .session_public_id()
            .ok_or_else(|| ToolError::InvalidRequest("No session context available".into()))?;
        let scope = if let Some(scope_arg) = args["scope"].as_str() {
            context
                .scope_policy()
                .validate_scope(&session_public_id, scope_arg)
                .map_err(|e| ToolError::PermissionDenied(e.to_string()))?;
            scope_arg.to_string()
        } else {
            session_public_id
        };

        // Get knowledge store from context
        let knowledge_store = context
            .knowledge_store()
            .ok_or_else(|| ToolError::ProviderError("Knowledge store not available".into()))?;

        // Create ingest request
        let request = IngestRequest {
            source,
            raw_text,
            summary,
            entities,
            topics,
            connections: Vec::new(), // Not exposed in tool API
            importance,
        };

        // Ingest and get entry
        let entry = knowledge_store
            .ingest(&scope, request)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to ingest: {}", e)))?;

        Ok(format!(
            "Ingested knowledge entry {} (scope: {}, importance: {:.2})",
            entry.public_id, scope, importance
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::sqlite::SqliteKnowledgeStore;
    use crate::test_utils::sqlite_conn_with_schema;
    use crate::tools::AgentToolContext;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_knowledge_ingest_basic() {
        let temp_dir = TempDir::new().unwrap();
        let db = sqlite_conn_with_schema();
        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(db));

        let mut context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        context.with_knowledge_store(knowledge_store);

        let tool = KnowledgeIngestTool::new();

        let args = json!({
            "text": "The user prefers dark mode",
            "source": "user_message",
            "summary": "User prefers dark mode",
            "topics": ["preferences", "ui"],
            "importance": 0.7
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Failed: {:?}", result);
        assert!(result.unwrap().contains("Ingested knowledge entry"));
    }

    #[tokio::test]
    async fn test_knowledge_ingest_missing_required() {
        let temp_dir = TempDir::new().unwrap();
        let context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        let tool = KnowledgeIngestTool::new();

        let args = json!({
            "source": "user_message"
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }
}
