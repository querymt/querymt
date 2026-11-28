//! Agents store: configured agents, their live ACP connections, and the data
//! the Start and Agents pages read (capabilities, models, sessions, mesh).
//!
//! Mirrors `querymt-desktop/src/lib/stores/agents.svelte.ts` at the scope of
//! those two pages. Views observe the entity and re-render on `cx.notify()`.

pub(crate) mod agent_log;
pub(crate) mod automations;
pub(crate) mod delegates;
mod devtools;
mod early_commands;
pub(crate) mod fork;
pub(crate) mod launch;
pub(crate) mod mcp_notice;
mod mesh_live;
mod pins;
mod plans;
pub(crate) mod plugins;
pub(crate) mod setup;
pub(crate) mod sidebar;
#[cfg(not(target_family = "wasm"))]
pub(crate) mod updates;
pub(crate) mod worktree;

#[cfg(test)]
mod open_tests;

use std::{collections::HashMap, rc::Rc, time::Duration};

use gpui_kit::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Image, ImageFormat,
    Subscription, Task, WeakEntity,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::Instrument as _;

use crate::rpc::{Incoming, RpcClient};
use crate::session::open::{
    Delivery, Link, OpenSession, OpenSessionEvent, SessionCaps, SessionKey, SessionRef,
};
use crate::session::{ListedPlan, SessionView};
use crate::transport::{self, Inbound, Target};
use crate::util::now_ms;

// ---------------------------------------------------------------------------
// Config

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Endpoint {
    /// Local process speaking ACP over stdio (native only).
    Process { command: String },
    /// `host:port` of an ACP WebSocket server (`/acp/ws`).
    WebSocket { address: String, secure: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub endpoint: Endpoint,
    pub auto_start: bool,
    /// The server that serves this web UI. It cannot be edited or deleted.
    #[serde(default)]
    pub host_managed: bool,
}

impl AgentConfig {
    pub fn is_websocket(&self) -> bool {
        matches!(self.endpoint, Endpoint::WebSocket { .. })
    }

    pub fn endpoint_label(&self) -> String {
        match &self.endpoint {
            Endpoint::Process { command } => command.clone(),
            Endpoint::WebSocket {
                address,
                secure: true,
            } => format!("wss://{address}"),
            Endpoint::WebSocket { address, .. } => address.clone(),
        }
    }

    pub fn transport_label(&self) -> &'static str {
        if self.is_websocket() {
            "WebSocket endpoint"
        } else {
            "Local process"
        }
    }

    fn target(&self) -> Target {
        match &self.endpoint {
            Endpoint::Process { command } => Target::Command(command.clone()),
            Endpoint::WebSocket { address, secure } => Target::WebSocket(format!(
                "{}://{address}/acp/ws",
                if *secure { "wss" } else { "ws" }
            )),
        }
    }
}

/// Port of `normalizeAcpWebSocketEndpoint`: accepts `host:port`, `ws://host:port`,
/// `wss://host:port/acp/ws`, … and returns `(host:port, secure)`.
pub fn normalize_address(input: &str) -> Result<(String, bool), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a server address as host:port.".into());
    }
    let (rest, secure) = if let Some(rest) = trimmed.strip_prefix("wss://") {
        (rest, true)
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        (rest, false)
    } else if trimmed.contains("://") {
        return Err("Enter a server address as host:port.".into());
    } else {
        (trimmed, false)
    };
    if rest.contains('@') {
        return Err("Server addresses must not contain credentials.".into());
    }
    if rest.contains('?') || rest.contains('#') {
        return Err("Server addresses must not contain query parameters or fragments.".into());
    }
    let (host, path) = match rest.find('/') {
        Some(ix) => (&rest[..ix], &rest[ix..]),
        None => (rest, ""),
    };
    if !matches!(path, "" | "/" | "/ws" | "/acp/ws") {
        return Err("Enter only the server host and port.".into());
    }
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err("Enter a server address as host:port.".into());
    }
    Ok((host.to_string(), secure))
}

fn slug(name: &str) -> String {
    let base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{}-{}", base.trim_matches('-'), now_ms() as u64)
}

// ---------------------------------------------------------------------------
// Runtime data

#[derive(Clone, Debug, PartialEq, Default)]
pub enum ConnState {
    #[default]
    Idle,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub provider: String,
    /// Bare model name within the provider (`claude-opus-4`).
    pub model: String,
    /// Mesh node serving the model (`node_id`); `None`: the agent's own.
    pub node_id: Option<String>,
    /// Display name of that node (`node_label`, else its id).
    pub node: Option<String>,
    /// Context window in tokens (`_querymt/modelInfo`), when known.
    pub context: Option<u64>,
    /// Known input modalities (`text`, `image`, `pdf`, …); empty when unknown.
    pub modalities: Vec<String>,
    pub(crate) raw: Value,
}

/// Selection key of a model (desktop `getModelSelectionKey`): its id, or for
/// a model served by a mesh node, id and node, so the same model on several
/// machines stays distinct.
pub fn model_key(id: &str, node_id: Option<&str>) -> String {
    match node_id {
        Some(node) => format!("{REMOTE_MODEL_KEY}{}", json!([id, node])),
        None => id.to_string(),
    }
}

/// Delay before queued session inputs are applied as one batch (about half a
/// frame at 60 Hz).
pub(crate) const SESSION_FLUSH_MS: u64 = 8;
/// A load response larger than this is decoded off the UI thread (native).
pub(crate) const HYDRATE_BYTES: usize = 256 * 1024;

const REMOTE_MODEL_KEY: &str = "querymt:model:";

/// The model of `models` a selection key names (desktop
/// `findModelBySelectionKey`). A bare id saved before keys carried the node
/// prefers the agent's own model.
pub fn find_model<'a>(models: &'a [Model], key: &str) -> Option<&'a Model> {
    models.iter().find(|m| m.key() == key).or_else(|| {
        let mut same = models.iter().filter(|m| m.id == key);
        let first = same.next()?;
        Some(
            std::iter::once(first)
                .chain(same)
                .find(|m| m.node_id.is_none())
                .unwrap_or(first),
        )
    })
}

impl Model {
    pub fn key(&self) -> String {
        model_key(&self.id, self.node_id.as_deref())
    }

    /// `label from provider on node` (desktop `modelSelectionTooltip`).
    pub fn describe(&self) -> String {
        match &self.node {
            Some(node) => format!("{} from {} on {node}", self.label, self.provider),
            None => format!("{} from {}", self.label, self.provider),
        }
    }

    /// `1M` / `200K` style context size, or `UNK`.
    pub fn context_short(&self) -> String {
        match self.context {
            Some(t) if t >= 1_000_000 => format!("{}M", (t as f64 / 1_000_000.).round()),
            Some(t) if t >= 1_000 => format!("{}K", (t as f64 / 1_000.).round()),
            Some(t) => t.to_string(),
            None => "UNK".into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionSummary {
    pub agent_id: String,
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at: String,
    /// `runtimeStatus.phase`; `unknown` when the agent could not observe it.
    pub phase: String,
    pub root: bool,
    /// Neither resolved nor archived (also when the agent reports no lifecycle).
    pub open: bool,
    pub resolved: bool,
    pub archived: bool,
    /// The agent reported resolved/archived state; without it the session
    /// belongs to no lifecycle view but All.
    pub lifecycle_known: bool,
    pub pinned: bool,
    pub messages: u64,
    /// The last turn finished with an error.
    pub has_errors: bool,
    pub parent: Option<String>,
    /// `user`, `delegation`, `worktree`, …
    pub fork_origin: Option<String>,
    pub fork_count: u64,
    pub has_children: bool,
    pub worktree: Option<Worktree>,
    pub remote: Option<RemoteOwner>,
    /// `items` plans from `_meta["querymt/plans"]`, kept current by
    /// `querymt/session/plansChanged`. Empty for agents that send none.
    pub plans: Vec<ListedPlan>,
}

/// Managed Git worktree a session runs in (`_meta.querymt.workspace`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Worktree {
    pub branch: Option<String>,
    pub checkout_path: Option<String>,
    /// The project the worktree was created from; sessions group under it.
    pub source_cwd: Option<String>,
    pub baseline_mode: Option<String>,
    /// `ready` when usable; otherwise e.g. `failed`, `archived`.
    pub lifecycle: Option<String>,
    pub handed_off_to: Option<String>,
    /// Session that owns the checkout now (the successor after a handoff).
    pub owner_session_id: Option<String>,
}

impl Worktree {
    pub fn usable(&self) -> bool {
        self.lifecycle.as_deref().is_none_or(|l| l == "ready")
    }
}

/// A session owned by another mesh node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteOwner {
    pub node_id: String,
    pub label: Option<String>,
    pub connected: bool,
}

impl RemoteOwner {
    pub fn name(&self) -> String {
        self.label
            .clone()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| self.node_id.chars().take(8).collect())
    }
}

/// Runtime phases in which the session is doing work.
const ACTIVE_PHASES: [&str; 7] = [
    "starting",
    "model",
    "tools",
    "waiting",
    "closing",
    "cancel_requested",
    // Optimistic placeholder for a session this app just launched.
    "running",
];

impl SessionSummary {
    pub fn running(&self) -> bool {
        ACTIVE_PHASES.contains(&self.phase.as_str())
    }

    /// The agent could not report whether the session is running.
    pub fn status_unknown(&self) -> bool {
        matches!(self.phase.as_str(), "" | "unknown")
    }

    /// Workspace the session is listed under: a worktree session belongs to
    /// the project it was created from, not its checkout.
    pub fn group_cwd(&self) -> &str {
        self.worktree
            .as_ref()
            .and_then(|w| w.source_cwd.as_deref())
            .filter(|c| !c.is_empty())
            .unwrap_or(&self.cwd)
    }

    /// Parse one `session/list` entry.
    pub fn from_list_entry(agent_id: &str, s: &Value) -> Self {
        let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
        let opt = |v: Option<&Value>| {
            v.and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let meta = s.get("_meta");
        let m = |k: &str| meta.and_then(|m| m.get(k));
        let p = |k: &str| meta.and_then(|m| m.pointer(k));
        let lifecycle = p("/querymt/lifecycle").filter(|l| {
            l.get("resolved").is_some_and(Value::is_boolean)
                && l.get("archived").is_some_and(Value::is_boolean)
        });
        let flag = |k: &str| {
            lifecycle
                .and_then(|l| l.get(k))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let (resolved, archived) = (flag("resolved"), flag("archived"));
        let phase = if p("/runtimeStatus/observation/availability").and_then(Value::as_str)
            == Some("unavailable")
        {
            "unknown".to_string()
        } else {
            text(p("/runtimeStatus/phase"))
        };
        let parent = opt(m("parentSessionId"));
        let worktree = p("/querymt/workspace")
            .filter(|w| w.get("workspaceId").is_some_and(|v| v.as_str().is_some()))
            .map(|w| Worktree {
                branch: opt(w.get("branch")),
                checkout_path: opt(w.get("checkoutPath")),
                source_cwd: opt(w.get("sourceCwd")),
                baseline_mode: opt(w.get("baselineMode")),
                lifecycle: opt(w.get("lifecycle")),
                handed_off_to: opt(w.get("handedOffTo")),
                owner_session_id: opt(w.get("ownerSessionId")),
            });
        let remote = (text(m("location")) == "remote")
            .then(|| opt(m("nodeId")))
            .flatten()
            .map(|node_id| RemoteOwner {
                node_id,
                label: opt(m("nodeLabel")),
                connected: text(m("connectionState")) != "disconnected",
            });
        Self {
            agent_id: agent_id.to_string(),
            id: text(s.get("sessionId")),
            cwd: text(s.get("cwd")),
            title: text(s.get("title")),
            updated_at: text(s.get("updatedAt")),
            phase,
            root: parent.is_none(),
            open: !resolved && !archived,
            resolved,
            archived,
            lifecycle_known: lifecycle.is_some(),
            pinned: p("/querymt/pin/pinned").and_then(Value::as_bool) == Some(true),
            messages: m("messageCount").and_then(Value::as_u64).unwrap_or(0),
            has_errors: m("hasErrors").and_then(Value::as_bool) == Some(true),
            parent,
            fork_origin: opt(m("forkOrigin")),
            fork_count: m("forkCount").and_then(Value::as_u64).unwrap_or(0),
            has_children: m("hasChildren").and_then(Value::as_bool) == Some(true),
            worktree,
            remote,
            plans: ListedPlan::parse_all(m("querymt/plans")),
        }
    }

    pub fn display_title(&self) -> String {
        if self.title.trim().is_empty() {
            "Untitled session".into()
        } else {
            self.title.clone()
        }
    }

    /// `01a1b838-d3f2` style short id (first two UUID groups).
    pub fn short_id(&self) -> String {
        self.id.splitn(3, '-').take(2).collect::<Vec<_>>().join("-")
    }
}

/// A file attached to a new session's first prompt.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// Base64 payload.
    pub data: String,
    /// Decoded image for thumbnails and the preview (images only).
    pub image: Option<std::sync::Arc<Image>>,
}

impl Attachment {
    pub fn from_bytes(id: String, name: String, mime: String, bytes: &[u8]) -> Self {
        use base64::Engine as _;
        let image = mime
            .starts_with("image/")
            .then(|| ImageFormat::from_mime_type(&mime))
            .flatten()
            .map(|format| std::sync::Arc::new(Image::from_bytes(format, bytes.to_vec())));
        Self {
            id,
            name,
            mime,
            size: bytes.len() as u64,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            image,
        }
    }

    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Launch modes offered on the Start page (same set as the Svelte app).
pub const LAUNCH_MODES: [(&str, &str, &str); 3] = [
    ("build", "Build", "Full read and write access."),
    ("plan", "Plan", "Inspect and plan without making changes."),
    ("review", "Review", "Review code without making changes."),
];

/// Reasoning effort choices for new sessions.
pub const LAUNCH_REASONING: [(&str, &str, &str); 5] = [
    ("auto", "Auto", "Use model-specific defaults."),
    ("low", "Low", "Minimal thinking and faster responses."),
    ("medium", "Medium", "Balanced thinking."),
    ("high", "High", "Thorough thinking."),
    ("max", "Max", "Deepest thinking and highest budget."),
];

#[derive(Clone, Debug, Default)]
pub struct MeshStatus {
    pub enabled: bool,
    pub peer_id: Option<String>,
    pub hostname: Option<String>,
    pub peers: u64,
    pub transport: Option<String>,
    /// Relay URL and whether it is connected.
    pub relays: Vec<(String, bool)>,
}

#[derive(Clone, Debug)]
pub struct MeshNode {
    pub id: String,
    pub label: String,
    pub active_sessions: u64,
    pub capabilities: Vec<String>,
    pub transport: String,
}

#[derive(Clone, Debug)]
pub struct MeshInvite {
    pub id: String,
    pub mesh_name: Option<String>,
    pub expires_at: f64,
    pub max_uses: u64,
    pub uses_remaining: u64,
    pub status: String,
    pub created_at: f64,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderAuth {
    pub provider: String,
    pub display_name: String,
    pub oauth_status: Option<String>,
    pub has_stored_api_key: bool,
    pub has_env_api_key: bool,
    pub env_var_name: Option<String>,
    pub supports_oauth: bool,
    /// `oauth`, `api_key`, `env_var`; `None` is Auto.
    pub preferred_method: Option<String>,
}

/// Auth ids of MCP server logins: `mcp:<server>` (desktop `isMcpAuthId`).
const MCP_AUTH_PREFIX: &str = "mcp:";

impl ProviderAuth {
    /// An MCP server login, not a model provider.
    pub fn is_mcp(&self) -> bool {
        self.provider.starts_with(MCP_AUTH_PREFIX)
    }

    /// Server name of an MCP login (desktop `mcpServerOfAuthId`).
    pub fn mcp_server(&self) -> &str {
        self.provider
            .strip_prefix(MCP_AUTH_PREFIX)
            .unwrap_or(&self.provider)
    }
}

/// `_querymt/auth/status` entries split into model providers and MCP server
/// logins (desktop `splitAuthEntries`).
fn parse_auth_status(v: &Value) -> (Vec<ProviderAuth>, Vec<ProviderAuth>) {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let flag = |p: &Value, k: &str| p.get(k).and_then(Value::as_bool).unwrap_or(false);
    v.get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let provider = text(p, "provider")?;
            Some(ProviderAuth {
                display_name: text(p, "display_name").unwrap_or_else(|| provider.clone()),
                provider,
                oauth_status: text(p, "oauth_status"),
                has_stored_api_key: flag(p, "has_stored_api_key"),
                has_env_api_key: flag(p, "has_env_api_key"),
                env_var_name: text(p, "env_var_name"),
                supports_oauth: flag(p, "supports_oauth"),
                preferred_method: text(p, "preferred_method"),
            })
        })
        .partition(|p| !p.is_mcp())
}

/// Profile id that lets the agent use its active profile.
pub const DEFAULT_PROFILE: &str = "default";

/// `session/new` params; a non-default profile goes in `_meta.querymt.profile_id`.
/// Profiles of a `_querymt/profiles` response.
fn parse_profiles(v: &Value) -> Vec<Profile> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    v.get("profiles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let id = text(p, "id")?;
            Some(Profile {
                name: text(p, "name").unwrap_or_else(|| id.clone()),
                id,
                description: text(p, "description").unwrap_or_default(),
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct PermOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// JSON Schema type of an elicitation form field (the kinds ACP forms allow).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    String,
    Number,
    Integer,
    Boolean,
    Array,
}

/// One choice of a single- or multi-select field.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FormField {
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub kind: FieldKind,
    pub required: bool,
    /// Choices (`oneOf` / `enum`, or `items.anyOf` for arrays); empty for free input.
    pub options: Vec<FieldOption>,
    /// The agent accepts a typed answer instead of a listed option.
    pub allow_custom: bool,
    pub default: Option<Value>,
}

impl FormField {
    /// The schema default, else an empty value of the field's type.
    pub fn initial_value(&self) -> Value {
        if let Some(v) = &self.default {
            return v.clone();
        }
        match self.kind {
            FieldKind::Boolean => Value::Bool(false),
            FieldKind::Array => Value::Array(Vec::new()),
            _ => Value::from(""),
        }
    }
}

/// What the user entered for one field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldDraft {
    pub value: Value,
    /// `Some` while "Custom answer…" is chosen.
    pub custom: Option<String>,
}

#[derive(Clone, Debug)]
pub enum RequestKind {
    Permission {
        /// Command / path preview of the tool call (may be empty).
        summary: String,
        options: Vec<PermOption>,
    },
    /// `fields` is empty for URL-mode requests and for forms that use field
    /// types this app cannot display (`unsupported`).
    Elicitation {
        fields: Vec<FormField>,
        url: Option<String>,
        unsupported: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum RequestStatus {
    Pending,
    /// Answered or retired; the text is what the card shows (`Submitted`, `Declined`, …).
    Resolved(String),
}

/// A button of a request card.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestAction {
    pub id: String,
    pub label: String,
    pub primary: bool,
}

pub const RESOLVED_SUBMITTED: &str = "Submitted";
const RESOLVED_GONE: &str = "No longer pending";
/// Answered cards kept in the Inbox (the newest ones).
const MAX_RESOLVED_REQUESTS: usize = 50;

/// A permission or input request from an agent. It stays listed after it is
/// answered (resolved), like the desktop inbox.
#[derive(Clone, Debug)]
pub struct PendingRequest {
    /// Card identity: `agent:<querymt id>` when the agent gives one, so a
    /// re-delivered request (reconnect, rejected answer) lands on the same
    /// card and keeps its draft.
    pub key: String,
    pub agent_id: String,
    /// JSON-RPC id of the delivery that can still be answered.
    rpc_id: Option<Value>,
    pub session_id: Option<String>,
    /// QueryMT elicitation / interaction id: compared with the agent's pending
    /// set during recovery.
    identity: Option<String>,
    /// `key` survives a reconnect (identity or tool call based).
    stable: bool,
    pub received_ms: f64,
    pub title: String,
    pub detail: String,
    pub kind: RequestKind,
    pub status: RequestStatus,
    /// The connection dropped; the card waits for the agent to re-deliver it.
    pub offline: bool,
    pub error: Option<String>,
    /// Answered, and a re-delivery means the agent rejected the answer.
    awaiting_ack: bool,
    /// The agent rejected the answer (`validationFailed`); keep the error on re-delivery.
    rejected: bool,
    /// One entry per form field: the user's answer so far. Kept across
    /// re-deliveries, so a reconnect or a rejected answer loses nothing.
    pub drafts: Vec<FieldDraft>,
}

impl PendingRequest {
    pub fn is_pending(&self) -> bool {
        self.status == RequestStatus::Pending
    }

    /// Pending, connected and holding a delivery to answer.
    pub fn answerable(&self) -> bool {
        self.is_pending() && !self.offline && self.rpc_id.is_some()
    }

    pub fn is_elicitation(&self) -> bool {
        matches!(self.kind, RequestKind::Elicitation { .. })
    }

    pub fn submitted(&self) -> bool {
        self.status == RequestStatus::Resolved(RESOLVED_SUBMITTED.into())
    }

    pub fn fields(&self) -> &[FormField] {
        match &self.kind {
            RequestKind::Elicitation { fields, .. } => fields,
            RequestKind::Permission { .. } => &[],
        }
    }

