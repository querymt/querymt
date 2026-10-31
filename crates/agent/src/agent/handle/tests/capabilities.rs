use super::*;

#[test]
fn legacy_capabilities_fixture_has_no_recovery_contract() {
    let legacy = serde_json::json!({
        "querymt_control_version": 1,
        "agent": {
            "id": "legacy-agent",
            "display_name": "Legacy QueryMT Agent",
            "kind": "local"
        },
        "transport": {
            "acp": true,
            "stdio": false,
            "websocket": true,
            "mesh": false,
            "mesh_transport": "none"
        },
        "features": {
            "mesh": false,
            "mesh_invites": false,
            "remote_sessions": false,
            "schedules": true,
            "remote_schedules": false,
            "profiles": false,
            "auth": true,
            "models": true,
            "steering": true
        },
        "methods": ["querymt/capabilities"],
        "notifications": []
    });

    let capabilities: crate::control::capabilities::CapabilitiesInfo =
        serde_json::from_value(legacy).expect("deserialize legacy capabilities");

    assert!(!capabilities.features.session_lifecycle);
    assert!(!capabilities.features.session_pins);
    assert!(capabilities.elicitation_recovery.is_none());
}

#[test]
fn session_lifecycle_feature_deserializes_explicit_values() {
    for expected in [false, true] {
        let features: crate::control::capabilities::ControlFeatureInfo =
            serde_json::from_value(serde_json::json!({
                "mesh": false,
                "mesh_invites": false,
                "remote_sessions": false,
                "session_lifecycle": expected,
                "schedules": false,
                "remote_schedules": false,
                "profiles": false,
                "auth": false,
                "models": false,
                "steering": false
            }))
            .expect("deserialize explicit session lifecycle capability");

        assert_eq!(features.session_lifecycle, expected);
        assert!(!features.session_pins);
    }
}

#[test]
fn session_pins_feature_preserves_explicit_values_and_serializes_them() {
    for expected in [false, true] {
        let features: crate::control::capabilities::ControlFeatureInfo =
            serde_json::from_value(serde_json::json!({
                "mesh": false,
                "mesh_invites": false,
                "remote_sessions": false,
                "session_lifecycle": false,
                "session_pins": expected,
                "schedules": false,
                "remote_schedules": false,
                "profiles": false,
                "auth": false,
                "models": false,
                "steering": false
            }))
            .expect("deserialize explicit session pins capability");

        assert_eq!(features.session_pins, expected);
        let serialized = serde_json::to_value(features).expect("serialize session pins capability");
        assert_eq!(serialized["session_pins"], expected);
    }
}

#[tokio::test]
async fn test_querymt_capabilities_lists_control_surface() {
    let f = HandleFixture::new().await;
    let result = ext_method_json(&f.handle, "querymt/capabilities", serde_json::json!({})).await;

    assert_eq!(result["querymt_control_version"], 1);
    assert_eq!(result["agent"]["display_name"], "QueryMT Agent");
    assert!(
        result["methods"]
            .as_array()
            .expect("methods array")
            .iter()
            .any(|method| method == "querymt/capabilities")
    );
    assert!(
        result["methods"]
            .as_array()
            .expect("methods array")
            .iter()
            .any(|method| method == "querymt/schedules/create")
    );
    assert!(
        result["methods"]
            .as_array()
            .expect("methods array")
            .iter()
            .any(|method| method == "querymt/schedules/get")
    );
    for expected in [
        "querymt/session/steer",
        "querymt/session/queue",
        "querymt/session/discardQueuedInput",
        "querymt/session/runtimeState",
        "querymt/session/undo",
        "querymt/session/redo",
        "querymt/session/undoStack",
        "querymt/session/getLifecycle",
        "querymt/session/setLifecycle",
        "querymt/session/setPinned",
        "querymt/auth/status",
        "querymt/auth/start",
        "querymt/auth/complete",
        "querymt/auth/logout",
        "querymt/auth/setApiToken",
        "querymt/auth/clearApiToken",
        "querymt/auth/setMethod",
    ] {
        assert!(
            result["methods"]
                .as_array()
                .expect("methods array")
                .iter()
                .any(|method| method == expected),
            "missing capability method {expected}"
        );
    }
    assert_eq!(result["features"]["auth"], true);
    assert_eq!(result["features"]["session_lifecycle"], true);
    assert_eq!(result["features"]["session_pins"], true);
    assert_eq!(result["elicitation_recovery"]["version"], 1);
    for method in [
        "querymt/elicitation/listPendingSessions",
        "querymt/elicitation/attachSession",
    ] {
        assert!(
            result["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|advertised| advertised == method),
            "missing recovery method {method}"
        );
    }
    let notifications = result["notifications"]
        .as_array()
        .expect("notifications array");
    assert!(
        notifications
            .iter()
            .any(|method| method == "querymt/models/changed")
    );
    assert!(
        notifications
            .iter()
            .any(|method| method == "querymt/session/delegationUpdate")
    );
    assert!(
        notifications
            .iter()
            .any(|method| method == "querymt/session/inputState")
    );
    assert!(
        notifications
            .iter()
            .any(|method| method == "querymt/session/pinsChanged")
    );
    assert_eq!(result["features"]["steering"], true);
    assert_eq!(result["transport"]["mesh_transport"], "none");
    assert_eq!(result["features"]["mesh_invites"], false);
    assert_eq!(result["features"]["profiles"], false);
    let methods = result["methods"].as_array().expect("methods array");
    for unavailable in [
        "querymt/profiles",
        "querymt/session/setDelegateModel",
        "querymt/session/delegateModels",
    ] {
        assert!(
            !methods.iter().any(|method| method == unavailable),
            "unexpected profile capability method {unavailable}"
        );
    }
    assert!(
        !notifications
            .iter()
            .any(|method| method == "querymt/session/delegateModelsChanged")
    );
}

struct NoViewStoreBackend {
    storage: Arc<crate::session::sqlite_storage::SqliteStorage>,
}

impl crate::session::backend::StorageBackend for NoViewStoreBackend {
    fn session_store(&self) -> Arc<dyn crate::session::store::SessionStore> {
        self.storage.clone()
    }

    fn event_journal(&self) -> Arc<dyn crate::session::projection::EventJournal> {
        self.storage.clone()
    }

    fn view_store(&self) -> Option<Arc<dyn crate::session::projection::ViewStore>> {
        None
    }
}

#[tokio::test]
async fn capabilities_report_lifecycle_unsupported_without_a_view_store() {
    let (plugin_registry, _temp_dir) = empty_plugin_registry().expect("plugin registry");
    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("test storage"),
    );
    let backend = Arc::new(NoViewStoreBackend { storage });
    let config = Arc::new(
        AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            backend,
            querymt::LLMParams::new()
                .provider("mock")
                .model("mock-model"),
        )
        .with_tool_policy(ToolPolicy::ProviderOnly)
        .build(),
    );
    let handle = LocalAgentHandle::from_config(config);
    let result = ext_method_json(&handle, "querymt/capabilities", serde_json::json!({})).await;

    assert_eq!(result["features"]["session_lifecycle"], false);
    assert_eq!(result["features"]["session_pins"], true);
    let methods = result["methods"].as_array().expect("methods array");
    for unavailable in [
        "querymt/session/getLifecycle",
        "querymt/session/setLifecycle",
    ] {
        assert!(!methods.iter().any(|method| method == unavailable));
    }
    assert!(
        methods
            .iter()
            .any(|method| method == "querymt/session/setPinned")
    );
}

