//! Neutral sub-agent runtime plans derived from protocol agent profiles.
//!
//! Protocol `.agents/agents/<entry>/agent.md` profiles declare delegation
//! targets. This module converts the resolved manifest into a **neutral plan**
//! — a description of the runtime inputs a later phase would need — without
//! constructing an agent, registering a handle, launching a process, or
//! touching storage. Registry integration (tasks 3.2–3.6) consumes the plan.
//!
//! The plan is deliberately runtime-agnostic so the same conversion serves
//! standalone agents, quorum delegates, direct builders, and future hosts. It
//! carries:
//!
//! - a stable normalized ID and the advertised [`AgentInfo`] metadata;
//! - the profile body as the target's agent-specific system prompt;
//! - the resolved model overlay selected from `config.json`'s preset name;
//! - tool and MCP restrictions, resolved against the effective manifest;
//! - provenance of the contributing files.
//!
//! Everything that cannot be represented is reported as a source-aware
//! diagnostic and skipped rather than silently dropped. In particular:
//!
//! - `enabled: false` profiles are skipped without diagnostics;
//! - profiles whose role is not `delegation-target` are skipped;
//! - `stdio`/`executable` and unknown connection types never launch a process;
//! - unknown model presets, unresolvable MCP references, and unsupported
//!   `config.json` fields are diagnosed while leaving sibling targets intact.

use super::adapters::{DotagentsLlmOverlay, select_model_overlay};
use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::layer::DotagentsSource;
use super::manifest::{
    DotagentsAgent, DotagentsAgentConnectionType, DotagentsAgentRole, DotagentsManifest,
    DotagentsMcpServer,
};
use crate::config::McpServerConfig;
use crate::delegation::AgentInfo;
use std::collections::BTreeMap;

/// Fields honored in an agent's adjacent `config.json`.
///
/// Anything else is retained in `extensions` and reported as an unsupported
/// field so embedders can see why it had no effect.
const SUPPORTED_AGENT_CONFIG_FIELDS: &[&str] =
    &["model", "modelpreset", "tools", "mcpservers", "mcp"];

/// A neutral, side-effect-free plan for one protocol sub-agent.
///
/// Constructing a plan never starts a runtime. Callers that decide to
/// materialize the target use the plan's fields to build an agent handle
/// through the normal QueryMT builders.
#[derive(Debug, Clone)]
pub struct DotagentsSubAgentPlan {
    /// Stable normalized target ID used for registry registration and
    /// collision resolution.
    pub id: String,
    /// Metadata advertised to delegation consumers.
    pub info: AgentInfo,
    /// The profile body, used as this target's agent-specific system prompt.
    /// Frontmatter is already stripped by the parser.
    pub system_prompt: String,
    /// Selected model overlay, when `config.json` names a resolvable preset.
    pub model: Option<DotagentsLlmOverlay>,
    /// Requested tool names, when the profile restricts tools.
    pub tools: Option<Vec<String>>,
    /// MCP server configurations this target may attach, ordered by name.
    ///
    /// Only entries referenced by `config.json` (or all enabled manifest
    /// servers when the profile does not restrict them) are included.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Provenance of the profile that produced this plan.
    pub source: DotagentsSource,
    /// Diagnostics gathered while converting the profile.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

/// The outcome of converting every protocol agent profile in a manifest.
#[derive(Debug, Default, Clone)]
pub struct DotagentsSubAgentPlans {
    /// Plans keyed by normalized target ID, ordered by ID.
    pub plans: BTreeMap<String, DotagentsSubAgentPlan>,
    /// Diagnostics not attributable to a single skipped profile.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

impl DotagentsSubAgentPlans {
    /// Whether any plan was produced.
    pub fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }

    /// Look up a plan by normalized ID.
    pub fn get(&self, id: &str) -> Option<&DotagentsSubAgentPlan> {
        self.plans.get(id)
    }

    /// Convert every enabled, supported protocol agent profile.
    ///
    /// Disabled and non-delegation roles are skipped. Each skipped or degraded
    /// target contributes source-aware diagnostics; a profile that cannot be
    /// converted never prevents sibling profiles from being planned.
    pub fn from_manifest(manifest: &DotagentsManifest) -> Self {
        let mut outcome = Self::default();
        for agent in manifest.agents.values() {
            match plan_agent(manifest, agent) {
                Ok(Some(plan)) => {
                    outcome.plans.insert(plan.id.clone(), plan);
                }
                Ok(None) => {}
                Err(diagnostic) => outcome.diagnostics.push(diagnostic),
            }
        }
        outcome
    }
}

/// Convert a single protocol agent profile into a neutral plan.
///
/// Returns `Ok(None)` when the profile is intentionally inactive (disabled, or
/// not a delegation target). Returns `Err` only when the profile as a whole
/// cannot be represented, so callers can isolate the failure.
pub fn plan_agent(
    manifest: &DotagentsManifest,
    agent: &DotagentsAgent,
) -> Result<Option<DotagentsSubAgentPlan>, DotagentsDiagnostic> {
    if !agent.enabled {
        // Intentionally inactive: no diagnostic, matching MCP disabled entries.
        return Ok(None);
    }
    if agent.role != DotagentsAgentRole::DelegationTarget {
        return Ok(None);
    }

    let mut diagnostics = Vec::new();

    // Connection type gates everything else: an unsupported connection must not
    // be planned at all, and never launches a process.
    match agent.connection.connection_type {
        DotagentsAgentConnectionType::Internal => {}
        DotagentsAgentConnectionType::Stdio => {
            return Ok(Some(unsupported_connection_plan(agent, "stdio")));
        }
        DotagentsAgentConnectionType::Unknown => {
            let declared = agent
                .connection
                .declared_type
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            return Ok(Some(unsupported_connection_plan(agent, &declared)));
        }
    }

    for field in agent.config.extensions.keys() {
        if SUPPORTED_AGENT_CONFIG_FIELDS.contains(&field.to_ascii_lowercase().as_str()) {
            continue;
        }
        diagnostics.push(
            DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::UnsupportedTransport,
                format!(
                    "agent profile `{}` sets unsupported `config.json` field `{field}`; it has no effect",
                    agent.id
                ),
            )
            .with_source(agent.source.clone()),
        );
    }

