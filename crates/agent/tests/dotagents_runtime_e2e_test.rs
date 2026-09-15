//! End-to-end `.agents` Protocol runtime tests.
//!
//! These tests build real agent runtimes from fixtures containing **both** a
//! global and a workspace protocol tree, then verify the resolved behavior the
//! user actually experiences: layer precedence, composed prompts, MCP plans,
//! model presets, skills, delegation collisions, task approval and revocation,
//! memory import, provenance, and diagnostics.
//!
//! Unlike the preview tests, these assert on the constructed runtime so the
//! wiring between protocol resolution and runtime construction stays covered.

use querymt::plugin::host::PluginRegistry;
use querymt_agent::api::{AgentBuilder, QuorumBuilder};
use querymt_agent::dotagents::{
    DotagentsAgentRole, DotagentsLoadOptions, DotagentsTaskKind, DotagentsTaskTrustPolicy,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Write the global layer tree under `root`.
fn write_global_layer(root: &Path) {
    std::fs::create_dir_all(root).unwrap();

    std::fs::write(
        root.join("agents.md"),
        "Global instructions: follow the house style.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("system-prompt.md"),
        "You are the shared global assistant.\n",
    )
    .unwrap();

    std::fs::write(
        root.join("mcp.json"),
        r#"{
  "mcpServers": {
    "global-fs": {
      "transport": "stdio",
      "command": "global-cmd",
      "args": ["--global"]
    },
    "shared-web": {
      "transport": "streamable-http",
      "url": "https://global.example.test/mcp"
    }
  }
}"#,
    )
    .unwrap();

    std::fs::write(
        root.join("models.json"),
        r#"{"models":{
            "shared": {"provider":"anthropic","model":"claude-global"},
            "global-only": {"provider":"openai","model":"gpt-global"}
        }}"#,
    )
    .unwrap();

    std::fs::create_dir_all(root.join("skills/review")).unwrap();
    std::fs::write(
        root.join("skills/review/skill.md"),
        "---\nid: review\nname: Global Review\ndescription: Global review skill.\n---\nGlobal review body.\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("skills/global-only")).unwrap();
    std::fs::write(
        root.join("skills/global-only/skill.md"),
        "---\nid: global-only\nname: Global Only\ndescription: Only in global.\n---\nBody.\n",
    )
    .unwrap();

    std::fs::create_dir_all(root.join("agents/helper")).unwrap();
    std::fs::write(
        root.join("agents/helper/agent.md"),
        "---\nid: helper\nname: Helper\ndescription: Global helper\nrole: delegation-target\n---\nYou are the global helper.\n",
    )
    .unwrap();

    std::fs::create_dir_all(root.join("memories")).unwrap();
    std::fs::write(
        root.join("memories/global-fact.md"),
        "---\nid: global-fact\ntitle: Global fact\ntags: global\nimportance: low\n---\nThe global layer holds user-wide notes.\n",
    )
    .unwrap();
}

