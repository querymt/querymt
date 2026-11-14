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

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegateReasoningEffort {
    Auto,
    Low,
    Medium,
    High,
    Max,
}

impl DelegateReasoningEffort {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    pub(crate) fn session_effort(self) -> Option<querymt::chat::ReasoningEffort> {
        use querymt::chat::ReasoningEffort;
        match self {
            Self::Auto => None,
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
            Self::Max => Some(ReasoningEffort::Max),
        }
    }
}

impl std::str::FromStr for DelegateReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "max" => Ok(Self::Max),
            _ => Err(format!("Invalid delegate reasoning effort: {value}")),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DelegateRouteOverrides {
    pub model: Option<DelegateModelOverride>,
    pub reasoning_effort: Option<DelegateReasoningEffort>,
}

#[derive(Debug, Default)]
struct DelegateOverrideCacheState {
    values: HashMap<(String, String), DelegateRouteOverrides>,
}

#[derive(Debug, Clone, Default)]
pub struct DelegateModelOverrideStore {
    state: Arc<RwLock<DelegateOverrideCacheState>>,
}

impl DelegateModelOverrideStore {
    pub async fn set(
        &self,
        parent_session_id: impl Into<String>,
        agent_id: impl Into<String>,
        model: DelegateModelOverride,
    ) {
        let parent_session_id = parent_session_id.into();
        let agent_id = agent_id.into();
        let _ = self
            .update_route(&parent_session_id, &agent_id, Some(model), None)
            .await;
    }

    pub async fn get(
        &self,
        parent_session_id: &str,
        agent_id: &str,
    ) -> Option<DelegateModelOverride> {
        self.get_route(parent_session_id, agent_id).await.model
    }

    /// Clear only the model override, matching a request with omitted reasoning.
    pub async fn clear(&self, parent_session_id: &str, agent_id: &str) {
        let _ = self
            .update_route(parent_session_id, agent_id, None, None)
            .await;
    }

    pub async fn get_reasoning(
        &self,
        parent_session_id: &str,
        agent_id: &str,
    ) -> Option<DelegateReasoningEffort> {
        self.get_route(parent_session_id, agent_id)
            .await
            .reasoning_effort
    }

    pub(crate) async fn get_route(
        &self,
        parent_session_id: &str,
        agent_id: &str,
    ) -> DelegateRouteOverrides {
        self.state
            .read()
            .await
            .values
            .get(&(parent_session_id.to_string(), agent_id.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) async fn update_route(
        &self,
        parent_session_id: &str,
        agent_id: &str,
        model: Option<DelegateModelOverride>,
        reasoning_effort: Option<Option<DelegateReasoningEffort>>,
    ) -> (bool, DelegateRouteOverrides) {
        let key = (parent_session_id.to_string(), agent_id.to_string());
        let mut state = self.state.write().await;
        let current = state.values.get(&key).cloned().unwrap_or_default();
        let mut next = current.clone();
        next.model = model;
        if let Some(reasoning_effort) = reasoning_effort {
            next.reasoning_effort = reasoning_effort;
        }
        if next == current {
            return (false, current);
        }
        if next.model.is_none() && next.reasoning_effort.is_none() {
            state.values.remove(&key);
        } else {
            state.values.insert(key, next.clone());
        }
        (true, next)
    }

    pub(crate) async fn list_parent_routes(
        &self,
        parent_session_id: &str,
    ) -> BTreeMap<String, DelegateRouteOverrides> {
        self.state
            .read()
            .await
            .values
            .iter()
            .filter(|((session_id, _), _)| session_id == parent_session_id)
            .map(|((_, agent_id), value)| (agent_id.clone(), value.clone()))
            .collect()
    }

    /// Durable storage wins even when it contains no override; never resurrect stale cache entries.
    pub(crate) async fn resolve(
        &self,
        store: &dyn crate::session::store::SessionStore,
        parent_session_id: &str,
        agent_id: &str,
    ) -> crate::session::error::SessionResult<DelegateRouteOverrides> {
        match store.get_delegate_assignments(parent_session_id).await? {
            Some(state) => Ok(DelegateRouteOverrides {
                model: state.overrides.get(agent_id).cloned(),
                reasoning_effort: state.reasoning_overrides.get(agent_id).copied(),
            }),
            None => Ok(self.get_route(parent_session_id, agent_id).await),
        }
    }

    pub async fn clear_parent(&self, parent_session_id: &str) {
        let mut state = self.state.write().await;
        state
            .values
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
        let unsupported = crate::test_utils::MockSessionStore::new();
        assert_eq!(
            cache
                .resolve(&unsupported, &parent.public_id, "coder")
                .await
                .unwrap()
                .model,
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

        let routes = store.list_parent_routes("parent-1").await;
        assert_eq!(routes["coder"].model, Some(first.clone()));
        assert_eq!(routes["reviewer"].model, Some(second.clone()));
        let (changed, route) = store
            .update_route(
                "parent-1",
                "coder",
                Some(first.clone()),
                Some(Some(DelegateReasoningEffort::High)),
            )
            .await;
        assert!(changed);
        assert_eq!(route.model, Some(first.clone()));
        assert_eq!(route.reasoning_effort, Some(DelegateReasoningEffort::High));
        let (changed, route) = store.update_route("parent-1", "coder", None, None).await;
        assert!(changed);
        assert!(route.model.is_none());
        assert_eq!(route.reasoning_effort, Some(DelegateReasoningEffort::High));
        let (changed, route) = store
            .update_route("parent-1", "coder", None, Some(None))
            .await;
        assert!(changed);
        assert_eq!(route, DelegateRouteOverrides::default());
        assert!(
            !store
                .list_parent_routes("parent-1")
                .await
                .contains_key("coder")
        );
        let (changed, route) = store.update_route("parent-1", "coder", None, None).await;
        assert!(!changed);
        assert_eq!(route, DelegateRouteOverrides::default());
        assert_eq!(store.get("parent-1", "coder").await, None);
        assert_eq!(store.get("parent-2", "coder").await, Some(second));

        store.clear_parent("parent-1").await;
        assert_eq!(store.get("parent-1", "reviewer").await, None);
        assert!(store.get("parent-2", "coder").await.is_some());
    }
}
