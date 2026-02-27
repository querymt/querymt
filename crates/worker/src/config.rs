//! Minimal `AgentConfig` construction for sandboxed worker processes.
//!
//! The worker needs just enough infrastructure to run a `SessionActor`:
//! - A `SessionProvider` backed by the shared SQLite database
//! - A `ToolRegistry` with all built-in tools
//! - An `EventSink` (logging/no-op; events flow via mesh)
//!
//! Everything else (auth, delegation, middleware, MCP servers, snapshots)
//! is left at defaults or disabled.

use querymt::LLMParams;
use querymt_agent::agent::agent_config::AgentConfig;
use querymt_agent::agent::agent_config_builder::AgentConfigBuilder;
use querymt_agent::session::backend::StorageBackend;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use std::path::Path;
use std::sync::Arc;

/// Build a minimal `AgentConfig` for a sandboxed worker process.
///
/// Opens the shared SQLite database at `db_path` (without running migrations,
/// since the orchestrator already owns the schema). Registers all built-in
/// tools and returns a config suitable for `SessionActor::new()`.
pub async fn build_worker_config(db_path: &Path) -> anyhow::Result<Arc<AgentConfig>> {
    // Open the shared SQLite database.
    // Use connect_with_options(migrate=false) since the orchestrator
    // already manages migrations. The worker only needs read + message writes.
    let storage = SqliteStorage::connect_with_options(db_path.to_path_buf(), false)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open database: {}", e))?;

    let store = storage.session_store();

    // Use an in-memory EventJournal instead of the shared DB's journal.
    //
    // The worker's SessionActor calls emit_event (e.g. on SetMode and
    // SetSessionModel). If those writes went to the shared DB file, SQLite
    // would need to create -journal/-wal/-shm sidecar files in ~/.qmt/ — but
    // the sandbox restricts directory writes there, causing "unable to open
    // database file" errors.
    //
    // An in-memory journal has no filesystem requirements, so all writes
    // succeed unconditionally inside the sandbox. Event persistence is the
    // orchestrator's responsibility; the worker only needs the session store
    // (reads + message inserts) from the shared DB.
    let noop_storage = SqliteStorage::connect(":memory:".into())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create in-memory event journal: {}", e))?;
    let event_journal = noop_storage.event_journal();

    // Use a default LLMParams — the worker inherits the session's LLM config
    // from the database. The initial_config is only used as a fallback.
    let initial_config = LLMParams::new();

    // Create a PluginRegistry with empty config (worker doesn't need plugin loading).
    let cache_path = std::env::temp_dir().join("querymt-worker-cache");
    let plugin_config = querymt::plugin::host::config::PluginConfig {
        providers: Vec::new(),
        oci: None,
    };
    let plugin_registry = Arc::new(
        querymt::plugin::host::PluginRegistry::from_config_with_cache_path(
            plugin_config,
            cache_path,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create plugin registry: {}", e))?,
    );

    // Build config with all built-in tools registered.
    let config =
        AgentConfigBuilder::new(plugin_registry, store, event_journal, initial_config).build();

    Ok(Arc::new(config))
}