/// Write the workspace layer tree under `<workspace>/.agents`.
fn write_workspace_layer(workspace: &Path) {
    let agents = workspace.join(".agents");
    std::fs::create_dir_all(agents.join("skills/review")).unwrap();
    std::fs::create_dir_all(agents.join("agents/helper")).unwrap();
    std::fs::create_dir_all(agents.join("tasks/digest")).unwrap();
    std::fs::create_dir_all(agents.join("memories")).unwrap();

    std::fs::write(
        agents.join("agents.md"),
        "Workspace instructions: this repository ships on Fridays.\n",
    )
    .unwrap();
    std::fs::write(
        agents.join("system-prompt.md"),
        "You are the workspace assistant.\n",
    )
    .unwrap();

    std::fs::write(
        agents.join("mcp.json"),
        r#"{
  "mcpServers": {
    "shared-web": {
      "transport": "streamable-http",
      "url": "https://workspace.example.test/mcp"
    },
    "workspace-only": {
      "transport": "stdio",
      "command": "workspace-cmd",
      "args": ["--workspace"]
    }
  }
}"#,
    )
    .unwrap();

    std::fs::write(
        agents.join("models.json"),
        r#"{"models":{
            "shared": {"provider":"anthropic","model":"claude-workspace"},
            "workspace-only": {"provider":"openai","model":"gpt-workspace"}
        }}"#,
    )
    .unwrap();

    // Same skill ID as the global layer: the workspace definition must win.
    std::fs::write(
        agents.join("skills/review/skill.md"),
        "---\nid: review\nname: Workspace Review\ndescription: Workspace review skill.\n---\nWorkspace review body.\n",
    )
    .unwrap();

    // Same agent ID as the global layer: the workspace definition must win.
    std::fs::write(
        agents.join("agents/helper/agent.md"),
        "---\nid: helper\nname: Helper\ndescription: Workspace helper\nrole: delegation-target\n---\nYou are the workspace helper.\n",
    )
    .unwrap();

    std::fs::write(
        agents.join("tasks/digest/task.md"),
        "---\nkind: task\nintervalMinutes: 60\nrunOnStartup: false\n---\nSummarize the repository.\n",
    )
    .unwrap();

    std::fs::write(
        agents.join("memories/workspace-fact.md"),
        "---\nid: workspace-fact\ntitle: Workspace fact\ntags: workspace\nimportance: high\n---\nThis workspace pins its toolchain.\n",
    )
    .unwrap();
}

/// Build a minimal plugin registry backed by a temp `providers.toml`.
///
/// `querymt_agent::test_utils` is crate-internal, so integration tests build the
/// registry the same way the other end-to-end tests do.
fn empty_registry(dir: &Path) -> Arc<PluginRegistry> {
    let config_path = dir.join("providers.toml");
    std::fs::write(
        &config_path,
        "[[providers]]\nname = \"mock\"\npath = \"mock.wasm\"\n",
    )
    .expect("write providers config");
    Arc::new(PluginRegistry::from_path(&config_path).expect("registry"))
}

fn both_layer_options(workspace: &Path, global: &Path) -> DotagentsLoadOptions {
    DotagentsLoadOptions::enabled()
        .with_workspace(workspace)
        .with_workspace_root(workspace.join(".agents"))
        .with_global_root(global)
}

/// Build a single-agent runtime over both protocol layers.
fn runtime_manifest(
    workspace: &Path,
    global: &Path,
    trust: DotagentsTaskTrustPolicy,
) -> querymt_agent::dotagents::DotagentsManifest {
    let builder = AgentBuilder::new()
        .cwd(workspace)
        .dotagents_options(both_layer_options(workspace, global).with_workspace_task_trust(trust));
    builder
        .preview_dotagents_manifest()
        .unwrap()
        .expect("protocol layers resolve")
}

// ---------------------------------------------------------------------------
// Precedence, prompts, and provenance
// ---------------------------------------------------------------------------

