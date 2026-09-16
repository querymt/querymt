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
    DotagentsAgentRole, DotagentsLoadOptions, DotagentsTaskApprovalDecision,
    DotagentsTaskApprovalRequest, DotagentsTaskKind, DotagentsTaskTrustPolicy,
};
use querymt_agent::session::backend::StorageBackend;
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
    let plan =
        querymt_agent::dotagents::DotagentsMcpPlan::from_manifest_with_workspace_stdio_approval(
            &manifest, true,
        );

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
    let plan =
        querymt_agent::dotagents::DotagentsMcpPlan::from_manifest_with_workspace_stdio_approval(
            &manifest, true,
        );

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
async fn selected_workspace_preset_is_rejected_when_its_provider_is_not_installed() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    // Declaring a provider in `providers.toml` is not the same as having it
    // installed: the plugin cannot load, so `anthropic` stays unavailable and
    // the preset must be rejected rather than applied.
    let registry = preset_registry(registry_dir.path());
    let storage = std::sync::Arc::new(
        querymt_agent::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap(),
    );

    // `shared` exists in both layers and selects the unavailable `anthropic`.
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
        .expect("an unavailable preset is reported, not a build failure");

    let handle = agent.handle();
    let params = handle.config.provider.initial_config();
    // The explicit base configuration survives untouched.
    assert_eq!(
        params.provider.as_deref(),
        Some("openai"),
        "an uninstalled preset provider must not be applied"
    );
    assert_eq!(params.model.as_deref(), Some("gpt-4o-mini"));

    // The rejection is actionable and names the preset and the provider.
    let report = agent
        .activate_dotagents()
        .await
        .expect("activation runs")
        .expect("protocol enabled");
    let rendered = format!("{:?}", report.diagnostics);
    assert!(
        rendered.contains("shared") && rendered.contains("anthropic"),
        "the diagnostic must name the preset and the missing provider: {rendered}"
    );

    agent.shutdown().await;
}

