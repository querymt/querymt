//! Signed invite grants for dynamic mesh joining (v2.5).
//!
//! An invite token contains everything a joining node needs to connect to an
//! existing mesh:
//!
//! - The inviter's `PeerId` (dialed via iroh relay to join)
//! - A cryptographic signature proving the invite was created by the inviter
//! - Optional human-readable mesh name
//! - Expiry timestamp (default 24h)
//! - Use limits (default single-use)
//!
//! Tokens are compact enough to fit in a QR code (~470 chars encoded, fits QR
//! version 14 at 73x73) and can also be shared as `qmt://mesh/join/<base64>`
//! URLs or plain CLI strings.
//!
//! # Security model (v2.5)
//!
//! Each invite is a **signed grant** — the host's ed25519 identity keypair signs
//! the invite payload.  The signing key never leaves the host.  Each invite is
//! individually identifiable, use-limited, expirable, and revocable.
//!
//! The joiner verifies the signature offline (no network needed for verification)
//! before attempting to connect to the inviter.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── Data structures ────────────────────────────────────────────────────────────

/// Current wire format version for binary-encoded invite tokens.
const WIRE_VERSION: u8 = 3;

/// A signed invite grant token.
///
/// Contains the grant payload and an ed25519 signature over its binary
/// wire-format serialization.  The token is encoded as
/// `base64url(wire_bytes + 64-byte signature)` for URLs and QR codes.
///
/// JSON serde derives are kept for the `InviteStore` (on-disk persistence)
/// but the wire format for sharing is compact binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedInviteGrant {
    /// The grant payload (the signed content).
    pub grant: InviteGrant,
    /// Ed25519 signature over the wire bytes of `grant`,
    /// hex-encoded (128 hex chars = 64 bytes) for JSON storage.
    pub signature: String,
}

/// The payload that is signed — contains all invite metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InviteGrant {
    /// Format version.  3 for binary wire format.
    pub version: u8,
    /// Unique invite identifier (UUID v7, time-ordered).
    pub invite_id: String,
    /// PeerId of the inviting node (entry point for joining).
    pub inviter_peer_id: String,
    /// Optional human-readable mesh name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_name: Option<String>,
    /// Unix timestamp (seconds) after which this invite is invalid.
    /// 0 = no expiry.
    pub expires_at: u64,
    /// Maximum number of times this invite can be used.
    /// 0 = unlimited.
    pub max_uses: u32,
    /// Permissions granted to the joining node.
    pub permissions: InvitePermissions,
}

/// Permissions granted to a joining node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvitePermissions {
    /// Can this joiner create their own invites?
    #[serde(default)]
    pub can_invite: bool,
    /// Role: "member" (full access) or "client" (query only).
    #[serde(default = "default_member_role")]
    pub role: String,
}

fn default_member_role() -> String {
    "member".to_string()
}

impl Default for InvitePermissions {
    fn default() -> Self {
        Self {
            can_invite: false,
            role: default_member_role(),
        }
    }
}

/// Errors that can occur when working with invite tokens.
#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    #[error("invite token has expired")]
    Expired,

    #[error("invalid invite token: {0}")]
    InvalidToken(String),

    #[error("invalid signature: {0}")]
    InvalidSignature(String),

    #[error("invite has been revoked")]
    InviteRevoked,

    #[error("invite has been fully consumed (no uses remaining)")]
    InviteConsumed,

    #[error("invite not found: {0}")]
    NotFound(String),

    #[error("store error: {0}")]
    StoreError(String),
}

// ── InviteGrant methods ────────────────────────────────────────────────────────

