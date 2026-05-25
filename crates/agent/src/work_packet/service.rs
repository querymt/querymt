//! Deterministic workflow service for work packets.
//!
//! Wraps [`WorkPacketStore`] with higher-level operations used by
//! runtime slash commands and (eventually) by packet tools.
//! No LLM calls — pure async application logic.

use crate::work_packet::{
    UpdateWorkPacket, WorkPacket, WorkPacketError, WorkPacketFilter, WorkPacketStatus,
    WorkPacketStore,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Selector / resolution types
// ---------------------------------------------------------------------------

/// How the caller identifies the target packet.
#[derive(Debug, Clone)]
pub enum PacketSelector {
    /// Exact public id (e.g. `pkt_abc123`).
    Id(String),
    /// Free-form full-text search query.
    Query(String),
    /// List most recent packets.
    Recent,
}

impl PacketSelector {
    /// Parse a raw argument string into a selector.
    ///
    /// - Strings starting with `pkt_` are treated as exact ids.
    /// - Empty string => `Recent`.
    /// - Anything else => `Query`.
    pub fn from_arg(arg: &str) -> Self {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            Self::Recent
        } else if trimmed.starts_with("pkt_") {
            Self::Id(trimmed.to_string())
        } else {
            Self::Query(trimmed.to_string())
        }
    }
}

/// Result of resolving a selector against the store.
#[derive(Debug, Clone)]
pub enum PacketResolution {
    /// No matching packets found.
    None { query: String },
    /// Exactly one match.
    One(Box<WorkPacket>),
    /// Multiple matches — caller should disambiguate.
    Many(Vec<WorkPacket>),
}

// ---------------------------------------------------------------------------
// Structured results for specific workflows
// ---------------------------------------------------------------------------

/// Result of resuming a packet for a session.
#[derive(Debug, Clone)]
pub struct ResumePacketResult {
    /// The packet that was resumed.
    pub packet: WorkPacket,
    /// Previous status (before the resume transition).
    pub previous_status: WorkPacketStatus,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct WorkPacketService {
    store: Arc<dyn WorkPacketStore>,
}

impl WorkPacketService {
    pub fn new(store: Arc<dyn WorkPacketStore>) -> Self {
        Self { store }
    }

    // ── Resolve ────────────────────────────────────────────────────────

    /// Resolve a selector into zero, one, or many packets.
    pub async fn resolve(
        &self,
        selector: &PacketSelector,
    ) -> Result<PacketResolution, WorkPacketError> {
        match selector {
            PacketSelector::Id(id) => match self.store.load(id).await {
                Ok(pkt) => Ok(PacketResolution::One(pkt.into())),
                Err(WorkPacketError::NotFound(_)) => {
                    Ok(PacketResolution::None { query: id.clone() })
                }
                Err(e) => Err(e),
            },
            PacketSelector::Query(q) => {
                let filter = WorkPacketFilter {
                    limit: 10,
                    ..Default::default()
                };
                let packets = self.store.search(q, &filter).await?;
                match packets.len() {
                    0 => Ok(PacketResolution::None { query: q.clone() }),
                    1 => Ok(PacketResolution::One(
                        packets.into_iter().next().unwrap().into(),
                    )),
                    _ => Ok(PacketResolution::Many(packets)),
                }
            }
            PacketSelector::Recent => {
                let filter = WorkPacketFilter {
                    limit: 10,
                    ..Default::default()
                };
                let packets = self.store.list(&filter).await?;
                if packets.is_empty() {
                    Ok(PacketResolution::None {
                        query: "(recent)".to_string(),
                    })
                } else {
                    Ok(PacketResolution::Many(packets))
                }
            }
        }
    }

    // ── Resume ─────────────────────────────────────────────────────────

    /// Resume a packet for the given session.
    ///
    /// - Resolves the selector.
    /// - If exactly one match: updates status to `in_progress` (unless
    ///   complete/archived), sets it as the active packet, and returns
    ///   the result.
    /// - Otherwise returns the resolution as-is for the caller to handle.
    pub async fn resume_packet(
        &self,
        session_id: &str,
        selector: &PacketSelector,
    ) -> Result<ResumeOrResolution, WorkPacketError> {
        let resolution = self.resolve(selector).await?;

        match resolution {
            PacketResolution::One(packet) => {
                let previous_status = packet.status;

                // Transition status if appropriate.
                if !matches!(
                    packet.status,
                    WorkPacketStatus::Complete | WorkPacketStatus::Archived
                ) {
                    self.store
                        .update(
                            &packet.public_id,
                            UpdateWorkPacket {
                                status: Some(WorkPacketStatus::InProgress),
                                ..Default::default()
                            },
                        )
                        .await?;
                }

                // Set active packet for this session.
                self.store
                    .set_active_packet(session_id, Some(&packet.public_id))
                    .await?;

                // Reload to get updated timestamps.
                let packet = self.store.load(&packet.public_id).await?;

                Ok(ResumeOrResolution::Resumed(Box::new(ResumePacketResult {
                    packet,
                    previous_status,
                })))
            }
            other => Ok(ResumeOrResolution::NeedsDisambiguation(other)),
        }
    }

    // ── Active status ──────────────────────────────────────────────────

    /// Get the status of the active packet for a session (if any).
    pub async fn active_status(
        &self,
        session_id: &str,
    ) -> Result<Option<WorkPacket>, WorkPacketError> {
        let active_id = self.store.get_active_packet(session_id).await?;
        match active_id {
            Some(id) => self.store.load(&id).await.map(Some),
            None => Ok(None),
        }
    }

    // ── Show ───────────────────────────────────────────────────────────

    /// Show a specific packet (by id or search).
    pub async fn show(
        &self,
        selector: &PacketSelector,
    ) -> Result<PacketResolution, WorkPacketError> {
        self.resolve(selector).await
    }

    // ── List ───────────────────────────────────────────────────────────

    /// List recent packets or search.
    pub async fn list(
        &self,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WorkPacket>, WorkPacketError> {
        let filter = WorkPacketFilter {
            limit,
            ..Default::default()
        };
        match query {
            Some(q) if !q.trim().is_empty() => self.store.search(q, &filter).await,
            _ => self.store.list(&filter).await,
        }
    }

    // ── Raw store access (for tools that still need it) ────────────────

    pub fn store(&self) -> &Arc<dyn WorkPacketStore> {
        &self.store
    }
}

// ---------------------------------------------------------------------------
// Composite result
// ---------------------------------------------------------------------------

/// Either the packet was resumed, or the resolution needs disambiguation.
#[derive(Debug, Clone)]
pub enum ResumeOrResolution {
    Resumed(Box<ResumePacketResult>),
    NeedsDisambiguation(PacketResolution),
}
