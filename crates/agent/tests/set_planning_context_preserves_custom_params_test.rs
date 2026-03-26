use kameo::actor::Spawn;
use querymt::LLMParams;
use querymt_agent::SessionActor;
use querymt_agent::agent::agent_config_builder::AgentConfigBuilder;
use querymt_agent::agent::core::{McpToolState, SessionRuntime};
use querymt_agent::agent::messages::SetPlanningContext;
use querymt_agent::session::backend::StorageBackend;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use querymt_agent::session::store::SessionExecutionConfig;
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn set_planning_context_preserves_custom_params() {
    let storage = Arc::new(
        SqliteStorage::connect(":memory:".into())
            .await
            .expect("storage"),
    );

    let config_path = {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("providers.toml");
        std::fs::write(&p, "providers = []\n").unwrap();
        // Leak the TempDir so the file stays alive for the test
        let p = p.clone();
        std::mem::forget(d);
        p
    };
    let registry =
        Arc::new(querymt::plugin::host::PluginRegistry::from_path(&config_path).expect("registry"));

    // Initial config with custom provider-specific params (llama_cpp-style)
    let initial_config = LLMParams::new()
        .provider("llama_cpp")
        .model("test-model")
        .system("You are a helpful assistant.")
        .parameter("text_only", true)
        .parameter("n_ctx", 150000)
        .parameter("flash_attention", "enabled")
        .parameter("kv_cache_type_k", "q8_0");

    let provider = Arc::new(querymt_agent::session::provider::SessionProvider::new(
        registry.clone(),
        storage.session_store(),
        initial_config,
    ));

    // Create session — writes config (with custom params) to DB
    let session = provider
        .create_session(None, None, &SessionExecutionConfig::default())
        .await
        .expect("create session");
    let session_id = session.session().public_id.clone();

    // Sanity: custom params are in the DB before SetPlanningContext
    let before = storage
        .session_store()
        .get_session_llm_config(&session_id)
        .await
        .unwrap()
        .unwrap();
    let before_params = before.params.as_ref().unwrap();
    assert_eq!(before_params.get("text_only").unwrap(), true);
    assert_eq!(before_params.get("n_ctx").unwrap(), 150000);

    // Spawn a SessionActor
    let agent_config = Arc::new(
        AgentConfigBuilder::new(
            registry,
            storage.session_store(),
            storage.event_journal(),
            LLMParams::new().provider("llama_cpp").model("test-model"),
        )
        .build(),
    );
    let runtime = SessionRuntime::new(None, HashMap::new(), McpToolState::empty());
    let actor = SessionActor::new(agent_config, session_id.clone(), runtime);
    let actor_ref = SessionActor::spawn(actor);

    // Send SetPlanningContext — this previously dropped all custom params
    actor_ref
        .ask(SetPlanningContext {
            summary: "Here is the planning context for the delegate.".to_string(),
        })
        .await
        .expect("SetPlanningContext");

    // Read back config — custom params must still be present
    let after = storage
        .session_store()
        .get_session_llm_config(&session_id)
        .await
        .unwrap()
        .unwrap();
    let after_params = after.params.as_ref().expect("params should exist");

    assert_eq!(
        after_params.get("text_only").unwrap(),
        true,
        "text_only must survive SetPlanningContext"
    );
    assert_eq!(
        after_params.get("n_ctx").unwrap(),
        150000,
        "n_ctx must survive SetPlanningContext"
    );
    assert_eq!(
        after_params.get("flash_attention").unwrap(),
        "enabled",
        "flash_attention must survive SetPlanningContext"
    );
    assert_eq!(
        after_params.get("kv_cache_type_k").unwrap(),
        "q8_0",
        "kv_cache_type_k must survive SetPlanningContext"
    );

    // System prompt should now include the planning context
    let system = after_params
        .get("system")
        .unwrap()
        .as_array()
        .expect("system is array");
    assert_eq!(system.len(), 2, "original prompt + planning context");
    assert_eq!(system[0].as_str().unwrap(), "You are a helpful assistant.");
    assert!(
        system[1]
            .as_str()
            .unwrap()
            .contains("planning context for the delegate")
    );
}
