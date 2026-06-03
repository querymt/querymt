//! Scope types and scoped DHT naming for multi-transport mesh isolation.
//!
//! A *scope* is a namespace boundary for DHT actor registration: it determines
//! the key prefix used when publishing or looking up remote actors.

use std::fmt;

/// Logical namespace / membership boundary for the mesh.
///
/// Scoped DHT names provide discovery isolation, not cryptographic access
/// control. Invite verification and admission enforcement remain necessary for
/// real security.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MeshScopeId {
    /// LAN scope identified by a stable LAN ID.
    Lan { lan_id: String },
    /// An Iroh mesh scope identified by a stable mesh identifier derived from
    /// invite grants.
    Iroh { mesh_id: String },
}

impl MeshScopeId {
    fn encode_mesh_id(mesh_id: &str) -> String {
        mesh_id
            .bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{:02X}", b),
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub const DEFAULT_LAN_ID: &'static str = "default";

    pub fn lan_default() -> Self {
        Self::Lan {
            lan_id: Self::DEFAULT_LAN_ID.to_string(),
        }
    }

    pub fn dht_prefix(&self) -> String {
        match self {
            Self::Lan { lan_id } => format!("scope::lan::{}::", Self::encode_mesh_id(lan_id)),
            Self::Iroh { mesh_id } => format!("scope::iroh::{}::", Self::encode_mesh_id(mesh_id)),
        }
    }

    pub fn is_lan(&self) -> bool {
        matches!(self, Self::Lan { .. })
    }

    pub fn is_iroh(&self) -> bool {
        matches!(self, Self::Iroh { .. })
    }

    pub fn iroh_mesh_id(&self) -> Option<&str> {
        match self {
            Self::Lan { .. } => None,
            Self::Iroh { mesh_id } => Some(mesh_id),
        }
    }
}

impl fmt::Display for MeshScopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lan { lan_id } => write!(f, "lan:{}", lan_id),
            Self::Iroh { mesh_id } => write!(f, "iroh:{}", mesh_id),
        }
    }
}

/// Physical reachability mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MeshTransportKind {
    Lan,
    Iroh,
}

impl fmt::Display for MeshTransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lan => write!(f, "lan"),
            Self::Iroh => write!(f, "iroh"),
        }
    }
}

pub fn scoped_node_manager(scope: &MeshScopeId) -> String {
    format!("{}node_manager", scope.dht_prefix())
}

pub fn scoped_node_manager_for_peer(
    scope: &MeshScopeId,
    peer_id: &(impl fmt::Display + ?Sized),
) -> String {
    format!("{}node_manager::peer::{}", scope.dht_prefix(), peer_id)
}

pub fn scoped_provider_host(scope: &MeshScopeId, peer_id: &(impl fmt::Display + ?Sized)) -> String {
    format!("{}provider_host::peer::{}", scope.dht_prefix(), peer_id)
}

pub fn scoped_provider_catalog(
    scope: &MeshScopeId,
    peer_id: &(impl fmt::Display + ?Sized),
) -> String {
    format!("{}provider_catalog::peer::{}", scope.dht_prefix(), peer_id)
}

pub fn scoped_session(scope: &MeshScopeId, session_id: &str) -> String {
    format!("{}session::{}", scope.dht_prefix(), session_id)
}

pub fn scoped_event_relay(
    scope: &MeshScopeId,
    session_id: &str,
    peer_id: &(impl fmt::Display + ?Sized),
) -> String {
    format!(
        "{}event_relay::{}::{}",
        scope.dht_prefix(),
        session_id,
        peer_id
    )
}
