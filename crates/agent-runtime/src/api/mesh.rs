//! Public mesh API plus remote-only support helpers split into focused submodules.

#[cfg(feature = "remote")]
pub mod invite;
#[cfg(feature = "remote")]
pub mod runtime;
#[cfg(feature = "remote")]
pub mod runtime_config;
pub mod types;

#[cfg(feature = "remote")]
pub(crate) use invite::admit_via_invite_on_runtime;
pub use types::{AgentMesh, Mesh, MeshJoinOutcome, MeshRuntime, MeshSpec};
