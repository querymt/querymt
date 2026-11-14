use crate::worktree::domain::BaselineMode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Managed Git worktree workspace settings (`[agent.workspace]` /
/// `[quorum.workspace]`).
///
/// Settings only *enable the capability* and tune how a requested worktree is
/// built. Isolation itself stays an explicit per-request decision: an option-less
/// session is local whether or not the service is enabled.
///
/// ```toml
/// [agent.workspace]
/// enabled = true
/// # storage_root = "~/.qmt/worktrees"   # relative paths resolve against the config file
/// # baseline = "commit"                 # "commit" | "current_working_tree"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceSettings {
    /// Build a workspace service. Default `false`: no service, behavior unchanged.
    pub enabled: bool,

    /// Durable storage root for managed checkouts. Defaults to
    /// `<qmt config dir>/worktrees` (`~/.qmt/worktrees`). A leading `~/` is
    /// expanded; relative paths resolve against the config file.
    pub storage_root: Option<PathBuf>,

    /// Default baseline for explicitly requested top-level isolation that does
    /// not choose one itself. It never decides *whether* to isolate. Delegates
    /// always start from the parent's working contents.
    pub baseline: BaselineMode,
}

impl WorkspaceSettings {
    /// Settings with the service enabled and every other value defaulted.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Resolve a relative `storage_root` against the directory of the config
    /// file it was written in.
    #[doc(hidden)]
    pub fn anchored_at(mut self, config_dir: &Path) -> Self {
        if let Some(root) = self.storage_root.take() {
            self.storage_root = Some(
                if root.is_relative() && !starts_with_tilde(root.as_path()) {
                    config_dir.join(root)
                } else {
                    root
                },
            );
        }
        self
    }
}

fn starts_with_tilde(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|c| c.as_os_str() == "~")
}
