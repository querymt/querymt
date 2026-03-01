//! Context search (retrieval) tool for querying indexed content.
//!
//! Searches the FTS5 index for content previously stored by context-safe
//! tools (`context_execute`, `context_execute_file`, `context_fetch`,
//! `batch_execute`). Supports batched queries and source-scoped filtering.

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{Tool as ToolTrait, ToolContext, ToolError};

/// Default maximum results per query.
const DEFAULT_MAX_RESULTS: usize = 10;

/// Maximum queries allowed in a single batched call.
const MAX_BATCH_QUERIES: usize = 20;

pub struct ContextSearchTool;

impl Default for ContextSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextSearchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ContextSearchTool {
    fn name(&self) -> &str {
        "context_search"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: concat!(
                    "Search indexed content from previous context-safe tool calls ",
                    "(context_execute, context_execute_file, context_fetch, batch_execute). ",
                    "Supports single or batched queries with optional source-label scoping. ",
                    "Query formulation guidance: prefer identifier/keyword queries (for example: ",
                    "'parse_service_prefix', 'split_once', 'deny_cidrs') over long punctuation-heavy code snippets. ",
                    "If an exact code snippet search returns no matches, retry with shorter token queries and/or ",
                    "split into 2-4 key terms using AND semantics. ",
                    "Use list_sources first, then set source_label to narrow searches."
                )
                .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query text. Prefer short token-based queries (identifiers, function names, key terms). Avoid long punctuation-heavy snippets unless necessary. Supports FTS5 syntax (AND, OR, NOT, phrase matching with quotes)."
                        },
                        "queries": {
                            "type": "array",
                            "description": "Batched queries. Each returns independent results. Use this instead of multiple tool calls.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "query": {
                                        "type": "string",
                                        "description": "Search query text. Prefer compact token queries (e.g. function names, identifiers). If no results, retry with fewer tokens or an AND-style keyword query."
                                    },
                                    "source_label": {
                                        "type": "string",
                                        "description": "Optional: limit search to a specific source label."
                                    },
                                    "max_results": {
                                        "type": "integer",
                                        "description": "Maximum results for this query (default: 10).",
                                        "default": 10
                                    }
                                },
                                "required": ["query"]
                            },
                            "maxItems": 20
                        },
                        "source_label": {
                            "type": "string",
                            "description": "Optional: limit search to outputs from a specific source label."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum results to return (default: 10).",
                            "default": 10
                        },
                        "list_sources": {
                            "type": "boolean",
                            "description": "If true, list all available indexed sources instead of searching.",
                            "default": false
                        }
                    }
                }),
            },
        }
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let list_sources = args
            .get("list_sources")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if list_sources {
            return self.list_sources(context).await;
        }

        // Check for batched queries
        if let Some(queries) = args.get("queries").and_then(Value::as_array) {
            if queries.len() > MAX_BATCH_QUERIES {
                return Err(ToolError::InvalidRequest(format!(
                    "maximum {} queries per batch",
                    MAX_BATCH_QUERIES
                )));
            }
            return self.run_batched_queries(queries, context).await;
        }

        // Single query mode
        let query = args.get("query").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidRequest(
                "either 'query', 'queries', or 'list_sources' is required".to_string(),
            )
        })?;

        let source_label = args.get("source_label").and_then(Value::as_str);

        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_RESULTS as u64) as usize;

        let results = self
            .search(context, query, source_label, max_results)
            .await?;

        let result = json!({
            "query": query,
            "results": results,
            "total_results": results.as_array().map(|a| a.len()).unwrap_or(0),
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

impl ContextSearchTool {
    async fn search(
        &self,
        context: &dyn ToolContext,
        query: &str,
        source_label: Option<&str>,
        max_results: usize,
    ) -> Result<Value, ToolError> {
        match context
            .search_context_content(query, source_label, max_results)
            .await
        {
            Ok(snippets) => serde_json::to_value(snippets)
                .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e))),
            Err(e) => {
                // Return empty results rather than failing the tool call
                log::debug!("context_search: retrieval failed: {}", e);
                Ok(json!([]))
            }
        }
    }

    async fn run_batched_queries(
        &self,
        queries: &[Value],
        context: &dyn ToolContext,
    ) -> Result<String, ToolError> {
        let mut results = Vec::new();

        for query_spec in queries {
            let query = query_spec
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("");

            if query.is_empty() {
                results.push(json!({
                    "query": "",
                    "error": "empty query",
                    "results": []
                }));
                continue;
            }

            let source_label = query_spec.get("source_label").and_then(Value::as_str);

            let max_results = query_spec
                .get("max_results")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_RESULTS as u64) as usize;

            let search_results = self
                .search(context, query, source_label, max_results)
                .await?;

            results.push(json!({
                "query": query,
                "results": search_results,
                "total_results": search_results.as_array().map(|a| a.len()).unwrap_or(0),
            }));
        }

        let result = json!({
            "batched": true,
            "queries": results
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }

    async fn list_sources(&self, context: &dyn ToolContext) -> Result<String, ToolError> {
        let sources = context.list_context_sources().await.unwrap_or_default();
        let result = json!({
            "sources": sources,
            "hint": if sources.is_empty() {
                "No indexed sources found. Use context_execute, context_execute_file, context_fetch, or batch_execute to index content first."
            } else {
                "Use source labels with context_search to scope your queries."
            }
        });
        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;

    #[tokio::test]
    async fn test_context_search_list_sources() {
        let context = AgentToolContext::basic("test".to_string(), None);
        let tool = ContextSearchTool::new();

        let args = json!({ "list_sources": true });
        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        // Should return sources array (possibly empty)
        assert!(parsed.get("sources").is_some());
    }

    #[tokio::test]
    async fn test_context_search_single_query() {
        let context = AgentToolContext::basic("test".to_string(), None);
        let tool = ContextSearchTool::new();

        let args = json!({ "query": "test query" });
        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["query"], "test query");
        // results may be empty since no content is indexed in basic context
        assert!(parsed.get("results").is_some());
    }

    #[tokio::test]
    async fn test_context_search_batched_queries() {
        let context = AgentToolContext::basic("test".to_string(), None);
        let tool = ContextSearchTool::new();

        let args = json!({
            "queries": [
                { "query": "first query" },
                { "query": "second query", "max_results": 3 }
            ]
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["batched"], true);
        let queries = parsed["queries"].as_array().unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0]["query"], "first query");
        assert_eq!(queries[1]["query"], "second query");
    }

    #[tokio::test]
    async fn test_context_search_requires_query_or_list() {
        let context = AgentToolContext::basic("test".to_string(), None);
        let tool = ContextSearchTool::new();

        let args = json!({});
        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_context_search_too_many_queries() {
        let context = AgentToolContext::basic("test".to_string(), None);
        let tool = ContextSearchTool::new();

        let queries: Vec<Value> = (0..21)
            .map(|i| json!({ "query": format!("q{}", i) }))
            .collect();
        let args = json!({ "queries": queries });
        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }
}
