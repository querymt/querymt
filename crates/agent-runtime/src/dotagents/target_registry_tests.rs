//! Tests for protocol delegation targets in the roster: collision handling,
//! build on first use, and unsupported connections.

use super::*;
use crate::delegation::member::MemberBuildSpec;
use crate::delegation::roster::{
    AgentRoster, Member, MemberAgent, MemberOrigin, RosterSnapshot, test_roster,
};
use crate::dotagents::{
    DotagentsAgent, DotagentsAgentConfig, DotagentsAgentConnection, DotagentsAgentConnectionType,
    DotagentsAgentRole, DotagentsLayer, DotagentsManifest, DotagentsSource, DotagentsSubAgentPlans,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Adds the plans that survive the merge to `roster` as protocol members.
fn add_protocol_members(
    roster: &AgentRoster,
    plans: &DotagentsSubAgentPlans,
) -> DotagentsTargetMerge {
    let merge = merge_protocol_targets(plans, Some(&roster.snapshot()));
    for plan in merge.accepted.clone() {
        let spec = MemberBuildSpec::from_protocol_plan(&plan, None).unwrap();
        roster
            .add(Member::pending(plan.info, MemberOrigin::Protocol, spec))
            .unwrap();
    }
    merge
}

fn explicit_member(roster: &AgentRoster, id: &str, name: &str) {
    let mut info = crate::test_utils::delegation::agent_info(id);
    info.name = name.to_string();
    roster
        .add(Member::pending(
            info,
            MemberOrigin::Config,
            MemberBuildSpec::new(crate::delegation::roster::AgentId::try_from(id).unwrap()),
        ))
        .unwrap();
}

fn protocol_agent(id: &str) -> DotagentsAgent {
    DotagentsAgent {
        id: id.to_string(),
        name: id.to_string(),
        description: format!("{id} description"),
        enabled: true,
        role: DotagentsAgentRole::DelegationTarget,
        connection: DotagentsAgentConnection::default(),
        capabilities: vec!["read".to_string()],
        config: DotagentsAgentConfig::default(),
        body: format!("{id} body"),
        extensions: BTreeMap::new(),
        source: DotagentsSource::entry(
            DotagentsLayer::Workspace,
            format!(".agents/agents/{id}/agent.md"),
            id,
        ),
        fingerprint: format!("fp-{id}"),
    }
}

fn manifest_with_agents(ids: &[&str]) -> DotagentsManifest {
    let mut manifest = DotagentsManifest::default();
    for id in ids {
        manifest
            .agents
            .insert((*id).to_string(), protocol_agent(id));
    }
    manifest
}

/// Build a plan for `id` attributed to `layer`, so precedence can be tested.
fn plan_in_layer(id: &str, layer: DotagentsLayer) -> crate::dotagents::DotagentsSubAgentPlan {
    let mut manifest = DotagentsManifest::default();
    let mut agent = protocol_agent(id);
    agent.source = DotagentsSource::entry(
        layer,
        format!("{}/agents/{id}/agent.md", layer.as_str()),
        id,
    );
    agent.body = format!("{id} body from {}", layer.as_str());
    manifest.agents.insert(id.to_string(), agent);
    DotagentsSubAgentPlans::from_manifest(&manifest)
        .plans
        .remove(id)
        .expect("plan is produced")
}

fn plans_from(items: Vec<crate::dotagents::DotagentsSubAgentPlan>) -> DotagentsSubAgentPlans {
    let mut plans = DotagentsSubAgentPlans::default();
    for plan in items {
        plans.plans.insert(plan.id.clone(), plan);
    }
    plans
}

#[test]
fn protocol_layer_precedence_prefers_workspace_over_global() {
    let global = plan_in_layer("shared", DotagentsLayer::Global);
    let workspace = plan_in_layer("shared", DotagentsLayer::Workspace);
    assert_eq!(workspace.source.layer, DotagentsLayer::Workspace);
    assert_eq!(workspace.system_prompt, "shared body from workspace");

    let plans = plans_from(vec![workspace.clone()]);
    let merge = merge_protocol_targets(&plans, None);
    assert_eq!(merge.accepted.len(), 1);
    assert_eq!(merge.accepted[0].source.layer, DotagentsLayer::Workspace);

    let diagnostic = protocol_layer_collision(&global, &workspace);
    assert_eq!(diagnostic.code, DotagentsDiagnosticCode::Collision);
    assert!(
        diagnostic.message.contains("global"),
        "{}",
        diagnostic.message
    );
    assert!(
        diagnostic.message.contains("workspace"),
        "{}",
        diagnostic.message
    );
    assert!(diagnostic.source.is_some(), "diagnostic carries provenance");
}

#[tokio::test]
async fn explicit_config_precedence_beats_protocol_layers() {
    let plans = plans_from(vec![plan_in_layer("shared", DotagentsLayer::Workspace)]);
    let (roster, _dir) = test_roster().await;
    explicit_member(&roster, "shared", "Explicit Shared");

    let merge = merge_protocol_targets(&plans, Some(&roster.snapshot()));
    assert!(merge.accepted.is_empty());
    let diagnostic = merge
        .diagnostics
        .iter()
        .find(|d| d.code == DotagentsDiagnosticCode::Collision)
        .expect("explicit collision is diagnosed");
    assert!(diagnostic.message.contains("Explicit Shared"));
    assert!(diagnostic.message.contains("agent.md"));
}

#[test]
fn distinct_protocol_ids_across_layers_both_survive() {
    let plans = plans_from(vec![
        plan_in_layer("global-only", DotagentsLayer::Global),
        plan_in_layer("workspace-only", DotagentsLayer::Workspace),
    ]);
    let merge = merge_protocol_targets(&plans, None);
    let ids: Vec<&str> = merge.accepted.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["global-only", "workspace-only"]);
    assert!(merge.diagnostics.is_empty());
}