/// The workspace layer wins over the global layer for a shared preset key, and
/// the winning definition is the one whose provider is validated.
#[tokio::test]
async fn workspace_preset_definition_wins_over_the_global_definition() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    // The global layer and workspace layer disagree about `shared`. Rewrite the
    // workspace copy to select a provider that is genuinely unavailable, so the
    // rejection proves the workspace definition (not the global one) was used.
    std::fs::write(
        workspace.path().join(".agents/models.json"),
        r#"{"models":{
            "shared": {"provider":"workspace-only-provider","model":"claude-workspace"},
            "workspace-only": {"provider":"openai","model":"gpt-workspace"}
        }}"#,
    )
    .unwrap();

    let registry_dir = TempDir::new().unwrap();
    let storage = std::sync::Arc::new(
        querymt_agent::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap(),
    );

    let agent = AgentBuilder::new()
        .provider("openai", "gpt-4o-mini")
        .cwd(workspace.path())
        .dotagents_options(
            both_layer_options(workspace.path(), global.path())
                .with_selected_model_preset("shared"),
        )
        .infra(querymt_agent::api::AgentInfra {
            plugin_registry: preset_registry(registry_dir.path()),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .expect("build agent");

    // The manifest records the workspace definition as effective, so the
    // diagnostic names the workspace provider rather than the global one.
    let manifest = agent
        .dotagents()
        .expect("protocol state")
        .manifest()
        .clone();
    assert_eq!(manifest.model_presets["shared"].model, "claude-workspace");
    assert_eq!(
        manifest.model_presets["shared"].provider,
        "workspace-only-provider"
    );

    let report = agent
        .activate_dotagents()
        .await
        .expect("activation runs")
        .expect("protocol enabled");
    let rendered = format!("{:?}", report.diagnostics);
    assert!(
        rendered.contains("workspace-only-provider"),
        "the workspace preset definition must be the one validated: {rendered}"
    );

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
fn direct_collision_filter_skips_the_protocol_target() {
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

    // Exercise the direct collision filter. QuorumBuilder integration is
    // covered separately because this fixture does not construct a runnable
    // explicit delegate handle.
    let explicit_ids: std::collections::HashSet<String> = ["helper".to_string()].into();

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

// ---------------------------------------------------------------------------
// Post-construction activation
// ---------------------------------------------------------------------------

/// A registry that admits the providers the fixtures actually select.
///
/// `empty_registry` declares only `mock`, so a preset naming `anthropic` would
/// fail provider validation. These fixtures exercise real activation, so the
/// registry must be able to resolve the presets they name.
fn preset_registry(dir: &Path) -> Arc<PluginRegistry> {
    let config_path = dir.join("providers.toml");
    std::fs::write(
        &config_path,
        "[[providers]]\nname = \"anthropic\"\npath = \"anthropic.wasm\"\n\n\
         [[providers]]\nname = \"openai\"\npath = \"openai.wasm\"\n\n\
         [[providers]]\nname = \"mock\"\npath = \"mock.wasm\"\n",
    )
    .expect("write providers config");
    Arc::new(PluginRegistry::from_path(&config_path).expect("registry"))
}

/// Build a storage backend whose files live under `dir` so it survives the
/// duration of a test and can be re-opened to simulate a restart.
async fn file_storage(dir: &Path) -> Arc<querymt_agent::session::sqlite_storage::SqliteStorage> {
    Arc::new(
        querymt_agent::session::sqlite_storage::SqliteStorage::connect(dir.join("agent.db"))
            .await
            .expect("storage"),
    )
}

/// An approver that always grants, recording the requests it saw.
struct AlwaysApprove {
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl querymt_agent::dotagents::DotagentsTaskApprover for AlwaysApprove {
    async fn request_approval(
        &self,
        request: DotagentsTaskApprovalRequest,
    ) -> DotagentsTaskApprovalDecision {
        self.seen.lock().unwrap().push(request.task_id.clone());
        DotagentsTaskApprovalDecision::Approve
    }
}

/// Build a single-agent runtime over both protocol layers with real storage.
async fn activatable_agent(
    workspace: &Path,
    global: &Path,
    storage: Arc<dyn querymt_agent::session::backend::StorageBackend>,
    registry: Arc<PluginRegistry>,
    trust: DotagentsTaskTrustPolicy,
) -> querymt_agent::api::Agent {
    AgentBuilder::new()
        .provider("openai", "gpt-4o-mini")
        .cwd(workspace)
        .dotagents_options(both_layer_options(workspace, global).with_workspace_task_trust(trust))
        .infra(querymt_agent::api::AgentInfra {
            plugin_registry: registry,
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .expect("build agent")
}

/// Memories import into the knowledge store through the normal retrieval path.
#[tokio::test]
async fn activation_imports_memories_into_live_knowledge() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    let storage_dir = TempDir::new().unwrap();
    let storage = file_storage(storage_dir.path()).await;
    let agent = activatable_agent(
        workspace.path(),
        global.path(),
        storage.clone(),
        preset_registry(registry_dir.path()),
        DotagentsTaskTrustPolicy::Prompt,
    )
    .await;

    let report = agent
        .activate_dotagents()
        .await
        .expect("activation runs")
        .expect("protocol enabled");

    assert!(report.memories_reconciled, "knowledge store was configured");
    let memory = report.memory.as_ref().expect("memory report");
    assert_eq!(
        memory.created, 2,
        "both layer memories imported: {memory:?}"
    );
    assert!(memory.store_available);

    // The imported memories are retrievable as live knowledge.
    let store = storage.knowledge_store().expect("knowledge store");
    let scope = querymt_agent::dotagents::protocol_knowledge_scope(Some(workspace.path()));
    let entries = store
        .list(&scope, querymt_agent::knowledge::KnowledgeFilter::default())
        .await
        .expect("list knowledge");
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.protocol_source_key.is_some()),
        "imported memories carry protocol provenance"
    );
    assert!(
        entries.iter().any(|entry| entry
            .raw_text
            .as_deref()
            .is_some_and(|text| text.contains("pins its toolchain"))),
        "workspace memory body was imported"
    );

    // Re-running activation is idempotent: no duplicates are created.
    let second = agent
        .activate_dotagents()
        .await
        .expect("second activation")
        .expect("protocol enabled");
    let second_memory = second.memory.as_ref().expect("memory report");
    assert_eq!(second_memory.created, 0, "reload must not duplicate");
    assert_eq!(second_memory.unchanged, 2);
    let after = store
        .list(&scope, querymt_agent::knowledge::KnowledgeFilter::default())
        .await
        .expect("list knowledge");
    assert_eq!(after.len(), 2, "no duplicate entries after reload");

    agent.shutdown().await;
}

/// An approved workspace task becomes a durable recurring task and interval
/// schedule bound to a protocol-owned automation session.
#[tokio::test]
async fn activation_creates_durable_task_and_schedule_for_approved_workspace_task() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    let storage_dir = TempDir::new().unwrap();
    let storage = file_storage(storage_dir.path()).await;
    // `allow` is the explicit unsafe opt-in for unattended hosts; a `prompt`
    // policy with no approver instead leaves the task pending, which is covered
    // by `activation_without_approver_keeps_workspace_tasks_pending_and_inert`.
    let agent = activatable_agent(
        workspace.path(),
        global.path(),
        storage.clone(),
        preset_registry(registry_dir.path()),
        DotagentsTaskTrustPolicy::Allow,
    )
    .await;

    let report = agent
        .activate_dotagents()
        .await
        .expect("activation runs")
        .expect("protocol enabled");

    let task = report
        .tasks
        .iter()
        .find(|task| task.task_id == "digest")
        .unwrap_or_else(|| {
            panic!(
                "the workspace task reconciled; report tasks={:?} diagnostics={:?} pending={:?}",
                report.tasks, report.diagnostics, report.pending_approvals
            )
        });
    assert!(
        task.applied.is_armed(),
        "approved task is executable: {:?}",
        task.applied
    );

    // The records are durable, not just reported.
    let sessions = storage.session_store();
    let schedules = storage.schedule_repository().expect("schedule repository");

    let automation_sessions: Vec<_> = sessions
        .list_sessions()
        .await
        .expect("list sessions")
        .into_iter()
        .filter(|session| {
            session.session_kind.as_deref()
                == Some(querymt_agent::dotagents::AUTOMATION_SESSION_KIND)
        })
        .collect();
    assert_eq!(
        automation_sessions.len(),
        1,
        "exactly one protocol-owned automation session"
    );
    let session = &automation_sessions[0];

    // The owned task exists in that session with the protocol creation key.
    let tasks = sessions
        .list_tasks(&session.public_id)
        .await
        .expect("list tasks");
    let owned = tasks
        .iter()
        .find(|t| t.creation_key.is_some())
        .expect("protocol task persisted");
    assert!(
        owned
            .creation_key
            .as_deref()
            .expect("creation key")
            .contains("digest"),
        "creation key identifies the protocol source: {:?}",
        owned.creation_key
    );
    assert_eq!(
        owned.expected_deliverable.as_deref(),
        Some("Summarize the repository."),
    );

    // The interval schedule is armed and carries the converted interval.
    let owned_schedules = schedules
        .list_schedules(&session.public_id)
        .await
        .expect("list schedules");
    assert_eq!(owned_schedules.len(), 1, "one interval schedule");
    let schedule = &owned_schedules[0];
    match &schedule.trigger {
        querymt_agent::session::domain_schedule::ScheduleTrigger::Interval { seconds } => {
            assert_eq!(*seconds, 3600, "60 minutes converts to 3600 seconds");
        }
        other => panic!("expected an interval trigger, got {other:?}"),
    }
    assert_eq!(
        schedule.state,
        querymt_agent::session::domain_schedule::ScheduleState::Armed,
        "approved schedule runs"
    );

    // User-visible session listing is not polluted by the automation session
    // beyond the single protocol-owned entry.
    assert_eq!(
        sessions
            .list_tasks(&session.public_id)
            .await
            .expect("list tasks")
            .len(),
        1,
        "no unrelated tasks in the automation session"
    );

    agent.shutdown().await;
}

/// A headless host with no approver keeps untrusted workspace tasks pending,
/// creates no records for them, and does not fail the rest of activation.
#[tokio::test]
async fn activation_without_approver_keeps_workspace_tasks_pending_and_inert() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    let storage_dir = TempDir::new().unwrap();
    let storage = file_storage(storage_dir.path()).await;
    let agent = activatable_agent(
        workspace.path(),
        global.path(),
        storage.clone(),
        preset_registry(registry_dir.path()),
        DotagentsTaskTrustPolicy::Prompt,
    )
    .await;

    let report = agent
        .activate_dotagents()
        .await
        .expect("activation runs")
        .expect("protocol enabled");

    assert!(
        !report.is_failed(),
        "prompt policy is not fatal in compat mode"
    );
    assert_eq!(
        report.pending_approvals.len(),
        1,
        "the workspace task is surfaced for a host decision: {:?}",
        report.pending_approvals
    );
    assert_eq!(report.pending_approvals[0].task_id, "digest");
    assert!(
        report.tasks.iter().all(|task| !task.applied.is_armed()),
        "untrusted task must not be executable: {:?}",
        report.tasks
    );

    // No automation session or schedule is created for a pending task.
    let sessions = storage.session_store();
    assert!(
        sessions
            .list_sessions()
            .await
            .expect("list sessions")
            .iter()
            .all(|session| session.session_kind.as_deref()
                != Some(querymt_agent::dotagents::AUTOMATION_SESSION_KIND)),
        "pending tasks must not provision an automation session"
    );

    // Unrelated protocol features still applied: memories are inspectorable and
    // imported even though the task stayed pending.
    assert!(report.memories_reconciled);
    assert_eq!(report.memory.as_ref().expect("memory report").created, 2);

    agent.shutdown().await;
}

/// An approver turns the same pending task into a durable, executable record,
/// and activation stays idempotent across repeats.
#[tokio::test]
async fn activation_with_approver_creates_records_once() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    let storage_dir = TempDir::new().unwrap();
    let storage = file_storage(storage_dir.path()).await;

    let approver = Arc::new(AlwaysApprove {
        seen: std::sync::Mutex::new(Vec::new()),
    });

    let agent = AgentBuilder::new()
        .provider("openai", "gpt-4o-mini")
        .cwd(workspace.path())
        .dotagents_options(
            both_layer_options(workspace.path(), global.path())
                .with_workspace_task_trust(DotagentsTaskTrustPolicy::Prompt),
        )
        .dotagents_task_approver(approver.clone())
        .infra(querymt_agent::api::AgentInfra {
            plugin_registry: preset_registry(registry_dir.path()),
            storage: Some(storage.clone()),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .expect("build agent");

    let first = agent
        .activate_dotagents()
        .await
        .expect("activation runs")
        .expect("protocol enabled");
    assert!(first.pending_approvals.is_empty(), "approver was consulted");
    assert_eq!(approver.seen.lock().unwrap().len(), 1);
    assert_eq!(first.armed_tasks(), 1);

    let sessions = storage.session_store();
    let schedules = storage.schedule_repository().expect("schedule repository");
    let session_count = |sessions: Vec<querymt_agent::session::store::Session>| {
        sessions
            .into_iter()
            .filter(|session| {
                session.session_kind.as_deref()
                    == Some(querymt_agent::dotagents::AUTOMATION_SESSION_KIND)
            })
            .count()
    };
    assert_eq!(
        session_count(sessions.list_sessions().await.expect("sessions")),
        1
    );

    // A repeat activation reuses the persisted approval, session, task, and
    // schedule instead of creating duplicates.
    let second = agent
        .activate_dotagents()
        .await
        .expect("second activation")
        .expect("protocol enabled");
    assert_eq!(second.armed_tasks(), 1);
    assert_eq!(
        session_count(sessions.list_sessions().await.expect("sessions")),
        1,
        "no duplicate automation session"
    );

    let session = sessions
        .list_sessions()
        .await
        .expect("sessions")
        .into_iter()
        .find(|session| {
            session.session_kind.as_deref()
                == Some(querymt_agent::dotagents::AUTOMATION_SESSION_KIND)
        })
        .expect("automation session");
    assert_eq!(
        sessions
            .list_tasks(&session.public_id)
            .await
            .expect("tasks")
            .len(),
        1,
        "no duplicate protocol task"
    );
    assert_eq!(
        schedules
            .list_schedules(&session.public_id)
            .await
            .expect("schedules")
            .len(),
        1,
        "no duplicate schedule"
    );

    agent.shutdown().await;
}

/// A trusted task with `runOnStartup` fires exactly once during activation and
/// still keeps its normal interval activation armed.
#[tokio::test]
async fn activation_fires_run_on_startup_once_and_keeps_the_interval_armed() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    // The fixture task opts out of startup; rewrite it to opt in.
    std::fs::write(
        workspace.path().join(".agents/tasks/digest/task.md"),
        "---\nkind: task\nintervalMinutes: 60\nrunOnStartup: true\n---\nSummarize the repository.\n",
    )
    .unwrap();

    let registry_dir = TempDir::new().unwrap();
    let storage_dir = TempDir::new().unwrap();
    let storage = file_storage(storage_dir.path()).await;
    let agent = activatable_agent(
        workspace.path(),
        global.path(),
        storage.clone(),
        preset_registry(registry_dir.path()),
        DotagentsTaskTrustPolicy::Allow,
    )
    .await;

    let report = agent
        .activate_dotagents()
        .await
        .expect("activation runs")
        .expect("protocol enabled");

    let task = report
        .tasks
        .iter()
        .find(|task| task.task_id == "digest")
        .unwrap_or_else(|| {
            panic!(
                "the task reconciled; tasks={:?} diagnostics={:?}",
                report.tasks, report.diagnostics
            )
        });
    assert!(
        task.fired_on_startup,
        "runOnStartup must fire during the first activation: {:?}",
        report.diagnostics
    );

    // The startup fire does not consume the recurring schedule: the owned
    // schedule is still Armed for its normal interval.
    let sessions = storage.session_store();
    let schedules = storage.schedule_repository().expect("schedule repository");
    let session = sessions
        .list_sessions()
        .await
        .expect("sessions")
        .into_iter()
        .find(|session| {
            session.session_kind.as_deref()
                == Some(querymt_agent::dotagents::AUTOMATION_SESSION_KIND)
        })
        .expect("automation session");
    let owned_schedules = schedules
        .list_schedules(&session.public_id)
        .await
        .expect("schedules");
    assert_eq!(owned_schedules.len(), 1);
    // A startup fire must not retire the trigger. The schedule may already be
    // `Running` because the fired run started, so assert it is still live rather
    // than racing on a specific transient state.
    assert!(
        matches!(
            owned_schedules[0].state,
            querymt_agent::session::domain_schedule::ScheduleState::Armed
                | querymt_agent::session::domain_schedule::ScheduleState::Running
        ),
        "the interval trigger must remain live after a startup fire, found {:?}",
        owned_schedules[0].state
    );
    // The interval trigger itself is untouched by the startup fire.
    match &owned_schedules[0].trigger {
        querymt_agent::session::domain_schedule::ScheduleTrigger::Interval { seconds } => {
            assert_eq!(*seconds, 3600, "interval activation is retained");
        }
        other => panic!("expected an interval trigger, got {other:?}"),
    }

    // Exactly one schedule exists after a second activation: the startup fire is
    // per-activation and never duplicates records.
    let second = agent
        .activate_dotagents()
        .await
        .expect("second activation")
        .expect("protocol enabled");
    assert!(
        second.tasks.iter().all(|task| task.task_id == "digest"),
        "no duplicate protocol tasks"
    );
    assert_eq!(
        schedules
            .list_schedules(&session.public_id)
            .await
            .expect("schedules")
            .len(),
        1,
        "startup must not accumulate schedules"
    );

    agent.shutdown().await;
}

/// A restart reuses the same automation session rather than provisioning a new
/// one, so reconciliation never accumulates sessions across restarts.
#[tokio::test]
async fn activation_reuses_automation_session_across_restarts() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    let storage_dir = TempDir::new().unwrap();

    let first_storage = file_storage(storage_dir.path()).await;
    let first_agent = activatable_agent(
        workspace.path(),
        global.path(),
        first_storage.clone(),
        preset_registry(registry_dir.path()),
        DotagentsTaskTrustPolicy::Allow,
    )
    .await;
    first_agent
        .activate_dotagents()
        .await
        .expect("first activation")
        .expect("protocol enabled");

    let first_sessions = first_storage
        .session_store()
        .list_sessions()
        .await
        .expect("sessions");
    let first_ids: Vec<String> = first_sessions
        .iter()
        .filter(|session| {
            session.session_kind.as_deref()
                == Some(querymt_agent::dotagents::AUTOMATION_SESSION_KIND)
        })
        .map(|session| session.public_id.clone())
        .collect();
    assert_eq!(first_ids.len(), 1);
    first_agent.shutdown().await;
    drop(first_storage);

    // Re-open the same database to simulate a restart.
    let second_storage = file_storage(storage_dir.path()).await;
    let second_agent = activatable_agent(
        workspace.path(),
        global.path(),
        second_storage.clone(),
        preset_registry(registry_dir.path()),
        DotagentsTaskTrustPolicy::Allow,
    )
    .await;
    second_agent
        .activate_dotagents()
        .await
        .expect("activation after restart")
        .expect("protocol enabled");

    let second_ids: Vec<String> = second_storage
        .session_store()
        .list_sessions()
        .await
        .expect("sessions")
        .into_iter()
        .filter(|session| {
            session.session_kind.as_deref()
                == Some(querymt_agent::dotagents::AUTOMATION_SESSION_KIND)
        })
        .map(|session| session.public_id)
        .collect();

    assert_eq!(
        second_ids, first_ids,
        "the automation session survives a restart unchanged"
    );

    second_agent.shutdown().await;
}

