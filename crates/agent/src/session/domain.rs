//! Domain model entities for the agent refactor.
//! These entities represent intent, tasks, decisions, and progress as first-class concepts.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Task kind determines lifecycle and completion semantics
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// One-time task with clear completion
    Finite,
    /// Repeated or periodic task
    Recurring,
    /// Open-ended, continuously evolving task
    Evolving,
}

/// Task status tracks current state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Active,
    Paused,
    Done,
    Cancelled,
}

/// A unit of work within a session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    #[serde(skip)]
    pub id: i64,
    pub public_id: String,
    #[serde(skip)]
    pub session_id: i64,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub expected_deliverable: Option<String>,
    pub acceptance_criteria: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Snapshot of current user intent at a point in time (internal only)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentSnapshot {
    #[serde(skip)]
    pub id: i64,
    #[serde(skip)]
    pub session_id: i64,
    #[serde(skip)]
    pub task_id: Option<i64>,
    /// Authoritative summary of what the user wants
    pub summary: String,
    /// Constraints or boundaries
    pub constraints: Option<String>,
    /// Hint for next action
    pub next_step_hint: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Decision status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Accepted,
    Rejected,
}

/// Records a decision made during task execution (internal only)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    #[serde(skip)]
    pub id: i64,
    #[serde(skip)]
    pub session_id: i64,
    #[serde(skip)]
    pub task_id: Option<i64>,
    pub description: String,
    pub rationale: Option<String>,
    pub status: DecisionStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Alternative status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlternativeStatus {
    Active,
    Discarded,
}

/// Alternative approach considered (internal only)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alternative {
    #[serde(skip)]
    pub id: i64,
    #[serde(skip)]
    pub session_id: i64,
    #[serde(skip)]
    pub task_id: Option<i64>,
    pub description: String,
    pub status: AlternativeStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Progress entry kind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    ToolCall,
    Artifact,
    Note,
    Checkpoint,
}

/// Records a step of progress during task execution (internal only)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEntry {
    #[serde(skip)]
    pub id: i64,
    #[serde(skip)]
    pub session_id: i64,
    #[serde(skip)]
    pub task_id: Option<i64>,
    pub kind: ProgressKind,
    pub content: String,
    /// JSON metadata for extensibility
    pub metadata: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Artifact produced during task execution (internal only)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    #[serde(skip)]
    pub id: i64,
    #[serde(skip)]
    pub session_id: i64,
    #[serde(skip)]
    pub task_id: Option<i64>,
    pub kind: String,
    pub uri: Option<String>,
    pub path: Option<String>,
    pub summary: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Delegation status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Requested,
    Running,
    Complete,
    Failed,
    Cancelled,
}

/// Agent-to-agent delegation record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delegation {
    #[serde(skip)]
    pub id: i64,
    pub public_id: String,
    #[serde(skip)]
    pub session_id: i64,
    #[serde(skip)]
    pub task_id: Option<i64>,
    pub target_agent_id: String,
    pub objective: String,
    /// Rapidhash of objective for deduplication tracking
    pub objective_hash: crate::hash::RapidHash,
    pub context: Option<String>,
    pub constraints: Option<String>,
    pub expected_output: Option<String>,
    /// Optional structured verification specification (preferred over parsing expected_output)
    #[serde(default)]
    pub verification_spec: Option<crate::verification::VerificationSpec>,
    /// AI-generated summary of parent planning conversation for coder context
    pub planning_summary: Option<String>,
    pub status: DelegationStatus,
    /// Number of retry attempts for this objective
    pub retry_count: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
}

/// Fork point type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkPointType {
    MessageIndex,
    ProgressEntry,
}

/// Fork origin (why the fork happened)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkOrigin {
    User,
    Delegation,
}

impl std::fmt::Display for ForkOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForkOrigin::User => write!(f, "user"),
            ForkOrigin::Delegation => write!(f, "delegation"),
        }
    }
}

impl std::str::FromStr for ForkOrigin {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(ForkOrigin::User),
            "delegation" => Ok(ForkOrigin::Delegation),
            _ => Err(format!("Unknown fork origin: {}", s)),
        }
    }
}

impl std::fmt::Display for ForkPointType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForkPointType::MessageIndex => write!(f, "message_index"),
            ForkPointType::ProgressEntry => write!(f, "progress_entry"),
        }
    }
}

