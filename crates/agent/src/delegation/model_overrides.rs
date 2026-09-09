use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::RwLock;
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DelegateModelOverride {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DelegateModelOverrideStore {
    overrides: Arc<RwLock<HashMap<(String, String), DelegateModelOverride>>>,
}

impl DelegateModelOverrideStore {
    pub async fn set(
        &self,
        parent_session_id: impl Into<String>,
        agent_id: impl Into<String>,
        model: DelegateModelOverride,
    ) {
        self.overrides
            .write()
            .await
            .insert((parent_session_id.into(), agent_id.into()), model);
    }

    pub async fn get(
        &self,
        parent_session_id: &str,
        agent_id: &str,
    ) -> Option<DelegateModelOverride> {
        self.overrides
            .read()
            .await
            .get(&(parent_session_id.to_string(), agent_id.to_string()))
            .cloned()
    }

    pub async fn clear(&self, parent_session_id: &str, agent_id: &str) {
        self.overrides
            .write()
            .await
            .remove(&(parent_session_id.to_string(), agent_id.to_string()));
    }

    pub(crate) async fn update(
        &self,
        parent_session_id: &str,
        agent_id: &str,
        model: Option<DelegateModelOverride>,
    ) -> bool {
        let key = (parent_session_id.to_string(), agent_id.to_string());
        let mut overrides = self.overrides.write().await;
        if overrides.get(&key) == model.as_ref() {
            return false;
        }
        match model {
            Some(model) => {
                overrides.insert(key, model);
            }
            None => {
                overrides.remove(&key);
            }
        }
        true
    }

    pub(crate) async fn list_parent(
        &self,
        parent_session_id: &str,
    ) -> BTreeMap<String, DelegateModelOverride> {
        self.overrides
            .read()
            .await
            .iter()
            .filter(|((session_id, _), _)| session_id == parent_session_id)
            .map(|((_, agent_id), model)| (agent_id.clone(), model.clone()))
            .collect()
    }

    /// Durable storage wins even when it contains no override; never resurrect stale cache entries.
    pub(crate) async fn resolve(
        &self,
        store: &dyn crate::session::store::SessionStore,
        parent_session_id: &str,
        agent_id: &str,
    ) -> crate::session::error::SessionResult<Option<DelegateModelOverride>> {
        match store.get_delegate_assignments(parent_session_id).await? {
            Some(state) => Ok(state.overrides.get(agent_id).cloned()),
            None => Ok(self.get(parent_session_id, agent_id).await),
        }
    }

    pub async fn clear_parent(&self, parent_session_id: &str) {
        self.overrides
            .write()
            .await
            .retain(|(session_id, _), _| session_id != parent_session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                .unwrap(),
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
                .is_none()
        );
        assert!(cache.resolve(&storage, "missing", "coder").await.is_err());
        // Existing custom storage implementations retain their explicit in-memory behavior.
        let unsupported = crate::test_utils::MockSessionStore::new();
        assert_eq!(
            cache
                .resolve(&unsupported, &parent.public_id, "coder")
                .await
                .unwrap(),
            Some(old)
        );
    }

    #[tokio::test]
    async fn overrides_are_isolated_and_clearable() {
        let store = DelegateModelOverrideStore::default();
        let first = DelegateModelOverride {
            model_id: "provider/first".into(),
            node_id: None,
        };
        let second = DelegateModelOverride {
            model_id: "provider/second".into(),
            node_id: Some("node-2".into()),
        };

        store.set("parent-1", "coder", first.clone()).await;
        store.set("parent-1", "reviewer", second.clone()).await;
        store.set("parent-2", "coder", second.clone()).await;

        assert_eq!(store.get("parent-1", "coder").await, Some(first.clone()));
        assert_eq!(
            store.get("parent-1", "reviewer").await,
            Some(second.clone())
        );

        assert_eq!(
            store.list_parent("parent-1").await,
            BTreeMap::from([
                ("coder".into(), first.clone()),
                ("reviewer".into(), second.clone()),
            ])
        );
        assert!(!store.update("parent-1", "coder", Some(first)).await);
        assert!(store.update("parent-1", "coder", None).await);
        assert!(!store.update("parent-1", "coder", None).await);
        assert_eq!(store.get("parent-1", "coder").await, None);
        assert_eq!(store.get("parent-2", "coder").await, Some(second));

        store.clear_parent("parent-1").await;
        assert_eq!(store.get("parent-1", "reviewer").await, None);
        assert!(store.get("parent-2", "coder").await.is_some());
    }
}
