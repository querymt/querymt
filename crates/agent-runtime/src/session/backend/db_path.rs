//! Default location of the shared sessions database.
//!
//! Split from the storage-backend contract because it needs
//! `querymt_utils::providers::config_dir`, which the core layer must not
//! depend on. `session::backend` re-exports everything here.

use crate::session::error::{SessionError, SessionResult};
use std::path::PathBuf;

pub const QMT_SESSIONS_DB_ENV: &str = "QMT_SESSIONS_DB";

/// Resolve the default on-disk SQLite path for agent state.
///
/// Uses the shared QueryMT config directory (`$HOME/.qmt`) and ensures the
/// directory exists before returning `<config_dir>/sessions.db`.
pub fn default_agent_db_path() -> SessionResult<PathBuf> {
    let cfg_dir = querymt_utils::providers::config_dir()
        .map_err(|e| SessionError::Other(format!("Failed to resolve QueryMT config dir: {e}")))?;

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
    if let Some(path) = override_path {
        return Ok(path);
    }

    if let Some(path) = std::env::var_os(QMT_SESSIONS_DB_ENV)
        .filter(|value| !value.to_string_lossy().trim().is_empty())
    {
        return Ok(PathBuf::from(path));
    }

    default_agent_db_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn default_agent_db_path_points_to_qmt_dir() {
        let path = default_agent_db_path().expect("default agent db path");
        let cfg_dir = querymt_utils::providers::config_dir().expect("config dir");
        assert_eq!(path, cfg_dir.join("sessions.db"));
    }

    #[test]
    fn resolve_agent_db_path_prefers_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvVarGuard::set(QMT_SESSIONS_DB_ENV, "/tmp/env-sessions.db");
        assert_eq!(
            resolve_agent_db_path(Some(PathBuf::from("/tmp/cli-sessions.db"))).unwrap(),
            PathBuf::from("/tmp/cli-sessions.db")
        );
    }

    #[test]
    fn resolve_agent_db_path_uses_non_empty_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvVarGuard::set(QMT_SESSIONS_DB_ENV, "/tmp/env-sessions.db");
        assert_eq!(
            resolve_agent_db_path(None).unwrap(),
            PathBuf::from("/tmp/env-sessions.db")
        );
    }

    #[test]
    fn resolve_agent_db_path_ignores_empty_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvVarGuard::set(QMT_SESSIONS_DB_ENV, "   ");
        let path = resolve_agent_db_path(None).expect("default agent db path");
        let cfg_dir = querymt_utils::providers::config_dir().expect("config dir");
        assert_eq!(path, cfg_dir.join("sessions.db"));
    }
}