#[tokio::test]
async fn listing_does_not_build_members() {
    let manifest = manifest_with_agents(&["reviewer"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let (roster, _dir) = test_roster().await;
    add_protocol_members(&roster, &plans);

    let snapshot = roster.snapshot();
    assert_eq!(snapshot.len(), 1);
    let member = snapshot.get("reviewer").expect("listed");
    assert_eq!(member.info().description, "reviewer description");
    assert_eq!(member.info().capabilities, vec!["read".to_string()]);
    assert_eq!(member.origin(), MemberOrigin::Protocol);
    assert!(!member.is_built());
    assert_eq!(snapshot.built_local().count(), 0);
}

#[tokio::test]
async fn protocol_target_is_built_on_first_delegation() {
    let manifest = manifest_with_agents(&["a", "b", "c"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let (roster, _dir) = test_roster().await;
    add_protocol_members(&roster, &plans);

    let snapshot = roster.snapshot();
    assert_eq!(snapshot.built_local().count(), 0);

    let first = match roster.resolve(snapshot.get("a").unwrap()).expect("builds") {
        MemberAgent::Local(handle) => handle,
        _ => panic!("local member"),
    };
    assert_eq!(roster.snapshot().built_local().count(), 1);
    assert!(!snapshot.get("b").unwrap().is_built());

    // Later resolutions reuse the one handle.
    for _ in 0..3 {
        match roster.resolve(snapshot.get("a").unwrap()).unwrap() {
            MemberAgent::Local(again) => assert!(Arc::ptr_eq(&first, &again)),
            _ => panic!("local member"),
        }
    }
    assert_eq!(roster.snapshot().built_local().count(), 1);
}

#[tokio::test]
async fn explicit_delegate_collides_with_a_protocol_target() {
    let manifest = manifest_with_agents(&["shared", "protocol-only"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let (roster, _dir) = test_roster().await;
    explicit_member(&roster, "shared", "Explicit Shared");

    let merge = add_protocol_members(&roster, &plans);

    let snapshot = roster.snapshot();
    let shared = snapshot.get("shared").expect("explicit visible");
    assert_eq!(shared.info().name, "Explicit Shared");
    assert_eq!(shared.origin(), MemberOrigin::Config);
    assert_eq!(
        snapshot.get("protocol-only").unwrap().origin(),
        MemberOrigin::Protocol
    );
    let diagnostic = merge
        .diagnostics
        .iter()
        .find(|d| d.code == DotagentsDiagnosticCode::Collision)
        .expect("collision is diagnosed");
    assert!(diagnostic.message.contains("shared"));
    assert!(diagnostic.message.contains("Explicit Shared"));
    assert!(diagnostic.source.is_some(), "collision carries provenance");
}

#[tokio::test]
async fn external_connection_cannot_be_represented_safely() {
    // A stdio/executable profile points at a command that would create a
    // marker file if it were ever launched. Nothing may execute it.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let marker = dir.path().join("LAUNCHED");

    let mut manifest = DotagentsManifest::default();
    for (id, connection_type, declared) in [
        (
            "stdio-target",
            DotagentsAgentConnectionType::Stdio,
            Some("stdio"),
        ),
        (
            "unknown-target",
            DotagentsAgentConnectionType::Unknown,
            Some("carrier-pigeon"),
        ),
    ] {
        let mut agent = protocol_agent(id);
        agent.connection = DotagentsAgentConnection {
            connection_type,
            declared_type: declared.map(str::to_string),
            command: Some(format!("touch {}", marker.display())),
        };
        manifest.agents.insert(id.to_string(), agent);
    }

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    assert_eq!(plans.plans.len(), 2);
    assert!(
        !marker.exists(),
        "planning must not launch the declared command"
    );
    for id in ["stdio-target", "unknown-target"] {
        let plan = plans.get(id).expect("profile is retained for inspection");
        let diagnostic = plan
            .diagnostics
            .iter()
            .find(|d| d.code == DotagentsDiagnosticCode::UnsupportedTransport)
            .expect("unsupported connection is diagnosed");
        assert!(diagnostic.message.contains(id), "{}", diagnostic.message);
        assert!(diagnostic.is_error());
        assert!(
            diagnostic.message.contains("never launched"),
            "{}",
            diagnostic.message
        );
    }

    // The roster never receives them, so nothing can be built or launched.
    let (roster, _dir) = test_roster().await;
    add_protocol_members(&roster, &plans);
    assert!(roster.snapshot().is_empty());
    assert!(
        !marker.exists(),
        "no path may launch the declared command: marker {} exists",
        marker.display()
    );
}

#[tokio::test]
async fn collision_resolution_merges_before_explicit_precedence() {
    let manifest = manifest_with_agents(&["a", "b"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let (roster, _dir) = test_roster().await;
    explicit_member(&roster, "a", "A explicit");

    let merge = merge_protocol_targets(&plans, Some(&roster.snapshot()));
    assert_eq!(merge.accepted.len(), 1);
    assert_eq!(merge.accepted[0].id, "b");
    assert_eq!(merge.diagnostics.len(), 1);
    assert_eq!(
        merge.diagnostics[0].code,
        DotagentsDiagnosticCode::Collision
    );
}

#[test]
fn an_empty_snapshot_has_no_collisions() {
    let manifest = manifest_with_agents(&["a"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let merge = merge_protocol_targets(&plans, Some(&RosterSnapshot::default()));
    assert_eq!(merge.accepted.len(), 1);
    let _ = BTreeMap::<String, String>::new();
}

#[test]
fn profile_is_disabled() {
    let mut manifest = manifest_with_agents(&["off", "on"]);
    manifest.agents.get_mut("off").unwrap().enabled = false;
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    assert!(plans.get("off").is_none(), "a disabled profile has no plan");
    let merge = merge_protocol_targets(&plans, None);
    let ids: Vec<&str> = merge.accepted.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["on"]);
}