    /// Buttons, as the desktop app: permission options, or Submit / Decline / Cancel.
    pub fn actions(&self) -> Vec<RequestAction> {
        let action = |id: &str, label: &str, primary: bool| RequestAction {
            id: id.into(),
            label: label.into(),
            primary,
        };
        match &self.kind {
            RequestKind::Permission { options, .. } => {
                let primary = options
                    .iter()
                    .position(|o| o.kind == "allow_once")
                    .or_else(|| options.iter().position(|o| o.kind.starts_with("allow")));
                options
                    .iter()
                    .enumerate()
                    .map(|(ix, o)| action(&o.id, &o.name, Some(ix) == primary))
                    .collect()
            }
            RequestKind::Elicitation {
                unsupported: true, ..
            } => vec![
                action("decline", "Decline", false),
                action("cancel", "Cancel", false),
            ],
            RequestKind::Elicitation { url: Some(_), .. } => vec![
                action("accept", "Open flow", true),
                action("cancel", "Cancel", false),
            ],
            RequestKind::Elicitation { .. } => vec![
                action("accept", "Submit", true),
                action("decline", "Decline", false),
                action("cancel", "Cancel", false),
            ],
        }
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// `description` (or `_meta.querymt.description`, or the hoisted
/// `optionDescriptions` entry) when it says more than the label.
fn option_description(option: &Value, label: &str, fallback: Option<&str>) -> Option<String> {
    [
        str_at(option, "description"),
        option
            .pointer("/_meta/querymt/description")
            .and_then(Value::as_str),
        fallback,
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|d| !d.is_empty() && *d != label)
    .map(str::to_owned)
}

fn field_options(prop: &Value, hoisted: Option<&Value>) -> Vec<FieldOption> {
    let titled = |list: &Value| -> Vec<FieldOption> {
        list.as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| {
                let value = str_at(o, "const")?.to_string();
                let label = str_at(o, "title").unwrap_or(&value).to_string();
                let fallback = hoisted.and_then(|h| h.get(&value)).and_then(Value::as_str);
                Some(FieldOption {
                    description: option_description(o, &label, fallback),
                    value,
                    label,
                })
            })
            .collect()
    };
    let plain = |list: &Value, names: Option<&Value>| -> Vec<FieldOption> {
        list.as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(ix, v)| {
                let value = v.as_str()?.to_string();
                let label = names
                    .and_then(|n| n.get(ix))
                    .and_then(Value::as_str)
                    .unwrap_or(&value)
                    .to_string();
                Some(FieldOption {
                    value,
                    label,
                    description: None,
                })
            })
            .collect()
    };
    let source = match str_at(prop, "type") {
        Some("array") => prop.get("items").unwrap_or(&Value::Null),
        _ => prop,
    };
    for key in ["anyOf", "oneOf"] {
        if let Some(list) = source.get(key) {
            let options = titled(list);
            if !options.is_empty() {
                return options;
            }
        }
    }
    source
        .get("enum")
        .map(|list| plain(list, source.get("enumNames")))
        .unwrap_or_default()
}

/// Fields of a form-mode `requestedSchema`, in schema order; `None` when a
/// property has a type this app cannot represent faithfully.
fn form_fields(
    schema: &Value,
    allow_custom: bool,
    hoisted: Option<&Value>,
) -> Option<Vec<FormField>> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Some(Vec::new());
    };
    props
        .iter()
        .map(|(key, prop)| {
            let kind = match str_at(prop, "type")? {
                "string" => FieldKind::String,
                "number" => FieldKind::Number,
                "integer" => FieldKind::Integer,
                "boolean" => FieldKind::Boolean,
                "array" => FieldKind::Array,
                _ => return None,
            };
            let options = field_options(prop, hoisted.and_then(|h| h.get(key)));
            Some(FormField {
                key: key.clone(),
                label: str_at(prop, "title")
                    .filter(|t| !t.is_empty())
                    .unwrap_or(key)
                    .to_string(),
                description: str_at(prop, "description")
                    .filter(|d| !d.trim().is_empty())
                    .map(str::to_owned),
                kind,
                required: required.contains(&key.as_str()),
                allow_custom: allow_custom && !options.is_empty(),
                options,
                default: prop.get("default").cloned(),
            })
        })
        .collect()
}

fn parse_request(
    agent_id: &str,
    rpc_id: Value,
    method: &str,
    params: &Value,
) -> Option<PendingRequest> {
    let session_id = str_at(params, "sessionId").map(str::to_owned);
    let meta = params.pointer("/_meta/querymt");
    let meta_str = |k: &str| meta.and_then(|m| str_at(m, k)).filter(|s| !s.is_empty());
    let (kind, title, detail, identity, fallback_key) = if method == "session/request_permission" {
        let tool = params.get("toolCall").unwrap_or(&Value::Null);
        let title = str_at(tool, "title")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("Permission request")
            .to_string();
        let tool_kind = str_at(tool, "kind").unwrap_or_default().to_string();
        let detail = if tool_kind.is_empty() {
            format!("{title} needs approval before the agent can continue.")
        } else {
            format!("{title} ({tool_kind}) needs approval before the agent can continue.")
        };
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|o| PermOption {
                id: str_at(o, "optionId").unwrap_or_default().to_string(),
                name: str_at(o, "name").unwrap_or_default().to_string(),
                kind: str_at(o, "kind").unwrap_or_default().to_string(),
            })
            .collect();
        // Reuse the transcript's tool parsing for the command / path preview.
        let summary = crate::session::SessionView::tool_call_summary(tool);
        let fallback = str_at(tool, "toolCallId")
            .filter(|id| !id.is_empty())
            .map(|tc| format!("tc:{}:{tc}", session_id.as_deref().unwrap_or_default()));
        (
            RequestKind::Permission { summary, options },
            title,
            detail,
            meta_str("interaction_id").map(str::to_owned),
            fallback,
        )
    } else if method.trim_start_matches('_') == "elicitation/create" {
        let message = str_at(params, "message").unwrap_or_default().to_string();
        let url = (str_at(params, "mode") == Some("url"))
            .then(|| str_at(params, "url").map(str::to_owned))
            .flatten();
        let schema = params.get("requestedSchema");
        let allow_custom = meta_str("source") == Some("builtin:question")
            || [meta, schema.and_then(|s| s.pointer("/_meta/querymt"))]
                .into_iter()
                .flatten()
                .any(|m| {
                    m.get("allow_custom").and_then(Value::as_bool) == Some(true)
                        || m.get("allowCustom").and_then(Value::as_bool) == Some(true)
                });
        let hoisted = meta.and_then(|m| m.get("optionDescriptions"));
        let (fields, unsupported) = match (url.is_some(), schema) {
            (false, Some(schema)) => match form_fields(schema, allow_custom, hoisted) {
                Some(fields) => (fields, false),
                None => (Vec::new(), true),
            },
            _ => (Vec::new(), false),
        };
        let title = schema
            .and_then(|s| str_at(s, "title"))
            .filter(|t| !t.is_empty())
            .unwrap_or("Elicitation request")
            .to_string();
        let detail = if unsupported {
            format!(
                "{message}\n\nThis form uses field types this app cannot display, so it cannot be submitted here."
            )
        } else {
            message
        };
        (
            RequestKind::Elicitation {
                fields,
                url,
                unsupported,
            },
            title,
            detail,
            meta_str("elicitation_id")
                .or_else(|| str_at(params, "elicitationId"))
                .map(str::to_owned),
            None,
        )
    } else {
        return None;
    };
    let stable = identity.is_some() || fallback_key.is_some();
    let key = match identity.as_ref().or(fallback_key.as_ref()) {
        Some(id) => format!("{agent_id}:{id}"),
        None => format!("{agent_id}:rpc:{rpc_id}"),
    };
    let drafts = match &kind {
        RequestKind::Elicitation { fields, .. } => fields
            .iter()
            .map(|f| FieldDraft {
                value: f.initial_value(),
                custom: None,
            })
            .collect(),
        RequestKind::Permission { .. } => Vec::new(),
    };
    Some(PendingRequest {
        drafts,
        key,
        agent_id: agent_id.to_string(),
        rpc_id: Some(rpc_id),
        session_id,
        identity,
        stable,
        received_ms: now_ms(),
        title,
        detail,
        kind,
        status: RequestStatus::Pending,
        offline: false,
        error: None,
        awaiting_ack: false,
        rejected: false,
    })
}

/// The `content` of an accepted elicitation, or the message to show (desktop
/// `validateElicitationFields` + `buildElicitationResponse`).
pub fn elicitation_content(fields: &[FormField], drafts: &[FieldDraft]) -> Result<Value, String> {
    let mut content = serde_json::Map::new();
    for (field, draft) in fields.iter().zip(drafts) {
        let key = field.key.clone();
        if let Some(custom) = &draft.custom {
            let custom = custom.trim();
            if custom.is_empty() {
                return Err("Please enter a custom response.".into());
            }
            let value = if field.kind == FieldKind::Array {
                json!([custom])
            } else {
                Value::from(custom)
            };
            content.insert(key, value);
            continue;
        }
        let incomplete = || format!("Please complete {}.", field.label);
        let text = match &draft.value {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        match field.kind {
            FieldKind::Boolean => {
                content.insert(key, Value::Bool(draft.value.as_bool().unwrap_or(false)));
            }
            FieldKind::Array => {
                let items = draft.value.as_array().cloned().unwrap_or_default();
                if field.required && items.is_empty() {
                    return Err(incomplete());
                }
                content.insert(key, Value::Array(items));
            }
            FieldKind::Number | FieldKind::Integer => {
                let text = text.trim();
                if text.is_empty() {
                    if field.required {
                        return Err(incomplete());
                    }
                    continue;
                }
                let value = if field.kind == FieldKind::Integer {
                    text.parse::<i64>()
                        .map(Value::from)
                        .map_err(|_| format!("{} must be a whole number.", field.label))?
                } else {
                    text.parse::<f64>()
                        .ok()
                        .filter(|n| n.is_finite())
                        .map(Value::from)
                        .ok_or_else(|| format!("{} must be a number.", field.label))?
                };
                content.insert(key, value);
            }
            FieldKind::String => {
                if text.trim().is_empty() {
                    if field.required {
                        return Err(incomplete());
                    }
                    // An empty choice would not match the listed options.
                    if !field.options.is_empty() {
                        continue;
                    }
                }
                content.insert(key, Value::from(text));
            }
        }
    }
    Ok(Value::Object(content))
}

/// Card text of an answered permission (desktop `describePermissionResolution`).
fn permission_resolution(option: &PermOption) -> String {
    match option.kind.as_str() {
        "allow_once" => "Allowed once".into(),
        "allow_always" => "Allowed always".into(),
        "reject_once" => "Rejected once".into(),
        "reject_always" => "Rejected always".into(),
        _ => option.name.clone(),
    }
}

/// Resume authorities for question recovery go only over an encrypted or
/// loopback transport (desktop `supportsElicitationRecoveryTransport`).
fn recovery_transport_ok(endpoint: &Endpoint) -> bool {
    match endpoint {
        Endpoint::Process { .. } => true,
        Endpoint::WebSocket { secure: true, .. } => true,
        Endpoint::WebSocket { address, .. } => {
            let host = address
                .rsplit_once(':')
                .map_or(address.as_str(), |(h, _)| h)
                .trim_matches(['[', ']'])
                .to_ascii_lowercase();
            matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
                || host.ends_with(".localhost")
        }
    }
}

const ELICITATION_RECOVERY_VERSION: u64 = 1;
const ELICITATION_RECOVERY_DENIED: i64 = -32003;

/// `querymt/capabilities` advertises the v1 question-recovery contract.
fn supports_elicitation_recovery(caps: &Value) -> bool {
    let has = |list: &str, name: &str| {
        caps.get(list)
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|m| m.as_str() == Some(name)))
    };
    caps.pointer("/elicitation_recovery/version")
        .and_then(Value::as_u64)
        == Some(ELICITATION_RECOVERY_VERSION)
        && has("methods", "querymt/elicitation/listPendingSessions")
        && has("methods", "querymt/elicitation/attachSession")
        && has("notifications", "querymt/elicitation/recoveryAuthority")
}

/// An OAuth flow waiting for the user to paste the callback code.
#[derive(Clone, Debug)]
pub struct OAuthFlow {
    pub provider: String,
    pub flow_id: String,
    pub needs_code: bool,
}

#[derive(Default)]
pub struct AgentRuntime {
    pub state: ConnState,
    pub agent_title: Option<String>,
    pub agent_version: Option<String>,
    pub capabilities: Option<Value>,
    /// Managed git worktrees, when `initialize` advertises them.
    pub worktrees: Option<launch::WorktreeSupport>,
    /// Profiles of mesh peers, loaded when one is chosen as launch target.
    pub peer_profiles: launch::PeerProfileMap,
    /// Delegate routing per session id.
    pub delegates: HashMap<String, delegates::DelegateState>,
    pub models: Vec<Model>,
    pub sessions: Vec<SessionSummary>,
    pub sessions_loaded: bool,
    /// Older catalog pages are still loading after the first one was shown.
    pub catalog_loading: bool,
    /// Why the last `session/list` failed; cleared by the next success.
    pub sessions_error: Option<String>,
    /// Session ids with a pin / lifecycle / delete request in flight.
    pub session_pending: std::collections::HashSet<String>,
    /// Session id -> error of its last failed action.
    pub session_errors: HashMap<String, String>,
    /// `initialize` advertised `sessionCapabilities.delete`.
    pub can_delete_sessions: bool,
    /// `initialize` advertised `sessionCapabilities.fork`.
    pub can_fork_sessions: bool,
    /// Parent session id -> its loaded direct children.
    pub children: HashMap<String, ChildList>,
    pub(crate) schedules: automations::ScheduleCatalog,
    pub mesh: Option<MeshStatus>,
    pub mesh_nodes: Vec<MeshNode>,
    pub mesh_invites: Vec<MeshInvite>,
    /// URL of the invite created most recently in this app session.
    pub last_invite: Option<String>,
    /// Model providers of `_querymt/auth/status`.
    pub providers: Vec<ProviderAuth>,
    /// MCP server logins (`mcp:<server>`) of `_querymt/auth/status`: they
    /// share the auth methods but are not model providers.
    pub mcp_servers: Vec<ProviderAuth>,
    /// `_querymt/auth/status` answered since connecting.
    pub providers_loaded: bool,
    /// Why the last `_querymt/auth/status` failed.
    pub providers_error: Option<String>,
    /// `_querymt/models` answered (or failed) since connecting.
    pub models_loaded: bool,
    pub profiles: Vec<Profile>,
    pub oauth: Option<OAuthFlow>,
    /// Error from the last settings/mesh action, shown inline.
    pub action_error: Option<String>,
    /// Last maintenance outcome: (message, is_error).
    pub maintenance: Option<(String, bool)>,
    /// (plugin, message) of failed plugin updates.
    pub failed_plugins: Vec<(String, String)>,
    /// The plugin update step in progress.
    pub plugin_progress: Option<plugins::PluginProgress>,
    /// Names of actions in flight on the current connection (e.g. `invite`,
    /// `key:xai`); a connect or disconnect clears them.
    pub pending: std::collections::HashSet<String>,
    pub log: agent_log::AgentLog,
    /// JSON-RPC traffic across reconnects (debug events, Developer mode).
    pub wire: Rc<crate::rpc_log::WireLog>,
    pub pid: Option<u32>,
    /// Set when the local agent process has exited; what an install waits on.
    exit: Option<transport::Exit>,
    pub connected_at: Option<f64>,
    pub busy: bool,
    rpc: Option<Rc<RpcClient>>,
    tasks: Vec<Task<()>>,
    generation: u64,
    /// Consecutive automatic reconnect attempts since the last success.
    reconnect_attempt: u32,
    refresh_scheduled: bool,
    /// A pinned-list refresh after `pinsChanged` is pending.
    pins_refresh_scheduled: bool,
    /// A mesh re-read after `mesh/joined` / `peerExpired` is pending.
    mesh_refresh_scheduled: bool,
    /// Session id -> MCP servers it had to leave out (sign-in notice).
    skipped_mcp: HashMap<String, Vec<mcp_notice::SkippedMcp>>,
    /// Session id -> resume authority for re-attaching its pending questions
    /// after a reconnect (`querymt/elicitation/recoveryAuthority`).
    resume_authorities: HashMap<String, String>,
}

/// Direct children of one session, loaded on demand.
#[derive(Clone, Debug, Default)]
pub struct ChildList {
    pub sessions: Vec<SessionSummary>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
}

impl AgentRuntime {
    /// Every loaded copy of a session: the catalog row and nested child rows.
    fn sessions_mut<'a>(
        &'a mut self,
        session_id: &'a str,
    ) -> impl Iterator<Item = &'a mut SessionSummary> + 'a {
        self.sessions
            .iter_mut()
            .chain(
                self.children
                    .values_mut()
                    .flat_map(|c| c.sessions.iter_mut()),
            )
            .filter(move |s| s.id == session_id)
    }

    fn begin_session_action(&mut self, session_id: &str) {
        self.session_pending.insert(session_id.to_string());
        self.session_errors.remove(session_id);
    }

    fn end_session_action(&mut self, session_id: &str, error: Option<String>) {
        self.session_pending.remove(session_id);
        if let Some(error) = error {
            self.log_error(error.clone());
            self.session_errors.insert(session_id.to_string(), error);
        }
    }

    /// Note something about this agent in its log.
    fn log(&mut self, line: impl Into<String>) {
        self.log_at(agent_log::Level::Info, line.into());
    }

    /// A request or capability that did not work; the agent still runs.
    fn log_warn(&mut self, line: impl Into<String>) {
        self.log_at(agent_log::Level::Warn, line.into());
    }

    fn log_error(&mut self, line: impl Into<String>) {
        self.log_at(agent_log::Level::Error, line.into());
    }

    fn log_at(&mut self, level: agent_log::Level, line: String) {
        #[cfg(not(target_family = "wasm"))]
        if std::env::var_os("QMT_DEBUG").is_some() {
            eprintln!("[agent] {line}");
        }
        self.log.push_app(level, line);
    }

    /// One stderr line of the agent process.
    fn log_agent(&mut self, line: &str) {
        #[cfg(not(target_family = "wasm"))]
        if std::env::var_os("QMT_DEBUG").is_some() {
            eprintln!("[agent stderr] {line}");
        }
        self.log.push_agent(line);
    }

    pub fn is_connected(&self) -> bool {
        self.state == ConnState::Connected
    }

    /// `querymt_control_version` reported by `_querymt/capabilities`.
    pub fn control_version(&self) -> Option<u64> {
        self.capabilities
            .as_ref()?
            .get("querymt_control_version")?
            .as_u64()
    }

    /// Enabled flags of a capabilities object, e.g. `transport` or `features`.
    pub fn capability_flags(&self, group: &str) -> Vec<String> {
        let Some(Value::Object(map)) = self.capabilities.as_ref().and_then(|c| c.get(group)) else {
            return vec![];
        };
        map.iter()
            .filter(|(_, v)| v.as_bool() == Some(true))
            .map(|(k, _)| k.replace('_', " "))
            .collect()
    }

    /// Whether `_querymt/capabilities` lists a logical method (`querymt/session/steer`).
    pub fn supports_method(&self, method: &str) -> bool {
        self.capabilities
            .as_ref()
            .and_then(|c| c.get("methods"))
            .and_then(Value::as_array)
            .is_some_and(|m| m.iter().any(|v| v.as_str() == Some(method)))
    }

    pub fn feature(&self, name: &str) -> bool {
        self.capabilities
            .as_ref()
            .and_then(|c| c.pointer(&format!("/features/{name}")))
            .and_then(Value::as_bool)
            == Some(true)
    }

    /// Resolve / archive are available (desktop `supportsSessionLifecycle`).
    pub fn can_set_lifecycle(&self) -> bool {
        self.is_connected()
            && self.feature("session_lifecycle")
            && self.supports_method("querymt/session/setLifecycle")
    }

    /// Pinning is available (desktop `supportsSessionPins`).
    pub fn can_pin(&self) -> bool {
        self.is_connected() && self.supports_method("querymt/session/setPinned")
    }

    /// The agent can take input mid-turn (steer / queue), like the desktop app checks.
    pub fn can_control_run(&self) -> bool {
        self.feature("steering")
            && self.supports_method("querymt/session/steer")
            && self.supports_method("querymt/session/queue")
            && self.supports_method("querymt/session/runtimeState")
    }

    pub fn method_count(&self) -> usize {
        self.capabilities
            .as_ref()
            .and_then(|c| c.get("methods"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    }
}

// ---------------------------------------------------------------------------
// Persistence

/// A string preference that reads a value of any other type as unset, so one
/// bad key never loses the whole saved state.
fn lenient_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => s,
        _ => String::new(),
    })
}

/// The version of the appearance preferences (theme keys, accent meaning).
///
/// Version 0 is a file saved before themes existed, which stored `orange`
/// as the accent when it was merely the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct AppearanceVersion(u32);

impl AppearanceVersion {
    pub const CURRENT: Self = Self(1);

    /// What a file without the key was saved by.
    pub fn legacy() -> Self {
        Self(0)
    }

    /// A present value that is not a version is read as current, so garbage
    /// never triggers the migration.
    fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(serde_json::Value::deserialize(d)?
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .map_or(Self::CURRENT, Self))
    }
}

