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

/// Default total request-attempt budget, including the initial request
pub const DEFAULT_RATE_LIMIT_MAX_RETRIES: usize = 3;

/// Default wait time in seconds if no retry-after header
pub const DEFAULT_RATE_LIMIT_WAIT_SECS: u64 = 60;

/// Default backoff multiplier for rate limiting
pub const DEFAULT_RATE_LIMIT_BACKOFF_MULTIPLIER: f64 = 2.0;

/// Default stream-recreation cap within the shared request-attempt budget.
pub const DEFAULT_STREAM_MAX_RETRIES: usize = 2;

/// Default proportional jitter applied to calculated backoff delays
pub const DEFAULT_RATE_LIMIT_JITTER_RATIO: f64 = 0.2;

/// Default upper bound for calculated retry delays
pub const DEFAULT_RATE_LIMIT_MAX_WAIT_SECS: u64 = 300;

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

fn default_rate_limit_jitter_ratio() -> f64 {
    DEFAULT_RATE_LIMIT_JITTER_RATIO
}

fn default_rate_limit_max_wait_secs() -> u64 {
    DEFAULT_RATE_LIMIT_MAX_WAIT_SECS
}

fn default_task_completion_guard() -> bool {
    false
}

/// Configuration for rate limit retry behavior
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Total request-attempt budget, including the initial request (default: 3)
    #[serde(default = "default_rate_limit_max_retries")]
    pub max_retries: usize,

    /// Default wait time in seconds if no retry-after header (default: 60)
    #[serde(default = "default_rate_limit_wait_secs")]
    pub default_wait_secs: u64,

    /// Backoff multiplier when no retry-after header (default: 2.0)
    /// Wait time increases exponentially: default_wait_secs * multiplier^(attempt-1)
    /// Must be finite and `> 0`.
    #[serde(default = "default_rate_limit_backoff_multiplier")]
    pub backoff_multiplier: f64,

    /// Maximum stream recreations within the shared request-attempt budget (default: 2).
    /// Each recreation restarts the provider request and also consumes `max_retries`.
    #[serde(default = "default_stream_max_retries")]
    pub max_stream_retries: usize,

    /// Proportional jitter for calculated delays (`0.0..=1.0`). Provider retry
    /// hints are not jittered.
    ///
    /// Runtime-only for now: not written into `session_execution_configs` so older
    /// qmtcode builds (strict `deny_unknown_fields`) can still open sessions.
    /// TODO(session-compat): drop `skip_serializing` and persist once older readers
    /// are retired, or gate behind a session config migration.
    #[serde(default = "default_rate_limit_jitter_ratio", skip_serializing)]
    pub jitter_ratio: f64,

    /// Upper bound for every retry delay (`>= 1`).
    ///
    /// Provider `Retry-After` / message hints are clamped by this field before
    /// sleeping or emitting retry-wait events.
    ///
    /// Runtime-only for now — same session-compat rationale as `jitter_ratio`.
    /// TODO(session-compat): persist with `jitter_ratio` when ready.
    #[serde(default = "default_rate_limit_max_wait_secs", skip_serializing)]
    pub max_wait_secs: u64,
}

impl RateLimitConfig {
    /// Total request-attempt budget, clamped to at least one attempt.
    /// This is the single clamp site — call `max_attempts()`, never
    /// `max_retries.max(1)` inline.
    pub fn max_attempts(&self) -> usize {
        self.max_retries.max(1)
    }

