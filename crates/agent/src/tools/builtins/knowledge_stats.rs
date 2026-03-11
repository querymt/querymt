//! Knowledge stats tool implementation.


use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{json, Value};

pub struct KnowledgeStatsTool;

impl Default for KnowledgeStatsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl KnowledgeStatsTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for KnowledgeStatsTool {
    fn name(&self) -> &str {
        "knowledge_stats"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "knowledge_stats".to_string(),
                description: "Get statistics about the knowledge store for a given scope. Returns counts of entries, consolidations, and timestamps.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
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

        // Get stats
        let stats = knowledge_store
            .stats(&scope)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to get stats: {}", e)))?;

        // Format response
        let mut response = format!("Knowledge statistics for scope '{}':\n\n", scope);
        response.push_str(&format!("Total entries: {}\n", stats.total_entries));
        response.push_str(&format!(
            "Unconsolidated entries: {}\n",
            stats.unconsolidated_entries
        ));
        response.push_str(&format!(
            "Total consolidations: {}\n",
            stats.total_consolidations
        ));

        if let Some(latest_entry_at) = stats.latest_entry_at {
            response.push_str(&format!(
                "Latest entry: {}\n",
                latest_entry_at.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "unknown".to_string())
            ));
        } else {
            response.push_str("Latest entry: none\n");
        }

        if let Some(latest_consolidation_at) = stats.latest_consolidation_at {
            response.push_str(&format!(
                "Latest consolidation: {}\n",
                latest_consolidation_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "unknown".to_string())
            ));
        } else {
            response.push_str("Latest consolidation: none\n");
        }

        // Add recommendation if there are unconsolidated entries
        if stats.unconsolidated_entries > 0 {
            response.push_str(&format!(
                "\nNote: There are {} unconsolidated entries that could be consolidated into insights.",
                stats.unconsolidated_entries
            ));
        }

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
    async fn test_knowledge_stats_empty() {
        let temp_dir = TempDir::new().unwrap();
        let db = sqlite_conn_with_schema();
        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(db));

        let mut context =
            AgentToolContext::basic("test_session".to_string(), Some(temp_dir.path().to_path_buf()));
        context.with_knowledge_store(knowledge_store);

        let tool = KnowledgeStatsTool::new();

        let args = json!({});

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Failed: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("Total entries: 0"));
    }

    #[tokio::test]
    async fn test_knowledge_stats_with_data() {
        let temp_dir = TempDir::new().unwrap();
        let db = sqlite_conn_with_schema();
        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(db));

        // Ingest some data
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

        let mut context =
            AgentToolContext::basic("test_session".to_string(), Some(temp_dir.path().to_path_buf()));
        context.with_knowledge_store(knowledge_store);

        let tool = KnowledgeStatsTool::new();

        let args = json!({});

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Failed: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("Total entries: 2"));
        assert!(output.contains("Unconsolidated entries: 2"));
    }
}
