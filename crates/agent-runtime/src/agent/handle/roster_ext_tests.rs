//! `querymt/agents/list|add|remove` (spec: agent-roster).

use super::*;
use crate::delegation::root_tests::root_over;
use crate::delegation::roster::AgentId;
use crate::test_utils::MockLlmProvider;
use serde_json::{Value, json};

async fn plain_root() -> (Arc<LocalAgentHandle>, tempfile::TempDir) {
    let (root, dir, _) = root_over(MockLlmProvider::new(), &[], None, vec![], false).await;
    (root, dir)
}

fn raw(value: &Value) -> Arc<serde_json::value::RawValue> {
    Arc::from(serde_json::value::RawValue::from_string(value.to_string()).unwrap())
}

async fn call(root: &LocalAgentHandle, method: &str, params: Value) -> Result<Value, Error> {
    let response = root
        .ext_method(ExtRequest::new(method, raw(&params)))
        .await?;
    Ok(serde_json::from_str(response.0.get()).expect("valid JSON"))
}

fn local(id: &str) -> Value {
    json!({"kind": "local", "id": id, "provider": "mock", "model": "mock-model"})
}

fn message(error: &Error) -> String {
    error
        .data
        .as_ref()
        .map(|data| data.to_string())
        .unwrap_or_default()
}

async fn ids(root: &LocalAgentHandle) -> (u64, Vec<String>) {
    let list = call(root, "querymt/agents/list", json!({})).await.unwrap();
    let agents = list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["id"].as_str().unwrap().to_string())
        .collect();
    (list["generation"].as_u64().unwrap(), agents)
}

#[tokio::test]
async fn plain_agent_has_an_empty_roster() {
    let (root, _dir) = plain_root().await;
    let list = call(&root, "querymt/agents/list", json!({})).await.unwrap();
    assert_eq!(list["agents"], json!([]));
    assert_eq!(list["generation"], 0);
    root.shutdown().await;
}

