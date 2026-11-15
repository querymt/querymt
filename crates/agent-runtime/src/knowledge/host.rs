//! Host wiring for repository knowledge: where indexes live by default.
//!
//! The repository-knowledge code does not know how QueryMT lays out its
//! configuration; the agent supplies the shared config directory here.

use crate::knowledge::repository::{
    RepositoryKnowledgeRegistry, RepositoryKnowledgeSettings, RootError,
};
use std::path::PathBuf;
use std::sync::Arc;

/// The shared QueryMT config directory (`QMT_CONFIG_DIR`, `QMT_HOME`, or
/// `~/.qmt`), so the index location follows host overrides.
pub fn default_config_dir() -> Result<PathBuf, RootError> {
    querymt_utils::providers::config_dir().map_err(|e| RootError::Unavailable {
        reason: format!("cannot resolve the QueryMT config directory: {e}"),
    })
}

/// A registry that places indexes under [`default_config_dir`]. `None` unless
/// the host enabled repository knowledge. Performs no filesystem access.
pub fn registry_from_settings(
    settings: &RepositoryKnowledgeSettings,
) -> Option<Arc<RepositoryKnowledgeRegistry>> {
    RepositoryKnowledgeRegistry::from_settings_with(settings, default_config_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Scenario "Config directory is overridden", on the production path: a
    /// registry built without an explicit directory places the index under the
    /// host's `QMT_CONFIG_DIR`, not a hardcoded home path.
    #[test]
    fn the_default_index_location_follows_the_host_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let host_dir = tmp.path().join("host-config");
        fs::create_dir_all(&host_dir).unwrap();
        // Canonical form, as the cache path is resolved (`/var` vs `/private/var`).
        let host_dir = host_dir.canonicalize().unwrap();
        let previous = std::env::var_os("QMT_CONFIG_DIR");
        // SAFETY: nextest runs each test in its own process; the previous
        // value is restored before returning.
        unsafe { std::env::set_var("QMT_CONFIG_DIR", &host_dir) };
        let registry = registry_from_settings(&RepositoryKnowledgeSettings::enabled()).unwrap();
        let opened = registry.service_for(Some(&repo)).map(|_| ());
        unsafe {
            match previous {
                Some(value) => std::env::set_var("QMT_CONFIG_DIR", value),
                None => std::env::remove_var("QMT_CONFIG_DIR"),
            }
        }
        opened.unwrap();
        let expected = registry
            .resolve(Some(&repo))
            .unwrap()
            .index_path(&host_dir)
            .unwrap();
        assert!(expected.starts_with(&host_dir));
        assert!(expected.exists(), "index created under QMT_CONFIG_DIR");
    }
}