impl Default for AppearanceVersion {
    fn default() -> Self {
        Self::CURRENT
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Prefs {
    pub dark: Option<bool>,
    #[serde(default)]
    pub cwd: String,
    /// Agent id -> model id used for new sessions.
    #[serde(default)]
    pub launch_models: HashMap<String, String>,
    pub primary_agent: Option<String>,
    /// Update channel, automatic checks and the skipped version.
    #[serde(default)]
    pub updates: crate::update::UpdatePrefs,
    /// Seeded agents the user deleted (only `local-qmtcode`); they are not
    /// added again on later starts. Reset app data clears the list.
    #[serde(default)]
    pub dismissed_seeds: Vec<String>,
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Width of the turn diff panel the user dragged it to; `None` keeps the
    /// default (44% of the page, at most the chat width).
    #[serde(default)]
    pub turn_diff_width: Option<f32>,
    /// Accent preset id (`blue`, `orange`, …); empty: the theme's own accent.
    #[serde(default)]
    pub accent: String,
    /// Theme key of the dark appearance; empty, unknown or a light theme's
    /// key: QueryMT Dark. Kept as saved until the user picks another.
    #[serde(default, deserialize_with = "lenient_string")]
    pub theme_dark: String,
    /// Theme key of the light appearance (as `theme_dark`).
    #[serde(default, deserialize_with = "lenient_string")]
    pub theme_light: String,
    /// Which version of the appearance preferences saved this file.
    #[serde(
        default = "AppearanceVersion::legacy",
        deserialize_with = "AppearanceVersion::lenient"
    )]
    pub appearance_version: AppearanceVersion,
    #[serde(default)]
    pub show_pinned_sidebar: bool,
    /// Tab order of the sidebar's open sessions (`agent/session`, top first).
    #[serde(default)]
    pub sidebar_open_order: Vec<String>,
    /// Tab order of the sidebar's pinned sessions.
    #[serde(default)]
    pub sidebar_pinned_order: Vec<String>,
    /// Launch mode for new sessions (`build`, `plan`, `review`).
    #[serde(default)]
    pub launch_mode: String,
    /// Reasoning effort for new sessions (`auto`, `low`, …).
    #[serde(default)]
    pub launch_reasoning: String,
    /// Agent id -> profile id for new sessions; absent means the agent default.
    #[serde(default)]
    pub launch_profiles: HashMap<String, String>,
    /// Agent id -> most recently picked model ids (newest first).
    #[serde(default)]
    pub recent_models: HashMap<String, Vec<String>>,
    /// Recently opened root sessions as (agent id, session id), newest first.
    #[serde(default)]
    pub recent_sessions: Vec<(String, String)>,
    /// Mouse-wheel smoothing preset (`off`, `snappy`, `normal`, `smooth`, `glide`).
    #[serde(default)]
    pub scroll_smoothing: String,
    /// Mouse-wheel motion curve (`ease_out`, `ease_in_out`).
    #[serde(default)]
    pub scroll_curve: String,
    /// Mouse-wheel inertia (`off`, `light`, `strong`).
    #[serde(default)]
    pub scroll_inertia: String,
    /// Fade streamed reply text in word by word; `None` means on.
    #[serde(default)]
    pub stream_fade: Option<bool>,
    /// How image attachments are sent in prompts.
    #[serde(default)]
    pub image_send_mode: ImageSendMode,
    /// Shortcut that sends a prompt from the composers.
    #[serde(default)]
    pub send_shortcut: SendShortcut,
    /// List sessions owned by mesh peers alongside local ones.
    #[serde(default)]
    pub show_remote_sessions: bool,
    /// Mesh peers (node ids) whose sessions are listed when
    /// `show_remote_sessions` is on.
    #[serde(default)]
    pub remote_session_peers: Vec<String>,
    /// Time zone last picked for an automation; empty: the system zone.
    #[serde(default)]
    pub schedule_timezone: String,
    /// First-run setup finished or skipped.
    #[serde(default)]
    pub onboarding: setup::Onboarding,
    /// Workspace folders saved on the Workspaces page, newest first: paths
    /// on the machine where the agent runs.
    #[serde(default)]
    pub workspaces: Vec<String>,
    /// Recent model keys per (agent, profile, delegate).
    #[serde(default)]
    pub recent_delegate_models: HashMap<String, Vec<String>>,
    /// Developer-only tools: protocol debug events, message metadata.
    #[serde(default)]
    pub developer_mode: bool,
    /// The quick panel (desktop only).
    #[serde(default)]
    pub quick: QuickPrefs,
}

/// Settings of the quick panel. Off until the end-to-end checks on every
/// platform pass (change `quick-panel-default` then).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickPrefs {
    /// The panel, its shortcut, the tray icon and the background mode are on.
    #[serde(default)]
    pub enabled: bool,
    /// Global shortcut in `global-hotkey` form (`alt+Space`); empty: the
    /// platform default.
    #[serde(default)]
    pub shortcut: String,
    /// How the session picker lays its sessions out.
    #[serde(default)]
    pub picker_layout: PickerLayout,
    /// Where the user left the panel, per display (its uuid, else its id).
    #[serde(default)]
    pub positions: std::collections::BTreeMap<String, QuickPosition>,
}

/// Where the user left the quick panel on one display: its edges in logical
/// pixels from the display's top-left corner. Which edge is kept on reopening
/// is the one the platform keeps while the panel grows (design D18).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickPosition {
    pub left: i32,
    pub top: i32,
    pub bottom: i32,
}

/// The quick panel's session picker: one line per session, or cards in two
/// columns with each session's progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickerLayout {
    #[default]
    List,
    Grid,
}

impl PickerLayout {
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub fn other(self) -> Self {
        match self {
            Self::List => Self::Grid,
            Self::Grid => Self::List,
        }
    }
}

impl Prefs {
    /// The theme shown in the dark or light appearance: the saved key, or the
    /// QueryMT theme when it is empty, unknown or of the other appearance.
    pub fn theme_id(&self, dark: bool) -> crate::theme::ThemeId {
        let saved = if dark {
            &self.theme_dark
        } else {
            &self.theme_light
        };
        crate::theme::ThemeId::resolve(saved, dark)
    }

    /// The saved accent: a preset, or the shown theme's own.
    pub fn accent_choice(&self) -> crate::theme::Accent {
        crate::theme::Accent::parse(&self.accent)
    }

    /// What to show in the `dark` or light appearance.
    pub fn appearance(&self, dark: bool) -> crate::theme::Appearance {
        crate::theme::Appearance {
            theme: self.theme_id(dark),
            accent: self.accent_choice(),
        }
    }
    /// Peers whose sessions the catalog asks for (desktop `remoteNodeIds`).
    pub fn remote_node_ids(&self) -> Vec<String> {
        if self.show_remote_sessions {
            self.remote_session_peers.clone()
        } else {
            Vec::new()
        }
    }

    /// Whether a session of `node` (None: local) belongs in the catalog.
    pub fn shows_node(&self, node: Option<&str>) -> bool {
        node.is_none_or(|n| {
            self.show_remote_sessions && self.remote_session_peers.iter().any(|p| p == n)
        })
    }
}

/// Key combination that sends a prompt; the others insert a new line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendShortcut {
    #[default]
    Enter,
    ShiftEnter,
    /// Ctrl+Enter, or Cmd+Enter on macOS.
    SecondaryEnter,
}

impl SendShortcut {
    /// Whether an Enter press with these modifiers sends the prompt.
    pub fn submits(self, shift: bool, secondary: bool) -> bool {
        match self {
            Self::Enter => !shift,
            Self::ShiftEnter => shift,
            Self::SecondaryEnter => secondary,
        }
    }
}

/// ACP encoding of image attachments in a prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSendMode {
    /// Native ACP `image` content blocks.
    #[default]
    Image,
    /// Embedded `resource` blocks, for agents without image prompt support.
    Resource,
}

impl Prefs {
    pub fn mode(&self) -> &str {
        if self.launch_mode.is_empty() {
            "build"
        } else {
            &self.launch_mode
        }
    }

    pub fn reasoning(&self) -> &str {
        if self.launch_reasoning.is_empty() {
            "auto"
        } else {
            &self.launch_reasoning
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    configs: Vec<AgentConfig>,
    #[serde(default)]
    prefs: Prefs,
}

#[cfg(not(target_family = "wasm"))]
fn state_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("querymt-gpui").join("state.json"))
}

fn load_persisted() -> Option<Persisted> {
    #[cfg(not(target_family = "wasm"))]
    let text = std::fs::read_to_string(state_path()?).ok()?;
    #[cfg(target_family = "wasm")]
    let text = web_sys::window()?
        .local_storage()
        .ok()??
        .get_item("querymt-gpui:state")
        .ok()??;
    serde_json::from_str(&text).ok()
}

/// The quick panel settings as saved, read before the app starts (a second
/// instance decides from them whether to forward its request).
#[cfg(not(target_family = "wasm"))]
pub fn persisted_quick_prefs() -> QuickPrefs {
    load_persisted().map(|p| p.prefs.quick).unwrap_or_default()
}

fn save_persisted(state: &Persisted) {
    // Tests must never overwrite the user's state.
    if cfg!(test) {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(state) else {
        return;
    };
    #[cfg(not(target_family = "wasm"))]
    if let Some(path) = state_path() {
        // Write a sibling file and rename it over the old one, so a crash or a
        // full disk mid-write never leaves a truncated state file behind.
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(err) = write() {
            eprintln!("querymt-gpui: failed to save {}: {err}", path.display());
        }
    }
    #[cfg(target_family = "wasm")]
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        // Quota or private-mode failures are not actionable here; the next
        // successful save rewrites the whole state.
        let _ = storage.set_item("querymt-gpui:state", &text);
    }
}

/// Agents to add on first run (or that the environment asks for).
fn seed_configs() -> Vec<AgentConfig> {
    let mut seeds = Vec::new();
    #[cfg(not(target_family = "wasm"))]
    {
        // `--agent host:port` or QMT_AGENT=host:port adds a WebSocket agent.
        let args: Vec<String> = std::env::args().collect();
        let from_arg = args
            .iter()
            .position(|a| a == "--agent")
            .and_then(|i| args.get(i + 1).cloned());
        if let Some(address) = from_arg.or_else(|| std::env::var("QMT_AGENT").ok())
            && let Ok((address, secure)) = normalize_address(&address)
        {
            seeds.push(AgentConfig {
                id: format!("ws-{address}"),
                name: address.clone(),
                endpoint: Endpoint::WebSocket { address, secure },
                auto_start: true,
                host_managed: false,
            });
        }
        let exe = std::env::current_exe().ok();
        let path_var = std::env::var_os("PATH");
        seeds.extend(local_qmtcode_seed(
            exe.as_deref(),
            path_var.as_deref(),
            std::env::consts::EXE_SUFFIX,
        ));
    }
    #[cfg(target_family = "wasm")]
    if let Some(location) = web_sys::window().map(|w| w.location()) {
        // `?agent=host:port` points the page at a specific server; otherwise
        // the server serving this page is the agent (embedded mode).
        let query = location.search().unwrap_or_default();
        let requested = query
            .trim_start_matches('?')
            .split('&')
            .find_map(|kv| kv.strip_prefix("agent="))
            .map(|v| {
                js_sys::decode_uri_component(v)
                    .ok()
                    .and_then(|s| s.as_string())
                    .unwrap_or(v.into())
            });
        let protocol = location.protocol().unwrap_or_default();
        let (address, secure, host_managed) = match requested {
            Some(addr) => match normalize_address(&addr) {
                Ok((a, s)) => (Some(a), s, false),
                Err(_) => (None, false, false),
            },
            None if protocol == "http:" || protocol == "https:" => {
                (location.host().ok(), protocol == "https:", true)
            }
            None => (None, false, false),
        };
        if let Some(address) = address {
            seeds.push(AgentConfig {
                id: format!("ws-{address}"),
                name: if host_managed {
                    "This server".into()
                } else {
                    address.clone()
                },
                endpoint: Endpoint::WebSocket { address, secure },
                auto_start: true,
                host_managed,
            });
        }
    }
    seeds
}

/// Id of the seeded agent that runs the bundled (or PATH) qmtcode.
#[cfg(not(target_family = "wasm"))]
const LOCAL_QMTCODE_ID: &str = "local-qmtcode";

/// "Local qmtcode", when a bundled copy exists or qmtcode is on PATH.
#[cfg(not(target_family = "wasm"))]
fn local_qmtcode_seed(
    exe: Option<&std::path::Path>,
    path_var: Option<&std::ffi::OsStr>,
    exe_suffix: &str,
) -> Option<AgentConfig> {
    crate::local_agent::available_with_suffix(exe, path_var, exe_suffix).then(|| AgentConfig {
        id: LOCAL_QMTCODE_ID.into(),
        name: "Local qmtcode".into(),
        endpoint: Endpoint::Process {
            command: "qmtcode --acp".into(),
        },
        auto_start: true,
        host_managed: false,
    })
}

/// The agents, preferences and first-run flag a start begins with: the saved
/// state (if any) plus the seeds that are neither already present nor
/// dismissed.
fn startup_state(
    loaded: Option<Persisted>,
    seeds: Vec<AgentConfig>,
) -> (Vec<AgentConfig>, Prefs, bool) {
    // No saved state: a fresh install, decided before anything is saved.
    // State saved before setup existed is an existing user: setup is done.
    let first_run = loaded.is_none();
    let mut persisted = loaded.unwrap_or_default();
    if !first_run && !persisted.prefs.onboarding.done() {
        persisted.prefs.onboarding = setup::Onboarding {
            version: setup::ONBOARDING_VERSION,
            completed_at: Some(crate::util::rfc3339_now()),
            skipped_at: None,
        };
    }
    // Before themes existed `orange` was what the Accent row saved while it
    // showed the default selected, not a choice: it is the theme's accent now.
    if persisted.prefs.appearance_version < AppearanceVersion::CURRENT {
        if persisted.prefs.accent == "orange" {
            persisted.prefs.accent.clear();
        }
        persisted.prefs.appearance_version = AppearanceVersion::CURRENT;
    }
    let mut configs = persisted.configs;
    for seed in seeds {
        if persisted.prefs.dismissed_seeds.contains(&seed.id) {
            continue;
        }
        if !configs
            .iter()
            .any(|c| c.id == seed.id || c.endpoint == seed.endpoint)
        {
            configs.insert(0, seed);
        }
    }
    (configs, persisted.prefs, first_run)
}

// ---------------------------------------------------------------------------
// Store

/// The start page's choice of where the next session runs. A launch copies it
/// into its [`launch::LaunchRequest`]; its progress belongs to the caller.
#[derive(Default)]
pub struct LaunchState {
    /// Mesh peer the next session runs on; `None`: the agent's machine.
    pub target: Option<String>,
    /// Worktree placement of the next local session.
    pub worktree: launch::WorktreeMode,
    /// Profile on the target peer; `None`: the peer's active profile.
    pub remote_profile: Option<String>,
}

/// The open sessions. The index is weak: a session lives exactly as long as
/// some surface holds its entity.
#[derive(Default)]
struct OpenSessions {
    hub: HashMap<SessionKey, WeakEntity<OpenSession>>,
    /// What the store hears from each live session.
    subs: HashMap<SessionKey, Subscription>,
    /// A session a test window starts with in its main pane.
    #[cfg(test)]
    test_main: Option<Entity<OpenSession>>,
}

/// What the store tells every surface that shows sessions.
#[derive(Clone, Debug)]
pub(crate) enum StoreEvent {
    /// A session was made from `from` (a fork, a worktree). A surface that
    /// shows `from` may switch to `child`.
    Spawned { from: SessionKey, child: SessionRef },
    /// The agent deleted this session.
    Deleted(SessionKey),
}

impl EventEmitter<StoreEvent> for AgentsStore {}

pub struct AgentsStore {
    pub configs: Vec<AgentConfig>,
    pub prefs: Prefs,
    pub launch: LaunchState,
    /// The sessions open in the app (design D1).
    open_sessions: OpenSessions,
    /// Permission and input requests waiting for the user (the Inbox).
    pub requests: Vec<PendingRequest>,
    runtime: HashMap<String, AgentRuntime>,
    /// This start is a fresh install (or follows Reset app data), and setup
    /// was not finished or skipped yet.
    pub first_run: bool,
    /// Sidebar lists: sessions already seen this run.
    sidebar: sidebar::SidebarState,
    /// Slash commands advertised for sessions that have no view yet, until
    /// a view takes them (design D3).
    early_commands: early_commands::EarlyCommands,
    /// Self-update: what the last check found and the schedule (native only).
    #[cfg(not(target_family = "wasm"))]
    pub(crate) updates: updates::Updates,
}

