//! Wire protocol types for UI WebSocket communication.
//!
//! Contains all message types exchanged between the UI client and server,
//! as well as supporting DTOs for sessions, models, and agents.

use crate::events::EventEnvelope;
use crate::index::FileIndexEntry;
use crate::session::projection::AuditView;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use typeshare::typeshare;

/// A block of content in a UI prompt (text or resource reference).
/// Typeshare-annotated: generated for TypeScript and Swift.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UiPromptBlock {
    Text {
        text: String,
    },
    ResourceLink {
        name: String,
        uri: String,
        #[serde(default)]
        description: Option<String>,
    },
}

/// Information about an available agent for the UI.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiAgentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
}

/// Routing mode for message distribution.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    Single,
    Broadcast,
}

/// Cached model list entry with canonical identity.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Canonical internal identifier (e.g., "hf:repo:file.gguf", "file:/path/to/model.gguf", or provider-specific ID)
    pub id: String,
    /// Human-readable display label
    pub label: String,
    /// Model source: "preset", "cached", "custom", "catalog"
    pub source: String,
    /// Provider name
    pub provider: String,
    /// Original model identifier (for backwards compatibility)
    pub model: String,
    /// Stable node id where this provider lives. `None` = local node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Human-readable node label for display purposes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_label: Option<String>,
    /// Model family/repo for grouping (e.g., "Qwen2.5-Coder-32B-Instruct")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Quantization level (e.g., "Q8_0", "Q6_K", "unknown")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilityEntry {
    pub provider: String,
    pub supports_custom_models: bool,
}

/// Recent model usage entry from event history.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentModelEntry {
    pub provider: String,
    pub model: String,
    pub last_used: String, // ISO 8601 timestamp
    pub use_count: u32,
}

/// OAuth authentication status for a provider.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthStatus {
    NotAuthenticated,
    Expired,
    Connected,
}

pub use crate::session::provider::AuthMethod;
pub use querymt_utils::OAuthFlowKind;

/// Provider entry for dashboard auth UI (supports both OAuth and API token auth).
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderEntry {
    pub provider: String,
    pub display_name: String,
    /// OAuth status (`None` if provider has no OAuth support)
    pub oauth_status: Option<OAuthStatus>,
    /// Whether a manually-entered API key is stored in the keyring
    pub has_stored_api_key: bool,
    /// Whether the environment variable for this provider is set
    pub has_env_api_key: bool,
    /// The environment variable name for this provider (e.g. "OPENAI_API_KEY")
    pub env_var_name: Option<String>,
    /// Whether this provider supports OAuth flows
    pub supports_oauth: bool,
    /// User's preferred auth method (`None` = auto/default)
    pub preferred_method: Option<AuthMethod>,
}

/// Summary of a session for listing.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Public ID of the parent session (if this is a child session)
    pub parent_session_id: Option<String>,
    /// Fork origin: "user" or "delegation"
    pub fork_origin: Option<String>,
    /// Whether this session has child sessions
    pub has_children: bool,
    /// Node label where this session lives. "local" for local sessions,
    /// peer hostname/label for remote sessions (display only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// Information about a remote node discovered in the kameo mesh.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteNodeInfo {
    /// Stable mesh node id (PeerId string).
    pub id: String,
    /// Human-readable label / hostname
    pub label: String,
    /// Node capabilities
    pub capabilities: Vec<String>,
    /// Number of active sessions on the node
    pub active_sessions: u32,
}

/// Group of sessions by working directory.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    pub cwd: Option<String>,
    pub sessions: Vec<SessionSummary>,
    pub latest_activity: Option<String>,
}

