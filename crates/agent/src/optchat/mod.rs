//! Experimental, removable session-local OptChat memory.
//!
//! Integration is deliberately limited to configuration and AgentBuilder. The
//! normal transcript/schema, execution state machine and provider APIs are unchanged.
mod config;
mod middleware;
mod runtime;
mod source;
mod store;
mod tools;

use anyhow::{Result, ensure};
pub use config::{CompactorSettings, MemorySettings};
use querymt::chat::{ChatOutput, ChatOutputStatus, FinishReason};
use std::sync::Arc;

/// Install the experiment after normal agent assembly. Disabled is a strict no-op.
pub(crate) fn install(
    config: &mut crate::agent::AgentConfig,
    mut settings: MemorySettings,
) -> Result<()> {
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    if settings.directory.is_none() {
        let home = std::env::var_os("QMT_CONFIG_DIR")
            .filter(|path| !path.is_empty())
            .or_else(|| std::env::var_os("QMT_HOME").filter(|path| !path.is_empty()))
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".qmt")))
            .ok_or_else(|| anyhow::anyhow!("set agent.memory.directory explicitly"))?;
        settings.directory = Some(home.join("experimental-optchat"));
    }
    for name in ["zoom", "date"] {
        ensure!(
            config.tool_registry.find(name).is_none(),
            "OptChat tool name conflicts with existing {name}"
        );
        ensure!(
            !config.tool_config.denylist.contains(name),
            "OptChat requires the {name} tool; remove it from the denylist"
        );
    }
    ensure!(
        !matches!(
            config.tool_config.policy,
            crate::agent::core::ToolPolicy::ProviderOnly
        ),
        "OptChat requires a built-in tool policy; provider-only mode is not supported"
    );
    let runtime = Arc::new(runtime::Runtime::new(settings, config.provider.clone()));
    let mut registry = (*config.tool_registry).clone();
    registry.add(Arc::new(tools::MemoryTool {
        runtime: runtime.clone(),
        date: false,
    }));
    registry.add(Arc::new(tools::MemoryTool {
        runtime: runtime.clone(),
        date: true,
    }));
    config.tool_registry = Arc::new(registry);
    if let Some(allowlist) = &mut config.tool_config.allowlist {
        allowlist.extend(["zoom".into(), "date".into()]);
    }
    config.execution_policy.pruning.enabled = false;
    config.execution_policy.compaction.auto = false;
    // The experiment owns context limits. Do not stack two competing history policies.
    config
        .middleware_drivers
        .retain(|driver| driver.name() != "ContextMiddleware");
    config
        .middleware_drivers
        .insert(0, Arc::new(middleware::MemoryMiddleware(runtime)));
    log::warn!(
        "Experimental OptChat enabled: session-local sidecar memory, extra summary requests; conventional pruning/compaction disabled"
    );
    Ok(())
}

fn summary_text(output: &ChatOutput) -> Result<String> {
    ensure!(
        !output
            .status
            .is_some_and(|status| status != ChatOutputStatus::Completed)
            && !matches!(
                output.finish_reason,
                Some(
                    FinishReason::Length
                        | FinishReason::ContentFilter
                        | FinishReason::Error
                        | FinishReason::ToolCalls
                )
            )
            && output.tool_calls().is_none_or(|calls| calls.is_empty()),
        "OptChat summary did not complete successfully"
    );
    let text = output.text().unwrap_or_default();
    ensure!(!text.trim().is_empty(), "OptChat summary is empty");
    Ok(text)
}

#[cfg(test)]
mod tests;