#[tokio::test]
async fn capabilities_report_session_pins_unsupported_for_default_store() {
    let (plugin_registry, _temp_dir) = empty_plugin_registry().expect("plugin registry");
    let event_storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("test storage"),
    );
    let backend = Arc::new(TestStorageBackend {
        session_store: Arc::new(MockSessionStore::new()),
        event_storage,
    });
    let config = Arc::new(
        AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            backend,
            querymt::LLMParams::new()
                .provider("mock")
                .model("mock-model"),
        )
        .with_tool_policy(ToolPolicy::ProviderOnly)
        .build(),
    );
    let handle = LocalAgentHandle::from_config(config);
    let result = ext_method_json(&handle, "querymt/capabilities", serde_json::json!({})).await;

    assert_eq!(result["features"]["session_pins"], false);
    assert!(
        !result["methods"]
            .as_array()
            .expect("methods array")
            .iter()
            .any(|method| method == "querymt/session/setPinned")
    );
    assert!(
        !result["notifications"]
            .as_array()
            .expect("notifications array")
            .iter()
            .any(|notification| notification == "querymt/session/pinsChanged")
    );
}

#[tokio::test]
async fn test_querymt_capabilities_advertises_profile_methods_when_configured() {
    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;

    let result = ext_method_json(&f.handle, "querymt/capabilities", serde_json::json!({})).await;
    let methods = result["methods"].as_array().expect("methods array");

    assert_eq!(result["features"]["profiles"], true);
    let notifications = result["notifications"]
        .as_array()
        .expect("notifications array");
    assert!(
        notifications
            .iter()
            .any(|method| method == "querymt/session/delegateModelsChanged")
    );
    assert!(
        !methods
            .iter()
            .any(|method| method == "querymt/session/delegateModelsChanged")
    );
    for expected in [
        "querymt/profiles",
        "querymt/profile/agents",
        "querymt/profile/setActive",
        "querymt/session/setDelegateModel",
        "querymt/session/delegateModels",
    ] {
        assert!(
            methods.iter().any(|method| method == expected),
            "missing profile capability method {expected}"
        );
    }
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_capabilities_composite_mesh_reports_invites_and_multi_transport() {
    let f = HandleFixture::new().await;
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    f.handle.set_mesh(mesh.clone());

    let result = ext_method_json(&f.handle, "querymt/capabilities", serde_json::json!({})).await;

    assert_eq!(result["transport"]["mesh_transport"], "multi");
    assert_eq!(result["features"]["mesh"], true);
    assert_eq!(result["features"]["mesh_invites"], true);
    assert_eq!(result["features"]["profiles"], false);
    assert!(
        result["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .any(|method| {
                method == crate::acp::shared::QMT_NOTIFICATION_DELEGATE_MODELS_CHANGED
            }),
        "mesh-only viewers advertise relayed delegate-model changes"
    );
    assert!(
        result["notifications"]
            .as_array()
            .expect("notifications array")
            .iter()
            .any(|method| method == "querymt/schedules/changed")
    );
}
