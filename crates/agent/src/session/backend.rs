//! Unified storage backend abstraction.
//!
//! This module provides a trait-based abstraction over storage backends,
//! enabling clean separation between session persistence (command side)
//! and event/projection handling (query side).

use crate::knowledge::KnowledgeStore;
use crate::session::error::{SessionError, SessionResult};
use crate::session::projection::{EventJournal, ViewStore};
use crate::session::repo_schedule::ScheduleRepository;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::SessionStore;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

pub const QMT_SESSIONS_DB_ENV: &str = "QMT_SESSIONS_DB";

/// Resolve the default on-disk SQLite path for agent state.
///
/// Uses the shared QueryMT config directory (`$HOME/.qmt`) and ensures the
/// directory exists before returning `<config_dir>/agent.db`.
pub fn default_agent_db_path() -> SessionResult<PathBuf> {
    let cfg_dir = querymt_utils::providers::config_dir()
        .map_err(|e| SessionError::Other(format!("Failed to resolve QueryMT config dir: {e}")))?;

    std::fs::create_dir_all(&cfg_dir).map_err(|e| {
        SessionError::Other(format!(
            "Failed to create QueryMT config dir {:?}: {e}",
            cfg_dir
        ))
    })?;

    Ok(cfg_dir.join("agent.db"))
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

/// Unified storage backend providing both command and query side stores.
///
/// Implementations provide:
/// - `session_store()`: For session/message persistence (command side)
/// - `event_journal()`: For durable event persistence
/// - `view_store()`: For generating views (query side, optional)
/// - `schedule_repository()`: For schedule persistence (optional)
/// - `knowledge_store()`: For knowledge persistence (optional)
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Session persistence (command side).
    fn session_store(&self) -> Arc<dyn SessionStore>;

    /// Durable event journal.
    fn event_journal(&self) -> Arc<dyn EventJournal>;

    /// Projection queries (query side).
    /// Returns None if backend doesn't support local projections (e.g., Kafka).
    fn view_store(&self) -> Option<Arc<dyn ViewStore>>;

    /// Schedule persistence for the scheduler actor.
    /// Returns None if backend doesn't support scheduling.
    fn schedule_repository(&self) -> Option<Arc<dyn ScheduleRepository>> {
        None
    }

    /// Knowledge store for the knowledge tools.
    /// Returns None if backend doesn't support knowledge storage.
    fn knowledge_store(&self) -> Option<Arc<dyn KnowledgeStore>> {
        None
    }
}

/// SQLite implementation of StorageBackend.
///
/// Implements all storage traits on a single unified struct.
/// For single-process deployments with local persistence.
#[async_trait]
impl StorageBackend for SqliteStorage {
    fn session_store(&self) -> Arc<dyn SessionStore> {
        Arc::new(self.clone())
    }

    fn event_journal(&self) -> Arc<dyn EventJournal> {
        Arc::new(self.clone())
    }

    fn view_store(&self) -> Option<Arc<dyn ViewStore>> {
        Some(Arc::new(self.clone()))
    }

    fn schedule_repository(&self) -> Option<Arc<dyn ScheduleRepository>> {
        Some(Arc::new(
            crate::session::repo_schedule::SqliteScheduleRepository::new(self.conn()),
        ))
    }

    fn knowledge_store(&self) -> Option<Arc<dyn KnowledgeStore>> {
        Some(Arc::new(
            crate::knowledge::sqlite::SqliteKnowledgeStore::new(self.conn()),
        ))
    }
}

impl SqliteStorage {
    /// Helper: connect to SQLite at the given path.
    ///
    /// Creates the database file if needed and applies pending migrations.
    pub async fn connect_backend(path: PathBuf) -> SessionResult<Self> {
        SqliteStorage::connect(path).await
    }
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
        assert_eq!(path, cfg_dir.join("agent.db"));
    }

    #[test]
    fn resolve_agent_db_path_prefers_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvVarGuard::set(QMT_SESSIONS_DB_ENV, "/tmp/env-agent.db");
        assert_eq!(
            resolve_agent_db_path(Some(PathBuf::from("/tmp/cli-agent.db"))).unwrap(),
            PathBuf::from("/tmp/cli-agent.db")
        );
    }

    #[test]
    fn resolve_agent_db_path_uses_non_empty_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvVarGuard::set(QMT_SESSIONS_DB_ENV, "/tmp/env-agent.db");
        assert_eq!(
            resolve_agent_db_path(None).unwrap(),
            PathBuf::from("/tmp/env-agent.db")
        );
    }

    #[test]
    fn resolve_agent_db_path_ignores_empty_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvVarGuard::set(QMT_SESSIONS_DB_ENV, "   ");
        let path = resolve_agent_db_path(None).expect("default agent db path");
        let cfg_dir = querymt_utils::providers::config_dir().expect("config dir");
        assert_eq!(path, cfg_dir.join("agent.db"));
    }
}
