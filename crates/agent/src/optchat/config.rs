use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Experimental session-local tree memory. No files or workers exist when disabled.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MemorySettings {
    /// Opt in to tree memory; false leaves normal agent behavior untouched.
    pub enabled: bool,
    /// Currently only `optchat` is supported.
    pub strategy: String,
    /// Currently only `session` is supported.
    pub scope: String,
    /// Separate experimental storage; never the normal sessions database.
    pub directory: Option<PathBuf>,
    /// Target view size after a merge batch (UTF-8 bytes).
    pub view_min_bytes: usize,
    /// View size that triggers a merge batch (UTF-8 bytes).
    pub view_max_bytes: usize,
    /// Maximum UTF-8 bytes in one summary.
    pub summary_bytes: usize,
    /// Shared concurrent summarization requests, across this agent's sessions.
    pub concurrency: usize,
    /// Maximum model attempts to fit a summary into its byte budget.
    pub shortening_attempts: usize,
    /// Timeout for a summary, including shortening attempts, after admission.
    pub request_timeout_secs: u64,
    /// Overall catch-up timeout, including waiting for another session's slots.
    pub catch_up_timeout_secs: u64,
    /// Stop safely before a single turn exceeds this many estimated input bytes.
    pub max_turn_bytes: usize,
    /// Optional independently constructed summarizer; absent means the session model.
    pub compactor: Option<CompactorSettings>,
}

/// Optional independent provider/model for background summaries.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactorSettings {
    /// Provider identifier resolved through the normal provider registry.
    pub provider: String,
    /// Model used for summaries, independently of the foreground model.
    pub model: String,
    /// Provider parameters; may contain credentials and are redacted in Debug.
    #[serde(default)]
    pub parameters: serde_json::Map<String, serde_json::Value>,
}

impl std::fmt::Debug for CompactorSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactorSettings")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("parameters", &"[redacted]")
            .finish()
    }
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: "optchat".into(),
            scope: "session".into(),
            directory: None,
            view_min_bytes: 64_000,
            view_max_bytes: 128_000,
            summary_bytes: 512,
            concurrency: 8,
            shortening_attempts: 5,
            request_timeout_secs: 60,
            catch_up_timeout_secs: 180,
            max_turn_bytes: 256_000,
            compactor: None,
        }
    }
}

impl MemorySettings {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        ensure!(
            self.strategy == "optchat",
            "agent.memory.strategy must be optchat"
        );
        ensure!(
            self.scope == "session",
            "experimental OptChat supports only session scope"
        );
        ensure!(
            self.summary_bytes >= 512 && self.summary_bytes <= 4096,
            "summary_bytes must be between 512 and 4096"
        );
        ensure!(
            self.view_min_bytes >= self.summary_bytes * 2 + 128,
            "view_min_bytes must fit two summary lines"
        );
        ensure!(
            self.view_max_bytes > self.view_min_bytes,
            "view_max_bytes must exceed view_min_bytes"
        );
        ensure!(
            (1..=8).contains(&self.concurrency),
            "memory concurrency must be between 1 and 8"
        );
        ensure!(
            (1..=5).contains(&self.shortening_attempts),
            "shortening_attempts must be between 1 and 5"
        );
        ensure!(
            self.request_timeout_secs > 0 && self.catch_up_timeout_secs > 0,
            "memory timeouts must be positive"
        );
        ensure!(
            self.max_turn_bytes >= 1024,
            "max_turn_bytes must be at least 1024"
        );
        if let Some(directory) = &self.directory {
            ensure!(
                !directory.as_os_str().is_empty(),
                "memory directory must not be empty"
            );
        }
        if let Some(compactor) = &self.compactor {
            ensure!(
                !compactor.provider.trim().is_empty() && !compactor.model.trim().is_empty(),
                "compactor provider and model must not be empty"
            );
            ensure!(
                !compactor.parameters.contains_key("system")
                    && !compactor.parameters.contains_key("tools"),
                "compactor parameters cannot override system or tools"
            );
        }
        Ok(())
    }
}