/// Messages from UI client to server.
/// Typeshare-annotated: generated for TypeScript and Swift.
#[typeshare]
#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UiClientMessage {
    Init,
    SetActiveAgent {
        agent_id: String,
    },
    SetRoutingMode {
        mode: RoutingMode,
    },
    NewSession {
        cwd: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
    },
    Prompt {
        prompt: Vec<UiPromptBlock>,
    },
    ListSessions,
    LoadSession {
        session_id: String,
    },
    DeleteSession {
        session_id: String,
    },
    ListAllModels {
        #[serde(default)]
        refresh: bool,
    },
    SetSessionModel {
        session_id: String,
        model_id: String,
        /// Optional mesh node id (PeerId string) that owns the provider. `None` = local.
        #[serde(default, alias = "node")]
        node_id: Option<String>,
    },
    /// Get recent models from event history
    GetRecentModels {
        #[serde(default)]
        limit_per_workspace: Option<u32>,
    },
    /// Request file index for @ mentions
    GetFileIndex,
    /// Request LLM config details by config_id
    GetLlmConfig {
        #[typeshare(serialized_as = "number")]
        config_id: i64,
    },
    /// Cancel the active session for the current agent
    CancelSession,
    /// Undo filesystem changes to a specific message point
    Undo {
        message_id: String,
    },
    /// Redo: restore filesystem to pre-undo state
    Redo,
    /// Fork the active session at a specific message boundary.
    ForkSession {
        message_id: String,
    },
    /// Subscribe to a session's event stream
    SubscribeSession {
        session_id: String,
        #[serde(default)]
        agent_id: Option<String>,
    },
    /// Unsubscribe from a session's event stream
    UnsubscribeSession {
        session_id: String,
    },
    /// Respond to an elicitation request
    ElicitationResponse {
        elicitation_id: String,
        action: String,
        #[typeshare(serialized_as = "any")]
        content: Option<serde_json::Value>,
    },
    /// List configured OAuth-capable providers and their auth status
    ListAuthProviders,
    /// Start OAuth login flow for provider
    #[serde(rename = "start_oauth_login")]
    StartOAuthLogin {
        provider: String,
    },
    /// Complete OAuth login flow using pasted callback URL/code
    #[serde(rename = "complete_oauth_login")]
    CompleteOAuthLogin {
        flow_id: String,
        response: String,
    },
    /// Disconnect OAuth credentials for provider
    #[serde(rename = "disconnect_oauth")]
    DisconnectOAuth {
        provider: String,
    },
    /// Set the agent's operating mode (build/plan/review)
    SetAgentMode {
        mode: String,
    },
    /// Get the current agent mode
    GetAgentMode,
    /// List remote nodes discovered in the kameo mesh
    ListRemoteNodes,
    /// List sessions on a specific remote node
    ListRemoteSessions {
        /// Stable node id (PeerId string) identifying the target node
        #[serde(alias = "node")]
        node_id: String,
    },
    /// Create a new session on a specific remote node
    CreateRemoteSession {
        /// Stable node id (PeerId string) identifying the target node
        #[serde(alias = "node")]
        node_id: String,
        /// Working directory on the remote machine (optional)
        cwd: Option<String>,
        /// Client-generated request ID for correlating the response
        #[serde(default)]
        request_id: Option<String>,
    },
    /// Attach an existing remote session to the local dashboard
    AttachRemoteSession {
        /// Stable node id (PeerId string) identifying the target node
        #[serde(alias = "node")]
        node_id: String,
        /// Session ID to attach
        session_id: String,
    },
    AddCustomModelFromHf {
        provider: String,
        repo: String,
        filename: String,
        #[serde(default)]
        display_name: Option<String>,
    },
    AddCustomModelFromFile {
        provider: String,
        file_path: String,
        #[serde(default)]
        display_name: Option<String>,
    },
    DeleteCustomModel {
        provider: String,
        model_id: String,
    },
    /// Set an API token for a provider (stored in SecretStore)
    #[serde(rename = "set_api_token")]
    SetApiToken {
        provider: String,
        api_key: String,
    },
    /// Clear a stored API token for a provider
    #[serde(rename = "clear_api_token")]
    ClearApiToken {
        provider: String,
    },
    /// Set the preferred auth method for a provider
    #[serde(rename = "set_auth_method")]
    SetAuthMethod {
        provider: String,
        method: AuthMethod,
    },
    /// Trigger an update of all OCI provider plugins.
    UpdatePlugins,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoStackFrame {
    pub message_id: String,
}

/// Result of updating a single OCI plugin, reported in `PluginUpdateComplete`.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUpdateResult {
    pub plugin_name: String,
    pub success: bool,
    pub message: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamCursor {
    #[serde(default)]
    #[typeshare(serialized_as = "number")]
    pub local_seq: u64,
    #[serde(default)]
    #[typeshare(serialized_as = "Record<string, number>")]
    pub remote_seq_by_source: HashMap<String, u64>,
}

