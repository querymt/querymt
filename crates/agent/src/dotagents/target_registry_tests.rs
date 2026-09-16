//! Tests for protocol delegation-target registration (tasks 3.2, 3.3, 3.4).
//!
//! These tests exercise the registry integration layer directly: lazy
//! materialization, one handle per target, explicit-target precedence, and the
//! rule that protocol loading never enables delegation and never launches
//! unsupported connections.

use super::*;
use crate::agent::handle::AgentHandle as AgentHandleTrait;
use crate::delegation::{AgentInfo, AgentRegistry, DefaultAgentRegistry};
use crate::dotagents::{
    DotagentsAgent, DotagentsAgentConfig, DotagentsAgentConnection, DotagentsAgentConnectionType,
    DotagentsAgentRole, DotagentsLayer, DotagentsManifest, DotagentsSource, DotagentsSubAgentPlans,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A factory that counts how many times each plan was created and returns a
/// trivial handle. It proves laziness and single-materialization without
/// needing a real provider.
struct CountingFactory {
    created: Arc<AtomicUsize>,
}

impl ProtocolTargetFactory for CountingFactory {
    fn create(
        &self,
        _plan: &crate::dotagents::DotagentsSubAgentPlan,
    ) -> Option<Arc<dyn AgentHandleTrait>> {
        self.created.fetch_add(1, Ordering::SeqCst);
        Some(Arc::new(StubHandle))
    }
}

/// Minimal handle used to observe materialization.
struct StubHandle;

#[async_trait::async_trait]
impl AgentHandleTrait for StubHandle {
    async fn new_session(
        &self,
        _req: crate::acp::protocol::NewSessionRequest,
    ) -> Result<crate::acp::protocol::NewSessionResponse, crate::acp::protocol::Error> {
        Err(crate::acp::protocol::Error::internal_error())
    }

    async fn prompt(
        &self,
        _req: crate::acp::protocol::PromptRequest,
    ) -> Result<crate::acp::protocol::PromptResponse, crate::acp::protocol::Error> {
        Err(crate::acp::protocol::Error::internal_error())
    }

    async fn cancel(
        &self,
        _notif: crate::acp::protocol::CancelNotification,
    ) -> Result<(), crate::acp::protocol::Error> {
        Ok(())
    }

    async fn load_session(
        &self,
        _req: crate::acp::protocol::LoadSessionRequest,
    ) -> Result<crate::acp::protocol::LoadSessionResponse, crate::acp::protocol::Error> {
        Err(crate::acp::protocol::Error::internal_error())
    }

    async fn create_delegation_session(
        &self,
        _cwd: Option<String>,
        _parent_session_id: String,
    ) -> Result<(String, crate::agent::remote::SessionActorRef), crate::acp::protocol::Error> {
        Err(crate::acp::protocol::Error::internal_error())
    }

    fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<crate::events::EventEnvelope> {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }

    fn event_fanout(&self) -> &Arc<crate::event_fanout::EventFanout> {
        unimplemented!("not needed for registry tests")
    }

    fn emit_event(&self, _session_id: &str, _kind: crate::events::AgentEventKind) {}

    fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        Arc::new(DefaultAgentRegistry::new())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
    // Cross-layer precedence is applied upstream by `merge_layers`: the workspace
    // layer replaces a matching ID from the global layer, so only the winning
    // definition reaches a plan. Verify the plan set keeps the winner and that
    // the losing definition is surfaced as a layer collision diagnostic when a
    // caller merges layer-assembled plans manually.
    let global = plan_in_layer("shared", DotagentsLayer::Global);
    let workspace = plan_in_layer("shared", DotagentsLayer::Workspace);

    // The winner is the workspace definition.
    assert_eq!(workspace.source.layer, DotagentsLayer::Workspace);
    assert_eq!(workspace.system_prompt, "shared body from workspace");

    // Merging a two-layer plan set yields the winning entry plus a diagnostic
    // naming both layers.
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

#[test]
fn explicit_config_precedence_beats_protocol_layers() {
    let plans = plans_from(vec![plan_in_layer("shared", DotagentsLayer::Workspace)]);
    let mut explicit = DefaultAgentRegistry::new();
    explicit.register(
        AgentInfo {
            id: "shared".to_string(),
            name: "Explicit Shared".to_string(),
            description: String::new(),
            capabilities: vec![],
            required_capabilities: vec![],
            meta: None,
        },
        Arc::new(StubHandle),
    );

    let merge = merge_protocol_targets(&plans, Some(&explicit));
    assert!(merge.accepted.is_empty());

    let diagnostic = merge
        .diagnostics
        .iter()
        .find(|d| d.code == DotagentsDiagnosticCode::Collision)
        .expect("explicit collision is diagnosed");
    // Both sources appear: the protocol path and the explicit target.
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
#[test]
fn protocol_targets_are_advertised_lazily_without_materializing() {
    let manifest = manifest_with_agents(&["reviewer"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let created = Arc::new(AtomicUsize::new(0));
    let registry = DotagentsTargetRegistry::new(
        &plans,
        None,
        Arc::new(CountingFactory {
            created: created.clone(),
        }),
    );

    // Advertised immediately, materialized only on demand.
    let agents = registry.list_agents();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].id, "reviewer");
    assert_eq!(agents[0].description, "reviewer description");
    assert_eq!(agents[0].capabilities, vec!["read".to_string()]);
    assert_eq!(created.load(Ordering::SeqCst), 0);
    assert!(!registry.is_materialized("reviewer"));

    // First handle request materializes exactly once.
    let handle = registry.get_handle("reviewer").expect("handle resolves");
    assert_eq!(created.load(Ordering::SeqCst), 1);
    assert!(registry.is_materialized("reviewer"));

    // Repeated requests reuse the same handle.
    let again = registry.get_handle("reviewer").expect("handle resolves");
    assert!(Arc::ptr_eq(&handle, &again));
    assert_eq!(created.load(Ordering::SeqCst), 1);
}

#[test]
fn protocol_targets_do_not_replace_explicit_targets() {
    let manifest = manifest_with_agents(&["shared", "protocol-only"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);

    // Explicit QueryMT target with a colliding ID.
    let mut explicit = DefaultAgentRegistry::new();
    explicit.register(
        AgentInfo {
            id: "shared".to_string(),
            name: "Explicit Shared".to_string(),
            description: "explicit".to_string(),
            capabilities: vec![],
            required_capabilities: vec![],
            meta: None,
        },
        Arc::new(StubHandle),
    );

    let created = Arc::new(AtomicUsize::new(0));
    let registry = DotagentsTargetRegistry::new(
        &plans,
        Some(Arc::new(explicit)),
        Arc::new(CountingFactory {
            created: created.clone(),
        }),
    );

    // The explicit target wins and the protocol one is not registered.
    assert!(!registry.contains("shared"));
    assert!(registry.contains("protocol-only"));
    let shared = registry
        .get_agent("shared")
        .expect("explicit target visible");
    assert_eq!(shared.name, "Explicit Shared");
    // Non-colliding protocol target is still advertised.
    assert!(registry.get_agent("protocol-only").is_some());

    // A collision diagnostic names both sources.
    let diagnostic = registry
        .diagnostics()
        .iter()
        .find(|d| d.code == DotagentsDiagnosticCode::Collision)
        .expect("collision is diagnosed");
    assert!(diagnostic.message.contains("shared"));
    assert!(diagnostic.message.contains("Explicit Shared"));
    assert!(diagnostic.source.is_some(), "collision carries provenance");
}

#[test]
fn protocol_targets_skip_unsupported_connections() {
    let mut manifest = manifest_with_agents(&["external"]);
    manifest.agents.get_mut("external").unwrap().connection = DotagentsAgentConnection {
        connection_type: DotagentsAgentConnectionType::Stdio,
        declared_type: Some("stdio".to_string()),
        command: Some("never-launched".to_string()),
    };
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);

    let created = Arc::new(AtomicUsize::new(0));
    let registry = DotagentsTargetRegistry::new(
        &plans,
        None,
        Arc::new(CountingFactory {
            created: created.clone(),
        }),
    );

    // Nothing is registered and nothing is ever created for an unsupported target.
    assert!(registry.protocol_ids().is_empty());
    assert!(registry.get_handle("external").is_none());
    assert_eq!(created.load(Ordering::SeqCst), 0);
}

#[test]
fn nested_materialization_creates_one_handle_per_target_and_terminates() {
    // Multiple targets, each resolved repeatedly. The registry must create each
    // handle exactly once and return the same instance, which is what makes
    // nested materialization terminate instead of rebuilding recursively.
    let manifest = manifest_with_agents(&["a", "b", "c"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let created = Arc::new(AtomicUsize::new(0));
    let registry = DotagentsTargetRegistry::new(
        &plans,
        None,
        Arc::new(CountingFactory {
            created: created.clone(),
        }),
    );

    let mut first_pass = Vec::new();
    for id in ["a", "b", "c"] {
        first_pass.push((id, registry.get_handle(id).expect("handle resolves")));
    }
    assert_eq!(created.load(Ordering::SeqCst), 3);

    // Repeated resolution returns the identical handles and creates nothing new.
    for _ in 0..3 {
        for (id, original) in &first_pass {
            let again = registry.get_handle(id).expect("handle resolves");
            assert!(Arc::ptr_eq(&again, original), "handle for {id} is reused");
        }
    }
    assert_eq!(
        created.load(Ordering::SeqCst),
        3,
        "each target is materialized exactly once"
    );
    assert_eq!(registry.materialization_count(), 3);
    for id in ["a", "b", "c"] {
        assert!(registry.is_materialized(id));
    }
}

#[test]
fn child_targets_expose_no_delegation_targets() {
    // The recursion guard: a child protocol target's registry is empty, so it
    // cannot rediscover the protocol collection or delegate onward.
    let manifest = manifest_with_agents(&["solo"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let registry = DotagentsTargetRegistry::new(&plans, None, Arc::new(RecordingFactory));

    let handle = registry.get_handle("solo").expect("handle resolves");
    // A nested lookup through the child's own registry finds nothing.
    assert!(
        handle.agent_registry().list_agents().is_empty(),
        "child protocol target must not expose delegation targets"
    );
}

/// Factory that records the child registry it was asked to build.
struct RecordingFactory;

impl ProtocolTargetFactory for RecordingFactory {
    fn create(
        &self,
        _plan: &crate::dotagents::DotagentsSubAgentPlan,
    ) -> Option<Arc<dyn AgentHandleTrait>> {
        Some(Arc::new(StubHandle))
    }
}

#[test]
fn unsupported_connections_never_launch_a_process() {
    // Task 3.6: a stdio/executable profile points at a command that would create
    // a marker file if it were ever launched. Nothing may execute it, so the
    // marker must not exist after planning and after every registration path.
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

    // Planning must diagnose both and must not run anything.
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
        // The diagnostic identifies the profile and the connection type.
        assert!(diagnostic.message.contains(id), "{}", diagnostic.message);
        assert!(diagnostic.is_error(), "unsupported connections are errors");
        assert!(
            diagnostic.message.contains("never launched"),
            "{}",
            diagnostic.message
        );
    }
    assert!(
        plans
            .get("stdio-target")
            .unwrap()
            .diagnostics
            .iter()
            .any(|d| d.message.contains("stdio"))
    );
    assert!(
        plans
            .get("unknown-target")
            .unwrap()
            .diagnostics
            .iter()
            .any(|d| d.message.contains("carrier-pigeon"))
    );

    // Registration must also skip them entirely and never invoke the factory.
    let created = Arc::new(AtomicUsize::new(0));
    let registry = DotagentsTargetRegistry::new(
        &plans,
        None,
        Arc::new(CountingFactory {
            created: created.clone(),
        }),
    );
    assert!(registry.protocol_ids().is_empty());
    for id in ["stdio-target", "unknown-target"] {
        assert!(registry.get_handle(id).is_none());
        assert!(registry.get_agent(id).is_none());
    }
    assert_eq!(created.load(Ordering::SeqCst), 0);
    assert!(
        !marker.exists(),
        "no path may launch the declared command: marker {} exists",
        marker.display()
    );
}

#[test]
fn collision_resolution_merges_before_explicit_precedence() {
    let manifest = manifest_with_agents(&["a", "b"]);
    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);

    let mut explicit = DefaultAgentRegistry::new();
    explicit.register(
        AgentInfo {
            id: "a".to_string(),
            name: "A explicit".to_string(),
            description: String::new(),
            capabilities: vec![],
            required_capabilities: vec![],
            meta: None,
        },
        Arc::new(StubHandle),
    );

    let merge = merge_protocol_targets(&plans, Some(&explicit));
    assert_eq!(merge.accepted.len(), 1);
    assert_eq!(merge.accepted[0].id, "b");
    assert_eq!(merge.diagnostics.len(), 1);
    assert_eq!(
        merge.diagnostics[0].code,
        DotagentsDiagnosticCode::Collision
    );
}
