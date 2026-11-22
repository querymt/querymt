//! Guarded mesh bootstrap and the agent's invite admission adapter.

mod join;

pub use join::join_mesh_via_invite;
pub use querymt_remote::bootstrap_mesh_runtime_with_policy as bootstrap_mesh_runtime;
pub use querymt_remote::{
    MeshError, MeshEvent, MeshHandle, MeshRoute, MeshRuntimeConfig, MeshRuntimeHandle, MeshScopeId,
    PeerEvent, RouteTable,
};
