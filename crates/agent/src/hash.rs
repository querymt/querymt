//! Fast non-cryptographic hashing using rapidhash.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A 64-bit rapidhash value.
///
/// Serializes as a 16-character hex string for JSON compatibility with JavaScript
/// (which loses precision on large u64 values).
///
/// # Examples
///
/// ```
/// use querymt_agent::hash::RapidHash;
///
/// let hash = RapidHash::new(b"hello world");
/// println!("{}", hash); // prints hex: "779a65e7023cd2e7"
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RapidHash(u64);

impl RapidHash {
    /// Hash the given data using the rapidhash v3 algorithm.
    #[inline]
    pub fn new(data: &[u8]) -> Self {
        // Use default secrets for non-cryptographic hashing
        Self(rapidhash::v3::rapidhash_v3(data))
    }

    /// Get the raw u64 value.
    #[inline]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Convert to a 16-character lowercase hex string.
    ///
    /// Used for SQLite TEXT storage and debugging.
    pub fn to_hex(&self) -> String {
        format!("{:016x}", self.0)
    }

    /// Parse from a hex string.
    ///
    /// Accepts both 16-character and shorter hex strings.
    pub fn from_hex(s: &str) -> Result<Self, std::num::ParseIntError> {
        u64::from_str_radix(s, 16).map(Self)
    }
}

impl fmt::Debug for RapidHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RapidHash({:016x})", self.0)
    }
}

impl fmt::Display for RapidHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

// Serialize as hex string for JSON compatibility with JavaScript
impl Serialize for RapidHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for RapidHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_deterministic() {
        let data = b"hello world";
        let hash1 = RapidHash::new(data);
        let hash2 = RapidHash::new(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_different_inputs_different_hashes() {
        let hash1 = RapidHash::new(b"hello");
        let hash2 = RapidHash::new(b"world");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hex_roundtrip() {
        let hash = RapidHash::new(b"test");
        let hex = hash.to_hex();
        assert_eq!(hex.len(), 16);
        let parsed = RapidHash::from_hex(&hex).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn test_serde_json() {
        let hash = RapidHash::new(b"test");
        let json = serde_json::to_string(&hash).unwrap();
        // Should be a quoted hex string
        assert!(json.starts_with('"'));
        assert!(json.ends_with('"'));
        assert_eq!(json.len(), 18); // 16 chars + 2 quotes

        let deserialized: RapidHash = serde_json::from_str(&json).unwrap();
        assert_eq!(hash, deserialized);
    }

    #[test]
    fn test_display() {
        let hash = RapidHash::new(b"test");
        let display = format!("{}", hash);
        assert_eq!(display.len(), 16);
        assert!(display.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