#[tokio::test]
async fn roster_methods_are_advertised() {
    let (root, _dir) = plain_root().await;
    let capabilities = crate::control::capabilities::get_capabilities(&root);
    for method in [
        "querymt/agents/list",
        "querymt/agents/add",
        "querymt/agents/remove",
    ] {
        assert!(capabilities.methods.iter().any(|m| m == method), "{method}");
    }
    assert!(
        capabilities
            .notifications
            .iter()
            .any(|n| n == "querymt/agents/changed")
    );
    root.shutdown().await;
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn added_remote_member_is_listed() {
    let (root, _dir) = plain_root().await;
    root.set_mesh(
        crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone(),
    );
    let response = call(
        &root,
        "querymt/agents/add",
        json!({"agent": {"kind": "remote", "id": "gpu", "name": "GPU", "peer": "gpu-node"}}),
    )
    .await
    .unwrap();
    assert_eq!(response["generation"], 1);

    let list = call(&root, "querymt/agents/list", json!({})).await.unwrap();
    assert_eq!(list["agents"][0]["id"], "gpu");
    assert_eq!(list["agents"][0]["origin"], "runtime");
    assert_eq!(list["agents"][0]["location"], "remote");
    root.shutdown().await;
}

#[tokio::test]
async fn remote_member_without_a_mesh_is_rejected() {
    let (root, _dir) = plain_root().await;
    let error = call(
        &root,
        "querymt/agents/add",
        json!({"agent": {"kind": "remote", "id": "gpu", "name": "GPU", "peer": "gpu-node"}}),
    )
    .await
    .unwrap_err();
    assert!(message(&error).contains("mesh is required"), "{error:?}");
    assert_eq!(ids(&root).await, (0, vec![]));
    root.shutdown().await;
}

#[tokio::test]
async fn duplicate_member_id_is_rejected() {
    let (root, _dir) = plain_root().await;
    call(
        &root,
        "querymt/agents/add",
        json!({"agent": local("coder")}),
    )
    .await
    .unwrap();
    let error = call(
        &root,
        "querymt/agents/add",
        json!({"agent": local("coder")}),
    )
    .await
    .unwrap_err();
    let text = message(&error);
    assert!(text.contains("coder") && text.contains("runtime"), "{text}");
    assert_eq!(ids(&root).await, (1, vec!["coder".to_string()]));
    root.shutdown().await;
}

#[tokio::test]
async fn invalid_member_is_rejected() {
    let (root, _dir) = plain_root().await;
    let mut unknown_key = local("a");
    unknown_key["colour"] = json!("red");
    let mut unknown_kind = local("b");
    unknown_kind["kind"] = json!("carrier-pigeon");
    let mut bad_middleware = local("c");
    bad_middleware["middleware"] = json!([{"type": "no_such_middleware"}]);
    for (agent, needle) in [
        (unknown_key, "colour"),
        (unknown_kind, "carrier-pigeon"),
        (bad_middleware, "no_such_middleware"),
    ] {
        let error = call(&root, "querymt/agents/add", json!({"agent": agent}))
            .await
            .unwrap_err();
        assert!(message(&error).contains(needle), "{needle}: {error:?}");
    }
    assert_eq!(ids(&root).await, (0, vec![]));
    root.shutdown().await;
}

#[tokio::test]
async fn peer_routing_is_not_accepted_for_a_member_added_at_runtime() {
    let (root, _dir) = plain_root().await;
    let mut agent = local("coder");
    agent["peer"] = json!("gpu-node");
    let error = call(&root, "querymt/agents/add", json!({"agent": agent}))
        .await
        .unwrap_err();
    assert!(
        message(&error).contains("only supported for configured delegates"),
        "{error:?}"
    );
    assert_eq!(ids(&root).await, (0, vec![]));
    root.shutdown().await;
}

#[tokio::test]
async fn adding_a_member_while_delegation_is_disabled_is_rejected() {
    let (root, _dir, _) =
        root_over(MockLlmProvider::new(), &["read_tool"], None, vec![], false).await;
    let error = call(
        &root,
        "querymt/agents/add",
        json!({"agent": local("coder")}),
    )
    .await
    .unwrap_err();
    let text = message(&error);
    assert!(
        text.contains("delegation is disabled") && text.contains("delegate"),
        "{text}"
    );
    assert_eq!(ids(&root).await, (0, vec![]));
    root.shutdown().await;
}

#[tokio::test]
async fn file_prompt_part_is_rejected_over_acp() {
    let (root, _dir) = plain_root().await;
    let mut agent = local("coder");
    agent["system"] = json!([{"file": "/etc/passwd"}]);
    let error = call(&root, "querymt/agents/add", json!({"agent": agent}))
        .await
        .unwrap_err();
    assert!(message(&error).contains("name a file"), "{error:?}");
    assert_eq!(ids(&root).await, (0, vec![]));
    root.shutdown().await;
}

#[tokio::test]
async fn removing_an_unknown_member_fails() {
    let (root, _dir) = plain_root().await;
    let error = call(&root, "querymt/agents/remove", json!({"id": "ghost"}))
        .await
        .unwrap_err();
    assert!(
        message(&error).contains("unknown agent 'ghost'"),
        "{error:?}"
    );
    assert_eq!(ids(&root).await.0, 0);
    root.shutdown().await;
}

#[tokio::test]
async fn clients_are_notified_of_a_roster_change() {
    let (root, _dir) = plain_root().await;
    let mut first = root.subscribe_ext_notifications();
    let mut second = root.subscribe_ext_notifications();

    let response = call(
        &root,
        "querymt/agents/add",
        json!({"agent": local("coder")}),
    )
    .await
    .unwrap();
    let generation = response["generation"].as_u64().unwrap();

    for client in [&mut first, &mut second] {
        let notification = client.try_recv().expect("notified");
        assert_eq!(notification.method.as_ref(), "querymt/agents/changed");
        let params: Value = serde_json::from_str(notification.params.get()).unwrap();
        assert_eq!(params["generation"].as_u64(), Some(generation));
        assert_eq!(params["added"], "coder");
        assert!(params.get("removed").is_none());
        assert!(client.try_recv().is_err(), "exactly one notification");
    }
    assert_eq!(response["added"], "coder");

    let removed = call(&root, "querymt/agents/remove", json!({"id": "coder"}))
        .await
        .unwrap();
    assert_eq!(removed["removed"], "coder");
    assert!(removed.get("added").is_none());
    let notification = first.try_recv().expect("notified of the removal");
    let params: Value = serde_json::from_str(notification.params.get()).unwrap();
    assert_eq!(params["removed"], "coder");
    assert_eq!(params["generation"].as_u64(), Some(generation + 1));
    // Drain what `second` also received so the failure check below is exact.
    while second.try_recv().is_ok() {}

    // A failed request sends nothing.
    call(&root, "querymt/agents/remove", json!({"id": "ghost"}))
        .await
        .unwrap_err();
    assert!(first.try_recv().is_err());
    root.shutdown().await;
}

/// Every JSON example of "Changing agents while running" in the docs.
fn roster_docs_examples() -> Vec<Value> {
    let doc = include_str!("../../../../../docs/docs/agent/delegation.md");
    let section = doc
        .split("\n## Changing agents while running\n")
        .nth(1)
        .expect("delegation.md has the section")
        .split("\n## ")
        .next()
        .unwrap();
    section
        .split("```json\n")
        .skip(1)
        .map(|rest| {
            let block = rest.split("```").next().unwrap();
            serde_json::from_str(block)
                .unwrap_or_else(|error| panic!("example is not JSON ({error}):\n{block}"))
        })
        .collect()
}

#[test]
fn roster_docs_examples_parse() {
    use crate::delegation::member::{MemberBuildSpec, MemberSpec, available_local_tool_names};
    use crate::delegation::roster::{MemberLocation, MemberOrigin};

    let examples = roster_docs_examples();
    assert!(examples.len() >= 7, "found {} examples", examples.len());
    let (mut adds, mut lists, mut notifications) = (0, 0, 0);
    for example in examples {
        match example["method"].as_str() {
            Some("querymt/agents/add") => {
                adds += 1;
                let agent = example["params"]["agent"].clone();
                let spec: MemberSpec = serde_json::from_value(agent).expect("documented member");
                spec.check_external(&AgentId::try_from(spec.raw_id()).unwrap())
                    .expect("accepted from a client");
                if let MemberSpec::Local(delegate) = spec {
                    MemberBuildSpec::from_delegate_config(
                        *delegate,
                        &[],
                        &available_local_tool_names(),
                    )
                    .expect("documented local member lowers");
                }
            }
            Some("querymt/agents/list") | Some("querymt/agents/remove") => {
                let params = &example["params"];
                assert!(params.is_object(), "{example}");
            }
            Some("querymt/agents/changed") => {
                notifications += 1;
                assert!(example["params"]["generation"].is_u64(), "{example}");
            }
            Some(other) => panic!("unexpected method {other}"),
            None if example.get("agents").is_some() => {
                lists += 1;
                assert!(example["generation"].is_u64());
                for agent in example["agents"].as_array().unwrap() {
                    for key in ["id", "name", "description", "capabilities"] {
                        assert!(agent.get(key).is_some(), "{key} in {agent}");
                    }
                    serde_json::from_value::<MemberOrigin>(agent["origin"].clone())
                        .expect("documented origin");
                    serde_json::from_value::<MemberLocation>(agent["location"].clone())
                        .expect("documented location");
                }
            }
            None => assert!(example["generation"].is_u64(), "{example}"),
        }
    }
    assert_eq!((adds, lists, notifications), (2, 1, 1));
}
