//! Tool registry for managing and finding tools

use crate::tools::context::{FileAccess, Tool};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Registry for managing available tools.
///
/// Tools are kept sorted by name (byte order), so `definitions()` and
/// `names()` are deterministic regardless of registration order or process.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
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

    /// Definitions of all tools, ordered by name.
    pub fn definitions(&self) -> Vec<querymt::chat::Tool> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn definition(&self, name: &str) -> Option<querymt::chat::Tool> {
        self.tools.get(name).map(|tool| tool.definition())
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
    /// `arguments` must be the *effective* arguments of a prepared call
    /// (normalised and hook-adjusted); this helper never parses or repairs
    /// raw model output.
    ///
    /// This may touch the file system (path canonicalization); from async code
    /// prefer [`Self::file_access_batch`].
    pub fn file_access(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        cwd: Option<&Path>,
    ) -> FileAccess {
        file_access_of(self.tools.get(name), name, arguments, cwd)
    }

    /// [`Self::file_access`] for a whole batch of `(tool name, effective
    /// arguments)` pairs, evaluated on the blocking pool so canonicalization
    /// never stalls the async executor. Results are in input order.
    ///
    /// Panics are contained per call (see [`Self::file_access`]). If the
    /// blocking task itself fails, every call is declared [`FileAccess::Any`],
    /// which the planner serializes against all file-touching calls.
    pub async fn file_access_batch(
        self: &Arc<Self>,
        calls: Vec<(String, serde_json::Value)>,
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

    /// Names of all tools, ordered by name.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}

/// Declared file access of one call to `tool` (`None` = unknown tool).
/// A panic in the tool's `file_access` is contained and declared
/// [`FileAccess::Any`].
pub(crate) fn file_access_of(
    tool: Option<&Arc<dyn Tool>>,
    name: &str,
    arguments: &serde_json::Value,
    cwd: Option<&Path>,
) -> FileAccess {
    let Some(tool) = tool else {
        return FileAccess::None;
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tool.file_access(arguments, cwd)
    }))
    .unwrap_or_else(|_| {
        log::warn!("file_access of tool `{name}` panicked; serializing the call");
        FileAccess::Any
    })
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

    struct NamedTool(String);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            &self.0
        }

        fn definition(&self) -> ChatTool {
            ChatTool {
                tool_type: "function".to_string(),
                function: FunctionTool {
                    name: self.0.clone(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                    strict: None,
                },
            }
        }

        async fn call(
            &self,
            _args: serde_json::Value,
            _context: &dyn ToolContext,
        ) -> Result<Vec<ToolResultPart>, ToolError> {
            Ok(Vec::new())
        }
    }

    fn registry_of(names: &[&str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for name in names {
            registry.add(Arc::new(NamedTool((*name).to_string())));
        }
        registry
    }

    fn definition_names(registry: &ToolRegistry) -> Vec<String> {
        registry
            .definitions()
            .into_iter()
            .map(|tool| tool.function.name)
            .collect()
    }

    /// Listing is ordered by name (bytes) and does not depend on the order
    /// tools were registered in.
    #[test]
    fn listing_is_ordered_by_name_whatever_the_registration_order() {
        let names = ["write_file", "shell", "Zed", "read_tool", "edit", "glob"];
        let mut sorted = names;
        sorted.sort_unstable();
        let mut reversed = names;
        reversed.reverse();

        for order in [names, reversed] {
            let registry = registry_of(&order);
            assert_eq!(definition_names(&registry), sorted);
            assert_eq!(registry.names(), sorted);
        }
    }

    #[test]
    fn removing_a_tool_keeps_the_order_of_the_rest() {
        let mut registry = registry_of(&["c", "a", "b", "d"]);
        registry.remove("a");
        assert_eq!(registry.names(), ["b", "c", "d"]);
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
                    ("read_tool".into(), serde_json::json!({"path": "a.md"})),
                    ("panics".into(), serde_json::json!({})),
                    ("unregistered".into(), serde_json::json!({})),
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
