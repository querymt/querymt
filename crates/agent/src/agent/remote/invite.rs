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
use std::time::{SystemTime, UNIX_EPOCH};

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

// ── MembershipToken ────────────────────────────────────────────────────────────

/// A self-contained, cryptographically signed proof of mesh membership.
///
/// Issued by the admitting node after consuming an invite.  The token is
/// **verifiable by any mesh member** — they extract the admitter's ed25519
/// public key from `admitted_by` (a PeerId) and verify the signature, with no
/// shared state required.  This makes reconnection to any node possible even
/// when the original inviter is offline.
///
/// # Wire format
/// ```text
///   [0]        version (u8, always 1)
///   [1..17]    mesh_id length (u8) + mesh_id bytes (up to 255)
///   …          peer_id (PeerId multihash, variable)
///   …          admitted_by (PeerId multihash, variable)
///   …          invite_id length (u8) + invite_id bytes
///   …          issued_at (u64 big-endian)
///   …          expires_at (u64 big-endian)
///   …          flags (u8: bit0=can_invite, bit1=role_is_client)
/// ```
/// Followed immediately by the 64-byte ed25519 signature.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MembershipToken {
    /// Format version — always 1.
    pub version: u8,
    /// Deterministic mesh identifier: `"{inviter_peer_id}:{mesh_name_or_anon}"`.
    pub mesh_id: String,
    /// The admitted joiner's PeerId.
    pub peer_id: String,
    /// The admitting peer's PeerId (the signer).
    pub admitted_by: String,
    /// Which invite was consumed (audit trail, empty on token-based readmission).
    pub invite_id: String,
    /// Permissions copied from the original invite.
    pub permissions: InvitePermissions,
    /// Unix timestamp when the token was issued.
    pub issued_at: u64,
    /// Unix timestamp after which the token is invalid.  0 = no expiry.
    pub expires_at: u64,
    /// Ed25519 signature by `admitted_by` over the wire payload, hex-encoded.
    pub signature: String,
}

/// Derive the deterministic mesh identifier from the inviter's PeerId and
/// an optional human-readable mesh name.
pub fn mesh_id_for(inviter_peer_id: &str, mesh_name: Option<&str>) -> String {
    format!("{}:{}", inviter_peer_id, mesh_name.unwrap_or("anon"))
}

