pub mod error;
pub use error::{SessionError, SessionResult};

pub mod domain;
pub mod provider;
pub use provider::{SessionHandle, SessionProvider};
pub mod repository;
pub mod schema;
pub mod sqlite_storage;
pub use sqlite_storage::SqliteStorage;
pub mod store;

// Compaction system (3-layer)
pub mod compaction;
pub mod pruning;
pub use compaction::{
    CompactionResult, RetryConfig as CompactionRetryConfig, SessionCompaction,
    filter_to_effective_history, get_last_compaction, has_compaction,
};
pub use pruning::{
    PrunableToolResult, PruneAnalysis, PruneConfig, SimpleTokenEstimator, TokenEstimator,
    compute_prune_candidates,
};

// Storage backend abstraction
pub mod backend;
pub use backend::StorageBackend;

// Repository implementations
pub mod repo_artifact;
pub mod repo_decision;
pub mod repo_delegation;
pub mod repo_intent;
pub mod repo_progress;
pub mod repo_session;
pub mod repo_task;

pub use repo_artifact::SqliteArtifactRepository;
pub use repo_decision::SqliteDecisionRepository;
pub use repo_delegation::SqliteDelegationRepository;
pub use repo_intent::SqliteIntentRepository;
pub use repo_progress::SqliteProgressRepository;
pub use repo_session::SqliteSessionRepository;
pub use repo_task::SqliteTaskRepository;

// Projection stores
pub mod projection;

pub use projection::{
    AuditView, DefaultRedactor, FieldPredicate, FieldSensitivity, FilterExpr, PredicateOp,
    RedactedView, RedactionPolicy, Redactor, SessionGroup, SessionListFilter, SessionListItem,
    SessionListView, SummaryView, ViewStore,
};

// Phase 3: Runtime integration
pub mod runtime;
pub use runtime::{RuntimeContext, SessionForkHelper};

// Tests
#[cfg(test)]
mod repo_tests;