    /// Validate numeric ranges. Used by deserialize and for programmatic configs.
    pub fn validate(&self) -> Result<(), String> {
        if !self.backoff_multiplier.is_finite() || self.backoff_multiplier <= 0.0 {
            return Err(format!(
                "rate_limit.backoff_multiplier must be finite and > 0, got {}",
                self.backoff_multiplier
            ));
        }
        if !self.jitter_ratio.is_finite() || !(0.0..=1.0).contains(&self.jitter_ratio) {
            return Err(format!(
                "rate_limit.jitter_ratio must be finite and in 0.0..=1.0, got {}",
                self.jitter_ratio
            ));
        }
        if self.max_wait_secs == 0 {
            return Err("rate_limit.max_wait_secs must be >= 1".into());
        }
        Ok(())
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_RATE_LIMIT_MAX_RETRIES,
            default_wait_secs: DEFAULT_RATE_LIMIT_WAIT_SECS,
            backoff_multiplier: DEFAULT_RATE_LIMIT_BACKOFF_MULTIPLIER,
            max_stream_retries: DEFAULT_STREAM_MAX_RETRIES,
            jitter_ratio: DEFAULT_RATE_LIMIT_JITTER_RATIO,
            max_wait_secs: DEFAULT_RATE_LIMIT_MAX_WAIT_SECS,
        }
    }
}

impl<'de> Deserialize<'de> for RateLimitConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default = "default_rate_limit_max_retries")]
            max_retries: usize,
            #[serde(default = "default_rate_limit_wait_secs")]
            default_wait_secs: u64,
            #[serde(default = "default_rate_limit_backoff_multiplier")]
            backoff_multiplier: f64,
            #[serde(default = "default_stream_max_retries")]
            max_stream_retries: usize,
            #[serde(default = "default_rate_limit_jitter_ratio")]
            jitter_ratio: f64,
            #[serde(default = "default_rate_limit_max_wait_secs")]
            max_wait_secs: u64,
        }

        let raw = Raw::deserialize(deserializer)?;
        let cfg = Self {
            max_retries: raw.max_retries,
            default_wait_secs: raw.default_wait_secs,
            backoff_multiplier: raw.backoff_multiplier,
            max_stream_retries: raw.max_stream_retries,
            jitter_ratio: raw.jitter_ratio,
            max_wait_secs: raw.max_wait_secs,
        };
        cfg.validate().map_err(serde::de::Error::custom)?;
        Ok(cfg)
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
/// Also controls `snapshot` (undo/redo via git), `rate_limit` (429 retry), and
/// whether an unfinished finite task forces an additional model request.
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
    /// Force one additional model request when a finite task remains active.
    #[serde(default = "default_task_completion_guard")]
    pub task_completion_guard: bool,
}

/// Runtime execution policy — the configs that survive to `AgentConfig`
/// (excludes `SnapshotBackendConfig` which is consumed at build time).
#[derive(Debug, Clone, Default)]
pub struct RuntimeExecutionPolicy {
    pub tool_output: ToolOutputConfig,
    pub pruning: PruningConfig,
    pub compaction: CompactionConfig,
    pub rate_limit: RateLimitConfig,
    pub task_completion_guard: bool,
}

impl From<&ExecutionPolicy> for RuntimeExecutionPolicy {
    fn from(ep: &ExecutionPolicy) -> Self {
        Self {
            tool_output: ep.tool_output.clone(),
            pruning: ep.pruning.clone(),
            compaction: ep.compaction.clone(),
            rate_limit: ep.rate_limit.clone(),
            task_completion_guard: ep.task_completion_guard,
        }
    }
}

// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_completion_guard_is_opt_in() {
        let default_policy = ExecutionPolicy::default();
        assert!(!default_policy.task_completion_guard);

        let policy: ExecutionPolicy = serde_json::from_value(json!({
            "task_completion_guard": true
        }))
        .expect("deserialize execution policy");
        assert!(policy.task_completion_guard);
        assert!(RuntimeExecutionPolicy::from(&policy).task_completion_guard);
    }

    #[test]
    fn rate_limit_default_allows_two_stream_recreations() {
        let config = RateLimitConfig::default();
        assert_eq!(config.max_attempts(), 3);
        assert_eq!(config.max_stream_retries, 2);
    }

    #[test]
    fn rate_limit_config_omits_runtime_only_fields_on_serialize() {
        let cfg = RateLimitConfig {
            jitter_ratio: 0.9,
            max_wait_secs: 42,
            ..Default::default()
        };

        let value = serde_json::to_value(&cfg).expect("serialize");
        let obj = value.as_object().expect("object");

        assert!(
            !obj.contains_key("jitter_ratio"),
            "jitter_ratio must stay runtime-only until session-compat is ready: {value}"
        );
        assert!(
            !obj.contains_key("max_wait_secs"),
            "max_wait_secs must stay runtime-only until session-compat is ready: {value}"
        );

        // Stable keys older qmtcode already understands.
        for key in [
            "max_retries",
            "default_wait_secs",
            "backoff_multiplier",
            "max_stream_retries",
        ] {
            assert!(obj.contains_key(key), "missing expected key {key}: {value}");
        }
    }

    #[test]
    fn rate_limit_config_deserializes_missing_runtime_only_fields_to_defaults() {
        let cfg: RateLimitConfig = serde_json::from_value(json!({
            "max_retries": 5,
            "default_wait_secs": 30,
            "backoff_multiplier": 1.5,
            "max_stream_retries": 2,
        }))
        .expect("deserialize without runtime-only fields");
        assert!((cfg.jitter_ratio - DEFAULT_RATE_LIMIT_JITTER_RATIO).abs() < f64::EPSILON);
        assert_eq!(cfg.max_wait_secs, DEFAULT_RATE_LIMIT_MAX_WAIT_SECS);
    }

    #[test]
    fn rate_limit_config_still_deserializes_runtime_only_fields_when_present() {
        // Blobs already written by earlier builds of this branch should still load.
        let cfg: RateLimitConfig = serde_json::from_value(json!({
            "max_retries": 5,
            "default_wait_secs": 30,
            "backoff_multiplier": 2.0,
            "max_stream_retries": 2,
            "jitter_ratio": 0.5,
            "max_wait_secs": 120,
        }))
        .expect("deserialize with runtime-only fields");
        assert!((cfg.jitter_ratio - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.max_wait_secs, 120);
    }

    #[test]
    fn rate_limit_config_rejects_jitter_ratio_out_of_range() {
        let err = serde_json::from_value::<RateLimitConfig>(json!({
            "jitter_ratio": 20.0,
        }))
        .expect_err("jitter_ratio > 1 must fail at parse");
        let msg = err.to_string();
        assert!(
            msg.contains("jitter_ratio"),
            "error should name the field: {msg}"
        );
    }

    #[test]
    fn rate_limit_config_rejects_negative_jitter_ratio() {
        let err = serde_json::from_value::<RateLimitConfig>(json!({
            "jitter_ratio": -0.1,
        }))
        .expect_err("negative jitter_ratio must fail at parse");
        assert!(err.to_string().contains("jitter_ratio"));
    }

    #[test]
    fn rate_limit_config_rejects_non_positive_backoff_multiplier() {
        let err = serde_json::from_value::<RateLimitConfig>(json!({
            "backoff_multiplier": 0.0,
        }))
        .expect_err("backoff_multiplier <= 0 must fail at parse");
        assert!(err.to_string().contains("backoff_multiplier"));
    }

    #[test]
    fn rate_limit_config_rejects_zero_max_wait_secs() {
        let err = serde_json::from_value::<RateLimitConfig>(json!({
            "max_wait_secs": 0,
        }))
        .expect_err("max_wait_secs == 0 must fail at parse");
        assert!(err.to_string().contains("max_wait_secs"));
    }

    #[test]
    fn rate_limit_config_accepts_boundary_jitter_ratio() {
        for ratio in [0.0, 1.0] {
            let cfg: RateLimitConfig = serde_json::from_value(json!({
                "jitter_ratio": ratio,
            }))
            .unwrap_or_else(|e| panic!("jitter_ratio={ratio} should be valid: {e}"));
            assert!((cfg.jitter_ratio - ratio).abs() < f64::EPSILON);
        }
    }
}
