//! `.agents` Protocol support (draft).
//!
//! This module discovers, parses, layers, and adapts portable `.agents/`
//! configuration into QueryMT runtime inputs. Discovery and parsing are
//! deliberately **side-effect free**: [`DotagentsLoader`] produces a
//! [`DotagentsManifest`] that callers can preview without starting MCP
//! servers, scheduling tasks, or ingesting memories. Runtime adaptation and
//! persistence reconciliation happen in later, explicitly invoked phases.
//!
//! The upstream protocol is a draft that describes conventions rather than a
//! complete versioned schema. This module therefore:
//!
//! - keeps protocol types isolated from runtime construction;
//! - retains or diagnoses unknown fields for forward compatibility;
//! - attaches source-aware diagnostics to every parse, merge, and adaptation
//!   outcome;
//! - redacts resolved secret values from public/manifest views.
//!
//! Protocol support is opt-in. When [`DotagentsLoadOptions::enabled`] is
//! `false`, or when no protocol directory exists, existing QueryMT behavior is
//! preserved and no `.agents` directory is created.

mod adapters;
mod automation;
mod confine;
mod coordinator;
mod diagnostics;
mod discovery;
mod frontmatter;
mod layer;
mod loader;
mod manifest;
mod memory;
mod merge;
mod options;
mod parsers;
mod reconcile;
mod settings;
mod subagent;
mod target_registry;
mod tasks;

pub use adapters::{
    DotagentsLlmOverlay, DotagentsMcpPlan, DotagentsMcpServerView, DotagentsModelPresetView,
    RedactedValue, apply_selected_model_preset, convert_server,
    convert_server_with_workspace_stdio_approval, select_model_overlay, validate_preset_provider,
};
pub use automation::{
    AUTOMATION_SESSION_KIND, DotagentsAutomationIdentity, DotagentsAutomationRepository,
    SqliteDotagentsAutomationRepository, bind_automation_session_profile,
};
pub use confine::{ConfineResult, ConfinementPolicy, read_confined_text};
pub use coordinator::{
    DotagentsActivatedTask, DotagentsActivationContext, DotagentsActivationReport,
    DotagentsExecutionTarget, DotagentsExecutionTargetResolver, DotagentsRuntimeCoordinator,
    DotagentsStartupTrigger, ensure_target_session,
};
pub use diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode, DotagentsSeverity};
pub use discovery::{
    PROTOCOL_DIR_NAME, ResolvedLayerRoot, default_global_root, default_workspace_root,
    discover_layer_roots,
};
pub use frontmatter::{
    ParsedFrontmatter, field_bool, field_list, field_present, field_string, field_u64,
    parse_frontmatter_markdown,
};
pub use layer::{DotagentsLayer, DotagentsSource, DotagentsSourceRef};
pub use loader::{DotagentsLoadError, DotagentsLoader, assemble_layer};
pub(crate) use loader::{compose_prompt, resolve_for_builder};
pub use manifest::{
    DotagentsAgent, DotagentsAgentConfig, DotagentsAgentConnection, DotagentsAgentConnectionType,
    DotagentsAgentRole, DotagentsManifest, DotagentsMcpServer, DotagentsMcpTransport,
    DotagentsMemory, DotagentsModelPreset, DotagentsPrompt, DotagentsSkill, DotagentsTask,
    DotagentsTaskKind, DotagentsUnsupported, DotagentsUnsupportedKind,
};
pub use memory::{
    DotagentsMemoryDeactivation, DotagentsMemoryDeactivationReason, DotagentsMemoryIngest,
    DotagentsMemoryPlan, DotagentsMemoryReconcileAction, DotagentsMemoryReconcileOutcome,
    DotagentsMemoryReconcileReport, DotagentsMemorySkip, DotagentsStoredMemory,
    decide_memory_reconciliation, memory_source_key, memory_summary, memory_text,
    normalize_importance, plan_memory_ingest as plan_memory, reconcile_memories,
    reconcile_memory_plan,
};
pub use reconcile::{DotagentsTaskApplied, DotagentsTaskReconciler};

/// The knowledge scope protocol memories import into.
///
/// Protocol memories are workspace knowledge, so they share the scope the agent
/// already uses for a working directory. This keeps imported memories
/// retrievable through the normal knowledge path instead of isolating them in a
/// protocol-only namespace.
pub fn protocol_knowledge_scope(workspace: Option<&std::path::Path>) -> String {
    workspace
        .map(|path| {
            std::fs::canonicalize(path)
                .unwrap_or_else(|_| lexical_normalize_path(path))
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "global".to_string())
}

fn lexical_normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;

    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = std::path::PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::ParentDir) | None => normalized.push(component.as_os_str()),
                Some(Component::CurDir) => {
                    unreachable!("components omit current-directory entries")
                }
            },
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}
pub use merge::{DotagentsLayerContent, insert_unique, merge_layers, normalize_id, source_of};
pub use options::{DotagentsLoadOptions, DotagentsStrictness};
pub use parsers::{
    ParsedAgentDocument, ParsedMcpDocument, ParsedModelsDocument, fingerprint, parse_agent_config,
    parse_agent_document, parse_mcp_document, parse_memory_document, parse_models_document,
    parse_prompt_document, parse_skill_document, parse_task_document,
};
pub use settings::DotagentsSettings;
pub use subagent::{DotagentsSubAgentPlan, DotagentsSubAgentPlans, plan_agent as plan_sub_agent};
pub use target_registry::{
    DotagentsTargetMerge, DotagentsTargetRegistry, ProtocolTargetFactory, merge_protocol_targets,
    protocol_layer_collision,
};
pub use tasks::{
    DotagentsActivationDisposition, DotagentsActivationFacility, DotagentsAutomationBinding,
    DotagentsScheduleRecord, DotagentsStartupFire, DotagentsStartupPlan,
    DotagentsStartupSkipReason, DotagentsTaskActivation, DotagentsTaskActivationError,
    DotagentsTaskApproval, DotagentsTaskApprovalDecision, DotagentsTaskApprovalRequest,
    DotagentsTaskApprover, DotagentsTaskIdentity, DotagentsTaskOwnership,
    DotagentsTaskReconcileAction, DotagentsTaskReconcileOutcome, DotagentsTaskRecord,
    DotagentsTaskRetireAction, DotagentsTaskRetireOutcome, DotagentsTaskRetireReason,
    DotagentsTaskStateRepository, DotagentsTaskTrustOutcome, DotagentsTaskTrustPolicy,
    SqliteDotagentsTaskStateRepository, classify_activation_unavailable,
    decide_task_reconciliation, evaluate_task_trust, plan_startup_fire, plan_task_activation,
    plan_task_retirement, summarize_prompt, task_execution_fingerprint,
};

#[cfg(test)]
mod target_registry_tests;
#[cfg(test)]
mod tests;
