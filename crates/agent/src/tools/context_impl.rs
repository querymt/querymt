//! Agent-specific implementation of ToolContext

use async_trait::async_trait;
use serde_json::json;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::elicitation::{ElicitationAction, ElicitationResponse};
use crate::tools::context::{ToolContext, ToolError};

/// A request to elicit information from the user, sent from tool context to the agent
pub struct ElicitationRequest {
    pub elicitation_id: String,
    pub message: String,
    pub requested_schema: serde_json::Value,
    pub source: String,
    pub response_tx: oneshot::Sender<ElicitationResponse>,
}

/// Implementation of ToolContext that provides access to agent services
pub struct AgentToolContext {
    session_id: String,
    cwd: Option<PathBuf>,
    agent_registry: Option<Arc<dyn crate::delegation::AgentRegistry>>,
    elicitation_tx: Option<mpsc::Sender<ElicitationRequest>>,
    cancellation_token: CancellationToken,
    read_only: bool,
}

impl AgentToolContext {
    pub fn new(
        session_id: String,
        cwd: Option<PathBuf>,
        agent_registry: Option<Arc<dyn crate::delegation::AgentRegistry>>,
        elicitation_tx: Option<mpsc::Sender<ElicitationRequest>>,
    ) -> Self {
        Self {
            session_id,
            cwd,
            agent_registry,
            elicitation_tx,
            cancellation_token: CancellationToken::new(),
            read_only: false,
        }
    }

    /// Create a context with an explicit cancellation token.
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = token;
        self
    }

    /// Mark this context as read-only (Plan/Review mode).
    ///
    /// Write tools check `is_read_only()` to produce clear
    /// `PermissionDenied` errors before hitting the OS sandbox.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Create a basic context for testing or simple operations
    pub fn basic(session_id: String, cwd: Option<PathBuf>) -> Self {
        Self::new(session_id, cwd, None, None)
    }

    /// Create a basic read-only context for testing.
    pub fn basic_read_only(session_id: String, cwd: Option<PathBuf>) -> Self {
        Self::new(session_id, cwd, None, None).with_read_only(true)
    }
}

#[async_trait]
impl ToolContext for AgentToolContext {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn agent_registry(&self) -> Option<Arc<dyn crate::delegation::AgentRegistry>> {
        self.agent_registry.clone()
    }

    async fn record_progress(
        &self,
        _kind: &str,
        _content: String,
        _metadata: Option<serde_json::Value>,
    ) -> Result<String, ToolError> {
        Ok(format!("progress_{}", uuid::Uuid::new_v4()))
    }

    async fn ask_question(
        &self,
        question_id: &str,
        question: &str,
        header: &str,
        options: &[(String, String)],
        multiple: bool,
    ) -> Result<Vec<String>, ToolError> {
        if let Some(tx) = &self.elicitation_tx {
            // Build MCP elicitation schema from question options
            let mut properties = serde_json::Map::new();

            if multiple {
                // Multi-select: use array type with items.anyOf
                let any_of: Vec<serde_json::Value> = options
                    .iter()
                    .map(|(label, desc)| {
                        json!({
                            "const": label,
                            "title": desc
                        })
                    })
                    .collect();
                properties.insert(
                    "selection".to_string(),
                    json!({
                        "type": "array",
                        "title": header,
                        "description": question,
                        "items": { "anyOf": any_of }
                    }),
                );
            } else {
                // Single select: use string type with oneOf
                let one_of: Vec<serde_json::Value> = options
                    .iter()
                    .map(|(label, desc)| {
                        json!({
                            "const": label,
                            "title": desc
                        })
                    })
                    .collect();
                properties.insert(
                    "selection".to_string(),
                    json!({
                        "type": "string",
                        "title": header,
                        "description": question,
                        "oneOf": one_of
                    }),
                );
            }

            let schema = json!({
                "type": "object",
                "properties": properties,
                "required": ["selection"]
            });

            // Send through elicitation channel
            let (response_tx, response_rx) = oneshot::channel();
            let request = ElicitationRequest {
                elicitation_id: question_id.to_string(),
                message: question.to_string(),
                requested_schema: schema,
                source: "builtin:question".to_string(),
                response_tx,
            };

            tx.send(request)
                .await
                .map_err(|_| ToolError::Other(anyhow::anyhow!("Elicitation channel closed")))?;

            let response = response_rx.await.map_err(|_| {
                ToolError::Other(anyhow::anyhow!("Elicitation response channel dropped"))
            })?;

            // Extract answers from the MCP response
            match response.action {
                ElicitationAction::Accept => {
                    if let Some(content) = response.content {
                        // Extract "selection" from content
                        match content.get("selection") {
                            Some(serde_json::Value::Array(arr)) => Ok(arr
                                .iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()),
                            Some(serde_json::Value::String(s)) => Ok(vec![s.clone()]),
                            _ => Err(ToolError::ProviderError(
                                "Invalid elicitation response format".into(),
                            )),
                        }
                    } else {
                        Err(ToolError::ProviderError(
                            "No content in accepted elicitation response".into(),
                        ))
                    }
                }
                ElicitationAction::Decline => {
                    Err(ToolError::ProviderError("User declined question".into()))
                }
                ElicitationAction::Cancel => {
                    Err(ToolError::ProviderError("User cancelled question".into()))
                }
            }
        } else {
            // Fall back to default stdin/stdout implementation
            <dyn ToolContext>::ask_question(self, question_id, question, header, options, multiple)
                .await
        }
    }

    fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_path_resolution() {
        let temp_dir = TempDir::new().unwrap();
        let context = AgentToolContext::basic(
            "test_session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );

        // Test relative path resolution
        let resolved = context.resolve_path("test.txt").unwrap();
        assert_eq!(resolved, temp_dir.path().join("test.txt"));

        // Test absolute path passthrough
        let abs_path = "/absolute/path.txt";
        let resolved = context.resolve_path(abs_path).unwrap();
        assert_eq!(resolved, PathBuf::from(abs_path));
    }

    #[test]
    fn test_path_resolution_without_cwd() {
        let context = AgentToolContext::basic("test_session".to_string(), None);

        // Test that relative path fails without cwd
        let result = context.resolve_path("test.txt");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no working directory")
        );

        // Test that absolute path still works
        let abs_path = "/absolute/path.txt";
        let resolved = context.resolve_path(abs_path).unwrap();
        assert_eq!(resolved, PathBuf::from(abs_path));
    }

    #[test]
    fn test_session_id() {
        let context =
            AgentToolContext::basic("test_session_123".to_string(), Some(PathBuf::from("/tmp")));

        assert_eq!(context.session_id(), "test_session_123");
    }

    #[test]
    fn test_cwd() {
        let cwd = PathBuf::from("/workspace");
        let context = AgentToolContext::basic("session".to_string(), Some(cwd.clone()));

        assert_eq!(context.cwd(), Some(cwd.as_path()));
    }

    #[test]
    fn test_cwd_none() {
        let context = AgentToolContext::basic("session".to_string(), None);

        assert_eq!(context.cwd(), None);
    }
}