impl InviteGrant {
    /// Serialize the grant to compact binary wire format.
    ///
    /// Layout:
    /// ```text
    ///   [0]       version (u8, always 3)
    ///   [1..17]   invite_id (16 bytes, raw UUID)
    ///   [17..55]  inviter_peer_id (38 bytes, raw PeerId multihash)
    ///   [55..63]  expires_at (u64 big-endian)
    ///   [63..67]  max_uses (u32 big-endian)
    ///   [67]      flags (u8: bit0=can_invite, bit1=role_is_client)
    ///   [68]      mesh_name_len (u8, 0 = no name)
    ///   [69..69+N] mesh_name (UTF-8 bytes)
    /// ```
    #[cfg(feature = "remote")]
    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, InviteError> {
        let uuid = uuid::Uuid::parse_str(&self.invite_id)
            .map_err(|e| InviteError::InvalidToken(format!("invalid invite_id UUID: {e}")))?;

        let peer_id_str = &self.inviter_peer_id;
        // PeerId string → PeerId → raw bytes (38 bytes for ed25519 identity multihash)
        let peer_id_parsed: libp2p::PeerId = peer_id_str
            .parse()
            .map_err(|e| InviteError::InvalidToken(format!("invalid inviter_peer_id: {e}")))?;
        let peer_id_bytes = peer_id_parsed.to_bytes();

        let name_bytes = self.mesh_name.as_deref().unwrap_or("").as_bytes();
        if name_bytes.len() > 255 {
            return Err(InviteError::InvalidToken(
                "mesh_name exceeds 255 bytes".to_string(),
            ));
        }

        let mut flags: u8 = 0;
        if self.permissions.can_invite {
            flags |= 0x01;
        }
        if self.permissions.role == "client" {
            flags |= 0x02;
        }

        let mut buf = Vec::with_capacity(69 + name_bytes.len());
        buf.push(WIRE_VERSION); // [0]
        buf.extend_from_slice(uuid.as_bytes()); // [1..17]
        buf.extend_from_slice(&peer_id_bytes); // [17..17+N] (typically 38 bytes)
        buf.extend_from_slice(&self.expires_at.to_be_bytes()); // +8
        buf.extend_from_slice(&self.max_uses.to_be_bytes()); // +4
        buf.push(flags); // +1
        buf.push(name_bytes.len() as u8); // +1
        buf.extend_from_slice(name_bytes); // +N

        Ok(buf)
    }

    /// Deserialize a grant from compact binary wire format.
    ///
    /// Returns `(grant, bytes_consumed)` so the caller knows where the
    /// signature starts.
    #[cfg(feature = "remote")]
    pub fn from_wire_bytes(data: &[u8]) -> Result<(Self, usize), InviteError> {
        if data.is_empty() {
            return Err(InviteError::InvalidToken("empty token".to_string()));
        }
        let version = data[0];
        if version != WIRE_VERSION {
            return Err(InviteError::InvalidToken(format!(
                "unsupported wire version: {version} (expected {WIRE_VERSION})"
            )));
        }

        // Minimum: 1 (ver) + 16 (uuid) + 2 (min peer_id varint) = 19 bytes
        // but we need to parse PeerId from multihash which is variable-length.
        // For ed25519 identity PeerIds it's always 38 bytes.
        // We'll read the PeerId length from the multihash prefix.
        if data.len() < 17 {
            return Err(InviteError::InvalidToken("token too short".to_string()));
        }

        let uuid_bytes: [u8; 16] = data[1..17]
            .try_into()
            .map_err(|_| InviteError::InvalidToken("truncated UUID".to_string()))?;
        let invite_id = uuid::Uuid::from_bytes(uuid_bytes).to_string();

        // Parse PeerId from remaining bytes.  For ed25519 identity keys the
        // multihash is: 0x00 (identity code) + varint(length=36) + 36 bytes
        // protobuf-encoded public key = 38 bytes total.
        //
        // We read the multihash length from the varint at byte 1 to support
        // other key types in the future, but for now ed25519 is always 38 bytes.
        let peer_id_start = 17;
        // The multihash starts with a code byte (0x00 for identity) followed
        // by a varint length.  For ed25519: 0x00, 0x24 (36), then 36 bytes = 38 total.
        // Read the varint to determine actual length.
        if data.len() < peer_id_start + 2 {
            return Err(InviteError::InvalidToken(
                "token too short for PeerId".to_string(),
            ));
        }
        let mh_code = data[peer_id_start];
        if mh_code != 0x00 {
            return Err(InviteError::InvalidToken(format!(
                "unsupported multihash code: 0x{mh_code:02x} (expected 0x00 identity)"
            )));
        }
        let mh_len = data[peer_id_start + 1] as usize;
        let peer_id_total = 2 + mh_len; // code + varint + payload
        if data.len() < peer_id_start + peer_id_total {
            return Err(InviteError::InvalidToken(
                "token too short for PeerId payload".to_string(),
            ));
        }
        let peer_id =
            libp2p::PeerId::from_bytes(&data[peer_id_start..peer_id_start + peer_id_total])
                .map_err(|e| {
                    InviteError::InvalidToken(format!("invalid PeerId in wire bytes: {e}"))
                })?;
        let pos = peer_id_start + peer_id_total;

        // Need: 8 (expires) + 4 (max_uses) + 1 (flags) + 1 (name_len) = 14 bytes
        if data.len() < pos + 14 {
            return Err(InviteError::InvalidToken(
                "token too short for fixed fields".to_string(),
            ));
        }

        let expires_at = u64::from_be_bytes(
            data[pos..pos + 8]
                .try_into()
                .map_err(|_| InviteError::InvalidToken("truncated expires_at".to_string()))?,
        );
        let max_uses = u32::from_be_bytes(
            data[pos + 8..pos + 12]
                .try_into()
                .map_err(|_| InviteError::InvalidToken("truncated max_uses".to_string()))?,
        );
        let flags = data[pos + 12];
        let name_len = data[pos + 13] as usize;

        let name_start = pos + 14;
        if data.len() < name_start + name_len {
            return Err(InviteError::InvalidToken(
                "token too short for mesh_name".to_string(),
            ));
        }

        let mesh_name = if name_len == 0 {
            None
        } else {
            Some(
                std::str::from_utf8(&data[name_start..name_start + name_len])
                    .map_err(|e| {
                        InviteError::InvalidToken(format!("invalid mesh_name UTF-8: {e}"))
                    })?
                    .to_string(),
            )
        };

        let grant = InviteGrant {
            version,
            invite_id,
            inviter_peer_id: peer_id.to_string(),
            mesh_name,
            expires_at,
            max_uses,
            permissions: InvitePermissions {
                can_invite: flags & 0x01 != 0,
                role: if flags & 0x02 != 0 {
                    "client".to_string()
                } else {
                    "member".to_string()
                },
            },
        };

        Ok((grant, name_start + name_len))
    }

