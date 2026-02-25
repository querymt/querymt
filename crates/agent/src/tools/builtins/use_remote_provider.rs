//! Tool: `use_remote_provider` — route a delegate's LLM provider to a named peer.
//!
//! Sends `SetProviderTarget` to the `RoutingActor`, returning confirmation with
//! the current routing state so the planner gets feedback.

use crate::agent::remote::routing::{RouteTarget, RoutingActor, SetProviderTarget};
use crate::tools::{Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use kameo::actor::ActorRef;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

/// Planner tool that routes a delegate's LLM provider to a named peer (or back to local).
pub struct UseRemoteProviderTool {
    actor: ActorRef<RoutingActor>,
}

impl UseRemoteProviderTool {
    /// Create a new tool backed by the given `RoutingActor`.
    pub fn new(actor: ActorRef<RoutingActor>) -> Self {
        Self { actor }
    }
}

#[async_trait]
impl ToolTrait for UseRemoteProviderTool {
    fn name(&self) -> &str {
        "use_remote_provider"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Route a delegate agent's LLM provider calls to a named mesh peer, \
                              or reset to local. The session still runs locally but LLM inference \
                              happens on the remote peer (e.g., a GPU node)."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "The delegate agent ID to route."
                        },
                        "peer_name": {
                            "type": ["string", "null"],
                            "description": "Peer hostname to use for LLM, or null to reset to local."
                        }
                    },
                    "required": ["agent_id"]
                }),
            },
        }
    }

    async fn call(&self, args: Value, _context: &dyn ToolContext) -> Result<String, ToolError> {
        let agent_id = args
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("agent_id is required".to_string()))?
            .to_string();

        let target = match args.get("peer_name").and_then(Value::as_str) {
            Some(name) => RouteTarget::Peer(name.to_string()),
            None => RouteTarget::Local,
        };

        let confirmation = self
            .actor
            .ask(SetProviderTarget {
                agent_id: agent_id.clone(),
                target: target.clone(),
            })
            .await
            .map_err(|e| ToolError::Other(anyhow::anyhow!("RoutingActor error: {:?}", e)))?;

        let status = match &target {
            RouteTarget::Local => format!("Provider for '{}' now uses local LLM.", agent_id),
            RouteTarget::Peer(name) => {
                format!(
                    "Provider for '{}' now routes LLM calls to peer '{}'.",
                    agent_id, name
                )
            }
        };

        let result = json!({
            "status": status,
            "agent_id": confirmation.agent_id,
            "policy": confirmation.policy.as_ref().map(|p| json!({
                "session_target": format!("{:?}", p.session_target),
                "provider_target": format!("{:?}", p.provider_target),
                "resolved_provider_node_id": p.resolved_provider_node_id,
            })),
        });

        Ok(serde_json::to_string_pretty(&result).unwrap_or(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::remote::routing::{RoutingActor, new_routing_snapshot_handle};
    use crate::tools::AgentToolContext;
    use kameo::actor::Spawn;

    #[tokio::test]
    async fn test_use_remote_provider_sets_provider_target() {
        let snapshot = new_routing_snapshot_handle();
        let actor = RoutingActor::spawn(RoutingActor::new(snapshot.clone()));
        let tool = UseRemoteProviderTool::new(actor);
        let ctx = AgentToolContext::basic("test-session".to_string(), None);

        let result = tool
            .call(json!({ "agent_id": "coder", "peer_name": "gpu-box" }), &ctx)
            .await
            .expect("call should succeed");

        assert!(result.contains("gpu-box"), "should mention the peer name");

        // Verify snapshot
        let snap = snapshot.load();
        let policy = snap.get("coder").expect("policy should exist");
        assert_eq!(policy.provider_target, RouteTarget::Peer("gpu-box".into()));
        // session_target should remain Local (default)
        assert_eq!(policy.session_target, RouteTarget::Local);
    }

    #[tokio::test]
    async fn test_use_remote_provider_resets_to_local() {
        let snapshot = new_routing_snapshot_handle();
        let actor = RoutingActor::spawn(RoutingActor::new(snapshot.clone()));
        let tool = UseRemoteProviderTool::new(actor);
        let ctx = AgentToolContext::basic("test-session".to_string(), None);

        // First route to peer
        tool.call(json!({ "agent_id": "coder", "peer_name": "gpu-box" }), &ctx)
            .await
            .unwrap();

        // Then reset to local
        let result = tool
            .call(json!({ "agent_id": "coder", "peer_name": null }), &ctx)
            .await
            .expect("call should succeed");

        assert!(result.contains("local"), "should mention local");

        let snap = snapshot.load();
        let policy = snap.get("coder").expect("policy should exist");
        assert_eq!(policy.provider_target, RouteTarget::Local);
    }

    #[tokio::test]
    async fn test_use_remote_provider_missing_agent_id() {
        let snapshot = new_routing_snapshot_handle();
        let actor = RoutingActor::spawn(RoutingActor::new(snapshot));
        let tool = UseRemoteProviderTool::new(actor);
        let ctx = AgentToolContext::basic("test-session".to_string(), None);

        let err = tool
            .call(json!({ "peer_name": "gpu-box" }), &ctx)
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidRequest(msg) => {
                assert!(msg.contains("agent_id"), "error should mention agent_id");
            }
            _ => panic!("expected InvalidRequest error"),
        }
    }
}