// ============================================================================
// Typeshare mirror types for upstream crate types
// ============================================================================
// These mirror structs exist solely to generate TypeScript/Swift types for
// upstream types (from `querymt`, `querymt_utils`, etc.) that cannot be
// annotated with `#[typeshare]` directly because they live in separate crates
// without `typeshare` as a dependency.

/// Mirror of `querymt::Usage` for typeshare generation.
/// Fields match the serialized JSON shape of the upstream type.
///
/// Note: kept for typeshare output; may be unused in Rust code paths.
#[allow(dead_code)]
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: u32,
    #[serde(default)]
    pub cache_read: u32,
    #[serde(default)]
    pub cache_write: u32,
}

impl From<querymt::Usage> for UsageInfo {
    fn from(u: querymt::Usage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            reasoning_tokens: u.reasoning_tokens,
            cache_read: u.cache_read,
            cache_write: u.cache_write,
        }
    }
}

/// Mirror of `querymt::chat::FunctionTool` for typeshare generation.
/// Note: kept for typeshare output; may be unused in Rust code paths.
#[allow(dead_code)]
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionToolInfo {
    pub name: String,
    pub description: String,
    /// JSON Schema for the function parameters
    #[typeshare(serialized_as = "any")]
    pub parameters: serde_json::Value,
}

/// Mirror of `querymt::chat::Tool` for typeshare generation.
/// Note: kept for typeshare output; may be unused in Rust code paths.
#[allow(dead_code)]
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    /// The type of tool (e.g. "function")
    #[serde(rename = "type")]
    pub tool_type: String,
    /// The function definition
    pub function: FunctionToolInfo,
}

impl From<&querymt::chat::Tool> for ToolInfo {
    fn from(t: &querymt::chat::Tool) -> Self {
        Self {
            tool_type: t.tool_type.clone(),
            function: FunctionToolInfo {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                parameters: t.function.parameters.clone(),
            },
        }
    }
}

/// Known values for `EventOrigin`, exposed for TypeScript/Swift type safety.
///
/// The real `EventOrigin` has a custom Serialize/Deserialize impl that
/// serializes to plain strings (`"local"`, `"remote"`, or any other string
/// for the `Unknown` variant). The `Unknown(String)` catch-all prevents
/// standard serde enum derivation, so `#[typeshare]` can't be applied to
/// the original type. The `origin` fields on events use
/// `serialized_as = "string"` because any string value is valid at runtime.
///
/// This enum provides the known discriminants for TS/Swift code to compare against.
/// Note: kept for typeshare output; may be unused in Rust code paths.
#[allow(dead_code)]
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventOriginKind {
    Local,
    Remote,
}

/// Mirror of `querymt_utils::OAuthFlowKind` for typeshare generation.
/// Matches the serialized JSON values of the upstream enum.
/// Note: kept for typeshare output; may be unused in Rust code paths.
#[allow(dead_code)]
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthFlowKindTs {
    /// Redirect/callback flow where the user pastes the callback URL or code.
    RedirectCode,
    /// Device flow where the backend polls the provider's token endpoint.
    DevicePoll,
}

/// Mirror of `querymt::mcp::config::McpServerConfig` for typeshare generation.
/// Note: kept for typeshare output; may be unused in Rust code paths.
#[allow(dead_code)]
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    /// Transport protocol: "http" or "stdio"
    pub protocol: String,
    /// URL for HTTP transport, command for stdio transport
    pub endpoint: String,
}

impl From<&crate::config::McpServerConfig> for McpServerInfo {
    fn from(c: &crate::config::McpServerConfig) -> Self {
        let (protocol, endpoint) = match c {
            crate::config::McpServerConfig::Http { name: _, url, .. } => {
                ("http".into(), url.into())
            }
            crate::config::McpServerConfig::Stdio {
                name: _, command, ..
            } => ("stdio".into(), command.clone()),
        };
        Self {
            name: c.name().into(),
            protocol,
            endpoint,
        }
    }
}