    /// Sign this grant with an ed25519 keypair, producing a `SignedInviteGrant`.
    #[cfg(feature = "remote")]
    pub fn sign(
        self,
        keypair: &libp2p::identity::Keypair,
    ) -> Result<SignedInviteGrant, InviteError> {
        let wire = self.to_wire_bytes()?;
        let signature_bytes = keypair
            .sign(&wire)
            .map_err(|e| InviteError::InvalidSignature(format!("signing failed: {e}")))?;
        Ok(SignedInviteGrant {
            grant: self,
            signature: hex::encode(signature_bytes),
        })
    }

    /// Check whether the grant has expired.
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
}

// ── SignedInviteGrant methods ──────────────────────────────────────────────────

impl SignedInviteGrant {
    /// Verify the signature and return the grant if valid.
    ///
    /// Extracts the inviter's public key from `grant.inviter_peer_id`, then
    /// verifies the ed25519 signature over the binary wire format of the grant.
    /// Also checks expiry and version.
    #[cfg(feature = "remote")]
    pub fn verify(&self) -> Result<&InviteGrant, InviteError> {
        // Check version.
        if self.grant.version != WIRE_VERSION {
            return Err(InviteError::InvalidToken(format!(
                "unsupported grant version: {} (expected {WIRE_VERSION})",
                self.grant.version
            )));
        }

        // Check expiry.
        if self.grant.is_expired() {
            return Err(InviteError::Expired);
        }

        // Parse the inviter's PeerId to extract the public key.
        let peer_id: libp2p::PeerId = self
            .grant
            .inviter_peer_id
            .parse()
            .map_err(|e| InviteError::InvalidToken(format!("invalid inviter_peer_id: {e}")))?;

        // Extract the ed25519 public key from the PeerId.
        // For identity-multihash PeerIds the protobuf-encoded public key
        // starts at byte offset 2 (after the multihash code + length prefix).
        let public_key = libp2p::identity::PublicKey::try_decode_protobuf(&peer_id.to_bytes()[2..])
            .map_err(|_| {
                InviteError::InvalidSignature(
                    "cannot extract public key from inviter_peer_id; \
                 only ed25519 identity PeerIds are supported"
                        .to_string(),
                )
            })?;

        // Decode the hex signature.
        let sig_bytes = hex::decode(&self.signature)
            .map_err(|e| InviteError::InvalidSignature(format!("hex decode failed: {e}")))?;

        // Verify the signature over the wire-format grant bytes.
        let wire = self.grant.to_wire_bytes()?;
        if !public_key.verify(&wire, &sig_bytes) {
            return Err(InviteError::InvalidSignature(
                "ed25519 signature verification failed".to_string(),
            ));
        }

        Ok(&self.grant)
    }

