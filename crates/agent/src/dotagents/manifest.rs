//! Typed document models and the side-effect-free resolved manifest.
//!
//! The manifest is the single inspectable output of discovery, parsing, and
//! layering. It does **not** start MCP servers, schedule tasks, or ingest
//! memories; those actions are performed later by the runtime adapters and the
//! reconciler.

use super::diagnostics::DotagentsDiagnostic;
use super::layer::{DotagentsSource, DotagentsSourceRef};
use std::collections::BTreeMap;

/// A Markdown singleton document (`agents.md` or `system-prompt.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsPrompt {
    /// Frontmatter-derived metadata retained for inspection.
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// The Markdown body with any frontmatter fence removed.
    pub body: String,
    /// Where the effective document came from.
    pub source: DotagentsSource,
    /// Content fingerprint of the raw source.
    pub fingerprint: String,
}

impl DotagentsPrompt {
    /// Whether the body is empty after trimming.
    pub fn is_empty(&self) -> bool {
        self.body.trim().is_empty()
    }
}

/// A protocol MCP transport value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsMcpTransport {
    /// Spawn a local process and speak MCP over stdio.
    Stdio,
    /// Connect over the streamable HTTP transport.
    StreamableHttp,
    /// WebSocket transport; not representable by the current runtime.
    WebSocket,
    /// An unknown future transport spelling.
    Unknown,
}

impl DotagentsMcpTransport {
    /// Parse a protocol transport spelling (case-insensitive, accepting
    /// `_`/`-` variations).
    pub fn parse(raw: &str) -> Self {
        let normalized = raw.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "stdio" => DotagentsMcpTransport::Stdio,
            "streamable-http" | "streamablehttp" | "http" => DotagentsMcpTransport::StreamableHttp,
            "websocket" | "ws" | "wss" => DotagentsMcpTransport::WebSocket,
            _ => DotagentsMcpTransport::Unknown,
        }
    }

    /// Stable canonical spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            DotagentsMcpTransport::Stdio => "stdio",
            DotagentsMcpTransport::StreamableHttp => "streamable-http",
            DotagentsMcpTransport::WebSocket => "websocket",
            DotagentsMcpTransport::Unknown => "unknown",
        }
    }

    /// Whether the current QueryMT runtime can represent this transport.
    pub fn is_supported(self) -> bool {
        matches!(
            self,
            DotagentsMcpTransport::Stdio | DotagentsMcpTransport::StreamableHttp
        )
    }
}

impl std::fmt::Display for DotagentsMcpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed MCP server entry from `mcp.json`.
///
/// `env` and `headers` may contain resolved secrets, so this type does not
/// derive `Debug`; its manual implementation masks those values.
#[derive(Clone, PartialEq, Eq)]
pub struct DotagentsMcpServer {
    /// Normalized server name (the `mcpServers` key).
    pub name: String,
    /// The resolved or inferred transport.
    pub transport: DotagentsMcpTransport,
    /// The raw transport spelling supplied by the document, if any.
    pub declared_transport: Option<String>,
    /// Executable to run for `stdio` servers.
    pub command: Option<String>,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
    /// Environment variables for `stdio` servers (values may reference
    /// environment variables and are redacted in public views).
    pub env: BTreeMap<String, String>,
    /// URL for HTTP servers.
    pub url: Option<String>,
    /// Supported HTTP headers.
    pub headers: BTreeMap<String, String>,
    /// Whether the entry is enabled (defaults to true).
    pub enabled: bool,
    /// Unknown fields retained for forward compatibility.
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Where the effective entry came from.
    pub source: DotagentsSource,
}

impl DotagentsMcpServer {
    /// Whether this server can be started by the current runtime.
    pub fn is_supported(&self) -> bool {
        self.enabled && self.transport.is_supported()
    }
}

/// A named model preset from `models.json`.
///
/// `credential` and `parameters` may contain resolved secrets, so this type
/// does not derive `Debug`; its manual implementation masks those values.
#[derive(Clone, PartialEq, Eq)]
pub struct DotagentsModelPreset {
    /// Preset name (the object key).
    pub name: String,
    /// Provider identifier.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Optional credential reference (environment reference recommended).
    pub credential: Option<String>,
    /// Provider parameters applied as an overlay.
    pub parameters: BTreeMap<String, serde_json::Value>,
    /// Unknown fields retained for forward compatibility.
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Where the effective entry came from.
    pub source: DotagentsSource,
}

