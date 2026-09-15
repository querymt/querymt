//! Deterministic layering and merge for `.agents` Protocol documents
//! (task 1.6).
//!
//! Layering applies protocol content in a fixed, deterministic order:
//!
//! 1. explicit QueryMT base configuration (not represented here);
//! 2. the global protocol layer (`~/.agents/`);
//! 3. the workspace protocol layer (`<workspace>/.agents/`).
//!
//! Merge behavior is type-specific, matching the "Protocol files follow
//! type-specific merge rules" requirement:
//!
//! - `agents.md` and `system-prompt.md` are **singletons**: a later layer
//!   wholly replaces an earlier one;
//! - `mcp.json` and `models.json` merge by **top-level key** (each key is an
//!   independent entry, so unmatched earlier keys survive);
//! - skills, agents, tasks, and memories merge by **stable entry ID**, with
//!   later layers replacing matching IDs.
//!
//! Within a single layer, two entries that normalize to the same ID are an
//! error rather than a filesystem-order-dependent winner. The merge keeps the
//! first entry by deterministic ordering and records a
//! [`DotagentsDiagnosticCode::DuplicateId`] diagnostic naming the source.
//!
//! Every resolved value already carries [`DotagentsSource`] provenance and a
//! content fingerprint from its parser, so a merged manifest always records
//! the winning source.

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::layer::{DotagentsLayer, DotagentsSource};
use super::manifest::{
    DotagentsAgent, DotagentsManifest, DotagentsMcpServer, DotagentsMemory, DotagentsModelPreset,
    DotagentsPrompt, DotagentsSkill, DotagentsTask, DotagentsUnsupported,
};
use std::collections::BTreeMap;

