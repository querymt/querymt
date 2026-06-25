//! Agent-owned mesh entry points that remain after moving the concrete runtime to
//! `querymt-remote`.
//!
//! The reusable runtime/bootstrap lives in `querymt-remote`. Agent keeps only the
//! invite-join adapter because it still talks to agent admission/node-manager code.

mod join;

pub use querymt_remote::{
    MeshError, MeshEvent, MeshHandle, MeshRoute, MeshRuntimeConfig, MeshRuntimeHandle, MeshScopeId,
    PeerEvent, RouteTable, bootstrap_mesh_runtime,
};

use querymt_remote::SignedInviteGrant;

pub async fn join_mesh_via_invite(
    invite: &SignedInviteGrant,
    identity_file: Option<std::path::PathBuf>,
) -> Result<MeshRuntimeHandle, MeshError> {
    join::join_mesh_via_invite(invite, identity_file).await
}
