use crate::MeshScopeId;
use libp2p::PeerId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialReason {
    Admission,
    Reconnect,
    ExistingMeshPeer,
    Manual,
}

#[derive(Debug)]
pub(crate) enum SwarmCommand {
    DialPeer {
        peer_id: PeerId,
        scope: Option<MeshScopeId>,
        reason: DialReason,
    },
    JoinIrohScope {
        mesh_id: String,
        peers: Vec<PeerId>,
    },
    LeaveIrohScope {
        mesh_id: String,
    },
    Sharing {
        peer: PeerId,
        request: crate::mesh_ingress::SharingRequest,
        tx: crate::mesh_ingress::ResponseSender,
    },
    Shutdown {
        completion: Option<tokio::sync::oneshot::Sender<()>>,
    },
    /// Retry presenting trust tokens, if `session` is still the peer's
    /// current connection session.
    PresentTrust {
        peer: PeerId,
        session: u64,
        attempt: u32,
    },
}

pub(crate) fn resolve_local_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
