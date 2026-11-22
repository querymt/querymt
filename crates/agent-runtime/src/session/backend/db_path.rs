//! Default location of the shared sessions database.
//!
//! Split from the storage-backend contract because it needs
//! `querymt_utils::providers::config_dir`, which the core layer must not
//! depend on. `session::backend` re-exports everything here.

use crate::session::error::{SessionError, SessionResult};
use std::{ffi::OsString, path::PathBuf};

pub const QMT_SESSIONS_DB_ENV: &str = "QMT_SESSIONS_DB";

/// Resolve the default on-disk SQLite path for agent state.
///
/// Uses the shared QueryMT config directory (`$HOME/.qmt`) and ensures the
/// directory exists before returning `<config_dir>/sessions.db`.
pub fn default_agent_db_path() -> SessionResult<PathBuf> {
    let cfg_dir = querymt_utils::providers::config_dir()
        .map_err(|e| SessionError::Other(format!("Failed to resolve QueryMT config dir: {e}")))?;
    default_agent_db_path_in(cfg_dir)
}

fn default_agent_db_path_in(cfg_dir: PathBuf) -> SessionResult<PathBuf> {
    std::fs::create_dir_all(&cfg_dir).map_err(|e| {
        SessionError::Other(format!(
            "Failed to create QueryMT config dir {:?}: {e}",
            cfg_dir
        ))
    })?;

    Ok(cfg_dir.join("sessions.db"))
}

/// Resolve the shared sessions database path.
///
/// Explicit runtime overrides win, then `QMT_SESSIONS_DB` when set to a
/// non-empty value, and finally the default QueryMT config directory path.
pub fn resolve_agent_db_path(override_path: Option<PathBuf>) -> SessionResult<PathBuf> {
    resolve_db_path(
        override_path,
        || std::env::var_os(QMT_SESSIONS_DB_ENV),
        default_agent_db_path,
    )
}

fn resolve_db_path(
    override_path: Option<PathBuf>,
    env_path: impl FnOnce() -> Option<OsString>,
    default_path: impl FnOnce() -> SessionResult<PathBuf>,
) -> SessionResult<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    if let Some(path) = env_path().filter(|value| !value.to_string_lossy().trim().is_empty()) {
        return Ok(PathBuf::from(path));
    }
    default_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_db_path_points_to_qmt_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join(".qmt");
        let path = default_agent_db_path_in(cfg_dir.clone()).expect("default agent db path");
        assert_eq!(path, cfg_dir.join("sessions.db"));
        assert!(cfg_dir.is_dir());
    }

    #[test]
    fn resolve_agent_db_path_prefers_override() {
        let path = PathBuf::from("cli-sessions.db");
        assert_eq!(
            resolve_db_path(
                Some(path.clone()),
                || panic!("override must not consult the environment"),
                || panic!("override must not use the default"),
            )
            .unwrap(),
            path
        );
    }

    #[test]
    fn resolve_agent_db_path_uses_non_empty_env() {
        assert_eq!(
            resolve_db_path(
                None,
                || Some("env-sessions.db".into()),
                || panic!("environment path must not use the default"),
            )
            .unwrap(),
            PathBuf::from("env-sessions.db")
        );
    }

    #[test]
    fn resolve_agent_db_path_ignores_empty_env() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join(".qmt");
        let path = resolve_db_path(
            None,
            || Some("   ".into()),
            || default_agent_db_path_in(cfg_dir.clone()),
        )
        .expect("default agent db path");
        assert_eq!(path, cfg_dir.join("sessions.db"));
    }
}
