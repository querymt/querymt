//! Knowledge layer — pluggable structured memory.
//!
//! Provides a scope-generic knowledge store for ingesting, consolidating,
//! querying, and retaining knowledge entries. The scoping is a parameter,
//! not a structural constraint:
//!
//! - A session-scoped memory agent uses `scope = session_public_id`.
//! - A global knowledge agent uses `scope = "global"`.
//! - A project-scoped agent uses `scope = "project:myapp"`.
//!
//! ## Design Rationale
//!
//! The consolidation/ingestion/query logic lives in system prompts, not in
//! Rust code. The LLM decides what to consolidate. The tools provide CRUD.
//! This module contains the store trait, domain types, and SQLite
//! implementation — no embedded AI logic.

pub mod sqlite;
pub mod text_processing;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// ─── Domain Types ────────────────────────────────────────────────────────────

/// Request to ingest a piece of raw information into the knowledge store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestRequest {
    /// Where this information came from (e.g. "user_message", "tool_output", "session:abc").
    pub source: String,
    /// The raw text of the information.
    pub raw_text: String,
    /// A concise summary of the information.
    pub summary: String,
    /// Named entities extracted from the text.
    #[serde(default)]
    pub entities: Vec<String>,
    /// Topics/tags for categorization.
    #[serde(default)]
    pub topics: Vec<String>,
    /// Cross-references or relationships to other entries/concepts.
    #[serde(default)]
    pub connections: Vec<String>,
    /// Importance score from 0.0 (trivial) to 1.0 (critical).
    #[serde(default = "default_importance")]
    pub importance: f64,
}

fn default_importance() -> f64 {
    0.5
}

/// A persisted knowledge entry.
///
/// ## Identity Contract
/// - `id: i64` is internal only (never leaves repository/API boundary).
/// - `public_id` is the stable external identity used by tools, events, and APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    /// Internal DB identity (never leaves repository/API boundary).
    #[serde(skip)]
    pub id: i64,
    /// Stable external identity.
    pub public_id: String,
    /// Scope this entry belongs to.
    pub scope: String,
    /// Source of the information.
    pub source: String,
    /// Raw text — nullable to support retention archive mode.
    pub raw_text: Option<String>,
    /// Concise summary (always present, survives archiving).
    pub summary: String,
    /// Named entities.
    #[serde(default)]
    pub entities: Vec<String>,
    /// Topics/tags.
    #[serde(default)]
    pub topics: Vec<String>,
    /// Cross-references.
    #[serde(default)]
    pub connections: Vec<String>,
    /// Importance score 0.0–1.0.
    pub importance: f64,
    /// When this entry was consolidated (None = unconsolidated).
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub consolidated_at: Option<OffsetDateTime>,
    /// When the entry was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A consolidation — a synthesis of multiple knowledge entries.
///
/// ## Identity Contract
/// - `id: i64` is internal only.
/// - `public_id` is the stable external identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Consolidation {
    #[serde(skip)]
    pub id: i64,
    pub public_id: String,
    pub scope: String,
    /// Public IDs of the source entries that were consolidated.
    pub source_entry_public_ids: Vec<String>,
    /// Summary of the consolidated insight.
    pub summary: String,
    /// Key insight derived from the consolidation.
    pub insight: String,
    /// Cross-references discovered during consolidation.
    #[serde(default)]
    pub connections: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Result of a knowledge query: entries + consolidations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeQueryResult {
    pub entries: Vec<KnowledgeEntry>,
    pub consolidations: Vec<Consolidation>,
}

/// Statistics for a knowledge scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeStats {
    pub total_entries: u64,
    pub unconsolidated_entries: u64,
    pub total_consolidations: u64,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub latest_entry_at: Option<OffsetDateTime>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub latest_consolidation_at: Option<OffsetDateTime>,
}

/// Filter parameters for listing knowledge entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeFilter {
    /// Filter by topics (entries must match at least one).
    pub topics: Option<Vec<String>>,
    /// Filter by entities (entries must match at least one).
    pub entities: Option<Vec<String>>,
    /// Filter by creation time (entries created after this time).
    pub since: Option<OffsetDateTime>,
    /// Filter by consolidation status.
    pub consolidated: Option<bool>,
    /// Maximum number of entries to return.
    #[serde(default = "default_filter_limit")]
    pub limit: usize,
}

fn default_filter_limit() -> usize {
    50
}

impl Default for KnowledgeFilter {
    fn default() -> Self {
        Self {
            topics: None,
            entities: None,
            since: None,
            consolidated: None,
            limit: default_filter_limit(),
        }
    }
}

