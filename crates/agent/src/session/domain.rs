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
