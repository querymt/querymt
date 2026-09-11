use super::*;

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
        "querymt/session/undo",
        "querymt/session/redo",
        "querymt/session/undoStack",
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
async fn test_querymt_capabilities_lan_mesh_reports_no_invites_and_lan_transport() {
    let f = HandleFixture::new().await;
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    f.handle.set_mesh(mesh.clone());

    let result = ext_method_json(&f.handle, "querymt/capabilities", serde_json::json!({})).await;

    assert_eq!(result["transport"]["mesh_transport"], "lan");
    assert_eq!(result["features"]["mesh"], true);
    assert_eq!(result["features"]["mesh_invites"], false);
    assert!(
        result["notifications"]
            .as_array()
            .expect("notifications array")
            .iter()
            .any(|method| method == "querymt/schedules/changed")
    );
}