    let model = match &agent.config.model_preset {
        Some(name) => match select_model_overlay(manifest, name) {
            Ok(overlay) => Some(overlay),
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                None
            }
        },
        None => None,
    };

    let mut mcp_diagnostics = Vec::new();
    let mcp_servers = resolve_mcp_servers(manifest, agent, &mut mcp_diagnostics);
    diagnostics.extend(mcp_diagnostics);

    let info = AgentInfo {
        id: agent.id.clone(),
        name: agent.name.clone(),
        description: agent.description.clone(),
        capabilities: agent.capabilities.clone(),
        required_capabilities: Vec::new(),
        meta: Some(serde_json::json!({
            "source": agent.source.to_string(),
            "origin": "dotagents",
        })),
    };

    let mut plan = DotagentsSubAgentPlan {
        id: agent.id.clone(),
        info,
        system_prompt: agent.body.clone(),
        model,
        tools: agent.config.tools.clone(),
        mcp_servers,
        source: agent.source.clone(),
        diagnostics,
    };
    plan.diagnostics.sort_by(|a, b| a.message.cmp(&b.message));
    Ok(Some(plan))
}

/// Build an inactive plan for an unsupported connection type.
///
/// The plan retains metadata and restrictions so the target stays inspectable,
/// but carries an error diagnostic and no MCP servers or model overlay, making
/// it clear the target must not be materialized.
fn unsupported_connection_plan(agent: &DotagentsAgent, declared: &str) -> DotagentsSubAgentPlan {
    let command = agent
        .connection
        .command
        .as_deref()
        .map(|command| format!(" (command `{command}` is never launched)"))
        .unwrap_or_default();
    let diagnostic = DotagentsDiagnostic::error(
        DotagentsDiagnosticCode::UnsupportedTransport,
        format!(
            "agent profile `{}` requests `{declared}` connection, which QueryMT cannot represent as a \
             delegation target{command}; the profile is retained for inspection but is not registered",
            agent.id
        ),
    )
    .with_source(agent.source.clone());

    DotagentsSubAgentPlan {
        id: agent.id.clone(),
        info: AgentInfo {
            id: agent.id.clone(),
            name: agent.name.clone(),
            description: agent.description.clone(),
            capabilities: agent.capabilities.clone(),
            required_capabilities: Vec::new(),
            meta: Some(serde_json::json!({
                "source": agent.source.to_string(),
                "origin": "dotagents",
                "unsupportedConnection": declared,
            })),
        },
        system_prompt: agent.body.clone(),
        model: None,
        tools: None,
        mcp_servers: Vec::new(),
        source: agent.source.clone(),
        diagnostics: vec![diagnostic],
    }
}

/// Resolve the MCP servers a protocol target may attach.
///
/// When `config.json` restricts `mcpServers`, only those named entries are
/// converted; unknown names are diagnosed. Otherwise every enabled, supported
/// manifest server is included so a target can use the workspace's shared MCP
/// configuration.
fn resolve_mcp_servers(
    manifest: &DotagentsManifest,
    agent: &DotagentsAgent,
    diagnostics: &mut Vec<DotagentsDiagnostic>,
) -> Vec<McpServerConfig> {
    let mut servers = Vec::new();

    let selected: Vec<(&String, &DotagentsMcpServer)> = match &agent.config.mcp_servers {
        Some(names) => names
            .iter()
            .map(|name| (name, manifest.mcp_servers.get(name)))
            .filter_map(|(name, server)| match server {
                Some(server) => Some((name, server)),
                None => {
                    diagnostics.push(
                        DotagentsDiagnostic::error(
                            DotagentsDiagnosticCode::UnresolvedReference,
                            format!(
                                "agent profile `{}` references MCP server `{name}`, which no enabled protocol layer defines",
                                agent.id
                            ),
                        )
                        .with_source(agent.source.clone()),
                    );
                    None
                }
            })
            .collect(),
        None => manifest
            .mcp_servers
            .iter()
            .filter(|(_, server)| server.enabled)
            .collect(),
    };

    for (_name, server) in selected {
        match super::adapters::convert_server_with_diagnostics(server) {
            Ok((Some(config), warnings)) => {
                servers.push(config);
                diagnostics.extend(warnings);
            }
            Ok((None, warnings)) => diagnostics.extend(warnings),
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }

    servers.sort_by(|a, b| a.name().cmp(b.name()));
    servers
}
