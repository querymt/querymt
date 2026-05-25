//! Work Packet system — durable, searchable context artifacts.
//!
//! A Work Packet generalizes the markdown-plan workflow into a first-class
//! concept that supports plans, handoffs, checkpoints, delegation inputs,
//! and results. The body is model-native markdown; structured metadata
//! enables discovery, linking, and lifecycle management.
//!
//! # Architecture
//!
//! - [`WorkPacket`] — domain model
//! - [`WorkPacketKind`] — plan, brief, handoff, checkpoint, result, etc.
//! - [`WorkPacketStatus`] — draft, ready, in_progress, complete, archived
//! - [`WorkPacketStore`] — async trait for persistence and retrieval
//! - [`sqlite`] — SQLite-backed implementation with FTS5

pub mod brief_generator;
pub mod service;
pub mod slash_commands;
pub mod sqlite;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// What kind of artifact this packet represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPacketKind {
    /// Implementation plan with phases, decisions, and open questions.
    Plan,
    /// Implementation brief for a coding agent.
    Brief,
    /// Session handoff / continuation context.
    Handoff,
    /// Progress checkpoint linked to an active packet.
    Checkpoint,
    /// Result summary from a completed effort or delegation.
    Result,
    /// Review findings.
    Review,
    /// Research summary.
    Research,
}

impl WorkPacketKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Brief => "brief",
            Self::Handoff => "handoff",
            Self::Checkpoint => "checkpoint",
            Self::Result => "result",
            Self::Review => "review",
            Self::Research => "research",
        }
    }

    /// Iterate over all known kinds.
    pub fn all() -> &'static [WorkPacketKind] {
        &[
            Self::Plan,
            Self::Brief,
            Self::Handoff,
            Self::Checkpoint,
            Self::Result,
            Self::Review,
            Self::Research,
        ]
    }
}

impl std::fmt::Display for WorkPacketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for WorkPacketKind {
    type Err = WorkPacketError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plan" => Ok(Self::Plan),
            "brief" => Ok(Self::Brief),
            "handoff" => Ok(Self::Handoff),
            "checkpoint" => Ok(Self::Checkpoint),
            "result" => Ok(Self::Result),
            "review" => Ok(Self::Review),
            "research" => Ok(Self::Research),
            other => Err(WorkPacketError::InvalidKind(other.to_string())),
        }
    }
}

/// Lifecycle status of a work packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPacketStatus {
    Draft,
    Ready,
    InProgress,
    Complete,
    Archived,
}

impl WorkPacketStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Ready => "ready",
            Self::InProgress => "in_progress",
            Self::Complete => "complete",
            Self::Archived => "archived",
        }
    }

    pub fn all() -> &'static [WorkPacketStatus] {
        &[
            Self::Draft,
            Self::Ready,
            Self::InProgress,
            Self::Complete,
            Self::Archived,
        ]
    }
}

impl std::fmt::Display for WorkPacketStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for WorkPacketStatus {
    type Err = WorkPacketError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(Self::Draft),
            "ready" => Ok(Self::Ready),
            "in_progress" => Ok(Self::InProgress),
            "complete" => Ok(Self::Complete),
            "archived" => Ok(Self::Archived),
            other => Err(WorkPacketError::InvalidStatus(other.to_string())),
        }
    }
}

/// A durable, user-visible context artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkPacket {
    /// Internal row id.
    pub id: i64,
    /// Externally-visible stable identifier (e.g. `pkt_abc123`).
    pub public_id: String,
    /// Scope (typically session public id or a shared namespace).
    pub scope: String,
    /// What kind of artifact this is.
    pub kind: WorkPacketKind,
    /// Lifecycle status.
    pub status: WorkPacketStatus,
    /// Short human-readable title.
    pub title: String,
    /// One-paragraph summary for search results.
    pub summary: String,
    /// The full body in markdown.
    pub body_markdown: String,
    /// Optional structured metadata as JSON.
    pub metadata_json: Option<serde_json::Value>,
    /// Session that originally created this packet.
    pub origin_session_id: Option<String>,
    /// Parent packet this was derived from.
    pub parent_packet_id: Option<String>,
    /// Delegation that produced this packet as input.
    pub source_delegation_id: Option<String>,
    /// Delegation that should consume this packet as input.
    pub target_delegation_id: Option<String>,
    /// When the packet was created.
    pub created_at: OffsetDateTime,
    /// When the packet was last updated.
    pub updated_at: OffsetDateTime,
}

