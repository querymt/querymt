//! Tests of `DelegateModelOverrideStore` that need a real SQLite session store
//! (moved out of `core_staging/delegation/model_overrides.rs`, D6).

use crate::core_staging::delegation::model_overrides::{
    DelegateModelOverride, DelegateModelOverrideStore,
};

#[tokio::test]
async fn durable_delegate_assignments_override_memory_and_propagate_failures() {
    use crate::session::store::SessionStore;
    let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
        .await
        .unwrap();
    let parent = storage
        .create_session(None, None, None, None)
        .await
        .unwrap();
    let cache = DelegateModelOverrideStore::default();
    let old = DelegateModelOverride {
        model_id: "provider/stale".into(),
        node_id: None,
    };
    let new = DelegateModelOverride {
        model_id: "provider/new".into(),
        node_id: Some("node".into()),
    };
    cache.set(&parent.public_id, "coder", old.clone()).await;
    assert!(
        cache
            .resolve(&storage, &parent.public_id, "coder")
            .await
            .unwrap()
            .model
            .is_none()
    );
    storage
        .set_delegate_assignment(&parent.public_id, "coder", Some(new.clone()), None)
        .await
        .unwrap();
    assert_eq!(
        cache
            .resolve(&storage, &parent.public_id, "coder")
            .await
            .unwrap()
            .model,
        Some(new)
    );
    storage
        .set_delegate_assignment(&parent.public_id, "coder", None, None)
        .await
        .unwrap();
    assert!(
        cache
            .resolve(&storage, &parent.public_id, "coder")
            .await
            .unwrap()
            .model
            .is_none()
    );
    assert!(cache.resolve(&storage, "missing", "coder").await.is_err());
    // Existing custom storage implementations retain their explicit in-memory behavior.
    let unsupported = crate::core_staging::test_support::MockSessionStore::new();
    assert_eq!(
        cache
            .resolve(&unsupported, &parent.public_id, "coder")
            .await
            .unwrap()
            .model,
        Some(old)
    );
}