impl AgentsStore {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (configs, prefs, first_run) = startup_state(load_persisted(), seed_configs());
        let mut this = Self {
            configs,
            prefs,
            launch: LaunchState::default(),
            open_sessions: Default::default(),
            requests: Vec::new(),
            runtime: HashMap::new(),
            sidebar: Default::default(),
            early_commands: Default::default(),
            #[cfg(not(target_family = "wasm"))]
            updates: Default::default(),
            first_run,
        };
        this.save();
        #[cfg(not(target_family = "wasm"))]
        this.start_update_scheduler(cx);
        let auto: Vec<String> = this
            .configs
            .iter()
            .filter(|c| c.auto_start)
            .map(|c| c.id.clone())
            .collect();
        for id in auto {
            this.connect(&id, cx);
        }
        this
    }

    pub fn save(&self) {
        save_persisted(&Persisted {
            configs: self.configs.clone(),
            prefs: self.prefs.clone(),
        });
    }

    // ---- queries ------------------------------------------------------------

    pub fn config(&self, id: &str) -> Option<&AgentConfig> {
        self.configs.iter().find(|c| c.id == id)
    }

    pub fn runtime(&self, id: &str) -> Option<&AgentRuntime> {
        self.runtime.get(id)
    }

    /// Display name of the mesh node serving `m` on `agent`: the catalog's
    /// label, else the mesh's label of that node, else the short peer id
    /// (as the Mesh page shows it). `None` for the agent's own models.
    pub fn model_node_name(&self, agent: &str, m: &Model) -> Option<String> {
        let id = m.node_id.as_deref()?;
        m.node
            .as_deref()
            .filter(|label| !label.is_empty() && *label != id)
            .map(str::to_owned)
            .or_else(|| {
                self.runtime(agent)?
                    .mesh_nodes
                    .iter()
                    .find(|n| n.id == id && !n.label.is_empty())
                    .map(|n| n.label.clone())
            })
            .or_else(|| Some(crate::util::short_peer(id)))
    }

    pub fn state(&self, id: &str) -> ConnState {
        self.runtime
            .get(id)
            .map(|r| r.state.clone())
            .unwrap_or_default()
    }

    pub fn connected_ids(&self) -> Vec<String> {
        self.configs
            .iter()
            .filter(|c| {
                self.runtime
                    .get(&c.id)
                    .is_some_and(AgentRuntime::is_connected)
            })
            .map(|c| c.id.clone())
            .collect()
    }

    /// Agent the Start page launches on: the preferred one if connected,
    /// otherwise the first connected agent.
    pub fn primary_agent(&self) -> Option<String> {
        let connected = self.connected_ids();
        self.prefs
            .primary_agent
            .clone()
            .filter(|id| connected.contains(id))
            .or_else(|| connected.first().cloned())
    }

    /// Sessions of connected agents; remote ones only from included peers.
    pub fn all_sessions(&self) -> Vec<&SessionSummary> {
        self.runtime
            .values()
            .filter(|r| r.is_connected())
            .flat_map(|r| r.sessions.iter())
            .filter(|s| {
                self.prefs
                    .shows_node(s.remote.as_ref().map(|r| r.node_id.as_str()))
            })
            .collect()
    }

    /// Turn listing of mesh peers' sessions on or off, then reload the
    /// catalogs so they include or drop those sessions.
    pub fn set_show_remote_sessions(&mut self, on: bool, cx: &mut Context<Self>) {
        self.prefs.show_remote_sessions = on;
        self.save();
        self.refresh_all(cx);
        cx.notify();
    }

    /// Include or exclude one mesh peer's sessions, then reload the catalogs.
    pub fn set_remote_session_peer(&mut self, node: &str, on: bool, cx: &mut Context<Self>) {
        let peers = &mut self.prefs.remote_session_peers;
        peers.retain(|p| p != node);
        if on {
            peers.push(node.to_string());
        }
        self.save();
        if self.prefs.show_remote_sessions {
            self.refresh_all(cx);
        }
        cx.notify();
    }

    pub fn models(&self, id: &str) -> &[Model] {
        self.runtime.get(id).map_or(&[], |r| &r.models)
    }

    pub fn profiles(&self, id: &str) -> &[Profile] {
        self.runtime.get(id).map_or(&[], |r| &r.profiles)
    }

    /// Profile for new sessions on `agent_id`: the saved choice while the agent
    /// still offers it, otherwise [`DEFAULT_PROFILE`].
    pub fn launch_profile(&self, agent_id: &str) -> &str {
        let profiles = self.profiles(agent_id);
        self.prefs
            .launch_profiles
            .get(agent_id)
            .filter(|id| profiles.iter().any(|p| &p.id == *id))
            .map_or(DEFAULT_PROFILE, String::as_str)
    }

    pub fn set_launch_profile(&mut self, agent_id: &str, profile_id: &str, cx: &mut Context<Self>) {
        if profile_id == DEFAULT_PROFILE {
            self.prefs.launch_profiles.remove(agent_id);
        } else {
            self.prefs
                .launch_profiles
                .insert(agent_id.to_string(), profile_id.to_string());
        }
        self.save();
        cx.notify();
    }

    /// The model a launch starts on: `choice` (a selection key) when the
    /// agent lists it, else the launch default.
    pub fn model_to_launch(&self, agent_id: &str, choice: Option<&str>) -> Option<&Model> {
        choice
            .and_then(|key| find_model(self.models(agent_id), key))
            .or_else(|| self.launch_model(agent_id))
    }

    pub fn launch_model(&self, agent_id: &str) -> Option<&Model> {
        let models = self.models(agent_id);
        self.prefs
            .launch_models
            .get(agent_id)
            .and_then(|key| find_model(models, key))
            .or_else(|| models.first())
    }

    pub fn needs_attention(&self, id: &str) -> Option<String> {
        match &self.runtime.get(id)?.state {
            ConnState::Failed(reason) => Some(reason.clone()),
            _ => None,
        }
    }

    // ---- config mutations ---------------------------------------------------

    pub fn add_config(
        &mut self,
        name: String,
        endpoint: Endpoint,
        cx: &mut Context<Self>,
    ) -> String {
        let id = slug(&name);
        self.configs.push(AgentConfig {
            id: id.clone(),
            name,
            endpoint,
            auto_start: true,
            host_managed: false,
        });
        self.save();
        self.connect(&id, cx);
        id
    }

    pub fn update_config(
        &mut self,
        id: &str,
        name: String,
        endpoint: Endpoint,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.configs.iter_mut().find(|c| c.id == id) else {
            return;
        };
        let endpoint_changed = config.endpoint != endpoint;
        config.name = name;
        config.endpoint = endpoint;
        self.save();
        if endpoint_changed {
            self.connect(id, cx);
        }
        cx.notify();
    }

    pub fn delete_config(&mut self, id: &str, cx: &mut Context<Self>) {
        self.disconnect(id, cx);
        self.runtime.remove(id);
        self.configs.retain(|c| c.id != id);
        self.prefs.launch_models.remove(id);
        self.prefs.launch_profiles.remove(id);
        // A deleted seeded agent stays deleted; `--agent` agents come back
        // whenever they are requested again.
        #[cfg(not(target_family = "wasm"))]
        if id == LOCAL_QMTCODE_ID && !self.prefs.dismissed_seeds.iter().any(|s| s == id) {
            self.prefs.dismissed_seeds.push(id.to_owned());
        }
        self.save();
        cx.notify();
    }

    pub fn toggle_auto_start(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(config) = self.configs.iter_mut().find(|c| c.id == id) {
            config.auto_start = !config.auto_start;
            self.save();
            cx.notify();
        }
    }

    /// Turn the quick panel on or off.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub fn set_quick_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.prefs.quick.enabled != on {
            self.prefs.quick.enabled = on;
            self.save();
            cx.notify();
        }
    }

    /// The shortcut of the quick panel (`global-hotkey` form).
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub fn set_quick_shortcut(&mut self, shortcut: &str, cx: &mut Context<Self>) {
        if self.prefs.quick.shortcut != shortcut {
            self.prefs.quick.shortcut = shortcut.to_owned();
            self.save();
            cx.notify();
        }
    }

    /// Remember where the quick panel was left on `display`, in memory: the
    /// panel writes it to disk once the window stops moving.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub fn remember_quick_position(&mut self, display: String, position: QuickPosition) {
        self.prefs.quick.positions.insert(display, position);
    }

    /// The layout of the quick panel's session picker.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub fn set_quick_picker_layout(&mut self, layout: PickerLayout, cx: &mut Context<Self>) {
        if self.prefs.quick.picker_layout != layout {
            self.prefs.quick.picker_layout = layout;
            self.save();
            cx.notify();
        }
    }

    pub fn set_primary_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        self.prefs.primary_agent = Some(id.to_string());
        self.save();
        cx.notify();
    }

    /// Recently chosen models, newest first, each machine's copy distinct.
    pub fn recent_models(&self, agent_id: &str) -> Vec<&Model> {
        let models = self.models(agent_id);
        let mut seen = std::collections::HashSet::new();
        self.prefs
            .recent_models
            .get(agent_id)
            .into_iter()
            .flatten()
            .filter_map(|key| find_model(models, key))
            .filter(|m| seen.insert(m.key()))
            .collect()
    }

    /// Put a chosen model (selection key) first in the agent's recent list.
    fn remember_model(&mut self, agent_id: &str, key: &str) {
        let recent = self
            .prefs
            .recent_models
            .entry(agent_id.to_string())
            .or_default();
        recent.retain(|k| k != key);
        recent.insert(0, key.to_string());
        recent.truncate(5);
    }

    pub fn set_launch_mode(&mut self, mode: &str, cx: &mut Context<Self>) {
        self.prefs.launch_mode = mode.to_string();
        self.save();
        cx.notify();
    }

    pub fn set_launch_reasoning(&mut self, reasoning: &str, cx: &mut Context<Self>) {
        self.prefs.launch_reasoning = reasoning.to_string();
        self.save();
        cx.notify();
    }

    /// Pin or unpin a session (`_querymt/session/setPinned`), optimistically.
    pub fn set_pinned(
        &mut self,
        agent_id: &str,
        session_id: &str,
        pinned: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone() else { return };
        for s in rt.sessions_mut(session_id) {
            s.pinned = pinned;
        }
        rt.begin_session_action(session_id);
        cx.notify();
        let (agent_id, session_id) = (agent_id.to_string(), session_id.to_string());
        cx.spawn(async move |this, cx| {
            let result = rpc
                .request(
                    "_querymt/session/setPinned",
                    json!({ "session_id": session_id, "pinned": pinned }),
                )
                .await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.runtime.get_mut(&agent_id) else {
                    return;
                };
                let error = result.err().map(|err| {
                    for sess in rt.sessions_mut(&session_id) {
                        sess.pinned = !pinned;
                    }
                    let verb = if pinned { "pin" } else { "unpin" };
                    format!("Could not {verb} the session: {err}")
                });
                rt.end_session_action(&session_id, error);
                cx.notify();
            });
        })
        .detach();
    }

    /// Record an opened root session for the Sessions page's Recent group
    /// (desktop `rememberRecentSession`): forks, delegates and other children
    /// are never recorded.
    fn remember_recent_session(&mut self, agent_id: &str, session_id: &str) {
        const LIMIT: usize = 50;
        let root = self
            .session_summary(agent_id, session_id)
            .is_some_and(|s| s.root && s.fork_origin.is_none());
        let entry = (agent_id.to_string(), session_id.to_string());
        if !root || self.prefs.recent_sessions.first() == Some(&entry) {
            return;
        }
        let recent = &mut self.prefs.recent_sessions;
        recent.retain(|e| e != &entry);
        recent.insert(0, entry);
        recent.truncate(LIMIT);
        self.save();
    }

    /// Delete a session and its history (`session/delete`). Only offered when
    /// `initialize` advertised `sessionCapabilities.delete`.
    pub fn delete_session(&mut self, agent_id: &str, session_id: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone().filter(|_| rt.can_delete_sessions) else {
            return;
        };
        rt.begin_session_action(session_id);
        cx.notify();
        let (agent_id, session_id) = (agent_id.to_string(), session_id.to_string());
        cx.spawn(async move |this, cx| {
            let result = rpc
                .request("session/delete", json!({ "sessionId": session_id }))
                .await;
            let _ = this.update(cx, |s, cx| {
                if result.is_ok() {
                    let before = s.prefs.recent_sessions.len();
                    s.prefs
                        .recent_sessions
                        .retain(|(a, id)| a != &agent_id || id != &session_id);
                    if s.prefs.recent_sessions.len() != before {
                        s.save();
                    }
                }
                let Some(rt) = s.runtime.get_mut(&agent_id) else {
                    return;
                };
                match result {
                    Ok(_) => {
                        rt.end_session_action(&session_id, None);
                        rt.sessions.retain(|x| x.id != session_id);
                        rt.children.remove(&session_id);
                        for list in rt.children.values_mut() {
                            list.sessions.retain(|x| x.id != session_id);
                        }
                        cx.emit(StoreEvent::Deleted(SessionKey::new(&agent_id, &session_id)));
                    }
                    Err(err) => rt.end_session_action(
                        &session_id,
                        Some(format!("Could not delete the session: {err}")),
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Direct children of a session (forks and worktree children; the agent
    /// leaves delegates out), via `session/list` with the `children` scope.
    pub fn load_children(&mut self, agent_id: &str, parent_id: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone().filter(|_| rt.is_connected()) else {
            return;
        };
        let list = rt.children.entry(parent_id.to_string()).or_default();
        if list.loading {
            return;
        }
        list.loading = true;
        list.error = None;
        cx.notify();
        let (generation, agent_id, parent_id) =
            (rt.generation, agent_id.to_string(), parent_id.to_string());
        let task = cx.spawn(async move |this, cx| {
            let meta =
                json!({ "session_scope": "children", "querymt": { "parentSessionId": parent_id } });
            let result = fetch_session_pages(&rpc, &agent_id, Some(meta), CHILD_PAGES).await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&agent_id, generation) else {
                    return;
                };
                let list = rt.children.entry(parent_id.clone()).or_default();
                list.loading = false;
                match result {
                    Ok(mut children) => {
                        children.retain(|c| {
                            c.parent.as_deref() == Some(parent_id.as_str())
                                && c.fork_origin.as_deref() != Some("delegation")
                        });
                        children.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                        list.sessions = children;
                        list.loaded = true;
                    }
                    Err(err) => list.error = Some(format!("Could not load child sessions: {err}")),
                }
                cx.notify();
            });
        });
        rt.tasks.push(task);
        rt.tasks.retain(|t| !t.is_ready());
    }

    /// Choose the model new sessions start with (a selection key).
    pub fn set_launch_model(&mut self, agent_id: &str, key: String, cx: &mut Context<Self>) {
        self.remember_model(agent_id, &key);
        self.prefs.launch_models.insert(agent_id.to_string(), key);
        self.save();
        cx.notify();
    }

    pub fn set_schedule_timezone(&mut self, zone: String) {
        if self.prefs.schedule_timezone != zone {
            self.prefs.schedule_timezone = zone;
            self.save();
        }
    }

    /// Save a workspace folder (already validated), first in the list.
    /// Returns false when it was saved before.
    pub fn add_workspace(&mut self, path: String, cx: &mut Context<Self>) -> bool {
        if self.prefs.workspaces.contains(&path) {
            return false;
        }
        self.prefs.workspaces.insert(0, path);
        self.save();
        cx.notify();
        true
    }

    /// Forget a saved workspace; the folder itself is untouched.
    pub fn remove_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        let before = self.prefs.workspaces.len();
        self.prefs.workspaces.retain(|w| w != path);
        if self.prefs.workspaces.len() != before {
            self.save();
            cx.notify();
        }
    }

    pub fn set_cwd(&mut self, cwd: String) {
        if self.prefs.cwd != cwd {
            self.prefs.cwd = cwd;
            self.save();
        }
    }

    pub fn set_dark(&mut self, dark: bool) {
        self.prefs.dark = Some(dark);
        self.save();
    }

    /// `None` follows the system appearance.
    pub fn set_theme_mode(&mut self, dark: Option<bool>, cx: &mut Context<Self>) {
        self.prefs.dark = dark;
        self.save();
        cx.notify();
    }

    pub fn update_prefs(&mut self, f: impl FnOnce(&mut Prefs), cx: &mut Context<Self>) {
        f(&mut self.prefs);
        self.save();
        cx.notify();
    }

    /// Forget everything this app stores (agent configurations and
    /// preferences) and start over like a fresh install. Sessions on the
    /// agents, provider sign-ins and project files are not touched.
    pub fn reset_app_data(&mut self, cx: &mut Context<Self>) {
        self.reset_with_seeds(seed_configs(), cx);
    }

    /// [`Self::reset_app_data`] over the given seeds. The preferences are
    /// replaced, so earlier dismissals no longer hide a seed.
    fn reset_with_seeds(&mut self, seeds: Vec<AgentConfig>, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.runtime.keys().cloned().collect();
        for id in &ids {
            self.disconnect(id, cx);
        }
        self.runtime.clear();
        self.requests.clear();
        self.open_sessions = OpenSessions::default();
        self.launch = LaunchState::default();
        self.prefs = Prefs::default();
        // The channel is back to the build's: an offer found (or a check
        // started) under the old one no longer applies.
        #[cfg(not(target_family = "wasm"))]
        self.updates.invalidate();
        self.configs = seeds;
        // Starts over like a fresh install, setup included.
        self.first_run = true;
        self.save();
        let auto: Vec<String> = self
            .configs
            .iter()
            .filter(|c| c.auto_start)
            .map(|c| c.id.clone())
            .collect();
        for id in auto {
            self.connect(&id, cx);
        }
        cx.notify();
    }

    // ---- extension calls (mesh, auth) --------------------------------------

    /// Call an agent extension method; `then` runs on the store with the
    /// result. A connect or disconnect in between makes the answer moot: it
    /// then changes nothing (the new generation cleared the pending tag).
    fn ext(
        &mut self,
        id: &str,
        tag: &str,
        method: &'static str,
        params: Value,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &str, Result<Value, String>, &mut Context<Self>) + 'static,
    ) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone() else {
            rt.action_error = Some("The agent is not connected.".into());
            cx.notify();
            return;
        };
        rt.action_error = None;
        rt.pending.insert(tag.to_string());
        let (generation, id, tag) = (rt.generation, id.to_string(), tag.to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = rpc.request(method, params).await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&id, generation) else {
                    return;
                };
                rt.pending.remove(&tag);
                if let Err(err) = &result {
                    rt.action_error = Some(err.clone());
                }
                then(s, &id, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn is_pending(&self, id: &str, tag: &str) -> bool {
        self.runtime
            .get(id)
            .is_some_and(|r| r.pending.contains(tag))
    }

    pub fn clear_action_error(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.action_error = None;
            cx.notify();
        }
    }

    /// Ask the agent to rebuild its model catalog, then reload the list and capabilities.
    pub fn refresh_models(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone().filter(|_| rt.is_connected()) else {
            return;
        };
        let (generation, id) = (rt.generation, id.to_string());
        rt.pending.insert("models".into());
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let refreshed = rpc
                .request(
                    "_querymt/refreshModels",
                    json!({ "wait_for_completion": true }),
                )
                .await;
            let models = match refreshed {
                Ok(v) if v.get("models").is_some() => Ok(v),
                _ => rpc.request("_querymt/models", json!({})).await,
            };
            let models = match models {
                Ok(v) => parse_models(&v),
                Err(err) => {
                    let _ = this.update(cx, |s, cx| {
                        if let Some(rt) = s.live(&id, generation) {
                            rt.log_warn(format!("Models unavailable: {err}"));
                            rt.pending.remove("models");
                        }
                        cx.notify();
                    });
                    return;
                }
            };
            let models = hydrate_model_info(&rpc, models).await;
            let _ = this.update(cx, |s, cx| {
                if let Some(rt) = s.live(&id, generation) {
                    rt.models = models;
                    rt.pending.remove("models");
                }
                cx.notify();
            });
        });
        rt.tasks.push(task);
    }

    /// Re-fetch mesh, providers and profiles (not sessions/models).
    pub fn refresh_extras(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone().filter(|_| rt.is_connected()) else {
            return;
        };
        let has_mesh = rt
            .capabilities
            .as_ref()
            .and_then(|c| c.pointer("/features/mesh"))
            .and_then(Value::as_bool)
            == Some(true);
        let (generation, id) = (rt.generation, id.to_string());
        rt.pending.insert("extras".into());
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            load_extras(this.clone(), id.clone(), generation, rpc, has_mesh, cx).await;
            let _ = this.update(cx, |s, cx| {
                if let Some(rt) = s.runtime.get_mut(&id) {
                    rt.pending.remove("extras");
                }
                cx.notify();
            });
        });
        rt.tasks.push(task);
    }

    pub fn create_invite(
        &mut self,
        id: &str,
        ttl: &str,
        max_uses: Option<u64>,
        mesh_name: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut params = json!({ "ttl": ttl });
        if let Some(n) = max_uses {
            params["max_uses"] = json!(n);
        }
        if let Some(name) = mesh_name.filter(|n| !n.trim().is_empty()) {
            params["mesh_name"] = json!(name);
        }
        self.ext(
            id,
            "invite",
            "_querymt/mesh/createInvite",
            params,
            cx,
            |s, id, res, cx| {
                if let (Ok(v), Some(rt)) = (&res, s.runtime.get_mut(id)) {
                    rt.last_invite = v.get("url").and_then(Value::as_str).map(str::to_owned);
                }
                s.refresh_extras(id, cx);
            },
        );
    }

    pub fn revoke_invite(&mut self, id: &str, invite_id: &str, cx: &mut Context<Self>) {
        let params = json!({ "invite_id": invite_id });
        self.ext(
            id,
            &format!("revoke:{invite_id}"),
            "_querymt/mesh/revokeInvite",
            params,
            cx,
            |s, id, _, cx| s.refresh_extras(id, cx),
        );
    }

    pub fn dismiss_invite(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.last_invite = None;
            cx.notify();
        }
    }

    pub fn set_api_key(&mut self, id: &str, provider: &str, key: String, cx: &mut Context<Self>) {
        let params = json!({ "provider": provider, "api_key": key });
        self.ext(
            id,
            &format!("key:{provider}"),
            "_querymt/auth/setApiToken",
            params,
            cx,
            |s, id, _, cx| s.refresh_extras(id, cx),
        );
    }

    pub fn clear_api_key(&mut self, id: &str, provider: &str, cx: &mut Context<Self>) {
        let params = json!({ "provider": provider });
        self.ext(
            id,
            &format!("key:{provider}"),
            "_querymt/auth/clearApiToken",
            params,
            cx,
            |s, id, _, cx| s.refresh_extras(id, cx),
        );
    }

    /// Preferred credential source (`querymt/auth/setMethod`); `auto` clears it.
    pub fn set_auth_method(
        &mut self,
        id: &str,
        provider: &str,
        method: &str,
        cx: &mut Context<Self>,
    ) {
        let params = json!({ "provider": provider, "method": if method == "auto" { Value::Null } else { json!(method) } });
        self.ext(
            id,
            &format!("method:{provider}"),
            "_querymt/auth/setMethod",
            params,
            cx,
            |s, id, _, cx| s.refresh_extras(id, cx),
        );
    }

    pub fn logout_provider(&mut self, id: &str, provider: &str, cx: &mut Context<Self>) {
        let params = json!({ "provider": provider });
        self.ext(
            id,
            &format!("oauth:{provider}"),
            "_querymt/auth/logout",
            params,
            cx,
            |s, id, _, cx| s.refresh_extras(id, cx),
        );
    }

    /// Start OAuth: opens the authorization URL; redirect flows then wait for a pasted code.
    pub fn start_oauth(&mut self, id: &str, provider: &str, cx: &mut Context<Self>) {
        let provider_name = provider.to_string();
        let params = json!({ "provider": provider });
        self.ext(
            id,
            &format!("oauth:{provider}"),
            "_querymt/auth/start",
            params,
            cx,
            move |s, id, res, cx| {
                let Ok(v) = res else { return };
                if let Some(url) = v.get("authorization_url").and_then(Value::as_str) {
                    cx.open_url(url);
                }
                let flow_id = v
                    .get("flow_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let needs_code = v.get("flow_kind").and_then(Value::as_str) != Some("device_poll");
                if let Some(rt) = s.runtime.get_mut(id) {
                    rt.oauth = Some(OAuthFlow {
                        provider: provider_name,
                        flow_id,
                        needs_code,
                    });
                }
            },
        );
    }

    /// Finish OAuth with the pasted callback URL/code (empty for device flows).
    pub fn complete_oauth(&mut self, id: &str, response: String, cx: &mut Context<Self>) {
        let Some(flow) = self.runtime.get(id).and_then(|r| r.oauth.clone()) else {
            return;
        };
        let params = json!({ "flow_id": flow.flow_id, "response": response });
        let tag = format!("oauth:{}", flow.provider);
        self.ext(
            id,
            &tag,
            "_querymt/auth/complete",
            params,
            cx,
            |s, id, res, cx| {
                if res.is_ok()
                    && let Some(rt) = s.runtime.get_mut(id)
                {
                    rt.oauth = None;
                }
                s.refresh_extras(id, cx);
            },
        );
    }

    pub fn cancel_oauth(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.oauth = None;
            cx.notify();
        }
    }

    // ---- connection lifecycle ----------------------------------------------

    pub fn connect(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(config) = self.config(id).cloned() else {
            return;
        };
        let config_id = config.id.clone();
        let developer = self.prefs.developer_mode;
        // A new connection generation: what the old one advertised is stale.
        self.early_commands.clear_agent(id);
        let rt = self.runtime.entry(id.to_string()).or_default();
        rt.wire.set_enabled(developer);
        if let Some(rpc) = rt.rpc.take() {
            rpc.close();
        }
        rt.tasks.clear();
        rt.generation += 1;
        // Actions in flight belonged to the old connection.
        rt.pending.clear();
        let generation = rt.generation;
        rt.schedules = Default::default();
        rt.providers_loaded = false;
        rt.providers_error = None;
        rt.models_loaded = false;
        rt.state = ConnState::Connecting;
        rt.pid = None;
        rt.exit = None;
        rt.log(format!("Connecting to {}…", config.endpoint_label()));
        cx.notify();

        let id = id.to_string();
        // Connect → initialize → details (capabilities, models, catalog, mesh).
        let connect_span = tracing::info_span!(
            "agent.connect",
            agent.id = %id,
            agent.endpoint = %config.endpoint_label(),
        );
        let task = cx.spawn(async move |this, cx| { async move {
            let conn = match transport::connect(config.target())
                .instrument(tracing::info_span!("transport.connect"))
                .await
            {
                Ok(conn) => conn,
                Err(err) => {
                    let _ = this.update(cx, |s, cx| s.fail(&id, generation, err, cx));
                    return;
                }
            };
            let rpc = RpcClient::new(conn.outgoing.clone());
            let reader = {
                let (rpc, id, this, incoming) = (rpc.clone(), id.clone(), this.clone(), conn.incoming);
                cx.spawn(async move |cx| {
                    while let Ok(inbound) = incoming.recv().await {
                        match inbound {
                            Inbound::Message(text) => {
                                if let Some(incoming) = rpc.handle(&text) {
                                    let _ = this.update(cx, |s, cx| s.on_incoming(&id, generation, incoming, cx));
                                }
                            }
                            Inbound::Log(line) => {
                                let _ = this.update(cx, |s, cx| {
                                    if let Some(rt) = s.live(&id, generation) {
                                        rt.log_agent(&line);
                                        cx.notify();
                                    }
                                });
                            }
                            Inbound::Note(line) => {
                                let _ = this.update(cx, |s, cx| {
                                    if let Some(rt) = s.live(&id, generation) {
                                        rt.log(line);
                                        cx.notify();
                                    }
                                });
                            }
                            Inbound::Closed(reason) => {
                                rpc.fail_all(&reason);
                                let _ = this.update(cx, |s, cx| s.fail(&id, generation, reason, cx));
                                break;
                            }
                        }
                    }
                })
            };
            let ok = this
                .update(cx, |s, _| {
                    let Some(rt) = s.live(&id, generation) else { return false };
                    rpc.set_wire_log(rt.wire.clone());
                    rt.rpc = Some(rpc.clone());
                    rt.pid = conn.pid;
                    rt.exit = conn.exit;
                    rt.tasks.push(reader);
                    true
                })
                .unwrap_or(false);
            if !ok {
                rpc.close();
                return;
            }

            let init = rpc
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": 1,
                        "clientInfo": { "name": "QueryMT GPUI", "version": crate::util::APP_VERSION },
                        // `session.compaction`: the agent sends ID-addressed
                        // `compaction_update` / `compaction_summary_chunk` only when advertised.
                        // `_meta.querymt.sessionLoadSnapshot.epoch` (as the desktop app): the
                        // transcript is built from the `session/load` snapshot, so the agent
                        // skips the historical `session/update` replay.
                        // `plan` (the todo list as `plan_update`) and `subagents`
                        // (`subagent_update`, tool-call session links): the session
                        // reducer is tested against exactly these shapes.
                        "clientCapabilities": {
                            "fs": {},
                            "terminal": false,
                            "plan": {},
                            "subagents": {},
                            "session": { "compaction": {} },
                            "elicitation": { "form": {} },
                            "_meta": { "querymt": { "sessionLoadSnapshot": { "epoch": 1 } } },
                        },
                    }),
                )
                .await;
            let init = match init {
                Ok(init) => init,
                Err(err) => {
                    rpc.close();
                    let _ = this.update(cx, |s, cx| s.fail(&id, generation, format!("initialize failed: {err}"), cx));
                    return;
                }
            };
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&id, generation) else { return };
                let info = init.get("agentInfo");
                rt.agent_title = info
                    .and_then(|i| i.get("title").or_else(|| i.get("name")))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                rt.agent_version = info.and_then(|i| i.get("version")).and_then(Value::as_str).map(str::to_owned);
                rt.worktrees = launch::worktree_support(&init);
                rt.peer_profiles.clear();
                rt.can_delete_sessions = init
                    .pointer("/agentCapabilities/sessionCapabilities/delete")
                    .is_some_and(|d| !d.is_null() && d.as_bool() != Some(false));
                rt.can_fork_sessions = init
                    .pointer("/agentCapabilities/sessionCapabilities/fork")
                    .is_some_and(|d| !d.is_null() && d.as_bool() != Some(false));
                rt.state = ConnState::Connected;
                rt.connected_at = Some(now_ms());
                let reconnected = std::mem::take(&mut rt.reconnect_attempt) > 0;
                rt.log(format!(
                    "Initialized {} {}",
                    rt.agent_title.as_deref().unwrap_or("agent"),
                    rt.agent_version.as_deref().unwrap_or("")
                ));
                // The live updates of the open sessions were lost with the
                // old connection: load them again on the new one.
                s.rebind_sessions(&id, reconnected, cx);
                cx.notify();
            });
            load_details(this, id, generation, rpc, cx).await;
        }.instrument(connect_span).await });
        if let Some(rt) = self.runtime.get_mut(&config_id) {
            rt.tasks.push(task);
        }
    }

    pub fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        self.early_commands.clear_agent(id);
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.generation += 1;
            // Actions in flight belonged to the old connection.
            rt.pending.clear();
            rt.reconnect_attempt = 0;
            if let Some(rpc) = rt.rpc.take() {
                rpc.close();
            }
            rt.tasks.clear();
            rt.state = ConnState::Idle;
            rt.pid = None;
            rt.exit = None;
            rt.log("Disconnected.");
            self.retire_requests(id);
            self.rebind_sessions(id, false, cx);
            cx.notify();
        }
    }

    /// Re-fetch capabilities, models, sessions and mesh status of a connected agent.
    pub fn refresh(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone().filter(|_| rt.is_connected()) else {
            return;
        };
        let (generation, id) = (rt.generation, id.to_string());
        rt.busy = true;
        cx.notify();
        let task =
            cx.spawn(async move |this, cx| load_details(this, id, generation, rpc, cx).await);
        rt.tasks.push(task);
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.configs.iter().map(|c| c.id.clone()).collect();
        for id in ids {
            match self.state(&id) {
                ConnState::Connected => self.refresh(&id, cx),
                ConnState::Failed(_) => self.connect(&id, cx),
                _ => {}
            }
        }
    }

    // ---- session launch -----------------------------------------------------

    /// `session/new` → optional model selection → `session/prompt` (not
    /// awaited). The task resolves to the new session, which only the caller
    /// shows; dropping the task cancels the launch (`Task` is `#[must_use]`,
    /// so an ignored launch is a compile warning).
    pub fn launch(
        &mut self,
        request: launch::LaunchRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<OpenSession>, launch::LaunchError>> {
        let launch::LaunchRequest {
            agent_id,
            cwd,
            prompt,
            attachments,
            placement,
            model,
            reasoning,
            mode,
        } = request;
        let agent_id = agent_id.as_str();
        let Some(rpc) = self.runtime.get(agent_id).and_then(|r| r.rpc.clone()) else {
            return Task::ready(Err(launch::LaunchError::NotConnected));
        };
        let model = self.model_to_launch(agent_id, model.as_deref()).cloned();
        // A mesh peer creates the session through the agent and owns it; this
        // machine creates it with `session/new`, in a worktree when chosen.
        let target = placement.target.clone();
        let (method, params) = match &target {
            Some(node) => (
                "_querymt/remote/createSession",
                launch::remote_session_params(node, &cwd, placement.remote_profile.as_deref()),
            ),
            None => (
                "session/new",
                launch::new_session_params(
                    &cwd,
                    self.launch_profile(agent_id),
                    self.worktree_for(agent_id, &placement),
                ),
            ),
        };
        let remote = target.as_ref().map(|node| RemoteOwner {
            node_id: node.clone(),
            label: self
                .launch_targets(agent_id)
                .into_iter()
                .find(|t| t.node_id.as_ref() == Some(node))
                .map(|t| t.label),
            connected: true,
        });
        // A chosen peer profile may have been removed on the peer since it
        // was listed: check right before creating (desktop `launchSession`).
        let profile_check = target.zip(placement.remote_profile).filter(|_| {
            self.runtime
                .get(agent_id)
                .is_some_and(|rt| rt.supports_method("querymt/profiles"))
        });
        let image_mode = self.prefs.image_send_mode;
        let mode = mode.unwrap_or_else(|| self.prefs.mode().to_string());
        let reasoning = reasoning.unwrap_or_else(|| self.prefs.reasoning().to_string());
        self.set_cwd(cwd.clone());
        cx.notify();

        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            use launch::LaunchError;
            async {
                if let Some((node, profile)) = profile_check {
                    let listed = launch::request_peer_profiles(&rpc, &node).await;
                    let _ = this.update(cx, |s, cx| {
                        s.store_peer_profiles(&agent_id, &node, &listed);
                        cx.notify();
                    });
                    if !listed.map_err(LaunchError::Create)?.iter().any(|p| p.id == profile) {
                        return Err(LaunchError::PeerProfileGone);
                    }
                }
                let created = rpc
                    .request(method, params)
                    .await
                    .map_err(LaunchError::Create)?;
                let session_id = created
                    .get("sessionId")
                    .or_else(|| created.get("session_id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| LaunchError::Create("Agent did not return a session id.".into()))?
                    .to_string();
                let skipped = mcp_notice::from_response(&created);
                let _ = this.update(cx, |s, _| {
                    if let Some(rt) = s.runtime.get_mut(&agent_id) {
                        mcp_notice::record(rt, &session_id, skipped, false);
                    }
                });
                // A worktree session runs in its checkout, not the folder.
                let cwd = launch::session_cwd(&created).unwrap_or(cwd);
                let log = |msg: String, cx: &mut AsyncApp| {
                    let agent_id = agent_id.clone();
                    let _ = this.update(cx, |s, _| {
                        if let Some(rt) = s.runtime.get_mut(&agent_id) {
                            rt.log_warn(msg);
                        }
                    });
                };
                // Order matters: mode → model → reasoning (the model can change reasoning choices).
                let mut options = config_options(&created);
                if let Some(list) = created.get("config_options").and_then(Value::as_array) {
                    options = list.clone();
                }
                match find_option(&options, &["mode"]) {
                    Some((id, values)) if values.iter().any(|v| v == &mode) => {
                        let params = json!({ "sessionId": session_id, "configId": id, "value": mode });
                        match rpc.request("session/set_config_option", params).await {
                            Ok(resp) => update_options(&mut options, &resp),
                            Err(err) => log(format!("Could not set mode {mode}: {err}"), cx),
                        }
                    }
                    _ => {
                        let has_mode = created
                            .pointer("/modes/availableModes")
                            .and_then(Value::as_array)
                            .is_some_and(|m| m.iter().any(|x| x.get("id").and_then(Value::as_str) == Some(&mode)));
                        if has_mode {
                            let params = json!({ "sessionId": session_id, "modeId": mode });
                            if let Err(err) = rpc.request("session/set_mode", params).await {
                                log(format!("Could not set mode {mode}: {err}"), cx);
                            }
                        }
                    }
                }
                if let Some(model) = &model {
                    let mut entry = model.raw.clone();
                    if let Some(obj) = entry.as_object_mut() {
                        obj.remove("availability");
                    }
                    let params = json!({
                        "sessionId": session_id, "configId": "model", "value": model.id,
                        "_meta": { "querymt": { "modelEntry": entry } },
                    });
                    match rpc.request("session/set_config_option", params).await {
                        Ok(resp) => update_options(&mut options, &resp),
                        Err(err) => log(format!("Could not select model {}: {err}", model.id), cx),
                    }
                }
                if reasoning != "auto" {
                    let aliases = ["thought_level", "reasoning", "reasoning_effort", "thought"];
                    match find_option(&options, &aliases) {
                        Some((id, values)) if values.iter().any(|v| v == &reasoning) => {
                            let params = json!({ "sessionId": session_id, "configId": id, "value": reasoning });
                            match rpc.request("session/set_config_option", params).await {
                                // The view is built from `options` below, so it must
                                // carry the applied level, not the pre-change `auto`.
                                Ok(resp) => apply_option_value(&mut options, &resp, &id, &reasoning),
                                Err(err) => log(format!("Could not set reasoning {reasoning}: {err}"), cx),
                            }
                        }
                        _ => log(format!("Reasoning effort {reasoning} is not offered for this model."), cx),
                    }
                }
                let first_prompt = prompt_content(&prompt, &attachments, image_mode);
                let session = this.update(cx, |s, cx| {
                    let mut view = SessionView::new(&agent_id, &session_id, &cwd, &prompt.chars().take(80).collect::<String>());
                    // Commands advertised since `session/new` go in first.
                    s.take_early_commands(&mut view);
                    view.apply_modes(created.get("modes"));
                    view.options = options.clone();
                    view.mode = Some(mode.clone());
                    view.model = model.as_ref().map(|m| m.id.clone());
                    view.model_node = model.as_ref().and_then(|m| m.node_id.clone());
                    let session = s.adopt(view, cx);
                    // The prompt resolves when the turn ends; it keeps running in the background.
                    session.update(cx, |o, cx| o.start_prompt(&first_prompt, cx));
                    cx.notify();
                    session
                })
                .map_err(|_| LaunchError::Create("The app closed before the session started.".into()))?;
                let _ = this.update(cx, |s, cx| {
                    if let Some(rt) = s.runtime.get_mut(&agent_id) {
                        rt.sessions.insert(
                            0,
                            SessionSummary {
                                agent_id: agent_id.clone(),
                                id: session_id.clone(),
                                cwd: cwd.clone(),
                                title: prompt.chars().take(80).collect(),
                                phase: "running".into(),
                                root: true,
                                open: true,
                                lifecycle_known: true,
                                messages: 1,
                                remote: remote.clone(),
                                ..Default::default()
                            },
                        );
                    }
                    s.schedule_sessions_refresh(&agent_id, Duration::from_millis(1500), cx);
                });
                Ok(session)
            }
            .await
        })
    }

    // ---- session view -------------------------------------------------------

    /// The live entity of `key`; a dead index entry is pruned.
    fn live_session(&mut self, key: &SessionKey) -> Option<Entity<OpenSession>> {
        match self.open_sessions.hub.get(key)?.upgrade() {
            Some(entity) => Some(entity),
            None => {
                self.open_sessions.hub.remove(key);
                self.open_sessions.subs.remove(key);
                None
            }
        }
    }

    /// Some open session is waiting for its load response.
    fn any_session_loading(&self, cx: &App) -> bool {
        self.open_sessions
            .hub
            .values()
            .filter_map(WeakEntity::upgrade)
            .any(|session| session.read(cx).view.loading)
    }

    /// What `OpenSession`s of `agent_id` get to talk to the agent with.
    fn link_for(&self, agent_id: &str) -> Link {
        let rt = self.runtime.get(agent_id);
        let has = |method: &str| rt.is_some_and(|rt| rt.supports_method(method));
        let config = self.configs.iter().find(|c| c.id == agent_id);
        let agent_line = rt
            .map(|rt| {
                format!(
                    "{} ({}) · {} {}",
                    config.map_or(agent_id, |c| c.name.as_str()),
                    config.map(AgentConfig::endpoint_label).unwrap_or_default(),
                    rt.agent_title.as_deref().unwrap_or("unknown agent"),
                    rt.agent_version.as_deref().unwrap_or(""),
                )
            })
            .unwrap_or_default();
        let windowed = rt
            .and_then(|rt| rt.capabilities.as_ref())
            .and_then(|c| c.pointer("/features/session_history_window"))
            .and_then(Value::as_bool)
            == Some(true);
        Link {
            rpc: rt.and_then(|rt| rt.rpc.clone()),
            connected: rt.is_some_and(AgentRuntime::is_connected),
            generation: rt.map_or(0, |rt| rt.generation),
            caps: SessionCaps {
                can_control_run: rt.is_some_and(AgentRuntime::can_control_run),
                tool_content: has("querymt/session/toolContent"),
                undo: [
                    "querymt/session/undoStack",
                    "querymt/session/undo",
                    "querymt/session/redo",
                ]
                .iter()
                .all(|m| has(m)),
                discard_queued: has("querymt/session/discardQueuedInput"),
                history_window: windowed,
            },
            agent_line,
        }
    }

    /// Give every live session of `agent_id` the agent's current link, and
    /// load each again when `reload` (their live updates were lost with the
    /// old connection).
    pub(crate) fn rebind_sessions(&mut self, agent_id: &str, reload: bool, cx: &mut Context<Self>) {
        let link = self.link_for(agent_id);
        let live: Vec<Entity<OpenSession>> = self
            .open_sessions
            .hub
            .values()
            .filter_map(WeakEntity::upgrade)
            .filter(|session| session.read(cx).key.agent.as_str() == agent_id)
            .collect();
        for session in live {
            let link = link.clone();
            session.update(cx, |o, cx| {
                o.rebind(link, cx);
                if reload {
                    o.reload(cx);
                }
            });
        }
    }

    /// Read the open session `key`, if it is open.
    pub(crate) fn with_session<R>(
        &self,
        key: &SessionKey,
        cx: &App,
        read: impl FnOnce(&SessionView) -> R,
    ) -> Option<R> {
        let session = self.open_sessions.hub.get(key)?.upgrade()?;
        Some(read(&session.read(cx).view))
    }

    /// Entity for `view`, indexed weakly and heard by the store.
    fn adopt(&mut self, view: SessionView, cx: &mut Context<Self>) -> Entity<OpenSession> {
        let key = SessionKey::new(&view.agent_id, &view.session_id);
        let link = self.link_for(key.agent.as_str());
        let session = cx.new(|_| OpenSession::new(view, link));
        self.open_sessions
            .hub
            .insert(key.clone(), session.downgrade());
        let subscription = cx.subscribe(&session, |this, session, event: &OpenSessionEvent, cx| {
            this.on_session_event(&session, event, cx)
        });
        self.open_sessions.subs.insert(key, subscription);
        session
    }

    /// The session for `target`: the one that is already open, or a new one
    /// loaded from the agent. The caller holds the returned handle for as long
    /// as it shows the session.
    #[must_use]
    pub(crate) fn open(
        &mut self,
        target: SessionRef,
        cx: &mut Context<Self>,
    ) -> Entity<OpenSession> {
        let SessionRef { key, cwd, title } = target;
        if let Some(existing) = self.live_session(&key) {
            // Shared, not loaded again, unless its last load failed.
            if existing.read(cx).view.error.is_some() {
                let link = self.link_for(key.agent.as_str());
                existing.update(cx, |o, cx| {
                    o.rebind(link, cx);
                    o.reload(cx);
                });
            }
            return existing;
        }
        let (agent_id, session_id) = (key.agent.as_str(), key.session.as_str());
        self.remember_recent_session(agent_id, session_id);
        let mut view = SessionView::new(agent_id, session_id, &cwd, &title);
        // Before the load: a later load or update applies on top of them.
        self.take_early_commands(&mut view);
        if let Some(s) = self
            .runtime
            .get(agent_id)
            .and_then(|rt| rt.sessions.iter().find(|s| s.id == session_id))
        {
            view.phase = s.phase.clone();
        }
        if self
            .runtime
            .get(agent_id)
            .is_some_and(AgentRuntime::is_connected)
        {
            crate::telemetry::mark_session_open(session_id);
        }
        let session = self.adopt(view, cx);
        session.update(cx, |o, cx| o.load(cx));
        cx.notify();
        session
    }

    /// What an open session tells the store: its turn ended, its load ended,
    /// or the user stopped it.
    fn on_session_event(
        &mut self,
        session: &Entity<OpenSession>,
        event: &OpenSessionEvent,
        cx: &mut Context<Self>,
    ) {
        let key = session.read(cx).key.clone();
        let (agent_id, session_id) = (key.agent.as_str(), key.session.as_str());
        match event {
            OpenSessionEvent::TurnFinished { error } => {
                if let (Some(err), Some(rt)) = (error, self.runtime.get_mut(agent_id)) {
                    rt.log_error(format!("Prompt failed: {err}"));
                }
                self.schedule_sessions_refresh(agent_id, Duration::from_millis(300), cx);
            }
            OpenSessionEvent::Loaded { skipped_mcp } => {
                if let (Some(servers), Some(rt)) = (skipped_mcp, self.runtime.get_mut(agent_id)) {
                    mcp_notice::record(rt, session_id, servers.clone(), false);
                }
                self.refresh_delegates(&key, cx);
            }
            OpenSessionEvent::Cancelled => self.cancel_session_requests(&key, cx),
        }
        cx.notify();
    }

    /// The model of `view`, with a node only when it is not the agent's own
    /// machine.
    pub fn session_model_key(&self, view: &SessionView) -> Option<String> {
        let own = self
            .runtime(&view.agent_id)
            .and_then(|rt| rt.mesh.as_ref())
            .and_then(|m| m.peer_id.as_deref());
        let node = view.model_node.as_deref().filter(|n| Some(*n) != own);
        Some(model_key(view.model.as_deref()?, node))
    }

    /// The model of `view` in its agent's catalog.
    pub fn session_model(&self, view: &SessionView) -> Option<&Model> {
        find_model(self.models(&view.agent_id), &self.session_model_key(view)?)
    }

    /// Revert the workspace of `session` to before the user turn `message_id`
    /// (and every later turn), then reload the transcript.
    pub fn undo_workspace_to(
        &mut self,
        session: &Entity<OpenSession>,
        message_id: String,
        cx: &mut Context<Self>,
    ) {
        let blocked = self.fork_pending_for(session.read(cx).key.agent.as_str());
        session.update(cx, |o, cx| o.undo_to(message_id, blocked, cx));
    }

    /// Re-apply the most recently undone turn (`querymt/session/redo`).
    pub fn redo_workspace(&mut self, session: &Entity<OpenSession>, cx: &mut Context<Self>) {
        let blocked = self.fork_pending_for(session.read(cx).key.agent.as_str());
        session.update(cx, |o, cx| o.redo(blocked, cx));
    }

    /// Send a prompt to `session`. While a turn runs on an agent with turn
    /// control, `delivery` (steer or queue) picks the QueryMT extension.
    pub fn send_prompt(
        &mut self,
        session: &Entity<OpenSession>,
        text: String,
        attachments: Vec<Attachment>,
        delivery: Delivery,
        cx: &mut Context<Self>,
    ) {
        let image_mode = self.prefs.image_send_mode;
        session.update(cx, |o, cx| {
            o.send(text, attachments, delivery, image_mode, cx)
        });
    }

    /// Change a config option of `session` (`session/set_config_option`);
    /// an empty `config_id` means the mode of an agent that only has
    /// `session/set_mode`. For `model`, `value` is a selection key
    /// ([`model_key`]), so the model on the chosen machine is sent.
    pub fn set_session_option(
        &mut self,
        session: &Entity<OpenSession>,
        config_id: &str,
        value: &str,
        cx: &mut Context<Self>,
    ) {
        let agent_id = session.read(cx).key.agent.to_string();
        if session.read(cx).link().rpc.is_none() {
            return;
        }
        let model = (config_id == "model")
            .then(|| {
                self.runtime
                    .get(&agent_id)
                    .and_then(|rt| find_model(&rt.models, value))
                    .cloned()
            })
            .flatten();
        if let Some(model) = &model {
            self.remember_model(&agent_id, &model.key());
            self.save();
        }
        session.update(cx, |o, cx| o.set_option(config_id, value, model, cx));
    }

    /// Resolve / archive flags (`_querymt/session/setLifecycle`), optimistically.
    pub fn set_lifecycle(
        &mut self,
        agent_id: &str,
        session_id: &str,
        resolved: Option<bool>,
        archived: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        let Some(rt) = self.runtime.get_mut(agent_id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone() else { return };
        // Previous state of every loaded copy, restored if the agent refuses.
        let mut previous = None;
        for s in rt.sessions_mut(session_id) {
            previous.get_or_insert((s.resolved, s.archived, s.lifecycle_known));
            if let Some(r) = resolved {
                s.resolved = r;
            }
            if let Some(a) = archived {
                s.archived = a;
            }
            s.open = !s.resolved && !s.archived;
            s.lifecycle_known = true;
        }
        rt.begin_session_action(session_id);
        cx.notify();
        let mut params = json!({ "session_id": session_id });
        if let Some(r) = resolved {
            params["resolved"] = json!(r);
        }
        if let Some(a) = archived {
            params["archived"] = json!(a);
        }
        let (agent_id, session_id) = (agent_id.to_string(), session_id.to_string());
        cx.spawn(async move |this, cx| {
            let result = rpc.request("_querymt/session/setLifecycle", params).await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.runtime.get_mut(&agent_id) else {
                    return;
                };
                match result {
                    Err(err) => {
                        if let Some((r, a, known)) = previous {
                            for sess in rt.sessions_mut(&session_id) {
                                (sess.resolved, sess.archived, sess.lifecycle_known) =
                                    (r, a, known);
                                sess.open = !r && !a;
                            }
                        }
                        rt.end_session_action(
                            &session_id,
                            Some(format!("Could not update the session: {err}")),
                        );
                    }
                    Ok(_) => {
                        rt.end_session_action(&session_id, None);
                        s.schedule_sessions_refresh(&agent_id, Duration::from_millis(200), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Pending prompts of a stopped session are moot.
    fn cancel_session_requests(&mut self, session: &SessionKey, cx: &mut Context<Self>) {
        let keys: Vec<(String, bool)> = self
            .requests
            .iter()
            .filter(|r| {
                r.answerable()
                    && r.agent_id == session.agent.as_str()
                    && r.session_id.as_deref() == Some(session.session.as_str())
            })
            .map(|r| (r.key.clone(), r.is_elicitation()))
            .collect();
        for (key, elicitation) in keys {
            if elicitation {
                self.answer_elicitation(&key, "cancel", cx);
            } else {
                self.resolve_request(
                    &key,
                    json!({ "outcome": { "outcome": "cancelled" } }),
                    "Cancelled".into(),
                    false,
                    cx,
                );
            }
        }
        cx.notify();
    }

    // ---- inbox --------------------------------------------------------------

    /// Answer the live delivery of `key` and keep the card as resolved.
    fn resolve_request(
        &mut self,
        key: &str,
        result: Value,
        resolution: String,
        expect_ack: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(req) = self
            .requests
            .iter_mut()
            .find(|r| r.key == key && r.answerable())
        else {
            return;
        };
        let rpc_id = req.rpc_id.take();
        req.status = RequestStatus::Resolved(resolution);
        req.error = None;
        req.rejected = false;
        // Only a stable identity can be re-delivered to this card if the
        // agent rejects the answer.
        req.awaiting_ack = expect_ack && req.stable;
        let agent = req.agent_id.clone();
        if let (Some(id), Some(rpc)) =
            (rpc_id, self.runtime.get(&agent).and_then(|r| r.rpc.clone()))
        {
            rpc.respond(id, Ok(result));
        }
        self.trim_resolved();
        cx.notify();
    }

    /// Keep at most [`MAX_RESOLVED_REQUESTS`] answered cards (newest first).
    fn trim_resolved(&mut self) {
        let mut resolved: Vec<(f64, String)> = self
            .requests
            .iter()
            .filter(|r| !r.is_pending() && !r.awaiting_ack)
            .map(|r| (r.received_ms, r.key.clone()))
            .collect();
        if resolved.len() <= MAX_RESOLVED_REQUESTS {
            return;
        }
        resolved.sort_by(|a, b| b.0.total_cmp(&a.0));
        let drop: std::collections::HashSet<String> = resolved
            .into_iter()
            .skip(MAX_RESOLVED_REQUESTS)
            .map(|(_, k)| k)
            .collect();
        self.requests.retain(|r| !drop.contains(&r.key));
    }

    /// Requests still waiting for the user (the Inbox badge).
    pub fn pending_request_count(&self) -> usize {
        self.requests.iter().filter(|r| r.is_pending()).count()
    }

    /// A request from the agent: a new card, or the re-delivery of one already
    /// shown (reconnect, rejected answer), which keeps its card and draft.
    fn receive_request(&mut self, mut request: PendingRequest, cx: &mut Context<Self>) {
        let rpc = self
            .runtime
            .get(&request.agent_id)
            .and_then(|r| r.rpc.clone());
        let Some(ix) = self.requests.iter().position(|r| r.key == request.key) else {
            self.requests.push(request);
            cx.notify();
            return;
        };
        let existing = &mut self.requests[ix];
        if existing.answerable() {
            if existing.is_elicitation() {
                // The question is already answerable here: dismiss the duplicate.
                if let (Some(rpc), Some(id)) = (rpc, request.rpc_id.take()) {
                    rpc.respond(id, Ok(json!({ "action": "cancel" })));
                }
                return;
            }
            // A newer delivery of the same permission supersedes the old one.
            if let (Some(rpc), Some(id)) = (rpc, existing.rpc_id.take()) {
                rpc.respond(id, Ok(json!({ "outcome": { "outcome": "cancelled" } })));
            }
            existing.rpc_id = request.rpc_id.take();
        } else if existing.is_pending() || existing.awaiting_ack || existing.rejected {
            if !existing.is_pending() || (existing.awaiting_ack && !existing.rejected) {
                existing.error = Some("The agent rejected this answer.".into());
            } else if !existing.rejected {
                existing.error = None;
            }
            existing.status = RequestStatus::Pending;
            existing.rpc_id = request.rpc_id.take();
            existing.offline = false;
            existing.awaiting_ack = false;
            existing.rejected = false;
            if existing.drafts.len() != request.drafts.len() {
                existing.kind = request.kind;
                existing.drafts = request.drafts;
            }
        } else {
            // Answered for good earlier: this is a new question with the same id.
            self.requests.remove(ix);
            self.requests.push(request);
        }
        cx.notify();
    }

    /// The agent's connection is gone: no answer is sent for any card. Cards
    /// with a stable identity wait offline for a re-delivery; others retire.
    fn retire_requests(&mut self, agent_id: &str) {
        for r in self.requests.iter_mut().filter(|r| r.agent_id == agent_id) {
            r.awaiting_ack = false;
            if !r.is_pending() {
                continue;
            }
            r.rpc_id = None;
            if r.stable {
                r.offline = true;
            } else {
                r.status = RequestStatus::Resolved(RESOLVED_GONE.into());
            }
        }
    }

    /// Retire question cards of `session_id` that the agent no longer reports
    /// as pending. Only cards that existed before recovery began (`boundary`).
    fn reconcile_elicitations(
        &mut self,
        agent_id: &str,
        session_id: &str,
        pending: &[String],
        boundary: &std::collections::HashSet<String>,
    ) {
        for r in self.requests.iter_mut().filter(|r| {
            r.agent_id == agent_id
                && r.session_id.as_deref() == Some(session_id)
                && r.is_pending()
                && boundary.contains(&r.key)
        }) {
            let Some(identity) = r.identity.as_deref() else {
                continue;
            };
            if !pending.iter().any(|p| p == identity) {
                r.rpc_id = None;
                r.offline = false;
                r.status = RequestStatus::Resolved(RESOLVED_GONE.into());
            }
        }
    }

    /// `querymt/elicitation/*` notifications (and ACP `elicitation/complete`).
    fn on_elicitation_notification(&mut self, agent_id: &str, method: &str, params: &Value) {
        let method = method.trim_start_matches('_');
        if method == "querymt/elicitation/recoveryAuthority" {
            if params.get("version").and_then(Value::as_u64) == Some(ELICITATION_RECOVERY_VERSION)
                && let (Some(sid), Some(authority)) = (
                    str_at(params, "session_id").filter(|s| !s.is_empty()),
                    str_at(params, "resume_authority").filter(|s| !s.is_empty()),
                )
                && let Some(rt) = self.runtime.get_mut(agent_id)
            {
                rt.resume_authorities
                    .insert(sid.to_string(), authority.to_string());
            }
            return;
        }
        let Some(eid) = str_at(params, "elicitation_id")
            .or_else(|| str_at(params, "elicitationId"))
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let Some(req) = self
            .requests
            .iter_mut()
            .find(|r| r.agent_id == agent_id && r.identity.as_deref() == Some(eid))
        else {
            return;
        };
        match method {
            // The agent rejected the submitted answer and keeps the question
            // pending; it re-delivers it, which makes the card answerable again.
            "querymt/elicitation/validationFailed" if req.awaiting_ack => {
                req.status = RequestStatus::Pending;
                req.rpc_id = None;
                req.offline = true;
                req.awaiting_ack = false;
                req.rejected = true;
                req.error = Some(match str_at(params, "message").filter(|m| !m.is_empty()) {
                    Some(m) => format!("The agent rejected this answer: {m}"),
                    None => "The agent rejected this answer.".into(),
                });
            }
            "querymt/elicitation/completed" | "elicitation/complete" => {
                req.awaiting_ack = false;
                if req.is_pending() {
                    let outcome = match str_at(params, "outcome") {
                        Some("accept") => RESOLVED_SUBMITTED,
                        Some("decline") => "Declined",
                        Some("cancel") => "Cancelled",
                        _ if method == "elicitation/complete" => "Completed",
                        _ => RESOLVED_GONE,
                    };
                    req.rpc_id = None;
                    req.offline = false;
                    req.error = None;
                    req.status = RequestStatus::Resolved(outcome.into());
                }
                self.trim_resolved();
            }
            _ => {}
        }
    }

    /// Replace the user's answer for one field of a pending question.
    pub fn set_draft(
        &mut self,
        key: &str,
        field: usize,
        draft: FieldDraft,
        cx: &mut Context<Self>,
    ) {
        if let Some(req) = self
            .requests
            .iter_mut()
            .find(|r| r.key == key && r.is_pending())
            && let Some(slot) = req.drafts.get_mut(field)
        {
            *slot = draft;
            req.error = None;
            cx.notify();
        }
    }

    pub fn answer_permission(&mut self, key: &str, option_id: &str, cx: &mut Context<Self>) {
        let Some(resolution) =
            self.requests
                .iter()
                .find(|r| r.key == key)
                .and_then(|r| match &r.kind {
                    RequestKind::Permission { options, .. } => options
                        .iter()
                        .find(|o| o.id == option_id)
                        .map(permission_resolution),
                    RequestKind::Elicitation { .. } => None,
                })
        else {
            return;
        };
        self.resolve_request(
            key,
            json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
            resolution,
            false,
            cx,
        );
    }

    /// `action`: `accept` (with `content`), `decline` or `cancel`.
    pub fn answer_elicitation(&mut self, key: &str, action: &str, cx: &mut Context<Self>) {
        let Some(req) = self
            .requests
            .iter_mut()
            .find(|r| r.key == key && r.answerable())
        else {
            return;
        };
        let (result, resolution) = match action {
            "decline" => (json!({ "action": "decline" }), "Declined"),
            "cancel" => (json!({ "action": "cancel" }), "Cancelled"),
            "accept" => {
                let content = match &req.kind {
                    RequestKind::Elicitation { url: Some(_), .. } => Ok(None),
                    // Never submit a form whose fields cannot be represented.
                    RequestKind::Elicitation {
                        unsupported: true, ..
                    }
                    | RequestKind::Permission { .. } => return,
                    RequestKind::Elicitation { fields, .. } => {
                        elicitation_content(fields, &req.drafts).map(Some)
                    }
                };
                match content {
                    Ok(content) => {
                        let mut result = json!({ "action": "accept" });
                        if let Some(content) = content {
                            result["content"] = content;
                        }
                        (result, RESOLVED_SUBMITTED)
                    }
                    Err(message) => {
                        req.error = Some(message);
                        cx.notify();
                        return;
                    }
                }
            }
            _ => return,
        };
        self.resolve_request(key, result, resolution.into(), action == "accept", cx);
    }

    /// The listed session `session_id`: in the root catalog, else in an
    /// expanded parent's children (worktree children and forks only show
    /// up there).
    pub fn session_summary(&self, agent_id: &str, session_id: &str) -> Option<&SessionSummary> {
        let rt = self.runtime.get(agent_id)?;
        rt.sessions
            .iter()
            .chain(rt.children.values().flat_map(|c| c.sessions.iter()))
            .find(|s| s.id == session_id)
    }

    // ---- internals ----------------------------------------------------------

    /// Runtime of `id` if `generation` is still the current connection.
    fn live(&mut self, id: &str, generation: u64) -> Option<&mut AgentRuntime> {
        self.runtime
            .get_mut(id)
            .filter(|rt| rt.generation == generation)
    }

    fn fail(&mut self, id: &str, generation: u64, reason: String, cx: &mut Context<Self>) {
        let Some(rt) = self.live(id, generation) else {
            return;
        };
        if let Some(rpc) = rt.rpc.take() {
            rpc.close();
        }
        rt.log_error(format!("Error: {reason}"));
        rt.state = ConnState::Failed(reason);
        rt.busy = false;
        self.retire_requests(id);
        self.rebind_sessions(id, false, cx);
        self.schedule_reconnect(id, generation, cx);
        cx.notify();
    }

    /// Like the web client: a WebSocket agent that is set to start
    /// automatically retries after a dropped or failed connection, backing
    /// off from 250 ms to 8 s. A manual connect or disconnect bumps the
    /// generation, which cancels the pending retry.
    fn schedule_reconnect(&mut self, id: &str, generation: u64, cx: &mut Context<Self>) {
        let Some(config) = self.config(id) else {
            return;
        };
        if !config.is_websocket() || !config.auto_start {
            return;
        }
        let Some(rt) = self.live(id, generation) else {
            return;
        };
        let delay = reconnect_delay(rt.reconnect_attempt);
        rt.reconnect_attempt += 1;
        rt.log_warn(format!(
            "Reconnecting in {:.1}s (attempt {})…",
            delay.as_secs_f32(),
            rt.reconnect_attempt
        ));
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |s, cx| {
                let waiting = s
                    .live(&id, generation)
                    .is_some_and(|rt| matches!(rt.state, ConnState::Failed(_)));
                if waiting {
                    s.connect(&id, cx);
                }
            });
        })
        .detach();
    }

    fn on_incoming(
        &mut self,
        id: &str,
        generation: u64,
        incoming: Incoming,
        cx: &mut Context<Self>,
    ) {
        match incoming {
            Incoming::Request {
                id: request_id,
                method,
                params,
            } => {
                let Some(rpc) = self.live(id, generation).and_then(|rt| rt.rpc.clone()) else {
                    return;
                };
                match parse_request(id, request_id.clone(), &method, &params) {
                    Some(request) => self.receive_request(request, cx),
                    None => rpc.respond(
                        request_id,
                        Err((
                            -32601,
                            format!("Method not supported by this client: {method}"),
                        )),
                    ),
                }
            }
            Incoming::Notification { method, params } => {
                // The open session an input is addressed to; inputs for a
                // session no surface holds change no transcript.
                let key = SessionKey::from_update(id, &params);
                let target = key.as_ref().and_then(|k| self.live_session(k));
                if method == "session/update" {
                    match &target {
                        Some(session) => session.update(cx, |o, cx| {
                            o.view.queue_session_update(&params);
                            o.schedule_flush(cx);
                        }),
                        // No surface holds it yet (e.g. `launch` before its
                        // view exists): keep what it advertises.
                        None => self.note_early_commands(id, &params),
                    }
                }
                // Run state of any run, including ones other clients started.
                // Child links arrive as `subagent_update`s through the core.
                if method
                    .trim_start_matches('_')
                    .ends_with("session/stateUpdate")
                    && let Some(session) = &target
                {
                    session.update(cx, |o, cx| {
                        o.view.queue_state_update(&params);
                        o.schedule_flush(cx);
                    });
                }
                if method.ends_with("session/runtimeChanged") {
                    let sid = params
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let phase = params
                        .pointer("/runtime/phase")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if let Some(session) = &target {
                        session.update(cx, |o, cx| {
                            if let Some(rt) = params.get("runtime") {
                                o.view.queue_runtime(rt);
                            }
                            o.schedule_flush(cx);
                        });
                    }
                    if let Some(s) = self
                        .runtime
                        .get_mut(id)
                        .and_then(|rt| rt.sessions.iter_mut().find(|s| s.id == sid))
                    {
                        s.phase = phase;
                    }
                    cx.notify();
                }
                if method.ends_with("session/inputState")
                    && let Some(session) = &target
                {
                    let input_id = params
                        .get("input_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let delivery = params
                        .get("delivery")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let state = params
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    // The core lists the input for any client; an input
                    // sent from here waits for its submit response to
                    // learn its agent id, so an early state is kept.
                    session.update(cx, |o, cx| {
                        o.view.queue_input_state(&params);
                        o.view.note_input_state(&input_id, &delivery, &state);
                        o.schedule_flush(cx);
                    });
                }
                if method
                    .trim_start_matches('_')
                    .starts_with("querymt/elicitation/")
                    || method == "elicitation/complete"
                {
                    self.on_elicitation_notification(id, &method, &params);
                    cx.notify();
                }
                // Plans of listed sessions are patched in place; no re-list.
                if method.trim_start_matches('_') == "querymt/session/plansChanged" {
                    self.on_plans_changed(id, &params, cx);
                }
                if method.contains("runtimeChanged")
                    || method.contains("lifecycle")
                    || method.ends_with("session/update")
                {
                    self.schedule_sessions_refresh(id, Duration::from_millis(1200), cx);
                } else if method.trim_start_matches('_') == "querymt/session/pinsChanged" {
                    self.on_pins_changed(id, &params, cx);
                } else if mesh_live::is_membership_change(&method) {
                    self.on_mesh_membership_changed(id, cx);
                } else if method.trim_start_matches('_') == "querymt/session/mcpAuthRequired" {
                    self.on_mcp_auth_required(id, &params, cx);
                } else if plugins::is_plugin_update(&method) {
                    self.on_plugin_update(id, &method, &params, cx);
                } else if method.trim_start_matches('_') == "querymt/schedules/changed" {
                    if self
                        .live(id, generation)
                        .is_some_and(|rt| rt.schedules.loaded || rt.schedules.loading)
                    {
                        self.refresh_schedules(id, cx);
                    }
                } else if method.ends_with("session/delegateModelsChanged") {
                    if let Some(key) = key.as_ref().filter(|_| target.is_some()) {
                        self.refresh_delegates(key, cx);
                    }
                } else if method.contains("models/changed") || method.contains("nodesChanged") {
                    self.refresh(id, cx);
                }
            }
        }
    }

    /// Debounced `session/list` refresh after runtime notifications.
    fn schedule_sessions_refresh(&mut self, id: &str, delay: Duration, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone() else { return };
        if rt.refresh_scheduled {
            return;
        }
        rt.refresh_scheduled = true;
        let (generation, id) = (rt.generation, id.to_string());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // The first page holds everything that changed recently; merge it in.
            let remote_nodes = this
                .read_with(cx, |s, _| s.prefs.remote_node_ids())
                .unwrap_or_default();
            let sessions = fetch_session_page(&rpc, &id, Some(&root_scope(&remote_nodes)), None)
                .await
                .map(|(page, _)| page);
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&id, generation) else {
                    return;
                };
                rt.refresh_scheduled = false;
                match sessions {
                    Ok(fresh) => {
                        merge_sessions(rt, fresh);
                        rt.sessions_loaded = true;
                        rt.sessions_error = None;
                    }
                    Err(err) => rt.sessions_error = Some(err),
                }
                cx.notify();
            });
        });
        rt.tasks.push(task);
        rt.tasks.retain(|t| !t.is_ready());
    }
}

