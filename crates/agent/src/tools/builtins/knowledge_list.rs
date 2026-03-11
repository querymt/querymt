//! Knowledge list unconsolidated tool implementation.

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

pub struct KnowledgeListTool;

impl Default for KnowledgeListTool {
    fn default() -> Self {
        Self::new()
    }
}

impl KnowledgeListTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for KnowledgeListTool {
    fn name(&self) -> &str {
        "knowledge_list_unconsolidated"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "knowledge_list_unconsolidated".to_string(),
                description: "List knowledge entries that have not been consolidated yet. Use this to find entries that need to be synthesized into higher-level insights.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 100,
                            "description": "Maximum number of entries to return (default 20)"
                        },
                        "scope": {
                            "type": "string",
                            "description": "Knowledge scope to query (defaults to current session)"
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        // Extract optional fields
        let limit = args["limit"].as_u64().unwrap_or(20) as usize;
        if !(1..=100).contains(&limit) {
            return Err(ToolError::InvalidRequest(
                "limit must be between 1 and 100".into(),
            ));
        }

        // Determine scope with policy validation
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

        // Get knowledge store
        let knowledge_store = context
            .knowledge_store()
            .ok_or_else(|| ToolError::ProviderError("Knowledge store not available".into()))?;

        // List unconsolidated entries
        let entries = knowledge_store
            .list_unconsolidated(&scope, limit)
            .await
            .map_err(|e| {
                ToolError::ProviderError(format!("Failed to list unconsolidated entries: {}", e))
            })?;

        // Format response
        if entries.is_empty() {
            return Ok(format!(
                "No unconsolidated knowledge entries found for scope '{}'",
                scope
            ));
        }

        let mut response = format!(
            "Found {} unconsolidated knowledge entries for scope '{}':\n\n",
            entries.len(),
            scope
        );

        for (idx, entry) in entries.iter().enumerate() {
            response.push_str(&format!(
                "**{}. {}** ({})\n",
                idx + 1,
                entry.public_id,
                entry.source
            ));
            response.push_str(&format!("   Summary: {}\n", entry.summary));
            if !entry.topics.is_empty() {
                response.push_str(&format!("   Topics: {}\n", entry.topics.join(", ")));
            }
            if !entry.entities.is_empty() {
                response.push_str(&format!("   Entities: {}\n", entry.entities.join(", ")));
            }
            response.push_str(&format!("   Importance: {:.2}\n", entry.importance));
            response.push_str(&format!(
                "   Created: {}\n\n",
                entry
                    .created_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "unknown".to_string())
            ));
        }

        response
            .push_str("\nUse knowledge_consolidate with the public IDs to create consolidations.");

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::sqlite::SqliteKnowledgeStore;
    use crate::knowledge::{IngestRequest, KnowledgeStore};
    use crate::test_utils::sqlite_conn_with_schema;
    use crate::tools::AgentToolContext;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_knowledge_list_unconsolidated() {
        let temp_dir = TempDir::new().unwrap();
        let db = sqlite_conn_with_schema();
        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(db));

        // Ingest some unconsolidated entries
        let _ = knowledge_store
            .ingest(
                "test_session",
                IngestRequest {
                    source: "test".to_string(),
                    raw_text: "User prefers dark mode".to_string(),
                    summary: "Dark mode preference".to_string(),
                    entities: vec![],
                    topics: vec!["preferences".to_string()],
                    connections: vec![],
                    importance: 0.7,
                },
            )
            .await
            .unwrap();

        let _ = knowledge_store
            .ingest(
                "test_session",
                IngestRequest {
                    source: "test".to_string(),
                    raw_text: "User works late at night".to_string(),
                    summary: "Late night work".to_string(),
                    entities: vec![],
                    topics: vec!["habits".to_string()],
                    connections: vec![],
                    importance: 0.6,
                },
            )
            .await
            .unwrap();

        let mut context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        context.with_knowledge_store(knowledge_store);

        let tool = KnowledgeListTool::new();

        let args = json!({
            "limit": 10
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Failed: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("Found 2 unconsolidated"));
        assert!(output.contains("Dark mode preference"));
        assert!(output.contains("Late night work"));
    }

    #[tokio::test]
    async fn test_knowledge_list_unconsolidated_empty() {
        let temp_dir = TempDir::new().unwrap();
        let db = sqlite_conn_with_schema();
        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(db));

        let mut context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        context.with_knowledge_store(knowledge_store);

        let tool = KnowledgeListTool::new();

        let args = json!({});

        let result = tool.call(args, &context).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("No unconsolidated"));
    }
}