/// A protocol skill entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsSkill {
    /// Stable normalized ID (directory name unless `id` is provided).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description used for skill discovery.
    pub description: String,
    /// Whether the skill is enabled (defaults to true).
    pub enabled: bool,
    /// The skill Markdown body.
    pub body: String,
    /// Unknown frontmatter fields retained for forward compatibility.
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Where the effective entry came from.
    pub source: DotagentsSource,
    /// Content fingerprint of the raw source.
    pub fingerprint: String,
}

/// The role a protocol agent profile declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsAgentRole {
    /// A profile intended to be used as a delegation target.
    DelegationTarget,
    /// Any other or unknown role.
    Other,
}

impl DotagentsAgentRole {
    /// Parse a protocol role spelling.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "delegation-target" => DotagentsAgentRole::DelegationTarget,
            _ => DotagentsAgentRole::Other,
        }
    }
}

/// How a protocol agent connects to its runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsAgentConnectionType {
    /// Reuse QueryMT's local runtime (fully supported).
    Internal,
    /// An external executable/ACP process (not launched by this crate).
    Stdio,
    /// An unknown future connection type.
    Unknown,
}

impl DotagentsAgentConnectionType {
    /// Parse a protocol connection-type spelling.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "internal" => DotagentsAgentConnectionType::Internal,
            "stdio" | "executable" => DotagentsAgentConnectionType::Stdio,
            _ => DotagentsAgentConnectionType::Unknown,
        }
    }
}

/// The declared connection for a protocol agent profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsAgentConnection {
    /// The connection type.
    pub connection_type: DotagentsAgentConnectionType,
    /// The raw declared type, if any.
    pub declared_type: Option<String>,
    /// Referenced executable, when applicable (never launched).
    pub command: Option<String>,
}

impl Default for DotagentsAgentConnection {
    fn default() -> Self {
        Self {
            connection_type: DotagentsAgentConnectionType::Internal,
            declared_type: None,
            command: None,
        }
    }
}

/// Supported settings parsed from an agent's adjacent `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DotagentsAgentConfig {
    /// Selected model preset name.
    pub model_preset: Option<String>,
    /// Allowed tool names, when restricted.
    pub tools: Option<Vec<String>>,
    /// Referenced MCP server names, when restricted.
    pub mcp_servers: Option<Vec<String>>,
    /// Unknown fields retained for inspection and diagnostics.
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// A protocol agent profile from `.agents/agents/<id>/agent.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsAgent {
    /// Stable normalized ID (directory name unless metadata provides one).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description advertised to delegation consumers.
    pub description: String,
    /// Whether the profile is enabled (defaults to true).
    pub enabled: bool,
    /// Declared role.
    pub role: DotagentsAgentRole,
    /// Declared connection.
    pub connection: DotagentsAgentConnection,
    /// Capabilities advertised to delegation consumers.
    pub capabilities: Vec<String>,
    /// Supported settings from adjacent `config.json`.
    pub config: DotagentsAgentConfig,
    /// The profile Markdown body; becomes that target's system prompt.
    pub body: String,
    /// Unknown frontmatter fields retained for forward compatibility.
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Where the effective entry came from.
    pub source: DotagentsSource,
    /// Content fingerprint of the raw source.
    pub fingerprint: String,
}

/// The `kind` discriminator for protocol task documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsTaskKind {
    /// A repeat task (`kind: task`).
    Task,
    /// Any other or unknown kind.
    Other,
}

impl DotagentsTaskKind {
    /// Parse a protocol task kind spelling.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "task" => DotagentsTaskKind::Task,
            _ => DotagentsTaskKind::Other,
        }
    }
}

/// A protocol repeat task from `.agents/tasks/<id>/task.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTask {
    /// Stable normalized ID (directory name unless metadata provides one).
    pub id: String,
    /// Display name.
    pub name: String,
    /// The `kind` discriminator.
    pub kind: DotagentsTaskKind,
    /// Whether the task is enabled (defaults to true).
    pub enabled: bool,
    /// Whether the task fires once per runtime startup.
    pub run_on_startup: bool,
    /// Interval in minutes, when an interval schedule is requested.
    pub interval_minutes: Option<u64>,
    /// Optional target profile/model reference.
    pub profile_id: Option<String>,
    /// The prompt body executed by the task.
    pub prompt: String,
    /// Unknown frontmatter fields retained for forward compatibility.
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Where the effective entry came from.
    pub source: DotagentsSource,
    /// Content fingerprint of the effective source.
    pub fingerprint: String,
}

