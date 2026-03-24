//! Invite tokens for dynamic mesh joining.
//!
//! An invite token contains everything a joining node needs to connect to an
//! existing mesh:
//!
//! - The inviter's `PeerId` (dialed via iroh relay to join)
//! - A shared `mesh_secret` (the sole gate for mesh membership in v1)
//! - Optional human-readable mesh name
//! - Optional expiry timestamp
//!
//! Tokens are compact enough to fit in a QR code (~200-300 bytes encoded) and
//! can also be shared as `qmt://mesh/join/<base64>` URLs or plain CLI strings.
//!
//! # Security model (v1)
//!
//! The `mesh_secret` is a 256-bit random value generated when the mesh is
//! created.  Anyone who possesses it can join.  To revoke access, rotate the
//! secret and distribute new invites to remaining members.  This is the same
//! model as WiFi passwords or Signal group invite links — simple and sufficient
//! for personal/small-team use.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

/// A mesh invite token.
///
/// Serialized compactly as JSON + base64url for sharing via QR code, URL, or
/// clipboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshInvite {
    /// `PeerId` of the inviting node (as a string).
    ///
    /// The joining node dials this peer via iroh relay to enter the mesh.
    /// Once connected, Kademlia propagates the full member list.
    pub inviter_peer_id: String,

    /// 256-bit shared secret (hex-encoded for readability in JSON).
    ///
    /// The sole gate for mesh membership in v1.  Anyone with this value can
    /// join the mesh.  Rotate to revoke outstanding invites.
    pub mesh_secret: String,

    /// Optional human-readable name for the mesh (e.g. "Alice's Mesh").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_name: Option<String>,

    /// Unix timestamp (seconds) after which the invite is no longer valid.
    ///
    /// `0` means no expiry.
    #[serde(default)]
    pub expires_at: u64,
}

/// Errors that can occur when working with invite tokens.
#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    #[error("invite token has expired")]
    Expired,

    #[error("invalid invite token: {0}")]
    InvalidToken(String),

    #[error("invalid mesh secret: {0}")]
    InvalidSecret(String),
}

impl MeshInvite {
    /// Create a new invite from a `PeerId` string and a raw 32-byte secret.
    ///
    /// # Arguments
    /// - `peer_id` — the inviter's `PeerId` (as returned by `MeshHandle::peer_id()`)
    /// - `mesh_secret` — 32-byte random value (the mesh membership gate)
    /// - `mesh_name` — optional human-readable label
    /// - `ttl_secs` — optional time-to-live in seconds; `None` means no expiry
    pub fn new(
        peer_id: &str,
        mesh_secret: &[u8; 32],
        mesh_name: Option<String>,
        ttl_secs: Option<u64>,
    ) -> Self {
        let expires_at = ttl_secs
            .map(|ttl| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    + ttl
            })
            .unwrap_or(0);

        Self {
            inviter_peer_id: peer_id.to_string(),
            mesh_secret: hex::encode(mesh_secret),
            mesh_name,
            expires_at,
        }
    }

    /// Encode the invite as a URL-safe base64 string.
    ///
    /// The format is `base64url(json(self))` — compact, no padding, safe for
    /// URLs and QR codes.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("MeshInvite is always serializable");
        URL_SAFE_NO_PAD.encode(&json)
    }

    /// Decode an invite from a URL-safe base64 string.
    pub fn decode(token: &str) -> Result<Self, InviteError> {
        // Strip the qmt://mesh/join/ prefix if present.
        let raw = token.strip_prefix("qmt://mesh/join/").unwrap_or(token);

        let bytes = URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|e| InviteError::InvalidToken(format!("base64 decode failed: {e}")))?;

        let invite: Self = serde_json::from_slice(&bytes)
            .map_err(|e| InviteError::InvalidToken(format!("json decode failed: {e}")))?;

        Ok(invite)
    }

    /// Encode as a `qmt://mesh/join/...` URL.
    pub fn to_url(&self) -> String {
        format!("qmt://mesh/join/{}", self.encode())
    }

    /// Check whether the invite has expired.
    pub fn is_expired(&self) -> bool {
        if self.expires_at == 0 {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now > self.expires_at
    }

    /// Validate the invite: check expiry and secret format.
    pub fn validate(&self) -> Result<(), InviteError> {
        if self.is_expired() {
            return Err(InviteError::Expired);
        }

        // Validate hex-encoded secret is 32 bytes (64 hex chars).
        let secret_bytes = hex::decode(&self.mesh_secret)
            .map_err(|e| InviteError::InvalidSecret(format!("hex decode failed: {e}")))?;
        if secret_bytes.len() != 32 {
            return Err(InviteError::InvalidSecret(format!(
                "expected 32 bytes, got {}",
                secret_bytes.len()
            )));
        }

        Ok(())
    }

    /// Extract the raw 32-byte mesh secret.
    pub fn secret_bytes(&self) -> Result<[u8; 32], InviteError> {
        let bytes = hex::decode(&self.mesh_secret)
            .map_err(|e| InviteError::InvalidSecret(format!("hex decode failed: {e}")))?;
        <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
            InviteError::InvalidSecret(format!("expected 32 bytes, got {}", bytes.len()))
        })
    }
}

// ── Mesh secret management ─────────────────────────────────────────────────────

