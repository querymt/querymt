use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt_agent::session::provider::SessionProvider;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use querymt_agent::session::store::SessionStore;
use std::sync::Arc;
use tempfile::TempDir;

async fn create_test_provider() -> (TempDir, Arc<SessionProvider>) {
    let temp_dir = TempDir::new().expect("temp dir");
    let db_path = temp_dir.path().join("test.db");
    let store = SqliteStorage::connect(db_path).await.expect("store");

    // Create initial config with a system prompt
    let initial_config = LLMParams::new()
        .provider("anthropic")
        .model("claude-3-5-sonnet-20241022")
        .system("You are a helpful assistant.")
        .system("Be concise and clear.");

    // Create a minimal plugin registry with temp config file
    let config_path = temp_dir.path().join("providers.toml");
    std::fs::write(
        &config_path,
        "[[providers]]\nname = \"mock\"\npath = \"mock.wasm\"\n",
    )
    .expect("write config");
    let registry = Arc::new(PluginRegistry::from_path(&config_path).expect("registry"));

    let provider = Arc::new(SessionProvider::new(
        registry,
        Arc::new(store) as Arc<dyn SessionStore>,
        initial_config,
    ));

    (temp_dir, provider)
}

#[tokio::test]
async fn test_initial_config_getter() {
    let (_temp_dir, provider) = create_test_provider().await;

    // Verify we can access initial_config
    let initial_config = provider.initial_config();
    assert_eq!(initial_config.provider.as_ref().unwrap(), "anthropic");
    assert_eq!(
        initial_config.model.as_ref().unwrap(),
        "claude-3-5-sonnet-20241022"
    );
    assert_eq!(initial_config.system.len(), 2);
    assert_eq!(initial_config.system[0], "You are a helpful assistant.");
    assert_eq!(initial_config.system[1], "Be concise and clear.");
}

#[tokio::test]
async fn test_session_creation_includes_system_prompt() {
    let (_temp_dir, provider) = create_test_provider().await;

    // Create a session
    let session = provider
        .create_session(None, None)
        .await
        .expect("create session");
    let session_id = &session.session().public_id;

    // Verify initial config has the system prompt in the database
    let initial_config = provider
        .history_store()
        .get_session_llm_config(session_id)
        .await
        .expect("get config")
        .expect("config exists");

    assert_eq!(initial_config.provider, "anthropic");
    assert_eq!(initial_config.model, "claude-3-5-sonnet-20241022");

    // Verify system prompt is in params
    let params = initial_config.params.expect("params exist");
    let system_array = params
        .get("system")
        .expect("system exists")
        .as_array()
        .expect("system is array");
    assert_eq!(system_array.len(), 2);
    assert_eq!(
        system_array[0].as_str().unwrap(),
        "You are a helpful assistant."
    );
    assert_eq!(system_array[1].as_str().unwrap(), "Be concise and clear.");
}

#[tokio::test]
async fn test_create_or_get_llm_config_preserves_system() {
    let (_temp_dir, provider) = create_test_provider().await;

    // Create a config with system prompt
    let config_with_system = LLMParams::new()
        .provider("anthropic")
        .model("claude-3-opus-20240229")
        .system("Custom system prompt part 1.")
        .system("Custom system prompt part 2.");

    let llm_config = provider
        .history_store()
        .create_or_get_llm_config(&config_with_system)
        .await
        .expect("create config");

    // Verify system prompt is stored
    let stored_config = provider
        .history_store()
        .get_llm_config(llm_config.id)
        .await
        .expect("get config")
        .expect("config exists");

    let params = stored_config.params.expect("params exist");
    let system_array = params
        .get("system")
        .expect("system exists")
        .as_array()
        .expect("system is array");
    assert_eq!(system_array.len(), 2);
    assert_eq!(
        system_array[0].as_str().unwrap(),
        "Custom system prompt part 1."
    );
    assert_eq!(
        system_array[1].as_str().unwrap(),
        "Custom system prompt part 2."
    );
}

#[tokio::test]
async fn test_config_without_system_gets_none_in_params() {
    let temp_dir = TempDir::new().expect("temp dir");
    let db_path = temp_dir.path().join("test.db");
    let store = SqliteStorage::connect(db_path).await.expect("store");

    // Create initial config WITHOUT system prompt
    let initial_config = LLMParams::new()
        .provider("anthropic")
        .model("claude-3-5-sonnet-20241022");

    // Create a minimal plugin registry with temp config file
    let config_path = temp_dir.path().join("providers.toml");
    std::fs::write(
        &config_path,
        "[[providers]]\nname = \"mock\"\npath = \"mock.wasm\"\n",
    )
    .expect("write config");
    let registry = Arc::new(PluginRegistry::from_path(&config_path).expect("registry"));
    let provider = Arc::new(SessionProvider::new(
        registry,
        Arc::new(store) as Arc<dyn SessionStore>,
        initial_config,
    ));

    let session = provider
        .create_session(None, None)
        .await
        .expect("create session");
    let session_id = &session.session().public_id;

    let config = provider
        .history_store()
        .get_session_llm_config(session_id)
        .await
        .expect("get config")
        .expect("config exists");

    // When there's no system prompt, params should be None or not contain "system"
    if let Some(params) = config.params {
        assert!(
            params.get("system").is_none()
                || params.get("system").unwrap().as_array().unwrap().is_empty()
        );
    }
}