/// A protocol memory from `.agents/memories/<id>.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsMemory {
    /// Stable normalized ID.
    pub id: String,
    /// Optional title.
    pub title: Option<String>,
    /// Topics/tags.
    pub tags: Vec<String>,
    /// Protocol importance value, when provided.
    pub importance: Option<String>,
    /// Explicit content override from metadata, when provided.
    pub content: Option<String>,
    /// The Markdown body.
    pub body: String,
    /// Whether the memory is enabled (defaults to true).
    pub enabled: bool,
    /// Unknown frontmatter fields retained for forward compatibility.
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Where the effective entry came from.
    pub source: DotagentsSource,
    /// Content fingerprint of the effective source.
    pub fingerprint: String,
}

/// A category of unsupported protocol content retained for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsUnsupportedKind {
    /// `speakmcp-settings.json` (out of scope).
    SpeakMcpSettings,
    /// `layouts/` preferences (out of scope).
    Layouts,
    /// `.backups/` rotation content (out of scope).
    Backups,
    /// A top-level entry this implementation does not recognize.
    Unknown,
}

/// An unsupported but detected protocol artifact, retained for inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsUnsupported {
    /// The category of the unsupported artifact.
    pub kind: DotagentsUnsupportedKind,
    /// A short identifier (typically the top-level entry name).
    pub id: String,
    /// Where it was detected.
    pub source: DotagentsSource,
}

/// The fully layered, side-effect-free resolved manifest.
///
/// All collections are deterministically ordered and keyed by stable ID. The
/// manifest records the effective source of every value, plus diagnostics
/// gathered during parse and merge. It is safe to inspect without triggering
/// MCP startup, scheduling, or persistence.
///
/// `Debug` is implemented manually (rather than derived) so that formatting a
/// manifest can never expose resolved secrets from MCP or model entries.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DotagentsManifest {
    /// Discovered layer roots and their existence.
    pub layers: Vec<DotagentsSourceRef>,
    /// The effective `agents.md` document, if present in any enabled layer.
    pub agents_md: Option<DotagentsPrompt>,
    /// The effective `system-prompt.md` document, if present.
    pub system_prompt: Option<DotagentsPrompt>,
    /// Effective MCP servers keyed by normalized name.
    pub mcp_servers: BTreeMap<String, DotagentsMcpServer>,
    /// Effective model presets keyed by preset name.
    pub model_presets: BTreeMap<String, DotagentsModelPreset>,
    /// Effective skills keyed by normalized ID.
    pub skills: BTreeMap<String, DotagentsSkill>,
    /// Effective agent profiles keyed by normalized ID.
    pub agents: BTreeMap<String, DotagentsAgent>,
    /// Effective repeat tasks keyed by normalized ID.
    pub tasks: BTreeMap<String, DotagentsTask>,
    /// Effective memories keyed by normalized ID.
    pub memories: BTreeMap<String, DotagentsMemory>,
    /// Detected but unsupported protocol artifacts.
    pub unsupported: Vec<DotagentsUnsupported>,
    /// Deterministically ordered diagnostics.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

/// Masked rendering of secret-bearing maps for `Debug`/`Display` output.
///
/// Keys stay visible so configuration mistakes remain actionable, while values
/// are replaced with a fixed mask. Values that are *not* secrets (such as an
/// MCP command or URL) are printed directly by the caller.
fn write_masked<'a, I>(f: &mut std::fmt::Formatter<'_>, name: &str, keys: I) -> std::fmt::Result
where
    I: IntoIterator<Item = &'a String>,
{
    let keys: Vec<&String> = keys.into_iter().collect();
    if keys.is_empty() {
        return Ok(());
    }
    write!(f, " {name}={{")?;
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{key}: ***")?;
    }
    f.write_str("}")
}

