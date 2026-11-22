//! Session, mesh and callback pieces of the public API that the runtime itself
//! needs; `querymt-agent` re-exports them next to `Agent`.

pub mod callbacks;
pub mod mesh;
pub mod session;
pub mod sessions;

pub use callbacks::{
    DelegationCallback, ErrorCallback, MessageCallback, ToolCallCallback, ToolCompleteCallback,
};
pub use mesh::{AgentMesh, MeshJoinOutcome, MeshSpec};
pub use session::AgentSession;
pub use sessions::{
    AcpSessionListError, AgentLoadedSession, AgentSessions, ArchiveFilter, ListSessionsOptions,
    RemoteSessionConnectionState, RemoteSessionMode, ResolutionFilter, SessionChildrenPage,
    SessionChildrenPageWithLifecycle, SessionGroup, SessionGroupWithLifecycle, SessionLifecycle,
    SessionLifecycleFilter, SessionLifecyclePatch, SessionListMode, SessionListPage,
    SessionListPageWithLifecycle, SessionMeta, SessionSummary, SessionSummaryWithLifecycle,
};
