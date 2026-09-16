//! Integration tests for `.agents` Protocol manifest preview.
//!
//! Preview must expose every effective section plus resolved diagnostics
//! through the public builder API without runtime side effects: no MCP
//! server is started, no task is persisted or scheduled, no memory is
//! imported, and no protocol directory is created.

use querymt_agent::api::{AgentBuilder, QuorumBuilder};
use querymt_agent::dotagents::{
    DotagentsAgentConnectionType, DotagentsAgentRole, DotagentsLoadError, DotagentsLoadOptions,
    DotagentsMcpTransport, DotagentsStrictness, DotagentsTaskKind,
};
use std::collections::BTreeMap;
use std::path::Path;
use tempfile::TempDir;

/// Recursively snapshot `root` as sorted `relative path -> descriptor` pairs.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            out.insert(format!("{rel}/"), "dir".to_string());
            walk(base, &path, out);
        } else {
            let content = std::fs::read(&path).unwrap_or_default();
            out.insert(
                rel,
                format!("file:{}:{:08x}", content.len(), fnv1a(&content)),
            );
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Create a protocol tree covering every supported top-level entry.
fn write_workspace_fixture(workspace: &Path) {
    let agents = workspace.join(".agents");
    std::fs::create_dir_all(agents.join("skills/review")).unwrap();
    std::fs::create_dir_all(agents.join("agents/helper")).unwrap();
    std::fs::create_dir_all(agents.join("tasks/digest")).unwrap();
    std::fs::create_dir_all(agents.join("memories")).unwrap();

    std::fs::write(
        agents.join("agents.md"),
        "# Workspace instructions\n\nReview the codebase politely.\n",
    )
    .unwrap();
    std::fs::write(
        agents.join("system-prompt.md"),
        "You are the workspace assistant.\n",
    )
    .unwrap();

    // The stdio command would create a marker file if anything ever launched
    // it. Preview must leave the marker absent.
    #[cfg(unix)]
    let (command, args_json) = (
        "/bin/sh",
        format!(
            "[\"-c\", \"touch {}\"]",
            workspace.join("mcp-launched.marker").display()
        ),
    );
    #[cfg(not(unix))]
    let (command, args_json) = ("nonexistent-mcp-server-xyz".to_string(), "[]".to_string());

    std::fs::write(
        agents.join("mcp.json"),
        format!(
            r#"{{
  "mcpServers": {{
    "fs": {{
      "transport": "stdio",
      "command": "{command}",
      "args": {args_json},
      "env": {{"TOKEN": "${{MCP_TOKEN}}"}}
    }},
    "web": {{
      "transport": "streamable-http",
      "url": "https://example.test/mcp",
      "headers": {{"Authorization": "Bearer ${{MCP_TOKEN}}"}}
    }}
  }}
}}"#
        ),
    )
    .unwrap();

    std::fs::write(
        agents.join("models.json"),
        r#"{"models":{"fast":{
            "provider":"anthropic",
            "model":"claude-sonnet-4-20250514",
            "credential":"${ANTHROPIC_API_KEY}",
            "parameters":{"temperature":0.2}
        }}}"#,
    )
    .unwrap();

    std::fs::write(
        agents.join("skills/review/skill.md"),
        "---\nid: review\nname: Code Review\ndescription: Reviews code changes.\nenabled: true\n---\nReview carefully.\n",
    )
    .unwrap();

    std::fs::write(
        agents.join("agents/helper/agent.md"),
        "---\nname: Helper\ndescription: Helps out\n---\nYou are a helper.\n",
    )
    .unwrap();
    std::fs::write(
        agents.join("agents/helper/config.json"),
        r#"{"model":"fast","tools":["read_tool"],"mcpServers":["fs"]}"#,
    )
    .unwrap();

    std::fs::write(
        agents.join("tasks/digest/task.md"),
        "---\nkind: task\nintervalMinutes: 60\nrunOnStartup: false\nprofileId: helper\n---\nSummarize the repository.\n",
    )
    .unwrap();

    std::fs::write(
        agents.join("memories/team-fact.md"),
        "---\ntitle: Team fact\ntags: team, onboarding\nimportance: high\n---\nQueryMT uses durable sessions.\n",
    )
    .unwrap();
}

