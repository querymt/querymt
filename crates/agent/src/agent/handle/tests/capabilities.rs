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
        "querymt/session/history",
        "querymt/session/toolContent",
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
    assert_eq!(result["features"]["session_history_window"], true);
    assert_eq!(result["features"]["session_tool_content"], true);
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

#[tokio::test]
async fn capabilities_advertise_workspace_files_query_and_index_ready() {
    let f = HandleFixture::new().await;
    let result = ext_method_json(&f.handle, "querymt/capabilities", serde_json::json!({})).await;
    let listed = |key: &str, name: &str| {
        result[key]
            .as_array()
            .expect("array")
            .iter()
            .any(|entry| entry == name)
    };
    assert!(listed("methods", "querymt/workspace/files"));
    assert!(listed("notifications", "querymt/workspace/indexReady"));
}

#[tokio::test]
async fn workspace_files_rejects_invalid_targets_with_invalid_params() {
    let f = HandleFixture::new().await;
    let cwd = f._temp_dir.path().display().to_string();
    let missing = f._temp_dir.path().join("nope").display().to_string();
    for params in [
        serde_json::json!({"session_id": "s", "cwd": cwd}),
        serde_json::json!({}),
        serde_json::json!({"cwd": "relative/dir"}),
        serde_json::json!({"cwd": missing}),
        serde_json::json!({"cwd": "/"}),
    ] {
        let error = f
            .handle
            .ext_method(crate::acp::protocol::ExtRequest::new(
                "querymt/workspace/files",
                raw_params(&params.to_string()),
            ))
            .await
            .expect_err("invalid target");
        assert_eq!(
            error.code,
            crate::acp::protocol::ErrorCode::InvalidParams,
            "{params}"
        );
    }
}

#[tokio::test]
async fn workspace_files_unknown_session_is_resource_not_found() {
    let f = HandleFixture::new().await;
    let error = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/workspace/files",
            raw_params(r#"{"session_id":"does-not-exist"}"#),
        ))
        .await
        .expect_err("unknown session");
    assert_eq!(
        error.code,
        crate::acp::protocol::ErrorCode::ResourceNotFound
    );
}

#[tokio::test]
async fn workspace_files_cwd_only_lists_entries_and_creates_no_session() {
    let f = HandleFixture::new().await;
    let root = f._temp_dir.path().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/model.rs"), "").unwrap();
    std::fs::write(root.join("target.txt"), "").unwrap();
    let cwd = root.display().to_string();
    // The index builds in the background; poll until it is ready.
    let mut result = serde_json::Value::Null;
    for _ in 0..100 {
        result = ext_method_json(
            &f.handle,
            "querymt/workspace/files",
            serde_json::json!({"cwd": cwd, "query": "model"}),
        )
        .await;
        if result["status"] == "ready" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(result["status"], "ready", "{result}");
    assert_eq!(result["entries"][0]["path"], "src/model.rs");
    assert_eq!(result["entries"][0]["name"], "model.rs");
    assert!(
        result["entries"][0]["uri"]
            .as_str()
            .unwrap()
            .starts_with("file://")
    );
    assert!(f.handle.registry.lock().await.session_ids().is_empty());
}

#[test]
fn index_ready_event_translates_to_extension_notification() {
    use crate::events::{AgentEventKind, EventOrigin};
    let event = crate::events::EventEnvelope::Durable(crate::events::DurableEvent {
        event_id: "e".into(),
        stream_seq: 1,
        session_seq: 1,
        session_id: "sess-1".into(),
        timestamp: 1,
        origin: EventOrigin::Local,
        source_node: None,
        kind: AgentEventKind::WorkspaceIndexReady {
            workspace_root: "/repo".into(),
        },
    });
    let session_id = "sess-1";
    crate::workspace_files::cache::insert(
        session_id,
        std::sync::Arc::new(crate::agent::file_proxy::GetFileIndexResponse {
            files: Vec::new(),
            generated_at: 0,
            workspace_root: "/repo".into(),
            cwd: None,
        }),
    );
    let notification = crate::acp::shared::on_workspace_index_ready(&event).expect("notification");
    // Every transport handles the event through this function, so the stale
    // cached index is always dropped.
    assert!(crate::workspace_files::cache::get(session_id).is_none());
    assert_eq!(notification["method"], "querymt/workspace/indexReady");
    assert_eq!(notification["params"]["session_id"], "sess-1");
    assert_eq!(notification["params"]["workspace_root"], "/repo");
}

#[tokio::test]
async fn workspace_files_excludes_gitignored_entries() {
    let f = HandleFixture::new().await;
    let root = f._temp_dir.path().join("ignored-repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(root.join("target/built.rs"), "").unwrap();
    std::fs::write(root.join("kept.rs"), "").unwrap();
    let cwd = root.display().to_string();
    let mut result = serde_json::Value::Null;
    for _ in 0..100 {
        result = ext_method_json(
            &f.handle,
            "querymt/workspace/files",
            serde_json::json!({"cwd": cwd}),
        )
        .await;
        if result["status"] == "ready" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(result["status"], "ready", "{result}");
    let paths: Vec<&str> = result["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"kept.rs"), "{paths:?}");
    assert!(!paths.iter().any(|p| p.starts_with("target")), "{paths:?}");
}

#[tokio::test]
async fn workspace_files_rejects_filesystem_root_aliases() {
    let f = HandleFixture::new().await;
    let mut alias = dunce::canonicalize(f._temp_dir.path()).unwrap();
    for _ in 0..alias.components().count() {
        alias.push("..");
    }
    let error = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/workspace/files",
            raw_params(&serde_json::json!({"cwd": alias}).to_string()),
        ))
        .await
        .expect_err("root alias");
    assert_eq!(error.code, crate::acp::protocol::ErrorCode::InvalidParams);
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_files_resolves_symlinked_and_dotdot_cwds() {
    let f = HandleFixture::new().await;
    let root = f._temp_dir.path().join("alias-repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "").unwrap();
    let link = f._temp_dir.path().join("alias-link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    let canonical = dunce::canonicalize(&root).unwrap();
    for cwd in [link.clone(), link.join("src/..")] {
        let mut result = serde_json::Value::Null;
        for _ in 0..100 {
            result = ext_method_json(
                &f.handle,
                "querymt/workspace/files",
                serde_json::json!({"cwd": cwd, "query": "lib"}),
            )
            .await;
            if result["status"] == "ready" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(result["status"], "ready", "{result}");
        assert_eq!(result["cwd"], canonical.display().to_string());
        assert_eq!(result["entries"][0]["path"], "src/lib.rs");
    }
}