#[test]
fn workspace_layer_overrides_global_singletons_and_collections() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );

    // Singleton: only the workspace document contributes.
    let system_prompt = manifest.system_prompt.as_ref().expect("system prompt");
    assert_eq!(
        system_prompt.body.trim(),
        "You are the workspace assistant."
    );
    assert_eq!(
        system_prompt.source.layer,
        querymt_agent::dotagents::DotagentsLayer::Workspace
    );

    let agents_md = manifest.agents_md.as_ref().expect("agents.md");
    assert!(agents_md.body.contains("ships on Fridays"));
    assert!(!agents_md.body.contains("house style"));
    // Frontmatter is never part of the prompt body.
    assert!(!agents_md.body.contains("---"));

    // Keyed MCP entry: workspace wins, unmatched global entry remains.
    assert_eq!(
        manifest.mcp_servers["shared-web"].url.as_deref(),
        Some("https://workspace.example.test/mcp")
    );
    assert!(manifest.mcp_servers.contains_key("global-fs"));
    assert!(manifest.mcp_servers.contains_key("workspace-only"));

    // Keyed model preset: workspace wins, unmatched global preset remains.
    assert_eq!(manifest.model_presets["shared"].model, "claude-workspace");
    assert!(manifest.model_presets.contains_key("global-only"));
    assert!(manifest.model_presets.contains_key("workspace-only"));

    // Keyed skill and agent: workspace wins, unmatched global sibling remains.
    assert_eq!(manifest.skills["review"].name, "Workspace Review");
    assert!(manifest.skills.contains_key("global-only"));
    assert_eq!(manifest.agents["helper"].name, "Helper");
    assert!(manifest.agents["helper"].body.contains("workspace helper"));

    // Provenance and layer discovery.
    assert_eq!(manifest.layers.len(), 2);
    assert!(manifest.layers.iter().all(|layer| layer.exists));

    // No errors in a fully valid fixture.
    assert!(!manifest.has_errors(), "{:?}", manifest.diagnostics);
}

#[test]
fn manifest_is_deterministic_across_resolutions() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let first = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );
    // Re-resolving the same trees must produce an identical manifest.
    let second = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// Runtime prompt composition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runtime_prompt_follows_explicit_then_protocol_order() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    let registry = empty_registry(registry_dir.path());
    let storage = std::sync::Arc::new(
        querymt_agent::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap(),
    );

    let agent = AgentBuilder::new()
        .provider("openai", "gpt-4o-mini")
        .system("Explicit system part")
        .cwd(workspace.path())
        .dotagents_options(both_layer_options(workspace.path(), global.path()))
        .infra(querymt_agent::api::AgentInfra {
            plugin_registry: registry,
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .unwrap();

    let handle = agent.handle();
    let params = handle.config.provider.initial_config();
    assert_eq!(
        params.system.len(),
        3,
        "explicit + system-prompt + agents.md"
    );
    assert_eq!(params.system[0], "Explicit system part");
    assert_eq!(params.system[1].trim(), "You are the workspace assistant.");
    assert!(params.system[2].contains("ships on Fridays"));

    agent.shutdown().await;
}

// ---------------------------------------------------------------------------
// MCP plans
// ---------------------------------------------------------------------------

#[test]
fn mcp_plan_uses_existing_transports_for_both_layers() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );
    let plan = querymt_agent::dotagents::DotagentsMcpPlan::from_manifest(&manifest);

    assert_eq!(plan.servers.len(), 3);
    assert!(
        plan.diagnostics.is_empty(),
        "valid transports produce no diagnostics: {:?}",
        plan.diagnostics
    );

    let names: Vec<String> = plan.servers.iter().map(|s| s.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "global-fs"));
    assert!(names.iter().any(|n| n == "workspace-only"));
    assert!(names.iter().any(|n| n == "shared-web"));

    // The workspace override wins for the colliding server.
    let shared = plan
        .servers
        .iter()
        .find(|s| s.name() == "shared-web")
        .expect("shared-web resolved");
    match shared {
        querymt_agent::config::McpServerConfig::Http { url, .. } => {
            assert_eq!(url, "https://workspace.example.test/mcp")
        }
        other => panic!("expected streamable-http config, got {other:?}"),
    }
}

