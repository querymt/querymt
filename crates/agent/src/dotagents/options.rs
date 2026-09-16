//! Load options for `.agents` Protocol discovery.

use super::DotagentsTaskTrustPolicy;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How strictly the loader treats error diagnostics during resolution.
///
/// This controls whether an error diagnostic is fatal for the resolved
/// configuration as a whole, or whether unaffected valid entries remain
/// available. Runtime activation (see the runtime adapters) additionally uses
/// this to decide whether construction fails or continues with warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DotagentsStrictness {
    /// Activate valid entries, return structured warnings, and fail only when
    /// the selected root configuration cannot be applied safely.
    #[default]
    Compatibility,
    /// Fail resolution when any error diagnostic is present.
    Strict,
}

impl DotagentsStrictness {
    /// Whether this policy treats error diagnostics as fatal.
    pub fn is_strict(self) -> bool {
        matches!(self, DotagentsStrictness::Strict)
    }
}

/// Options controlling `.agents` Protocol discovery and parsing.
///
/// The default options are disabled, so callers must explicitly opt in before
/// any filesystem discovery occurs. Roots may be overridden for deterministic
/// embedding and tests, and layers may be disabled independently.
#[derive(Debug, Clone)]
pub struct DotagentsLoadOptions {
    /// Whether protocol loading is enabled. When `false`, the loader performs
    /// no discovery and produces an empty manifest.
    enabled: bool,

    /// Workspace used to derive the workspace layer root (`<workspace>/.agents/`).
    ///
    /// When `None`, no workspace layer is considered; the process working
    /// directory is never used implicitly.
    workspace: Option<PathBuf>,

    /// Override for the global layer root (defaults to `~/.agents/`).
    global_root: Option<PathBuf>,

    /// Override for the workspace layer root (defaults to
    /// `<workspace>/.agents/`).
    workspace_root: Option<PathBuf>,

    /// Whether the global layer is considered when enabled.
    global_enabled: bool,

    /// Whether the workspace layer is considered when enabled.
    workspace_enabled: bool,

    /// Selected model preset name from `models.json`.
    selected_model_preset: Option<String>,

    /// Additional explicitly trusted roots for confined file resolution.
    trusted_roots: Vec<PathBuf>,

    /// Diagnostic strictness policy.
    strictness: DotagentsStrictness,

    /// Trust policy for repository-controlled workspace repeat tasks.
    workspace_task_trust: DotagentsTaskTrustPolicy,
}

impl Default for DotagentsLoadOptions {
    fn default() -> Self {
        Self::disabled()
    }
}

impl DotagentsLoadOptions {
    /// Create disabled-by-default load options.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            workspace: None,
            global_root: None,
            workspace_root: None,
            global_enabled: true,
            workspace_enabled: true,
            selected_model_preset: None,
            trusted_roots: Vec::new(),
            strictness: DotagentsStrictness::default(),
            workspace_task_trust: DotagentsTaskTrustPolicy::Prompt,
        }
    }

    /// Create enabled load options with no workspace.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::disabled()
        }
    }

    /// Set the enabled flag.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Set the workspace used to derive the workspace layer.
    pub fn with_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Override the global layer root.
    pub fn with_global_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.global_root = Some(root.into());
        self
    }

    /// Override the workspace layer root.
    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    /// Enable or disable the global layer.
    pub fn with_global_enabled(mut self, enabled: bool) -> Self {
        self.global_enabled = enabled;
        self
    }

    /// Enable or disable the workspace layer.
    pub fn with_workspace_enabled(mut self, enabled: bool) -> Self {
        self.workspace_enabled = enabled;
        self
    }

    /// Select a named model preset from `models.json`.
    pub fn with_selected_model_preset(mut self, preset: impl Into<String>) -> Self {
        self.selected_model_preset = Some(preset.into());
        self
    }

    /// Set the diagnostic strictness policy.
    pub fn with_strictness(mut self, strictness: DotagentsStrictness) -> Self {
        self.strictness = strictness;
        self
    }

    /// Set the trust policy for repository-controlled workspace tasks.
    pub fn with_workspace_task_trust(mut self, policy: DotagentsTaskTrustPolicy) -> Self {
        self.workspace_task_trust = policy;
        self
    }

    /// Add an explicitly trusted root for confined resolution.
    pub fn with_trusted_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.trusted_roots.push(root.into());
        self
    }

    /// Whether protocol loading is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The effective workspace, if any.
    pub fn workspace(&self) -> Option<&std::path::Path> {
        self.workspace.as_deref()
    }

    /// Apply a workspace only when no workspace or workspace root is configured.
    ///
    /// This is the fallback used when threading a caller-supplied workspace
    /// (for example an agent's working directory) into protocol resolution. An
    /// explicit `workspace` or `workspace_root` always wins, so a caller that
    /// has already chosen its layer roots is never overridden.
    pub fn with_workspace_fallback(self, workspace: impl Into<PathBuf>) -> Self {
        if self.workspace.is_some() || self.workspace_root.is_some() {
            return self;
        }
        self.with_workspace(workspace)
    }

    /// The explicit global root override, if any.
    pub fn global_root_override(&self) -> Option<&std::path::Path> {
        self.global_root.as_deref()
    }

    /// The explicit workspace root override, if any.
    pub fn workspace_root_override(&self) -> Option<&std::path::Path> {
        self.workspace_root.as_deref()
    }

    /// Whether the global layer is considered when enabled.
    pub fn is_global_enabled(&self) -> bool {
        self.global_enabled
    }

    /// Whether the workspace layer is considered when enabled.
    pub fn is_workspace_enabled(&self) -> bool {
        self.workspace_enabled
    }

    /// The selected model preset name, if any.
    pub fn selected_model_preset(&self) -> Option<&str> {
        self.selected_model_preset.as_deref()
    }

    /// Explicitly trusted roots for confined resolution.
    pub fn trusted_roots(&self) -> &[PathBuf] {
        &self.trusted_roots
    }

    /// The diagnostic strictness policy.
    pub fn strictness(&self) -> DotagentsStrictness {
        self.strictness
    }

    /// Trust policy for repository-controlled workspace tasks.
    pub fn workspace_task_trust(&self) -> DotagentsTaskTrustPolicy {
        self.workspace_task_trust
    }
}
