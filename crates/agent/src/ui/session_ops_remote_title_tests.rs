#![cfg(all(test, feature = "api", feature = "remote"))]

use crate::agent::remote::RemoteSessionInfo;
use crate::ui::handlers::refresh_attached_remote_summary;
use crate::ui::messages::SessionSummary;
use std::collections::HashMap;

#[test]
fn refresh_attached_remote_summary_updates_title_for_attached_session() {
    let mut by_node = HashMap::<String, Vec<SessionSummary>>::new();
    by_node.insert(
        "peer-a".to_string(),
        vec![SessionSummary {
            session_id: "sess-1".to_string(),
            name: None,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            parent_session_id: None,
            fork_origin: None,
            session_kind: None,
            has_children: false,
            node: Some("peer-a".to_string()),
            node_id: None,
            attached: Some(true),
            runtime_state: Some("active".to_string()),
        }],
    );

    let changed = refresh_attached_remote_summary(
        &mut by_node,
        "peer-a",
        "node-123",
        &RemoteSessionInfo {
            session_id: "sess-1".to_string(),
            actor_id: 42,
            cwd: Some("/tmp".to_string()),
            created_at: 123,
            title: Some("Remote title from snapshot".to_string()),
            peer_label: "peer-a".to_string(),
            runtime_state: Some("active".to_string()),
        },
    );

    assert!(changed);
    let summary = &by_node["peer-a"][0];
    assert_eq!(summary.title.as_deref(), Some("Remote title from snapshot"));
    assert_eq!(summary.name.as_deref(), Some("Remote title from snapshot"));
    assert_eq!(summary.node_id.as_deref(), Some("node-123"));
}

#[test]
fn refresh_attached_remote_summary_does_not_clear_existing_title_on_none() {
    let mut by_node = HashMap::<String, Vec<SessionSummary>>::new();
    by_node.insert(
        "peer-a".to_string(),
        vec![SessionSummary {
            session_id: "sess-1".to_string(),
            name: Some("Existing".to_string()),
            cwd: None,
            title: Some("Existing".to_string()),
            created_at: None,
            updated_at: None,
            parent_session_id: None,
            fork_origin: None,
            session_kind: None,
            has_children: false,
            node: Some("peer-a".to_string()),
            node_id: None,
            attached: Some(true),
            runtime_state: Some("active".to_string()),
        }],
    );

    let changed = refresh_attached_remote_summary(
        &mut by_node,
        "peer-a",
        "node-123",
        &RemoteSessionInfo {
            session_id: "sess-1".to_string(),
            actor_id: 42,
            cwd: Some("/tmp".to_string()),
            created_at: 123,
            title: None,
            peer_label: "peer-a".to_string(),
            runtime_state: Some("active".to_string()),
        },
    );

    assert!(changed);
    let summary = &by_node["peer-a"][0];
    assert_eq!(summary.title.as_deref(), Some("Existing"));
    assert_eq!(summary.name.as_deref(), Some("Existing"));
}
