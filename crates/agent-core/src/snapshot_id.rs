//! Opaque snapshot identifier stored in message parts and session records.

use std::sync::Arc;

/// Unique identifier for a snapshot (e.g., git commit SHA).
///
/// Opaque to the trait: backends parse it on use. It serializes as a bare
/// string, so stored records and wire payloads are unchanged.
///
/// Backed by `Arc<str>`: a second owner (the runtime and the stored record,
/// for example) is a reference-count increment, not an allocation. Serde is
/// written by hand so the `rc` feature is not needed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapshotId(Arc<str>);

impl SnapshotId {
    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SnapshotId {
    fn from(value: String) -> Self {
        Self(Arc::from(value))
    }
}

impl From<&str> for SnapshotId {
    fn from(value: &str) -> Self {
        Self(Arc::from(value))
    }
}

impl serde::Serialize for SnapshotId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for SnapshotId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}