impl std::fmt::Debug for DotagentsMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("DotagentsMcpServer");
        debug
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("declared_transport", &self.declared_transport)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("url", &self.url)
            .field("enabled", &self.enabled)
            .field("source", &self.source);
        // Never render env/header values: they may hold resolved secrets.
        let env_keys: Vec<String> = self.env.keys().cloned().collect();
        let header_keys: Vec<String> = self.headers.keys().cloned().collect();
        debug.field("env_keys", &env_keys);
        debug.field("header_keys", &header_keys);
        debug.finish()
    }
}

impl std::fmt::Display for DotagentsMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MCP server `{}` ({}, {}, from {})",
            self.name,
            self.transport,
            if self.enabled { "enabled" } else { "disabled" },
            self.source
        )?;
        if let Some(command) = &self.command {
            write!(f, " command={command}")?;
        }
        if let Some(url) = &self.url {
            write!(f, " url={url}")?;
        }
        write_masked(f, "env", self.env.keys())?;
        write_masked(f, "headers", self.headers.keys())
    }
}

impl std::fmt::Debug for DotagentsModelPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Credentials and parameter values may be secrets; only names/types are shown.
        let parameter_keys: Vec<String> = self.parameters.keys().cloned().collect();
        f.debug_struct("DotagentsModelPreset")
            .field("name", &self.name)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("credential", &self.credential.as_ref().map(|_| "***"))
            .field("parameter_keys", &parameter_keys)
            .field("source", &self.source)
            .finish()
    }
}

impl std::fmt::Display for DotagentsModelPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "model preset `{}` ({}/{}, from {})",
            self.name, self.provider, self.model, self.source
        )?;
        if self.credential.is_some() {
            f.write_str(" credential=***")?;
        }
        write_masked(f, "parameters", self.parameters.keys())
    }
}

impl std::fmt::Debug for DotagentsManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secret-bearing entries render through their own redacting Debug impls.
        f.debug_struct("DotagentsManifest")
            .field("layers", &self.layers)
            .field("agents_md", &self.agents_md)
            .field("system_prompt", &self.system_prompt)
            .field("mcp_servers", &self.mcp_servers)
            .field("model_presets", &self.model_presets)
            .field("skills", &self.skills)
            .field("agents", &self.agents)
            .field("tasks", &self.tasks)
            .field("memories", &self.memories)
            .field("unsupported", &self.unsupported)
            .field("diagnostics", &self.diagnostics)
            .finish()
    }
}

impl DotagentsManifest {
    /// Create an empty manifest.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether any enabled layer root exists.
    pub fn has_any_layer(&self) -> bool {
        self.layers.iter().any(|layer| layer.exists)
    }

    /// Whether the manifest contains no effective protocol content.
    pub fn is_empty(&self) -> bool {
        self.agents_md.is_none()
            && self.system_prompt.is_none()
            && self.mcp_servers.is_empty()
            && self.model_presets.is_empty()
            && self.skills.is_empty()
            && self.agents.is_empty()
            && self.tasks.is_empty()
            && self.memories.is_empty()
    }

    /// Whether any error diagnostic is present.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(DotagentsDiagnostic::is_error)
    }

    /// All error diagnostics.
    pub fn errors(&self) -> impl Iterator<Item = &DotagentsDiagnostic> {
        self.diagnostics.iter().filter(|d| d.is_error())
    }

    /// Look up a resolved model preset by name.
    pub fn model_preset(&self, name: &str) -> Option<&DotagentsModelPreset> {
        self.model_presets.get(name)
    }

    /// Sort all collections and diagnostics deterministically.
    ///
    /// Collections are already keyed by `BTreeMap`; this sorts the vector
    /// collections by their source identity so identical files always produce
    /// identical manifests.
    pub(crate) fn sort_deterministically(&mut self) {
        self.unsupported.sort_by(|a, b| {
            (a.source.layer, &a.source.lexical_path, &a.id).cmp(&(
                b.source.layer,
                &b.source.lexical_path,
                &b.id,
            ))
        });
        self.diagnostics.sort_by(|a, b| {
            let key = |d: &DotagentsDiagnostic| {
                (
                    d.severity,
                    d.code.as_str(),
                    d.source
                        .as_ref()
                        .map(|s| (s.layer, s.lexical_path.clone(), s.entry_id.clone())),
                    d.message.clone(),
                )
            };
            key(a).cmp(&key(b))
        });
    }
}
