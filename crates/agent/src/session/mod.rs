pub use error::{SessionError, SessionResult};
pub use querymt_agent_core::session::{
    domain, domain_schedule, error, lifecycle, location, projection, ranges, repository, store,
};

pub mod provider;
pub mod provider_config;
pub use provider::{SessionHandle, SessionProvider};
pub use querymt_agent_services::session::run_lease;
pub mod run_reconcile;
pub use querymt_agent_services::session::task_service;
pub use querymt_agent_sqlite::session::{schema, sqlite_storage};
pub use sqlite_storage::SqliteStorage;
pub use task_service::TaskService;

// Compaction system (3-layer)
pub mod compaction;
pub use compaction::{
    CompactionResult, RetryConfig as CompactionRetryConfig, SessionCompaction,
    filter_to_effective_history, get_last_compaction, has_compaction,
};
pub use pruning::{
    PrunableToolResult, PruneAnalysis, PruneConfig, SimpleTokenEstimator, TokenEstimator,
    compute_prune_candidates,
};
pub use querymt_agent_services::session::pruning;

// Storage backend abstraction
pub mod backend;
pub use backend::StorageBackend;

// Repository implementations
pub use querymt_agent_sqlite::repo_schedule;
pub use querymt_agent_sqlite::session::{
    repo_artifact, repo_decision, repo_delegation, repo_intent, repo_progress, repo_session,
    repo_task,
};

pub use repo_artifact::SqliteArtifactRepository;
pub use repo_decision::SqliteDecisionRepository;
pub use repo_delegation::SqliteDelegationRepository;
pub use repo_intent::SqliteIntentRepository;
pub use repo_progress::SqliteProgressRepository;
pub use repo_schedule::SqliteScheduleRepository;
pub use repo_session::SqliteSessionRepository;
pub use repo_task::SqliteTaskRepository;

// Schedule domain types
pub use domain_schedule::{
    EventTriggerFilter, Schedule, ScheduleConfig, ScheduleExecutionLimits, ScheduleState,
    ScheduleTrigger,
};
pub use repository::ScheduleRepository;

// Projection stores
pub use querymt_agent_services::session::{history_window, remote_sync, turns};
pub mod load_snapshot;

pub use load_snapshot::{
    SessionLoadSnapshot, StreamCursor, cursor_from_events, load_session_snapshot,
};
pub use projection::{
    AuditView, DefaultRedactor, FieldPredicate, FieldSensitivity, FilterExpr, PredicateOp,
    RedactedView, RedactionPolicy, Redactor, SessionGroup, SessionListFilter, SessionListItem,
    SessionListView, SummaryView, ViewStore,
};

// Phase 3: Runtime integration
pub use querymt_agent_services::session::runtime;
pub use runtime::{RuntimeContext, SessionForkHelper};
