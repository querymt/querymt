//! Scope types and scoped DHT naming for multi-transport mesh isolation.
//!
//! This module introduces the logical scope model described in the
//! multi-transport scoped mesh plan.  A *scope* is a namespace boundary
//! for DHT actor registration — it determines the key prefix used when
//! publishing or looking up remote actors.
//!
//! ## Core types
//!
//! - [`MeshScopeId`] — identifies a logical mesh scope (LAN ambient, or an
//!   Iroh mesh identified by `mesh_id`).
//! - [`MeshTransportKind`] — labels a physical transport mechanism.  This is
//!   route metadata, not a runtime mode.
//!
//! ## Scoped DHT helpers
//!
//! The `scoped_*` functions mirror the existing helpers in
//! [`super::dht_name`] but accept a [`MeshScopeId`] parameter.  For
//! [`MeshScopeId::Lan`] they produce byte-for-byte identical output to the
//! unscoped versions, preserving full backward compatibility.

use std::fmt;

// ── Scope identity ────────────────────────────────────────────────────────────

/// Logical namespace / membership boundary for the mesh.
///
/// LAN uses an empty DHT prefix (backward compatible with existing
/// registrations).  Each Iroh mesh gets its own prefix derived from the
/// `mesh_id`, preventing accidental cross-scope lookups.
///
/// # Security note
///
/// Scoped DHT names provide *discovery isolation*, not cryptographic access
/// control.  A malicious peer that knows a scope's `mesh_id` can query the
/// same DHT keys.  Invite verification and admission enforcement remain
/// necessary for real security.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MeshScopeId {
    /// Ambient LAN scope — maps to no DHT prefix.
    Lan,
    /// An Iroh mesh scope identified by a stable mesh identifier derived
    /// from invite grants.
    Iroh { mesh_id: String },
}

impl MeshScopeId {
    /// Return the DHT key prefix for this scope.
    ///
    /// - [`Lan`](MeshScopeId::Lan) → `""` (empty string, backward compatible)
    /// - [`Iroh`](MeshScopeId::Iroh) → `"scope::{mesh_id}::"`
    pub fn dht_prefix(&self) -> String {
        match self {
            Self::Lan => String::new(),
            Self::Iroh { mesh_id } => format!("scope::{}::", mesh_id),
        }
    }

    /// Returns `true` if this is the LAN ambient scope.
    pub fn is_lan(&self) -> bool {
        matches!(self, Self::Lan)
    }

    /// Returns `true` if this is an Iroh scope.
    pub fn is_iroh(&self) -> bool {
        matches!(self, Self::Iroh { .. })
    }

    /// Return the `mesh_id` if this is an Iroh scope, `None` otherwise.
    pub fn iroh_mesh_id(&self) -> Option<&str> {
        match self {
            Self::Lan => None,
            Self::Iroh { mesh_id } => Some(mesh_id),
        }
    }
}

impl fmt::Display for MeshScopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lan => write!(f, "lan"),
            Self::Iroh { mesh_id } => write!(f, "iroh:{}", mesh_id),
        }
    }
}

// ── Transport kind ────────────────────────────────────────────────────────────

/// Physical reachability mechanism.
///
/// Transport kind is *route metadata*: it records how a peer was discovered
/// or is reachable.  It is not a runtime mode — the runtime supports multiple
/// transports simultaneously.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MeshTransportKind {
    /// LAN TCP / QUIC transport (local network, mDNS discovery).
    Lan,
    /// Iroh QUIC/relay transport (NAT traversal, internet connectivity).
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

// ── Scoped DHT name helpers ───────────────────────────────────────────────────

/// Scoped DHT name for the global `RemoteNodeManager` singleton.
///
/// For [`MeshScopeId::Lan`] this returns `"node_manager"` — identical to
/// [`super::dht_name::NODE_MANAGER`].
pub fn scoped_node_manager(scope: &MeshScopeId) -> String {
    format!("{}node_manager", scope.dht_prefix())
}