/// Options for knowledge queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryOpts {
    /// Maximum number of results.
    #[serde(default = "default_query_limit")]
    pub limit: usize,
    /// Whether to include consolidations in results.
    #[serde(default = "default_true")]
    pub include_consolidations: bool,
    /// How to retrieve and rank results.
    #[serde(default)]
    pub retrieval_mode: RetrievalMode,
}

fn default_query_limit() -> usize {
    20
}

fn default_true() -> bool {
    true
}

impl Default for QueryOpts {
    fn default() -> Self {
        Self {
            limit: default_query_limit(),
            include_consolidations: true,
            retrieval_mode: RetrievalMode::default(),
        }
    }
}

/// How knowledge queries retrieve and rank results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    /// Full-text search over summary/raw_text/source.
    #[default]
    Keyword,
    /// Keyword search + structured boosts (entities/topics/importance).
    Hybrid,
}

/// Policy for retaining/archiving knowledge entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Delete entries older than this many days.
    pub max_age_days: Option<u32>,
    /// Keep at most this many entries per scope.
    pub max_entries: Option<u64>,
    /// If true, archive raw_text (set to NULL) instead of deleting entries.
    #[serde(default)]
    pub archive_raw_text: bool,
}

/// Result of applying a retention policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionResult {
    /// Number of entries archived (raw_text set to NULL).
    pub archived: u64,
    /// Number of entries deleted.
    pub deleted: u64,
}

// ─── KnowledgeStore Trait ────────────────────────────────────────────────────

/// Pluggable knowledge storage trait.
///
/// All operations are scoped by a `scope` parameter, enabling per-session,
/// per-project, or global knowledge through a single interface.
///
/// ## Query Contract
///
/// `query()` produces deterministic, explainable retrieval:
/// 1. Candidate selection via `retrieval_mode` (`Keyword` or `Hybrid`).
/// 2. Ranking score combines text relevance and optional boosts from
///    `entities/topics/importance`.
/// 3. Output is ordered by descending score and stable tie-break (`created_at`,
///    then row `id`).
/// 4. Citations always reference public IDs (`entry_public_id`,
///    `consolidation_public_id`), never row IDs.
#[async_trait]
pub trait KnowledgeStore: Send + Sync {
    /// Ingest a piece of raw information.
    async fn ingest(
        &self,
        scope: &str,
        entry: IngestRequest,
    ) -> Result<KnowledgeEntry, KnowledgeError>;

    /// List entries that have not been consolidated yet.
    async fn list_unconsolidated(
        &self,
        scope: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeEntry>, KnowledgeError>;

    /// List entries matching a filter.
    async fn list(
        &self,
        scope: &str,
        filter: KnowledgeFilter,
    ) -> Result<Vec<KnowledgeEntry>, KnowledgeError>;

    /// Mark entries as consolidated and store the consolidation result.
    ///
    /// The `source_entry_public_ids` in the consolidation are used to look up
    /// and mark the corresponding entries with `consolidated_at = now()`.
    async fn consolidate(
        &self,
        scope: &str,
        consolidation: ConsolidateRequest,
    ) -> Result<Consolidation, KnowledgeError>;

    /// Query: retrieve relevant entries + consolidations for a question.
    async fn query(
        &self,
        scope: &str,
        question: &str,
        opts: QueryOpts,
    ) -> Result<KnowledgeQueryResult, KnowledgeError>;

    /// Stats for a scope.
    async fn stats(&self, scope: &str) -> Result<KnowledgeStats, KnowledgeError>;

    /// Retention: archive/delete old entries.
    async fn apply_retention(
        &self,
        scope: &str,
        policy: &RetentionPolicy,
    ) -> Result<RetentionResult, KnowledgeError>;

    /// Check if a source key has already been ingested (deduplication).
    async fn is_source_ingested(
        &self,
        scope: &str,
        source_key: &str,
    ) -> Result<bool, KnowledgeError>;

    /// Mark a source key as ingested (deduplication).
    async fn mark_source_ingested(
        &self,
        scope: &str,
        source_key: &str,
    ) -> Result<(), KnowledgeError>;
}

/// Request to create a consolidation from multiple entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidateRequest {
    /// Public IDs of the source entries to consolidate.
    pub source_entry_public_ids: Vec<String>,
    /// Summary of the consolidated insight.
    pub summary: String,
    /// Key insight derived from the consolidation.
    pub insight: String,
    /// Cross-references discovered during consolidation.
    #[serde(default)]
    pub connections: Vec<String>,
}

// ─── Error Type ──────────────────────────────────────────────────────────────

