//! The harness adapter profile (`examples/confs/harness_adapter.toml`) contract.

use querymt::plugin::host::PluginRegistry;
use querymt_agent::api::{Agent, AgentInfra};
use querymt_agent::config::{Config, SingleAgentConfig, ToolRef, load_config};
use querymt_agent::session::backend::StorageBackend;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use querymt_agent::template::SessionTemplateContext;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Environment variables are process-global; serialize the tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

const FIXED_TOOLS: [&str; 15] = [
    "edit",
    "read_tool",
    "write_file",
    "multiedit",
    "replace_symbol",
    "glob",
    "search_text",
    "ls",
    "index",
    "get_symbol",
    "get_function",
    "find_symbol_references",
    "shell",
    "todowrite",
    "todoread",
];

fn profile_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/confs/harness_adapter.toml")
}

fn set_endpoint(base_url: Option<&str>, model: &str, key: &str) {
    // SAFETY: callers hold ENV_LOCK, so no other test thread touches these.
    unsafe {
        match base_url {
            Some(url) => std::env::set_var("QMT_BASE_URL", url),
            None => std::env::remove_var("QMT_BASE_URL"),
        }
        std::env::set_var("QMT_MODEL", model);
        std::env::set_var("QMT_API_KEY", key);
    }
}

async fn load_single() -> SingleAgentConfig {
    match load_config(profile_path()).await.expect("profile loads") {
        Config::Single(config) => *config,
        Config::Multi(_) => panic!("harness adapter profile must be a single-agent config"),
    }
}

/// Names of the tools offered to the model, in declaration order: the agent
/// registry's definitions filtered by the session tool policy, which is what
/// `AgentConfig::collect_tools` does for `BuiltInOnly`.
fn offered_tools(agent: &Agent) -> Vec<String> {
    let handle = agent.handle();
    let policy = handle.config.tool_config_snapshot();
    handle
        .tool_registry()
        .definitions()
        .into_iter()
        .map(|t| t.function.name)
        .filter(|name| policy.allows(ToolRef::builtin(name)))
        .collect()
}

async fn build_agent(config: SingleAgentConfig) -> (TempDir, Agent) {
    let dir = TempDir::new().expect("temp dir");
    let providers = dir.path().join("providers.toml");
    std::fs::write(&providers, "providers = []\n").expect("providers.toml");
    let registry = Arc::new(PluginRegistry::from_path(&providers).expect("registry"));
    let storage = SqliteStorage::connect(dir.path().join("sessions.db"))
        .await
        .expect("storage");
    let agent = Agent::builder_from_config(config)
        .expect("builder")
        .infra(AgentInfra {
            plugin_registry: registry,
            storage: Some(Arc::new(storage) as Arc<dyn StorageBackend>),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .expect("agent builds from the profile");
    (dir, agent)
}

#[tokio::test]
async fn profile_resolves_endpoint_from_environment() {
    let _guard = ENV_LOCK.lock().await;
    set_endpoint(Some("http://proxy/v1"), "m", "k");

    let config = load_single().await;
    assert_eq!(config.agent.provider, "openai");
    assert_eq!(config.agent.model, "m");
    assert_eq!(config.agent.api_key.as_deref(), Some("k"));
    let params = config.agent.parameters.expect("parameters table");
    assert_eq!(
        params.get("base_url").and_then(|v| v.as_str()),
        Some("http://proxy/v1")
    );
}

#[tokio::test]
async fn missing_endpoint_variable_fails_profile_load() {
    let _guard = ENV_LOCK.lock().await;
    set_endpoint(None, "m", "k");

    let error = load_config(profile_path())
        .await
        .expect_err("loading without QMT_BASE_URL must fail");
    assert!(
        format!("{error:#}").contains("QMT_BASE_URL"),
        "error should name the variable: {error:#}"
    );
}

#[tokio::test]
async fn profile_advertises_the_fixed_tool_set() {
    let _guard = ENV_LOCK.lock().await;
    set_endpoint(Some("http://proxy/v1"), "m", "k");

    let (_dir, agent) = build_agent(load_single().await).await;
    let offered = offered_tools(&agent);
    let names: BTreeSet<String> = offered.iter().cloned().collect();
    let expected: BTreeSet<String> = FIXED_TOOLS.iter().map(|s| s.to_string()).collect();
    assert_eq!(names, expected);
    assert_eq!(offered.len(), FIXED_TOOLS.len(), "no duplicates");

    // Stable order: a second agent built from the same profile declares the
    // tools in the same order (the registry is ordered by name).
    let (_dir2, again) = build_agent(load_single().await).await;
    assert_eq!(offered_tools(&again), offered);
}

#[tokio::test]
async fn interactive_and_web_tools_are_absent() {
    let _guard = ENV_LOCK.lock().await;
    set_endpoint(Some("http://proxy/v1"), "m", "k");

    let (_dir, agent) = build_agent(load_single().await).await;
    let names = offered_tools(&agent);
    for forbidden in ["question", "browse", "skill", "session_history"] {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "`{forbidden}` must not be declared"
        );
    }
}

#[tokio::test]
async fn profile_disables_history_rewriting_and_sets_budget() {
    let _guard = ENV_LOCK.lock().await;
    set_endpoint(Some("http://proxy/v1"), "m", "k");

    let config = load_single().await;
    assert!(!config.agent.execution.pruning.enabled, "pruning is off");
    assert!(!config.agent.execution.compaction.auto, "compaction is off");
    assert!(
        config
            .middleware
            .iter()
            .all(|m| m.middleware_type != "context"),
        "no context middleware"
    );
    let limits: Vec<_> = config
        .middleware
        .iter()
        .filter(|m| m.middleware_type == "limits")
        .collect();
    assert_eq!(limits.len(), 1, "exactly one limits middleware");
    assert_eq!(limits[0].config.get("max_steps"), Some(&50.into()));
}

#[tokio::test]
async fn prompt_omits_endpoint_identity() {
    let _guard = ENV_LOCK.lock().await;
    set_endpoint(Some("http://proxy-y/v1"), "adapter-x", "secret-key-z");

    let config = load_single().await;
    let dir = TempDir::new().expect("temp dir");
    let context = SessionTemplateContext::builder()
        .cwd(dir.path())
        .provider("openai")
        .model("adapter-x")
        .build();

    let mut rendered = String::new();
    for part in &config.agent.system {
        let prompt = part.resolved().expect("system files are resolved at load");
        rendered.push_str(&context.render(prompt.text()).expect("renders"));
        rendered.push('\n');
    }

    assert!(!rendered.is_empty());
    assert!(rendered.contains("Platform:"), "environment block present");
    for leaked in ["adapter-x", "proxy-y", "openai", "secret-key-z"] {
        assert!(!rendered.contains(leaked), "prompt leaks `{leaked}`");
    }
}
