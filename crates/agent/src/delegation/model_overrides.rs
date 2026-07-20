use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

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

        assert_eq!(store.get("parent-1", "coder").await, Some(first));
        assert_eq!(
            store.get("parent-1", "reviewer").await,
            Some(second.clone())
        );

        store.clear("parent-1", "coder").await;
        assert_eq!(store.get("parent-1", "coder").await, None);
        assert_eq!(store.get("parent-2", "coder").await, Some(second));

        store.clear_parent("parent-1").await;
        assert_eq!(store.get("parent-1", "reviewer").await, None);
        assert!(store.get("parent-2", "coder").await.is_some());
    }
}
