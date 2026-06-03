use super::*;

// ============================================================================
// Compaction Configuration (3-Layer System)
// ============================================================================

/// Default maximum lines before truncation
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Default maximum bytes before truncation (50 KB)
pub const DEFAULT_MAX_BYTES: usize = 51200;

/// Default tokens to protect from pruning
pub const DEFAULT_PRUNE_PROTECT_TOKENS: usize = 40_000;

/// Default minimum tokens required before pruning
pub const DEFAULT_PRUNE_MINIMUM_TOKENS: usize = 20_000;

/// Default protected tools that should never be pruned
pub const DEFAULT_PROTECTED_TOOLS: &[&str] = &["skill"];

/// Default maximum retry attempts for compaction
pub const DEFAULT_MAX_RETRIES: usize = 3;

/// Default initial backoff in milliseconds
pub const DEFAULT_INITIAL_BACKOFF_MS: u64 = 1000;

/// Default backoff multiplier
pub const DEFAULT_BACKOFF_MULTIPLIER: f64 = 2.0;

fn default_max_lines() -> usize {
    DEFAULT_MAX_LINES
}

fn default_max_bytes() -> usize {
    DEFAULT_MAX_BYTES
}

fn default_prune_protect_tokens() -> usize {
    DEFAULT_PRUNE_PROTECT_TOKENS
}

fn default_prune_minimum_tokens() -> usize {
    DEFAULT_PRUNE_MINIMUM_TOKENS
}

fn default_protected_tools() -> Vec<String> {
    DEFAULT_PROTECTED_TOOLS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_max_retries() -> usize {
    DEFAULT_MAX_RETRIES
}

fn default_initial_backoff_ms() -> u64 {
    DEFAULT_INITIAL_BACKOFF_MS
}

fn default_backoff_multiplier() -> f64 {
    DEFAULT_BACKOFF_MULTIPLIER
}

/// Where to store overflow output when tool output is truncated
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverflowStorage {
    /// Discard overflow (don't save)
    Discard,
    /// Save to temp directory (/tmp/qmt-tool-outputs/{session_id}/)
    #[default]
    TempDir,
    /// Save to persistent data directory
    DataDir,
    // TODO: Database storage option for future implementation
}

/// Configuration for tool output truncation (Layer 1)
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputConfig {
    /// Maximum lines before truncation
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,

    /// Maximum bytes before truncation
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,

    /// Where to save full output when truncated
    #[serde(default)]
    pub overflow_storage: OverflowStorage,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            overflow_storage: OverflowStorage::default(),
        }
    }
}

/// Configuration for pruning (Layer 2) - runs after every turn
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PruningConfig {
    /// Enable/disable pruning
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Tokens of recent tool outputs to protect from pruning
    #[serde(default = "default_prune_protect_tokens")]
    pub protect_tokens: usize,

    /// Minimum tokens to clear before pruning (avoids small pruning operations)
    #[serde(default = "default_prune_minimum_tokens")]
    pub minimum_tokens: usize,

    /// Tools that should never be pruned
    #[serde(default = "default_protected_tools")]
    pub protected_tools: Vec<String>,
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            protect_tokens: DEFAULT_PRUNE_PROTECT_TOKENS,
            minimum_tokens: DEFAULT_PRUNE_MINIMUM_TOKENS,
            protected_tools: default_protected_tools(),
        }
    }
}

/// Retry configuration for compaction LLM calls
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    /// Maximum retry attempts
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,

    /// Initial backoff delay in milliseconds
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,

    /// Exponential backoff multiplier
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MS,
            backoff_multiplier: DEFAULT_BACKOFF_MULTIPLIER,
        }
    }
}

/// Configuration for AI compaction (Layer 3) - runs on context overflow
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    /// Enable/disable AI compaction (setting true auto-enables ContextMiddleware)
    #[serde(default = "default_true")]
    pub auto: bool,

    /// Optional: different provider for compaction (cheaper model)
    pub provider: Option<String>,

    /// Optional: different model for compaction (cheaper model)
    pub model: Option<String>,

    /// Retry configuration for compaction LLM calls
    #[serde(default)]
    pub retry: RetryConfig,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: true,
            provider: None,
            model: None,
            retry: RetryConfig::default(),
        }
    }
}