/// Safety cap on `session/list` pages for the full catalog (250 per page):
/// the whole history is loaded so tab counts and search are complete.
const CATALOG_PAGES: usize = 200;
/// Children pages of one parent (10 per page).
const CHILD_PAGES: usize = 50;

/// `session/list` scope of the catalog: root sessions only, as the desktop
/// app lists (`DEFAULT_SESSION_LIST_SCOPE`). Forks load per parent, and
/// delegates are never listed; unscoped, they were ~3/4 of every page.
fn root_scope(remote_nodes: &[String]) -> Value {
    let mut meta = json!({ "session_scope": "root" });
    // Mesh peers whose sessions are listed too (desktop `remoteNodeIds`).
    if !remote_nodes.is_empty() {
        meta["remoteNodeIds"] = json!(remote_nodes);
    }
    meta
}

/// Merge listed sessions into `rt`, newest first; listed entries replace
/// the known ones of the same id.
fn merge_sessions(rt: &mut AgentRuntime, listed: Vec<SessionSummary>) {
    let ids: std::collections::HashSet<String> = listed.iter().map(|s| s.id.clone()).collect();
    let mut merged = listed;
    merged.extend(rt.sessions.drain(..).filter(|s| !ids.contains(&s.id)));
    merged.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    rt.sessions = merged;
}

