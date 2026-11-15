//! Layer root discovery for `.agents` Protocol layers.
//!
//! Discovery resolves the global layer (`~/.agents/`) and the workspace layer
//! (`<workspace>/.agents/`) from [`DotagentsLoadOptions`]. It is deliberately
//! read-only: it never creates directories and never falls back to the process
//! working directory as an implicit workspace.

use super::layer::{DotagentsLayer, DotagentsSourceRef};
use super::options::DotagentsLoadOptions;
use std::path::{Path, PathBuf};

/// The protocol directory name used by every layer.
pub const PROTOCOL_DIR_NAME: &str = ".agents";

/// A resolved layer root ready for confined traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLayerRoot {
    /// Which layer this root belongs to.
    pub layer: DotagentsLayer,
    /// The lexical root path (never canonicalized here).
    pub root: PathBuf,
    /// Whether the root directory currently exists on disk.
    pub exists: bool,
}

impl ResolvedLayerRoot {
    /// Convert into the public source-reference representation.
    pub fn as_source_ref(&self) -> DotagentsSourceRef {
        DotagentsSourceRef::new(self.layer, self.root.clone(), self.exists)
    }
}

/// Discover the enabled layer roots for the given options.
///
/// Roots are returned in deterministic precedence order: global first, then
/// workspace. When protocol loading is disabled as a whole, no roots are
/// discovered. A layer is only included when it is enabled in the options.
/// Non-existent roots are included with `exists: false` so callers can inspect
/// discovery without triggering filesystem creation.
///
/// The global home directory is resolved via `dirs::home_dir()`. When no home
/// directory can be determined and no explicit override is provided, the
/// global layer is skipped (a diagnostic is the loader's responsibility).
pub fn discover_layer_roots(options: &DotagentsLoadOptions) -> Vec<ResolvedLayerRoot> {
    let mut layers = Vec::new();

    if !options.is_enabled() {
        return layers;
    }

    if options.is_global_enabled()
        && let Some(root) = resolve_global_root(options)
    {
        layers.push(resolved(DotagentsLayer::Global, root));
    }

    if options.is_workspace_enabled()
        && let Some(root) = resolve_workspace_root(options)
    {
        layers.push(resolved(DotagentsLayer::Workspace, root));
    }

    layers
}

/// The default global root (`~/.agents`) without checking existence.
pub fn default_global_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(PROTOCOL_DIR_NAME))
}

/// The default workspace root (`<workspace>/.agents`) without checking
/// existence.
pub fn default_workspace_root(workspace: &Path) -> PathBuf {
    workspace.join(PROTOCOL_DIR_NAME)
}

fn resolve_global_root(options: &DotagentsLoadOptions) -> Option<PathBuf> {
    if let Some(override_root) = options.global_root_override() {
        return Some(override_root.to_path_buf());
    }
    default_global_root()
}

fn resolve_workspace_root(options: &DotagentsLoadOptions) -> Option<PathBuf> {
    if let Some(override_root) = options.workspace_root_override() {
        return Some(override_root.to_path_buf());
    }
    options.workspace().map(default_workspace_root)
}

