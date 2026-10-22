//! Tool registry for managing and finding tools

use crate::tools::context::{FileAccess, Tool};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Registry for managing available tools
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn add(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.name().to_string(), tool);
        self
    }

    pub fn extend(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> &mut Self {
        for tool in tools {
            self.add(tool);
        }
        self
    }

    pub fn definitions(&self) -> Vec<querymt::chat::Tool> {
        self.definitions_for_cwd(None)
    }

    pub fn definitions_for_cwd(&self, cwd: Option<&Path>) -> Vec<querymt::chat::Tool> {
        self.tools
            .values()
            .map(|tool| tool.definition_for_cwd(cwd))
            .collect()
    }

    pub fn definition_for_cwd(
        &self,
        name: &str,
        cwd: Option<&Path>,
    ) -> Option<querymt::chat::Tool> {
        self.tools
            .get(name)
            .map(|tool| tool.definition_for_cwd(cwd))
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Declared file access of one call, as seen by the scheduler and by
    /// file-aware middleware. Unknown tools declare [`FileAccess::None`].
    ///
    /// A panic in the tool's `file_access` is contained: only that call is
    /// declared [`FileAccess::Any`], which conflicts with every other call
    /// that touches files, so it is fully serialized.
    ///
    /// This may touch the file system (path canonicalization); from async code
    /// prefer [`Self::file_access_batch`].
    pub fn file_access(&self, name: &str, arguments: &str, cwd: Option<&Path>) -> FileAccess {
        let Some(tool) = self.tools.get(name) else {
            return FileAccess::None;
        };
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tool.file_access(&args, cwd)
        }))
        .unwrap_or_else(|_| {
            log::warn!("file_access of tool `{name}` panicked; serializing the call");
            FileAccess::Any
        })
    }

    /// [`Self::file_access`] for a whole batch of `(tool name, raw arguments)`
    /// pairs, evaluated on the blocking pool so canonicalization never stalls
    /// the async executor. Results are in input order.
    ///
    /// Panics are contained per call (see [`Self::file_access`]). If the
    /// blocking task itself fails, every call is declared [`FileAccess::Any`],
    /// which the planner serializes against all file-touching calls.
    pub async fn file_access_batch(
        self: &Arc<Self>,
        calls: Vec<(String, String)>,
        cwd: Option<PathBuf>,
    ) -> Vec<FileAccess> {
        let count = calls.len();
        // Shares the registry with the blocking task; no map copy.
        let registry = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            calls
                .iter()
                .map(|(name, arguments)| registry.file_access(name, arguments, cwd.as_deref()))
                .collect()
        })
        .await
        .unwrap_or_else(|error| {
            log::warn!("file_access evaluation failed: {error}; serializing the batch");
            vec![FileAccess::Any; count]
        })
    }

    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use querymt::chat::{FunctionTool, Tool as ChatTool, ToolResultPart};

    use crate::tools::{ToolContext, ToolError};

    struct PanickingTool;

    #[async_trait]
    impl Tool for PanickingTool {
        fn name(&self) -> &str {
            "panics"
        }

        fn definition(&self) -> ChatTool {
            ChatTool {
                tool_type: "function".to_string(),
                function: FunctionTool {
                    name: "panics".to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                    strict: None,
                },
            }
        }

        fn file_access(&self, _args: &serde_json::Value, _cwd: Option<&Path>) -> FileAccess {
            panic!("boom");
        }

        async fn call(
            &self,
            _args: serde_json::Value,
            _context: &dyn ToolContext,
        ) -> Result<Vec<ToolResultPart>, ToolError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn panicking_file_access_marks_only_that_call_as_any() {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(PanickingTool));
        registry.add(Arc::new(crate::tools::builtins::ReadTool::new()));
        let registry = Arc::new(registry);
        let cwd = dunce::canonicalize(std::env::temp_dir()).unwrap();

        let accesses = registry
            .file_access_batch(
                vec![
                    ("read_tool".into(), r#"{"path":"a.md"}"#.into()),
                    ("panics".into(), "{}".into()),
                    ("unregistered".into(), "{}".into()),
                ],
                Some(cwd.clone()),
            )
            .await;

        assert_eq!(
            accesses,
            vec![
                FileAccess::reads([cwd.join("a.md")]),
                FileAccess::Any,
                FileAccess::None,
            ]
        );
    }
}