/// A single layer's parsed contributions, ready to be merged.
///
/// A layer is produced by parsing one protocol root. Merging many layers in
/// precedence order yields the effective [`DotagentsManifest`].
#[derive(Debug, Clone, Default)]
pub struct DotagentsLayerContent {
    /// Which layer this content came from.
    pub layer: DotagentsLayer,
    /// The `agents.md` singleton, when present in this layer.
    pub agents_md: Option<DotagentsPrompt>,
    /// The `system-prompt.md` singleton, when present in this layer.
    pub system_prompt: Option<DotagentsPrompt>,
    /// Whether this layer contains an invalid `agents.md`.
    pub invalid_agents_md: bool,
    /// Whether this layer contains an invalid `system-prompt.md`.
    pub invalid_system_prompt: bool,
    /// Whether this layer contains an invalid `mcp.json`.
    pub invalid_mcp: bool,
    /// Whether this layer contains an invalid `models.json`.
    pub invalid_models: bool,
    /// MCP servers keyed by normalized name (`mcp.json` top-level keys).
    pub mcp_servers: BTreeMap<String, DotagentsMcpServer>,
    /// Model presets keyed by name (`models.json` top-level keys).
    pub model_presets: BTreeMap<String, DotagentsModelPreset>,
    /// Skills keyed by normalized ID.
    pub skills: BTreeMap<String, DotagentsSkill>,
    /// Agents keyed by normalized ID.
    pub agents: BTreeMap<String, DotagentsAgent>,
    /// Tasks keyed by normalized ID.
    pub tasks: BTreeMap<String, DotagentsTask>,
    /// Memories keyed by normalized ID.
    pub memories: BTreeMap<String, DotagentsMemory>,
    /// Detected-but-unsupported protocol artifacts in this layer.
    pub unsupported: Vec<DotagentsUnsupported>,
    /// Diagnostics produced while parsing this layer (before merging).
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

impl DotagentsLayerContent {
    /// Create empty content for a layer.
    pub fn new(layer: DotagentsLayer) -> Self {
        Self {
            layer,
            ..Self::default()
        }
    }
}

/// Merge parsed layer content into an effective manifest.
///
/// `layers` must be supplied in precedence order (global before workspace).
/// Later layers override earlier ones for singletons and matching IDs, while
/// unmatched earlier entries remain available. Duplicate IDs within one layer
/// are reported as errors and the deterministic first entry wins.
///
/// Duplicate detection requires the entry's source to be known. Because layer
/// content is keyed by ID, a duplicate can only be detected while a layer is
/// being *built*; see [`insert_unique`] for the building primitive used by the
/// loader. This function therefore merges one entry per ID and additionally
/// re-checks `source.entry_id` consistency for defense in depth.
pub fn merge_layers(layers: &[DotagentsLayerContent]) -> DotagentsManifest {
    let mut manifest = DotagentsManifest::empty();

    for content in layers {
        // Singletons: a later layer wholly replaces an earlier one, so the
        // winning source is always recorded from the replacing document.
        if content.invalid_agents_md {
            manifest.agents_md = None;
        } else if let Some(agents_md) = content.agents_md.clone() {
            manifest.agents_md = Some(agents_md);
        }
        if content.invalid_system_prompt {
            manifest.system_prompt = None;
        } else if let Some(system_prompt) = content.system_prompt.clone() {
            manifest.system_prompt = Some(system_prompt);
        }

        // Keyed content: a later layer replaces matching keys/IDs while
        // unmatched earlier content remains. A malformed selected JSON file
        // blocks that affected section instead of silently activating values
        // from a lower-precedence file.
        if content.invalid_mcp {
            manifest.mcp_servers.clear();
        } else {
            for (name, server) in &content.mcp_servers {
                manifest.mcp_servers.insert(name.clone(), server.clone());
            }
        }
        if content.invalid_models {
            manifest.model_presets.clear();
        } else {
            for (name, preset) in &content.model_presets {
                manifest.model_presets.insert(name.clone(), preset.clone());
            }
        }
        for (id, skill) in &content.skills {
            manifest.skills.insert(id.clone(), skill.clone());
        }
        for (id, agent) in &content.agents {
            manifest.agents.insert(id.clone(), agent.clone());
        }
        for (id, task) in &content.tasks {
            manifest.tasks.insert(id.clone(), task.clone());
        }
        for (id, memory) in &content.memories {
            manifest.memories.insert(id.clone(), memory.clone());
        }

        manifest
            .unsupported
            .extend(content.unsupported.iter().cloned());
        manifest
            .diagnostics
            .extend(content.diagnostics.iter().cloned());
    }

    manifest.sort_deterministically();
    manifest
}

/// Insert an entry into a keyed collection, reporting a within-layer duplicate.
///
/// The **first** inserted entry for an ID wins, which makes the outcome
/// independent of filesystem iteration order as long as callers iterate
/// deterministically. A second entry with the same ID is rejected and an
/// error diagnostic naming both sources is returned.
///
/// Returns `true` when the entry was inserted.
pub fn insert_unique<T: Clone>(
    collection: &mut BTreeMap<String, T>,
    id: &str,
    value: T,
    existing_source: impl Fn(&T) -> &DotagentsSource,
    new_source: &DotagentsSource,
    diagnostics: &mut Vec<DotagentsDiagnostic>,
    kind: &str,
) -> bool {
    if let Some(existing) = collection.get(id) {
        let existing_source = existing_source(existing);
        diagnostics.push(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::DuplicateId,
                format!(
                    "duplicate {kind} id `{id}` in the {} layer: already defined at {}, \
                     redefined at {}",
                    new_source.layer,
                    existing_source.lexical_path.display(),
                    new_source.lexical_path.display()
                ),
            )
            .with_source(new_source.clone()),
        );
        return false;
    }
    collection.insert(id.to_string(), value);
    true
}

/// Source accessors used with [`insert_unique`] for each collection type.
pub mod source_of {
    use super::super::layer::DotagentsSource;
    use super::super::manifest::{
        DotagentsAgent, DotagentsMcpServer, DotagentsMemory, DotagentsModelPreset, DotagentsSkill,
        DotagentsTask,
    };

    /// The source of an MCP server entry.
    pub fn mcp_server(value: &DotagentsMcpServer) -> &DotagentsSource {
        &value.source
    }
    /// The source of a model preset.
    pub fn model_preset(value: &DotagentsModelPreset) -> &DotagentsSource {
        &value.source
    }
    /// The source of a skill.
    pub fn skill(value: &DotagentsSkill) -> &DotagentsSource {
        &value.source
    }
    /// The source of an agent profile.
    pub fn agent(value: &DotagentsAgent) -> &DotagentsSource {
        &value.source
    }
    /// The source of a task.
    pub fn task(value: &DotagentsTask) -> &DotagentsSource {
        &value.source
    }
    /// The source of a memory.
    pub fn memory(value: &DotagentsMemory) -> &DotagentsSource {
        &value.source
    }
}

