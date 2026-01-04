//! Delegate tool implementation using ToolContext
//!
//! This tool validates delegation parameters and returns immediately.
//! The actual delegation execution is handled asynchronously by the
//! `DelegationOrchestrator` which listens for `DelegationRequested` events.

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

pub struct DelegateTool;

impl Default for DelegateTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DelegateTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn definition(&self) -> Tool {
        // Note: In the new architecture, we might not have access to the registry
        // at definition time to populate the enum. For now, we'll keep it simple.
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "delegate".to_string(),
                description: "Delegate a task to another specialized agent.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "target_agent_id": {
                            "type": "string",
                            "description": "The ID of the agent to delegate to."
                        },
                        "objective": {
                            "type": "string",
                            "description": "The goal of the delegation."
                        },
                        "context": {
                            "type": "string",
                            "description": "Additional context for the task."
                        },
                        "constraints": {
                            "type": "string",
                            "description": "Constraints or boundaries for the task."
                        },
                        "expected_output": {
                            "type": "string",
                            "description": "What the delegated agent is expected to produce."
                        }
                    },
                    "required": ["target_agent_id", "objective"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        // Extract and validate arguments
        let target_id = args["target_agent_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing target_agent_id".into()))?;

        let objective = args["objective"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing objective".into()))?;

        // Get agent registry from context to validate target agent exists
        let registry = context.agent_registry().ok_or_else(|| {
            ToolError::ProviderError("No agent registry available in context".into())
        })?;

        // Validate target agent exists and check capability requirements
        let target_info = registry
            .get_agent(target_id)
            .ok_or_else(|| ToolError::InvalidRequest(format!("Unknown agent: '{}'.", target_id)))?;

        if target_info
            .required_capabilities
            .contains(&CapabilityRequirement::Filesystem)
            && context.cwd().is_none()
        {
            return Err(ToolError::InvalidRequest(format!(
                "Cannot delegate to '{}': agent requires filesystem access but no working directory is set",
                target_id
            )));
        }

        // Return immediately - the actual delegation will be handled asynchronously
        // by the DelegationOrchestrator when it receives the DelegationRequested event
        // (emitted by the agent after this tool call completes)
        Ok(format!(
            "Delegation to agent '{}' has been queued.\n\
             Objective: {}\n\n\
             The task will be executed asynchronously. \
             You will receive results when the delegation completes.",
            target_id, objective
        ))
    }
}
