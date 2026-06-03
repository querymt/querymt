#![doc = "Reusable mesh and remote provider primitives for QueryMT."]

pub mod identity;
pub mod invite;
#[cfg(feature = "kameo-mesh")]
pub mod mesh_bootstrap;
pub mod mesh_config;
#[cfg(feature = "kameo-mesh")]
pub mod mesh_events;
#[cfg(feature = "kameo-mesh")]
pub mod mesh_handle;
#[cfg(feature = "kameo-mesh")]
pub mod mesh_routes;
#[cfg(feature = "kameo-mesh")]
pub mod mesh_runtime;
pub mod mesh_runtime_config;
#[cfg(feature = "kameo-mesh")]
pub mod mesh_runtime_support;
pub mod mesh_state;
pub mod names;
pub mod node_id;
pub mod provider_backend;
pub mod provider_backend_helpers;
pub mod provider_catalog;
pub mod provider_client;
pub mod provider_client_runtime;
pub mod provider_protocol;
pub mod provider_host_actor;
pub mod provider_host_error;
pub mod provider_host_support;
#[cfg(feature = "kameo-mesh")]
pub mod mesh_client;
pub mod remote_chat_provider;
pub mod provider_stream_state;
pub mod runtime_helpers;
#[cfg(feature = "kameo-mesh")]
pub mod runtime_handle;
pub mod provider_transport;
pub mod provider_stream_router;
pub mod provider_share;
pub mod scope;
pub mod stream_router_protocol;
#[cfg(feature = "qr")]
pub mod qr;

pub use identity::*;
pub use invite::*;
pub use mesh_config::{MeshError, MeshTransportMode};
#[cfg(feature = "kameo-mesh")]
pub use mesh_events::{MeshEvent, PeerEvent};
#[cfg(feature = "kameo-mesh")]
pub use mesh_handle::{MeshHandle, ReRegisterFn};
#[cfg(feature = "kameo-mesh")]
pub use mesh_routes::{MeshRoute, RouteTable};
#[cfg(feature = "kameo-mesh")]
pub use mesh_runtime::{bootstrap_mesh_handle, bootstrap_mesh_runtime};
pub use mesh_runtime_config::{
    DirectoryMode, IrohMeshConfig, LanDiscovery, LanMeshConfig, MeshRuntimeConfig,
};
pub use mesh_state::{
    MeshLocalRole, MeshState, MeshStateEntry, MeshStateStore, MeshStatus, default_mesh_state_path,
};
pub use names::{event_relay, node_manager_for_peer, provider_host, session, NODE_MANAGER};
pub use node_id::NodeId;
pub use provider_backend::{ProviderBuildRequest, RemoteProviderBackend};
pub use provider_backend_helpers::{
    ClosureProviderBackend, ModelAllowlistBackend, RegistryProviderBackend, StaticCatalogBackend,
    StaticProviderBackend,
};
pub use provider_client::RemoteProviderClientConfig;
pub use provider_client_runtime::{
    PeerAliveFuture, RemoteProviderClientCore, RemoteProviderClientTransport, RenewLeaseFuture,
    StreamPeerAliveFn, StreamRenewFn,
};
pub use provider_catalog::{
    GetProviderCatalog, ProviderCatalogActor, ProviderCatalogBackend, ProviderCatalogEntry,
    ProviderCatalogNodeInfo, ProviderCatalogSnapshot, fallback_provider_host_catalog,
};
#[cfg(feature = "kameo-mesh")]
pub use mesh_client::{KameoMeshClientTransport, MeshChatProvider, find_provider_on_mesh};
pub use provider_host_actor::ProviderHostActor;
pub use provider_host_error::RemoteProviderHostError;
pub use provider_host_support::{
    StreamReceiverActor, build_provider_for_request, merge_remote_provider_params,
    params_for_remote_provider,
};
pub use provider_protocol::{
    CancelProviderStreamRequest, GenericProviderStreamRequest, GetProviderStreamStatus,
    ProviderChatRequest, ProviderChatResponse, ProviderStreamPhase, ProviderStreamRequest,
    ProviderStreamStatus, RenewProviderStreamLease, StreamChunkRelay, StreamRelayMessage,
    default_stream_heartbeat_secs, default_stream_lease_ttl_secs, keep_stream_message_buffered,
    relay_message_is_terminal, should_ack_relay_message,
};
pub use provider_stream_state::RemoteProviderStreamState;
pub use provider_share::ProviderShare;
pub use runtime_helpers::{enabled_transports_from_mode, mode_has_transport, scoped_actor_name};
#[cfg(feature = "kameo-mesh")]
pub use runtime_handle::{MeshRuntimeHandle, MeshScopeHandle};
pub use remote_chat_provider::RemoteChatProvider;
pub use provider_stream_router::{
    AttachStreamConsumer, DetachStreamConsumer, ProviderStreamRouterActor, RegisterRequest,
    RouterError,
};
pub use provider_transport::{
    decode_payload_handler_error, remote_send_error_base, remote_send_error_to_llm_error_no_handler,
    should_retry_remote_send,
};
pub use scope::scoped_provider_catalog;
pub use scope::{
    MeshScopeId, MeshTransportKind, scoped_event_relay, scoped_node_manager,
    scoped_node_manager_for_peer, scoped_provider_host, scoped_session,
};
pub use stream_router_protocol::{
    GetRouterStatus, RequestPhase, RoutedRequestStatus, RoutedStreamRelayMessage,
    terminal_request_phase,
};
