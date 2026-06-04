#![cfg(all(test, feature = "remote"))]

use super::remote_setup::spawn_and_register_local_mesh_actors_with_name;
use crate::agent::LocalAgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::remote::test_helpers::fixtures::get_test_mesh;
use crate::session::backend::StorageBackend;
use crate::test_utils::{SharedLlmProvider, TestProviderFactory, mock_plugin_registry};
use querymt::LLMParams;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test]
async fn mesh_publication_triggers_local_only_model_refresh() {
    let provider = Arc::new(Mutex::new(crate::test_utils::MockLlmProvider::new()));
    let shared = SharedLlmProvider {
        inner: provider,
        tools: vec![].into_boxed_slice(),
    };
    let factory = Arc::new(TestProviderFactory { provider: shared });
    let (plugin_registry, _temp_dir) = mock_plugin_registry(factory).expect("plugin registry");
    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("create storage"),
    );
    let config = Arc::new(
        AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            storage.session_store(),
            storage.event_journal(),
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .build(),
    );
    let handle = LocalAgentHandle::from_config(config);
    let mesh = get_test_mesh().await;
    handle.set_mesh(mesh.clone());

    let refresh = handle.model_inventory.ensure_local_refresh().await;
    assert!(refresh.started_new_refresh());
    assert!(refresh.waits_for_completion());

    let _actors = spawn_and_register_local_mesh_actors_with_name(&handle, mesh, None).await;

    tokio::time::timeout(std::time::Duration::from_secs(5), refresh.wait())
        .await
        .expect("local refresh should complete");

    let (_, meta) = handle.model_inventory.get_snapshot().await;
    assert!(meta.local_updated_at.is_some());
    assert!(meta.remote_updated_at.is_none());
}
