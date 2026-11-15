//! Agents store: configured agents, their live ACP connections, and the data
//! the Start and Agents pages read (capabilities, models, sessions, mesh).
//!
//! Mirrors `querymt-desktop/src/lib/stores/agents.svelte.ts` at the scope of
//! those two pages. Views observe the entity and re-render on `cx.notify()`.

use std::{collections::HashMap, rc::Rc, time::Duration};

use gpui_kit::{AsyncApp, Context, Image, ImageFormat, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::rpc::{Incoming, RpcClient};
use crate::session::{SessionView, UndoFrame, UserAttachment};
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
    pub node: Option<String>,
    /// Context window in tokens (`_querymt/modelInfo`), when known.
    pub context: Option<u64>,
    /// Known input modalities (`text`, `image`, `pdf`, …); empty when unknown.
    pub modalities: Vec<String>,
    raw: Value,
}

impl Model {
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

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub agent_id: String,
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at: String,
    pub phase: String,
    pub root: bool,
    pub open: bool,
    pub resolved: bool,
    pub archived: bool,
    pub pinned: bool,
    pub messages: u64,
}

impl SessionSummary {
    pub fn running(&self) -> bool {
        !self.phase.is_empty() && self.phase != "idle"
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
fn new_session_params(cwd: &str, profile: &str) -> Value {
    let mut params = json!({ "cwd": cwd, "mcpServers": [] });
    if profile != DEFAULT_PROFILE {
        params["_meta"] = json!({ "querymt": { "profile_id": profile } });
    }
    params
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub kind: String,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct PermOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    Text,
    Number,
    Integer,
    Bool,
    Enum(Vec<(String, String)>),
}

#[derive(Clone, Debug)]
pub struct ElicitField {
    pub key: String,
    pub title: String,
    pub description: String,
    pub kind: FieldKind,
    pub required: bool,
    pub default: Option<Value>,
}

#[derive(Clone, Debug)]
pub enum RequestKind {
    Permission {
        title: String,
        summary: String,
        kind: String,
        options: Vec<PermOption>,
    },
    Elicitation {
        message: String,
        fields: Vec<ElicitField>,
        url: Option<String>,
    },
}

/// A permission or input request from an agent, waiting for the user.
#[derive(Clone, Debug)]
pub struct PendingRequest {
    pub key: String,
    pub agent_id: String,
    rpc_id: Value,
    pub session_id: Option<String>,
    pub elicitation_id: Option<String>,
    pub received_ms: f64,
    pub kind: RequestKind,
}

fn parse_request(
    agent_id: &str,
    rpc_id: Value,
    method: &str,
    params: &Value,
) -> Option<PendingRequest> {
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let kind = if method == "session/request_permission" {
        let tool = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let mut call = crate::session::SessionView::default();
        let mut update = tool.clone();
        if let Some(obj) = update.as_object_mut() {
            obj.insert("sessionUpdate".into(), Value::from("tool_call"));
        }
        call.apply(&update);
        let (title, summary, kind) = match call.items.first().map(|i| &i.kind) {
            Some(crate::session::ItemKind::Work(entries)) if !entries.is_empty() => {
                match &entries[0] {
                    crate::session::WorkEntry::Tool(c) => {
                        (c.title.clone(), c.summary(), c.kind.clone())
                    }
                    _ => (text(tool.get("title")), String::new(), String::new()),
                }
            }
            _ => (text(tool.get("title")), String::new(), String::new()),
        };
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|o| PermOption {
                id: text(o.get("optionId")),
                name: text(o.get("name")),
                kind: text(o.get("kind")),
            })
            .collect();
        RequestKind::Permission {
            title: if title.is_empty() {
                "Tool call".into()
            } else {
                title
            },
            summary,
            kind,
            options,
        }
    } else if method.contains("elicitation") {
        let schema = params.get("requestedSchema");
        let required: Vec<String> = schema
            .and_then(|s| s.get("required"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let mut fields = Vec::new();
        if let Some(props) = schema
            .and_then(|s| s.get("properties"))
            .and_then(Value::as_object)
        {
            for (key, prop) in props {
                let enum_values: Vec<String> = prop
                    .get("enum")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
                let one_of: Vec<(String, String)> = prop
                    .get("oneOf")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|o| {
                        let v = o.get("const")?.as_str()?.to_string();
                        Some((
                            v.clone(),
                            o.get("title")
                                .and_then(Value::as_str)
                                .unwrap_or(&v)
                                .to_string(),
                        ))
                    })
                    .collect();
                let names: Vec<String> = prop
                    .get("enumNames")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
                let kind = if !one_of.is_empty() {
                    FieldKind::Enum(one_of)
                } else if !enum_values.is_empty() {
                    FieldKind::Enum(
                        enum_values
                            .iter()
                            .enumerate()
                            .map(|(i, v)| (v.clone(), names.get(i).cloned().unwrap_or(v.clone())))
                            .collect(),
                    )
                } else {
                    match prop.get("type").and_then(Value::as_str) {
                        Some("boolean") => FieldKind::Bool,
                        Some("number") => FieldKind::Number,
                        Some("integer") => FieldKind::Integer,
                        _ => FieldKind::Text,
                    }
                };
                fields.push(ElicitField {
                    key: key.clone(),
                    title: prop
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(key)
                        .to_string(),
                    description: text(prop.get("description")),
                    kind,
                    required: required.contains(key),
                    default: prop.get("default").cloned(),
                });
            }
        }
        RequestKind::Elicitation {
            message: text(params.get("message")),
            fields,
            url: params.get("url").and_then(Value::as_str).map(str::to_owned),
        }
    } else {
        return None;
    };
    let elicitation_id = params
        .get("elicitationId")
        .or_else(|| params.pointer("/_meta/querymt/elicitation_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(PendingRequest {
        key: format!("{agent_id}:{rpc_id}"),
        agent_id: agent_id.to_string(),
        rpc_id,
        session_id,
        elicitation_id,
        received_ms: now_ms(),
        kind,
    })
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
    pub models: Vec<Model>,
    pub sessions: Vec<SessionSummary>,
    pub sessions_loaded: bool,
    pub mesh: Option<MeshStatus>,
    pub mesh_nodes: Vec<MeshNode>,
    pub mesh_invites: Vec<MeshInvite>,
    /// URL of the invite created most recently in this app session.
    pub last_invite: Option<String>,
    pub providers: Vec<ProviderAuth>,
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
    refresh_scheduled: bool,
}

impl AgentRuntime {
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
    /// Mouse-wheel smoothing preset (`off`, `snappy`, `normal`, `smooth`, `glide`).
    #[serde(default)]
    pub scroll_smoothing: String,
    /// Mouse-wheel motion curve (`ease_out`, `ease_in_out`).
    #[serde(default)]
    pub scroll_curve: String,
    /// Mouse-wheel inertia (`off`, `light`, `strong`).
    #[serde(default)]
    pub scroll_inertia: String,
    /// How image attachments are sent in prompts.
    #[serde(default)]
    pub image_send_mode: ImageSendMode,
    /// Shortcut that sends a prompt from the composers.
    #[serde(default)]
    pub send_shortcut: SendShortcut,
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
    _launch_task: Option<Task<()>>,
}

impl AgentsStore {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let persisted = load_persisted().unwrap_or_default();
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

    pub fn all_sessions(&self) -> Vec<&SessionSummary> {
        self.runtime
            .values()
            .filter(|r| r.is_connected())
            .flat_map(|r| r.sessions.iter())
            .collect()
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
            .and_then(|id| models.iter().find(|m| &m.id == id))
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

    pub fn recent_models(&self, agent_id: &str) -> Vec<&Model> {
        let models = self.models(agent_id);
        self.prefs
            .recent_models
            .get(agent_id)
            .into_iter()
            .flatten()
            .filter_map(|id| models.iter().find(|m| &m.id == id))
            .collect()
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
        if let Some(s) = rt.sessions.iter_mut().find(|s| s.id == session_id) {
            s.pinned = pinned;
        }
        cx.notify();
        let (agent_id, session_id) = (agent_id.to_string(), session_id.to_string());
        cx.spawn(async move |this, cx| {
            let result = rpc
                .request(
                    "_querymt/session/setPinned",
                    json!({ "session_id": session_id, "pinned": pinned }),
                )
                .await;
            if let Err(err) = result {
                let _ = this.update(cx, |s, cx| {
                    if let Some(rt) = s.runtime.get_mut(&agent_id) {
                        if let Some(sess) = rt.sessions.iter_mut().find(|x| x.id == session_id) {
                            sess.pinned = !pinned;
                        }
                        rt.log(format!(
                            "Could not {} session: {err}",
                            if pinned { "pin" } else { "unpin" }
                        ));
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn set_launch_model(&mut self, agent_id: &str, model_id: String, cx: &mut Context<Self>) {
        let recent = self
            .prefs
            .recent_models
            .entry(agent_id.to_string())
            .or_default();
        recent.retain(|id| id != &model_id);
        recent.insert(0, model_id.clone());
        recent.truncate(5);
        self.prefs
            .launch_models
            .insert(agent_id.to_string(), model_id);
        self.save();
        cx.notify();
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
        rt.state = ConnState::Connecting;
        rt.pid = None;
        rt.log(format!("Connecting to {}…", config.endpoint_label()));
        cx.notify();

        let id = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let conn = match transport::connect(config.target()).await {
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
                rt.state = ConnState::Connected;
                rt.connected_at = Some(now_ms());
                rt.log(format!(
                    "Initialized {} {}",
                    rt.agent_title.as_deref().unwrap_or("agent"),
                    rt.agent_version.as_deref().unwrap_or("")
                ));
                cx.notify();
            });
            load_details(this, id, generation, rpc, cx).await;
        });
        if let Some(rt) = self.runtime.get_mut(&config_id) {
            rt.tasks.push(task);
        }
    }

    pub fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.runtime.get_mut(id) {
            rt.generation += 1;
            if let Some(rpc) = rt.rpc.take() {
                rpc.close();
            }
            rt.tasks.clear();
            rt.state = ConnState::Idle;
            rt.pid = None;
            rt.log("Disconnected.");
            self.requests.retain(|r| r.agent_id != id);
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
        let params = new_session_params(&cwd, self.launch_profile(agent_id));
        let image_mode = self.prefs.image_send_mode;
        let mode = self.prefs.mode().to_string();
        let reasoning = self.prefs.reasoning().to_string();
        self.launch = LaunchState {
            busy: true,
            error: None,
            last_started: None,
        };
        self.session = None;
        self.set_cwd(cwd.clone());
        cx.notify();

        let agent_id = agent_id.to_string();
        self._launch_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<String, String> = async {
                let created = rpc.request("session/new", params).await?;
                let session_id = created
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or("Agent did not return a session id.")?
                    .to_string();
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
                            if let Err(err) = rpc.request("session/set_config_option", params).await {
                                log(format!("Could not set reasoning {reasoning}: {err}"), cx);
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
                                updated_at: String::new(),
                                phase: "running".into(),
                                root: true,
                                open: true,
                                resolved: false,
                                archived: false,
                                pinned: false,
                                messages: 1,
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
        cx.spawn(async move |this, cx| {
            let started = now_ms();
            let result = rpc.request_detailed("session/load", params).await;
            let elapsed_ms = now_ms() - started;
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
                s.refresh_undo_stack(cx);
                cx.notify();
            });
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
                        cx.notify();
                        s.reload_session(cx);
                        return;
                    }
                    Err(err) => view.history_error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Clear a failed older-history fetch and try again.
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
        let Some(view) = self.session.as_mut() else {
            return;
        };
        if view.loading || view.running() || view.undo_pending {
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
        view.pending_options.insert(key.clone());
        let req = if config_id.is_empty() {
            rpc.request(
                "session/set_mode",
                json!({ "sessionId": session_id, "modeId": value }),
            )
        } else {
            let mut params =
                json!({ "sessionId": session_id, "configId": config_id, "value": value });
            if config_id == "model"
                && let Some(model) = rt.models.iter().find(|m| m.id == value)
            {
                let mut entry = model.raw.clone();
                if let Some(obj) = entry.as_object_mut() {
                    obj.remove("availability");
                }
                params["_meta"] = json!({ "querymt": { "modelEntry": entry } });
            }
            rpc.request("session/set_config_option", params)
        };
        let (config_id, value) = (config_id.to_string(), value.to_string());
        if config_id == "model" {
            let recent = self
                .prefs
                .recent_models
                .entry(agent_id.clone())
                .or_default();
            recent.retain(|id| id != &value);
            recent.insert(0, value.clone());
            recent.truncate(5);
            self.save();
        }
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
                                view.mode = Some(value.clone());
                            } else if resp.get("configOptions").is_some() {
                                view.apply_config(resp.get("configOptions"));
                            } else {
                                if let Some(o) = view.options.iter_mut().find(|o| {
                                    o.get("id").and_then(Value::as_str) == Some(&config_id)
                                }) {
                                    o["currentValue"] = Value::String(value.clone());
                                }
                                match config_id.as_str() {
                                    "model" => view.model = Some(value.clone()),
                                    "mode" => view.mode = Some(value.clone()),
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
        if let Some(s) = rt.sessions.iter_mut().find(|s| s.id == session_id) {
            if let Some(r) = resolved {
                s.resolved = r;
            }
            if let Some(a) = archived {
                s.archived = a;
            }
            s.open = !s.resolved && !s.archived;
        }
        cx.notify();
        let mut params = json!({ "session_id": session_id });
        if let Some(r) = resolved {
            params["resolved"] = json!(r);
        }
        if let Some(a) = archived {
            params["archived"] = json!(a);
        }
        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = rpc.request("_querymt/session/setLifecycle", params).await;
            let _ = this.update(cx, |s, cx| {
                match result {
                    Err(err) => {
                        if let Some(rt) = s.runtime.get_mut(&agent_id) {
                            rt.log(format!("Could not update the session: {err}"));
                        }
                    }
                    Ok(_) => s.schedule_sessions_refresh(&agent_id, Duration::from_millis(200), cx),
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
        // Pending permission prompts of this session are moot now.
        let (agent, sid) = (view.agent_id.clone(), view.session_id.clone());
        let keys: Vec<String> = self
            .requests
            .iter()
            .filter(|r| r.agent_id == agent && r.session_id.as_deref() == Some(sid.as_str()))
            .map(|r| r.key.clone())
            .collect();
        for key in keys {
            self.respond_request(&key, json!({ "outcome": { "outcome": "cancelled" } }), cx);
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

    fn respond_request(&mut self, key: &str, result: Value, cx: &mut Context<Self>) {
        let Some(ix) = self.requests.iter().position(|r| r.key == key) else {
            return;
        };
        let req = self.requests.remove(ix);
        if let Some(rpc) = self.runtime.get(&req.agent_id).and_then(|r| r.rpc.clone()) {
            rpc.respond(req.rpc_id, Ok(result));
        }
        cx.notify();
    }

    pub fn answer_permission(&mut self, key: &str, option_id: &str, cx: &mut Context<Self>) {
        self.respond_request(
            key,
            json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
            cx,
        );
    }

    /// `action`: `accept` (with `content`), `decline` or `cancel`.
    pub fn answer_elicitation(
        &mut self,
        key: &str,
        action: &str,
        content: Option<Value>,
        cx: &mut Context<Self>,
    ) {
        let mut result = json!({ "action": action });
        if let Some(content) = content {
            result["content"] = content;
        }
        self.respond_request(key, result, cx);
    }

    pub fn session_summary(&self, agent_id: &str, session_id: &str) -> Option<&SessionSummary> {
        self.runtime
            .get(agent_id)?
            .sessions
            .iter()
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
        self.requests.retain(|r| r.agent_id != id);
        cx.notify();
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
                    Some(request) => {
                        self.requests.retain(|r| r.key != request.key);
                        self.requests.push(request);
                        cx.notify();
                    }
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
                if method.ends_with("elicitation/completed") || method == "elicitation/complete" {
                    let eid = params
                        .get("elicitation_id")
                        .or_else(|| params.get("elicitationId"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.requests
                        .retain(|r| r.agent_id != id || r.elicitation_id.as_deref() != Some(eid));
                    cx.notify();
                }
                if method.contains("runtimeChanged")
                    || method.contains("lifecycle")
                    || method.ends_with("session/update")
                {
                    self.schedule_sessions_refresh(id, Duration::from_millis(1200), cx);
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
            let sessions = fetch_sessions(&rpc, &id, 1).await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&id, generation) else {
                    return;
                };
                rt.refresh_scheduled = false;
                if let Ok(fresh) = sessions {
                    let ids: std::collections::HashSet<String> =
                        fresh.iter().map(|s| s.id.clone()).collect();
                    let mut merged = fresh;
                    merged.extend(rt.sessions.drain(..).filter(|s| !ids.contains(&s.id)));
                    merged.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                    rt.sessions = merged;
                    rt.sessions_loaded = true;
                }
                cx.notify();
            });
        });
        rt.tasks.push(task);
        rt.tasks.retain(|t| !t.is_ready());
    }
}

async fn fetch_sessions(
    rpc: &RpcClient,
    agent_id: &str,
    max_pages: usize,
) -> Result<Vec<SessionSummary>, String> {
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..max_pages {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let page = rpc.request("session/list", params).await?;
        for s in page
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let text =
                |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
            let meta = s.get("_meta");
            let lifecycle = meta.and_then(|m| m.pointer("/querymt/lifecycle"));
            let flag = |k: &str| {
                lifecycle
                    .and_then(|l| l.get(k))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            };
            let pinned = meta
                .and_then(|m| m.pointer("/querymt/pin/pinned"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            sessions.push(SessionSummary {
                agent_id: agent_id.to_string(),
                id: text(s.get("sessionId")),
                cwd: text(s.get("cwd")),
                title: text(s.get("title")),
                updated_at: text(s.get("updatedAt")),
                phase: text(meta.and_then(|m| m.pointer("/runtimeStatus/phase"))),
                root: meta
                    .and_then(|m| m.get("parentSessionId"))
                    .is_none_or(Value::is_null),
                open: !flag("resolved") && !flag("archived"),
                resolved: flag("resolved"),
                archived: flag("archived"),
                pinned,
                messages: meta
                    .and_then(|m| m.get("messageCount"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
        }
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok(sessions)
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

    let caps = rpc.request("_querymt/capabilities", json!({})).await;
    let has_mesh = caps
        .as_ref()
        .ok()
        .and_then(|c| c.pointer("/features/mesh"))
        .and_then(Value::as_bool)
        == Some(true);
    apply!(|rt: &mut AgentRuntime| match caps {
        Ok(caps) => rt.capabilities = Some(caps),
        Err(err) => rt.log(format!("Capabilities unavailable: {err}")),
    });

    let models = rpc.request("_querymt/models", json!({})).await;
    let parsed = match &models {
        Ok(v) => Some(parse_models(v)),
        Err(_) => None,
    };
    apply!(|rt: &mut AgentRuntime| match (&models, parsed.clone()) {
        (_, Some(models)) => rt.models = models,
        (Err(err), None) => rt.log(format!("Models unavailable: {err}")),
        _ => {}
    });
    // Capabilities (context window, modalities) come from a second call; apply
    // them once they arrive so the list is usable immediately.
    if let Some(models) = parsed.filter(|m| !m.is_empty()) {
        let models = hydrate_model_info(&rpc, models).await;
        apply!(|rt: &mut AgentRuntime| rt.models = models);
    }

    // Up to 1000 sessions: enough for the Sessions page, including resolved/archived.
    let sessions = fetch_sessions(&rpc, &id, 4).await;
    apply!(|rt: &mut AgentRuntime| match sessions {
        Ok(sessions) => {
            rt.sessions = sessions;
            rt.sessions_loaded = true;
        }
        Err(err) => rt.log(format!("Sessions unavailable: {err}")),
    });

    load_extras(
        this.clone(),
        id.clone(),
        generation,
        rpc.clone(),
        has_mesh,
        cx,
    )
    .await;
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

fn update_options(options: &mut Vec<Value>, response: &Value) {
    let fresh = config_options(response);
    if !fresh.is_empty() {
        *options = fresh;
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
                rt.profiles = v
                    .get("profiles")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|p| {
                        let id = text(p, "id")?;
                        Some(Profile {
                            name: text(p, "name").unwrap_or_else(|| id.clone()),
                            id,
                            description: text(p, "description").unwrap_or_default(),
                            tags: strs(p.get("tags")),
                            kind: text(p, "config_kind").unwrap_or_default(),
                            source: text(p, "source").unwrap_or_default(),
                        })
                    })
                    .collect();
            }
        }),
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_params_send_only_non_default_profiles() {
        let plain = new_session_params("/w", DEFAULT_PROFILE);
        assert_eq!(plain, json!({ "cwd": "/w", "mcpServers": [] }));
        let bound = new_session_params("/w", "review");
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
}
