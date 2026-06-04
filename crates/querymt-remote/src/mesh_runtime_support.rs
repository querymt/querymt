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
