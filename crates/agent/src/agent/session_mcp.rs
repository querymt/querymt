//! Session MCP attachment model.
//!
//! Provides a single, unified extension point for attaching MCP capabilities to
//! sessions materialized on this node, independent of transport (ACP, mesh
//! remote, scheduler, CLI).

use crate::acp::protocol::{Error, McpServer};
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
    /// Attest that workspace-dependent integrations serve this effective cwd.
    /// Implementations must validate fixed roots, including absolute stdio
    /// arguments and already-connected peers. Unknown roots fail closed.
    async fn validate_workspace_roots(
        &self,
        context: &SessionMcpAttachmentContext,
        servers: &[McpServer],
        peers: &[ConnectedMcpPeer],
    ) -> Result<(), Error> {
        if servers.is_empty() && peers.is_empty() {
            return Ok(());
        }
        Err(Error::invalid_request().data(format!(
            "MCP workspace roots are unverified for {}; configure a workspace-aware attachment source that validates all server and peer roots",
            context.cwd.as_deref().unwrap_or_else(|| std::path::Path::new(".")).display()
        )))
    }

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

/// Split a list of attachments into server configs and connected peers.
pub fn split_attachments(
    attachments: Vec<SessionMcpAttachment>,
) -> (Vec<McpServer>, Vec<ConnectedMcpPeer>) {
    let mut servers = Vec::new();
    let mut peers = Vec::new();
    for attachment in attachments {
        match attachment {
            SessionMcpAttachment::ServerConfig(config) => servers.push(config),
            SessionMcpAttachment::ConnectedPeer(peer) => peers.push(peer),
        }
    }
    (servers, peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_mcp_roots_fail_closed_before_startup() {
        let context = SessionMcpAttachmentContext {
            session_id: "isolated".into(),
            cwd: Some(PathBuf::from("/workspaces/child")),
            kind: SessionMaterializationKind::New,
        };
        let server = McpServer::Stdio(
            crate::acp::protocol::McpServerStdio::new("fixed-root", "/bin/false")
                .args(vec!["/original/project".into()]),
        );
        let source = NoopSessionMcpAttachmentSource;
        assert!(
            source
                .validate_workspace_roots(&context, &[server], &[])
                .await
                .is_err()
        );
        assert!(
            source
                .validate_workspace_roots(&context, &[], &[])
                .await
                .is_ok()
        );
    }
}
