//! `querymt/agents/*` with profiles and rebuilds (spec: agent-roster).

use super::*;
use serde_json::{Value, json};

fn local(id: &str) -> Value {
    json!({"kind": "local", "id": id, "provider": "test", "model": "test-model"})
}

async fn call(
    handle: &LocalAgentHandle,
    method: &str,
    params: Value,
) -> Result<Value, agent_client_protocol::Error> {
    let response = handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            method,
            raw_params(&params.to_string()),
        ))
        .await?;
    Ok(serde_json::from_str(response.0.get()).expect("valid JSON"))
}

async fn list(handle: &LocalAgentHandle, profile: &str) -> (u64, Vec<String>) {
    let list = call(handle, "querymt/agents/list", json!({"profileId": profile}))
        .await
        .expect("list");
    let ids = list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["id"].as_str().unwrap().to_string())
        .collect();
    (list["generation"].as_u64().unwrap(), ids)
}

#[tokio::test]
async fn runtime_changes_are_gone_after_restart() {
    let profile_dir = tempfile::tempdir().unwrap();
    write_profile(profile_dir.path(), "alpha.toml", ALPHA_PROFILE_TOML);
    write_profile(profile_dir.path(), "quorum.toml", QUORUM_PROFILE_TOML);
    let storage: Arc<dyn StorageBackend> = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap(),
    );

    let first = HandleFixture::new()
        .await
        .with_profile_storage("alpha", profile_dir.path(), storage.clone())
        .await;
    let added = call(
        &first.handle,
        "querymt/agents/add",
        json!({"profileId": "quorum", "agent": local("tester")}),
    )
    .await
    .expect("add");
    assert_eq!(added["generation"], 1);
    call(
        &first.handle,
        "querymt/agents/remove",
        json!({"profile_id": "quorum", "id": "coder"}),
    )
    .await
    .expect("remove a configured member");
    assert_eq!(
        list(&first.handle, "quorum").await,
        (2, vec!["reviewer".to_string(), "tester".to_string()])
    );

    // A restart: new runtimes from the same configuration and database.
    let second = HandleFixture::new()
        .await
        .with_profile_storage("alpha", profile_dir.path(), storage)
        .await;
    assert_eq!(
        list(&second.handle, "quorum").await,
        (0, vec!["coder".to_string(), "reviewer".to_string()])
    );
}

#[tokio::test]
async fn adding_a_member_to_one_profile_leaves_another_unchanged() {
    let (f, _dir) = profile_fixture_with_files(&[
        ("alpha.toml", ALPHA_PROFILE_TOML),
        ("beta.toml", BETA_PROFILE_TOML),
    ])
    .await;
    let mut notifications = f.handle.subscribe_ext_notifications();

    let added = call(
        &f.handle,
        "querymt/agents/add",
        json!({"profileId": "alpha", "agent": local("reviewer")}),
    )
    .await
    .expect("add");
    assert_eq!(added["generation"], 1);
    assert_eq!(added["profileId"], "alpha");

    assert_eq!(
        list(&f.handle, "alpha").await,
        (1, vec!["reviewer".to_string()])
    );
    assert_eq!(list(&f.handle, "beta").await, (0, vec![]));
    // The root of the connection is untouched as well.
    assert!(f.handle.config.roster().is_none());

    let notification = notifications.try_recv().expect("clients are told");
    assert_eq!(notification.method.as_ref(), "querymt/agents/changed");
    let params: Value = serde_json::from_str(notification.params.get()).unwrap();
    assert_eq!(params["generation"], 1);
    assert_eq!(params["profileId"], "alpha");
}

#[tokio::test]
async fn unknown_profile_is_rejected() {
    let (f, _dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    let mut notifications = f.handle.subscribe_ext_notifications();
    for (method, params) in [
        ("querymt/agents/list", json!({"profileId": "missing"})),
        (
            "querymt/agents/add",
            json!({"profileId": "missing", "agent": local("reviewer")}),
        ),
        (
            "querymt/agents/remove",
            json!({"profileId": "missing", "id": "reviewer"}),
        ),
    ] {
        let error = call(&f.handle, method, params).await.unwrap_err();
        assert_eq!(error.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);
        assert!(
            error.data.as_ref().unwrap().to_string().contains("missing"),
            "{method}: {error:?}"
        );
    }
    assert!(notifications.try_recv().is_err());
}