    /// Encode the signed grant as a URL-safe base64 string.
    ///
    /// Format: `base64url(wire_bytes + 64-byte-signature)` — compact binary.
    #[cfg(feature = "remote")]
    pub fn encode(&self) -> String {
        let wire = self.grant.to_wire_bytes().unwrap_or_default();
        let sig_bytes = hex::decode(&self.signature).unwrap_or_default();
        let mut payload = wire;
        payload.extend_from_slice(&sig_bytes);
        URL_SAFE_NO_PAD.encode(&payload)
    }

    /// Decode a signed grant from a URL-safe base64 string (v3 binary wire format).
    #[cfg(feature = "remote")]
    pub fn decode(token: &str) -> Result<Self, InviteError> {
        let raw = token.strip_prefix("qmt://mesh/join/").unwrap_or(token);

        let bytes = URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|e| InviteError::InvalidToken(format!("base64 decode failed: {e}")))?;

        if bytes.is_empty() {
            return Err(InviteError::InvalidToken("empty token".to_string()));
        }

        if bytes[0] != WIRE_VERSION {
            return Err(InviteError::InvalidToken(format!(
                "unsupported token version: {} (expected {WIRE_VERSION})",
                bytes[0]
            )));
        }

        Self::decode_binary(&bytes)
    }

    /// Decode from binary wire format: `wire_bytes + 64-byte signature`.
    #[cfg(feature = "remote")]
    fn decode_binary(bytes: &[u8]) -> Result<Self, InviteError> {
        let (grant, consumed) = InviteGrant::from_wire_bytes(bytes)?;

        let sig_start = consumed;
        if bytes.len() < sig_start + 64 {
            return Err(InviteError::InvalidToken(format!(
                "token too short for signature: {} bytes remaining, need 64",
                bytes.len().saturating_sub(sig_start)
            )));
        }
        if bytes.len() > sig_start + 64 {
            return Err(InviteError::InvalidToken(format!(
                "unexpected trailing bytes: {} extra",
                bytes.len() - sig_start - 64
            )));
        }

        let signature = hex::encode(&bytes[sig_start..sig_start + 64]);

        Ok(Self { grant, signature })
    }

    /// Encode as a `qmt://mesh/join/...` URL.
    #[cfg(feature = "remote")]
    pub fn to_url(&self) -> String {
        format!("qmt://mesh/join/{}", self.encode())
    }
}

// ── InviteStore (host-side tracking) ───────────────────────────────────────────

/// Status of an invite in the store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InviteStatus {
    /// Waiting for joiners.
    Pending,
    /// max_uses reached.
    Consumed,
    /// Manually revoked by host.
    Revoked,
}

/// Host-side record for tracking an issued invite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteRecord {
    pub invite_id: String,
    pub grant: InviteGrant,
    pub created_at: u64,
    pub uses_remaining: u32,
    pub status: InviteStatus,
    /// PeerIds of joiners who have used this invite.
    pub used_by: Vec<String>,
}

/// File-backed invite store at `~/.qmt/invites.json`.
///
/// Tracks all invites created by this host for audit, use-limit enforcement,
/// and revocation.
pub struct InviteStore {
    path: PathBuf,
    records: HashMap<String, InviteRecord>,
}

impl InviteStore {
    /// Load an existing store from disk, or create an empty one.
    pub fn load_or_create(path: &Path) -> Result<Self, InviteError> {
        if path.exists() {
            let data = std::fs::read_to_string(path).map_err(|e| {
                InviteError::StoreError(format!("failed to read {}: {e}", path.display()))
            })?;
            let records: HashMap<String, InviteRecord> =
                serde_json::from_str(&data).map_err(|e| {
                    InviteError::StoreError(format!("failed to parse {}: {e}", path.display()))
                })?;
            Ok(Self {
                path: path.to_path_buf(),
                records,
            })
        } else {
            Ok(Self {
                path: path.to_path_buf(),
                records: HashMap::new(),
            })
        }
    }

    /// Create a new signed invite and store the record.
    #[cfg(feature = "remote")]
    pub fn create_invite(
        &mut self,
        keypair: &libp2p::identity::Keypair,
        peer_id_str: &str,
        mesh_name: Option<String>,
        ttl_secs: Option<u64>,
        max_uses: u32,
        permissions: InvitePermissions,
    ) -> Result<SignedInviteGrant, InviteError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let expires_at = ttl_secs.map(|ttl| now + ttl).unwrap_or(0);

        let grant = InviteGrant {
            version: WIRE_VERSION,
            invite_id: uuid::Uuid::now_v7().to_string(),
            inviter_peer_id: peer_id_str.to_string(),
            mesh_name,
            expires_at,
            max_uses,
            permissions,
        };