// ============================================================================
// End Compaction Configuration
// ============================================================================

// ============================================================================
// Rate Limit Configuration
// ============================================================================

/// Default maximum retry attempts for rate limiting
pub const DEFAULT_RATE_LIMIT_MAX_RETRIES: usize = 3;

/// Default wait time in seconds if no retry-after header
pub const DEFAULT_RATE_LIMIT_WAIT_SECS: u64 = 60;

/// Default backoff multiplier for rate limiting
pub const DEFAULT_RATE_LIMIT_BACKOFF_MULTIPLIER: f64 = 2.0;

/// Default max retries for mid-stream transport failures
pub const DEFAULT_STREAM_MAX_RETRIES: usize = 1;

fn default_rate_limit_max_retries() -> usize {
    DEFAULT_RATE_LIMIT_MAX_RETRIES
}

fn default_rate_limit_wait_secs() -> u64 {
    DEFAULT_RATE_LIMIT_WAIT_SECS
}

fn default_rate_limit_backoff_multiplier() -> f64 {
    DEFAULT_RATE_LIMIT_BACKOFF_MULTIPLIER
}

fn default_stream_max_retries() -> usize {
    DEFAULT_STREAM_MAX_RETRIES
}

/// Configuration for rate limit retry behavior
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Maximum number of retry attempts (default: 3)
    #[serde(default = "default_rate_limit_max_retries")]
    pub max_retries: usize,

    /// Default wait time in seconds if no retry-after header (default: 60)
    #[serde(default = "default_rate_limit_wait_secs")]
    pub default_wait_secs: u64,

    /// Backoff multiplier when no retry-after header (default: 2.0)
    /// Wait time increases exponentially: default_wait_secs * multiplier^(attempt-1)
    #[serde(default = "default_rate_limit_backoff_multiplier")]
    pub backoff_multiplier: f64,

    /// Max retries for mid-stream transport failures (default: 1).
    /// On each retry, accumulated text is discarded and the stream is re-created.
    #[serde(default = "default_stream_max_retries")]
    pub max_stream_retries: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_RATE_LIMIT_MAX_RETRIES,
            default_wait_secs: DEFAULT_RATE_LIMIT_WAIT_SECS,
            backoff_multiplier: DEFAULT_RATE_LIMIT_BACKOFF_MULTIPLIER,
            max_stream_retries: DEFAULT_STREAM_MAX_RETRIES,
        }
    }
}

// ============================================================================
// End Rate Limit Configuration
// ============================================================================

// ============================================================================
// Delegation Summary Configuration
// ============================================================================

/// Configuration for delegation summary LLM call
/// This generates an Implementation Brief from the parent planning conversation
/// before delegation to provide context to the coder agent
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegationSummaryConfig {
    /// LLM provider for the summary call (can be different from main agent)
    pub provider: String,

    /// Model for the summary call (should be cheap/fast, e.g., claude-haiku)
    pub model: String,

    /// API key override (optional, falls back to env)
    pub api_key: Option<String>,

    /// Enable/disable (default: true when config section present)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Maximum tokens for the summary (prevents runaway context consumption)
    #[serde(default = "default_summary_max_tokens")]
    pub max_tokens: Option<usize>,

    /// Timeout in seconds for the summarizer LLM call (default: 30)
    #[serde(default = "default_summary_timeout")]
    pub timeout_secs: u64,

    /// Minimum estimated tokens in parent history before triggering LLM summarization.
    /// Below this, raw formatted history is injected directly (no LLM call).
    /// Default: 2000 (~8000 chars / ~10-15 messages)
    #[serde(default = "default_min_history_tokens")]
    pub min_history_tokens: usize,
}

fn default_summary_max_tokens() -> Option<usize> {
    Some(2000)
}

fn default_summary_timeout() -> u64 {
    30
}

fn default_min_history_tokens() -> usize {
    2000
}

impl Default for DelegationSummaryConfig {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_string(),
            model: "claude-haiku".to_string(),
            api_key: None,
            enabled: true,
            max_tokens: default_summary_max_tokens(),
            timeout_secs: default_summary_timeout(),
            min_history_tokens: default_min_history_tokens(),
        }
    }
}

