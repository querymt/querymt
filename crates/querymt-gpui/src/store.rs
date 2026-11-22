//! Agents store: configured agents, their live ACP connections, and the data
//! the Start and Agents pages read (capabilities, models, sessions, mesh).
//!
//! Mirrors `querymt-desktop/src/lib/stores/agents.svelte.ts` at the scope of
//! those two pages. Views observe the entity and re-render on `cx.notify()`.

pub(crate) mod automations;
pub(crate) mod delegates;
pub(crate) mod fork;
pub(crate) mod launch;
pub(crate) mod setup;
pub(crate) mod worktree;

use std::{collections::HashMap, rc::Rc, time::Duration};

use gpui_kit::{AsyncApp, Context, Image, ImageFormat, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::Instrument as _;

use crate::rpc::{Incoming, RpcClient};
use crate::session::{ParentSession, SessionView, UndoFrame, UserAttachment};
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
    raw: Value,
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

    /// Transcript representation of this attachment (keeps image bytes for the thumbnail).
    pub fn to_user_attachment(&self) -> UserAttachment {
        UserAttachment {
            name: self.name.clone(),
            mime: self.mime.clone(),
            size: Some(self.size),
            data: self.is_image().then(|| self.data.clone()),
        }
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

#[derive(Clone, Debug)]
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
        let summary = {
            let mut call = crate::session::SessionView::default();
            let mut update = tool.clone();
            if let Some(obj) = update.as_object_mut() {
                obj.insert("sessionUpdate".into(), Value::from("tool_call"));
            }
            call.apply(&update);
            match call.items.first().map(|i| &i.kind) {
                Some(crate::session::ItemKind::Work(entries)) => match entries.first() {
                    Some(crate::session::WorkEntry::Tool(c)) => c.summary(),
                    _ => String::new(),
                },
                _ => String::new(),
            }
        };
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
    pub providers: Vec<ProviderAuth>,
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
    /// Names of actions in flight (e.g. `invite`, `key:xai`).
    pub pending: std::collections::HashSet<String>,
    pub logs: Vec<String>,
    pub pid: Option<u32>,
    pub connected_at: Option<f64>,
    pub busy: bool,
    rpc: Option<Rc<RpcClient>>,
    tasks: Vec<Task<()>>,
    generation: u64,
    /// Consecutive automatic reconnect attempts since the last success.
    reconnect_attempt: u32,
    refresh_scheduled: bool,
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
            self.log(error.clone());
            self.session_errors.insert(session_id.to_string(), error);
        }
    }

    fn log(&mut self, line: impl Into<String>) {
        let line = line.into();
        #[cfg(not(target_family = "wasm"))]
        if std::env::var_os("QMT_DEBUG").is_some() {
            eprintln!("[agent] {line}");
        }
        self.logs.push(line);
        if self.logs.len() > 300 {
            self.logs.drain(..self.logs.len() - 300);
        }
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

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Prefs {
    pub dark: Option<bool>,
    #[serde(default)]
    pub cwd: String,
    /// Agent id -> model id used for new sessions.
    #[serde(default)]
    pub launch_models: HashMap<String, String>,
    pub primary_agent: Option<String>,
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Width of the turn diff panel the user dragged it to; `None` keeps the
    /// default (44% of the page, at most the chat width).
    #[serde(default)]
    pub turn_diff_width: Option<f32>,
    /// Accent preset id (`orange`, `blue`, …).
    #[serde(default)]
    pub accent: String,
    #[serde(default)]
    pub show_pinned_sidebar: bool,
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
}

impl Prefs {
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
        let on_path = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("qmtcode").is_file())
        });
        if on_path {
            seeds.push(AgentConfig {
                id: "local-qmtcode".into(),
                name: "Local qmtcode".into(),
                endpoint: Endpoint::Process {
                    command: "qmtcode --acp".into(),
                },
                auto_start: true,
                host_managed: false,
            });
        }
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

// ---------------------------------------------------------------------------
// Store

#[derive(Default)]
pub struct LaunchState {
    pub busy: bool,
    pub error: Option<String>,
    pub last_started: Option<String>,
    /// Mesh peer the next session runs on; `None`: the agent's machine.
    pub target: Option<String>,
    /// Worktree placement of the next local session.
    pub worktree: launch::WorktreeMode,
    /// Profile on the target peer; `None`: the peer's active profile.
    pub remote_profile: Option<String>,
}

pub struct AgentsStore {
    pub configs: Vec<AgentConfig>,
    pub prefs: Prefs,
    pub launch: LaunchState,
    /// The session open in the transcript view.
    pub session: Option<SessionView>,
    /// Permission and input requests waiting for the user (the Inbox).
    pub requests: Vec<PendingRequest>,
    runtime: HashMap<String, AgentRuntime>,
    /// This start is a fresh install (or follows Reset app data), and setup
    /// was not finished or skipped yet.
    pub first_run: bool,
    _launch_task: Option<Task<()>>,
}

impl AgentsStore {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let loaded = load_persisted();
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
        let mut configs = persisted.configs;
        for seed in seed_configs() {
            if !configs
                .iter()
                .any(|c| c.id == seed.id || c.endpoint == seed.endpoint)
            {
                configs.insert(0, seed);
            }
        }
        let mut this = Self {
            configs,
            prefs: persisted.prefs,
            launch: LaunchState::default(),
            session: None,
            requests: Vec::new(),
            runtime: HashMap::new(),
            first_run,
            _launch_task: None,
        };
        this.save();
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

    /// Selection key of the open session's model, with a node only when it
    /// is not the agent's own machine.
    pub fn session_model_key(&self) -> Option<String> {
        let view = self.session.as_ref()?;
        let own = self
            .runtime(&view.agent_id)
            .and_then(|rt| rt.mesh.as_ref())
            .and_then(|m| m.peer_id.as_deref());
        let node = view.model_node.as_deref().filter(|n| Some(*n) != own);
        Some(model_key(view.model.as_deref()?, node))
    }