        let signed = grant.clone().sign(keypair)?;

        let record = InviteRecord {
            invite_id: grant.invite_id.clone(),
            grant,
            created_at: now,
            uses_remaining: max_uses,
            status: InviteStatus::Pending,
            used_by: Vec::new(),
        };

        self.records.insert(record.invite_id.clone(), record);
        self.save()?;

        Ok(signed)
    }

    /// Validate and consume one use of an invite.
    ///
    /// Checks status, expiry, and remaining uses.  On success, decrements
    /// `uses_remaining` and records the joiner's PeerId.
    pub fn validate_and_consume(
        &mut self,
        invite_id: &str,
        joiner_peer_id: &str,
    ) -> Result<(), InviteError> {
        let record = self
            .records
            .get_mut(invite_id)
            .ok_or_else(|| InviteError::NotFound(invite_id.to_string()))?;

        // Check status.
        match record.status {
            InviteStatus::Revoked => return Err(InviteError::InviteRevoked),
            InviteStatus::Consumed => return Err(InviteError::InviteConsumed),
            InviteStatus::Pending => {}
        }

        // Check expiry.
        if record.grant.is_expired() {
            return Err(InviteError::Expired);
        }

        // Check uses (0 = unlimited).
        if record.grant.max_uses > 0 && record.uses_remaining == 0 {
            record.status = InviteStatus::Consumed;
            self.save()?;
            return Err(InviteError::InviteConsumed);
        }

        // Consume one use.
        if record.grant.max_uses > 0 {
            record.uses_remaining -= 1;
            if record.uses_remaining == 0 {
                record.status = InviteStatus::Consumed;
            }
        }
        record.used_by.push(joiner_peer_id.to_string());
        self.save()?;

        Ok(())
    }

    /// Revoke an invite by ID.
    pub fn revoke(&mut self, invite_id: &str) -> Result<(), InviteError> {
        let record = self
            .records
            .get_mut(invite_id)
            .ok_or_else(|| InviteError::NotFound(invite_id.to_string()))?;

        record.status = InviteStatus::Revoked;
        self.save()?;
        Ok(())
    }

    /// List all pending (active, non-revoked, non-consumed) invites.
    pub fn list_pending(&self) -> Vec<&InviteRecord> {
        self.records
            .values()
            .filter(|r| r.status == InviteStatus::Pending)
            .collect()
    }

    /// Persist the store to disk.
    fn save(&self) -> Result<(), InviteError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                InviteError::StoreError(format!(
                    "failed to create directory {}: {e}",
                    parent.display()
                ))
            })?;
        }

        let json = serde_json::to_string_pretty(&self.records)
            .map_err(|e| InviteError::StoreError(format!("serialization failed: {e}")))?;

        std::fs::write(&self.path, json).map_err(|e| {
            InviteError::StoreError(format!("failed to write {}: {e}", self.path.display()))
        })?;

        Ok(())
    }
}

// ── Default invite store path ──────────────────────────────────────────────────

/// Return the default invite store path: `~/.qmt/invites.json`.
pub fn default_invite_store_path() -> Result<PathBuf, InviteError> {
    let home = dirs::home_dir()
        .ok_or_else(|| InviteError::StoreError("cannot determine home directory".to_string()))?;
    Ok(home.join(".qmt").join("invites.json"))
}

// ── Duration parsing utilities ─────────────────────────────────────────────────