// ============================================================================
// End Delegation Summary Configuration
// ============================================================================

// ============================================================================
// Snapshot Backend Configuration
// ============================================================================

fn default_snapshot_backend() -> String {
    "none".to_string()
}

fn default_max_snapshots() -> Option<usize> {
    Some(100)
}

fn default_max_age_days() -> Option<u64> {
    Some(30)
}

/// Configuration for snapshot backend (undo/redo support).
///
/// Snapshots capture the state of modified files before each agent action,
/// enabling undo/redo. Requires the `[agent.execution.snapshot]` section.
///
/// ```toml
/// [agent.execution.snapshot]
/// backend = "git"
/// max_snapshots = 100
/// max_age_days = 30
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBackendConfig {
    /// Snapshot storage backend.
    /// - `"git"`: Commits snapshots into the current git repository.
    /// - `"none"`: Snapshots disabled (default).
    #[serde(default = "default_snapshot_backend")]
    #[schemars(extend("enum" = ["git", "none"]))]
    pub backend: String,

    /// Maximum number of snapshots to retain. Oldest are removed first.
    #[serde(default = "default_max_snapshots")]
    pub max_snapshots: Option<usize>,

    /// Maximum age of snapshots in days. Older snapshots are pruned automatically.
    #[serde(default = "default_max_age_days")]
    pub max_age_days: Option<u64>,
}

impl Default for SnapshotBackendConfig {
    fn default() -> Self {
        Self {
            backend: default_snapshot_backend(),
            max_snapshots: default_max_snapshots(),
            max_age_days: default_max_age_days(),
        }
    }
}

// ============================================================================
// End Snapshot Backend Configuration
// ============================================================================

// ============================================================================
// ExecutionPolicy — groups the 5 execution-policy configs shared across
// AgentSettings, PlannerConfig, and DelegateConfig.
// ============================================================================

/// Execution-policy configuration (3-layer context management system).
///
/// - **Layer 1** `tool_output`: Truncates individual tool outputs exceeding
///   size limits. Saves overflowed content to temp storage.
/// - **Layer 2** `pruning`: Removes old tool output entries from the context
///   window after every turn to reclaim token budget.
/// - **Layer 3** `compaction`: AI-powered summarisation triggered when the
///   context window fills. Condenses history to free space.
///
/// Also controls `snapshot` (undo/redo via git) and `rate_limit` (429 retry).
///
/// ```toml
/// [agent.execution.tool_output]
/// max_lines = 2000
/// max_bytes = 51200
///
/// [agent.execution.pruning]
/// protect_tokens = 40000
///
/// [agent.execution.compaction]
/// auto = true
///
/// [agent.execution.snapshot]
/// backend = "git"
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ExecutionPolicy {
    /// Tool output truncation settings (Layer 1)
    pub tool_output: ToolOutputConfig,
    /// Pruning settings — runs after every turn (Layer 2)
    pub pruning: PruningConfig,
    /// AI compaction settings — runs on context overflow (Layer 3)
    pub compaction: CompactionConfig,
    /// Snapshot backend for undo/redo support
    pub snapshot: SnapshotBackendConfig,
    /// Rate limit retry configuration
    pub rate_limit: RateLimitConfig,
}

/// Runtime execution policy — the 4 configs that survive to `AgentConfig`
/// (excludes `SnapshotBackendConfig` which is consumed at build time).
#[derive(Debug, Clone, Default)]
pub struct RuntimeExecutionPolicy {
    pub tool_output: ToolOutputConfig,
    pub pruning: PruningConfig,
    pub compaction: CompactionConfig,
    pub rate_limit: RateLimitConfig,
}

impl From<&ExecutionPolicy> for RuntimeExecutionPolicy {
    fn from(ep: &ExecutionPolicy) -> Self {
        Self {
            tool_output: ep.tool_output.clone(),
            pruning: ep.pruning.clone(),
            compaction: ep.compaction.clone(),
            rate_limit: ep.rate_limit.clone(),
        }
    }
}

// ============================================================================