/// Follow `session/list` cursors up to `max_pages`, with optional `_meta`.
async fn fetch_session_pages(
    rpc: &RpcClient,
    agent_id: &str,
    meta: Option<Value>,
    max_pages: usize,
) -> Result<Vec<SessionSummary>, String> {
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..max_pages {
        let (page, next) =
            fetch_session_page(rpc, agent_id, meta.as_ref(), cursor.as_deref()).await?;
        sessions.extend(page);
        // A repeated cursor would loop forever; stop instead.
        if next.is_none() || next == cursor {
            break;
        }
        cursor = next;
    }
    Ok(sessions)
}

/// One `session/list` page and the cursor of the next, if any.
async fn fetch_session_page(
    rpc: &RpcClient,
    agent_id: &str,
    meta: Option<&Value>,
    cursor: Option<&str>,
) -> Result<(Vec<SessionSummary>, Option<String>), String> {
    let mut params = match cursor {
        Some(c) => json!({ "cursor": c }),
        None => json!({}),
    };
    if let Some(meta) = meta {
        params["_meta"] = meta.clone();
    }
    let page = rpc
        .request("session/list", params)
        .instrument(tracing::info_span!(
            "sessions.page",
            page.cursor = cursor.is_some()
        ))
        .await?;
    let sessions = page
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| SessionSummary::from_list_entry(agent_id, s))
        .filter(|s| !s.id.is_empty())
        .collect();
    let next = page
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    Ok((sessions, next))
}

