//! Host-owned mesh sharing policy, independent of discovery and membership.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::NodeId;

pub mod quota;
mod store;
pub use store::{GrantStore, GrantStoreError, PeerGrantState, wait_for_revision_loss};

/// Wire contract under which a protected operation was received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SharingProtocol {
    /// Legacy wire format: only an explicit Full grant permits resource access.
    Legacy,
    /// The grant-aware mesh sharing contract.
    V1,
}

/// Durable key for one direction of sharing between two authenticated peers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerGrantKey {
    /// The identity of the host sharing resources.
    pub host: NodeId,
    /// The identity of the recipient of the grant.
    pub peer: NodeId,
}

/// Resources available to one peer on the host. Missing policy is No access.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum PeerGrant {
    #[default]
    None,
    Full,
    Custom {
        sessions: bool,
        filesystem: bool,
        providers: Vec<ProviderGrant>,
    },
}

/// A shared provider and its model selection (quota configuration is separate).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderGrant {
    pub provider_id: String,
    pub models: ModelSelection,
}

/// Which canonical model IDs a shared provider makes available.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSelection {
    All,
    Selected { model_ids: Vec<String> },
}

/// The host rejected a grant before applying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidPeerGrant {
    #[error("filesystem access requires session access")]
    FilesystemRequiresSessions,
    #[error("provider IDs must be nonempty and unique")]
    InvalidProviders,
    #[error("a model selection must contain nonempty, unique IDs")]
    InvalidModels,
}

impl PeerGrant {
    /// Check the shape and v1 capability constraints of a proposed grant.
    /// Canonical IDs must additionally be checked against the host catalog by the owner API.
    pub fn validate(&self) -> Result<(), InvalidPeerGrant> {
        let Self::Custom {
            sessions,
            filesystem,
            providers,
        } = self
        else {
            return Ok(());
        };
        if *filesystem && !sessions {
            return Err(InvalidPeerGrant::FilesystemRequiresSessions);
        }
        let mut ids = HashSet::with_capacity(providers.len());
        for provider in providers {
            if !valid_id(&provider.provider_id) || !ids.insert(&provider.provider_id) {
                return Err(InvalidPeerGrant::InvalidProviders);
            }
            if let ModelSelection::Selected { model_ids } = &provider.models {
                let mut ids = HashSet::with_capacity(model_ids.len());
                if model_ids.is_empty()
                    || model_ids.iter().any(|id| !valid_id(id) || !ids.insert(id))
                {
                    return Err(InvalidPeerGrant::InvalidModels);
                }
            }
        }
        Ok(())
    }

    /// Whether the peer may access host sessions.
    pub fn allows_sessions(&self, protocol: SharingProtocol) -> bool {
        matches!(self, Self::Full)
            || protocol == SharingProtocol::V1
                && matches!(self, Self::Custom { sessions: true, .. })
    }

    /// Whether the peer may access session workspace files, subject to confinement.
    pub fn allows_filesystem(&self, protocol: SharingProtocol) -> bool {
        matches!(self, Self::Full)
            || protocol == SharingProtocol::V1
                && matches!(
                    self,
                    Self::Custom {
                        sessions: true,
                        filesystem: true,
                        ..
                    }
                )
    }

    /// Whether the peer may invoke the unsandboxed shell, subject to host tool policy.
    pub fn allows_shell(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Check a resolved host provider/model pair. This does not check available quota.
    pub fn allows_model(&self, protocol: SharingProtocol, provider: &str, model: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Custom { providers, .. } if protocol == SharingProtocol::V1 => providers
                .iter()
                .find(|grant| grant.provider_id == provider)
                .is_some_and(|grant| match &grant.models {
                    ModelSelection::All => true,
                    ModelSelection::Selected { model_ids } => {
                        model_ids.iter().any(|id| id == model)
                    }
                }),
            _ => false,
        }
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn peer_access_defaults_to_no_access() {
        let grant = PeerGrant::default();
        for protocol in [SharingProtocol::V1, SharingProtocol::Legacy] {
            assert!(!grant.allows_sessions(protocol));
            assert!(!grant.allows_filesystem(protocol));
            assert!(!grant.allows_model(protocol, "example", "model-1"));
        }
        assert!(!grant.allows_shell());
    }

    #[test]
    fn peer_access_validates_restricted_capability_combinations() {
        let custom = |sessions, filesystem| PeerGrant::Custom {
            sessions,
            filesystem,
            providers: vec![],
        };
        assert_eq!(
            custom(false, true).validate(),
            Err(InvalidPeerGrant::FilesystemRequiresSessions)
        );
        let grant = custom(true, false);
        assert!(grant.allows_sessions(SharingProtocol::V1));
        assert!(!grant.allows_filesystem(SharingProtocol::V1));
        assert!(!grant.allows_shell());
        let invalid = json!({"mode": "custom", "sessions": true, "filesystem": true, "providers": [], "shell": true});
        assert!(serde_json::from_value::<PeerGrant>(invalid).is_err());
    }

    #[test]
    fn peer_access_distinguishes_selected_and_all_models() {
        let grant = PeerGrant::Custom {
            sessions: false,
            filesystem: false,
            providers: vec![
                ProviderGrant {
                    provider_id: "selected".into(),
                    models: ModelSelection::Selected {
                        model_ids: vec!["model-1".into()],
                    },
                },
                ProviderGrant {
                    provider_id: "all".into(),
                    models: ModelSelection::All,
                },
            ],
        };
        grant.validate().unwrap();
        assert!(grant.allows_model(SharingProtocol::V1, "selected", "model-1"));
        assert!(!grant.allows_model(SharingProtocol::V1, "selected", "new-model"));
        assert!(grant.allows_model(SharingProtocol::V1, "all", "new-model"));
        assert!(!grant.allows_model(SharingProtocol::V1, "other", "model-1"));
        assert!(!grant.allows_model(SharingProtocol::Legacy, "all", "new-model"));
        assert!(PeerGrant::Full.allows_model(SharingProtocol::Legacy, "all", "new-model"));
    }
}
