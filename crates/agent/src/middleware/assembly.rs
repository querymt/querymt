//! Shared middleware assembly for single agents and quorum members.
//!
//! Both build paths turn `[[middleware]]` entries into drivers through
//! [`build_from_entries`], so an invalid entry fails identically everywhere and
//! "turned off" is the factory's `Ok(None)`, never an error message.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::agent::agent_config::AgentConfig;
use crate::config::MiddlewareEntry;
use crate::middleware::{
    ContextConfig, ContextMiddleware, MIDDLEWARE_REGISTRY, MiddlewareDriver,
    OutputLimitRecoveryMiddleware, ParallelEditGuard, ParallelEditMode,
};

/// Driver names that block the matching auto-added default.
const CONTEXT_DRIVER: &str = "ContextMiddleware";
const RECOVERY_DRIVER: &str = "OutputLimitRecoveryMiddleware";
const GUARD_DRIVER: &str = "ParallelEditGuard";

fn has_entry(entries: &[MiddlewareEntry], type_name: &str) -> bool {
    entries.iter().any(|e| e.middleware_type == type_name)
}

fn has_driver(drivers: &[Arc<dyn MiddlewareDriver>], name: &str) -> bool {
    drivers.iter().any(|d| d.name() == name)
}

/// Builds the drivers for `entries` plus the auto-added defaults.
///
/// `existing` are drivers the caller already installed (programmatic
/// factories); they only influence which defaults are added. `config` is the
/// agent config built so far; factories read the compaction policy, event sink
/// and tool registry from it. The returned drivers are in installation order:
/// entries first, then context, output-limit recovery and the parallel edit
/// guard (the guard goes last so its note is the final message of a request).
///
/// A default is skipped when the caller supplied a driver of that kind or any
/// entry of that type exists, including one turned off by its own config.
///
/// # Errors
///
/// Fails on the first entry with an unknown type or invalid configuration,
/// naming the middleware type.
pub(crate) fn build_from_entries(
    entries: &[MiddlewareEntry],
    existing: &[Arc<dyn MiddlewareDriver>],
    config: &AgentConfig,
) -> Result<Vec<Arc<dyn MiddlewareDriver>>> {
    let mut built = Vec::with_capacity(entries.len() + 3);

    for entry in entries {
        let driver = MIDDLEWARE_REGISTRY
            .create(&entry.middleware_type, &entry.config, config)
            .with_context(|| format!("middleware '{}'", entry.middleware_type))?;
        built.extend(driver);
    }

    let present = |built: &[Arc<dyn MiddlewareDriver>], name: &str| {
        has_driver(existing, name) || has_driver(built, name)
    };

    if config.execution_policy.compaction.auto
        && !has_entry(entries, "context")
        && !present(&built, CONTEXT_DRIVER)
    {
        log::info!("Auto-enabling ContextMiddleware for compaction");
        built.push(Arc::new(ContextMiddleware::new(
            ContextConfig::default().auto_compact(true),
        )));
    }

    if !has_entry(entries, "output_limit_recovery") && !present(&built, RECOVERY_DRIVER) {
        built.push(Arc::new(OutputLimitRecoveryMiddleware::default()));
    }

    if !has_entry(entries, "parallel_edit_guard") && !present(&built, GUARD_DRIVER) {
        built.push(Arc::new(ParallelEditGuard::new(
            config.tool_registry.clone(),
            ParallelEditMode::Warn,
        )));
    }

    Ok(built)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(kind: &str, config: serde_json::Value) -> MiddlewareEntry {
        MiddlewareEntry {
            middleware_type: kind.into(),
            config,
        }
    }

    async fn config() -> (AgentConfig, tempfile::TempDir) {
        use crate::agent::agent_config_builder::AgentConfigBuilder;
        use crate::session::sqlite_storage::SqliteStorage;
        use crate::test_utils::helpers::empty_plugin_registry;
        let (registry, dir) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = querymt::LLMParams::new().provider("mock").model("mock");
        (
            AgentConfigBuilder::new(Arc::new(registry), storage, llm).build(),
            dir,
        )
    }

    fn names(drivers: &[Arc<dyn MiddlewareDriver>]) -> Vec<&'static str> {
        drivers.iter().map(|d| d.name()).collect()
    }

    #[tokio::test]
    async fn defaults_are_added_without_entries() {
        let (config, _dir) = config().await;
        let built = build_from_entries(&[], &[], &config).unwrap();
        assert_eq!(
            names(&built),
            [CONTEXT_DRIVER, RECOVERY_DRIVER, GUARD_DRIVER],
            "compaction is auto by default, so context is added first"
        );
    }

    #[tokio::test]
    async fn turned_off_entry_is_skipped_and_suppresses_its_default() {
        let (config, _dir) = config().await;
        let entries = [
            entry("context", json!({"enabled": false})),
            entry("output_limit_recovery", json!({"enabled": false})),
            entry("parallel_edit_guard", json!({"enabled": false})),
        ];
        let built = build_from_entries(&entries, &[], &config).unwrap();
        assert!(built.is_empty(), "{:?}", names(&built));
    }

    #[tokio::test]
    async fn existing_driver_suppresses_the_matching_default() {
        let (config, _dir) = config().await;
        let existing: Vec<Arc<dyn MiddlewareDriver>> =
            vec![Arc::new(OutputLimitRecoveryMiddleware::default())];
        let built = build_from_entries(&[], &existing, &config).unwrap();
        assert_eq!(names(&built), [CONTEXT_DRIVER, GUARD_DRIVER]);
    }

    #[tokio::test]
    async fn invalid_entry_error_names_the_middleware() {
        let (config, _dir) = config().await;
        let err = build_from_entries(
            &[entry(
                "limits",
                json!({"cache_write_cost_per_million": -2.0}),
            )],
            &[],
            &config,
        )
        .err()
        .expect("invalid entry must fail");
        let text = format!("{err:#}");
        assert!(text.contains("middleware 'limits'"), "{text}");
        assert!(text.contains("cache_write_cost_per_million"), "{text}");
    }

    #[tokio::test]
    async fn unknown_type_fails() {
        let (config, _dir) = config().await;
        let err = build_from_entries(&[entry("nope", json!({}))], &[], &config)
            .err()
            .expect("unknown type must fail");
        assert!(format!("{err:#}").contains("unknown middleware type: nope"));
    }
}
