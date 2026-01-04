use async_trait::async_trait;
use querymt::chat::Tool;
use querymt::error::LLMError;
use serde_json::Value;
use std::sync::Arc;

#[async_trait(?Send)]
pub trait BuiltInTool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> Tool;
    async fn call(&self, args: Value) -> Result<String, LLMError>;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn BuiltInTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn add(&mut self, tool: Arc<dyn BuiltInTool>) -> &mut Self {
        self.tools.push(tool);
        self
    }

    pub fn definitions(&self) -> Vec<Tool> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn BuiltInTool>> {
        self.tools.iter().find(|tool| tool.name() == name).cloned()
    }
}
