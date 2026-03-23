//! Knowledge query tool implementation.

use crate::knowledge::{QueryOpts, RetrievalMode};
use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

pub struct KnowledgeQueryTool;

impl Default for KnowledgeQueryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl KnowledgeQueryTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for KnowledgeQueryTool {
    fn name(&self) -> &str {
        "knowledge_query"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "knowledge_query".to_string(),
                description: "Query the knowledge store to retrieve relevant entries and consolidations. Returns information with citations that can be used to answer questions.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question or query to search for"
                        },
                        "scope": {
                            "type": "string",
                            "description": "Knowledge scope to query (defaults to current session)"
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 100,
                            "description": "Maximum number of results to return (default 10)"
                        },
                        "retrieval_mode": {
                            "type": "string",
                            "enum": ["keyword", "hybrid"],
                            "description": "Retrieval mode: 'keyword' (text search) or 'hybrid' (text + structured boosts), default 'hybrid'"
                        },
                        "include_consolidations": {
                            "type": "boolean",
                            "description": "Whether to include consolidation insights (default true)"
                        }
                    },
                    "required": ["question"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        // Extract required fields
        let question = args["question"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'question' field".into()))?;

        // Extract optional fields
        let limit = args["limit"].as_u64().unwrap_or(10) as usize;
        if !(1..=100).contains(&limit) {
            return Err(ToolError::InvalidRequest(
                "limit must be between 1 and 100".into(),
            ));
        }

        let retrieval_mode = match args["retrieval_mode"].as_str() {
            Some("keyword") => RetrievalMode::Keyword,
            Some("hybrid") | None => RetrievalMode::Hybrid,
            Some(other) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Invalid retrieval_mode '{}'. Must be 'keyword' or 'hybrid'",
                    other
                )));
            }
        };

        let include_consolidations = args["include_consolidations"].as_bool().unwrap_or(true);

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

        // Build query options
        let opts = QueryOpts {
            limit,
            include_consolidations,
            retrieval_mode,
        };

        // Execute query
        let result = knowledge_store
            .query(&scope, question, opts)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Query failed: {}", e)))?;

        // Format response
        let mut response = String::new();
        response.push_str(&format!("Found {} entries", result.entries.len()));

        if include_consolidations && !result.consolidations.is_empty() {
            response.push_str(&format!(
                " and {} consolidations",
                result.consolidations.len()
            ));
        }
        response.push_str(&format!(" for scope '{}':\n\n", scope));

        // Format entries
        if !result.entries.is_empty() {
            response.push_str("## Knowledge Entries\n\n");
            for (idx, entry) in result.entries.iter().enumerate() {
                response.push_str(&format!("**[Entry {}]** ({})\n", idx + 1, entry.public_id));
                response.push_str(&format!("Summary: {}\n", entry.summary));
                if !entry.topics.is_empty() {
                    response.push_str(&format!("Topics: {}\n", entry.topics.join(", ")));
                }
                if !entry.entities.is_empty() {
                    response.push_str(&format!("Entities: {}\n", entry.entities.join(", ")));
                }
                response.push_str(&format!("Importance: {:.2}\n", entry.importance));
                response.push_str(&format!("Source: {}\n\n", entry.source));
            }
        }

        // Format consolidations
        if include_consolidations && !result.consolidations.is_empty() {
            response.push_str("## Consolidations\n\n");
            for (idx, cons) in result.consolidations.iter().enumerate() {
                response.push_str(&format!(
                    "**[Consolidation {}]** ({})\n",
                    idx + 1,
                    cons.public_id
                ));
                response.push_str(&format!("Summary: {}\n", cons.summary));
                response.push_str(&format!("Insight: {}\n", cons.insight));
                if !cons.connections.is_empty() {
                    response.push_str(&format!("Connections: {}\n", cons.connections.join(", ")));
                }
                response.push('\n');
            }
        }

        if result.entries.is_empty() && result.consolidations.is_empty() {
            response.push_str("No relevant knowledge found.");
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
    async fn test_knowledge_query_basic() {
        let temp_dir = TempDir::new().unwrap();
        let db = sqlite_conn_with_schema();
        let knowledge_store = Arc::new(SqliteKnowledgeStore::new(db));

        // Ingest some test data
        let _ = knowledge_store
            .ingest(
                "test_session",
                IngestRequest {
                    source: "test".to_string(),
                    raw_text: "The user prefers dark mode".to_string(),
                    summary: "User prefers dark mode".to_string(),
                    entities: vec![],
                    topics: vec!["preferences".to_string()],
                    connections: vec![],
                    importance: 0.7,
                },
            )
            .await
            .unwrap();

        let mut context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        context.with_knowledge_store(knowledge_store);

        let tool = KnowledgeQueryTool::new();

        let args = json!({
            "question": "user preferences",
            "limit": 10
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Failed: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("Found"));
    }

    #[tokio::test]
    async fn test_knowledge_query_missing_question() {
        let temp_dir = TempDir::new().unwrap();
        let context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        let tool = KnowledgeQueryTool::new();

        let args = json!({});

        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }
}
