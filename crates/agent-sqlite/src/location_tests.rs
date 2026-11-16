//! Session-location resolution tests that need a real SQLite store.
//!
//! They live in the sqlite layer because core cannot depend on `SqliteStorage`.

use crate::session::sqlite_storage::SqliteStorage;
use querymt_agent_core::session::location::{SessionLocation, resolve_session_location};
use querymt_agent_core::session::store::{RemoteSessionBookmark, SessionStore};
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
        let store = querymt_agent_core::session::backend::StorageBackend::session_store(&storage);
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
            fork_origin: None,
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

#[tokio::test]
async fn batch_resolution_preserves_local_remote_conflict_and_missing_states() {
    let fx = Fixture::new().await;
    let local = fx
        .store
        .create_session(None, None, None, None)
        .await
        .expect("create local session");
    let remote = fx.bookmark("sess-batch-remote");
    fx.store
        .save_remote_session_bookmark(&remote)
        .await
        .expect("persist remote bookmark");
    let conflict = fx
        .store
        .create_session(None, None, None, None)
        .await
        .expect("create colliding local session");
    let conflict_bookmark = fx.bookmark(&conflict.public_id);
    fx.store
        .save_remote_session_bookmark(&conflict_bookmark)
        .await
        .expect("persist conflicting bookmark");

    let ids = vec![
        local.public_id.clone(),
        remote.session_id.clone(),
        conflict.public_id.clone(),
        "sess-batch-missing".to_string(),
    ];
    let locations = fx
        .store
        .resolve_session_locations(&ids)
        .await
        .expect("resolve locations in one batch");

    assert_eq!(
        locations.get(&local.public_id),
        Some(&SessionLocation::Local)
    );
    assert_eq!(
        locations.get(&remote.session_id),
        Some(&SessionLocation::Remote {
            bookmark: remote.clone()
        })
    );
    assert_eq!(
        locations.get(&conflict.public_id),
        Some(&SessionLocation::Conflict {
            bookmark: conflict_bookmark
        })
    );
    assert_eq!(
        locations.get("sess-batch-missing"),
        Some(&SessionLocation::NotFound)
    );
}