    /// The open session's model in its agent's catalog.
    pub fn session_model(&self) -> Option<&Model> {
        let view = self.session.as_ref()?;
        find_model(self.models(&view.agent_id), &self.session_model_key()?)
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
                        if s.session
                            .as_ref()
                            .is_some_and(|v| v.agent_id == agent_id && v.session_id == session_id)
                        {
                            s.session = None;
                        }
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
        let ids: Vec<String> = self.runtime.keys().cloned().collect();
        for id in &ids {
            self.disconnect(id, cx);
        }
        self.runtime.clear();
        self.requests.clear();
        self.session = None;
        self.launch = LaunchState::default();
        self._launch_task = None;
        self.prefs = Prefs::default();
        self.configs = seed_configs();
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

    /// Call an agent extension method; `then` runs on the store with the result.
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
        cx.notify();
        let (id, tag) = (id.to_string(), tag.to_string());
        cx.spawn(async move |this, cx| {
            let result = rpc.request(method, params).await;
            let _ = this.update(cx, |s, cx| {
                if let Some(rt) = s.runtime.get_mut(&id) {
                    rt.pending.remove(&tag);
                    if let Err(err) = &result {
                        rt.action_error = Some(err.clone());
                    }
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
                            rt.log(format!("Models unavailable: {err}"));
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

    /// Update provider plugins (`querymt/updatePlugins`); the outcome lands in `maintenance`.
    pub fn update_plugins(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.maintenance = None;
        }
        self.ext(
            id,
            "plugins",
            "_querymt/updatePlugins",
            json!({}),
            cx,
            |s, id, res, cx| {
                let Some(rt) = s.runtime.get_mut(id) else {
                    return;
                };
                if let Ok(v) = res {
                    let results = v
                        .get("results")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let failed: Vec<(String, String)> = results
                        .iter()
                        .filter(|r| r.get("success").and_then(Value::as_bool) != Some(true))
                        .map(|r| {
                            (
                                r.get("plugin_name")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .unwrap_or_default(),
                                r.get("message")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| "Update failed.".into()),
                            )
                        })
                        .collect();
                    let ok = results.len() - failed.len();
                    rt.maintenance = Some(if failed.is_empty() {
                        (
                            format!("Updated {ok} plugin{}.", if ok == 1 { "" } else { "s" }),
                            false,
                        )
                    } else {
                        (
                            format!(
                                "{} of {} plugin updates failed.",
                                failed.len(),
                                results.len()
                            ),
                            true,
                        )
                    });
                    rt.failed_plugins = failed;
                }
                s.refresh_models(id, cx);
            },
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
        let rt = self.runtime.entry(id.to_string()).or_default();
        if let Some(rpc) = rt.rpc.take() {
            rpc.close();
        }
        rt.tasks.clear();
        rt.generation += 1;
        let generation = rt.generation;
        rt.schedules = Default::default();
        rt.providers_loaded = false;
        rt.providers_error = None;
        rt.models_loaded = false;
        rt.state = ConnState::Connecting;
        rt.pid = None;
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
                    rt.rpc = Some(rpc.clone());
                    rt.pid = conn.pid;
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
                        "clientInfo": { "name": "QueryMT GPUI", "version": env!("CARGO_PKG_VERSION") },
                        // `session.compaction`: the agent sends ID-addressed
                        // `compaction_update` / `compaction_summary_chunk` only when advertised.
                        // `_meta.querymt.sessionLoadSnapshot.epoch` (as the desktop app): the
                        // transcript is built from the `session/load` snapshot, so the agent
                        // skips the historical `session/update` replay.
                        "clientCapabilities": {
                            "fs": {},
                            "terminal": false,
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
                // The open session's live updates were lost with the old
                // connection: load it again on the new one.
                if reconnected && s.session.as_ref().is_some_and(|v| v.agent_id == id) {
                    s.reload_session(cx);
                }
                cx.notify();
            });
            load_details(this, id, generation, rpc, cx).await;
        }.instrument(connect_span).await });
        if let Some(rt) = self.runtime.get_mut(&config_id) {
            rt.tasks.push(task);
        }
    }

    pub fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.generation += 1;
            rt.reconnect_attempt = 0;
            if let Some(rpc) = rt.rpc.take() {
                rpc.close();
            }
            rt.tasks.clear();
            rt.state = ConnState::Idle;
            rt.pid = None;
            rt.log("Disconnected.");
            self.retire_requests(id);
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

    /// `session/new` → optional model selection → `session/prompt` (not awaited).
    pub fn launch(
        &mut self,
        agent_id: &str,
        cwd: String,
        prompt: String,
        attachments: Vec<Attachment>,
        cx: &mut Context<Self>,
    ) {
        let Some(rpc) = self.runtime.get(agent_id).and_then(|r| r.rpc.clone()) else {
            self.launch.error = Some("The selected agent is not connected.".into());
            cx.notify();
            return;
        };
        let model = self.launch_model(agent_id).cloned();
        // A mesh peer creates the session through the agent and owns it; this
        // machine creates it with `session/new`, in a worktree when chosen.
        let target = self.launch.target.clone();
        let (method, params) = match &target {
            Some(node) => (
                "_querymt/remote/createSession",
                launch::remote_session_params(node, &cwd, self.launch.remote_profile.as_deref()),
            ),
            None => (
                "session/new",
                launch::new_session_params(
                    &cwd,
                    self.launch_profile(agent_id),
                    self.launch_worktree(agent_id),
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
        let image_mode = self.prefs.image_send_mode;
        let mode = self.prefs.mode().to_string();
        let reasoning = self.prefs.reasoning().to_string();
        self.launch.busy = true;
        self.launch.error = None;
        self.launch.last_started = None;
        self.session = None;
        self.set_cwd(cwd.clone());
        cx.notify();

        let agent_id = agent_id.to_string();
        self._launch_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<String, String> = async {
                let created = rpc.request(method, params).await?;
                let session_id = created
                    .get("sessionId")
                    .or_else(|| created.get("session_id"))
                    .and_then(Value::as_str)
                    .ok_or("Agent did not return a session id.")?
                    .to_string();
                // A worktree session runs in its checkout, not the folder.
                let cwd = launch::session_cwd(&created).unwrap_or(cwd);
                let log = |msg: String, cx: &mut AsyncApp| {
                    let agent_id = agent_id.clone();
                    let _ = this.update(cx, |s, _| {
                        if let Some(rt) = s.runtime.get_mut(&agent_id) {
                            rt.log(msg);
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
                let _ = this.update(cx, |s, cx| {
                    let mut view = SessionView::new(&agent_id, &session_id, &cwd, &prompt.chars().take(80).collect::<String>());
                    view.push_local_user(&prompt, attachments.iter().map(Attachment::to_user_attachment).collect());
                    view.prompting = true;
                    view.sync_work(crate::util::now_ms());
                    view.apply_modes(created.get("modes"));
                    view.options = options.clone();
                    view.mode = Some(mode.clone());
                    view.model = model.as_ref().map(|m| m.id.clone());
                    view.model_node = model.as_ref().and_then(|m| m.node_id.clone());
                    s.session = Some(view);
                    cx.notify();
                });
                // The prompt resolves when the turn ends; keep it running in the background.
                let turn = rpc.request(
                    "session/prompt",
                    json!({ "sessionId": session_id, "prompt": prompt_content(&prompt, &attachments, image_mode) }),
                );
                let (this2, agent_id2, session_id2) = (this.clone(), agent_id.clone(), session_id.clone());
                cx.spawn(async move |cx| {
                    let result = turn.await;
                    let _ = this2.update(cx, |s, cx| s.finish_prompt(&agent_id2, &session_id2, result, cx));
                })
                .detach();
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
                Ok(session_id)
            }
            .await;
            let _ = this.update(cx, |s, cx| {
                s.launch.busy = false;
                match result {
                    Ok(id) => s.launch.last_started = Some(id),
                    Err(err) => s.launch.error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    // ---- session view -------------------------------------------------------

    /// Open a session in the transcript view, built from the `session/load` snapshot.
    pub fn open_session(
        &mut self,
        agent_id: &str,
        session_id: &str,
        cwd: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        if self.session.as_ref().is_some_and(|v| {
            v.agent_id == agent_id && v.session_id == session_id && v.error.is_none()
        }) {
            return;
        }
        self.remember_recent_session(agent_id, session_id);
        let mut view = SessionView::new(agent_id, session_id, cwd, title);
        let Some(rt) = self.runtime.get(agent_id) else {
            return;
        };
        if let Some(s) = rt.sessions.iter().find(|s| s.id == session_id) {
            view.phase = s.phase.clone();
        }
        let Some(rpc) = rt.rpc.clone().filter(|_| rt.is_connected()) else {
            view.error = Some("The agent of this session is not connected.".into());
            self.session = Some(view);
            cx.notify();
            return;
        };
        let windowed = rt
            .capabilities
            .as_ref()
            .and_then(|c| c.pointer("/features/session_history_window"))
            .and_then(Value::as_bool)
            == Some(true);
        view.loading = true;
        self.session = Some(view);
        cx.notify();

        let mut params = json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] });
        if windowed {
            params["_meta"] = json!({ "querymt": { "loadWindow": { "maxTurns": 20, "maxBytes": 1048576, "truncateBytes": 32768 } } });
        }
        // Context for the diagnostic report, captured before the request.
        let agent_line = {
            let config = self.configs.iter().find(|c| c.id == agent_id);
            format!(
                "{} ({}) · {} {}",
                config.map_or(agent_id, |c| c.name.as_str()),
                config.map(AgentConfig::endpoint_label).unwrap_or_default(),
                rt.agent_title.as_deref().unwrap_or("unknown agent"),
                rt.agent_version.as_deref().unwrap_or(""),
            )
        };
        let request_params = params.clone();
        let (agent_id, session_id) = (agent_id.to_string(), session_id.to_string());
        let load_span = tracing::info_span!(
            "session.open",
            session.windowed = windowed,
            session.items = tracing::field::Empty,
            session.apply_ms = tracing::field::Empty,
            session.error = tracing::field::Empty,
        );
        crate::telemetry::mark_session_open(&session_id);
        cx.spawn(async move |this, cx| {
            let started = now_ms();
            let result = rpc
                .request_detailed("session/load", params)
                .instrument(load_span.clone())
                .await;
            let elapsed_ms = now_ms() - started;
            let apply_started = now_ms();
            let _ = this.update(cx, |s, cx| {
                let Some(view) = s
                    .session
                    .as_mut()
                    .filter(|v| v.agent_id == agent_id && v.session_id == session_id)
                else {
                    return;
                };
                view.loading = false;
                match result {
                    Ok(v) => {
                        view.access = crate::session::SessionAccess::parse(&v);
                        view.apply_config(v.get("configOptions"));
                        view.apply_modes(v.get("modes"));
                        let snap = v.pointer("/_meta/querymt~1sessionLoadSnapshot.v1");
                        if let Some(rt) = snap.and_then(|s| s.get("runtimeStatus")) {
                            view.apply_runtime(rt);
                        }
                        if let Some(snap) = snap {
                            view.apply_load_snapshot(snap);
                        }
                        view.sync_work(crate::util::now_ms());
                        view.history_more = snap
                            .and_then(|s| s.pointer("/history/complete"))
                            .and_then(Value::as_bool)
                            == Some(false);
                        view.history_before = snap
                            .and_then(|s| s.pointer("/history/before"))
                            .and_then(Value::as_str)
                            .filter(|_| view.history_more)
                            .map(str::to_owned);
                        if let Some(notice) = view.after_load_notice.take() {
                            view.push_notice(notice, false);
                        }
                    }
                    Err(err) => {
                        view.error = Some(format!("Could not load the session: {err}"));
                        view.error_details = Some(load_error_report(
                            &agent_line,
                            &request_params,
                            &err,
                            elapsed_ms,
                        ));
                    }
                }
                if let Some(view) = s.session.as_ref() {
                    load_span.record("session.items", view.items.len());
                    if let Some(err) = &view.error {
                        load_span.record("session.error", err.as_str());
                    }
                }
                s.refresh_undo_stack(cx);
                s.refresh_delegates(cx);
                cx.notify();
            });
            load_span.record("session.apply_ms", now_ms() - apply_started);
        })
        .detach();
    }

    /// Fetch the turns preceding the loaded transcript (`_querymt/session/history`)
    /// and prepend them. One page in flight at a time; triggered when the user
    /// scrolls near the top.
    pub fn load_older_history(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.session.as_mut() else {
            return;
        };
        if view.history_loading || view.history_error.is_some() {
            return;
        }
        let Some(before) = view.history_before.clone() else {
            return;
        };
        let Some(rpc) = self.runtime.get(&view.agent_id).and_then(|r| r.rpc.clone()) else {
            return;
        };
        view.history_loading = true;
        let (agent_id, session_id) = (view.agent_id.clone(), view.session_id.clone());
        cx.notify();
        let req = rpc.request(
            "_querymt/session/history",
            json!({
                "sessionId": session_id,
                "before": before,
                "loadWindow": { "maxTurns": 20, "maxBytes": 1048576, "truncateBytes": 32768 },
            }),
        );
        cx.spawn(async move |this, cx| {
            let result = req.await;
            let _ = this.update(cx, |s, cx| {
                let Some(view) = s
                    .session
                    .as_mut()
                    .filter(|v| v.agent_id == agent_id && v.session_id == session_id)
                else {
                    return;
                };
                view.history_loading = false;
                match result {
                    Ok(page) => {
                        let events = page
                            .get("events")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let prompts = page
                            .get("userPrompts")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        view.prepend_events(&events, &prompts);
                        view.apply_truncation(page.get("truncated"));
                        crate::telemetry::record_history_load("ok");
                        tracing::info!(events = events.len(), "older history page loaded");
                        let complete = page
                            .pointer("/history/complete")
                            .and_then(Value::as_bool)
                            .unwrap_or(true);
                        view.history_before = page
                            .pointer("/history/before")
                            .and_then(Value::as_str)
                            .filter(|_| !complete)
                            .map(str::to_owned);
                        view.history_more = view.history_before.is_some();
                    }
                    // An undo rewrote history: the cursor is void, reload from scratch.
                    Err(err) if err.contains("history_changed") => {
                        crate::telemetry::record_history_load("history_changed");
                        cx.notify();
                        s.reload_session(cx);
                        return;
                    }
                    Err(err) => {
                        crate::telemetry::record_history_load("error");
                        tracing::warn!(error = %err, "older history page failed");
                        view.history_error = Some(err);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Clear a failed older-history fetch and try again.
    /// The open session's agent returns complete tool content a load shortened.
    pub fn supports_tool_content(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|v| self.runtime(&v.agent_id))
            .is_some_and(|rt| rt.supports_method("querymt/session/toolContent"))
    }

    /// Fetch the complete `part` (`result` or `arguments`) of tool call `id`
    /// of the open session, which a windowed load shortened.
    pub fn load_tool_content(&mut self, id: &str, part: &'static str, cx: &mut Context<Self>) {
        if !self.supports_tool_content() {
            return;
        }
        let Some(rpc) = self
            .session
            .as_ref()
            .and_then(|v| self.runtime(&v.agent_id))
            .and_then(|rt| rt.rpc.clone())
        else {
            return;
        };
        let Some(view) = self.session.as_mut() else {
            return;
        };
        let Some(call) = view.truncated_tool(id) else {
            return;
        };
        let shortened = match part {
            "result" => call.truncated_result.is_some(),
            _ => call.truncated_args.is_some(),
        };
        if !shortened || matches!(call.fetch, Some(crate::session::ContentFetch::Loading(_))) {
            return;
        }
        let (agent, session, id) = (
            view.agent_id.clone(),
            view.session_id.clone(),
            id.to_owned(),
        );
        view.set_tool_fetch(&id, Some(crate::session::ContentFetch::Loading(part)));
        cx.notify();
        let request = rpc.request(
            "_querymt/session/toolContent",
            json!({ "sessionId": session, "toolCallId": id, "part": part }),
        );
        cx.spawn(async move |this, cx| {
            let result = request.await.and_then(|v| {
                v.get("content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| "The agent returned no content.".to_string())
            });
            let _ = this.update(cx, |s, cx| {
                let Some(view) = s
                    .session
                    .as_mut()
                    .filter(|v| v.agent_id == agent && v.session_id == session)
                else {
                    return;
                };
                match result {
                    Ok(content) => view.set_full_tool_content(&id, part, content),
                    Err(error) => {
                        tracing::warn!(%error, tool = %id, part, "tool content fetch failed");
                        view.set_tool_fetch(
                            &id,
                            Some(crate::session::ContentFetch::Failed(format!(
                                "Could not load the full {}: {error}",
                                if part == "result" {
                                    "output"
                                } else {
                                    "parameters"
                                }
                            ))),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn retry_older_history(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.session.as_mut() {
            view.history_error = None;
        }
        self.load_older_history(cx);
    }

    /// Close and load the open session again.
    /// Agent of the open session supports workspace undo and redo.
    pub fn session_supports_undo(&self) -> bool {
        self.session
            .as_ref()
            .filter(|v| v.access.is_none())
            .and_then(|v| self.runtime.get(&v.agent_id))
            .is_some_and(|rt| {
                [
                    "querymt/session/undoStack",
                    "querymt/session/undo",
                    "querymt/session/redo",
                ]
                .iter()
                .all(|m| rt.supports_method(m))
            })
    }

    /// Fetch the open session's workspace undo stack (`querymt/session/undoStack`).
    pub fn refresh_undo_stack(&mut self, cx: &mut Context<Self>) {
        if !self.session_supports_undo() {
            return;
        }
        let Some(view) = self.session.as_ref() else {
            return;
        };
        let Some(rpc) = self.runtime.get(&view.agent_id).and_then(|r| r.rpc.clone()) else {
            return;
        };
        let (agent_id, session_id) = (view.agent_id.clone(), view.session_id.clone());
        let req = rpc.request(
            "_querymt/session/undoStack",
            json!({ "session_id": session_id }),
        );
        cx.spawn(async move |this, cx| {
            let Ok(resp) = req.await else { return };
            let _ = this.update(cx, |s, cx| {
                if let Some(view) = s
                    .session
                    .as_mut()
                    .filter(|v| v.agent_id == agent_id && v.session_id == session_id)
                {
                    view.undo_stack = UndoFrame::parse_stack(&resp);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Revert the workspace to before the latest undoable turn
    /// (`querymt/session/undo`), then reload the transcript.
    /// Revert the workspace to before the user turn `message_id` (and every
    /// later turn), then reload the transcript.
    pub fn undo_workspace_to(&mut self, message_id: String, cx: &mut Context<Self>) {
        self.start_undo_op(Some(message_id), cx);
    }

    /// Re-apply the most recently undone turn (`querymt/session/redo`).
    pub fn redo_workspace(&mut self, cx: &mut Context<Self>) {
        self.start_undo_op(None, cx);
    }

    /// `target`: undo to that turn; `None`: redo.
    fn start_undo_op(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        let undo = target.is_some();
        let supported = self.session_supports_undo();
        let forking = self.fork_pending();
        let Some(view) = self.session.as_mut() else {
            return;
        };
        if view.loading || view.running() || view.undo_pending || forking {
            return;
        }
        if !supported {
            view.push_notice("This agent does not support workspace undo and redo.", true);
            cx.notify();
            return;
        }
        let (method, params) = if let Some(target) = target {
            if view.undo_affected_turns(&target).is_none() {
                return;
            }
            (
                "_querymt/session/undo",
                json!({ "session_id": view.session_id, "message_id": target }),
            )
        } else {
            if view.undo_stack.is_empty() {
                return;
            }
            (
                "_querymt/session/redo",
                json!({ "session_id": view.session_id }),
            )
        };
        let Some(rpc) = self.runtime.get(&view.agent_id).and_then(|r| r.rpc.clone()) else {
            return;
        };
        view.undo_pending = true;
        let (agent_id, session_id) = (view.agent_id.clone(), view.session_id.clone());
        cx.notify();
        let req = rpc.request(method, params);
        cx.spawn(async move |this, cx| {
            let result = req.await;
            let _ = this.update(cx, |s, cx| {
                s.finish_undo_op(&agent_id, &session_id, undo, result, cx)
            });
        })
        .detach();
    }

    fn finish_undo_op(
        &mut self,
        agent_id: &str,
        session_id: &str,
        undo: bool,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .session
            .as_mut()
            .filter(|v| v.agent_id == agent_id && v.session_id == session_id)
        else {
            return;
        };
        view.undo_pending = false;
        let what = if undo { "Undo" } else { "Redo" };
        let resp = match result {
            Ok(resp) => resp,
            Err(err) => {
                view.push_notice(format!("{what} failed: {err}"), true);
                cx.notify();
                return;
            }
        };
        view.undo_stack = UndoFrame::parse_stack(&resp);
        if resp.get("success").and_then(Value::as_bool) != Some(true) {
            let msg = resp
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| format!("{what} failed."), str::to_owned);
            view.push_notice(msg, true);
            cx.notify();
            return;
        }
        let notice = if undo {
            let n = resp
                .get("reverted_files")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            match n {
                0 => "Turn undone.".to_string(),
                1 => "Undid 1 changed file.".to_string(),
                n => format!("Undid {n} changed files."),
            }
        } else {
            "Turn restored.".to_string()
        };
        // History changed: reload the transcript, then show the result.
        self.reload_session(cx);
        if let Some(view) = self.session.as_mut() {
            view.after_load_notice = Some(notice);
        }
    }

    pub fn reload_session(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.session.take() else {
            return;
        };
        self.open_session(&view.agent_id, &view.session_id, &view.cwd, &view.title, cx);
        if let Some(v) = self.session.as_mut() {
            v.parent = view.parent;
        }
    }

    /// Open delegated session `child` of the open session, remembering the
    /// open one so Back returns to it. Uses the child's own title and
    /// workspace when the session list knows them.
    pub fn open_child_session(&mut self, agent_id: &str, child: &str, cx: &mut Context<Self>) {
        let Some(current) = self.session.as_ref() else {
            return;
        };
        let parent = ParentSession {
            agent_id: current.agent_id.clone(),
            session_id: current.session_id.clone(),
            cwd: current.cwd.clone(),
            title: current.title.clone(),
            parent: current.parent.clone(),
        };
        let (cwd, title) = match self.session_summary(agent_id, child) {
            Some(s) if !s.cwd.is_empty() => (s.cwd.clone(), s.title.clone()),
            Some(s) => (parent.cwd.clone(), s.title.clone()),
            None => (parent.cwd.clone(), String::new()),
        };
        self.open_session(agent_id, child, &cwd, &title, cx);
        if let Some(v) = self
            .session
            .as_mut()
            .filter(|v| v.agent_id == agent_id && v.session_id == child)
        {
            v.parent = Some(Box::new(parent));
        }
    }

    /// Back from a delegated session: reopen its parent (keeping the chain).
    /// Returns false when the open session has no parent.
    pub fn open_parent_session(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(parent) = self.session.as_mut().and_then(|v| v.parent.take()) else {
            return false;
        };
        let ParentSession {
            agent_id,
            session_id,
            cwd,
            title,
            parent: grand,
        } = *parent;
        // Prefer the list's current title; the stored one may be stale.
        let title = self
            .session_summary(&agent_id, &session_id)
            .map(|s| s.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(title);
        self.open_session(&agent_id, &session_id, &cwd, &title, cx);
        if let Some(v) = self.session.as_mut() {
            v.parent = grand;
        }
        true
    }

    /// Whether the open session's agent supports steering / queueing input.
    pub fn session_can_control_run(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|v| self.runtime.get(&v.agent_id))
            .is_some_and(AgentRuntime::can_control_run)
    }

    /// Send a prompt to the open session. While a turn runs on an agent with
    /// turn control, `delivery` (`steer` / `queue`) picks the QueryMT extension.
    pub fn send_prompt(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        delivery: &str,
        cx: &mut Context<Self>,
    ) {
        let control = self.session_can_control_run();
        let Some(view) = self.session.as_mut() else {
            return;
        };
        let Some(rpc) = self.runtime.get(&view.agent_id).and_then(|r| r.rpc.clone()) else {
            view.push_notice("The agent is not connected.", true);
            cx.notify();
            return;
        };
        let (agent_id, session_id) = (view.agent_id.clone(), view.session_id.clone());
        let content = prompt_content(&text, &attachments, self.prefs.image_send_mode);
        if view.running() && control {
            // Like desktop: slash commands can't steer, they always queue.
            let wanted: &'static str = if text.trim_start().starts_with('/') || delivery == "queue"
            {
                "queue"
            } else {
                "steer"
            };
            let input_id = format!("gpui-{}", now_ms() as u64);
            // The message waits in the activity dock; the agent echoes it into
            // the transcript once it is applied (steer) or started (queue).
            view.add_pending_input(crate::session::PendingInput {
                input_id: input_id.clone(),
                delivery: wanted.into(),
                state: "sending".into(),
                text: text.clone(),
                attachments: attachments.len(),
                discard_pending: false,
            });
            let known_run = view.run_id.clone();
            cx.notify();
            cx.spawn(async move |this, cx| {
                // Steering must name the active run (`expected_run_id`); ask the
                // agent for it, falling back to the last one we saw.
                let mut delivery = wanted;
                let mut run_id = known_run;
                if delivery == "steer" {
                    let state = rpc.request("_querymt/session/runtimeState", json!({ "session_id": session_id })).await;
                    if let Ok(rt) = &state {
                        run_id = rt.get("active_run_id").and_then(Value::as_str).map(str::to_owned).or(run_id);
                        let phase = rt.get("phase").and_then(Value::as_str).unwrap_or_default();
                        let steerable = rt.get("steerable").and_then(Value::as_bool).unwrap_or(true);
                        if !steerable || matches!(phase, "closing" | "cancel_requested") {
                            run_id = None;
                        }
                        let _ = this.update(cx, |s, cx| {
                            if let Some(view) = s.session.as_mut().filter(|v| v.agent_id == agent_id && v.session_id == session_id) {
                                view.apply_runtime(rt);
                                view.sync_work(now_ms());
                                cx.notify();
                            }
                        });
                    }
                    // No steerable run: queue instead, as desktop does.
                    if run_id.is_none() {
                        delivery = "queue";
                    }
                }
                let mut params = json!({ "session_id": session_id, "prompt": content, "client_input_id": input_id });
                if let ("steer", Some(run)) = (delivery, &run_id) {
                    params["expected_run_id"] = json!(run);
                }
                let method = if delivery == "queue" { "_querymt/session/queue" } else { "_querymt/session/steer" };
                let result = rpc.request(method, params).await;
                let _ = this.update(cx, |s, cx| {
                    let Some(view) = s.session.as_mut().filter(|v| v.agent_id == agent_id && v.session_id == session_id) else { return };
                    match &result {
                        Err(err) => {
                            view.set_input_state(&input_id, delivery, "failed");
                            view.push_notice(format!("Could not deliver the message: {err}"), true);
                        }
                        Ok(v) => {
                            let status = v.get("status").and_then(Value::as_str).unwrap_or_default();
                            let id = v.pointer("/data/input_id").and_then(Value::as_str).unwrap_or(&input_id).to_string();
                            view.rekey_input(&input_id, &id);
                            let state = match status {
                                "steered" => "accepted",
                                "queued" => "queued",
                                // The session went idle meanwhile: it runs as a new turn.
                                _ => "started",
                            };
                            let delivery = if status == "queued" { "queue" } else { delivery };
                            view.set_input_state(&id, delivery, state);
                            // A notification may have overtaken this response.
                            if let Some((d, st)) = view.input_notes.remove(&id).or_else(|| view.input_notes.remove(&input_id)) {
                                view.set_input_state(&id, &d, &st);
                            }
                            if status == "started" {
                                view.phase = "starting".into();
                                view.sync_work(now_ms());
                            }
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        view.push_local_user(
            &text,
            attachments
                .iter()
                .map(Attachment::to_user_attachment)
                .collect(),
        );
        view.prompting = true;
        view.sync_work(crate::util::now_ms());
        let turn = rpc.request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": content }),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = turn.await;
            let _ = this.update(cx, |s, cx| {
                s.finish_prompt(&agent_id, &session_id, result, cx)
            });
        })
        .detach();
    }

    /// Remove a queued message before it starts (`querymt/session/discardQueuedInput`).
    pub fn discard_queued_input(&mut self, input_id: &str, cx: &mut Context<Self>) {
        let Some(view) = self.session.as_mut() else {
            return;
        };
        let Some(rpc) = self.runtime.get(&view.agent_id).and_then(|r| r.rpc.clone()) else {
            return;
        };
        let Some(input) = view
            .pending_inputs
            .iter_mut()
            .find(|i| i.input_id == input_id && i.state == "queued" && !i.discard_pending)
        else {
            return;
        };
        input.discard_pending = true;
        let (agent_id, session_id, input_id) = (
            view.agent_id.clone(),
            view.session_id.clone(),
            input_id.to_string(),
        );
        let req = rpc.request(
            "_querymt/session/discardQueuedInput",
            json!({ "session_id": session_id, "input_id": input_id }),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = req.await;
            let _ = this.update(cx, |s, cx| {
                let Some(view) = s
                    .session
                    .as_mut()
                    .filter(|v| v.agent_id == agent_id && v.session_id == session_id)
                else {
                    return;
                };
                match result {
                    Ok(_) => view.set_input_state(&input_id, "queue", "discarded"),
                    Err(err) => {
                        if let Some(i) = view
                            .pending_inputs
                            .iter_mut()
                            .find(|i| i.input_id == input_id)
                        {
                            i.discard_pending = false;
                        }
                        view.push_notice(
                            format!("Could not remove the queued message: {err}"),
                            true,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether the open session's agent can remove queued messages.
    pub fn can_discard_queued(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|v| self.runtime.get(&v.agent_id))
            .is_some_and(|rt| rt.supports_method("querymt/session/discardQueuedInput"))
    }

    /// Change a session config option (`session/set_config_option`); an empty
    /// `config_id` means the mode of an agent that only has `session/set_mode`.
    /// Change a session config option; for `model`, `value` is a selection
    /// key ([`model_key`]), so the model on the chosen machine is sent.
    pub fn set_session_option(&mut self, config_id: &str, value: &str, cx: &mut Context<Self>) {
        let Some(view) = self.session.as_mut() else {
            return;
        };
        let Some(rt) = self.runtime.get(&view.agent_id) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone() else { return };
        let (agent_id, session_id) = (view.agent_id.clone(), view.session_id.clone());
        let key = if config_id.is_empty() {
            "mode".to_string()
        } else {
            config_id.to_string()
        };
        // The model by selection key: its id goes on the wire, its catalog
        // entry (with the node) in `_meta`.
        let model = (config_id == "model")
            .then(|| find_model(&rt.models, value))
            .flatten()
            .cloned();
        let wire = model.as_ref().map_or(value, |m| m.id.as_str()).to_string();
        view.pending_options.insert(key.clone());
        let req = if config_id.is_empty() {
            rpc.request(
                "session/set_mode",
                json!({ "sessionId": session_id, "modeId": wire }),
            )
        } else {
            let mut params =
                json!({ "sessionId": session_id, "configId": config_id, "value": wire });
            if let Some(model) = &model {
                let mut entry = model.raw.clone();
                if let Some(obj) = entry.as_object_mut() {
                    obj.remove("availability");
                }
                params["_meta"] = json!({ "querymt": { "modelEntry": entry } });
            }
            rpc.request("session/set_config_option", params)
        };
        let config_id = config_id.to_string();
        if let Some(model) = &model {
            self.remember_model(&agent_id, &model.key());
            self.save();
        }
        let node = model.and_then(|m| m.node_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = req.await;
            let _ =
                this.update(cx, |s, cx| {
                    let Some(view) = s
                        .session
                        .as_mut()
                        .filter(|v| v.agent_id == agent_id && v.session_id == session_id)
                    else {
                        return;
                    };
                    view.pending_options.remove(&key);
                    match result {
                        Ok(resp) => {
                            if config_id.is_empty() {
                                view.mode = Some(wire.clone());
                            } else if resp.get("configOptions").is_some() {
                                view.apply_config(resp.get("configOptions"));
                                // A confirmation without the model entry names
                                // the model but not its machine: keep the choice.
                                if config_id == "model"
                                    && !view.model_has_entry()
                                    && view.model.as_deref() == Some(wire.as_str())
                                {
                                    view.model_node = node.clone();
                                }
                            } else {
                                if let Some(o) = view.options.iter_mut().find(|o| {
                                    o.get("id").and_then(Value::as_str) == Some(&config_id)
                                }) {
                                    o["currentValue"] = Value::String(wire.clone());
                                }
                                match config_id.as_str() {
                                    "model" => {
                                        view.model = Some(wire.clone());
                                        view.model_node = node.clone();
                                    }
                                    "mode" => view.mode = Some(wire.clone()),
                                    _ => {}
                                }
                            }
                        }
                        Err(err) => view.push_notice(
                            format!(
                                "Could not change {}: {err}",
                                if config_id.is_empty() {
                                    "mode"
                                } else {
                                    &config_id
                                }
                            ),
                            true,
                        ),
                    }
                    cx.notify();
                });
        })
        .detach();
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

    /// `session/cancel` for the open session's running turn.
    pub fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.session.as_ref() else {
            return;
        };
        if let Some(rpc) = self.runtime.get(&view.agent_id).and_then(|r| r.rpc.clone()) {
            rpc.notify("session/cancel", json!({ "sessionId": view.session_id }));
        }
        // Pending prompts of this session are moot now.
        let (agent, sid) = (view.agent_id.clone(), view.session_id.clone());
        let keys: Vec<(String, bool)> = self
            .requests
            .iter()
            .filter(|r| {
                r.answerable()
                    && r.agent_id == agent
                    && r.session_id.as_deref() == Some(sid.as_str())
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

    fn finish_prompt(
        &mut self,
        agent_id: &str,
        session_id: &str,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .session
            .as_mut()
            .filter(|v| v.agent_id == agent_id && v.session_id == session_id)
        {
            view.prompting = false;
            view.sync_work(crate::util::now_ms());
            match &result {
                Err(err) => view.push_notice(format!("Prompt failed: {err}"), true),
                Ok(v) => match v.get("stopReason").and_then(Value::as_str) {
                    Some("cancelled") => view.push_notice("Stopped.", false),
                    Some("max_tokens") => {
                        view.push_notice("Stopped: the model hit its token limit.", true)
                    }
                    Some("refusal") => view.push_notice("The model refused to continue.", true),
                    _ => {}
                },
            }
        }
        if let (Err(err), Some(rt)) = (&result, self.runtime.get_mut(agent_id)) {
            rt.log(format!("Prompt failed: {err}"));
        }
        // A finished turn adds an undo target and drops stale redo frames.
        if self
            .session
            .as_ref()
            .is_some_and(|v| v.agent_id == agent_id && v.session_id == session_id)
        {
            self.refresh_undo_stack(cx);
        }
        self.schedule_sessions_refresh(agent_id, Duration::from_millis(300), cx);
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

    pub fn clear_launch_error(&mut self, cx: &mut Context<Self>) {
        self.launch.error = None;
        cx.notify();
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
        rt.log(format!("Error: {reason}"));
        rt.state = ConnState::Failed(reason);
        rt.busy = false;
        self.retire_requests(id);
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
        rt.log(format!(
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
                if method == "session/update" {
                    let session_id = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(view) = self
                        .session
                        .as_mut()
                        .filter(|v| v.agent_id == id && v.session_id == session_id)
                        && let Some(update) = params.get("update")
                    {
                        view.apply(update);
                        cx.notify();
                    }
                }
                if method.ends_with("session/delegationUpdate") {
                    let sid = params
                        .get("sessionId")
                        .or_else(|| params.get("session_id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(view) = self
                        .session
                        .as_mut()
                        .filter(|v| v.agent_id == id && v.session_id == sid)
                    {
                        view.apply_child_links(std::slice::from_ref(&params));
                        cx.notify();
                    }
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
                    if let Some(view) = self
                        .session
                        .as_mut()
                        .filter(|v| v.agent_id == id && v.session_id == sid)
                    {
                        if let Some(rt) = params.get("runtime") {
                            view.apply_runtime(rt);
                        }
                        view.sync_work(crate::util::now_ms());
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
                if method.ends_with("session/inputState") {
                    let sid = params
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(view) = self
                        .session
                        .as_mut()
                        .filter(|v| v.agent_id == id && v.session_id == sid)
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
                        if view.pending_inputs.iter().any(|i| i.input_id == input_id) {
                            view.set_input_state(&input_id, &delivery, &state);
                        } else {
                            // Its submit response hasn't arrived yet (or the id
                            // is still the client's); apply it when it does.
                            view.input_notes.insert(input_id, (delivery, state));
                        }
                        cx.notify();
                    }
                }
                if method
                    .trim_start_matches('_')
                    .starts_with("querymt/elicitation/")
                    || method == "elicitation/complete"
                {
                    self.on_elicitation_notification(id, &method, &params);
                    cx.notify();
                }
                if method.contains("runtimeChanged")
                    || method.contains("lifecycle")
                    || method.ends_with("session/update")
                {
                    self.schedule_sessions_refresh(id, Duration::from_millis(1200), cx);
                } else if method.trim_start_matches('_') == "querymt/schedules/changed" {
                    if self
                        .live(id, generation)
                        .is_some_and(|rt| rt.schedules.loaded || rt.schedules.loading)
                    {
                        self.refresh_schedules(id, cx);
                    }
                } else if method.ends_with("session/delegateModelsChanged") {
                    let sid = params
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if self
                        .session
                        .as_ref()
                        .is_some_and(|v| v.agent_id == id && v.session_id == sid)
                    {
                        self.refresh_delegates(cx);
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
        Err(err) => rt.log(format!("Capabilities unavailable: {err}")),
    });
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
            (Err(err), None) => rt.log(format!("Models unavailable: {err}")),
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
            rt.log(format!("Sessions unavailable: {err}"));
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
                    .read_with(&cx2, |s, _| s.session.as_ref().is_some_and(|v| v.loading))
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
            rt.log(format!("Older sessions unavailable: {err}"));
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
fn load_error_report(
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
        version = env!("CARGO_PKG_VERSION"),
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
fn prompt_content(
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
                let flag = |p: &Value, k: &str| p.get(k).and_then(Value::as_bool).unwrap_or(false);
                rt.providers = v
                    .get("providers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|p| {
                        let provider = text(p, "provider")?;
                        Some(ProviderAuth {
                            display_name: text(p, "display_name")
                                .unwrap_or_else(|| provider.clone()),
                            provider,
                            oauth_status: text(p, "oauth_status"),
                            has_stored_api_key: flag(p, "has_stored_api_key"),
                            has_env_api_key: flag(p, "has_env_api_key"),
                            env_var_name: text(p, "env_var_name"),
                            supports_oauth: flag(p, "supports_oauth"),
                            preferred_method: text(p, "preferred_method"),
                        })
                    })
                    .collect();
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
            session: None,
            requests: Vec::new(),
            runtime: HashMap::new(),
            first_run: false,
            _launch_task: None,
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
