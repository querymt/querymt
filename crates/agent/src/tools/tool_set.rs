//! Turn-scoped tool lookup: session tools first, then the agent registry.
//!
//! Session tools (today only the bound `skill` tool) are built per turn from
//! session state. They shadow agent-level tools with the same name.

use crate::tools::ToolExecutionClass;
use crate::tools::context::{FileAccess, Tool};
use crate::tools::registry::{ToolRegistry, file_access_of};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Agent registry plus a turn-scoped registry of session tools.
#[derive(Clone)]
pub struct ToolSet {
    agent: Arc<ToolRegistry>,
    session: Arc<ToolRegistry>,
}

impl ToolSet {
    pub fn new(agent: Arc<ToolRegistry>, session: ToolRegistry) -> Self {
        Self {
            agent,
            session: Arc::new(session),
        }
    }

    /// Find a tool; session tools shadow agent tools.
    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.session.find(name).or_else(|| self.agent.find(name))
    }

    /// Definitions of all tools, one per name (session tools win).
    pub fn definitions(&self) -> Vec<querymt::chat::Tool> {
        let mut defs = self.session.definitions();
        let shadowed = self.session.names();
        defs.extend(
            self.agent
                .names()
                .into_iter()
                .filter(|name| !shadowed.contains(name))
                .filter_map(|name| self.agent.definition(&name)),
        );
        defs
    }

    pub fn definition(&self, name: &str) -> Option<querymt::chat::Tool> {
        self.session
            .definition(name)
            .or_else(|| self.agent.definition(name))
    }

    /// Execution class of `name`; unknown tools are `ParallelSafe`.
    pub fn execution_class(&self, name: &str) -> ToolExecutionClass {
        self.find(name)
            .map(|tool| tool.execution_class())
            .unwrap_or(ToolExecutionClass::ParallelSafe)
    }

    /// Declared file access of one call (see [`ToolRegistry::file_access`]).
    pub fn file_access(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        cwd: Option<&Path>,
    ) -> FileAccess {
        file_access_of(self.find(name).as_ref(), name, arguments, cwd)
    }

    /// Batch variant evaluated on the blocking pool (see
    /// [`ToolRegistry::file_access_batch`]).
    pub async fn file_access_batch(
        &self,
        calls: Vec<(String, serde_json::Value)>,
        cwd: Option<PathBuf>,
    ) -> Vec<FileAccess> {
        let count = calls.len();
        let set = self.clone();
        tokio::task::spawn_blocking(move || {
            calls
                .iter()
                .map(|(name, arguments)| set.file_access(name, arguments, cwd.as_deref()))
                .collect()
        })
        .await
        .unwrap_or_else(|error| {
            log::warn!("file_access evaluation failed: {error}; serializing the batch");
            vec![FileAccess::Any; count]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolContext, ToolError};
    use async_trait::async_trait;
    use querymt::chat::{FunctionTool, ToolResultPart};

    struct Named(&'static str, &'static str);

    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn definition(&self) -> querymt::chat::Tool {
            querymt::chat::Tool {
                tool_type: "function".into(),
                function: FunctionTool {
                    name: self.0.into(),
                    description: self.1.into(),
                    parameters: serde_json::json!({"type": "object"}),
                    strict: None,
                },
            }
        }
        async fn call(
            &self,
            _args: serde_json::Value,
            _ctx: &dyn ToolContext,
        ) -> Result<Vec<ToolResultPart>, ToolError> {
            Ok(vec![])
        }
    }

    #[test]
    fn session_tools_shadow_agent_tools() {
        let mut agent = ToolRegistry::new();
        agent.add(Arc::new(Named("a", "agent-a")));
        agent.add(Arc::new(Named("b", "agent-b")));
        let mut session = ToolRegistry::new();
        session.add(Arc::new(Named("a", "session-a")));
        session.add(Arc::new(Named("c", "session-c")));

        let set = ToolSet::new(Arc::new(agent), session);
        assert_eq!(
            set.definition("a").unwrap().function.description,
            "session-a"
        );
        assert_eq!(set.definition("b").unwrap().function.description, "agent-b");
        assert!(set.find("c").is_some());
        assert!(set.find("zzz").is_none());

        let mut names: Vec<_> = set
            .definitions()
            .into_iter()
            .map(|d| (d.function.name, d.function.description))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ("a".into(), "session-a".into()),
                ("b".into(), "agent-b".into()),
                ("c".into(), "session-c".into())
            ]
        );
    }
}