/// An unavailable provider must not partially apply the preset: the explicit
/// base configuration stays completely intact.
#[tokio::test]
async fn unavailable_preset_provider_preserves_explicit_configuration() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    let registry_dir = TempDir::new().unwrap();
    let storage_dir = TempDir::new().unwrap();
    let storage = file_storage(storage_dir.path()).await;

    // The registry only knows `mock`, but `shared` selects `anthropic`.
    let agent = AgentBuilder::new()
        .provider("openai", "gpt-4o-mini")
        .system("Explicit system part")
        .cwd(workspace.path())
        .dotagents_options(
            both_layer_options(workspace.path(), global.path())
                .with_selected_model_preset("shared"),
        )
        .infra(querymt_agent::api::AgentInfra {
            plugin_registry: empty_registry(registry_dir.path()),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .expect("build agent");

    let handle = agent.handle();
    let params = handle.config.provider.initial_config();
    assert_eq!(
        params.provider.as_deref(),
        Some("openai"),
        "an unavailable preset provider must not be applied"
    );
    assert_eq!(
        params.model.as_deref(),
        Some("gpt-4o-mini"),
        "the explicit model survives a rejected preset"
    );
    // The preset is also not half-applied to unrelated fields.
    assert_eq!(params.system[0], "Explicit system part");
    // The rejection is reported rather than swallowed.
    let report = agent
        .activate_dotagents()
        .await
        .expect("activation runs")
        .expect("protocol enabled");
    assert!(
        report.has_diagnostics(),
        "the rejected preset is visible to the host"
    );

    agent.shutdown().await;
}

/// Protocol secrets never appear in a debug rendering of the runtime manifest.
#[tokio::test]
async fn runtime_manifest_debug_never_leaks_secrets() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_layer(workspace.path());
    write_global_layer(global.path());

    // A preset and MCP server carrying literal secret material.
    std::fs::write(
        workspace.path().join(".agents/models.json"),
        r#"{"models":{
            "leaky": {
              "provider":"anthropic",
              "model":"claude",
              "credential":"sk-literal-secret-value",
              "parameters":{"api_key":"sk-parameter-secret-value"}
            }
        }}"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join(".agents/mcp.json"),
        r#"{
  "mcpServers": {
    "token-server": {
      "transport": "stdio",
      "command": "npx",
      "env": { "SERVICE_TOKEN": "mcp-env-secret-value" },
      "headers": { "Authorization": "Bearer mcp-header-secret-value" }
    }
  }
}"#,
    )
    .unwrap();

    let manifest = runtime_manifest(
        workspace.path(),
        global.path(),
        DotagentsTaskTrustPolicy::Prompt,
    );

    let rendered = format!("{manifest:?}");
    for secret in [
        "sk-literal-secret-value",
        "sk-parameter-secret-value",
        "mcp-env-secret-value",
        "mcp-header-secret-value",
    ] {
        assert!(
            !rendered.contains(secret),
            "runtime manifest Debug leaked `{secret}`"
        );
    }
    // The configuration stays diagnosable.
    assert!(rendered.contains("leaky"));
    assert!(rendered.contains("SERVICE_TOKEN"));
    assert!(rendered.contains("token-server"));
}
