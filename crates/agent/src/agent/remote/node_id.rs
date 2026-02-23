use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Stable node identity used for mesh routing.
///
/// Serialized as a string at storage/wire boundaries, but represented as
/// `PeerId` internally when the `remote` feature is enabled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeId(InnerNodeId);

#[cfg(feature = "remote")]
type InnerNodeId = libp2p::PeerId;
#[cfg(not(feature = "remote"))]
type InnerNodeId = String;

impl NodeId {
    #[cfg(feature = "remote")]
    pub fn from_peer_id(peer_id: libp2p::PeerId) -> Self {
        Self(peer_id)
    }

    #[cfg(feature = "remote")]
    pub fn as_peer_id(&self) -> &libp2p::PeerId {
        &self.0
    }

    pub fn as_str(&self) -> String {
        self.0.to_string()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        #[cfg(feature = "remote")]
        {
            value
                .parse::<libp2p::PeerId>()
                .map(Self)
                .map_err(|e| format!("invalid peer id '{}': {}", value, e))
        }
        #[cfg(not(feature = "remote"))]
        {
            if value.trim().is_empty() {
                Err("node id cannot be empty".to_string())
            } else {
                Ok(Self(value.to_string()))
            }
        }
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for NodeId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}
