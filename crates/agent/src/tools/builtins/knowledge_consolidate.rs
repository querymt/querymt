//! Knowledge consolidation tool implementation.

use crate::knowledge::ConsolidateRequest;
use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool};
use serde_json::{Value, json};

pub struct KnowledgeConsolidateTool;

impl Default for KnowledgeConsolidateTool {
    fn default() -> Self {
        Self::new()
    }
}

impl KnowledgeConsolidateTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for KnowledgeConsolidateTool {
    fn name(&self) -> &str {
        "knowledge_consolidate"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "knowledge_consolidate".to_string(),
                description: "Consolidate multiple knowledge entries into a higher-level insight. This marks the source entries as consolidated and stores the synthesis.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "source_ids": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Public IDs of knowledge entries to consolidate"
                        },
                        "summary": {
                            "type": "string",
                            "description": "A concise summary of the consolidated knowledge"
                        },
                        "insight": {
                            "type": "string",
                            "description": "The key insight or pattern derived from the entries"
                        },
                        "connections": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Cross-references or relationships to other concepts (optional)"
                        },
                        "scope": {
                            "type": "string",
                            "description": "Knowledge scope (defaults to current session)"
                        }
                    },
                    "required": ["source_ids", "summary", "insight"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
        // Extract required fields
        let source_ids = args["source_ids"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'source_ids' field".into()))?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<_>>();

        if source_ids.is_empty() {
            return Err(ToolError::InvalidRequest(
                "source_ids must contain at least one entry".into(),
            ));
        }

        let summary = args["summary"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'summary' field".into()))?
            .to_string();

        let insight = args["insight"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'insight' field".into()))?
            .to_string();

        // Extract optional fields
        let connections = args["connections"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

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

        let consolidation = ConsolidateRequest {
            source_entry_public_ids: source_ids.clone(),
            summary,
            insight,
            connections,
        };

        // Store consolidation
        let source_count = source_ids.len() as u32;
        let result = knowledge_store
            .consolidate(&scope, consolidation)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Consolidation failed: {}", e)))?;

        // Emit KnowledgeConsolidated event so the scheduler can react
        context.emit_event(crate::events::AgentEventKind::KnowledgeConsolidated {
            scope: scope.clone(),
            consolidation_public_id: result.public_id.clone(),
            source_count,
        });

        Ok(vec![Content::text(format!(
            "Created consolidation {} from {} entries (scope: {})",
            result.public_id,
            source_ids.len(),
            scope
        ))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_text_block(blocks: Vec<querymt::chat::Content>) -> String {
        blocks
            .into_iter()
            .find_map(|b| match b {
                querymt::chat::Content::Text { text } => Some(text),
                _ => None,
            })
            .unwrap_or_default()
    }
    use crate::knowledge::sqlite::SqliteKnowledgeStore;
    use crate::knowledge::{IngestRequest, KnowledgeStore};
    use crate::test_utils::sqlite_conn_with_schema;
    use crate::tools::AgentToolContext;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_knowledge_consolidate_basic() {
        let temp_dir = TempDir::new().unwrap();
        let db = sqlite_conn_with_schema();
        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(db));

        // Ingest some entries to consolidate
        let entry1 = knowledge_store
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

        let entry2 = knowledge_store
            .ingest(
                "test_session",
                IngestRequest {
                    source: "test".to_string(),
                    raw_text: "User works late at night".to_string(),
                    summary: "Late night work pattern".to_string(),
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

        let tool = KnowledgeConsolidateTool::new();

        let args = json!({
            "source_ids": [entry1.public_id, entry2.public_id],
            "summary": "User is a night owl who prefers dark UI",
            "insight": "The user's late-night work pattern and dark mode preference suggest they are sensitive to screen brightness",
            "connections": ["user_profile", "ui_preferences"]
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Failed: {:?}", result);
        assert!(first_text_block(result.unwrap()).contains("Created consolidation"));
    }

    #[tokio::test]
    async fn test_knowledge_consolidate_missing_fields() {
        let temp_dir = TempDir::new().unwrap();
        let context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        let tool = KnowledgeConsolidateTool::new();

        let args = json!({
            "summary": "Some summary"
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_knowledge_consolidate_empty_source_ids() {
        let temp_dir = TempDir::new().unwrap();
        let db = sqlite_conn_with_schema();
        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(db));

        let mut context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        context.with_knowledge_store(knowledge_store);

        let tool = KnowledgeConsolidateTool::new();

        let args = json!({
            "source_ids": [],
            "summary": "Some summary",
            "insight": "Some insight"
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least one"));
    }

    #[tokio::test]
    async fn test_knowledge_consolidate_emits_event() {
        use crate::event_fanout::EventFanout;
        use crate::event_sink::EventSink;
        use crate::session::backend::StorageBackend;
        use crate::session::sqlite_storage::SqliteStorage;

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let fanout = Arc::new(EventFanout::new());
        let event_sink = Arc::new(EventSink::new(storage.event_journal(), fanout.clone()));

        let mut rx = fanout.subscribe();

        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(sqlite_conn_with_schema()));

        // Ingest entries first
        let entry1 = knowledge_store
            .ingest(
                "test_session",
                IngestRequest {
                    source: "test".to_string(),
                    raw_text: "Fact A".to_string(),
                    summary: "Fact A".to_string(),
                    entities: vec![],
                    topics: vec![],
                    connections: vec![],
                    importance: 0.5,
                },
            )
            .await
            .unwrap();

        let mut context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        context.with_knowledge_store(knowledge_store);
        context.with_event_sink(event_sink);

        let tool = KnowledgeConsolidateTool::new();
        let args = json!({
            "source_ids": [entry1.public_id],
            "summary": "Consolidated fact",
            "insight": "Key insight"
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Failed: {:?}", result);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut found = false;
        while let Ok(envelope) = rx.try_recv() {
            if let crate::events::EventEnvelope::Durable(ev) = envelope
                && matches!(
                    ev.kind,
                    crate::events::AgentEventKind::KnowledgeConsolidated { .. }
                )
            {
                found = true;
                break;
            }
        }
        assert!(found, "Expected KnowledgeConsolidated event to be emitted");
    }
}
