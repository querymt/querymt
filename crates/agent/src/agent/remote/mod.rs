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
#[cfg(feature = "remote")]
pub(crate) mod admission;
pub mod dht_name;
pub mod event_forwarder;
#[cfg(feature = "remote")]
pub mod event_relay;
#[cfg(feature = "remote")]
pub mod identity;
#[cfg(feature = "remote")]
pub mod invite;
pub mod node_id;
pub mod node_manager;
#[cfg(feature = "remote")]
pub mod qr;
pub mod routing;
pub mod scope;

#[cfg(feature = "remote")]
pub mod mesh;

#[cfg(feature = "remote")]
pub mod mesh_state;

#[cfg(feature = "remote")]
pub mod transport;

#[cfg(feature = "remote")]
pub mod registry_exchange;

#[cfg(feature = "remote")]
pub mod cached_transport;

#[cfg(feature = "remote")]
pub(crate) mod provider_catalog_backend;

#[cfg(feature = "remote")]
pub(crate) mod provider_host_backend;

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
pub(crate) mod test_helpers;

#[cfg(all(test, feature = "remote"))]
mod provider_host_tests;

#[cfg(all(test, feature = "remote"))]
mod mesh_provider_tests;

#[cfg(all(test, feature = "remote"))]
mod remote_agent_stub_tests;

#[cfg(all(test, feature = "remote"))]
mod remote_setup_tests;

#[cfg(all(test, feature = "remote"))]
mod session_actor_ref_remote_tests;

#[cfg(all(test, feature = "remote"))]
mod event_relay_mesh_tests;

#[cfg(all(test, feature = "remote"))]
mod integration_tests;

#[cfg(all(test, feature = "remote"))]
mod provider_routing_tests;

#[cfg(all(test, feature = "remote"))]
mod concurrent_materialization_tests;

#[cfg(all(test, feature = "remote"))]
mod provider_host_responsiveness_tests;

pub use actor_ref::SessionActorRef;
#[cfg(feature = "remote")]
pub use cached_transport::{CachedDynMeshTransport, CachedMeshTransport};
pub use event_forwarder::EventForwarder;
#[cfg(feature = "remote")]
pub use event_relay::{EventRelayActor, RelayedEvent};
#[cfg(feature = "remote")]
pub use mesh::join_mesh_via_invite;
#[cfg(feature = "remote")]
pub use mesh::{MeshError, MeshHandle, PeerEvent, bootstrap_mesh_runtime};
#[cfg(feature = "remote")]
pub use mesh_state::{MeshLocalRole, MeshStateEntry, MeshStateStore, MeshStatus};
pub use node_id::NodeId;
#[cfg(feature = "remote")]
pub use node_manager::{
    CreateRemoteSchedule, CreateRemoteScheduleResponse, CreateRemoteSession,
    CreateRemoteSessionResponse, DeleteRemoteSchedule, ForkRemoteSession,
    ForkRemoteSessionResponse, GetNodeInfo, ListRemoteSchedules, ListRemoteSchedulesResponse,
    ListRemoteSessions, PauseRemoteSchedule, RemoteNodeManager, ResumeRemoteSchedule,
    ResumeRemoteSession, StopRemoteSessionRuntime, TriggerRemoteSchedule,
};
pub use node_manager::{ListRemoteSessionsResponse, NodeInfo, RemoteSessionInfo};
#[cfg(feature = "remote")]
pub use querymt_remote::MeshChatProvider;
#[cfg(feature = "remote")]
pub use querymt_remote::{
    DirectoryMode, IrohMeshConfig, LanDiscovery, LanMeshConfig, MeshRuntimeConfig,
    MeshTransportMode,
};
pub use querymt_remote::{ProviderHostActor, StreamReceiverActor};

#[cfg(feature = "remote")]
pub use querymt_remote::{
    CancelProviderStreamRequest, GetProviderStreamStatus, ProviderChatRequest,
    ProviderChatResponse, ProviderStreamPhase, ProviderStreamRequest, ProviderStreamStatus,
    RenewProviderStreamLease, StreamChunkRelay, StreamRelayMessage,
};
#[cfg(feature = "remote")]
pub use querymt_remote::{
    GetRouterStatus, ProviderStreamRouterActor as SessionStreamRouterActor, RequestPhase,
    RoutedRequestStatus, RoutedStreamRelayMessage,
};
#[cfg(feature = "remote")]
pub use querymt_remote::{MeshRuntimeHandle, MeshScopeHandle};
#[cfg(feature = "remote")]
pub use registry_exchange::{
    GetRegistrations, NotifyRegistration, RegistrationEntry, RegistryExchangeActor,
};
#[cfg(feature = "remote")]
pub use remote_handle::RemoteAgentHandle;
#[cfg(feature = "remote")]
pub use remote_setup::{
    LocalMeshActorRefs, register_local_mesh_actor_scope, register_remote_agents_from_config,
    spawn_and_register_local_mesh_actors, spawn_and_register_local_mesh_actors_with_name,
};
pub use routing::{
    ClearRoute, ListRoutes, ResolvePeer, RouteConfirmation, RouteTarget, RoutingActor,
    RoutingPolicy, RoutingSnapshot, RoutingSnapshotHandle, SetProviderTarget, SetSessionTarget,
    UnresolvePeer, new_routing_snapshot_handle,
};
pub use scope::{MeshScopeId, MeshTransportKind};
#[cfg(feature = "remote")]
pub use transport::{DynMeshTransport, MeshTransport};
