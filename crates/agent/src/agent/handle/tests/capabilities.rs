use super::*;

async fn ext_method_json(
    handle: &LocalAgentHandle,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let req = crate::acp::protocol::ExtRequest::new(
        method,
        std::sync::Arc::from(serde_json::value::RawValue::from_string(params.to_string()).unwrap()),
    );
    let resp = handle.ext_method(req).await.expect("ext_method");
    serde_json::from_str(resp.0.get()).expect("valid JSON")
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
    assert!(
        result["notifications"]
            .as_array()
            .expect("notifications array")
            .iter()
            .any(|method| method == "querymt/models/changed")
    );
    assert_eq!(result["transport"]["mesh_transport"], "none");
    assert_eq!(result["features"]["mesh_invites"], false);
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