/// After a (re)connect, re-attach the questions still pending for sessions
/// whose resume authority this app holds (desktop `recoverPendingElicitations`).
/// The agent re-delivers them, which rebinds the offline cards; cards it no
/// longer reports are retired without sending any answer.
async fn recover_elicitations(
    this: &WeakEntity<AgentsStore>,
    id: &str,
    generation: u64,
    rpc: &RpcClient,
    cx: &mut AsyncApp,
) {
    let Ok(Some((authorities, boundary))) = this.update(cx, |s, _| {
        let endpoint_ok = s
            .config(id)
            .is_some_and(|c| recovery_transport_ok(&c.endpoint));
        let rt = s.runtime.get(id).filter(|rt| rt.generation == generation)?;
        if !endpoint_ok || rt.resume_authorities.is_empty() {
            return None;
        }
        let boundary: std::collections::HashSet<String> = s
            .requests
            .iter()
            .filter(|r| r.agent_id == id)
            .map(|r| r.key.clone())
            .collect();
        Some((rt.resume_authorities.clone(), boundary))
    }) else {
        return;
    };
    let version = ELICITATION_RECOVERY_VERSION;
    for (session_id, authority) in authorities {
        let listed = rpc
            .request_detailed(
                "_querymt/elicitation/listPendingSessions",
                json!({ "version": version, "resume_authority": authority }),
            )
            .await;
        let session_ids: Vec<String> = match listed {
            Ok(v) => v
                .get("session_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect(),
            Err(err) => {
                let denied = err
                    .raw()
                    .and_then(|r| r.get("code"))
                    .and_then(Value::as_i64)
                    == Some(ELICITATION_RECOVERY_DENIED);
                if denied {
                    // Expired or revoked: forget it; its questions are gone.
                    let _ = this.update(cx, |s, cx| {
                        if let Some(rt) = s.live(id, generation) {
                            rt.resume_authorities.remove(&session_id);
                        }
                        s.reconcile_elicitations(id, &session_id, &[], &boundary);
                        cx.notify();
                    });
                }
                continue;
            }
        };
        for attached in &session_ids {
            let Ok(v) = rpc
                .request(
                    "_querymt/elicitation/attachSession",
                    json!({
                        "version": version,
                        "session_id": attached,
                        "resume_authority": authority,
                    }),
                )
                .await
            else {
                // Keep offline cards for a later reconnect.
                continue;
            };
            let pending: Vec<String> = v
                .get("elicitation_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect();
            let _ = this.update(cx, |s, cx| {
                s.reconcile_elicitations(id, attached, &pending, &boundary);
                cx.notify();
            });
        }
        if !session_ids.contains(&session_id) {
            let _ = this.update(cx, |s, cx| {
                s.reconcile_elicitations(id, &session_id, &[], &boundary);
                cx.notify();
            });
        }
    }
}

/// Capabilities, models, sessions, mesh — each applied as soon as it arrives.
async fn load_details(
    this: WeakEntity<AgentsStore>,
    id: String,
    generation: u64,
    rpc: Rc<RpcClient>,
    cx: &mut AsyncApp,
) {
    macro_rules! apply {
        ($f:expr) => {
            if this
                .update(cx, |s, cx| {
                    if let Some(rt) = s.live(&id, generation) {
                        #[allow(clippy::redundant_closure_call)]
                        ($f)(rt);
                        cx.notify();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
                == false
            {
                return;
            }
        };
    }

    let caps = rpc
        .request("_querymt/capabilities", json!({}))
        .instrument(tracing::info_span!("agent.capabilities"))
        .await;
    let has_mesh = caps
        .as_ref()
        .ok()
        .and_then(|c| c.pointer("/features/mesh"))
        .and_then(Value::as_bool)
        == Some(true);
    let recovery = caps.as_ref().is_ok_and(supports_elicitation_recovery);
    apply!(|rt: &mut AgentRuntime| match caps {
        Ok(caps) => rt.capabilities = Some(caps),
        Err(err) => rt.log_warn(format!("Capabilities unavailable: {err}")),
    });
    // What the agent can do is known now: open sessions use it.
    let _ = this.update(cx, |s, cx| s.rebind_sessions(&id, false, cx));
    if recovery {
        recover_elicitations(&this, &id, generation, &rpc, cx)
            .instrument(tracing::info_span!("agent.recover_elicitations"))
            .await;
    }

    let models = rpc
        .request("_querymt/models", json!({}))
        .instrument(tracing::info_span!("agent.models"))
        .await;
    let parsed = match &models {
        Ok(v) => Some(parse_models(v)),
        Err(_) => None,
    };
    apply!(|rt: &mut AgentRuntime| {
        rt.models_loaded = true;
        match (&models, parsed.clone()) {
            (_, Some(models)) => rt.models = models,
            (Err(err), None) => rt.log_warn(format!("Models unavailable: {err}")),
            _ => {}
        }
    });
    // Capabilities (context window, modalities) come from a second call; apply
    // them once they arrive so the list is usable immediately.
    if let Some(models) = parsed.filter(|m| !m.is_empty()) {
        let models = hydrate_model_info(&rpc, models)
            .instrument(tracing::info_span!("agent.model_info"))
            .await;
        apply!(|rt: &mut AgentRuntime| rt.models = models);
    }

    // The whole catalog, so lifecycle tab counts and search cover every session.
    let catalog_span = tracing::info_span!(
        "sessions.catalog",
        sessions.count = tracing::field::Empty,
        sessions.apply_ms = tracing::field::Empty,
    );
    // The first page (most recent sessions) is shown at once: the sidebar,
    // the Sessions page and deep links need only it. Paging the whole catalog
    // took ~15s for ~6k sessions and blocked all of them until it finished.
    let remote_nodes = this
        .read_with(cx, |s, _| s.prefs.remote_node_ids())
        .unwrap_or_default();
    let scope = root_scope(&remote_nodes);
    let first = fetch_session_page(&rpc, &id, Some(&scope), None)
        .instrument(catalog_span.clone())
        .await;
    let next = first.as_ref().ok().and_then(|(_, next)| next.clone());
    let apply_started = now_ms();
    apply!(|rt: &mut AgentRuntime| match first {
        Ok((sessions, _)) => {
            rt.sessions = sessions;
            rt.sessions_loaded = true;
            rt.catalog_loading = next.is_some();
            rt.sessions_error = None;
            // Expanded children reload on next expansion.
            rt.children.clear();
        }
        Err(err) => {
            rt.log_warn(format!("Sessions unavailable: {err}"));
            rt.sessions_loaded = true;
            rt.catalog_loading = false;
            rt.sessions_error = Some(err);
        }
    });
    catalog_span.record("sessions.apply_ms", now_ms() - apply_started);

    // The rest of the catalog (tab counts, search) loads alongside the
    // extras, each page shown as it arrives (as the desktop app does).
    let rest = {
        let (rpc, id, span) = (rpc.clone(), id.clone(), catalog_span.clone());
        let (this, mut cx2) = (this.clone(), cx.clone());
        async move {
            let Some(cursor) = next else {
                return Ok(());
            };
            let mut cursor = Some(cursor);
            for _ in 1..CATALOG_PAGES {
                let Some(current) = cursor.take() else { break };
                // The agent answers a `session/load` far slower while catalog
                // pages are in flight (6.1s instead of 0.3s): let an open
                // session load first.
                while this
                    .read_with(&cx2, |s, cx| s.any_session_loading(cx))
                    .unwrap_or(false)
                {
                    cx2.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                }
                let (page, next) =
                    fetch_session_page(&rpc, &id, Some(&scope), Some(&current)).await?;
                let live = this
                    .update(&mut cx2, |s, cx| {
                        let Some(rt) = s.live(&id, generation) else {
                            return false;
                        };
                        // Entries listed meanwhile (a refresh) are newer: keep them.
                        let known: std::collections::HashSet<&str> =
                            rt.sessions.iter().map(|s| s.id.as_str()).collect();
                        let older: Vec<SessionSummary> = page
                            .into_iter()
                            .filter(|s| !known.contains(s.id.as_str()))
                            .collect();
                        merge_sessions(rt, older);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !live {
                    return Ok(());
                }
                // A repeated cursor would loop forever; stop instead.
                cursor = next.filter(|n| *n != current);
            }
            Ok::<_, String>(())
        }
        .instrument(span)
    };
    let extras = load_extras(
        this.clone(),
        id.clone(),
        generation,
        rpc.clone(),
        has_mesh,
        cx,
    )
    .instrument(tracing::info_span!("agent.extras"));
    let (rest, ()) = futures::join!(rest, extras);
    apply!(|rt: &mut AgentRuntime| {
        rt.catalog_loading = false;
        if let Err(err) = rest {
            rt.log_warn(format!("Older sessions unavailable: {err}"));
        }
    });
    if let Ok(Some(count)) =
        this.read_with(cx, |s, _| s.runtime.get(&id).map(|rt| rt.sessions.len()))
    {
        catalog_span.record("sessions.count", count);
    }
    drop(catalog_span);
    apply!(|rt: &mut AgentRuntime| {
        rt.busy = false;
        let summary = format!(
            "Loaded control API v{}, {} models, {} sessions",
            rt.control_version().map_or("?".into(), |v| v.to_string()),
            rt.models.len(),
            rt.sessions.len()
        );
        rt.log(summary);
    });
}

/// Models of a `_querymt/models` (or `refreshModels`) response.
fn parse_models(v: &Value) -> Vec<Model> {
    v.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let s = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_owned);
            let id = s("id")?;
            let model =
                s("model").unwrap_or_else(|| id.rsplit('/').next().unwrap_or(&id).to_string());
            Some(Model {
                label: s("label").unwrap_or_else(|| model.clone()),
                provider: s("provider").unwrap_or_default(),
                model,
                node: s("node_label").or_else(|| s("node_id")),
                node_id: s("node_id"),
                context: None,
                modalities: Vec::new(),
                raw: m.clone(),
                id,
            })
        })
        .collect()
}

/// Fill `context` and `modalities` from `_querymt/modelInfo`; models stay as they are on failure.
async fn hydrate_model_info(rpc: &RpcClient, mut models: Vec<Model>) -> Vec<Model> {
    let mut seen = std::collections::HashSet::new();
    let identities: Vec<Value> = models
        .iter()
        .filter(|m| seen.insert(format!("{}/{}", m.provider, m.model)))
        .map(|m| json!({ "provider": m.provider, "model": m.model }))
        .collect();
    let Ok(resp) = rpc
        .request("_querymt/modelInfo", json!({ "models": identities }))
        .await
    else {
        return models;
    };
    let Some(infos) = resp.get("models").and_then(Value::as_object) else {
        return models;
    };
    for m in &mut models {
        let Some(info) = infos
            .get(&format!("{}/{}", m.provider, m.model))
            .filter(|i| !i.is_null())
        else {
            continue;
        };
        // The wire shape is flat (`limit`, `modalities`) but older agents nest it.
        m.context = info
            .pointer("/limit/context")
            .or_else(|| info.pointer("/limits/context"))
            .and_then(Value::as_u64);
        let input = info
            .pointer("/modalities/input")
            .or_else(|| info.pointer("/capabilities/modalities/input"));
        m.modalities = input
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        m.modalities.dedup();
    }
    models
}

// ---------------------------------------------------------------------------
// Session config helpers

fn config_options(response: &Value) -> Vec<Value> {
    response
        .get("configOptions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Delay before automatic reconnect attempt `attempt` (0-based): doubling
/// from 250 ms, capped at 8 s, as the web client does.
fn reconnect_delay(attempt: u32) -> std::time::Duration {
    let ms = (250u64 << attempt.min(5)).min(8_000);
    std::time::Duration::from_millis(ms)
}

fn update_options(options: &mut Vec<Value>, response: &Value) {
    let fresh = config_options(response);
    if !fresh.is_empty() {
        *options = fresh;
    }
}

/// Record a successful `set_config_option`: the agent's fresh option list
/// when it sends one, otherwise the chosen value on the option itself.
fn apply_option_value(options: &mut Vec<Value>, response: &Value, id: &str, value: &str) {
    if !config_options(response).is_empty() {
        update_options(options, response);
    } else if let Some(o) = options
        .iter_mut()
        .find(|o| o.get("id").and_then(Value::as_str) == Some(id))
    {
        o["currentValue"] = Value::String(value.to_string());
    }
}

/// Find a select config option by id/category/name alias; returns its id and choice values.
fn find_option(options: &[Value], aliases: &[&str]) -> Option<(String, Vec<String>)> {
    let norm = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(|s| s.to_lowercase().replace(['-', ' '], "_"))
    };
    let option = options.iter().find(|o| {
        ["id", "category", "name"]
            .iter()
            .filter_map(|k| norm(o.get(*k)))
            .any(|v| aliases.contains(&v.as_str()))
    })?;
    let id = option.get("id")?.as_str()?.to_string();
    let mut values = Vec::new();
    for entry in option
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match entry.get("options").and_then(Value::as_array) {
            Some(group) => values.extend(
                group
                    .iter()
                    .filter_map(|e| e.get("value")?.as_str().map(str::to_owned)),
            ),
            None => values.extend(
                entry
                    .get("value")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ),
        }
    }
    Some((id, values))
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Copyable report for a failed `session/load`: what was asked, of whom, and
/// the agent's raw JSON-RPC error (its `data` usually carries the real cause).
pub(crate) fn load_error_report(
    agent: &str,
    params: &Value,
    err: &crate::rpc::RpcError,
    elapsed_ms: f64,
) -> String {
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    let raw = err.raw().map_or_else(
        || "(none: the request failed before the agent answered)".to_string(),
        pretty,
    );
    format!(
        "QueryMT GPUI {version} · session/load failed\n\
         Session: {session}\n\
         Workspace: {cwd}\n\
         Agent: {agent}\n\
         Time: {time} ({elapsed:.0} ms)\n\
         Error: {err}\n\n\
         Agent error (JSON-RPC):\n{raw}\n\n\
         Request params:\n{params}\n",
        version = crate::util::APP_VERSION,
        session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("?"),
        cwd = params.get("cwd").and_then(Value::as_str).unwrap_or("?"),
        time = crate::util::rfc3339_now(),
        elapsed = elapsed_ms,
        params = pretty(params),
    )
}

/// ACP prompt blocks: text plus attachments. Images are native `image` blocks
/// unless `image_mode` asks for resources; other files are always embedded resources.
/// Serialized size of a value, counted without building the string.
pub(crate) fn json_size(value: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

pub(crate) fn prompt_content(
    prompt: &str,
    attachments: &[Attachment],
    image_mode: ImageSendMode,
) -> Vec<Value> {
    let mut content = Vec::new();
    if !prompt.is_empty() {
        content.push(json!({ "type": "text", "text": prompt }));
    }
    for a in attachments {
        let meta =
            json!({ "querymt": { "attachment_id": a.id, "filename": a.name, "size": a.size } });
        if a.is_image() && image_mode == ImageSendMode::Image {
            content.push(
                json!({ "type": "image", "data": a.data, "mimeType": a.mime, "_meta": meta }),
            );
        } else {
            content.push(json!({
                "type": "resource",
                "_meta": meta,
                "resource": {
                    "uri": format!("attachment:///{}/{}", percent_encode(&a.id), percent_encode(&a.name)),
                    "blob": a.data,
                    "mimeType": a.mime,
                    "_meta": meta,
                },
            }));
        }
    }
    content
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_owned))
        .collect()
}

/// Mesh status/nodes/invites, provider auth and profiles; each applied as it arrives.
async fn load_extras(
    this: WeakEntity<AgentsStore>,
    id: String,
    generation: u64,
    rpc: Rc<RpcClient>,
    has_mesh: bool,
    cx: &mut AsyncApp,
) {
    let apply = |f: Box<dyn FnOnce(&mut AgentRuntime)>, cx: &mut AsyncApp| -> bool {
        this.update(cx, |s, cx| {
            let Some(rt) = s.live(&id, generation) else {
                return false;
            };
            f(rt);
            cx.notify();
            true
        })
        .unwrap_or(false)
    };
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);

    if has_mesh {
        let status = rpc.request("_querymt/mesh/status", json!({})).await;
        let ok = apply(
            Box::new(move |rt| {
                if let Ok(m) = status {
                    let relays = m
                        .pointer("/iroh/relays")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|r| {
                            (
                                r.get("url")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                r.get("connected").and_then(Value::as_bool).unwrap_or(false),
                            )
                        })
                        .collect();
                    rt.mesh = Some(MeshStatus {
                        enabled: m.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                        peer_id: m.get("peer_id").and_then(Value::as_str).map(str::to_owned),
                        hostname: m.get("hostname").and_then(Value::as_str).map(str::to_owned),
                        peers: m
                            .get("known_peer_count")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        transport: m
                            .get("transport")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        relays,
                    });
                }
            }),
            cx,
        );
        if !ok {
            return;
        }
        let nodes = rpc.request("_querymt/mesh/nodes", json!({})).await;
        let invites = rpc.request("_querymt/mesh/listInvites", json!({})).await;
        if !apply(
            Box::new(move |rt| {
                if let Ok(v) = nodes {
                    rt.mesh_nodes = v
                        .get("nodes")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|n| {
                            Some(MeshNode {
                                id: text(n, "id")?,
                                label: text(n, "label").unwrap_or_default(),
                                active_sessions: n
                                    .get("active_sessions")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0),
                                capabilities: strs(n.get("capabilities")),
                                transport: text(n, "transport").unwrap_or_default(),
                            })
                        })
                        .collect();
                    rt.mesh_nodes.sort_by_key(|a| a.label.to_lowercase());
                }
                if let Ok(v) = invites {
                    let num = |i: &Value, k: &str| i.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                    rt.mesh_invites = v
                        .get("invites")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|i| {
                            Some(MeshInvite {
                                id: text(i, "invite_id")?,
                                mesh_name: text(i, "mesh_name"),
                                expires_at: num(i, "expires_at"),
                                max_uses: num(i, "max_uses") as u64,
                                uses_remaining: num(i, "uses_remaining") as u64,
                                status: text(i, "status").unwrap_or_default(),
                                created_at: num(i, "created_at"),
                            })
                        })
                        .collect();
                }
            }),
            cx,
        ) {
            return;
        }
    }

    let auth = rpc.request("_querymt/auth/status", json!({})).await;
    let profiles = rpc.request("_querymt/profiles", json!({})).await;
    apply(
        Box::new(move |rt| {
            rt.providers_loaded = true;
            rt.providers_error = auth.as_ref().err().map(ToString::to_string);
            if let Ok(v) = auth {
                (rt.providers, rt.mcp_servers) = parse_auth_status(&v);
            }
            if let Ok(v) = profiles {
                rt.profiles = parse_profiles(&v);
            }
        }),
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backs_off_like_the_web_client() {
        let ms: Vec<u128> = (0..8).map(|a| reconnect_delay(a).as_millis()).collect();
        assert_eq!(ms, [250, 500, 1000, 2000, 4000, 8000, 8000, 8000]);
    }

    /// An extension call answered after its connection was replaced changes
    /// nothing: no error, no `then`, and the same action started on the new
    /// connection stays pending.
    #[gpui_kit::test]
    fn ext_answer_from_an_old_connection_is_ignored(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let store = cx.new(|_| automations::schedule_ui_fixture());
        let (old_rpc, old_requests) =
            cx.update(|cx| store.update(cx, |s, _| s.rpc_for_test("test-agent")));
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.create_invite("test-agent", "1h", None, None, cx)
            })
        });
        cx.run_until_parked();
        let old_invite: Value = serde_json::from_str(&old_requests.try_recv().unwrap()).unwrap();

        // Reconnect: the old request fails as the old client closes, and the
        // same action starts on the new connection before that failure lands.
        let (rpc, requests) = cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.disconnect("test-agent", cx);
                let link = s.rpc_for_test("test-agent");
                s.runtime_mut_for_test("test-agent").state = ConnState::Connected;
                s.create_invite("test-agent", "1h", None, None, cx);
                link
            })
        });
        cx.run_until_parked();
        // A late answer to the old request cannot reach the new client either.
        old_rpc.handle(&json!({ "id": old_invite["id"], "result": { "url": "old" } }).to_string());
        cx.run_until_parked();
        cx.update(|cx| {
            let rt = store.read(cx).runtime("test-agent").unwrap();
            assert_eq!(rt.action_error, None, "the old failure is not reported");
            assert!(
                rt.pending.contains("invite"),
                "the new call is still in flight"
            );
            assert_eq!(rt.last_invite, None);
        });

        // The current connection's answer applies as usual.
        let invite: Value = serde_json::from_str(&requests.try_recv().unwrap()).unwrap();
        assert_eq!(invite["method"], "_querymt/mesh/createInvite");
        rpc.handle(&json!({ "id": invite["id"], "result": { "url": "new" } }).to_string());
        cx.run_until_parked();
        cx.update(|cx| {
            let rt = store.read(cx).runtime("test-agent").unwrap();
            assert!(!rt.pending.contains("invite"));
            assert_eq!(rt.last_invite.as_deref(), Some("new"));
        });
    }

    #[test]
    fn applied_reasoning_level_reaches_the_new_session_options() {
        let option = |current: &str| {
            json!({ "id": "reasoning_effort", "type": "select", "currentValue": current,
                    "options": [{ "value": "auto" }, { "value": "high" }] })
        };
        // The agent acknowledges without an option list: set the value ourselves.
        let mut options = vec![option("auto")];
        apply_option_value(&mut options, &json!({}), "reasoning_effort", "high");
        assert_eq!(options[0]["currentValue"], "high");
        // The agent returns fresh options: trust them.
        let mut options = vec![option("auto")];
        let resp = json!({ "configOptions": [option("high")] });
        apply_option_value(&mut options, &resp, "reasoning_effort", "low");
        assert_eq!(options[0]["currentValue"], "high");
    }

    #[test]
    fn session_list_entry_reads_relations_worktree_remote_and_status() {
        let entry = json!({
            "sessionId": "s1", "cwd": "/w/checkout", "title": "T", "updatedAt": "2026-11-07T23:26:14Z",
            "_meta": {
                "parentSessionId": "p1", "forkOrigin": "user", "forkCount": 2, "hasChildren": true,
                "messageCount": 5, "hasErrors": true,
                "location": "remote", "nodeId": "node-123456789", "nodeLabel": "", "connectionState": "disconnected",
                "runtimeStatus": { "phase": "idle", "observation": { "availability": "unavailable" } },
                "querymt": {
                    "lifecycle": { "session_id": "s1", "resolved": true, "archived": false },
                    "pin": { "pinned": true, "pinned_at": "x" },
                    "workspace": { "workspaceId": "w", "branch": "feat", "sourceCwd": "/w/repo", "lifecycle": "failed" }
                }
            }
        });
        let s = SessionSummary::from_list_entry("a", &entry);
        assert!(!s.root && s.parent.as_deref() == Some("p1"));
        assert_eq!((s.fork_count, s.has_children, s.messages), (2, true, 5));
        assert!(s.has_errors && s.pinned && s.resolved && !s.open && s.lifecycle_known);
        // Unobservable runtime is unknown, never running.
        assert!(s.status_unknown() && !s.running());
        assert_eq!(s.group_cwd(), "/w/repo");
        assert!(!s.worktree.as_ref().unwrap().usable());
        let remote = s.remote.unwrap();
        assert!(!remote.connected);
        assert_eq!(remote.name(), "node-123");

        let bare = SessionSummary::from_list_entry("a", &json!({ "sessionId": "s2", "cwd": "/w" }));
        assert!(bare.root && !bare.lifecycle_known && bare.status_unknown());
        assert_eq!(bare.group_cwd(), "/w");
        let live = json!({ "sessionId": "s3", "_meta": { "runtimeStatus": { "phase": "tools" } } });
        assert!(SessionSummary::from_list_entry("a", &live).running());
    }

    #[test]
    fn new_session_params_send_only_non_default_profiles() {
        use launch::{WorktreeMode, new_session_params};
        let plain = new_session_params("/w", DEFAULT_PROFILE, WorktreeMode::Local);
        assert_eq!(plain, json!({ "cwd": "/w", "mcpServers": [] }));
        let bound = new_session_params("/w", "review", WorktreeMode::Local);
        assert_eq!(
            bound.pointer("/_meta/querymt/profile_id"),
            Some(&json!("review"))
        );
    }

    fn attachment(name: &str, mime: &str) -> Attachment {
        Attachment {
            id: format!("id-{name}"),
            name: name.into(),
            mime: mime.into(),
            size: 3,
            data: "AAA".into(),
            image: None,
        }
    }

    #[test]
    fn image_send_mode_controls_how_images_are_encoded() {
        let files = [
            attachment("a.png", "image/png"),
            attachment("b.txt", "text/plain"),
        ];
        let kinds = |mode| -> Vec<String> {
            prompt_content("hi", &files, mode)
                .iter()
                .map(|b| b["type"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        assert_eq!(kinds(ImageSendMode::Image), ["text", "image", "resource"]);
        assert_eq!(
            kinds(ImageSendMode::Resource),
            ["text", "resource", "resource"]
        );
    }

    #[test]
    fn send_shortcut_decides_which_enter_sends() {
        assert!(SendShortcut::Enter.submits(false, false));
        assert!(!SendShortcut::Enter.submits(true, false));
        assert!(SendShortcut::ShiftEnter.submits(true, false));
        assert!(!SendShortcut::ShiftEnter.submits(false, false));
        assert!(SendShortcut::SecondaryEnter.submits(false, true));
        assert!(!SendShortcut::SecondaryEnter.submits(false, false));
    }

    #[test]
    fn same_model_on_two_machines_stays_distinct() {
        let models = parse_models(&json!({ "models": [
            { "id": "codex/gpt-6.1-sol", "provider": "codex", "model": "gpt-6.1-sol" },
            { "id": "codex/gpt-6.1-sol", "provider": "codex", "model": "gpt-6.1-sol",
              "node_id": "peer-thor", "node_label": "thor" },
        ]}));
        let (local, remote) = (&models[0], &models[1]);
        assert_eq!(local.key(), "codex/gpt-6.1-sol");
        assert_ne!(local.key(), remote.key());
        assert_eq!(
            find_model(&models, &remote.key()).unwrap().node.as_deref(),
            Some("thor")
        );
        assert!(find_model(&models, &local.key()).unwrap().node_id.is_none());
        // A bare id saved before keys had nodes prefers the agent's own model.
        assert!(find_model(&models[1..], "codex/gpt-6.1-sol").is_some());
        assert_eq!(remote.describe(), "gpt-6.1-sol from codex on thor");

        let mut store = AgentsStore {
            configs: Vec::new(),
            prefs: Prefs::default(),
            launch: Default::default(),
            open_sessions: Default::default(),
            requests: Vec::new(),
            runtime: HashMap::new(),
            sidebar: Default::default(),
            early_commands: Default::default(),
            #[cfg(not(target_family = "wasm"))]
            updates: Default::default(),
            first_run: false,
        };
        store.runtime.entry("a".into()).or_default().models = models.clone();
        store.remember_model("a", &remote.key());
        store.remember_model("a", &local.key());
        let recent: Vec<String> = store.recent_models("a").iter().map(|m| m.key()).collect();
        assert_eq!(recent, [local.key(), remote.key()]);
    }

    #[test]
    fn session_tracks_the_machine_of_its_model() {
        let option = |node: Option<&str>| {
            let mut entry = json!({ "id": "codex/m", "provider": "codex", "model": "m" });
            if let Some(node) = node {
                entry["node_id"] = json!(node);
            }
            json!([{ "id": "model", "type": "select", "currentValue": "codex/m",
                     "_meta": { "querymt": { "modelEntry": entry } } }])
        };
        let mut view = crate::session::SessionView::default();
        view.apply_config(Some(&option(Some("peer-thor"))));
        assert_eq!(view.model_node.as_deref(), Some("peer-thor"));
        // A confirmation without the entry keeps the machine of the same model…
        view.apply_config(Some(&json!([{ "id": "model", "currentValue": "codex/m" }])));
        assert_eq!(view.model_node.as_deref(), Some("peer-thor"));
        // …and drops it for another model.
        view.apply_config(Some(&json!([{ "id": "model", "currentValue": "codex/n" }])));
        assert!(view.model_node.is_none());
        view.apply_config(Some(&option(None)));
        assert!(view.model_node.is_none());
    }

    #[test]
    fn old_prefs_load_with_quick_defaults() {
        // Saved before the quick panel existed.
        let prefs: Prefs = serde_json::from_str(r#"{"dark":true,"primary_agent":"a"}"#).unwrap();
        assert_eq!(prefs.quick, QuickPrefs::default());
        assert!(!prefs.quick.enabled);
        assert!(prefs.quick.shortcut.is_empty());
        // Settings saved before the picker had layouts open in the list.
        assert_eq!(prefs.quick.picker_layout, PickerLayout::List);
        let before_layouts: Prefs = serde_json::from_str(
            r#"{"dark":null,"primary_agent":null,"quick":{"enabled":true,"shortcut":"alt+KeyK"}}"#,
        )
        .unwrap();
        assert_eq!(before_layouts.quick.picker_layout, PickerLayout::List);
        // A saved choice survives a round trip.
        let json = serde_json::to_string(&Prefs {
            quick: QuickPrefs {
                enabled: true,
                shortcut: "alt+KeyK".into(),
                picker_layout: PickerLayout::Grid,
                positions: [(
                    "display-1".to_string(),
                    QuickPosition {
                        left: 10,
                        top: 20,
                        bottom: 84,
                    },
                )]
                .into(),
            },
            ..prefs
        })
        .unwrap();
        assert!(json.contains(r#""picker_layout":"grid""#));
        let back: Prefs = serde_json::from_str(&json).unwrap();
        assert!(back.quick.enabled);
        assert_eq!(back.quick.shortcut, "alt+KeyK");
        assert_eq!(back.quick.picker_layout, PickerLayout::Grid);
        // A place left on a display survives a restart.
        assert_eq!(back.quick.positions["display-1"].bottom, 84);
        // A partial `quick` object fills in the rest.
        let partial: Prefs =
            serde_json::from_str(r#"{"dark":null,"primary_agent":null,"quick":{"enabled":true}}"#)
                .unwrap();
        assert!(partial.quick.enabled && partial.quick.shortcut.is_empty());
    }

    #[test]
    fn prefs_without_image_mode_default_to_native_images() {
        let prefs: Prefs = serde_json::from_str(r#"{"dark":null,"primary_agent":null}"#).unwrap();
        assert_eq!(prefs.image_send_mode, ImageSendMode::Image);
        let json = serde_json::to_string(&Prefs {
            image_send_mode: ImageSendMode::Resource,
            ..prefs
        })
        .unwrap();
        assert!(json.contains(r#""image_send_mode":"resource""#));
    }

    fn question(multiple: bool) -> Value {
        let choices = json!([
            { "const": "A", "title": "A", "_meta": { "querymt": { "description": "first" } } },
            { "const": "B", "title": "B", "_meta": { "querymt": { "description": "B" } } },
        ]);
        let selection = if multiple {
            json!({ "type": "array", "title": "Checks", "description": "Which?", "items": { "anyOf": choices } })
        } else {
            json!({ "type": "string", "title": "Pick", "description": "Which?", "oneOf": choices })
        };
        json!({
            "sessionId": "s1", "mode": "form", "message": "Which?",
            "requestedSchema": { "type": "object", "properties": { "selection": selection }, "required": ["selection"] },
            "_meta": { "querymt": { "elicitation_id": "q1", "source": "builtin:question" } }
        })
    }

    #[test]
    fn question_parses_options_descriptions_and_custom() {
        for multiple in [false, true] {
            let r =
                parse_request("a", json!(7), "elicitation/create", &question(multiple)).unwrap();
            assert_eq!(r.key, "a:q1");
            assert!(r.answerable() && r.stable);
            let f = &r.fields()[0];
            assert_eq!(
                f.kind,
                if multiple {
                    FieldKind::Array
                } else {
                    FieldKind::String
                }
            );
            assert!(f.required && f.allow_custom);
            assert_eq!(f.options.len(), 2);
            assert_eq!(f.options[0].description.as_deref(), Some("first"));
            // A description equal to the label adds nothing.
            assert_eq!(f.options[1].description, None);
            assert_eq!(
                r.drafts[0].value,
                if multiple { json!([]) } else { json!("") }
            );
            let labels: Vec<_> = r.actions().into_iter().map(|a| a.label).collect();
            assert_eq!(labels, ["Submit", "Decline", "Cancel"]);
        }
    }

    #[test]
    fn multi_select_answers_are_arrays_and_custom_is_wrapped() {
        let r = parse_request("a", json!(1), "elicitation/create", &question(true)).unwrap();
        let fields = r.fields();
        let empty = [FieldDraft {
            value: json!([]),
            custom: None,
        }];
        assert_eq!(
            elicitation_content(fields, &empty).unwrap_err(),
            "Please complete Checks."
        );
        let picked = [FieldDraft {
            value: json!(["A", "B"]),
            custom: None,
        }];
        assert_eq!(
            elicitation_content(fields, &picked).unwrap(),
            json!({ "selection": ["A", "B"] })
        );
        let custom = [FieldDraft {
            value: json!([]),
            custom: Some(" mine ".into()),
        }];
        assert_eq!(
            elicitation_content(fields, &custom).unwrap(),
            json!({ "selection": ["mine"] })
        );
        let blank = [FieldDraft {
            value: json!([]),
            custom: Some("  ".into()),
        }];
        assert_eq!(
            elicitation_content(fields, &blank).unwrap_err(),
            "Please enter a custom response."
        );

        let single = parse_request("a", json!(1), "elicitation/create", &question(false)).unwrap();
        let custom = [FieldDraft {
            value: json!(""),
            custom: Some("other".into()),
        }];
        assert_eq!(
            elicitation_content(single.fields(), &custom).unwrap(),
            json!({ "selection": "other" })
        );
    }

    #[test]
    fn form_fields_keep_types_and_reject_unknown_ones() {
        let params = json!({
            "mode": "form", "message": "m",
            "requestedSchema": { "title": "Setup", "properties": {
                "name": { "type": "string" }, "n": { "type": "integer" },
                "ok": { "type": "boolean", "default": true }
            }, "required": ["name"] }
        });
        let r = parse_request("a", json!(1), "elicitation/create", &params).unwrap();
        assert_eq!(r.title, "Setup");
        assert!(!r.stable && r.key == "a:rpc:1");
        let kinds: Vec<_> = r.fields().iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [FieldKind::String, FieldKind::Integer, FieldKind::Boolean]
        );
        let drafts = [
            FieldDraft {
                value: json!("x"),
                custom: None,
            },
            FieldDraft {
                value: json!("4.5"),
                custom: None,
            },
            FieldDraft {
                value: json!(true),
                custom: None,
            },
        ];
        assert_eq!(
            elicitation_content(r.fields(), &drafts).unwrap_err(),
            "n must be a whole number."
        );

        let odd = json!({ "mode": "form", "message": "m",
            "requestedSchema": { "properties": { "x": { "type": "object" } } } });
        let r = parse_request("a", json!(2), "elicitation/create", &odd).unwrap();
        assert!(matches!(
            r.kind,
            RequestKind::Elicitation {
                unsupported: true,
                ..
            }
        ));
        let labels: Vec<_> = r.actions().into_iter().map(|a| a.label).collect();
        assert_eq!(labels, ["Decline", "Cancel"]);
    }

    #[test]
    fn permission_keys_use_interaction_id_and_primary_allow() {
        let params = json!({
            "sessionId": "s", "toolCall": { "toolCallId": "t1", "title": "Run ls", "kind": "execute" },
            "options": [
                { "optionId": "r", "name": "Reject", "kind": "reject_once" },
                { "optionId": "o", "name": "Allow", "kind": "allow_once" }
            ],
            "_meta": { "querymt": { "interaction_id": "i9" } }
        });
        let r = parse_request("a", json!(3), "session/request_permission", &params).unwrap();
        assert_eq!(r.key, "a:i9");
        assert!(r.detail.contains("(execute)"));
        let primary: Vec<_> = r.actions().into_iter().map(|a| a.primary).collect();
        assert_eq!(primary, [false, true]);
    }

    #[test]
    fn recovery_only_over_secure_or_loopback() {
        let ws = |address: &str, secure| Endpoint::WebSocket {
            address: address.into(),
            secure,
        };
        assert!(recovery_transport_ok(&ws("127.0.0.1:3030", false)));
        assert!(recovery_transport_ok(&ws("[::1]:3030", false)));
        assert!(recovery_transport_ok(&ws("example.com:443", true)));
        assert!(!recovery_transport_ok(&ws("10.0.0.2:3030", false)));
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod seed_tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// A temp dir with an app executable directory and a PATH directory.
    struct Env {
        root: tempfile::TempDir,
    }

    impl Env {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }

        fn touch(&self, rel: &str) {
            let path = self.root.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }

        fn exe(&self) -> PathBuf {
            self.root.path().join("app").join("querymt-gpui")
        }

        fn path_var(&self) -> OsString {
            std::env::join_paths([self.root.path().join("bin")]).unwrap()
        }

        fn seed(&self, suffix: &str) -> Option<AgentConfig> {
            local_qmtcode_seed(Some(&self.exe()), Some(&self.path_var()), suffix)
        }
    }

    const SUFFIX: &str = std::env::consts::EXE_SUFFIX;

    fn ws_seed() -> AgentConfig {
        AgentConfig {
            id: "ws-host:1234".into(),
            name: "host:1234".into(),
            endpoint: Endpoint::WebSocket {
                address: "host:1234".into(),
                secure: false,
            },
            auto_start: true,
            host_managed: false,
        }
    }

    fn local_seed() -> AgentConfig {
        AgentConfig {
            id: LOCAL_QMTCODE_ID.into(),
            name: "Local qmtcode".into(),
            endpoint: Endpoint::Process {
                command: "qmtcode --acp".into(),
            },
            auto_start: false,
            host_managed: false,
        }
    }

    fn ids(configs: &[AgentConfig]) -> Vec<&str> {
        configs.iter().map(|c| c.id.as_str()).collect()
    }

    /// The app is renamed "QueryMT Desktop", but its state stays where the
    /// earlier builds wrote it: nothing in the path follows the bundle name
    /// or identifier.
    #[test]
    fn user_state_survives_the_rename() {
        let path = state_path().expect("a config directory");
        assert!(
            path.ends_with(std::path::Path::new("querymt-gpui").join("state.json")),
            "{}",
            path.display()
        );
    }

    #[test]
    fn bundled_qmtcode_seeds_the_local_agent() {
        let env = Env::new();
        env.touch(&format!("app/qmtcode{SUFFIX}"));
        let seed = env.seed(SUFFIX).expect("seeded");
        assert_eq!(seed.id, "local-qmtcode");
        assert_eq!(seed.name, "Local qmtcode");
        assert_eq!(
            seed.endpoint,
            Endpoint::Process {
                command: "qmtcode --acp".into()
            }
        );
        assert!(seed.auto_start);
        let (configs, ..) = startup_state(None, vec![seed]);
        assert_eq!(ids(&configs), ["local-qmtcode"]);
    }

    #[test]
    fn windows_path_qmtcode_seeds_the_local_agent() {
        let env = Env::new();
        // A suffix-less file is not a Windows executable.
        env.touch("bin/qmtcode");
        assert!(env.seed(".exe").is_none());
        env.touch("bin/qmtcode.exe");
        assert_eq!(env.seed(".exe").unwrap().id, "local-qmtcode");
    }

    #[test]
    fn nothing_to_seed_without_qmtcode() {
        let env = Env::new();
        assert!(env.seed(SUFFIX).is_none());
        let (configs, ..) = startup_state(None, Vec::new());
        assert!(configs.is_empty());
    }

    #[test]
    fn existing_installs_gain_the_local_agent() {
        let saved = Persisted {
            configs: vec![ws_seed()],
            prefs: Prefs::default(),
        };
        let (configs, prefs, first_run) = startup_state(Some(saved), vec![local_seed()]);
        assert_eq!(ids(&configs), ["local-qmtcode", "ws-host:1234"]);
        assert!(!first_run);
        assert!(prefs.dismissed_seeds.is_empty());
    }

    #[test]
    fn older_state_files_load_without_dismissals() {
        // Written before dismissals existed.
        let text = r#"{"configs":[],"prefs":{"dark":true}}"#;
        let saved: Persisted = serde_json::from_str(text).unwrap();
        assert!(saved.prefs.dismissed_seeds.is_empty());
        let (configs, ..) = startup_state(Some(saved), vec![local_seed()]);
        assert_eq!(ids(&configs), ["local-qmtcode"], "nothing is dismissed");
    }

    fn load(text: &str) -> Persisted {
        serde_json::from_str(text).expect("state loads")
    }

    /// The preferences the next start would read after `prefs` was saved.
    fn reload(prefs: &Prefs) -> Prefs {
        let text = serde_json::to_string(&Persisted {
            configs: Vec::new(),
            prefs: prefs.clone(),
        })
        .expect("state saves");
        startup_state(Some(load(&text)), Vec::new()).1
    }

    #[test]
    fn older_preferences_load_without_theme_keys() {
        let saved = load(r#"{"configs":[],"prefs":{"dark":false,"cwd":"/w","accent":"blue"}}"#);
        assert_eq!(saved.prefs.dark, Some(false));
        assert_eq!(saved.prefs.cwd, "/w");
        assert_eq!(saved.prefs.theme_id(true), crate::theme::ThemeId::QMT_DARK);
        assert_eq!(
            saved.prefs.theme_id(false),
            crate::theme::ThemeId::QMT_LIGHT
        );
        let (_, prefs, _) = startup_state(Some(saved), Vec::new());
        assert_eq!(prefs.accent, "blue", "a non-orange accent is kept");
        assert_eq!(prefs.accent_choice().id(), "blue");
    }

    #[test]
    fn legacy_orange_accent_becomes_the_theme_accent() {
        let saved = load(r#"{"configs":[],"prefs":{"accent":"orange"}}"#);
        assert_eq!(saved.prefs.appearance_version, AppearanceVersion::legacy());
        let (_, prefs, _) = startup_state(Some(saved), Vec::new());
        assert_eq!(prefs.accent, "");
        assert_eq!(prefs.appearance_version, AppearanceVersion::CURRENT);
        assert_eq!(prefs.accent_choice(), crate::theme::Accent::Theme);
    }

    #[test]
    fn orange_picked_after_the_upgrade_is_kept() {
        let (_, mut prefs, _) = startup_state(None, Vec::new());
        prefs.accent = "orange".into();
        assert_eq!(reload(&prefs).accent, "orange");
        assert_eq!(reload(&reload(&prefs)).accent, "orange");
    }

    #[gpui_kit::test]
    fn orange_survives_reset_app_data(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let store = cx.new(|_| {
            let mut store = automations::schedule_ui_fixture();
            store.prefs.accent = "teal".into();
            store
        });
        cx.update(|cx| store.update(cx, |s, cx| s.reset_with_seeds(vec![local_seed()], cx)));
        let prefs = cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.update_prefs(|p| p.accent = "orange".into(), cx);
                s.prefs.clone()
            })
        });
        assert_eq!(prefs.appearance_version, AppearanceVersion::CURRENT);
        assert_eq!(reload(&prefs).accent, "orange");
    }

    #[test]
    fn invalid_version_does_not_rewrite_the_accent() {
        let saved =
            load(r#"{"configs":[],"prefs":{"accent":"orange","appearance_version":"two"}}"#);
        let (_, prefs, _) = startup_state(Some(saved), Vec::new());
        assert_eq!(prefs.accent, "orange");
        assert_eq!(prefs.appearance_version, AppearanceVersion::CURRENT);
    }

    #[test]
    fn fresh_install_is_current() {
        let (_, prefs, first_run) = startup_state(None, Vec::new());
        assert!(first_run);
        assert_eq!(prefs.appearance_version, AppearanceVersion::CURRENT);
        assert_eq!(prefs.accent, "");
    }

    #[test]
    fn non_string_theme_value_is_unset() {
        let text = serde_json::json!({
            "configs": [local_seed()],
            "prefs": { "theme_light": 7, "theme_dark": "tokyo-night", "cwd": "/w" },
        })
        .to_string();
        let saved = load(&text);
        assert_eq!(saved.prefs.theme_light, "");
        assert_eq!(saved.prefs.theme_dark, "tokyo-night");
        assert_eq!(saved.prefs.cwd, "/w");
        assert_eq!(saved.configs.len(), 1, "agents still load");
    }

    #[test]
    fn unknown_theme_id_is_kept() {
        let saved = load(
            r#"{"configs":[],"prefs":{"theme_dark":"removed-theme","theme_light":"tokyo-night"}}"#,
        );
        let (_, prefs, _) = startup_state(Some(saved), Vec::new());
        // Resolved to the built-in themes, saved as it was.
        assert_eq!(prefs.theme_id(true), crate::theme::ThemeId::QMT_DARK);
        assert_eq!(prefs.theme_id(false), crate::theme::ThemeId::QMT_LIGHT);
        let again = reload(&prefs);
        assert_eq!(again.theme_dark, "removed-theme");
        assert_eq!(again.theme_light, "tokyo-night");
    }

    #[gpui_kit::test]
    fn deleted_local_agent_does_not_return(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let store = cx.new(|_| {
            let mut store = automations::schedule_ui_fixture();
            store.configs = vec![local_seed()];
            store
        });
        cx.update(|cx| store.update(cx, |s, cx| s.delete_config(LOCAL_QMTCODE_ID, cx)));
        // What the next start reads back.
        let saved = cx.update(|cx| {
            let s = store.read(cx);
            Persisted {
                configs: s.configs.clone(),
                prefs: s.prefs.clone(),
            }
        });
        assert_eq!(saved.prefs.dismissed_seeds, ["local-qmtcode"]);
        let json = serde_json::to_string(&saved).unwrap();
        let reloaded: Persisted = serde_json::from_str(&json).unwrap();
        let (configs, ..) = startup_state(Some(reloaded), vec![local_seed()]);
        assert!(
            ids(&configs).is_empty(),
            "still deleted: {:?}",
            ids(&configs)
        );
    }

    #[gpui_kit::test]
    fn reset_app_data_restores_the_local_agent(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let store = cx.new(|_| {
            let mut store = automations::schedule_ui_fixture();
            store.configs = Vec::new();
            store.prefs.dismissed_seeds = vec!["local-qmtcode".into()];
            store
        });
        cx.update(|cx| store.update(cx, |s, cx| s.reset_with_seeds(vec![local_seed()], cx)));
        cx.update(|cx| {
            let s = store.read(cx);
            assert!(s.prefs.dismissed_seeds.is_empty());
            assert_eq!(ids(&s.configs), ["local-qmtcode"]);
        });
    }

    #[gpui_kit::test]
    fn requested_agent_returns_after_deletion(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let store = cx.new(|_| {
            let mut store = automations::schedule_ui_fixture();
            store.configs = vec![ws_seed()];
            store
        });
        cx.update(|cx| store.update(cx, |s, cx| s.delete_config("ws-host:1234", cx)));
        let saved = cx.update(|cx| {
            let s = store.read(cx);
            assert!(s.configs.is_empty());
            Persisted {
                configs: s.configs.clone(),
                prefs: s.prefs.clone(),
            }
        });
        assert!(saved.prefs.dismissed_seeds.is_empty(), "not dismissible");
        // The next start passes `--agent host:1234` again.
        let (configs, ..) = startup_state(Some(saved), vec![ws_seed()]);
        assert_eq!(ids(&configs), ["ws-host:1234"]);
    }
}