/// Parse a human-friendly duration string (e.g. "24h", "7d", "30m", "none")
/// into seconds.  Returns `None` for "none" (no expiry).
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("none") {
        return None;
    }
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('d') {
        (n, 86400u64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else {
        // Assume seconds if no suffix.
        (s, 1u64)
    };
    num_str.parse::<u64>().ok().map(|n| n * multiplier)
}

/// Format seconds into a human-friendly duration string.
pub fn format_duration_human(secs: u64) -> String {
    if secs >= 86400 && secs.is_multiple_of(86400) {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a valid ed25519 PeerId string for tests.
    fn test_peer_id() -> String {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        keypair.public().to_peer_id().to_string()
    }

    /// Build a test grant with valid PeerId and UUID.
    fn test_grant(mesh_name: Option<&str>) -> InviteGrant {
        InviteGrant {
            version: 3,
            invite_id: uuid::Uuid::now_v7().to_string(),
            inviter_peer_id: test_peer_id(),
            mesh_name: mesh_name.map(str::to_string),
            expires_at: 0,
            max_uses: 1,
            permissions: InvitePermissions::default(),
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let grant = test_grant(Some("Test Mesh"));
        let signed = SignedInviteGrant {
            grant,
            signature: hex::encode([0u8; 64]),
        };

        let encoded = signed.encode();
        let decoded = SignedInviteGrant::decode(&encoded).unwrap();
        assert_eq!(signed, decoded);
    }

    #[test]
    fn url_roundtrip() {
        let grant = test_grant(None);
        let signed = SignedInviteGrant {
            grant,
            signature: hex::encode([0u8; 64]),
        };

        let url = signed.to_url();
        assert!(url.starts_with("qmt://mesh/join/"));

        let decoded = SignedInviteGrant::decode(&url).unwrap();
        assert_eq!(signed, decoded);
    }

    #[test]
    fn no_expiry_is_not_expired() {
        let grant = InviteGrant {
            version: 3,
            invite_id: "test".to_string(),
            inviter_peer_id: "peer".to_string(),
            mesh_name: None,
            expires_at: 0,
            max_uses: 1,
            permissions: InvitePermissions::default(),
        };
        assert!(!grant.is_expired());
    }

    #[test]
    fn past_expiry_is_expired() {
        let grant = InviteGrant {
            version: 3,
            invite_id: "test".to_string(),
            inviter_peer_id: "peer".to_string(),
            mesh_name: None,
            expires_at: 1, // definitely in the past
            max_uses: 1,
            permissions: InvitePermissions::default(),
        };
        assert!(grant.is_expired());
    }

    #[test]
    fn future_expiry_is_not_expired() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let grant = InviteGrant {
            version: 3,
            invite_id: "test".to_string(),
            inviter_peer_id: "peer".to_string(),
            mesh_name: None,
            expires_at: now + 3600,
            max_uses: 1,
            permissions: InvitePermissions::default(),
        };
        assert!(!grant.is_expired());
    }

    #[test]
    fn invalid_base64_returns_error() {
        let result = SignedInviteGrant::decode("!!!not-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn valid_base64_but_invalid_payload_returns_error() {
        let encoded = URL_SAFE_NO_PAD.encode(b"not a valid token");
        let result = SignedInviteGrant::decode(&encoded);
        assert!(result.is_err());
    }

    #[test]
    fn token_fits_in_qr_code() {
        // Binary v3 tokens should be ~180 base64 chars (no name) or ~200 (with name).
        // QR version 9 (53x53) at EcLevel::L holds 230 bytes.
        let grant = test_grant(Some("My Agent Mesh"));
        let signed = SignedInviteGrant {
            grant,
            signature: hex::encode([0xABu8; 64]),
        };
        let encoded = signed.encode();
        assert!(
            encoded.len() < 300,
            "encoded token is {} chars, should be < 300 for compact binary format",
            encoded.len()
        );
    }

    #[test]
    fn wire_bytes_roundtrip() {
        let grant = test_grant(Some("Wire Test"));
        let wire = grant.to_wire_bytes().unwrap();
        let (decoded, consumed) = InviteGrant::from_wire_bytes(&wire).unwrap();
        assert_eq!(consumed, wire.len());
        assert_eq!(grant, decoded);
    }

    #[test]
    fn wire_bytes_no_name_roundtrip() {
        let grant = test_grant(None);
        let wire = grant.to_wire_bytes().unwrap();
        let (decoded, consumed) = InviteGrant::from_wire_bytes(&wire).unwrap();
        assert_eq!(consumed, wire.len());
        assert_eq!(grant, decoded);
    }

    #[test]
    fn wire_bytes_permissions_roundtrip() {
        let mut grant = test_grant(None);
        grant.permissions = InvitePermissions {
            can_invite: true,
            role: "client".to_string(),
        };
        let wire = grant.to_wire_bytes().unwrap();
        let (decoded, _) = InviteGrant::from_wire_bytes(&wire).unwrap();
        assert!(decoded.permissions.can_invite);
        assert_eq!(decoded.permissions.role, "client");
    }

    #[test]
    fn default_permissions() {
        let perms = InvitePermissions::default();
        assert!(!perms.can_invite);
        assert_eq!(perms.role, "member");
    }

    // ── Sign/verify roundtrip (requires `remote` feature) ──────────────────────

    #[cfg(feature = "remote")]
    #[test]
    fn sign_verify_roundtrip() {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id().to_string();

        let grant = InviteGrant {
            version: 3,
            invite_id: uuid::Uuid::now_v7().to_string(),
            inviter_peer_id: peer_id,
            mesh_name: Some("Test Mesh".to_string()),
            expires_at: 0,
            max_uses: 1,
            permissions: InvitePermissions::default(),
        };

        let signed = grant.sign(&keypair).unwrap();

        // Verify should succeed.
        let verified = signed.verify().unwrap();
        assert_eq!(verified.version, 3);
        assert_eq!(verified.mesh_name.as_deref(), Some("Test Mesh"));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn verify_rejects_tampered_grant() {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id().to_string();

        let grant = InviteGrant {
            version: 3,
            invite_id: uuid::Uuid::now_v7().to_string(),
            inviter_peer_id: peer_id,
            mesh_name: Some("Original".to_string()),
            expires_at: 0,
            max_uses: 1,
            permissions: InvitePermissions::default(),
        };

        let mut signed = grant.sign(&keypair).unwrap();
        // Tamper with the grant.
        signed.grant.mesh_name = Some("Tampered".to_string());

        let result = signed.verify();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            InviteError::InvalidSignature(_)
        ));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn verify_rejects_wrong_signer() {
        let keypair1 = libp2p::identity::Keypair::generate_ed25519();
        let keypair2 = libp2p::identity::Keypair::generate_ed25519();
        let peer_id2 = keypair2.public().to_peer_id().to_string();

        let grant = InviteGrant {
            version: 3,
            invite_id: uuid::Uuid::now_v7().to_string(),
            inviter_peer_id: peer_id2, // PeerId of keypair2
            mesh_name: None,
            expires_at: 0,
            max_uses: 1,
            permissions: InvitePermissions::default(),
        };

        // Sign with keypair1 but grant claims inviter is keypair2.
        let signed = grant.sign(&keypair1).unwrap();

        let result = signed.verify();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            InviteError::InvalidSignature(_)
        ));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn verify_rejects_expired_grant() {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id().to_string();

        let grant = InviteGrant {
            version: 3,
            invite_id: uuid::Uuid::now_v7().to_string(),
            inviter_peer_id: peer_id,
            mesh_name: None,
            expires_at: 1, // expired
            max_uses: 1,
            permissions: InvitePermissions::default(),
        };

        let signed = grant.sign(&keypair).unwrap();
        let result = signed.verify();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), InviteError::Expired));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn full_encode_decode_verify_roundtrip() {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id().to_string();

        let grant = InviteGrant {
            version: 3,
            invite_id: uuid::Uuid::now_v7().to_string(),
            inviter_peer_id: peer_id,
            mesh_name: Some("E2E Test".to_string()),
            expires_at: 0,
            max_uses: 5,
            permissions: InvitePermissions {
                can_invite: true,
                role: "member".to_string(),
            },
        };

        let signed = grant.sign(&keypair).unwrap();
        let url = signed.to_url();

        // Decode from URL.
        let decoded = SignedInviteGrant::decode(&url).unwrap();
        assert_eq!(signed, decoded);

        // Verify the decoded token.
        let verified = decoded.verify().unwrap();
        assert_eq!(verified.max_uses, 5);
        assert!(verified.permissions.can_invite);
    }

    // ── InviteStore tests ──────────────────────────────────────────────────────

    #[test]
    fn invite_store_load_or_create_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");

        let store = InviteStore::load_or_create(&path).unwrap();
        assert!(store.list_pending().is_empty());
    }

    #[test]
    fn invite_store_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");

        // Create a store with a record.
        {
            let mut store = InviteStore::load_or_create(&path).unwrap();
            let record = InviteRecord {
                invite_id: "test-001".to_string(),
                grant: InviteGrant {
                    version: 3,
                    invite_id: "test-001".to_string(),
                    inviter_peer_id: "peer".to_string(),
                    mesh_name: None,
                    expires_at: 0,
                    max_uses: 1,
                    permissions: InvitePermissions::default(),
                },
                created_at: 1000,
                uses_remaining: 1,
                status: InviteStatus::Pending,
                used_by: vec![],
            };
            store.records.insert("test-001".to_string(), record);
            store.save().unwrap();
        }

        // Reload and verify.
        let store = InviteStore::load_or_create(&path).unwrap();
        assert_eq!(store.list_pending().len(), 1);
        assert_eq!(store.list_pending()[0].invite_id, "test-001");
    }

    #[test]
    fn invite_store_validate_and_consume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        // Insert a single-use invite.
        let record = InviteRecord {
            invite_id: "single-use".to_string(),
            grant: InviteGrant {
                version: 3,
                invite_id: "single-use".to_string(),
                inviter_peer_id: "peer".to_string(),
                mesh_name: None,
                expires_at: 0,
                max_uses: 1,
                permissions: InvitePermissions::default(),
            },
            created_at: 1000,
            uses_remaining: 1,
            status: InviteStatus::Pending,
            used_by: vec![],
        };
        store.records.insert("single-use".to_string(), record);

        // First use should succeed.
        store
            .validate_and_consume("single-use", "joiner-1")
            .unwrap();
        assert_eq!(store.records["single-use"].status, InviteStatus::Consumed);
        assert_eq!(store.records["single-use"].used_by, vec!["joiner-1"]);

        // Second use should fail.
        let result = store.validate_and_consume("single-use", "joiner-2");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), InviteError::InviteConsumed));
    }

    #[test]
    fn invite_store_multi_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let record = InviteRecord {
            invite_id: "multi-use".to_string(),
            grant: InviteGrant {
                version: 3,
                invite_id: "multi-use".to_string(),
                inviter_peer_id: "peer".to_string(),
                mesh_name: None,
                expires_at: 0,
                max_uses: 3,
                permissions: InvitePermissions::default(),
            },
            created_at: 1000,
            uses_remaining: 3,
            status: InviteStatus::Pending,
            used_by: vec![],
        };
        store.records.insert("multi-use".to_string(), record);

        // Three uses should succeed.
        store.validate_and_consume("multi-use", "joiner-1").unwrap();
        store.validate_and_consume("multi-use", "joiner-2").unwrap();
        store.validate_and_consume("multi-use", "joiner-3").unwrap();

        assert_eq!(store.records["multi-use"].status, InviteStatus::Consumed);
        assert_eq!(store.records["multi-use"].used_by.len(), 3);

        // Fourth use should fail.
        let result = store.validate_and_consume("multi-use", "joiner-4");
        assert!(matches!(result.unwrap_err(), InviteError::InviteConsumed));
    }

    #[test]
    fn invite_store_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let record = InviteRecord {
            invite_id: "to-revoke".to_string(),
            grant: InviteGrant {
                version: 3,
                invite_id: "to-revoke".to_string(),
                inviter_peer_id: "peer".to_string(),
                mesh_name: None,
                expires_at: 0,
                max_uses: 10,
                permissions: InvitePermissions::default(),
            },
            created_at: 1000,
            uses_remaining: 10,
            status: InviteStatus::Pending,
            used_by: vec![],
        };
        store.records.insert("to-revoke".to_string(), record);

        // Revoke.
        store.revoke("to-revoke").unwrap();
        assert_eq!(store.records["to-revoke"].status, InviteStatus::Revoked);

        // Use should fail.
        let result = store.validate_and_consume("to-revoke", "joiner-1");
        assert!(matches!(result.unwrap_err(), InviteError::InviteRevoked));

        // Not in pending list.
        assert!(store.list_pending().is_empty());
    }

    #[test]
    fn invite_store_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let result = store.validate_and_consume("nonexistent", "joiner");
        assert!(matches!(result.unwrap_err(), InviteError::NotFound(_)));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn invite_store_create_invite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id().to_string();

        let signed = store
            .create_invite(
                &keypair,
                &peer_id,
                Some("Store Test".to_string()),
                Some(3600),
                1,
                InvitePermissions::default(),
            )
            .unwrap();

        // Verify the returned token.
        let verified = signed.verify().unwrap();
        assert_eq!(verified.mesh_name.as_deref(), Some("Store Test"));
        assert_eq!(verified.max_uses, 1);

        // Store should have one pending invite.
        assert_eq!(store.list_pending().len(), 1);

        // File should exist on disk.
        assert!(path.exists());
    }

    #[cfg(feature = "remote")]
    #[test]
    fn invite_store_create_consume_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id().to_string();

        let signed = store
            .create_invite(
                &keypair,
                &peer_id,
                None,
                None,
                1,
                InvitePermissions::default(),
            )
            .unwrap();

        let invite_id = signed.grant.invite_id.clone();

        // Consume.
        store.validate_and_consume(&invite_id, "joiner-1").unwrap();

        // Second consume should fail.
        let result = store.validate_and_consume(&invite_id, "joiner-2");
        assert!(matches!(result.unwrap_err(), InviteError::InviteConsumed));
    }
}
