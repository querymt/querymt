use super::*;

/// Hard implementation ceilings. Host settings can lower the defaults but can
/// never raise them past these values.
pub const REPOSITORY_KNOWLEDGE_MAX_RESPONSE_BYTES: usize = 64 * 1024;
pub const REPOSITORY_KNOWLEDGE_MAX_RESULT_LIMIT: usize = 50;
pub const REPOSITORY_KNOWLEDGE_MAX_DELIVERY_CARDS: usize = 4;
pub const REPOSITORY_KNOWLEDGE_MAX_DELIVERY_TOKENS: usize = 2_000;
pub const REPOSITORY_KNOWLEDGE_MAX_DELIVERY_BYTES: usize = 16 * 1024;

/// Portable repository knowledge settings (`[agent.repository_knowledge]` /
/// `[quorum.repository_knowledge]`).
///
/// The feature is **disabled by default**. Only host configuration can enable
/// it; a repository file (`.qmt/knowledge.toml`) selects *what* to index but can
/// never enable the feature, grant tools, or widen filesystem access.
///
/// ```toml
/// [agent.repository_knowledge]
/// enabled = true
/// # root = "."                  # explicit knowledge root (required for non-Git workspaces)
/// # max_files = 2000
/// # max_file_bytes = 524288
/// # max_total_bytes = 33554432
/// # max_response_bytes = 16384  # capped at 64 KiB
/// # force_hash = false          # hash every selected file on every refresh
///
/// [agent.repository_knowledge.delivery]
/// # Both automatic delivery stages are independently opt-in.
/// # orientation = false
/// # index_context = false
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RepositoryKnowledgeSettings {
    /// Build a repository knowledge service. Default `false`: no repository
    /// reads, writes, tools, or prompt changes.
    pub enabled: bool,

    /// Explicit knowledge root. When absent, the Git worktree root containing
    /// the session working directory is used. Relative paths resolve against
    /// the config file. Non-Git workspaces require this to be set.
    pub root: Option<PathBuf>,

    /// Host allowlist of roots repository knowledge may read from. When empty
    /// no extra restriction applies; when non-empty, a resolved root outside
    /// every listed directory is denied. Relative paths resolve against the
    /// config file.
    pub allowed_roots: Vec<PathBuf>,

    /// Maximum number of selected files indexed per refresh.
    pub max_files: usize,

    /// Maximum size in bytes of any single selected file.
    pub max_file_bytes: u64,

    /// Maximum total bytes indexed across all selected files.
    pub max_total_bytes: u64,

    /// Default serialized response budget in bytes for repository tools.
    /// Capped at 64 KiB.
    pub max_response_bytes: usize,

    /// Default result limit for `repo_knowledge_query`. Capped at 50.
    pub default_result_limit: usize,

    /// Hash every selected file during reconciliation, bypassing the
    /// metadata fast path (debugging aid).
    pub force_hash: bool,

    /// Include Git-ignored files matched by the manifest. Default `false`.
    pub allow_ignored: bool,

    /// Automatic delivery controls. Each stage is independently opt-in.
    pub delivery: RepositoryKnowledgeDeliverySettings,
}

impl Default for RepositoryKnowledgeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            root: None,
            allowed_roots: Vec::new(),
            max_files: 2_000,
            max_file_bytes: 512 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
            max_response_bytes: 16 * 1024,
            default_result_limit: 10,
            force_hash: false,
            allow_ignored: false,
            delivery: RepositoryKnowledgeDeliverySettings::default(),
        }
    }
}

/// Automatic delivery controls. Both stages default off and are independent of
/// each other and of tool availability.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RepositoryKnowledgeDeliverySettings {
    /// Prompt-triggered orientation cards before the first model request.
    pub orientation: bool,

    /// Knowledge-for-path context after successful built-in `index` calls.
    pub index_context: bool,

    /// Maximum cards delivered per turn (default 2, capped at 4).
    pub max_cards: usize,

    /// Maximum estimated tokens delivered per turn (default 600, capped at 2000).
    pub max_tokens: usize,

    /// Serialized hard ceiling per turn in bytes (default 4 KiB, capped at 16 KiB).
    pub max_bytes: usize,
}

impl Default for RepositoryKnowledgeDeliverySettings {
    fn default() -> Self {
        Self {
            orientation: false,
            index_context: false,
            max_cards: 2,
            max_tokens: 600,
            max_bytes: 4 * 1024,
        }
    }
}

impl RepositoryKnowledgeDeliverySettings {
    /// True when either automatic delivery stage is enabled.
    pub fn any_enabled(&self) -> bool {
        self.orientation || self.index_context
    }

    pub fn effective_max_cards(&self) -> usize {
        self.max_cards.min(REPOSITORY_KNOWLEDGE_MAX_DELIVERY_CARDS)
    }

    pub fn effective_max_tokens(&self) -> usize {
        self.max_tokens
            .min(REPOSITORY_KNOWLEDGE_MAX_DELIVERY_TOKENS)
    }

    pub fn effective_max_bytes(&self) -> usize {
        self.max_bytes.min(REPOSITORY_KNOWLEDGE_MAX_DELIVERY_BYTES)
    }
}

impl RepositoryKnowledgeSettings {
    /// Settings with the feature enabled and every other value defaulted.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Resolve a relative `root` against the directory of the config file it
    /// was written in.
    pub(crate) fn anchored_at(mut self, config_dir: &Path) -> Self {
        let anchor = |path: PathBuf| {
            if path.is_relative() {
                config_dir.join(path)
            } else {
                path
            }
        };
        self.root = self.root.take().map(anchor);
        self.allowed_roots = self.allowed_roots.drain(..).map(anchor).collect();
        self
    }

    /// Effective serialized response budget, never above the 64 KiB ceiling.
    pub fn effective_max_response_bytes(&self) -> usize {
        self.max_response_bytes
            .min(REPOSITORY_KNOWLEDGE_MAX_RESPONSE_BYTES)
    }

    /// Effective default result limit, within `1..=50`.
    pub fn effective_default_result_limit(&self) -> usize {
        self.default_result_limit
            .clamp(1, REPOSITORY_KNOWLEDGE_MAX_RESULT_LIMIT)
    }

    /// `Some(self)` only when the feature is enabled. Disabled settings must
    /// cause no repository reads, writes, or prompt changes, so callers use this
    /// to decide whether any repository service exists at all.
    pub fn if_enabled(&self) -> Option<&Self> {
        self.enabled.then_some(self)
    }

    /// Automatic delivery is only meaningful when the feature itself is
    /// enabled; delivery flags never enable the feature.
    pub fn orientation_active(&self) -> bool {
        self.enabled && self.delivery.orientation
    }

    pub fn index_context_active(&self) -> bool {
        self.enabled && self.delivery.index_context
    }
}
