//! Unified storage backend abstraction.
//!
//! This module provides a trait-based abstraction over storage backends,
//! enabling clean separation between session persistence (command side)
//! and event/projection handling (query side).

use crate::events::EventObserver;
use crate::session::error::SessionResult;
use crate::session::projection::{EventStore, ViewStore};
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::SessionStore;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

/// Unified storage backend providing both command and query side stores.
///
/// Implementations provide:
/// - `session_store()`: For session/message persistence (command side)
/// - `event_store()`: For event persistence and querying
/// - `view_store()`: For generating views (query side, optional)
/// - `event_observer()`: For event persistence (plugs into EventBus)
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Session persistence (command side).
    fn session_store(&self) -> Arc<dyn SessionStore>;

    /// Event persistence and querying.
    fn event_store(&self) -> Arc<dyn EventStore>;

    /// Event observer for persistence (plugs into EventBus).
    fn event_observer(&self) -> Arc<dyn EventObserver>;

    /// Projection queries (query side).
    /// Returns None if backend doesn't support local projections (e.g., Kafka).
    fn view_store(&self) -> Option<Arc<dyn ViewStore>>;
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

    fn event_store(&self) -> Arc<dyn EventStore> {
        Arc::new(self.clone())
    }

    fn event_observer(&self) -> Arc<dyn EventObserver> {
        Arc::new(self.clone())
    }

    fn view_store(&self) -> Option<Arc<dyn ViewStore>> {
        Some(Arc::new(self.clone()))
    }
}

impl SqliteStorage {
    /// Helper: Connect to SQLite database at the given path.
    ///
    /// Creates the database and schema if it doesn't exist.
    /// Use `:memory:` for in-memory database.
    pub async fn connect_backend(path: PathBuf) -> SessionResult<Self> {
        SqliteStorage::connect(path).await
    }
}
