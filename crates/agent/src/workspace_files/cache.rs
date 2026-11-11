//! Short-lived cache of remote session file indexes.
//!
//! A remote session's index is fetched over the mesh in full and then ranked on
//! this node. Keystroke-rate queries would otherwise refetch it every time, so
//! the fetched index is kept for [`REMOTE_INDEX_TTL`] and dropped early when
//! the peer reports `indexReady` for that session.

use crate::agent::file_proxy::GetFileIndexResponse;
use moka::sync::Cache;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// How long a fetched remote index is reused.
pub const REMOTE_INDEX_TTL: Duration = Duration::from_secs(5);
const MAX_CACHED_SESSIONS: u64 = 32;

fn cache() -> &'static Cache<String, Arc<GetFileIndexResponse>> {
    static CACHE: OnceLock<Cache<String, Arc<GetFileIndexResponse>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Cache::builder()
            .max_capacity(MAX_CACHED_SESSIONS)
            .time_to_live(REMOTE_INDEX_TTL)
            .build()
    })
}

/// The cached index of `session_id`, if still fresh.
pub fn get(session_id: &str) -> Option<Arc<GetFileIndexResponse>> {
    cache().get(session_id)
}

/// Remember the index fetched for `session_id`.
pub fn insert(session_id: &str, index: Arc<GetFileIndexResponse>) {
    cache().insert(session_id.to_owned(), index);
}

/// Forget the index of `session_id`, e.g. because its workspace was re-indexed.
pub fn invalidate(session_id: &str) {
    cache().invalidate(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(root: &str) -> Arc<GetFileIndexResponse> {
        Arc::new(GetFileIndexResponse {
            files: Vec::new(),
            generated_at: 0,
            workspace_root: root.into(),
            cwd: None,
        })
    }

    #[test]
    fn insert_get_and_invalidate() {
        insert("cache-test-session", index("/r"));
        assert_eq!(get("cache-test-session").unwrap().workspace_root, "/r");
        invalidate("cache-test-session");
        assert!(get("cache-test-session").is_none());
    }
}