/// Errors from knowledge store operations.
#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("Knowledge entry not found: {0}")]
    EntryNotFound(String),

    #[error("Consolidation not found: {0}")]
    ConsolidationNotFound(String),

    #[error("Invalid scope: {0}")]
    InvalidScope(String),

    #[error("Duplicate source key: {0}")]
    DuplicateSourceKey(String),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("{0}")]
    Other(String),
}

impl From<rusqlite::Error> for KnowledgeError {
    fn from(err: rusqlite::Error) -> Self {
        KnowledgeError::DatabaseError(err.to_string())
    }
}

impl From<serde_json::Error> for KnowledgeError {
    fn from(err: serde_json::Error) -> Self {
        KnowledgeError::SerializationError(err.to_string())
    }
}

// ─── Scope Policy ────────────────────────────────────────────────────────────

/// Pluggable policy for validating knowledge scope access.
///
/// ## Policy Modes
///
/// - **Default (single-user local mode):** Permit any scope string.
/// - **Multi-tenant:** Restrict to caller-owned scopes and explicit allowlist
///   prefixes (e.g. `session:*`, `project:*`, `global`).
/// - Violations return authorization errors; no best-effort fallback.
pub trait ScopePolicy: Send + Sync {
    /// Validate that the given session is allowed to access the requested scope.
    ///
    /// Returns `Ok(())` if access is permitted, or `Err(KnowledgeError)` with
    /// an authorization error if denied.
    fn validate_scope(
        &self,
        session_public_id: &str,
        requested_scope: &str,
    ) -> Result<(), KnowledgeError>;
}

/// Default scope policy: permit any scope string.
///
/// Suitable for single-user local mode where there is no multi-tenant concern.
#[derive(Debug, Clone, Default)]
pub struct PermissiveScopePolicy;

impl ScopePolicy for PermissiveScopePolicy {
    fn validate_scope(
        &self,
        _session_public_id: &str,
        _requested_scope: &str,
    ) -> Result<(), KnowledgeError> {
        Ok(())
    }
}

/// Multi-tenant scope policy: restrict access to owned scopes and allowlisted prefixes.
///
/// A session can access:
/// - Its own session scope (scope == session_public_id)
/// - Any scope matching an allowed prefix (e.g. `"global"`, `"project:"`)
///
/// All other scopes are denied by default.
#[derive(Debug, Clone)]
pub struct RestrictedScopePolicy {
    /// Allowed scope prefixes (e.g. `["global", "project:"]`).
    /// An exact match on the prefix or a scope starting with the prefix is permitted.
    pub allowed_prefixes: Vec<String>,
}

impl RestrictedScopePolicy {
    pub fn new(allowed_prefixes: Vec<String>) -> Self {
        Self { allowed_prefixes }
    }
}

impl ScopePolicy for RestrictedScopePolicy {
    fn validate_scope(
        &self,
        session_public_id: &str,
        requested_scope: &str,
    ) -> Result<(), KnowledgeError> {
        // Own session scope is always allowed
        if requested_scope == session_public_id {
            return Ok(());
        }

        // Check allowed prefixes
        for prefix in &self.allowed_prefixes {
            if requested_scope == prefix || requested_scope.starts_with(prefix) {
                return Ok(());
            }
        }

        Err(KnowledgeError::Other(format!(
            "Scope access denied: session '{}' cannot access scope '{}'. \
             Allowed: own session scope or prefixes {:?}",
            session_public_id, requested_scope, self.allowed_prefixes
        )))
    }
}

#[cfg(test)]
mod scope_policy_tests {
    use super::*;

    #[test]
    fn permissive_allows_everything() {
        let policy = PermissiveScopePolicy;
        assert!(policy.validate_scope("sess-1", "sess-1").is_ok());
        assert!(policy.validate_scope("sess-1", "global").is_ok());
        assert!(policy.validate_scope("sess-1", "other-session").is_ok());
    }

    #[test]
    fn restricted_allows_own_session() {
        let policy = RestrictedScopePolicy::new(vec![]);
        assert!(policy.validate_scope("sess-1", "sess-1").is_ok());
    }

    #[test]
    fn restricted_allows_prefixed_scopes() {
        let policy = RestrictedScopePolicy::new(vec!["global".to_string(), "project:".to_string()]);
        assert!(policy.validate_scope("sess-1", "global").is_ok());
        assert!(policy.validate_scope("sess-1", "project:myapp").is_ok());
    }

    #[test]
    fn restricted_denies_other_scopes() {
        let policy = RestrictedScopePolicy::new(vec!["global".to_string()]);
        assert!(policy.validate_scope("sess-1", "other-session").is_err());
        assert!(policy.validate_scope("sess-1", "project:myapp").is_err());
    }
}