/// Messages from server to UI client.
/// Typeshare-annotated: generated for TypeScript and Swift.
#[typeshare]
#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UiServerMessage {
    State {
        routing_mode: RoutingMode,
        active_agent_id: String,
        active_session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default_cwd: Option<String>,
        agents: Vec<UiAgentInfo>,
        sessions_by_agent: HashMap<String, String>,
        agent_mode: String,
    },
    SessionCreated {
        agent_id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    Event {
        agent_id: String,
        session_id: String,
        event: EventEnvelope,
    },
    SessionEvents {
        session_id: String,
        agent_id: String,
        events: Vec<EventEnvelope>,
        #[typeshare(serialized_as = "StreamCursor")]
        cursor: StreamCursor,
    },
    Error {
        message: String,
    },
    SessionList {
        groups: Vec<SessionGroup>,
    },
    SessionLoaded {
        session_id: String,
        agent_id: String,
        audit: AuditView,
        undo_stack: Vec<UndoStackFrame>,
        #[typeshare(serialized_as = "StreamCursor")]
        cursor: StreamCursor,
    },
    WorkspaceIndexStatus {
        session_id: String,
        status: String,
        message: Option<String>,
    },
    AllModelsList {
        models: Vec<ModelEntry>,
    },
    /// Recent models from event history, grouped by workspace
    RecentModels {
        by_workspace: HashMap<String, Vec<RecentModelEntry>>,
    },
    ProviderCapabilities {
        providers: Vec<ProviderCapabilityEntry>,
    },
    /// File index for autocomplete
    FileIndex {
        files: Vec<FileIndexEntry>,
        #[typeshare(serialized_as = "number")]
        generated_at: u64,
    },
    /// LLM config details response
    LlmConfig {
        #[typeshare(serialized_as = "number")]
        config_id: i64,
        provider: String,
        model: String,
        #[typeshare(serialized_as = "any")]
        params: Option<Value>,
    },
    /// Result of an undo operation
    UndoResult {
        success: bool,
        message: Option<String>,
        reverted_files: Vec<String>,
        message_id: Option<String>,
        undo_stack: Vec<UndoStackFrame>,
    },
    /// Result of a redo operation
    RedoResult {
        success: bool,
        message: Option<String>,
        undo_stack: Vec<UndoStackFrame>,
    },
    /// Result of a fork operation.
    ForkResult {
        success: bool,
        source_session_id: Option<String>,
        forked_session_id: Option<String>,
        message: Option<String>,
    },
    /// Current agent mode notification
    AgentMode {
        mode: String,
    },
    /// OAuth-capable providers and current authentication status
    AuthProviders {
        providers: Vec<AuthProviderEntry>,
    },
    /// OAuth flow started; frontend should open authorization_url
    #[serde(rename = "oauth_flow_started")]
    OAuthFlowStarted {
        flow_id: String,
        provider: String,
        authorization_url: String,
        #[typeshare(serialized_as = "OAuthFlowKindTs")]
        flow_kind: OAuthFlowKind,
    },
    /// OAuth flow completion result
    #[serde(rename = "oauth_result")]
    OAuthResult {
        provider: String,
        success: bool,
        message: String,
    },
    /// List of remote nodes discovered in the kameo mesh
    RemoteNodes {
        nodes: Vec<RemoteNodeInfo>,
    },
    /// Sessions available on a specific remote node
    RemoteSessions {
        /// Stable node id (PeerId string)
        node_id: String,
        /// Sessions on that node
        sessions: Vec<crate::agent::remote::RemoteSessionInfo>,
    },
    ModelDownloadStatus {
        provider: String,
        model_id: String,
        status: String,
        #[typeshare(serialized_as = "number")]
        bytes_downloaded: u64,
        #[typeshare(serialized_as = "Option<number>")]
        bytes_total: Option<u64>,
        percent: Option<f32>,
        #[typeshare(serialized_as = "Option<number>")]
        speed_bps: Option<u64>,
        #[typeshare(serialized_as = "Option<number>")]
        eta_seconds: Option<u64>,
        message: Option<String>,
    },
    /// Progress update for an OCI plugin update operation.
    PluginUpdateStatus {
        plugin_name: String,
        image_reference: String,
        phase: String,
        #[typeshare(serialized_as = "number")]
        bytes_downloaded: u64,
        #[typeshare(serialized_as = "Option<number>")]
        bytes_total: Option<u64>,
        percent: Option<f32>,
        message: Option<String>,
    },
    /// All OCI plugin updates have completed.
    PluginUpdateComplete {
        results: Vec<PluginUpdateResult>,
    },
    /// Result of setting/clearing an API token
    #[serde(rename = "api_token_result")]
    ApiTokenResult {
        provider: String,
        success: bool,
        message: String,
    },
}