/// Scoped DHT name for a per-peer `RemoteNodeManager`.
///
/// For [`MeshScopeId::Lan`] this returns `"node_manager::peer::{peer_id}"` —
/// identical to [`super::dht_name::node_manager_for_peer`].
pub fn scoped_node_manager_for_peer(scope: &MeshScopeId, peer_id: &impl fmt::Display) -> String {
    format!("{}node_manager::peer::{}", scope.dht_prefix(), peer_id)
}

/// Scoped DHT name for a `ProviderHostActor`.
///
/// For [`MeshScopeId::Lan`] this returns `"provider_host::peer::{peer_id}"` —
/// identical to [`super::dht_name::provider_host`].
pub fn scoped_provider_host(scope: &MeshScopeId, peer_id: &impl fmt::Display) -> String {
    format!("{}provider_host::peer::{}", scope.dht_prefix(), peer_id)
}

/// Scoped DHT name for a remote `SessionActor`.
///
/// For [`MeshScopeId::Lan`] this returns `"session::{session_id}"` —
/// identical to [`super::dht_name::session`].
pub fn scoped_session(scope: &MeshScopeId, session_id: &str) -> String {
    format!("{}session::{}", scope.dht_prefix(), session_id)
}

/// Scoped DHT name for an `EventRelayActor`.
///
/// For [`MeshScopeId::Lan`] this returns `"event_relay::{session_id}::{peer_id}"` —
/// identical to [`super::dht_name::event_relay`].
pub fn scoped_event_relay(
    scope: &MeshScopeId,
    session_id: &str,
    peer_id: &impl fmt::Display,
) -> String {
    format!(
        "{}event_relay::{}::{}",
        scope.dht_prefix(),
        session_id,
        peer_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::remote::dht_name;

    // ── MeshScopeId ─────────────────────────────────────────────────────────

    #[test]
    fn lan_dht_prefix_is_empty() {
        assert_eq!(MeshScopeId::Lan.dht_prefix(), "");
    }

    #[test]
    fn iroh_dht_prefix_is_scoped() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "personal".to_string(),
        };
        assert_eq!(scope.dht_prefix(), "scope::personal::");
    }

    #[test]
    fn lan_is_lan() {
        assert!(MeshScopeId::Lan.is_lan());
        assert!(!MeshScopeId::Lan.is_iroh());
    }

    #[test]
    fn iroh_is_iroh() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "test".to_string(),
        };
        assert!(scope.is_iroh());
        assert!(!scope.is_lan());
    }

    #[test]
    fn iroh_mesh_id_returns_id() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "my-mesh".to_string(),
        };
        assert_eq!(scope.iroh_mesh_id(), Some("my-mesh"));
        assert_eq!(MeshScopeId::Lan.iroh_mesh_id(), None);
    }

    #[test]
    fn display_lan() {
        assert_eq!(format!("{}", MeshScopeId::Lan), "lan");
    }

    #[test]
    fn display_iroh() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "personal".to_string(),
        };
        assert_eq!(format!("{}", scope), "iroh:personal");
    }

    // ── Scoped DHT names: LAN backward compatibility ────────────────────────
    // These tests verify that scoped_* helpers with MeshScopeId::Lan produce
    // byte-for-byte identical output to the existing dht_name functions.

    #[test]
    fn scoped_node_manager_lan_equals_unscoped() {
        assert_eq!(
            scoped_node_manager(&MeshScopeId::Lan),
            dht_name::NODE_MANAGER
        );
    }

    #[test]
    fn scoped_node_manager_for_peer_lan_equals_unscoped() {
        let peer_id = "12D3KooWABC";
        assert_eq!(
            scoped_node_manager_for_peer(&MeshScopeId::Lan, &peer_id),
            dht_name::node_manager_for_peer(&peer_id)
        );
    }

    #[test]
    fn scoped_provider_host_lan_equals_unscoped() {
        let peer_id = "12D3KooWABC";
        assert_eq!(
            scoped_provider_host(&MeshScopeId::Lan, &peer_id),
            dht_name::provider_host(&peer_id)
        );
    }

    #[test]
    fn scoped_session_lan_equals_unscoped() {
        let sid = "abc-123";
        assert_eq!(
            scoped_session(&MeshScopeId::Lan, sid),
            dht_name::session(sid)
        );
    }

    #[test]
    fn scoped_event_relay_lan_equals_unscoped() {
        let sid = "abc-123";
        let peer_id = "12D3KooWABC";
        assert_eq!(
            scoped_event_relay(&MeshScopeId::Lan, sid, &peer_id),
            dht_name::event_relay(sid, &peer_id)
        );
    }

    // ── Scoped DHT names: Iroh prefixing ────────────────────────────────────

    #[test]
    fn iroh_node_manager_is_prefixed() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };
        assert_eq!(scoped_node_manager(&scope), "scope::team-a::node_manager");
    }

    #[test]
    fn iroh_node_manager_for_peer_is_prefixed() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };
        assert_eq!(
            scoped_node_manager_for_peer(&scope, &"12D3KooWABC"),
            "scope::team-a::node_manager::peer::12D3KooWABC"
        );
    }

    #[test]
    fn iroh_provider_host_is_prefixed() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };
        assert_eq!(
            scoped_provider_host(&scope, &"12D3KooWABC"),
            "scope::team-a::provider_host::peer::12D3KooWABC"
        );
    }

    #[test]
    fn iroh_session_is_prefixed() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };
        assert_eq!(
            scoped_session(&scope, "sess-1"),
            "scope::team-a::session::sess-1"
        );
    }

    #[test]
    fn iroh_event_relay_is_prefixed() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };
        assert_eq!(
            scoped_event_relay(&scope, "sess-1", &"12D3KooWABC"),
            "scope::team-a::event_relay::sess-1::12D3KooWABC"
        );
    }

    // ── Non-collision between scopes ────────────────────────────────────────

    #[test]
    fn different_iroh_scopes_dont_collide() {
        let scope_a = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };
        let scope_b = MeshScopeId::Iroh {
            mesh_id: "team-b".to_string(),
        };

        assert_ne!(scoped_node_manager(&scope_a), scoped_node_manager(&scope_b));
        assert_ne!(
            scoped_session(&scope_a, "sess-1"),
            scoped_session(&scope_b, "sess-1")
        );
        assert_ne!(
            scoped_provider_host(&scope_a, &"12D3KooWABC"),
            scoped_provider_host(&scope_b, &"12D3KooWABC")
        );
    }

    #[test]
    fn lan_and_iroh_scopes_dont_collide() {
        let iroh_scope = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };

        // All scoped names must differ between LAN and Iroh
        assert_ne!(
            scoped_node_manager(&MeshScopeId::Lan),
            scoped_node_manager(&iroh_scope)
        );
        assert_ne!(
            scoped_session(&MeshScopeId::Lan, "sess-1"),
            scoped_session(&iroh_scope, "sess-1")
        );
        assert_ne!(
            scoped_provider_host(&MeshScopeId::Lan, &"12D3KooWABC"),
            scoped_provider_host(&iroh_scope, &"12D3KooWABC")
        );
        assert_ne!(
            scoped_event_relay(&MeshScopeId::Lan, "sess-1", &"12D3KooWABC"),
            scoped_event_relay(&iroh_scope, "sess-1", &"12D3KooWABC")
        );
    }

    #[test]
    fn same_name_different_scope_no_collision_node_manager_for_peer() {
        let peer_id = "12D3KooWABC";
        let scope_a = MeshScopeId::Iroh {
            mesh_id: "mesh-1".to_string(),
        };
        let scope_b = MeshScopeId::Iroh {
            mesh_id: "mesh-2".to_string(),
        };
        assert_ne!(
            scoped_node_manager_for_peer(&scope_a, &peer_id),
            scoped_node_manager_for_peer(&scope_b, &peer_id)
        );
        // Also distinct from LAN
        assert_ne!(
            scoped_node_manager_for_peer(&MeshScopeId::Lan, &peer_id),
            scoped_node_manager_for_peer(&scope_a, &peer_id)
        );
    }

    // ── MeshTransportKind ───────────────────────────────────────────────────

    #[test]
    fn transport_kind_display() {
        assert_eq!(format!("{}", MeshTransportKind::Lan), "lan");
        assert_eq!(format!("{}", MeshTransportKind::Iroh), "iroh");
    }
}