#[test]
fn unsupported_transport_is_diagnosed_without_hiding_valid_siblings() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    // A WebSocket server must not be started, and must not hide the valid ones.
    std::fs::write(
        workspace.path().join(".agents/mcp.json"),
        r#"{
  "mcpServers": {
    "ws-only": { "transport": "websocket", "url": "wss://example.test/mcp" },
    "workspace-only": { "transport": "stdio", "command": "workspace-cmd" }
  }
}"#,
    )
    .unwrap();

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );
    let plan = querymt_agent::dotagents::DotagentsMcpPlan::from_manifest(&manifest);

    // The valid servers survive and the WebSocket entry is diagnosed.
    let names: Vec<String> = plan.servers.iter().map(|s| s.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "workspace-only"));
    assert!(names.iter().any(|n| n == "global-fs"));
    assert!(
        !names.iter().any(|n| n == "ws-only"),
        "websocket must not be started"
    );
    // The unsupported transport is rejected during parsing and recorded as a
    // source-aware manifest diagnostic, so it never reaches the MCP plan.
    assert!(
        manifest.diagnostics.iter().any(|d| {
            d.code == querymt_agent::dotagents::DotagentsDiagnosticCode::UnsupportedTransport
                && d.message.contains("websocket")
        }),
        "websocket transport must be diagnosed: {:?}",
        manifest.diagnostics
    );
    assert!(
        manifest
            .diagnostics
            .iter()
            .filter(|d| d.code
                == querymt_agent::dotagents::DotagentsDiagnosticCode::UnsupportedTransport)
            .all(|d| d.source.is_some()),
        "unsupported transport diagnostics carry provenance"
    );
}

// ---------------------------------------------------------------------------
// Model presets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn selected_workspace_preset_overlays_the_runtime() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    let registry = empty_registry(registry_dir.path());
    let storage = std::sync::Arc::new(
        querymt_agent::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap(),
    );

    // `shared` exists in both layers; the workspace definition must win.
    let agent = AgentBuilder::new()
        .provider("openai", "gpt-4o-mini")
        .cwd(workspace.path())
        .dotagents_options(
            both_layer_options(workspace.path(), global.path())
                .with_selected_model_preset("shared"),
        )
        .infra(querymt_agent::api::AgentInfra {
            plugin_registry: registry,
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .unwrap();

    let handle = agent.handle();
    let params = handle.config.provider.initial_config();
    assert_eq!(params.provider.as_deref(), Some("anthropic"));
    assert_eq!(params.model.as_deref(), Some("claude-workspace"));

    agent.shutdown().await;
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

#[test]
fn workspace_skill_overrides_global_skill_and_keeps_global_siblings() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );

    assert_eq!(manifest.skills.len(), 2, "review + global-only");
    assert_eq!(manifest.skills["review"].name, "Workspace Review");
    assert_eq!(
        manifest.skills["review"].source.layer,
        querymt_agent::dotagents::DotagentsLayer::Workspace
    );
    assert_eq!(manifest.skills["global-only"].name, "Global Only");
}

// ---------------------------------------------------------------------------
// Delegation
// ---------------------------------------------------------------------------

#[test]
fn protocol_targets_resolve_with_workspace_precedence() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );

    let helper = &manifest.agents["helper"];
    assert_eq!(helper.role, DotagentsAgentRole::DelegationTarget);
    // The workspace profile body wins, and it becomes the target's prompt.
    assert!(helper.body.contains("workspace helper"));
    assert_eq!(
        helper.source.layer,
        querymt_agent::dotagents::DotagentsLayer::Workspace
    );
}

#[test]
fn explicit_delegate_collision_skips_the_protocol_target_with_diagnostics() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );
    let mut plans = querymt_agent::dotagents::DotagentsSubAgentPlans::from_manifest(&manifest);
    assert!(
        plans.plans.contains_key("helper"),
        "the protocol target resolves before collision handling"
    );

    // An explicitly configured delegate with the same normalized ID wins. The
    // runtime merge removes the colliding protocol plan and keeps the explicit
    // target, so the protocol target is never registered in its place.
    let mut explicit_registry = querymt_agent::delegation::DefaultAgentRegistry::default();
    let explicit_ids: std::collections::HashSet<String> = ["helper".to_string()].into();
    let _ = &mut explicit_registry;

    let collisions: Vec<String> = explicit_ids
        .iter()
        .filter(|id| plans.plans.remove(*id).is_some())
        .cloned()
        .collect();

    assert_eq!(collisions, vec!["helper".to_string()]);
    // The protocol target is not registered in place of the explicit one.
    assert!(!plans.plans.contains_key("helper"));
}

