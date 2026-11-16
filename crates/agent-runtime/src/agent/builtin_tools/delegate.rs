//! Delegate tool implementation using ToolContext
//!
//! This tool validates delegation parameters and returns immediately.
//! The actual delegation execution is handled asynchronously by the
//! `DelegationOrchestrator` which listens for `DelegationRequested` events.

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool, ToolResultPart};
use serde_json::{Value, json};

/// The `delegate` tool. It holds the roster of the runtime it belongs to, so
/// its definition always lists the current members and a call validates
/// against the same roster. Only `build_root` registers it.
pub struct DelegateTool {
    roster: std::sync::Arc<crate::delegation::roster::AgentRoster>,
}

impl DelegateTool {
    pub const NAME: &'static str = "delegate";

    pub(crate) fn new(roster: std::sync::Arc<crate::delegation::roster::AgentRoster>) -> Self {
        Self { roster }
    }

    /// The member list appended to the tool description, in ID order.
    fn member_listing(snapshot: &crate::delegation::roster::RosterSnapshot) -> String {
        use std::fmt::Write;

        let mut out = String::from("\n\nAvailable agents:\n");
        for info in snapshot.infos() {
            let _ = write!(out, "- `{}` ({})", info.id, info.name);
            if !info.description.is_empty() {
                let _ = write!(out, ": {}", info.description);
            }
            if !info.capabilities.is_empty() {
                let _ = write!(out, " [capabilities: {}]", info.capabilities.join(", "));
            }
            let _ = writeln!(out, " [default isolation: {}]", info.isolation);
        }
        out
    }
}

#[async_trait]
impl ToolTrait for DelegateTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn is_read_only(&self) -> bool {
        // Touches session or knowledge state, never the workspace files.
        true
    }

    fn definition(&self) -> Tool {
        let snapshot = self.roster.snapshot();
        let mut description = "Delegate a task to another specialized agent.".to_string();
        let mut target = json!({
            "type": "string",
            "description": "The ID of the agent to delegate to."
        });
        if !snapshot.is_empty() {
            description.push_str(&Self::member_listing(&snapshot));
            target["enum"] = Value::Array(
                snapshot
                    .iter()
                    .map(|member| Value::String(member.id().to_string()))
                    .collect(),
            );
        }
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: Self::NAME.to_string(),
                description,
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "target_agent_id": target,
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
                            "description": "Execution workspace for this delegate. Precedence: an explicit 'isolated'/'shared' here wins; otherwise the target agent's configured isolation applies (listed per agent under Available Agents); otherwise it follows the parent session (isolated parents isolate every delegate, local parents share their checkout). 'inherit' (default) means 'use the configured/parent policy'. 'isolated' is a private Git worktree seeded from the parent's current code state; its result is integrated with accept_delegation_result, never by delegating a patch/merge task. 'shared' shares the parent's checkout and is disclosed as shared execution. The resolved policy is reported in the tool result."
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

        // Validate the target exists and check capability requirements.
        let snapshot = self.roster.snapshot();
        let target_info = snapshot
            .get(target_id)
            .map(|member| member.info())
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
    use crate::delegation::roster::{MemberOrigin, test_member, test_roster};
    use std::path::Path;

    struct TestContext;

    #[async_trait]
    impl ToolContext for TestContext {
        fn session_id(&self) -> &str {
            "sess-test"
        }

        fn cwd(&self) -> Option<&Path> {
            None
        }

        async fn record_progress(
            &self,
            _kind: &str,
            _content: String,
            _metadata: Option<serde_json::Value>,
        ) -> Result<String, ToolError> {
            Ok(String::new())
        }

        fn as_any(&self) -> &dyn std::any::Any {
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

    async fn tool_with(ids: &[&str]) -> (DelegateTool, tempfile::TempDir) {
        let (roster, dir) = test_roster().await;
        for id in ids {
            roster.add(test_member(id, MemberOrigin::Config)).unwrap();
        }
        (DelegateTool::new(roster), dir)
    }

    #[tokio::test]
    async fn unknown_isolation_value_is_rejected() {
        let (tool, _dir) = tool_with(&[]).await;
        let err = tool
            .call(args_json(Some("sandboxed")), &TestContext)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("inherit, isolated, shared"),
            "error: {err}"
        );
    }

    #[tokio::test]
    async fn known_isolation_values_pass_validation() {
        let (tool, _dir) = tool_with(&["coder"]).await;
        for isolation in ["inherit", "isolated", "shared"] {
            let result = tool
                .call(args_json(Some(isolation)), &TestContext)
                .await
                .unwrap_or_else(|err| panic!("isolation '{isolation}' should parse: {err}"));
            assert!(!result.is_empty());
        }
        tool.call(args_json(None), &TestContext).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_agent_is_rejected() {
        let (tool, _dir) = tool_with(&["writer"]).await;
        let err = tool.call(args_json(None), &TestContext).await.unwrap_err();
        assert!(err.to_string().contains("Unknown agent: 'coder'"), "{err}");
    }

    #[tokio::test]
    async fn schema_exposes_isolation_enum() {
        let (tool, _dir) = tool_with(&[]).await;
        let parameters = tool.definition().function.parameters;
        assert_eq!(
            parameters["properties"]["isolation"]["enum"],
            serde_json::json!(["inherit", "isolated", "shared"])
        );
        assert!(parameters["properties"]["target_agent_id"]["enum"].is_null());
    }

    #[tokio::test]
    async fn delegate_tool_lists_members_ordered_by_id() {
        let (tool, _dir) = tool_with(&["writer", "coder"]).await;
        let definition = tool.definition().function;
        assert_eq!(
            definition.parameters["properties"]["target_agent_id"]["enum"],
            serde_json::json!(["coder", "writer"])
        );
        let coder = definition
            .description
            .find("`coder`")
            .expect("coder listed");
        let writer = definition
            .description
            .find("`writer`")
            .expect("writer listed");
        assert!(coder < writer, "{}", definition.description);
        assert!(definition.description.contains("coder description"));
        assert!(
            definition
                .description
                .contains("default isolation: inherit")
        );
    }

    #[tokio::test]
    async fn unchanged_roster_keeps_the_tool_definition_unchanged() {
        let (roster, _dir) = test_roster().await;
        roster
            .add(test_member("coder", MemberOrigin::Config))
            .unwrap();
        let tool = DelegateTool::new(roster.clone());
        let first = serde_json::to_string(&tool.definition()).unwrap();
        let second = serde_json::to_string(&tool.definition()).unwrap();
        assert_eq!(first, second);
        roster
            .add(test_member("reviewer", MemberOrigin::Runtime))
            .unwrap();
        let third = serde_json::to_string(&tool.definition()).unwrap();
        assert_ne!(first, third);
        assert!(third.contains("reviewer"));
    }
}