/// Request to create a new work packet.
#[derive(Debug, Clone)]
pub struct CreateWorkPacket {
    pub scope: String,
    pub kind: WorkPacketKind,
    pub title: String,
    pub summary: String,
    pub body_markdown: String,
    pub metadata_json: Option<serde_json::Value>,
    pub origin_session_id: Option<String>,
    pub parent_packet_id: Option<String>,
    pub source_delegation_id: Option<String>,
    pub target_delegation_id: Option<String>,
}

/// Fields that can be updated on an existing packet.
#[derive(Debug, Clone, Default)]
pub struct UpdateWorkPacket {
    pub kind: Option<WorkPacketKind>,
    pub status: Option<WorkPacketStatus>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub body_markdown: Option<String>,
    pub metadata_json: Option<serde_json::Value>,
    pub parent_packet_id: Option<Option<String>>,
    pub source_delegation_id: Option<Option<String>>,
    pub target_delegation_id: Option<Option<String>>,
}

/// Filter for searching / listing packets.
#[derive(Debug, Clone)]
pub struct WorkPacketFilter {
    pub scope: Option<String>,
    pub kind: Option<WorkPacketKind>,
    pub status: Option<WorkPacketStatus>,
    pub parent_packet_id: Option<String>,
    pub origin_session_id: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

impl Default for WorkPacketFilter {
    fn default() -> Self {
        Self {
            scope: None,
            kind: None,
            status: None,
            parent_packet_id: None,
            origin_session_id: None,
            limit: 50,
            offset: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by the work packet store.
#[derive(Debug, thiserror::Error)]
pub enum WorkPacketError {
    #[error("Packet not found: {0}")]
    NotFound(String),

    #[error("Invalid kind: {0}")]
    InvalidKind(String),

    #[error("Invalid status: {0}")]
    InvalidStatus(String),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("{0}")]
    Other(String),
}

impl From<rusqlite::Error> for WorkPacketError {
    fn from(err: rusqlite::Error) -> Self {
        Self::DatabaseError(err.to_string())
    }
}

impl From<serde_json::Error> for WorkPacketError {
    fn from(err: serde_json::Error) -> Self {
        Self::SerializationError(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Store trait
// ---------------------------------------------------------------------------

/// Async persistence interface for work packets.
#[async_trait]
pub trait WorkPacketStore: Send + Sync {
    /// Create a new packet. Returns the persisted packet with id and timestamps.
    async fn create(&self, packet: CreateWorkPacket) -> Result<WorkPacket, WorkPacketError>;

    /// Load a single packet by its public id.
    async fn load(&self, public_id: &str) -> Result<WorkPacket, WorkPacketError>;

    /// Search packets using a full-text query over title, summary, and body.
    /// Falls back to recent packets when the query is empty.
    async fn search(
        &self,
        query: &str,
        filter: &WorkPacketFilter,
    ) -> Result<Vec<WorkPacket>, WorkPacketError>;

    /// List packets matching a structured filter (no full-text query).
    async fn list(&self, filter: &WorkPacketFilter) -> Result<Vec<WorkPacket>, WorkPacketError>;

    /// Update mutable fields on an existing packet.
    async fn update(
        &self,
        public_id: &str,
        update: UpdateWorkPacket,
    ) -> Result<WorkPacket, WorkPacketError>;

    /// Link two packets by setting `parent_packet_id` on the child.
    async fn link(
        &self,
        child_public_id: &str,
        parent_public_id: &str,
    ) -> Result<(), WorkPacketError>;

    /// Delete a packet by public id.
    async fn delete(&self, public_id: &str) -> Result<(), WorkPacketError>;

    /// Set the active packet for a session.
    async fn set_active_packet(
        &self,
        session_public_id: &str,
        packet_public_id: Option<&str>,
    ) -> Result<(), WorkPacketError>;

    /// Get the active packet public id for a session.
    async fn get_active_packet(
        &self,
        session_public_id: &str,
    ) -> Result<Option<String>, WorkPacketError>;
}
