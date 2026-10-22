//! Delegate tool implementation using ToolContext
//!
//! This tool validates delegation parameters and returns immediately.
//! The actual delegation execution is handled asynchronously by the
//! `DelegationOrchestrator` which listens for `DelegationRequested` events.

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool, ToolResultPart};
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
                        },
                        "isolation": {
                            "type": "string",
                            "enum": ["inherit", "isolated", "shared"],
                            "description": "Execution workspace for this delegate. 'inherit' (default) resolves from the parent session: isolated parents isolate every delegate by default, local parents share their checkout. 'isolated' explicitly requests a private Git worktree checkout seeded from the parent's current code state. 'shared' explicitly shares the parent's checkout and is disclosed as shared execution."
                        }
                    },
                    "required": ["target_agent_id", "objective"]
                }),
                strict: None,
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        // Extract and validate arguments
        let target_id = args["target_agent_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing target_agent_id".into()))?;

        let objective = args["objective"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing objective".into()))?;

        // Fail fast on unknown isolation values so invalid requests never
        // reach the orchestrator with a silently misresolved policy.
        if let Some(raw) = args.get("isolation") {
            let value = raw
                .as_str()
                .ok_or_else(|| ToolError::InvalidRequest("isolation must be a string".into()))?;
            if value
                .parse::<crate::worktree::DelegationIsolation>()
                .is_err()
            {
                return Err(ToolError::InvalidRequest(format!(
                    "Unknown isolation '{}': expected one of inherit, isolated, shared",
                    value
                )));
            }
        }

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
        Ok(vec![ToolResultPart::text(format!(
            "Delegation to agent '{}' has been queued.\n\
             Objective: {}\n\n\
             The task will be executed asynchronously. \
             You will receive results when the delegation completes.",
            target_id, objective
        ))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::{AgentInfo, DefaultAgentRegistry};
    use async_trait::async_trait;
    use std::any::Any;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    struct StubAgentHandle {
        event_fanout: Arc<crate::event_fanout::EventFanout>,
    }

    impl StubAgentHandle {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                event_fanout: Arc::new(crate::event_fanout::EventFanout::new()),
            })
        }
    }

    #[async_trait]
    impl crate::agent::handle::AgentHandle for StubAgentHandle {
        async fn new_session(
            &self,
            _req: crate::acp::protocol::NewSessionRequest,
        ) -> Result<crate::acp::protocol::NewSessionResponse, crate::acp::protocol::Error> {
            Ok(crate::acp::protocol::NewSessionResponse::new(
                "stub-session".to_string(),
            ))
        }

        async fn prompt(
            &self,
            _req: crate::acp::protocol::PromptRequest,
        ) -> Result<crate::acp::protocol::PromptResponse, crate::acp::protocol::Error> {
            Ok(crate::acp::protocol::PromptResponse::new(
                crate::acp::protocol::StopReason::EndTurn,
            ))
        }

        async fn cancel(
            &self,
            _notif: crate::acp::protocol::CancelNotification,
        ) -> Result<(), crate::acp::protocol::Error> {
            Ok(())
        }

        async fn load_session(
            &self,
            _req: crate::acp::protocol::LoadSessionRequest,
        ) -> Result<crate::acp::protocol::LoadSessionResponse, crate::acp::protocol::Error>
        {
            Ok(crate::acp::protocol::LoadSessionResponse::new())
        }

        async fn create_delegation_session(
            &self,
            _cwd: Option<String>,
            _parent_session_id: String,
        ) -> Result<(String, crate::agent::remote::SessionActorRef), crate::acp::protocol::Error>
        {
            Err(crate::acp::protocol::Error::internal_error())
        }

        fn subscribe_events(
            &self,
        ) -> tokio::sync::broadcast::Receiver<crate::events::EventEnvelope> {
            self.event_fanout.subscribe()
        }

        fn event_fanout(&self) -> &Arc<crate::event_fanout::EventFanout> {
            &self.event_fanout
        }

        fn emit_event(&self, _session_id: &str, _kind: crate::events::AgentEventKind) {}

        fn agent_registry(&self) -> Arc<dyn crate::delegation::AgentRegistry + Send + Sync> {
            Arc::new(DefaultAgentRegistry::new())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct TestContext {
        registry: Option<Arc<dyn crate::delegation::AgentRegistry>>,
    }

    impl TestContext {
        fn without_registry() -> Self {
            Self { registry: None }
        }

        fn with_registry() -> Self {
            let mut registry = DefaultAgentRegistry::new();
            registry.register(
                AgentInfo {
                    isolation: Default::default(),
                    id: "coder".into(),
                    name: "coder".into(),
                    description: String::new(),
                    capabilities: vec![],
                    required_capabilities: vec![],
                    meta: None,
                },
                StubAgentHandle::new(),
            );
            Self {
                registry: Some(Arc::new(registry)),
            }
        }
    }

    #[async_trait]
    impl ToolContext for TestContext {
        fn session_id(&self) -> &str {
            "sess-test"
        }

        fn cwd(&self) -> Option<&Path> {
            None
        }

        fn agent_registry(&self) -> Option<Arc<dyn crate::delegation::AgentRegistry>> {
            self.registry.clone()
        }

        async fn record_progress(
            &self,
            _kind: &str,
            _content: String,
            _metadata: Option<serde_json::Value>,
        ) -> Result<String, ToolError> {
            Ok(String::new())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn args_json(isolation: Option<&str>) -> Value {
        let mut value = serde_json::json!({
            "target_agent_id": "coder",
            "objective": "do the thing",
        });
        if let Some(isolation) = isolation {
            value["isolation"] = Value::String(isolation.to_string());
        }
        value
    }

    #[tokio::test]
    async fn unknown_isolation_value_is_rejected() {
        let tool = DelegateTool::new();
        let err = tool
            .call(
                args_json(Some("sandboxed")),
                &TestContext::without_registry(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("inherit, isolated, shared"),
            "error: {err}"
        );
    }

    #[tokio::test]
    async fn known_isolation_values_pass_validation() {
        let tool = DelegateTool::new();
        let context = TestContext::with_registry();
        for isolation in ["inherit", "isolated", "shared"] {
            let result = tool
                .call(args_json(Some(isolation)), &context)
                .await
                .unwrap_or_else(|err| panic!("isolation '{isolation}' should parse: {err}"));
            assert!(!result.is_empty());
        }
        // Absent isolation stays valid (inherit default).
        tool.call(args_json(None), &context).await.unwrap();
    }

    #[test]
    fn schema_exposes_isolation_enum() {
        let definition = DelegateTool::new().definition();
        let parameters = definition.function.parameters;
        assert_eq!(
            parameters["properties"]["isolation"]["enum"],
            serde_json::json!(["inherit", "isolated", "shared"])
        );
        let _ = PathBuf::new();
    }
}
