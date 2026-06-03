/// Transport layer for the mesh runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum MeshTransportMode {
    #[default]
    Lan,
    Iroh,
    Composite,
}

impl MeshTransportMode {
    pub fn has_lan(&self) -> bool {
        matches!(self, Self::Lan | Self::Composite)
    }
}

/// Errors that can occur during mesh bootstrap.
#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("libp2p swarm error: {0}")]
    SwarmError(String),
    #[error("invalid listen address '{addr}': {reason}")]
    InvalidListenAddr { addr: String, reason: String },
    #[error("invalid bootstrap peer address '{addr}': {reason}")]
    InvalidBootstrapAddr { addr: String, reason: String },
}
