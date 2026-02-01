//! Tool management and permission handling

use crate::agent::core::{QueryMTAgent, SessionRuntime};
use crate::agent::execution_context::ExecutionContext;
use crate::agent::utils::{extract_locations, tool_kind_for_tool};
use agent_client_protocol::{
    Error, PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use std::sync::Arc;
use uuid::Uuid;

impl QueryMTAgent {
    /// Collects available tools based on current configuration.
    pub(crate) fn collect_tools(
        &self,
        provider: Arc<dyn querymt::LLMProvider>,
        runtime: Option<&SessionRuntime>,
    ) -> Vec<querymt::chat::Tool> {
        let mut tools = Vec::new();
        let config = self.tool_config_snapshot();

        match config.policy {
            crate::agent::core::ToolPolicy::BuiltInOnly => {
                if let Ok(registry) = self.tool_registry.lock() {
                    tools.extend(registry.definitions());
                }
            }
            crate::agent::core::ToolPolicy::ProviderOnly => {
                if let Some(provider_tools) = provider.tools() {
                    tools.extend(provider_tools.iter().cloned::<querymt::chat::Tool>());
                }
            }
            crate::agent::core::ToolPolicy::BuiltInAndProvider => {
                if let Ok(registry) = self.tool_registry.lock() {
                    tools.extend(registry.definitions());
                }
                if let Some(provider_tools) = provider.tools() {
                    tools.extend(provider_tools.iter().cloned::<querymt::chat::Tool>());
                }
            }
        }

        if let (
            Some(runtime),
            crate::agent::core::ToolPolicy::ProviderOnly
            | crate::agent::core::ToolPolicy::BuiltInAndProvider,
        ) = (runtime, config.policy)
        {
            tools.extend(runtime.mcp_tool_defs.iter().cloned());
        }

        tools
            .into_iter()
            .filter(|tool| is_tool_allowed_with(&config, &tool.function.name))
            .collect()
    }

    /// Checks if a tool is allowed by current configuration.
    pub(crate) fn is_tool_allowed(&self, name: &str) -> bool {
        let config = self.tool_config_snapshot();
        is_tool_allowed_with(&config, name)
    }

    /// Ensures tool permission is granted before execution.
    pub(crate) async fn ensure_tool_permission(
        &self,
        exec_ctx: &ExecutionContext,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<bool, Error> {
        let requires_permission = self.requires_permission_for_tool(tool_name);
        if !requires_permission {
            return Ok(true);
        }

        // Phase 5: Intent-based check
        if let Some(task) = &exec_ctx.state.active_task
            && task.status != crate::session::domain::TaskStatus::Active
        {
            return Ok(false);
        }

        if let Ok(cache) = exec_ctx.runtime.permission_cache.lock()
            && let Some(cached) = cache.get(tool_name)
        {
            return Ok(*cached);
        }

        let permission_id = Uuid::new_v4().to_string();
        self.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::PermissionRequested {
                permission_id: permission_id.clone(),
                task_id: exec_ctx
                    .state
                    .active_task
                    .as_ref()
                    .map(|task| task.public_id.clone()),
                tool_name: tool_name.to_string(),
                reason: format!("Tool {} requires explicit permission", tool_name),
            },
        );

        // Try bridge first (ACP stdio mode), then fall back to client (WebSocket mode)
        let bridge = self.bridge();
        let client = self.client.lock().ok().and_then(|c| c.clone());

        // If neither bridge nor client is available, auto-grant permission
        if bridge.is_none() && client.is_none() {
            self.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::PermissionGranted {
                    permission_id,
                    granted: true,
                },
            );
            return Ok(true);
        }

        let locations = extract_locations(args);
        let tool_update_fields = ToolCallUpdateFields::new()
            .title(format!("Run {}", tool_name))
            .kind(tool_kind_for_tool(tool_name))
            .status(ToolCallStatus::Pending)
            .locations(if locations.is_empty() {
                None
            } else {
                Some(locations)
            })
            .raw_input(args.clone());

        let request = RequestPermissionRequest::new(
            exec_ctx.session_id.clone(),
            ToolCallUpdate::new(
                ToolCallId::from(tool_call_id.to_string()),
                tool_update_fields,
            ),
            vec![
                PermissionOption::new(
                    PermissionOptionId::from("allow_once"),
                    "Allow once",
                    PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new(
                    PermissionOptionId::from("allow_always"),
                    "Always allow",
                    PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new(
                    PermissionOptionId::from("reject_once"),
                    "Reject once",
                    PermissionOptionKind::RejectOnce,
                ),
                PermissionOption::new(
                    PermissionOptionId::from("reject_always"),
                    "Always reject",
                    PermissionOptionKind::RejectAlways,
                ),
            ],
        );

        // Use bridge if available (Send future, no LocalSet required)
        let response = if let Some(bridge) = bridge {
            bridge.request_permission(request).await?
        } else if let Some(client) = client {
            // Fall back to client (requires LocalSet)
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::task::spawn_local(async move {
                let result = client.request_permission(request).await;
                let _ = tx.send(result);
            });
            rx.await
                .map_err(|_| Error::new(-32000, "Permission request cancelled"))??
        } else {
            unreachable!("Already checked for bridge/client availability")
        };
        let granted = match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                let option_id = selected.option_id.0.as_ref();
                let allow = option_id == "allow_once" || option_id == "allow_always";
                if let Ok(mut cache) = exec_ctx.runtime.permission_cache.lock() {
                    if option_id == "allow_always" {
                        cache.insert(tool_name.to_string(), true);
                    } else if option_id == "reject_always" {
                        cache.insert(tool_name.to_string(), false);
                    }
                }
                allow
            }
            _ => false,
        };

        self.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::PermissionGranted {
                permission_id,
                granted,
            },
        );

        Ok(granted)
    }
}

/// Checks if a tool is allowed based on configuration.
pub(crate) fn is_tool_allowed_with(config: &ToolConfig, name: &str) -> bool {
    if config.denylist.contains(name) {
        return false;
    }
    match &config.allowlist {
        Some(allowlist) => allowlist.contains(name),
        None => true,
    }
}

use crate::agent::core::ToolConfig;
use crate::events::AgentEventKind;