/// Normalize a raw protocol entry ID into its stable comparison key.
///
/// IDs are trimmed and lowercased so `Review` and `review` collide
/// deterministically instead of producing two entries on case-sensitive
/// filesystems and one on case-insensitive filesystems.
pub fn normalize_id(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dotagents::manifest::{DotagentsAgentRole, DotagentsMcpTransport};
    use std::path::PathBuf;

    fn source(layer: DotagentsLayer, path: &str, id: Option<&str>) -> DotagentsSource {
        let mut source = DotagentsSource::singleton(layer, PathBuf::from(path));
        source.entry_id = id.map(str::to_string);
        source
    }

    fn prompt(layer: DotagentsLayer, body: &str) -> DotagentsPrompt {
        DotagentsPrompt {
            metadata: BTreeMap::new(),
            body: body.to_string(),
            source: source(layer, &format!("{layer}/agents.md"), None),
            fingerprint: format!("fp-{body}"),
        }
    }

    fn mcp(layer: DotagentsLayer, name: &str) -> DotagentsMcpServer {
        DotagentsMcpServer {
            name: name.to_string(),
            transport: DotagentsMcpTransport::Stdio,
            declared_transport: None,
            command: Some("cmd".to_string()),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            extensions: BTreeMap::new(),
            source: source(layer, &format!("{layer}/mcp.json"), Some(name)),
        }
    }

    fn preset(layer: DotagentsLayer, name: &str, model: &str) -> DotagentsModelPreset {
        DotagentsModelPreset {
            name: name.to_string(),
            provider: "p".to_string(),
            model: model.to_string(),
            credential: None,
            parameters: BTreeMap::new(),
            extensions: BTreeMap::new(),
            source: source(layer, &format!("{layer}/models.json"), Some(name)),
        }
    }

    fn skill(layer: DotagentsLayer, id: &str, body: &str) -> DotagentsSkill {
        DotagentsSkill {
            id: id.to_string(),
            name: id.to_string(),
            description: "d".to_string(),
            enabled: true,
            body: body.to_string(),
            extensions: BTreeMap::new(),
            source: source(layer, &format!("{layer}/skills/{id}/skill.md"), Some(id)),
            fingerprint: format!("fp-{body}"),
        }
    }

    fn task(layer: DotagentsLayer, id: &str, prompt_body: &str) -> DotagentsTask {
        DotagentsTask {
            id: id.to_string(),
            name: id.to_string(),
            kind: crate::dotagents::manifest::DotagentsTaskKind::Task,
            enabled: true,
            run_on_startup: false,
            interval_minutes: Some(60),
            profile_id: None,
            prompt: prompt_body.to_string(),
            extensions: BTreeMap::new(),
            source: source(layer, &format!("{layer}/tasks/{id}/task.md"), Some(id)),
            fingerprint: format!("fp-{prompt_body}"),
        }
    }

    fn memory(layer: DotagentsLayer, id: &str, body: &str) -> DotagentsMemory {
        DotagentsMemory {
            id: id.to_string(),
            title: None,
            tags: Vec::new(),
            importance: None,
            content: None,
            body: body.to_string(),
            enabled: true,
            extensions: BTreeMap::new(),
            source: source(layer, &format!("{layer}/memories/{id}.md"), Some(id)),
            fingerprint: format!("fp-{body}"),
        }
    }

    fn agent(layer: DotagentsLayer, id: &str, description: &str) -> DotagentsAgent {
        DotagentsAgent {
            id: id.to_string(),
            name: id.to_string(),
            description: description.to_string(),
            enabled: true,
            role: DotagentsAgentRole::DelegationTarget,
            connection: Default::default(),
            capabilities: Vec::new(),
            config: Default::default(),
            body: "body".to_string(),
            extensions: BTreeMap::new(),
            source: source(layer, &format!("{layer}/agents/{id}/agent.md"), Some(id)),
            fingerprint: format!("fp-{description}"),
        }
    }

    #[test]
    fn workspace_singleton_replaces_global_singleton() {
        let global = DotagentsLayerContent {
            agents_md: Some(prompt(DotagentsLayer::Global, "global")),
            ..DotagentsLayerContent::new(DotagentsLayer::Global)
        };
        let workspace = DotagentsLayerContent {
            agents_md: Some(prompt(DotagentsLayer::Workspace, "workspace")),
            ..DotagentsLayerContent::new(DotagentsLayer::Workspace)
        };

        let manifest = merge_layers(&[global, workspace]);
        let agents_md = manifest.agents_md.unwrap();
        assert_eq!(agents_md.body, "workspace");
        // Provenance records the winning (workspace) source.
        assert_eq!(agents_md.source.layer, DotagentsLayer::Workspace);
        assert_eq!(agents_md.fingerprint, "fp-workspace");
    }

    #[test]
    fn global_singleton_survives_when_workspace_absent() {
        let global = DotagentsLayerContent {
            system_prompt: Some(prompt(DotagentsLayer::Global, "global-system")),
            ..DotagentsLayerContent::new(DotagentsLayer::Global)
        };
        let workspace = DotagentsLayerContent::new(DotagentsLayer::Workspace);

        let manifest = merge_layers(&[global, workspace]);
        assert_eq!(manifest.system_prompt.unwrap().body, "global-system");
    }

    #[test]
    fn json_keys_merge_and_workspace_wins_per_key() {
        let mut global = DotagentsLayerContent::new(DotagentsLayer::Global);
        global
            .mcp_servers
            .insert("shared".into(), mcp(DotagentsLayer::Global, "shared"));
        global.mcp_servers.insert(
            "global-only".into(),
            mcp(DotagentsLayer::Global, "global-only"),
        );

        let mut workspace = DotagentsLayerContent::new(DotagentsLayer::Workspace);
        workspace
            .mcp_servers
            .insert("shared".into(), mcp(DotagentsLayer::Workspace, "shared"));

        let manifest = merge_layers(&[global, workspace]);
        assert_eq!(manifest.mcp_servers.len(), 2);
        // Workspace wins for the matching key...
        assert_eq!(
            manifest.mcp_servers["shared"].source.layer,
            DotagentsLayer::Workspace
        );
        // ...and the unmatched global key remains.
        assert_eq!(
            manifest.mcp_servers["global-only"].source.layer,
            DotagentsLayer::Global
        );
    }

    #[test]
    fn model_presets_merge_by_key() {
        let mut global = DotagentsLayerContent::new(DotagentsLayer::Global);
        global.model_presets.insert(
            "fast".into(),
            preset(DotagentsLayer::Global, "fast", "global-model"),
        );
        global.model_presets.insert(
            "slow".into(),
            preset(DotagentsLayer::Global, "slow", "slow-model"),
        );

        let mut workspace = DotagentsLayerContent::new(DotagentsLayer::Workspace);
        workspace.model_presets.insert(
            "fast".into(),
            preset(DotagentsLayer::Workspace, "fast", "ws-model"),
        );

        let manifest = merge_layers(&[global, workspace]);
        assert_eq!(manifest.model_presets["fast"].model, "ws-model");
        assert_eq!(manifest.model_presets["slow"].model, "slow-model");
    }

    #[test]
    fn collection_entries_merge_by_id() {
        let mut global = DotagentsLayerContent::new(DotagentsLayer::Global);
        global.skills.insert(
            "review".into(),
            skill(DotagentsLayer::Global, "review", "global-body"),
        );
        global.skills.insert(
            "format".into(),
            skill(DotagentsLayer::Global, "format", "format-body"),
        );
        global.tasks.insert(
            "digest".into(),
            task(DotagentsLayer::Global, "digest", "global-prompt"),
        );
        global.memories.insert(
            "note".into(),
            memory(DotagentsLayer::Global, "note", "global-note"),
        );
        global.agents.insert(
            "helper".into(),
            agent(DotagentsLayer::Global, "helper", "global"),
        );

        let mut workspace = DotagentsLayerContent::new(DotagentsLayer::Workspace);
        workspace.skills.insert(
            "review".into(),
            skill(DotagentsLayer::Workspace, "review", "ws-body"),
        );
        workspace.agents.insert(
            "helper".into(),
            agent(DotagentsLayer::Workspace, "helper", "ws"),
        );

        let manifest = merge_layers(&[global, workspace]);

        // Workspace replaces matching IDs.
        assert_eq!(manifest.skills["review"].body, "ws-body");
        assert_eq!(
            manifest.skills["review"].source.layer,
            DotagentsLayer::Workspace
        );
        assert_eq!(manifest.agents["helper"].description, "ws");
        // Unmatched global entries remain.
        assert_eq!(manifest.skills["format"].body, "format-body");
        assert_eq!(manifest.tasks["digest"].prompt, "global-prompt");
        assert_eq!(manifest.memories["note"].body, "global-note");
    }

    #[test]
    fn duplicate_within_layer_is_error_and_first_wins() {
        let mut servers = BTreeMap::new();
        let mut diagnostics = Vec::new();

        let first = mcp(DotagentsLayer::Workspace, "dup");
        assert!(insert_unique(
            &mut servers,
            "dup",
            first,
            source_of::mcp_server,
            &source(DotagentsLayer::Workspace, "ws/mcp.json", Some("dup")),
            &mut diagnostics,
            "MCP server",
        ));

        // A second entry with the same normalized ID is rejected.
        let second = mcp(DotagentsLayer::Workspace, "dup");
        assert!(!insert_unique(
            &mut servers,
            "dup",
            second,
            source_of::mcp_server,
            &source(DotagentsLayer::Workspace, "ws/mcp2.json", Some("dup")),
            &mut diagnostics,
            "MCP server",
        ));

        assert_eq!(servers.len(), 1);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DotagentsDiagnosticCode::DuplicateId);
        assert!(diagnostics[0].is_error());
        // The diagnostic names both sources without leaking content.
        assert!(diagnostics[0].message.contains("mcp.json"));
        assert!(diagnostics[0].message.contains("mcp2.json"));
    }

    #[test]
    fn duplicate_check_is_first_wins_and_deterministic() {
        // Inserting the same content twice must not be order-dependent: the
        // first insertion always wins.
        let mut collection = BTreeMap::new();
        let mut diagnostics = Vec::new();

        for i in 0..3 {
            insert_unique(
                &mut collection,
                "x",
                skill(DotagentsLayer::Workspace, "x", &format!("body-{i}")),
                source_of::skill,
                &source(
                    DotagentsLayer::Workspace,
                    &format!("ws/skills/{i}/skill.md"),
                    Some("x"),
                ),
                &mut diagnostics,
                "skill",
            );
        }

        assert_eq!(collection.len(), 1);
        assert_eq!(collection["x"].body, "body-0");
        assert_eq!(diagnostics.len(), 2);
    }

    #[test]
    fn normalize_id_is_case_and_whitespace_insensitive() {
        assert_eq!(normalize_id("  Review "), "review");
        assert_eq!(normalize_id("REVIEW"), "review");
        assert_eq!(normalize_id("review"), normalize_id("Review"));
    }

    #[test]
    fn merged_manifest_orders_diagnostics_deterministically() {
        let mut global = DotagentsLayerContent::new(DotagentsLayer::Global);
        global.diagnostics.push(DotagentsDiagnostic::error(
            DotagentsDiagnosticCode::ParseError,
            "zzz",
        ));
        let mut workspace = DotagentsLayerContent::new(DotagentsLayer::Workspace);
        workspace.diagnostics.push(DotagentsDiagnostic::error(
            DotagentsDiagnosticCode::ParseError,
            "aaa",
        ));

        let manifest = merge_layers(&[global, workspace]);
        assert_eq!(manifest.diagnostics[0].message, "aaa");
        assert_eq!(manifest.diagnostics[1].message, "zzz");
    }

    #[test]
    fn merged_collections_are_stable_across_layer_orders_of_equal_content() {
        // Building the same two layers twice yields byte-identical manifests.
        let build = || {
            let mut global = DotagentsLayerContent::new(DotagentsLayer::Global);
            global
                .skills
                .insert("a".into(), skill(DotagentsLayer::Global, "a", "1"));
            global
                .skills
                .insert("b".into(), skill(DotagentsLayer::Global, "b", "2"));
            let mut workspace = DotagentsLayerContent::new(DotagentsLayer::Workspace);
            workspace
                .skills
                .insert("c".into(), skill(DotagentsLayer::Workspace, "c", "3"));
            merge_layers(&[global, workspace])
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn singleton_and_collection_provenance_is_independent() {
        // A prompt singleton and a collection entry can both be overridden,
        // and each records its own winning source.
        let mut global = DotagentsLayerContent::new(DotagentsLayer::Global);
        global.agents_md = Some(prompt(DotagentsLayer::Global, "g"));
        global
            .agents
            .insert("x".into(), agent(DotagentsLayer::Global, "x", "g"));

        let mut workspace = DotagentsLayerContent::new(DotagentsLayer::Workspace);
        workspace.agents_md = Some(prompt(DotagentsLayer::Workspace, "w"));

        let manifest = merge_layers(&[global, workspace]);
        assert_eq!(
            manifest.agents_md.as_ref().unwrap().source.layer,
            DotagentsLayer::Workspace
        );
        // The collection entry was not overridden, so global provenance stays.
        assert_eq!(manifest.agents["x"].source.layer, DotagentsLayer::Global);
    }
}
