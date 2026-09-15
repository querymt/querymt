//! Serializable `.agents` Protocol settings shared by TOML and builders.

use super::{DotagentsLoadOptions, DotagentsStrictness, DotagentsTaskTrustPolicy};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Opt-in protocol configuration.
///
/// The default is disabled, preserving all existing TOML-only and programmatic
/// behavior. Roots and workspace are only consulted when `enabled = true`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DotagentsSettings {
    /// Enable `.agents` discovery and loading.
    pub enabled: bool,
    /// Workspace used to derive `<workspace>/.agents`.
    pub workspace: Option<PathBuf>,
    /// Explicit global protocol root override.
    pub global_root: Option<PathBuf>,
    /// Explicit workspace protocol root override.
    pub workspace_root: Option<PathBuf>,
    /// Whether the global layer is enabled.
    #[serde(default = "default_true")]
    pub global_enabled: bool,
    /// Whether the workspace layer is enabled.
    #[serde(default = "default_true")]
    pub workspace_enabled: bool,
    /// Selected `models.json` preset.
    pub selected_model_preset: Option<String>,
    /// Additional roots trusted by confined resolution.
    pub trusted_roots: Vec<PathBuf>,
    /// Error handling policy.
    pub strictness: DotagentsStrictness,
    /// Trust policy for repository-controlled workspace repeat tasks.
    pub workspace_task_trust: DotagentsTaskTrustPolicy,
}

fn default_true() -> bool {
    true
}

impl Default for DotagentsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            workspace: None,
            global_root: None,
            workspace_root: None,
            global_enabled: true,
            workspace_enabled: true,
            selected_model_preset: None,
            trusted_roots: Vec::new(),
            strictness: DotagentsStrictness::Compatibility,
            workspace_task_trust: DotagentsTaskTrustPolicy::Prompt,
        }
    }
}

impl DotagentsSettings {
    /// Convert serializable settings to runtime load options.
    pub fn load_options(&self) -> DotagentsLoadOptions {
        let mut options = DotagentsLoadOptions::disabled()
            .with_enabled(self.enabled)
            .with_global_enabled(self.global_enabled)
            .with_workspace_enabled(self.workspace_enabled)
            .with_strictness(self.strictness)
            .with_workspace_task_trust(self.workspace_task_trust);
        if let Some(workspace) = &self.workspace {
            options = options.with_workspace(workspace);
        }
        if let Some(root) = &self.global_root {
            options = options.with_global_root(root);
        }
        if let Some(root) = &self.workspace_root {
            options = options.with_workspace_root(root);
        }
        if let Some(preset) = &self.selected_model_preset {
            options = options.with_selected_model_preset(preset);
        }
        for root in &self.trusted_roots {
            options = options.with_trusted_root(root);
        }
        options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_with_both_layers_available_when_enabled() {
        let settings = DotagentsSettings::default();
        assert!(!settings.enabled);
        assert!(settings.global_enabled);
        assert!(settings.workspace_enabled);
        assert_eq!(settings.strictness, DotagentsStrictness::Compatibility);
        assert_eq!(
            settings.workspace_task_trust,
            DotagentsTaskTrustPolicy::Prompt
        );
        assert!(!settings.load_options().is_enabled());
    }

    #[test]
    fn toml_round_trip_preserves_all_settings() {
        let input = r#"
enabled = true
workspace = "/workspace"
global_root = "/global"
workspace_root = "/workspace/.agents"
global_enabled = false
workspace_enabled = true
selected_model_preset = "fast"
trusted_roots = ["/shared"]
strictness = "strict"
workspace_task_trust = "deny"
"#;
        let settings: DotagentsSettings = toml::from_str(input).unwrap();
        let encoded = toml::to_string(&settings).unwrap();
        let decoded: DotagentsSettings = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, settings);

        let options = settings.load_options();
        assert!(options.is_enabled());
        assert!(!options.is_global_enabled());
        assert_eq!(options.selected_model_preset(), Some("fast"));
        assert_eq!(options.strictness(), DotagentsStrictness::Strict);
        assert_eq!(
            settings.workspace_task_trust,
            DotagentsTaskTrustPolicy::Deny
        );
        assert_eq!(
            options.workspace_task_trust(),
            DotagentsTaskTrustPolicy::Deny
        );
    }
}