impl std::str::FromStr for ForkPointType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "message_index" => Ok(ForkPointType::MessageIndex),
            "progress_entry" => Ok(ForkPointType::ProgressEntry),
            _ => Err(format!("Unknown fork point type: {}", s)),
        }
    }
}

/// State saved during an undo operation, enabling redo
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertState {
    pub public_id: String,
    pub session_id: String,
    /// The message ID we reverted to
    pub message_id: String,
    /// Snapshot taken before the undo (for redo)
    pub snapshot_id: String,
    /// Backend type used (e.g., "git")
    pub backend_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Information about a forked session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkInfo {
    #[serde(skip)]
    pub parent_session_id: Option<i64>,
    pub fork_origin: Option<ForkOrigin>,
    pub fork_point_type: Option<ForkPointType>,
    pub fork_point_ref: Option<String>,
    pub fork_instructions: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    // ── TaskKind ────────────────────────────────────────────────────────────

    #[test]
    fn test_task_kind_variants() {
        let kinds = [TaskKind::Finite, TaskKind::Recurring, TaskKind::Evolving];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let roundtrip: TaskKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, &roundtrip);
        }
    }

    #[test]
    fn test_task_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&TaskKind::Finite).unwrap(),
            r#""finite""#
        );
        assert_eq!(
            serde_json::to_string(&TaskKind::Recurring).unwrap(),
            r#""recurring""#
        );
        assert_eq!(
            serde_json::to_string(&TaskKind::Evolving).unwrap(),
            r#""evolving""#
        );
    }

    // ── TaskStatus ──────────────────────────────────────────────────────────

    #[test]
    fn test_task_status_variants_equality() {
        assert_eq!(TaskStatus::Active, TaskStatus::Active);
        assert_ne!(TaskStatus::Active, TaskStatus::Done);
        assert_ne!(TaskStatus::Paused, TaskStatus::Cancelled);
    }

    #[test]
    fn test_task_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&TaskStatus::Active).unwrap(),
            r#""active""#
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::Paused).unwrap(),
            r#""paused""#
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::Done).unwrap(),
            r#""done""#
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::Cancelled).unwrap(),
            r#""cancelled""#
        );
    }

    #[test]
    fn test_task_status_roundtrip() {
        for status in [
            TaskStatus::Active,
            TaskStatus::Paused,
            TaskStatus::Done,
            TaskStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let rt: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, rt);
        }
    }

    // ── Task ────────────────────────────────────────────────────────────────

    #[test]
    fn test_task_construction_and_fields() {
        let t = Task {
            id: 1,
            public_id: "task-abc".to_string(),
            session_id: 2,
            kind: TaskKind::Finite,
            status: TaskStatus::Active,
            expected_deliverable: Some("deliver X".to_string()),
            acceptance_criteria: Some("X is correct".to_string()),
            created_at: now(),
            updated_at: now(),
        };
        assert_eq!(t.public_id, "task-abc");
        assert_eq!(t.kind, TaskKind::Finite);
        assert_eq!(t.status, TaskStatus::Active);
        assert!(t.expected_deliverable.is_some());
        assert!(t.acceptance_criteria.is_some());
    }

    #[test]
    fn test_task_status_transitions() {
        let mut t = Task {
            id: 1,
            public_id: "x".to_string(),
            session_id: 1,
            kind: TaskKind::Evolving,
            status: TaskStatus::Active,
            expected_deliverable: None,
            acceptance_criteria: None,
            created_at: now(),
            updated_at: now(),
        };
        assert_eq!(t.status, TaskStatus::Active);
        t.status = TaskStatus::Paused;
        assert_eq!(t.status, TaskStatus::Paused);
        t.status = TaskStatus::Done;
        assert_eq!(t.status, TaskStatus::Done);
    }

    // ── DecisionStatus ──────────────────────────────────────────────────────

    #[test]
    fn test_decision_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&DecisionStatus::Accepted).unwrap(),
            r#""accepted""#
        );
        assert_eq!(
            serde_json::to_string(&DecisionStatus::Rejected).unwrap(),
            r#""rejected""#
        );
    }

    #[test]
    fn test_decision_status_roundtrip() {
        for s in [DecisionStatus::Accepted, DecisionStatus::Rejected] {
            let json = serde_json::to_string(&s).unwrap();
            let rt: DecisionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, rt);
        }
    }

    // ── AlternativeStatus ───────────────────────────────────────────────────

    #[test]
    fn test_alternative_status_roundtrip() {
        for s in [AlternativeStatus::Active, AlternativeStatus::Discarded] {
            let json = serde_json::to_string(&s).unwrap();
            let rt: AlternativeStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, rt);
        }
    }

    // ── DelegationStatus ────────────────────────────────────────────────────

    #[test]
    fn test_delegation_status_all_variants_roundtrip() {
        let statuses = [
            DelegationStatus::Requested,
            DelegationStatus::Running,
            DelegationStatus::Complete,
            DelegationStatus::Failed,
            DelegationStatus::Cancelled,
        ];
        for s in &statuses {
            let json = serde_json::to_string(s).unwrap();
            let rt: DelegationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, &rt);
        }
    }

    // ── ForkOrigin ──────────────────────────────────────────────────────────

    #[test]
    fn test_fork_origin_display_and_parse() {
        assert_eq!(ForkOrigin::User.to_string(), "user");
        assert_eq!(ForkOrigin::Delegation.to_string(), "delegation");

        let user: ForkOrigin = "user".parse().unwrap();
        assert_eq!(user, ForkOrigin::User);

        let delegation: ForkOrigin = "delegation".parse().unwrap();
        assert_eq!(delegation, ForkOrigin::Delegation);
    }

    #[test]
    fn test_fork_origin_parse_unknown_errors() {
        let result: Result<ForkOrigin, _> = "unknown".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_fork_origin_serialization() {
        assert_eq!(
            serde_json::to_string(&ForkOrigin::User).unwrap(),
            r#""user""#
        );
        assert_eq!(
            serde_json::to_string(&ForkOrigin::Delegation).unwrap(),
            r#""delegation""#
        );
    }

    // ── ForkPointType ───────────────────────────────────────────────────────

    #[test]
    fn test_fork_point_type_display_and_parse() {
        assert_eq!(ForkPointType::MessageIndex.to_string(), "message_index");
        assert_eq!(ForkPointType::ProgressEntry.to_string(), "progress_entry");

        let mi: ForkPointType = "message_index".parse().unwrap();
        assert_eq!(mi, ForkPointType::MessageIndex);

        let pe: ForkPointType = "progress_entry".parse().unwrap();
        assert_eq!(pe, ForkPointType::ProgressEntry);
    }

    #[test]
    fn test_fork_point_type_parse_unknown_errors() {
        let result: Result<ForkPointType, _> = "bogus".parse();
        assert!(result.is_err());
    }

    // ── ForkInfo construction ───────────────────────────────────────────────

    #[test]
    fn test_fork_info_construction() {
        let info = ForkInfo {
            parent_session_id: Some(42),
            fork_origin: Some(ForkOrigin::User),
            fork_point_type: Some(ForkPointType::MessageIndex),
            fork_point_ref: Some("msg-001".to_string()),
            fork_instructions: Some("do X differently".to_string()),
        };
        assert_eq!(info.parent_session_id, Some(42));
        assert_eq!(info.fork_origin, Some(ForkOrigin::User));
        assert_eq!(info.fork_point_type, Some(ForkPointType::MessageIndex));
        assert_eq!(info.fork_point_ref.as_deref(), Some("msg-001"));
    }

    #[test]
    fn test_fork_info_serialization_skips_parent_id() {
        let info = ForkInfo {
            parent_session_id: Some(99),
            fork_origin: Some(ForkOrigin::Delegation),
            fork_point_type: None,
            fork_point_ref: None,
            fork_instructions: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        // parent_session_id is #[serde(skip)] so it should NOT appear in JSON
        assert!(!json.contains("parent_session_id"));
        assert!(json.contains("delegation"));
    }

    // ── ProgressKind ────────────────────────────────────────────────────────

    #[test]
    fn test_progress_kind_all_variants_roundtrip() {
        let kinds = [
            ProgressKind::ToolCall,
            ProgressKind::Artifact,
            ProgressKind::Note,
            ProgressKind::Checkpoint,
        ];
        for k in &kinds {
            let json = serde_json::to_string(k).unwrap();
            let rt: ProgressKind = serde_json::from_str(&json).unwrap();
            assert_eq!(k, &rt);
        }
    }
}