impl UiServerMessage {
    /// Returns the message type name for logging purposes.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::State { .. } => "state",
            Self::SessionCreated { .. } => "session_created",
            Self::Event { .. } => "event",
            Self::Error { .. } => "error",
            Self::SessionList { .. } => "session_list",
            Self::SessionLoaded { .. } => "session_loaded",
            Self::WorkspaceIndexStatus { .. } => "workspace_index_status",
            Self::AllModelsList { .. } => "all_models_list",
            Self::RecentModels { .. } => "recent_models",
            Self::ProviderCapabilities { .. } => "provider_capabilities",
            Self::FileIndex { .. } => "file_index",
            Self::LlmConfig { .. } => "llm_config",
            Self::SessionEvents { .. } => "session_events",
            Self::UndoResult { .. } => "undo_result",
            Self::RedoResult { .. } => "redo_result",
            Self::ForkResult { .. } => "fork_result",
            Self::AgentMode { .. } => "agent_mode",
            Self::AuthProviders { .. } => "auth_providers",
            Self::OAuthFlowStarted { .. } => "oauth_flow_started",
            Self::OAuthResult { .. } => "oauth_result",
            Self::RemoteNodes { .. } => "remote_nodes",
            Self::RemoteSessions { .. } => "remote_sessions",
            Self::ModelDownloadStatus { .. } => "model_download_status",
            Self::PluginUpdateStatus { .. } => "plugin_update_status",
            Self::PluginUpdateComplete { .. } => "plugin_update_complete",
            Self::ApiTokenResult { .. } => "api_token_result",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OAuthFlowKind, PluginUpdateResult, UiClientMessage, UiServerMessage};
    use serde_json::json;

    // Note: All UiClientMessage and UiServerMessage tests use adjacently tagged serde format:
    //   client sends: {"type": "variant_name", "data": { ...fields... }}
    //   server sends: {"type": "variant_name", "data": { ...fields... }}
    // Unit variants (no fields) serialize as: {"type": "variant_name"}

    #[test]
    fn deserializes_start_oauth_login_tag() {
        let current: UiClientMessage = serde_json::from_value(json!({
            "type": "start_oauth_login",
            "data": { "provider": "openai" }
        }))
        .expect("start_oauth_login should deserialize with data wrapper");

        match current {
            UiClientMessage::StartOAuthLogin { provider } => assert_eq!(provider, "openai"),
            _ => panic!("expected StartOAuthLogin variant"),
        }
    }

    #[test]
    fn deserializes_complete_oauth_login_tag() {
        let current: UiClientMessage = serde_json::from_value(json!({
            "type": "complete_oauth_login",
            "data": { "flow_id": "flow-1", "response": "code" }
        }))
        .expect("complete_oauth_login should deserialize with data wrapper");

        match current {
            UiClientMessage::CompleteOAuthLogin { flow_id, response } => {
                assert_eq!(flow_id, "flow-1");
                assert_eq!(response, "code");
            }
            _ => panic!("expected CompleteOAuthLogin variant"),
        }
    }

    #[test]
    fn deserializes_disconnect_oauth_tag() {
        let current: UiClientMessage = serde_json::from_value(json!({
            "type": "disconnect_oauth",
            "data": { "provider": "openai" }
        }))
        .expect("disconnect_oauth should deserialize with data wrapper");

        match current {
            UiClientMessage::DisconnectOAuth { provider } => assert_eq!(provider, "openai"),
            _ => panic!("expected DisconnectOAuth variant"),
        }
    }

    #[test]
    fn deserializes_delete_session_tag() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "delete_session",
            "data": { "session_id": "sess-123" }
        }))
        .expect("delete_session should deserialize with data wrapper");

        match msg {
            UiClientMessage::DeleteSession { session_id } => {
                assert_eq!(session_id, "sess-123");
            }
            _ => panic!("expected DeleteSession"),
        }
    }

    #[test]
    fn deserializes_fork_session_tag() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "fork_session",
            "data": { "message_id": "msg-123" }
        }))
        .expect("fork_session should deserialize with data wrapper");

        match msg {
            UiClientMessage::ForkSession { message_id } => {
                assert_eq!(message_id, "msg-123");
            }
            _ => panic!("expected ForkSession"),
        }
    }

    // ── Remote message node/node_id alias tests ──────────────────────────

    #[test]
    fn create_remote_session_accepts_node_id_field() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "create_remote_session",
            "data": { "node_id": "peer-abc", "cwd": "/tmp" }
        }))
        .expect("node_id field should deserialize with data wrapper");

        match msg {
            UiClientMessage::CreateRemoteSession { node_id, cwd, .. } => {
                assert_eq!(node_id, "peer-abc");
                assert_eq!(cwd.as_deref(), Some("/tmp"));
            }
            _ => panic!("expected CreateRemoteSession"),
        }
    }

    #[test]
    fn create_remote_session_accepts_node_alias() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "create_remote_session",
            "data": { "node": "peer-abc", "cwd": "/tmp" }
        }))
        .expect("node alias should deserialize to node_id with data wrapper");

        match msg {
            UiClientMessage::CreateRemoteSession { node_id, cwd, .. } => {
                assert_eq!(node_id, "peer-abc");
                assert_eq!(cwd.as_deref(), Some("/tmp"));
            }
            _ => panic!("expected CreateRemoteSession"),
        }
    }

    #[test]
    fn list_remote_sessions_accepts_node_alias() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "list_remote_sessions",
            "data": { "node": "peer-xyz" }
        }))
        .expect("node alias should deserialize to node_id with data wrapper");

        match msg {
            UiClientMessage::ListRemoteSessions { node_id } => {
                assert_eq!(node_id, "peer-xyz");
            }
            _ => panic!("expected ListRemoteSessions"),
        }
    }

    #[test]
    fn attach_remote_session_accepts_node_alias() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "attach_remote_session",
            "data": { "node": "peer-xyz", "session_id": "sess-1" }
        }))
        .expect("node alias should deserialize to node_id with data wrapper");

        match msg {
            UiClientMessage::AttachRemoteSession {
                node_id,
                session_id,
            } => {
                assert_eq!(node_id, "peer-xyz");
                assert_eq!(session_id, "sess-1");
            }
            _ => panic!("expected AttachRemoteSession"),
        }
    }

    #[test]
    fn set_session_model_accepts_node_alias() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "set_session_model",
            "data": {
                "session_id": "sess-1",
                "model_id": "claude-3-opus",
                "node": "peer-abc"
            }
        }))
        .expect("node alias should deserialize to node_id with data wrapper");

        match msg {
            UiClientMessage::SetSessionModel {
                session_id,
                model_id,
                node_id,
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(model_id, "claude-3-opus");
                assert_eq!(node_id.as_deref(), Some("peer-abc"));
            }
            _ => panic!("expected SetSessionModel"),
        }
    }

    #[test]
    fn remote_sessions_server_msg_serializes_node_id() {
        let msg = UiServerMessage::RemoteSessions {
            node_id: "peer-abc".to_string(),
            sessions: Vec::new(),
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "remote_sessions");
        // adjacently tagged: payload is under "data"
        assert_eq!(json["data"]["node_id"], "peer-abc");
    }

    // ── Plugin update message tests (RED→GREEN) ───────────────────────────────

    #[test]
    fn update_plugins_client_message_deserializes() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "update_plugins"
        }))
        .expect("update_plugins (unit variant) should deserialize without data wrapper");
        assert!(matches!(msg, UiClientMessage::UpdatePlugins));
    }

    #[test]
    fn plugin_update_status_server_message_serializes() {
        let msg = UiServerMessage::PluginUpdateStatus {
            plugin_name: "my-plugin".to_string(),
            image_reference: "ghcr.io/org/plugin:latest".to_string(),
            phase: "downloading".to_string(),
            bytes_downloaded: 1024,
            bytes_total: Some(4096),
            percent: Some(25.0),
            message: None,
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "plugin_update_status");
        // adjacently tagged: payload is under "data"
        assert_eq!(json["data"]["plugin_name"], "my-plugin");
        assert_eq!(json["data"]["phase"], "downloading");
        assert_eq!(json["data"]["bytes_downloaded"], 1024);
        assert_eq!(json["data"]["bytes_total"], 4096);
        assert!((json["data"]["percent"].as_f64().unwrap() - 25.0).abs() < 0.01);
        assert!(json["data"]["message"].is_null());
    }

    #[test]
    fn plugin_update_complete_server_message_serializes() {
        let msg = UiServerMessage::PluginUpdateComplete {
            results: vec![
                PluginUpdateResult {
                    plugin_name: "ok-plugin".to_string(),
                    success: true,
                    message: None,
                },
                PluginUpdateResult {
                    plugin_name: "bad-plugin".to_string(),
                    success: false,
                    message: Some("network error".to_string()),
                },
            ],
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "plugin_update_complete");
        // adjacently tagged: payload is under "data"
        let results = json["data"]["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["plugin_name"], "ok-plugin");
        assert_eq!(results[0]["success"], true);
        assert_eq!(results[1]["success"], false);
        assert_eq!(results[1]["message"], "network error");
    }

    #[test]
    fn plugin_update_status_with_failed_message_serializes() {
        let msg = UiServerMessage::PluginUpdateStatus {
            plugin_name: "err-plugin".to_string(),
            image_reference: "ghcr.io/org/plugin:v1".to_string(),
            phase: "failed".to_string(),
            bytes_downloaded: 512,
            bytes_total: None,
            percent: None,
            message: Some("connection refused".to_string()),
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "plugin_update_status");
        // adjacently tagged: payload is under "data"
        assert_eq!(json["data"]["message"], "connection refused");
        assert!(json["data"]["bytes_total"].is_null());
        assert!(json["data"]["percent"].is_null());
    }

    // ── API token & auth method message tests (RED→GREEN) ───────────────────

    #[test]
    fn deserializes_set_api_token_tag() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "set_api_token",
            "data": { "provider": "openai", "api_key": "sk-test123" }
        }))
        .expect("set_api_token should deserialize with data wrapper");

        match msg {
            UiClientMessage::SetApiToken { provider, api_key } => {
                assert_eq!(provider, "openai");
                assert_eq!(api_key, "sk-test123");
            }
            _ => panic!("expected SetApiToken variant"),
        }
    }

    #[test]
    fn deserializes_clear_api_token_tag() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "clear_api_token",
            "data": { "provider": "openai" }
        }))
        .expect("clear_api_token should deserialize with data wrapper");

        match msg {
            UiClientMessage::ClearApiToken { provider } => {
                assert_eq!(provider, "openai");
            }
            _ => panic!("expected ClearApiToken variant"),
        }
    }

    #[test]
    fn deserializes_set_auth_method_tag() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "set_auth_method",
            "data": { "provider": "anthropic", "method": "oauth" }
        }))
        .expect("set_auth_method should deserialize with data wrapper");

        match msg {
            UiClientMessage::SetAuthMethod { provider, method } => {
                assert_eq!(provider, "anthropic");
                assert!(matches!(method, super::AuthMethod::OAuth));
            }
            _ => panic!("expected SetAuthMethod variant"),
        }
    }

    #[test]
    fn deserializes_set_auth_method_api_key_variant() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "set_auth_method",
            "data": { "provider": "openai", "method": "api_key" }
        }))
        .expect("set_auth_method api_key should deserialize");

        match msg {
            UiClientMessage::SetAuthMethod { method, .. } => {
                assert!(matches!(method, super::AuthMethod::ApiKey));
            }
            _ => panic!("expected SetAuthMethod variant"),
        }
    }

    #[test]
    fn deserializes_set_auth_method_env_var_variant() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "set_auth_method",
            "data": { "provider": "google", "method": "env_var" }
        }))
        .expect("set_auth_method env_var should deserialize");

        match msg {
            UiClientMessage::SetAuthMethod { method, .. } => {
                assert!(matches!(method, super::AuthMethod::EnvVar));
            }
            _ => panic!("expected SetAuthMethod variant"),
        }
    }

    #[test]
    fn auth_providers_server_msg_serializes_extended_fields() {
        let msg = UiServerMessage::AuthProviders {
            providers: vec![super::AuthProviderEntry {
                provider: "anthropic".to_string(),
                display_name: "Anthropic".to_string(),
                oauth_status: Some(super::OAuthStatus::Connected),
                has_stored_api_key: true,
                has_env_api_key: false,
                env_var_name: Some("ANTHROPIC_API_KEY".to_string()),
                supports_oauth: true,
                preferred_method: Some(super::AuthMethod::OAuth),
            }],
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "auth_providers");
        let p = &json["data"]["providers"][0];
        assert_eq!(p["provider"], "anthropic");
        assert_eq!(p["oauth_status"], "connected");
        assert_eq!(p["has_stored_api_key"], true);
        assert_eq!(p["has_env_api_key"], false);
        assert_eq!(p["env_var_name"], "ANTHROPIC_API_KEY");
        assert_eq!(p["supports_oauth"], true);
        assert_eq!(p["preferred_method"], "oauth");
    }

    #[test]
    fn auth_providers_server_msg_serializes_no_oauth_provider() {
        let msg = UiServerMessage::AuthProviders {
            providers: vec![super::AuthProviderEntry {
                provider: "groq".to_string(),
                display_name: "Groq".to_string(),
                oauth_status: None,
                has_stored_api_key: false,
                has_env_api_key: true,
                env_var_name: Some("GROQ_API_KEY".to_string()),
                supports_oauth: false,
                preferred_method: None,
            }],
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        let p = &json["data"]["providers"][0];
        assert_eq!(p["supports_oauth"], false);
        assert!(p["oauth_status"].is_null());
        assert!(p["preferred_method"].is_null());
    }

    #[test]
    fn api_token_result_server_msg_serializes() {
        let msg = UiServerMessage::ApiTokenResult {
            provider: "openai".to_string(),
            success: true,
            message: "API key stored successfully".to_string(),
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "api_token_result");
        assert_eq!(json["data"]["provider"], "openai");
        assert_eq!(json["data"]["success"], true);
        assert_eq!(json["data"]["message"], "API key stored successfully");
    }

    #[test]
    fn serializes_oauth_server_tags_without_extra_underscore() {
        let flow_started = serde_json::to_value(UiServerMessage::OAuthFlowStarted {
            flow_id: "flow-1".to_string(),
            provider: "openai".to_string(),
            authorization_url: "https://example.com".to_string(),
            flow_kind: OAuthFlowKind::RedirectCode,
        })
        .expect("OAuthFlowStarted should serialize");
        assert_eq!(flow_started["type"], "oauth_flow_started");
        // adjacently tagged: payload is under "data"
        assert_eq!(flow_started["data"]["flow_kind"], "redirect_code");

        let flow_started_device = serde_json::to_value(UiServerMessage::OAuthFlowStarted {
            flow_id: "flow-2".to_string(),
            provider: "kimi-code".to_string(),
            authorization_url: "https://example.com/device".to_string(),
            flow_kind: OAuthFlowKind::DevicePoll,
        })
        .expect("OAuthFlowStarted (DevicePoll) should serialize");
        assert_eq!(flow_started_device["data"]["flow_kind"], "device_poll");

        let result = serde_json::to_value(UiServerMessage::OAuthResult {
            provider: "openai".to_string(),
            success: true,
            message: "ok".to_string(),
        })
        .expect("OAuthResult should serialize");
        assert_eq!(result["type"], "oauth_result");
    }

    #[test]
    fn auth_method_display_and_from_str_round_trip() {
        use super::AuthMethod;
        for (method, expected_str) in [
            (AuthMethod::OAuth, "oauth"),
            (AuthMethod::ApiKey, "api_key"),
            (AuthMethod::EnvVar, "env_var"),
        ] {
            let s = method.to_string();
            assert_eq!(s, expected_str);
            let parsed: AuthMethod = s.parse().unwrap();
            assert_eq!(parsed, method);
        }
    }

    #[test]
    fn auth_method_from_str_rejects_unknown() {
        use super::AuthMethod;
        let result = "unknown".parse::<AuthMethod>();
        assert!(result.is_err());
    }

    #[test]
    fn auth_providers_oauth_only_provider_has_no_env_var() {
        // OAuth-only providers (like Codex) should have env_var_name = None
        // and has_stored_api_key / has_env_api_key = false.
        let msg = UiServerMessage::AuthProviders {
            providers: vec![super::AuthProviderEntry {
                provider: "codex".to_string(),
                display_name: "Codex".to_string(),
                oauth_status: Some(super::OAuthStatus::Connected),
                has_stored_api_key: false,
                has_env_api_key: false,
                env_var_name: None,
                supports_oauth: true,
                preferred_method: None,
            }],
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        let p = &json["data"]["providers"][0];
        assert_eq!(p["supports_oauth"], true);
        assert!(p["env_var_name"].is_null());
        assert_eq!(p["has_stored_api_key"], false);
        assert_eq!(p["has_env_api_key"], false);
    }
}