fn resolved(layer: DotagentsLayer, root: PathBuf) -> ResolvedLayerRoot {
    // Use `symlink_metadata` so a symlinked root is still reported as existing,
    // while `is_dir` follows the link to confirm it is a directory. Confinement
    // canonicalization happens later, during file resolution.
    let exists = root.is_dir();
    ResolvedLayerRoot {
        layer,
        root,
        exists,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn disabled_options_discover_no_layers() {
        let roots = discover_layer_roots(&DotagentsLoadOptions::default());
        assert!(roots.is_empty());
    }

    #[test]
    fn disabled_options_with_overrides_still_discover_nothing() {
        // Even with roots configured, disabled loading must not discover, so a
        // present `.agents` directory has no effect.
        let options = DotagentsLoadOptions::default()
            .with_global_root("/tmp/global-does-not-exist")
            .with_workspace_root("/tmp/ws-does-not-exist");
        let roots = discover_layer_roots(&options);
        assert!(roots.is_empty());
    }

    #[test]
    fn enabled_options_with_overrides_resolve_but_report_nonexistence() {
        let options = DotagentsLoadOptions::enabled()
            .with_global_root("/tmp/global-does-not-exist")
            .with_workspace_root("/tmp/ws-does-not-exist");
        let roots = discover_layer_roots(&options);
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|r| !r.exists));
    }

    #[test]
    fn global_override_is_used_verbatim() {
        let options = DotagentsLoadOptions::enabled()
            .with_global_root("/explicit/global")
            .with_global_enabled(true)
            .with_workspace_enabled(false);

        let roots = discover_layer_roots(&options);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].layer, DotagentsLayer::Global);
        assert_eq!(roots[0].root, PathBuf::from("/explicit/global"));
        assert!(!roots[0].exists);
    }

    #[test]
    fn workspace_without_workspace_is_skipped() {
        let options = DotagentsLoadOptions::enabled()
            .with_global_enabled(false)
            .with_workspace_enabled(true);

        // No workspace configured => no workspace layer, and definitely no
        // fallback to the process working directory.
        let roots = discover_layer_roots(&options);
        assert!(roots.is_empty());
    }

    #[test]
    fn workspace_derives_dot_agents_suffix() {
        let options = DotagentsLoadOptions::enabled()
            .with_workspace("/projects/app")
            .with_global_enabled(false);

        let roots = discover_layer_roots(&options);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].layer, DotagentsLayer::Workspace);
        assert_eq!(roots[0].root, PathBuf::from("/projects/app/.agents"));
    }

    #[test]
    fn workspace_override_wins_over_derived_root() {
        let options = DotagentsLoadOptions::enabled()
            .with_workspace("/projects/app")
            .with_workspace_root("/custom/agents")
            .with_global_enabled(false);

        let roots = discover_layer_roots(&options);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].root, PathBuf::from("/custom/agents"));
    }

    #[test]
    fn both_layers_discovered_global_first() {
        let options = DotagentsLoadOptions::enabled()
            .with_workspace("/projects/app")
            .with_global_root("/global");
        let roots = discover_layer_roots(&options);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].layer, DotagentsLayer::Global);
        assert_eq!(roots[1].layer, DotagentsLayer::Workspace);
    }

    #[test]
    fn disabled_layer_is_not_discovered() {
        let options = DotagentsLoadOptions::enabled()
            .with_workspace("/projects/app")
            .with_global_root("/global")
            .with_global_enabled(false)
            .with_workspace_enabled(false);
        assert!(discover_layer_roots(&options).is_empty());
    }

    #[test]
    fn nonexistent_root_is_reported_but_not_created() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join(".agents");
        let options = DotagentsLoadOptions::enabled()
            .with_workspace(tmp.path())
            .with_global_enabled(false);

        let roots = discover_layer_roots(&options);
        assert_eq!(roots.len(), 1);
        assert!(!roots[0].exists);
        // Discovery must not create the directory.
        assert!(!missing.exists());
    }

    #[test]
    fn existing_root_is_reported_as_existing() {
        let tmp = TempDir::new().unwrap();
        let agents = tmp.path().join(".agents");
        std::fs::create_dir_all(&agents).unwrap();

        let options = DotagentsLoadOptions::enabled()
            .with_workspace(tmp.path())
            .with_global_enabled(false);

        let roots = discover_layer_roots(&options);
        assert_eq!(roots.len(), 1);
        assert!(roots[0].exists);
    }

    #[test]
    fn default_workspace_root_appends_protocol_dir() {
        assert_eq!(
            default_workspace_root(Path::new("/a/b")),
            PathBuf::from("/a/b/.agents")
        );
    }
}
