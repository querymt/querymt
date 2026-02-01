//! Agent-specific implementation of ToolContext

use async_trait::async_trait;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::tools::context::{ToolContext, ToolError};

/// Implementation of ToolContext that provides access to agent services
pub struct AgentToolContext {
    session_id: String,
    cwd: Option<PathBuf>,
    agent_registry: Option<Arc<dyn crate::delegation::AgentRegistry>>,
}

impl AgentToolContext {
    pub fn new(
        session_id: String,
        cwd: Option<PathBuf>,
        agent_registry: Option<Arc<dyn crate::delegation::AgentRegistry>>,
    ) -> Self {
        Self {
            session_id,
            cwd,
            agent_registry,
        }
    }

    /// Create a basic context for testing or simple operations
    pub fn basic(session_id: String, cwd: Option<PathBuf>) -> Self {
        Self::new(session_id, cwd, None)
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