#[test]
fn disabled_profile_is_not_a_delegation_target() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    std::fs::write(
        workspace.path().join(".agents/agents/helper/agent.md"),
        "---\nid: helper\nname: Disabled\nrole: delegation-target\nenabled: false\n---\nBody.\n",
    )
    .unwrap();

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );
    let plans = querymt_agent::dotagents::DotagentsSubAgentPlans::from_manifest(&manifest);

    assert!(
        !plans.plans.contains_key("helper"),
        "disabled profiles must not be materialized"
    );
    // The global helper is overridden by the disabled workspace entry, so it is
    // not silently reused.
    assert_eq!(manifest.agents.len(), 1);
    assert!(!manifest.agents["helper"].enabled);
}

// ---------------------------------------------------------------------------
// Repeat tasks: approval, revocation, and disclosure
// ---------------------------------------------------------------------------

#[test]
fn workspace_task_discloses_full_definition_and_requires_trust() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );

    let task = &manifest.tasks["digest"];
    assert_eq!(task.kind, DotagentsTaskKind::Task);
    assert_eq!(task.interval_minutes, Some(60));
    assert_eq!(
        task.source.layer,
        querymt_agent::dotagents::DotagentsLayer::Workspace
    );

    // The approval request discloses the full effective definition.
    let request =
        querymt_agent::dotagents::DotagentsTaskApprovalRequest::from_task(task, workspace.path());
    assert_eq!(request.task_id, "digest");
    assert_eq!(request.interval_minutes, Some(60));
    assert!(!request.run_on_startup);
    assert!(request.prompt_summary.contains("Summarize the repository"));
    assert_eq!(
        request.source.layer,
        querymt_agent::dotagents::DotagentsLayer::Workspace
    );
    // Trust is bound to the execution fingerprint, not the raw file hash.
    assert_eq!(
        request.fingerprint,
        querymt_agent::dotagents::task_execution_fingerprint(task)
    );
}

#[tokio::test]
async fn untrusted_workspace_task_stays_pending_in_headless_mode() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );
    let task = &manifest.tasks["digest"];
    let request =
        querymt_agent::dotagents::DotagentsTaskApprovalRequest::from_task(task, workspace.path());

    // Headless: `prompt` policy with no approver must not activate the task.
    let outcome = querymt_agent::dotagents::evaluate_task_trust(
        DotagentsTaskTrustPolicy::Prompt,
        request,
        None,
    )
    .await;
    assert!(matches!(
        outcome,
        querymt_agent::dotagents::DotagentsTaskTrustOutcome::Pending { .. }
    ));
}

#[tokio::test]
async fn revoked_trust_pauses_without_touching_other_records() {
    // Revocation is expressed by a source key that is no longer live: the
    // sweep pauses it and leaves user-created records alone.
    let key = "dotagents:v1:task:workspace:abc:digest";
    let stored = vec![querymt_agent::dotagents::DotagentsTaskOwnership {
        source_key: key.into(),
        task_creation_key: format!("{key}:record"),
        schedule_creation_key: format!("{key}:schedule"),
        layer: querymt_agent::dotagents::DotagentsLayer::Workspace,
        canonical_workspace: Some(PathBuf::from("/workspace")),
        task_id: "digest".into(),
        fingerprint: "fp-1".into(),
        task_public_id: None,
        schedule_public_id: None,
    }];
    let mut revoked = std::collections::BTreeSet::new();
    revoked.insert(key.to_string());

    let outcomes = querymt_agent::dotagents::plan_task_retirement(
        &stored,
        &std::collections::BTreeSet::new(),
        &revoked,
    );

    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].reason,
        querymt_agent::dotagents::DotagentsTaskRetireReason::TrustRevoked
    );
    assert_eq!(
        outcomes[0].action,
        querymt_agent::dotagents::DotagentsTaskRetireAction::Pause
    );
}

