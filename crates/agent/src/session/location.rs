//! Authoritative session location resolution.
//!
//! Durable session identity and transient connectivity are separate concepts.
//! A [SessionLocation] is derived purely from durable storage — a local session
//! row and/or a persisted remote bookmark — never from the live registry
//! membership, which is only an attachment handle.
//!
//! Required invariants enforced by [resolve_session_location]:
//!
//! 1. A session ID resolves to exactly one authoritative location: local storage
//!    or a remote bookmark.
//! 2. If both a local session row and a remote bookmark exist for the same ID,
//!    resolution returns a typed [SessionLocation::Conflict].
//! 5. A bookmark is authoritative even when no actor is connected (Required
//!    Invariant #5 from the plan: "If only the bookmark exists, return `Remote`
//!    regardless of current registry state.").

use crate::session::error::SessionError;
use crate::session::store::{RemoteSessionBookmark, SessionStore};

/// Where a session ID durably lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLocation {
    /// A local session row exists in this node's storage.
    Local,
    /// A remote session bookmark exists, authoritatively marking the ID as
    /// remote regardless of current registry/attachment state.
    Remote { bookmark: RemoteSessionBookmark },
    /// Both a local session row and a remote bookmark exist for the same ID —
    /// an ambiguous state that callers must surface as a typed conflict.
    Conflict { bookmark: RemoteSessionBookmark },
    /// Neither a local session row nor a remote bookmark exists.
    NotFound,
}

/// Result of resolving a session's authoritative location.
pub type SessionLocationResult = Result<SessionLocation, SessionError>;

/// Resolve the authoritative durable location for `session_id`.
///
/// Looks first at the persisted remote bookmark, then at the local session row:
///
/// 1. remote bookmark exists AND local row exists  → `Conflict`
/// 2. only local row exists                         → `Local`
/// 3. only remote bookmark exists                   → `Remote` (regardless of registry)
/// 4. neither exists                                → `NotFound`
///
/// An installed registry attachment must never determine durable location; it
/// can only enrich runtime information after this returns the authoritative type.
pub async fn resolve_session_location(
    session_store: &dyn SessionStore,
    session_id: &str,
) -> SessionLocationResult {
    // 1. Check the remote bookmark (authoritative durable identity for remote).
    let bookmark = session_store
        .get_remote_session_bookmark(session_id)
        .await?;

    // 2. Check whether a local session row exists. `get_session` returns
    //    `Ok(None)` when no row matches, so a hard error here is a genuine
    //    storage failure and must propagate — never guess.
    let local_exists = session_store.get_session(session_id).await?.is_some();

    Ok(match (local_exists, bookmark) {
        (true, Some(bookmark)) => SessionLocation::Conflict { bookmark },
        (true, None) => SessionLocation::Local,
        (false, Some(bookmark)) => SessionLocation::Remote { bookmark },
        (false, None) => SessionLocation::NotFound,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::sqlite_storage::SqliteStorage;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Shared in-memory storage: exposes a `SessionStore` and a `SessionRepository`
    /// over the same SQLite database (authoritative local row detection).
    struct Fixture {
        store: Arc<dyn SessionStore>,
    }

    impl Fixture {
        async fn new() -> Self {
            let storage = SqliteStorage::connect(PathBuf::from(":memory:".to_string()))
                .await
                .expect("connect in-memory sqlite");
            let store = crate::session::backend::StorageBackend::session_store(&storage);
            Self { store }
        }

        fn bookmark(&self, session_id: &str) -> RemoteSessionBookmark {
            RemoteSessionBookmark {
                session_id: session_id.to_string(),
                node_id: "node-remote-1".to_string(),
                peer_label: "remote-host".to_string(),
                cwd: Some("/srv/work".to_string()),
                created_at: 1_700_000_000,
                title: Some("Remote session".to_string()),
            }
        }
    }

    async fn resolve(fx: &Fixture, id: &str) -> SessionLocation {
        resolve_session_location(fx.store.as_ref(), id)
            .await
            .expect("resolve_session_location")
    }

    #[tokio::test]
    async fn neither_local_nor_bookmark_is_not_found() {
        let fx = Fixture::new().await;
        assert_eq!(
            resolve(&fx, "sess-not-present").await,
            SessionLocation::NotFound
        );
    }

    #[tokio::test]
    async fn only_local_row_is_local() {
        let fx = Fixture::new().await;
        let session = fx
            .store
            .create_session(None, None, None, None)
            .await
            .expect("create local session");
        assert_eq!(
            resolve(&fx, &session.public_id).await,
            SessionLocation::Local
        );
    }

    #[tokio::test]
    async fn only_bookmark_is_remote_even_without_registry_attachment() {
        let fx = Fixture::new().await;
        let bookmark = fx.bookmark("sess-remote-only");
        fx.store
            .save_remote_session_bookmark(&bookmark)
            .await
            .expect("persist bookmark");
        // No local row, no registry entry → still authoritatively Remote.
        assert_eq!(
            resolve(&fx, "sess-remote-only").await,
            SessionLocation::Remote {
                bookmark: bookmark.clone()
            }
        );
    }

    #[tokio::test]
    async fn bookmark_plus_local_row_is_typed_conflict() {
        let fx = Fixture::new().await;
        let session = fx
            .store
            .create_session(None, None, None, None)
            .await
            .expect("create local session");
        let bookmark = fx.bookmark(&session.public_id);
        fx.store
            .save_remote_session_bookmark(&bookmark)
            .await
            .expect("persist bookmark");
        assert_eq!(
            resolve(&fx, &session.public_id).await,
            SessionLocation::Conflict {
                bookmark: bookmark.clone()
            }
        );
    }
}
