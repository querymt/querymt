//! Session MCP attachment model.
//!
//! Provides a single, unified extension point for attaching MCP capabilities to
//! sessions materialized on this node, independent of transport (ACP, mesh
//! remote, scheduler, CLI).

use agent_client_protocol::schema::{Error, McpServer};
use async_trait::async_trait;
use rmcp::RoleClient;
use rmcp::service::Peer;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// ConnectedMcpPeer
// ---------------------------------------------------------------------------

/// A fully initialized MCP client peer ready for reuse across sessions.
///
/// Owned by the runtime MCP attach layer; not serializable and lives in a
/// local process. Created from in-process pipe transports on mobile, but the
/// type itself is generic — any already-connected MCP peer fits here.
#[derive(Debug, Clone)]
pub struct ConnectedMcpPeer {
    /// Logical MCP server name used for adapter metadata and diagnostics.
    pub server_name: String,
    /// Initialized MCP client peer (handshake already completed).
    pub peer: Peer<RoleClient>,
}

// ---------------------------------------------------------------------------
// SessionMcpAttachment
// ---------------------------------------------------------------------------

/// A single MCP capability that the local runtime wants to attach to a session.
#[derive(Debug, Clone)]
pub enum SessionMcpAttachment {
    /// An already-initialized, connected MCP peer (in-process pipe, etc.).
    ConnectedPeer(ConnectedMcpPeer),
    /// A standard MCP server config (stdio, HTTP, etc.).
    ServerConfig(McpServer),
}

// ---------------------------------------------------------------------------
// Attachment context
// ---------------------------------------------------------------------------

/// What kind of session materialization triggered the attachment request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMaterializationKind {
    New,
    Load,
    Resume,
    Fork,
    RemoteCreate,
    RemoteResume,
}

/// Metadata available when the attachment source is consulted.
#[derive(Debug, Clone)]
pub struct SessionMcpAttachmentContext {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub kind: SessionMaterializationKind,
}

// ---------------------------------------------------------------------------
// SessionMcpAttachmentSource
// ---------------------------------------------------------------------------

/// A source of MCP attachments that should be available to **every** session
/// materialized on this agent node.
///
/// Unlike per-session request parameters or TOML-configured MCP servers, this
/// source represents capabilities that are intrinsic to the agent runtime
/// (e.g., mobile device MCP servers exposed via in-process pipes).
///
/// The source is consulted once per materialization.  If the session runtime
/// already exists, the source is **not** called again – attachment is
/// idempotent.
#[async_trait]
pub trait SessionMcpAttachmentSource: Send + Sync {
    /// Return the MCP attachments for the given session materialization context.
    async fn attachments(
        &self,
        context: &SessionMcpAttachmentContext,
    ) -> Result<Vec<SessionMcpAttachment>, Error>;
}

// ---------------------------------------------------------------------------
// No-op default
// ---------------------------------------------------------------------------

/// An attachment source that never provides any attachments (desktop / CLI
/// default).
pub struct NoopSessionMcpAttachmentSource;

#[async_trait]
impl SessionMcpAttachmentSource for NoopSessionMcpAttachmentSource {
    async fn attachments(
        &self,
        _context: &SessionMcpAttachmentContext,
    ) -> Result<Vec<SessionMcpAttachment>, Error> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split a list of [`SessionMcpAttachment`]s into server configs and
/// connected peers.
pub fn split_attachments(
    attachments: Vec<SessionMcpAttachment>,
) -> (Vec<McpServer>, Vec<ConnectedMcpPeer>) {
    let mut servers = Vec::new();
    let mut peers = Vec::new();

    for a in attachments {
        match a {
            SessionMcpAttachment::ServerConfig(config) => servers.push(config),
            SessionMcpAttachment::ConnectedPeer(peer) => peers.push(peer),
        }
    }

    (servers, peers)
}
