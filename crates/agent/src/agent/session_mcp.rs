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
    ///
    /// The default implementation accepts integrations whose root is not
    /// tied to another checkout of the same repository:
    ///
    /// - HTTP servers have no local filesystem root;
    /// - stdio servers run inside the effective cwd, so relative entries
    ///   resolve there. They are rejected only when an argument or
    ///   environment value is an absolute path inside a *different*
    ///   checkout of the same repository (a fixed root that would silently
    ///   operate on the original files).
    ///
    /// Already-connected peers have an opaque, fixed root and fail closed
    /// unless an implementation overrides this method to validate them.
    async fn validate_workspace_roots(
        &self,
        context: &SessionMcpAttachmentContext,
        servers: &[McpServer],
        peers: &[ConnectedMcpPeer],
    ) -> Result<(), Error> {
        let cwd = context.cwd.as_deref();
        if !peers.is_empty() {
            return Err(Error::invalid_request().data(format!(
                "MCP workspace roots are unverified for {}; connected peers have a fixed root. \
                 Configure a workspace-aware attachment source that validates peer roots",
                cwd.unwrap_or_else(|| std::path::Path::new(".")).display()
            )));
        }
        let Some(cwd) = cwd else {
            return Ok(());
        };
        for server in servers {
            if let McpServer::Stdio(stdio) = server
                && let Some(foreign) = foreign_checkout_reference(stdio, cwd)
            {
                return Err(Error::invalid_request().data(format!(
                    "MCP server '{}' references {}, a different checkout of the repository \
                     than the session workspace {}; it would operate on the wrong files",
                    stdio.name,
                    foreign.display(),
                    cwd.display()
                )));
            }
        }
        Ok(())
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

/// First absolute path argument/environment value of `stdio` that lies inside
/// a checkout of the same repository as `cwd`, other than `cwd`'s own.
fn foreign_checkout_reference(
    stdio: &crate::acp::protocol::McpServerStdio,
    cwd: &std::path::Path,
) -> Option<PathBuf> {
    let session = crate::worktree::identity::resolve_repository(cwd).ok()?;
    let candidates = stdio
        .args
        .iter()
        .map(String::as_str)
        .chain(stdio.env.iter().map(|item| item.value.as_str()));
    for value in candidates {
        // Accept `--flag=/abs/path` as well as bare absolute paths.
        let value = value.rsplit_once('=').map_or(value, |(_, tail)| tail);
        let path = std::path::Path::new(value);
        if !path.is_absolute() || !path.exists() {
            continue;
        }
        let Ok(other) = crate::worktree::identity::resolve_repository(path) else {
            continue;
        };
        if other.identity.common_dir == session.identity.common_dir
            && other.checkout.root != session.checkout.root
        {
            return Some(path.to_path_buf());
        }
    }
    None
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

    fn context(cwd: &std::path::Path) -> SessionMcpAttachmentContext {
        SessionMcpAttachmentContext {
            session_id: "isolated".into(),
            cwd: Some(cwd.to_path_buf()),
            kind: SessionMaterializationKind::New,
        }
    }

    fn stdio(name: &str, args: Vec<String>) -> McpServer {
        McpServer::Stdio(crate::acp::protocol::McpServerStdio::new(name, "/bin/false").args(args))
    }

    #[tokio::test]
    async fn workspace_independent_mcp_servers_are_accepted() {
        let repo = crate::worktree::test_support::init_repo();
        let source = NoopSessionMcpAttachmentSource;
        let ctx = context(repo.path());

        // No servers, an HTTP server, a relative stdio entry and an
        // absolute path outside any repository all carry no foreign root.
        let http = McpServer::Http(crate::acp::protocol::McpServerHttp::new(
            "remote",
            "https://example.invalid/mcp",
        ));
        let outside = tempfile::TempDir::new().unwrap();
        let servers = [
            http,
            stdio("relative", vec!["./server.js".into()]),
            stdio("outside", vec![outside.path().display().to_string()]),
        ];
        assert!(
            source
                .validate_workspace_roots(&ctx, &[], &[])
                .await
                .is_ok()
        );
        assert!(
            source
                .validate_workspace_roots(&ctx, &servers, &[])
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn fixed_root_in_another_checkout_of_the_repository_is_rejected() {
        use crate::worktree::test_support::{commit_files, init_repo, materialize};

        let repo = init_repo();
        commit_files(repo.path(), "init", &[("file.txt", "one")]);
        materialize(repo.path(), &[("file.txt", "one")]);
        let linked_parent = tempfile::TempDir::new().unwrap();
        let linked = linked_parent.path().join("linked");
        let gix_repo = gix::open(repo.path()).unwrap();
        let head = gix_repo.head_id().unwrap().detach();
        gix_repo
            .reference(
                "refs/heads/other",
                head,
                gix::refs::transaction::PreviousValue::Any,
                "test",
            )
            .unwrap();
        gix_repo
            .add_worktree(
                &linked,
                gix::worktree::add::Head::Attached(
                    gix::refs::FullName::try_from("refs/heads/other").unwrap(),
                ),
                gix::progress::Discard,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();

        let source = NoopSessionMcpAttachmentSource;
        // The session runs in the linked checkout; the server is pinned to
        // the original one (bare path and `--root=` forms).
        let ctx = context(&linked);
        for arg in [
            repo.path().display().to_string(),
            format!("--root={}", repo.path().display()),
        ] {
            let err = source
                .validate_workspace_roots(&ctx, &[stdio("pinned", vec![arg])], &[])
                .await
                .expect_err("a root pinned to another checkout must be rejected");
            assert!(format!("{err:?}").contains("different checkout"), "{err:?}");
        }
        // A server pointing at the session's own checkout is fine.
        assert!(
            source
                .validate_workspace_roots(
                    &ctx,
                    &[stdio("own", vec![linked.display().to_string()])],
                    &[],
                )
                .await
                .is_ok()
        );
    }
}
