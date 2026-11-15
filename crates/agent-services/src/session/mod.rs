//! Session services: task bookkeeping, history windows, turns, pruning, the
//! runtime context and remote sync.
//!
//! The storage contracts live in `querymt-agent-core`; they are re-exported
//! here under their old paths for the modules of this crate.

pub use error::{SessionError, SessionResult};
pub use querymt_agent_core::session::{
    backend, domain, domain_schedule, error, lifecycle, location, projection, ranges, repository,
    store,
};

pub mod history_window;
pub mod pruning;
pub mod remote_sync;
pub mod run_lease;
pub mod runtime;
pub mod task_service;
pub mod turns;

pub use pruning::{
    PrunableToolResult, PruneAnalysis, PruneConfig, SimpleTokenEstimator, TokenEstimator,
    compute_prune_candidates,
};
pub use runtime::{RuntimeContext, SessionForkHelper};
pub use task_service::TaskService;
