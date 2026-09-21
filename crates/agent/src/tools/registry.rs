//! Tool registry for managing and finding tools

use crate::tools::context::Tool;
use std::collections::HashMap;
use std::path::Path;
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

    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}