/// Generate a new random 32-byte mesh secret.
pub fn generate_mesh_secret() -> [u8; 32] {
    let mut secret = [0u8; 32];
    use std::io::Read;
    // Use getrandom via std for portability (works on iOS too).
    std::io::BufReader::new(std::fs::File::open("/dev/urandom").unwrap_or_else(|_| {
        // Fallback: this should never happen on any supported platform.
        panic!("cannot open /dev/urandom for mesh secret generation");
    }))
    .read_exact(&mut secret)
    .expect("failed to read random bytes for mesh secret");
    secret
}

/// Load a mesh secret from a file, or generate and save one.
///
/// The file contains the raw 32 bytes of the secret.
pub fn load_or_generate_mesh_secret(path: &std::path::Path) -> Result<[u8; 32], anyhow::Error> {
    use anyhow::Context;

    if path.exists() {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read mesh secret from {}", path.display()))?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "mesh secret file {} has {} bytes, expected 32",
                path.display(),
                bytes.len()
            );
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&bytes);
        Ok(secret)
    } else {
        let secret = generate_mesh_secret();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, secret)
            .with_context(|| format!("failed to write mesh secret to {}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        log::info!("Generated new mesh secret, saved to {}", path.display());
        Ok(secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_secret() -> [u8; 32] {
        let mut s = [0u8; 32];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }
        s
    }

    #[test]
    fn encode_decode_roundtrip() {
        let invite = MeshInvite::new(
            "12D3KooWTestPeerId",
            &test_secret(),
            Some("Test Mesh".to_string()),
            None,
        );

        let encoded = invite.encode();
        let decoded = MeshInvite::decode(&encoded).unwrap();

        assert_eq!(invite, decoded);
    }

    #[test]
    fn url_roundtrip() {
        let invite = MeshInvite::new("12D3KooWTestPeerId", &test_secret(), None, None);

        let url = invite.to_url();
        assert!(url.starts_with("qmt://mesh/join/"));

        let decoded = MeshInvite::decode(&url).unwrap();
        assert_eq!(invite, decoded);
    }

    #[test]
    fn no_expiry_is_not_expired() {
        let invite = MeshInvite::new("peer", &test_secret(), None, None);
        assert_eq!(invite.expires_at, 0);
        assert!(!invite.is_expired());
    }

    #[test]
    fn past_expiry_is_expired() {
        let mut invite = MeshInvite::new("peer", &test_secret(), None, None);
        invite.expires_at = 1; // Unix epoch + 1 second, definitely in the past
        assert!(invite.is_expired());
    }

    #[test]
    fn future_expiry_is_not_expired() {
        let invite = MeshInvite::new("peer", &test_secret(), None, Some(3600));
        assert!(!invite.is_expired());
    }

    #[test]
    fn validate_good_invite() {
        let invite = MeshInvite::new("peer", &test_secret(), None, None);
        assert!(invite.validate().is_ok());
    }

    #[test]
    fn validate_expired_invite() {
        let mut invite = MeshInvite::new("peer", &test_secret(), None, None);
        invite.expires_at = 1;
        let err = invite.validate().unwrap_err();
        assert!(matches!(err, InviteError::Expired));
    }

    #[test]
    fn validate_bad_secret_hex() {
        let mut invite = MeshInvite::new("peer", &test_secret(), None, None);
        invite.mesh_secret = "not-valid-hex".to_string();
        let err = invite.validate().unwrap_err();
        assert!(matches!(err, InviteError::InvalidSecret(_)));
    }

    #[test]
    fn validate_wrong_length_secret() {
        let mut invite = MeshInvite::new("peer", &test_secret(), None, None);
        invite.mesh_secret = hex::encode([0u8; 16]); // 16 bytes, not 32
        let err = invite.validate().unwrap_err();
        assert!(matches!(err, InviteError::InvalidSecret(_)));
    }

    #[test]
    fn secret_bytes_extraction() {
        let secret = test_secret();
        let invite = MeshInvite::new("peer", &secret, None, None);
        assert_eq!(invite.secret_bytes().unwrap(), secret);
    }

    #[test]
    fn token_fits_in_qr_code() {
        // QR code version 10 (57x57) can hold 652 alphanumeric chars.
        // Our tokens should be well under that.
        let invite = MeshInvite::new(
            "12D3KooWGzBK3Fse6thEECCdGPC43SDfKAfqMbADhD8LqSm2hN1j",
            &test_secret(),
            Some("My Agent Mesh".to_string()),
            Some(86400),
        );
        let encoded = invite.encode();
        assert!(
            encoded.len() < 400,
            "encoded token is {} bytes, should be < 400 for QR",
            encoded.len()
        );
    }

    #[test]
    fn invalid_base64_returns_error() {
        let result = MeshInvite::decode("!!!not-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn valid_base64_but_invalid_json_returns_error() {
        let encoded = URL_SAFE_NO_PAD.encode(b"not json");
        let result = MeshInvite::decode(&encoded);
        assert!(result.is_err());
    }

    #[test]
    fn mesh_secret_generate_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh_secret");

        let s1 = load_or_generate_mesh_secret(&path).unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap().len(), 32);

        let s2 = load_or_generate_mesh_secret(&path).unwrap();
        assert_eq!(s1, s2, "loading should return the same secret");
    }
}
