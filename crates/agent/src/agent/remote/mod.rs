//! Remote actor support for cross-machine session management.
//!
//! This module provides `SessionActorRef`, a location-transparent handle to a
//! `SessionActor` that works identically whether the actor is local or on a
//! remote machine in the kameo mesh.
//!
//! ## Feature gating
//!
//! The `Remote` variant of `SessionActorRef` and all libp2p/mesh functionality
//! are gated behind the `remote` cargo feature. Without it, only the `Local`
//! variant exists, and this module simply re-exports a thin wrapper around
//! `ActorRef<SessionActor>`.

pub mod actor_ref;
pub mod dht_name;
pub mod event_forwarder;
pub mod event_relay;
pub mod node_id;
pub mod node_manager;
pub mod routing;

#[cfg(feature = "remote")]
pub mod mesh;

#[cfg(feature = "remote")]
pub mod mesh_provider;

#[cfg(feature = "remote")]
pub mod transport;

#[cfg(feature = "remote")]
pub mod registry_exchange;

#[cfg(feature = "remote")]
pub mod cached_transport;

#[cfg(feature = "remote")]
pub mod provider_host;

#[cfg(feature = "remote")]
pub mod remote_handle;

#[cfg(feature = "remote")]
pub mod remote_setup;

#[cfg(feature = "remote")]
mod remote_actor_impl;

#[cfg(test)]
mod tests;

// ── Test modules (remote feature) ────────────────────────────────────────────

#[cfg(all(test, feature = "remote"))]
mod test_helpers;

#[cfg(all(test, feature = "remote"))]
mod provider_host_tests;

#[cfg(all(test, feature = "remote"))]
mod mesh_provider_tests;

#[cfg(all(test, feature = "remote"))]
mod remote_agent_stub_tests;

#[cfg(all(test, feature = "remote"))]
mod session_actor_ref_remote_tests;

#[cfg(all(test, feature = "remote"))]
mod event_relay_mesh_tests;

#[cfg(all(test, feature = "remote"))]
mod integration_tests;

#[cfg(all(test, feature = "remote"))]
mod provider_routing_tests;

pub use actor_ref::SessionActorRef;
#[cfg(feature = "remote")]
pub use cached_transport::{CachedDynMeshTransport, CachedMeshTransport};
pub use event_forwarder::EventForwarder;
pub use event_relay::{EventRelayActor, RelayedEvent};
#[cfg(feature = "remote")]
pub use mesh::{
    DirectoryMode, MeshConfig, MeshDiscovery, MeshError, MeshHandle, PeerEvent, bootstrap_mesh,
    bootstrap_mesh_default,
};
#[cfg(feature = "remote")]
pub use mesh_provider::MeshChatProvider;
pub use node_id::NodeId;
pub use node_manager::{AvailableModel, NodeInfo, RemoteSessionInfo};
#[cfg(feature = "remote")]
pub use node_manager::{
    CreateRemoteSession, CreateRemoteSessionResponse, DestroyRemoteSession, GetNodeInfo,
    ListAvailableModels, ListRemoteSessions, RemoteNodeManager,
};
#[cfg(feature = "remote")]
pub use provider_host::{
    ProviderChatRequest, ProviderChatResponse, ProviderHostActor, ProviderStreamRequest,
    StreamChunkRelay, StreamReceiverActor,
};
#[cfg(feature = "remote")]
pub use registry_exchange::{
    GetRegistrations, NotifyRegistration, RegistrationEntry, RegistryExchangeActor,
};
#[cfg(feature = "remote")]
pub use remote_handle::RemoteAgentHandle;
#[cfg(feature = "remote")]
pub use remote_setup::{MeshSetupResult, setup_mesh_from_config};
pub use routing::{
    ClearRoute, ListRoutes, ResolvePeer, RouteConfirmation, RouteTarget, RoutingActor,
    RoutingPolicy, RoutingSnapshot, RoutingSnapshotHandle, SetProviderTarget, SetSessionTarget,
    UnresolvePeer, new_routing_snapshot_handle,
};
#[cfg(feature = "remote")]
pub use transport::{DynMeshTransport, MeshTransport};