fn workspace_options(workspace: &Path, global: &Path) -> DotagentsLoadOptions {
    DotagentsLoadOptions::enabled()
        .with_workspace(workspace)
        .with_global_root(global)
        .with_workspace_root(workspace.join(".agents"))
}

#[test]
fn preview_observes_all_effective_sections_with_zero_side_effects() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_fixture(workspace.path());

    let before_workspace = snapshot(workspace.path());
    let before_global = snapshot(global.path());

    let builder = AgentBuilder::new()
        .cwd(workspace.path())
        .dotagents_options(workspace_options(workspace.path(), global.path()));

    let manifest = builder
        .preview_dotagents_manifest()
        .unwrap()
        .expect("protocol layers are present and enabled");

    // Instructions and prompts (frontmatter excluded from bodies).
    let agents_md = manifest.agents_md.as_ref().expect("agents.md resolved");
    assert!(agents_md.body.contains("Review the codebase politely."));
    assert!(!agents_md.body.contains("---"));
    let system_prompt = manifest
        .system_prompt
        .as_ref()
        .expect("system-prompt.md resolved");
    assert!(system_prompt.body.contains("workspace assistant"));

    // MCP servers: stdio and streamable HTTP, raw values (no interpolation).
    assert_eq!(manifest.mcp_servers.len(), 2);
    let fs = &manifest.mcp_servers["fs"];
    assert_eq!(fs.transport, DotagentsMcpTransport::Stdio);
    #[cfg(unix)]
    assert_eq!(fs.command.as_deref(), Some("/bin/sh"));
    #[cfg(not(unix))]
    assert_eq!(fs.command.as_deref(), Some("nonexistent-mcp-server-xyz"));
    assert_eq!(
        fs.env.get("TOKEN").map(String::as_str),
        Some("${MCP_TOKEN}")
    );
    let web = &manifest.mcp_servers["web"];
    assert_eq!(web.transport, DotagentsMcpTransport::StreamableHttp);
    assert_eq!(web.url.as_deref(), Some("https://example.test/mcp"));

    // Model presets.
    let fast = manifest.model_preset("fast").expect("preset resolved");
    assert_eq!(fast.provider, "anthropic");
    assert_eq!(fast.model, "claude-sonnet-4-20250514");

    // Skills.
    assert_eq!(manifest.skills.len(), 1);
    let skill = &manifest.skills["review"];
    assert!(skill.enabled);
    assert_eq!(skill.name, "Code Review");

    // Sub-agent profiles.
    assert_eq!(manifest.agents.len(), 1);
    let helper = &manifest.agents["helper"];
    assert_eq!(helper.role, DotagentsAgentRole::DelegationTarget);
    assert_eq!(
        helper.connection.connection_type,
        DotagentsAgentConnectionType::Internal
    );
    assert_eq!(helper.config.model_preset.as_deref(), Some("fast"));

    // Repeat tasks (inspectable, not reconciled).
    assert_eq!(manifest.tasks.len(), 1);
    let digest = &manifest.tasks["digest"];
    assert_eq!(digest.kind, DotagentsTaskKind::Task);
    assert_eq!(digest.interval_minutes, Some(60));
    assert_eq!(digest.profile_id.as_deref(), Some("helper"));

    // Memories (inspectable, not imported).
    assert_eq!(manifest.memories.len(), 1);
    let memory = &manifest.memories["team-fact"];
    assert_eq!(memory.title.as_deref(), Some("Team fact"));
    assert_eq!(memory.tags, vec!["team", "onboarding"]);

    // Provenance and diagnostics.
    assert_eq!(manifest.layers.len(), 2);
    assert!(manifest.diagnostics.is_empty());

    // Preview is deterministic: resolving again yields the same manifest.
    let builder = AgentBuilder::new()
        .cwd(workspace.path())
        .dotagents_options(workspace_options(workspace.path(), global.path()));
    let again = builder.preview_dotagents_manifest().unwrap().unwrap();
    assert_eq!(manifest, again);

    // Zero side effects: nothing created, modified, or launched.
    assert_eq!(before_workspace, snapshot(workspace.path()));
    assert_eq!(before_global, snapshot(global.path()));
    assert!(!workspace.path().join("mcp-launched.marker").exists());
    assert!(!workspace.path().join("agents.db").exists());
}