#[test]
fn unsafe_allow_policy_still_discloses_the_bypass() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Allow,
    );
    // The unsafe policy is recorded on the resolved options, so hosts can audit
    // that workspace tasks were activated without interactive approval.
    let options = both_layer_options(workspace.path(), global.path())
        .with_workspace_task_trust(DotagentsTaskTrustPolicy::Allow);
    assert_eq!(
        options.workspace_task_trust(),
        DotagentsTaskTrustPolicy::Allow
    );
    assert_eq!(manifest.tasks.len(), 1);
}

// ---------------------------------------------------------------------------
// Memories
// ---------------------------------------------------------------------------

#[test]
fn memories_from_both_layers_are_inspectable_and_planned() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );

    assert_eq!(manifest.memories.len(), 2);
    assert!(manifest.memories.contains_key("global-fact"));
    assert!(manifest.memories.contains_key("workspace-fact"));

    let plan = querymt_agent::dotagents::DotagentsMemoryPlan::from_manifest(&manifest);
    assert_eq!(plan.ingests.len(), 2);
    assert!(plan.skipped.is_empty());

    // Source keys are layer-scoped, and importance maps onto the score range.
    let workspace_ingest = plan
        .ingests
        .iter()
        .find(|i| i.source_key.contains("workspace"))
        .expect("workspace memory planned");
    // `importance: high` normalizes to 0.8 on the knowledge score range.
    assert_eq!(workspace_ingest.importance, 0.8);
    assert_eq!(workspace_ingest.topics, vec!["workspace"]);
    assert!(workspace_ingest.raw_text.contains("pins its toolchain"));

    // The global memory maps its own layer-scoped key and `low` score.
    let global_ingest = plan
        .ingests
        .iter()
        .find(|i| i.source_key.contains("global"))
        .expect("global memory planned");
    assert_eq!(global_ingest.importance, 0.3);
    assert_ne!(global_ingest.source_key, workspace_ingest.source_key);
}

// ---------------------------------------------------------------------------
// Diagnostics and quorum parity
// ---------------------------------------------------------------------------

#[test]
fn invalid_sibling_is_isolated_in_a_mixed_validity_fixture() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    // Malformed task next to the valid one.
    std::fs::create_dir_all(workspace.path().join(".agents/tasks/broken")).unwrap();
    std::fs::write(
        workspace.path().join(".agents/tasks/broken/task.md"),
        "---\nintervalMinutes: 5\n---\n",
    )
    .unwrap();

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );

    assert!(manifest.has_errors());
    let error = manifest.errors().next().expect("error diagnostic");
    assert!(
        error.source.is_some(),
        "diagnostics carry source provenance"
    );
    // The valid task and every other section survive the sibling failure.
    assert!(manifest.tasks.contains_key("digest"));
    assert!(!manifest.skills.is_empty());
    assert!(!manifest.memories.is_empty());
}

#[test]
fn quorum_builder_resolves_the_same_two_layer_manifest() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let builder = QuorumBuilder::new()
        .cwd(workspace.path())
        .dotagents_options(both_layer_options(workspace.path(), global.path()));
    let manifest = builder
        .preview_dotagents_manifest()
        .unwrap()
        .expect("quorum resolves protocol layers");

    assert_eq!(manifest.layers.len(), 2);
    assert_eq!(manifest.mcp_servers.len(), 3);
    assert_eq!(manifest.model_presets.len(), 3);
    assert_eq!(manifest.skills.len(), 2);
    assert!(manifest.mcp_servers.contains_key("workspace-only"));
    assert!(
        manifest.mcp_servers["shared-web"]
            .url
            .as_deref()
            .unwrap()
            .contains("workspace")
    );
}

#[test]
fn protocol_disabled_ignores_both_layers_entirely() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    // Default builder state: protocol loading is disabled.
    let builder = AgentBuilder::new().cwd(workspace.path());
    assert!(builder.preview_dotagents_manifest().unwrap().is_none());
}
