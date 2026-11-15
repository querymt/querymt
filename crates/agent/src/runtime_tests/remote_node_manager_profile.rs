#![cfg(feature = "remote")]
use crate::agent::remote::node_manager::{
    CreateRemoteSession, GetRemoteSessionProfile, ListRemoteSessions, RemoteNodeManager,
};
use crate::agent::remote::tests::test_agent_config;
use crate::agent::session_registry::SessionRegistry;
use crate::test_utils::{
    MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
};
use kameo::actor::Spawn;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test]
async fn test_owner_profile_identity_is_listed_and_available_by_session_id() {
    use crate::api::AgentInfra;
    use crate::profiles::{
        LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager, SessionProfileBinding,
    };

    let (config, _td) = test_agent_config().await;
    let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));
    let (plugins, _plugins_dir) =
        mock_plugin_registry(Arc::new(TestProviderFactory::new(SharedLlmProvider {
            inner: Arc::new(Mutex::new(MockLlmProvider::new())),
            tools: vec![].into_boxed_slice(),
        })))
        .expect("plugins");
    let catalog: Arc<dyn ProfileCatalog> = Arc::new(LocalProfileCatalog::builder().build());
    let profile = catalog.list_profiles().await.expect("profiles").remove(0);
    let profiles = Arc::new(ProfileRuntimeManager::with_infra_boxed(
        catalog,
        profile.id.clone(),
        AgentInfra {
            plugin_registry: Arc::new(plugins),
            storage: Some(config.storage.clone()),
            session_mcp_attachment_source: None,
            event_fanout: None,
        },
    ));
    let profile_slot = Arc::new(arc_swap::ArcSwap::from_pointee(None));
    let manager = RemoteNodeManager::new(
        config,
        registry,
        None,
        Arc::new(parking_lot::Mutex::new(None)),
    )
    .with_profiles_slot(profile_slot.clone());
    let node = RemoteNodeManager::spawn(manager);
    let session = node
        .ask(CreateRemoteSession { cwd: None })
        .await
        .expect("create");
    profile_slot.store(Arc::new(Some(profiles.clone())));
    profiles
        .set_session_binding(
            &session.session_id,
            SessionProfileBinding {
                profile_id: profile.id.clone(),
                agent_id: None,
                profile_fingerprint: None,
                profile_source: None,
                profile_config_kind: None,
                provider_lock_digest: None,
                provider_locks_json: None,
            },
        )
        .await
        .expect("bind session for listing");

    let listed = node
        .ask(ListRemoteSessions {
            offset: None,
            limit: None,
        })
        .await
        .expect("list");
    assert_eq!(
        listed.sessions[0].profile_id.as_deref(),
        Some(profile.id.as_str())
    );
    assert_eq!(
        listed.sessions[0].profile_label.as_deref(),
        Some(profile.name.as_str())
    );
    let targeted = node
        .ask(GetRemoteSessionProfile {
            session_id: session.session_id,
        })
        .await
        .expect("profile");
    assert_eq!(
        targeted.as_ref().map(|value| value.profile_label.as_str()),
        Some(profile.name.as_str())
    );
    assert!(
        node.ask(GetRemoteSessionProfile {
            session_id: "missing".into()
        })
        .await
        .expect("missing")
        .is_none()
    );
}