#[test]
fn preview_returns_none_when_protocol_loading_is_disabled() {
    let workspace = TempDir::new().unwrap();
    write_workspace_fixture(workspace.path());

    let before = snapshot(workspace.path());

    // Default builder state: protocol loading is off, so the directory must
    // not influence preview output and nothing may be created.
    let builder = AgentBuilder::new().cwd(workspace.path());
    let preview = builder.preview_dotagents_manifest().unwrap();
    assert!(preview.is_none());

    assert_eq!(before, snapshot(workspace.path()));
}

#[test]
fn preview_reports_diagnostics_and_keeps_valid_siblings() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_fixture(workspace.path());
    // Malformed sibling next to the valid task: empty prompt body.
    std::fs::create_dir_all(workspace.path().join(".agents/tasks/broken")).unwrap();
    std::fs::write(
        workspace.path().join(".agents/tasks/broken/task.md"),
        "---\nintervalMinutes: 5\n---\n",
    )
    .unwrap();

    let before = snapshot(workspace.path());

    let builder = AgentBuilder::new()
        .cwd(workspace.path())
        .dotagents_options(workspace_options(workspace.path(), global.path()));
    let manifest = builder.preview_dotagents_manifest().unwrap().unwrap();

    // Compatibility policy: the invalid sibling is diagnosed and isolated
    // while the valid sibling stays inspectable.
    assert!(manifest.has_errors());
    assert!(manifest.errors().count() >= 1);
    assert_eq!(manifest.tasks.len(), 1);
    assert!(manifest.tasks.contains_key("digest"));

    assert_eq!(before, snapshot(workspace.path()));
}

#[test]
fn strict_preview_preserves_typed_load_error_manifest() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_fixture(workspace.path());
    std::fs::create_dir_all(workspace.path().join(".agents/tasks/broken")).unwrap();
    std::fs::write(
        workspace.path().join(".agents/tasks/broken/task.md"),
        "---\nintervalMinutes: 5\n---\n",
    )
    .unwrap();

    let options = workspace_options(workspace.path(), global.path())
        .with_strictness(DotagentsStrictness::Strict);
    let error = AgentBuilder::new()
        .cwd(workspace.path())
        .dotagents_options(options.clone())
        .preview_dotagents_manifest()
        .unwrap_err();
    let load_error = error
        .downcast_ref::<DotagentsLoadError>()
        .expect("single-agent preview preserves DotagentsLoadError");
    assert!(load_error.manifest.has_errors());

    let error = QuorumBuilder::new()
        .cwd(workspace.path())
        .dotagents_options(options)
        .preview_dotagents_manifest()
        .unwrap_err();
    let load_error = error
        .downcast_ref::<DotagentsLoadError>()
        .expect("quorum preview preserves DotagentsLoadError");
    assert!(load_error.manifest.has_errors());
}

#[test]
fn quorum_builder_preview_resolves_manifest() {
    let workspace = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    write_workspace_fixture(workspace.path());

    let before = snapshot(workspace.path());

    let builder = QuorumBuilder::new()
        .cwd(workspace.path())
        .dotagents_options(workspace_options(workspace.path(), global.path()));
    let manifest = builder.preview_dotagents_manifest().unwrap().unwrap();

    assert!(manifest.agents_md.is_some());
    assert_eq!(manifest.mcp_servers.len(), 2);
    assert_eq!(manifest.tasks.len(), 1);

    assert_eq!(before, snapshot(workspace.path()));
}