/// Helper: current Unix timestamp in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl MembershipToken {
    /// Build and sign a new membership token.
    ///
    /// Called by the host after `validate_and_consume` succeeds.
    #[cfg(feature = "remote")]
    pub fn issue(
        mesh_id: String,
        joiner_peer_id: &str,
        admitter_keypair: &libp2p::identity::Keypair,
        invite_id: String,
        permissions: InvitePermissions,
        expires_at: u64,
    ) -> Result<Self, InviteError> {
        let admitted_by = admitter_keypair.public().to_peer_id().to_string();
        let issued_at = now_secs();

        let mut token = Self {
            version: 1,
            mesh_id,
            peer_id: joiner_peer_id.to_string(),
            admitted_by,
            invite_id,
            permissions,
            issued_at,
            expires_at,
            signature: String::new(), // filled below
        };

        let payload = token.signable_bytes()?;
        let sig = admitter_keypair
            .sign(&payload)
            .map_err(|e| InviteError::InvalidSignature(format!("signing failed: {e}")))?;
        token.signature = hex::encode(sig);
        Ok(token)
    }

    /// Verify the token's signature and expiry.
    ///
    /// Extracts the admitter's public key from `admitted_by` (a PeerId), then
    /// verifies the signature over the signable payload.  No private key or
    /// shared store is needed — any peer can call this.
    #[cfg(feature = "remote")]
    pub fn verify(&self) -> Result<(), InviteError> {
        if self.version != 1 {
            return Err(InviteError::InvalidToken(format!(
                "unsupported membership token version: {}",
                self.version
            )));
        }

        if self.expires_at != 0 && now_secs() > self.expires_at {
            return Err(InviteError::Expired);
        }

        let peer_id: libp2p::PeerId = self
            .admitted_by
            .parse()
            .map_err(|e| InviteError::InvalidToken(format!("invalid admitted_by PeerId: {e}")))?;

        let public_key = libp2p::identity::PublicKey::try_decode_protobuf(&peer_id.to_bytes()[2..])
            .map_err(|_| {
                InviteError::InvalidSignature(
                    "cannot extract public key from admitted_by PeerId".to_string(),
                )
            })?;

        let sig_bytes = hex::decode(&self.signature)
            .map_err(|e| InviteError::InvalidSignature(format!("hex decode failed: {e}")))?;

        let payload = self.signable_bytes()?;
        if !public_key.verify(&payload, &sig_bytes) {
            return Err(InviteError::InvalidSignature(
                "membership token signature verification failed".to_string(),
            ));
        }

        Ok(())
    }

    /// The bytes that are signed: everything except the `signature` field.
    fn signable_bytes(&self) -> Result<Vec<u8>, InviteError> {
        // Simple deterministic encoding: length-prefixed UTF-8 strings + u64 fields.
        let mut buf = Vec::new();
        buf.push(self.version);
        push_str(&mut buf, &self.mesh_id)?;
        push_str(&mut buf, &self.peer_id)?;
        push_str(&mut buf, &self.admitted_by)?;
        push_str(&mut buf, &self.invite_id)?;
        buf.extend_from_slice(&self.issued_at.to_be_bytes());
        buf.extend_from_slice(&self.expires_at.to_be_bytes());
        let mut flags: u8 = 0;
        if self.permissions.can_invite {
            flags |= 0x01;
        }
        if self.permissions.role == "client" {
            flags |= 0x02;
        }
        buf.push(flags);
        Ok(buf)
    }

    /// Check whether this token has expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at != 0 && now_secs() > self.expires_at
    }
}

/// Push a length-prefixed UTF-8 string into a buffer.
/// Errors if the string exceeds 65535 bytes.
fn push_str(buf: &mut Vec<u8>, s: &str) -> Result<(), InviteError> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len > u16::MAX as usize {
        return Err(InviteError::InvalidToken(format!(
            "field too long: {len} bytes (max 65535)"
        )));
    }
    buf.extend_from_slice(&(len as u16).to_be_bytes());
    buf.extend_from_slice(bytes);
    Ok(())
}

// ── MembershipStore (joiner-side) ──────────────────────────────────────────────

/// A cached peer address for reconnection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerEntry {
    pub peer_id: String,
    /// Multiaddr strings.  For iroh transport only the PeerId is needed
    /// (`/p2p/{peer_id}`), but we store any addresses we know about.
    pub addrs: Vec<String>,
}

/// One mesh that this node is a member of.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshMembership {
    /// The signed membership certificate.
    pub token: MembershipToken,
    /// Peer addresses known at last disconnect — tried on reconnect.
    pub known_peers: Vec<PeerEntry>,
    /// Unix timestamp of last successful connection to this mesh.
    pub last_connected: u64,
}

/// File-backed store at `~/.qmt/memberships.json`.
///
/// Persists membership tokens and cached peer addresses so a joining node can
/// rejoin a mesh without re-presenting the original invite token — and without
/// needing the original inviter to be online.
pub struct MembershipStore {
    path: PathBuf,
    memberships: HashMap<String, MeshMembership>,
}

impl MembershipStore {
    /// Load an existing store from disk, or create an empty one.
    pub fn load_or_create(path: &Path) -> Result<Self, InviteError> {
        if path.exists() {
            let data = std::fs::read_to_string(path).map_err(|e| {
                InviteError::StoreError(format!("failed to read {}: {e}", path.display()))
            })?;
            let memberships: HashMap<String, MeshMembership> = serde_json::from_str(&data)
                .map_err(|e| {
                    InviteError::StoreError(format!("failed to parse {}: {e}", path.display()))
                })?;
            Ok(Self {
                path: path.to_path_buf(),
                memberships,
            })
        } else {
            Ok(Self {
                path: path.to_path_buf(),
                memberships: HashMap::new(),
            })
        }
    }

    /// Store or overwrite the membership for a mesh.
    pub fn store_membership(
        &mut self,
        mesh_id: String,
        membership: MeshMembership,
    ) -> Result<(), InviteError> {
        self.memberships.insert(mesh_id, membership);
        self.save()
    }

    /// Look up an existing membership by mesh ID.
    pub fn get_membership(&self, mesh_id: &str) -> Option<&MeshMembership> {
        self.memberships.get(mesh_id)
    }

    /// Update the cached peer list for a mesh (called while connected).
    pub fn update_known_peers(
        &mut self,
        mesh_id: &str,
        peers: Vec<PeerEntry>,
    ) -> Result<(), InviteError> {
        if let Some(m) = self.memberships.get_mut(mesh_id) {
            m.known_peers = peers;
            m.last_connected = now_secs();
            self.save()?;
        }
        Ok(())
    }

    /// Touch the `last_connected` timestamp for a mesh.
    pub fn touch_last_connected(&mut self, mesh_id: &str) -> Result<(), InviteError> {
        if let Some(m) = self.memberships.get_mut(mesh_id) {
            m.last_connected = now_secs();
            self.save()?;
        }
        Ok(())
    }

    /// Iterate all stored memberships.
    pub fn all(&self) -> impl Iterator<Item = (&str, &MeshMembership)> {
        self.memberships.iter().map(|(k, v)| (k.as_str(), v))
    }

    fn save(&self) -> Result<(), InviteError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                InviteError::StoreError(format!(
                    "failed to create directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let json = serde_json::to_string_pretty(&self.memberships)
            .map_err(|e| InviteError::StoreError(format!("serialization failed: {e}")))?;
        std::fs::write(&self.path, json).map_err(|e| {
            InviteError::StoreError(format!("failed to write {}: {e}", self.path.display()))
        })
    }
}

/// Return the default membership store path: `~/.qmt/memberships.json`.
pub fn default_membership_store_path() -> Result<PathBuf, InviteError> {
    let home = dirs::home_dir()
        .ok_or_else(|| InviteError::StoreError("cannot determine home directory".to_string()))?;
    Ok(home.join(".qmt").join("memberships.json"))
}

/// Return the default admitted-peers store path: `~/.qmt/admitted_peers.json`.
pub fn default_admitted_peers_path() -> Result<PathBuf, InviteError> {
    let home = dirs::home_dir()
        .ok_or_else(|| InviteError::StoreError("cannot determine home directory".to_string()))?;
    Ok(home.join(".qmt").join("admitted_peers.json"))
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
/// and revocation.  Admitted peers are persisted separately in
/// `~/.qmt/admitted_peers.json` so the two files can evolve independently.
pub struct InviteStore {
    /// Path to `invites.json`.
    path: PathBuf,
    records: HashMap<String, InviteRecord>,
    /// Path to `admitted_peers.json` — sidecar file for membership tokens.
    admitted_path: PathBuf,
    /// Membership tokens keyed by the joiner's PeerId string.
    admitted_peers: HashMap<String, MembershipToken>,
}

impl InviteStore {
    /// Load (or create) both `invites.json` and `admitted_peers.json`.
    pub fn load_or_create(path: &Path) -> Result<Self, InviteError> {
        // Derive the sidecar path from the primary path's parent directory.
        let admitted_path = path
            .parent()
            .ok_or_else(|| {
                InviteError::StoreError("invite store path has no parent directory".to_string())
            })?
            .join("admitted_peers.json");

        let records = load_json_file::<HashMap<String, InviteRecord>>(path)?.unwrap_or_default();
        let admitted_peers =
            load_json_file::<HashMap<String, MembershipToken>>(&admitted_path)?.unwrap_or_default();

        Ok(Self {
            path: path.to_path_buf(),
            records,
            admitted_path,
            admitted_peers,
        })
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
        let expires_at = ttl_secs.map(|ttl| now_secs() + ttl).unwrap_or(0);

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
            created_at: now_secs(),
            uses_remaining: max_uses,
            status: InviteStatus::Pending,
            used_by: Vec::new(),
        };

        self.records.insert(record.invite_id.clone(), record);
        self.save_records()?;

        Ok(signed)
    }

    /// Validate and consume one use of an invite, then issue a signed
    /// `MembershipToken` for the joiner.
    ///
    /// This is the single atomic operation that:
    /// 1. Checks the invite is valid (not revoked, not expired, uses remaining).
    /// 2. Decrements `uses_remaining` (marking `Consumed` when it hits zero).
    /// 3. Signs a `MembershipToken` with the host's keypair.
    /// 4. Persists the token to `admitted_peers.json`.
    ///
    /// The returned token is self-contained — any mesh member can verify it
    /// without contacting the issuer.
    #[cfg(feature = "remote")]
    pub fn admit_peer(
        &mut self,
        invite_id: &str,
        joiner_peer_id: &str,
        keypair: &libp2p::identity::Keypair,
        mesh_name: Option<&str>,
    ) -> Result<MembershipToken, InviteError> {
        // --- consume the invite ---
        let (permissions, expires_at) = {
            let record = self
                .records
                .get_mut(invite_id)
                .ok_or_else(|| InviteError::NotFound(invite_id.to_string()))?;

            match record.status {
                InviteStatus::Revoked => return Err(InviteError::InviteRevoked),
                InviteStatus::Consumed => return Err(InviteError::InviteConsumed),
                InviteStatus::Pending => {}
            }

            if record.grant.is_expired() {
                return Err(InviteError::Expired);
            }

            if record.grant.max_uses > 0 && record.uses_remaining == 0 {
                record.status = InviteStatus::Consumed;
                self.save_records()?;
                return Err(InviteError::InviteConsumed);
            }

            if record.grant.max_uses > 0 {
                record.uses_remaining -= 1;
                if record.uses_remaining == 0 {
                    record.status = InviteStatus::Consumed;
                }
            }
            record.used_by.push(joiner_peer_id.to_string());

            (
                record.grant.permissions.clone(),
                record.grant.expires_at, // inherit invite expiry (0 = no expiry)
            )
        };
        self.save_records()?;

        // --- issue the membership token ---
        let mid = mesh_id_for(&keypair.public().to_peer_id().to_string(), mesh_name);
        let token = MembershipToken::issue(
            mid,
            joiner_peer_id,
            keypair,
            invite_id.to_string(),
            permissions,
            expires_at,
        )?;

        self.admitted_peers
            .insert(joiner_peer_id.to_string(), token.clone());
        self.save_admitted()?;

        Ok(token)
    }

    /// Look up a previously admitted peer by their PeerId.
    pub fn is_peer_admitted(&self, peer_id: &str) -> Option<&MembershipToken> {
        self.admitted_peers.get(peer_id)
    }

    /// Iterate all admitted peers and their membership tokens.
    pub fn admitted_memberships(&self) -> impl Iterator<Item = (&str, &MembershipToken)> {
        self.admitted_peers
            .iter()
            .map(|(peer_id, token)| (peer_id.as_str(), token))
    }

    /// Verify a membership token presented by a reconnecting peer.
    ///
    /// Pure cryptographic check — no store state required.  Any mesh node can
    /// call this; it only needs the token itself.
    #[cfg(feature = "remote")]
    pub fn verify_membership_token(token: &MembershipToken) -> Result<(), InviteError> {
        token.verify()
    }

    /// Validate and consume one use of an invite (use-count tracking only).
    ///
    /// Prefer [`admit_peer`](Self::admit_peer) when you also need a membership
    /// token.  This lower-level method is kept for callers that only need the
    /// use-limit side effect.
    pub fn validate_and_consume(
        &mut self,
        invite_id: &str,
        joiner_peer_id: &str,
    ) -> Result<(), InviteError> {
        let record = self
            .records
            .get_mut(invite_id)
            .ok_or_else(|| InviteError::NotFound(invite_id.to_string()))?;

        match record.status {
            InviteStatus::Revoked => return Err(InviteError::InviteRevoked),
            InviteStatus::Consumed => return Err(InviteError::InviteConsumed),
            InviteStatus::Pending => {}
        }

        if record.grant.is_expired() {
            return Err(InviteError::Expired);
        }

        if record.grant.max_uses > 0 && record.uses_remaining == 0 {
            record.status = InviteStatus::Consumed;
            self.save_records()?;
            return Err(InviteError::InviteConsumed);
        }

        if record.grant.max_uses > 0 {
            record.uses_remaining -= 1;
            if record.uses_remaining == 0 {
                record.status = InviteStatus::Consumed;
            }
        }
        record.used_by.push(joiner_peer_id.to_string());
        self.save_records()?;
        Ok(())
    }

    /// Revoke an invite by ID.
    pub fn revoke(&mut self, invite_id: &str) -> Result<(), InviteError> {
        let record = self
            .records
            .get_mut(invite_id)
            .ok_or_else(|| InviteError::NotFound(invite_id.to_string()))?;
        record.status = InviteStatus::Revoked;
        self.save_records()?;
        Ok(())
    }

    /// List all pending (active, non-revoked, non-consumed) invites.
    pub fn list_pending(&self) -> Vec<&InviteRecord> {
        self.records
            .values()
            .filter(|r| r.status == InviteStatus::Pending)
            .collect()
    }

    // ── persistence ───────────────────────────────────────────────────────────

    fn save_records(&self) -> Result<(), InviteError> {
        save_json_file(&self.path, &self.records)
    }

    fn save_admitted(&self) -> Result<(), InviteError> {
        save_json_file(&self.admitted_path, &self.admitted_peers)
    }
}

// ── shared file I/O helpers ────────────────────────────────────────────────────

/// Read and deserialize a JSON file, returning `None` if the file does not exist.
fn load_json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, InviteError> {
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(path)
        .map_err(|e| InviteError::StoreError(format!("failed to read {}: {e}", path.display())))?;
    let value = serde_json::from_str(&data)
        .map_err(|e| InviteError::StoreError(format!("failed to parse {}: {e}", path.display())))?;
    Ok(Some(value))
}

/// Serialize and atomically write a JSON file (via a temp-file rename).
fn save_json_file<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), InviteError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            InviteError::StoreError(format!(
                "failed to create directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| InviteError::StoreError(format!("serialization failed: {e}")))?;
    std::fs::write(path, json)
        .map_err(|e| InviteError::StoreError(format!("failed to write {}: {e}", path.display())))
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

    /// Build a minimal InviteStore seeded with one record, without touching disk.
    #[cfg(feature = "remote")]
    fn store_with_record(dir: &tempfile::TempDir, id: &str, max_uses: u32) -> InviteStore {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id().to_string();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();
        store
            .create_invite(
                &keypair,
                &peer_id,
                None,
                None,
                max_uses,
                InvitePermissions::default(),
            )
            .unwrap();
        // Re-key the auto-generated record under the requested id for tests that
        // need a predictable key.  (create_invite uses uuid::now_v7)
        let record = store.records.values().next().unwrap().clone();
        store.records.remove(&record.invite_id);
        let mut r = record;
        r.invite_id = id.to_string();
        r.grant.invite_id = id.to_string();
        store.records.insert(id.to_string(), r);
        store
    }

    #[test]
    fn invite_store_load_or_create_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let store = InviteStore::load_or_create(&path).unwrap();
        assert!(store.list_pending().is_empty());
    }

    #[cfg(feature = "remote")]
    #[test]
    fn invite_store_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");

        {
            let store = store_with_record(&dir, "test-001", 1);
            store.save_records().unwrap();
        }

        let store = InviteStore::load_or_create(&path).unwrap();
        assert_eq!(store.list_pending().len(), 1);
        assert_eq!(store.list_pending()[0].invite_id, "test-001");
    }

    #[cfg(feature = "remote")]
    #[test]
    fn invite_store_validate_and_consume() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_with_record(&dir, "single-use", 1);

        store
            .validate_and_consume("single-use", "joiner-1")
            .unwrap();
        assert_eq!(store.records["single-use"].status, InviteStatus::Consumed);
        assert_eq!(store.records["single-use"].used_by, vec!["joiner-1"]);

        let result = store.validate_and_consume("single-use", "joiner-2");
        assert!(matches!(result.unwrap_err(), InviteError::InviteConsumed));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn invite_store_multi_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_with_record(&dir, "multi-use", 3);

        store.validate_and_consume("multi-use", "joiner-1").unwrap();
        store.validate_and_consume("multi-use", "joiner-2").unwrap();
        store.validate_and_consume("multi-use", "joiner-3").unwrap();

        assert_eq!(store.records["multi-use"].status, InviteStatus::Consumed);
        assert_eq!(store.records["multi-use"].used_by.len(), 3);

        let result = store.validate_and_consume("multi-use", "joiner-4");
        assert!(matches!(result.unwrap_err(), InviteError::InviteConsumed));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn invite_store_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_with_record(&dir, "to-revoke", 10);

        store.revoke("to-revoke").unwrap();
        assert_eq!(store.records["to-revoke"].status, InviteStatus::Revoked);

        let result = store.validate_and_consume("to-revoke", "joiner-1");
        assert!(matches!(result.unwrap_err(), InviteError::InviteRevoked));
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

        let verified = signed.verify().unwrap();
        assert_eq!(verified.mesh_name.as_deref(), Some("Store Test"));
        assert_eq!(verified.max_uses, 1);
        assert_eq!(store.list_pending().len(), 1);
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

        store.validate_and_consume(&invite_id, "joiner-1").unwrap();

        let result = store.validate_and_consume(&invite_id, "joiner-2");
        assert!(matches!(result.unwrap_err(), InviteError::InviteConsumed));
    }

    // ── MembershipToken tests ──────────────────────────────────────────────────

    #[cfg(feature = "remote")]
    #[test]
    fn membership_token_sign_verify_roundtrip() {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let joiner_keypair = libp2p::identity::Keypair::generate_ed25519();
        let joiner_peer_id = joiner_keypair.public().to_peer_id().to_string();
        let admitter_peer_id = keypair.public().to_peer_id().to_string();

        let token = MembershipToken::issue(
            mesh_id_for(&admitter_peer_id, Some("TestMesh")),
            &joiner_peer_id,
            &keypair,
            "invite-abc".to_string(),
            InvitePermissions::default(),
            0,
        )
        .unwrap();

        assert_eq!(token.peer_id, joiner_peer_id);
        assert_eq!(token.admitted_by, admitter_peer_id);
        token.verify().unwrap();
    }

    #[cfg(feature = "remote")]
    #[test]
    fn membership_token_verifiable_by_third_party() {
        // The admitter signs; a third party (who only has the admitter's PeerId,
        // not their private key) must be able to verify.
        let admitter_kp = libp2p::identity::Keypair::generate_ed25519();
        let joiner_kp = libp2p::identity::Keypair::generate_ed25519();
        let admitter_peer_id = admitter_kp.public().to_peer_id().to_string();
        let joiner_peer_id = joiner_kp.public().to_peer_id().to_string();

        let token = MembershipToken::issue(
            mesh_id_for(&admitter_peer_id, None),
            &joiner_peer_id,
            &admitter_kp,
            "invite-xyz".to_string(),
            InvitePermissions::default(),
            0,
        )
        .unwrap();

        // Third party: drop admitter_kp, only keep the token (which embeds the PeerId).
        drop(admitter_kp);
        // verify() extracts the public key from token.admitted_by — no private key needed.
        token.verify().unwrap();
    }

    #[cfg(feature = "remote")]
    #[test]
    fn membership_token_rejects_tampered_peer_id() {
        let kp = libp2p::identity::Keypair::generate_ed25519();
        let joiner_kp = libp2p::identity::Keypair::generate_ed25519();
        let admitter_peer_id = kp.public().to_peer_id().to_string();

        let mut token = MembershipToken::issue(
            mesh_id_for(&admitter_peer_id, None),
            &joiner_kp.public().to_peer_id().to_string(),
            &kp,
            "inv".to_string(),
            InvitePermissions::default(),
            0,
        )
        .unwrap();

        // Tamper: swap peer_id for a different peer.
        let other_kp = libp2p::identity::Keypair::generate_ed25519();
        token.peer_id = other_kp.public().to_peer_id().to_string();

        assert!(matches!(
            token.verify().unwrap_err(),
            InviteError::InvalidSignature(_)
        ));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn membership_token_rejects_expired() {
        let kp = libp2p::identity::Keypair::generate_ed25519();
        let joiner_kp = libp2p::identity::Keypair::generate_ed25519();
        let admitter_peer_id = kp.public().to_peer_id().to_string();

        let token = MembershipToken::issue(
            mesh_id_for(&admitter_peer_id, None),
            &joiner_kp.public().to_peer_id().to_string(),
            &kp,
            "inv".to_string(),
            InvitePermissions::default(),
            1, // expired: unix epoch + 1s
        )
        .unwrap();

        assert!(matches!(token.verify().unwrap_err(), InviteError::Expired));
    }

    // ── InviteStore::admit_peer tests ──────────────────────────────────────────

    #[cfg(feature = "remote")]
    #[test]
    fn admit_peer_issues_verifiable_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let host_kp = libp2p::identity::Keypair::generate_ed25519();
        let host_peer_id = host_kp.public().to_peer_id().to_string();
        let joiner_kp = libp2p::identity::Keypair::generate_ed25519();
        let joiner_peer_id = joiner_kp.public().to_peer_id().to_string();

        let signed = store
            .create_invite(
                &host_kp,
                &host_peer_id,
                Some("Mesh".to_string()),
                None,
                1,
                InvitePermissions::default(),
            )
            .unwrap();
        let invite_id = signed.grant.invite_id.clone();

        let token = store
            .admit_peer(&invite_id, &joiner_peer_id, &host_kp, Some("Mesh"))
            .unwrap();

        // Token is self-contained and verifiable.
        token.verify().unwrap();
        assert_eq!(token.peer_id, joiner_peer_id);
        assert_eq!(token.admitted_by, host_peer_id);
        assert_eq!(token.invite_id, invite_id);

        // Invite is now consumed.
        assert_eq!(store.records[&invite_id].status, InviteStatus::Consumed);

        // Admitted peer is persisted.
        assert!(store.is_peer_admitted(&joiner_peer_id).is_some());

        // Sidecar file exists on disk.
        assert!(dir.path().join("admitted_peers.json").exists());
    }

    #[cfg(feature = "remote")]
    #[test]
    fn admitted_memberships_lists_all_admitted_peers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let host_kp = libp2p::identity::Keypair::generate_ed25519();
        let host_peer_id = host_kp.public().to_peer_id().to_string();

        let signed = store
            .create_invite(
                &host_kp,
                &host_peer_id,
                Some("Mesh".to_string()),
                None,
                2,
                InvitePermissions::default(),
            )
            .unwrap();
        let invite_id = signed.grant.invite_id.clone();

        store
            .admit_peer(&invite_id, "peer-A", &host_kp, Some("Mesh"))
            .unwrap();
        store
            .admit_peer(&invite_id, "peer-B", &host_kp, Some("Mesh"))
            .unwrap();

        let admitted: std::collections::HashSet<String> = store
            .admitted_memberships()
            .map(|(peer_id, _)| peer_id.to_string())
            .collect();

        assert_eq!(admitted.len(), 2);
        assert!(admitted.contains("peer-A"));
        assert!(admitted.contains("peer-B"));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn admit_peer_single_use_rejects_second_joiner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let host_kp = libp2p::identity::Keypair::generate_ed25519();
        let host_peer_id = host_kp.public().to_peer_id().to_string();

        let signed = store
            .create_invite(
                &host_kp,
                &host_peer_id,
                None,
                None,
                1,
                InvitePermissions::default(),
            )
            .unwrap();
        let invite_id = signed.grant.invite_id.clone();

        store
            .admit_peer(&invite_id, "peer-A", &host_kp, None)
            .unwrap();

        let result = store.admit_peer(&invite_id, "peer-B", &host_kp, None);
        assert!(matches!(result.unwrap_err(), InviteError::InviteConsumed));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn admit_peer_multi_use_admits_multiple_joiners() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = InviteStore::load_or_create(&path).unwrap();

        let host_kp = libp2p::identity::Keypair::generate_ed25519();
        let host_peer_id = host_kp.public().to_peer_id().to_string();

        let signed = store
            .create_invite(
                &host_kp,
                &host_peer_id,
                None,
                None,
                3,
                InvitePermissions::default(),
            )
            .unwrap();
        let invite_id = signed.grant.invite_id.clone();

        for i in 0..3 {
            store
                .admit_peer(&invite_id, &format!("peer-{i}"), &host_kp, None)
                .unwrap();
        }
        assert_eq!(store.records[&invite_id].status, InviteStatus::Consumed);

        let result = store.admit_peer(&invite_id, "peer-overflow", &host_kp, None);
        assert!(matches!(result.unwrap_err(), InviteError::InviteConsumed));
    }

    // ── MembershipStore tests ──────────────────────────────────────────────────

    #[cfg(feature = "remote")]
    #[test]
    fn membership_store_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memberships.json");

        let kp = libp2p::identity::Keypair::generate_ed25519();
        let joiner_kp = libp2p::identity::Keypair::generate_ed25519();
        let admitter_peer_id = kp.public().to_peer_id().to_string();
        let joiner_peer_id = joiner_kp.public().to_peer_id().to_string();

        let mid = mesh_id_for(&admitter_peer_id, Some("Persist"));
        let token = MembershipToken::issue(
            mid.clone(),
            &joiner_peer_id,
            &kp,
            "inv".to_string(),
            InvitePermissions::default(),
            0,
        )
        .unwrap();

        {
            let mut store = MembershipStore::load_or_create(&path).unwrap();
            store
                .store_membership(
                    mid.clone(),
                    MeshMembership {
                        token: token.clone(),
                        known_peers: vec![PeerEntry {
                            peer_id: admitter_peer_id.clone(),
                            addrs: vec!["/p2p/12D3KooWXXX".to_string()],
                        }],
                        last_connected: 9999,
                    },
                )
                .unwrap();
        }

        let store2 = MembershipStore::load_or_create(&path).unwrap();
        let m = store2.get_membership(&mid).unwrap();
        assert_eq!(m.token, token);
        assert_eq!(m.known_peers.len(), 1);
        assert_eq!(m.last_connected, 9999);
    }

    #[cfg(feature = "remote")]
    #[test]
    fn membership_store_update_known_peers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memberships.json");

        let kp = libp2p::identity::Keypair::generate_ed25519();
        let joiner_kp = libp2p::identity::Keypair::generate_ed25519();
        let admitter_peer_id = kp.public().to_peer_id().to_string();
        let joiner_peer_id = joiner_kp.public().to_peer_id().to_string();

        let mid = mesh_id_for(&admitter_peer_id, None);
        let token = MembershipToken::issue(
            mid.clone(),
            &joiner_peer_id,
            &kp,
            "inv".to_string(),
            InvitePermissions::default(),
            0,
        )
        .unwrap();

        let mut store = MembershipStore::load_or_create(&path).unwrap();
        store
            .store_membership(
                mid.clone(),
                MeshMembership {
                    token,
                    known_peers: vec![],
                    last_connected: 0,
                },
            )
            .unwrap();

        let new_peers = vec![
            PeerEntry {
                peer_id: "peer-C".to_string(),
                addrs: vec![],
            },
            PeerEntry {
                peer_id: "peer-D".to_string(),
                addrs: vec![],
            },
        ];
        store.update_known_peers(&mid, new_peers.clone()).unwrap();

        let m = store.get_membership(&mid).unwrap();
        assert_eq!(m.known_peers, new_peers);
    }
}
