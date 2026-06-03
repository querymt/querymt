pub use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "kameo-mesh")]
const WIRE_VERSION: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedInviteGrant {
    pub grant: InviteGrant,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InviteGrant {
    pub version: u8,
    pub invite_id: String,
    pub inviter_peer_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_name: Option<String>,
    pub expires_at: u64,
    pub max_uses: u32,
    pub permissions: InvitePermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvitePermissions {
    #[serde(default)]
    pub can_invite: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MembershipToken {
    pub version: u8,
    pub mesh_id: String,
    pub peer_id: String,
    pub admitted_by: String,
    pub invite_id: String,
    pub permissions: InvitePermissions,
    pub issued_at: u64,
    pub expires_at: u64,
    pub signature: String,
}

pub fn mesh_id_for(inviter_peer_id: &str, mesh_name: Option<&str>) -> String {
    format!("{}:{}", inviter_peer_id, mesh_name.unwrap_or("anon"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl MembershipToken {
    #[cfg(feature = "kameo-mesh")]
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
            signature: String::new(),
        };

        let payload = token.signable_bytes()?;
        let sig = admitter_keypair
            .sign(&payload)
            .map_err(|e| InviteError::InvalidSignature(format!("signing failed: {e}")))?;
        token.signature = hex::encode(sig);
        Ok(token)
    }

    #[cfg(feature = "kameo-mesh")]
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

    #[cfg(feature = "kameo-mesh")]
    fn signable_bytes(&self) -> Result<Vec<u8>, InviteError> {
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

    pub fn is_expired(&self) -> bool {
        self.expires_at != 0 && now_secs() > self.expires_at
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerEntry {
    pub peer_id: String,
    pub addrs: Vec<String>,
}

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

impl InviteGrant {
    #[cfg(feature = "kameo-mesh")]
    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, InviteError> {
        let uuid = uuid::Uuid::parse_str(&self.invite_id)
            .map_err(|e| InviteError::InvalidToken(format!("invalid invite_id UUID: {e}")))?;

        let peer_id_parsed: libp2p::PeerId = self
            .inviter_peer_id
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
        buf.push(WIRE_VERSION);
        buf.extend_from_slice(uuid.as_bytes());
        buf.extend_from_slice(&peer_id_bytes);
        buf.extend_from_slice(&self.expires_at.to_be_bytes());
        buf.extend_from_slice(&self.max_uses.to_be_bytes());
        buf.push(flags);
        buf.push(name_bytes.len() as u8);
        buf.extend_from_slice(name_bytes);
        Ok(buf)
    }

    #[cfg(feature = "kameo-mesh")]
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
        if data.len() < 17 {
            return Err(InviteError::InvalidToken("token too short".to_string()));
        }

        let uuid_bytes: [u8; 16] = data[1..17]
            .try_into()
            .map_err(|_| InviteError::InvalidToken("truncated UUID".to_string()))?;
        let invite_id = uuid::Uuid::from_bytes(uuid_bytes).to_string();

        let peer_id_start = 17;
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
        let peer_id_total = 2 + mh_len;
        if data.len() < peer_id_start + peer_id_total {
            return Err(InviteError::InvalidToken(
                "token too short for PeerId payload".to_string(),
            ));
        }
        let peer_id = libp2p::PeerId::from_bytes(&data[peer_id_start..peer_id_start + peer_id_total])
            .map_err(|e| InviteError::InvalidToken(format!("invalid PeerId in wire bytes: {e}")))?;
        let pos = peer_id_start + peer_id_total;

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
                    .map_err(|e| InviteError::InvalidToken(format!("invalid mesh_name UTF-8: {e}")))?
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

    #[cfg(feature = "kameo-mesh")]
    pub fn sign(self, keypair: &libp2p::identity::Keypair) -> Result<SignedInviteGrant, InviteError> {
        let wire = self.to_wire_bytes()?;
        let signature_bytes = keypair
            .sign(&wire)
            .map_err(|e| InviteError::InvalidSignature(format!("signing failed: {e}")))?;
        Ok(SignedInviteGrant {
            grant: self,
            signature: hex::encode(signature_bytes),
        })
    }

    pub fn is_expired(&self) -> bool {
        if self.expires_at == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now > self.expires_at
    }
}

impl SignedInviteGrant {
    #[cfg(feature = "kameo-mesh")]
    pub fn verify(&self) -> Result<&InviteGrant, InviteError> {
        if self.grant.version != WIRE_VERSION {
            return Err(InviteError::InvalidToken(format!(
                "unsupported grant version: {} (expected {WIRE_VERSION})",
                self.grant.version
            )));
        }
        if self.grant.is_expired() {
            return Err(InviteError::Expired);
        }

        let peer_id: libp2p::PeerId = self
            .grant
            .inviter_peer_id
            .parse()
            .map_err(|e| InviteError::InvalidToken(format!("invalid inviter_peer_id: {e}")))?;

        let public_key = libp2p::identity::PublicKey::try_decode_protobuf(&peer_id.to_bytes()[2..])
            .map_err(|_| {
                InviteError::InvalidSignature(
                    "cannot extract public key from inviter_peer_id; only ed25519 identity PeerIds are supported"
                        .to_string(),
                )
            })?;

        let sig_bytes = hex::decode(&self.signature)
            .map_err(|e| InviteError::InvalidSignature(format!("hex decode failed: {e}")))?;

        let wire = self.grant.to_wire_bytes()?;
        if !public_key.verify(&wire, &sig_bytes) {
            return Err(InviteError::InvalidSignature(
                "ed25519 signature verification failed".to_string(),
            ));
        }

        Ok(&self.grant)
    }

    #[cfg(feature = "kameo-mesh")]
    pub fn encode(&self) -> String {
        let wire = self.grant.to_wire_bytes().unwrap_or_default();
        let sig_bytes = hex::decode(&self.signature).unwrap_or_default();
        let mut payload = wire;
        payload.extend_from_slice(&sig_bytes);
        URL_SAFE_NO_PAD.encode(&payload)
    }

    #[cfg(feature = "kameo-mesh")]
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

    #[cfg(feature = "kameo-mesh")]
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

    #[cfg(feature = "kameo-mesh")]
    pub fn to_url(&self) -> String {
        format!("qmt://mesh/join/{}", self.encode())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InviteStatus {
    Pending,
    Consumed,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteRecord {
    pub invite_id: String,
    pub grant: InviteGrant,
    pub created_at: u64,
    pub uses_remaining: u32,
    pub status: InviteStatus,
    pub used_by: Vec<String>,
}

pub struct InviteStore {
    path: PathBuf,
    records: HashMap<String, InviteRecord>,
}

impl InviteStore {
    pub fn load_or_create(path: &Path) -> Result<Self, InviteError> {
        let records = load_json_file::<HashMap<String, InviteRecord>>(path)?.unwrap_or_default();
        Ok(Self {
            path: path.to_path_buf(),
            records,
        })
    }

    #[cfg(feature = "kameo-mesh")]
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

    #[cfg(feature = "kameo-mesh")]
    pub fn admit_peer(
        &mut self,
        invite_id: &str,
        joiner_peer_id: &str,
        keypair: &libp2p::identity::Keypair,
        mesh_name: Option<&str>,
    ) -> Result<MembershipToken, InviteError> {
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
            (record.grant.permissions.clone(), record.grant.expires_at)
        };
        self.save_records()?;

        let mid = mesh_id_for(&keypair.public().to_peer_id().to_string(), mesh_name);
        MembershipToken::issue(
            mid,
            joiner_peer_id,
            keypair,
            invite_id.to_string(),
            permissions,
            expires_at,
        )
    }

    #[cfg(feature = "kameo-mesh")]
    pub fn verify_membership_token(token: &MembershipToken) -> Result<(), InviteError> {
        token.verify()
    }

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

    pub fn revoke(&mut self, invite_id: &str) -> Result<(), InviteError> {
        let record = self
            .records
            .get_mut(invite_id)
            .ok_or_else(|| InviteError::NotFound(invite_id.to_string()))?;
        record.status = InviteStatus::Revoked;
        self.save_records()?;
        Ok(())
    }

    pub fn list_pending(&self) -> Vec<&InviteRecord> {
        self.records
            .values()
            .filter(|r| r.status == InviteStatus::Pending)
            .collect()
    }

    fn save_records(&self) -> Result<(), InviteError> {
        save_json_file(&self.path, &self.records)
    }
}

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

pub fn default_invite_store_path() -> Result<PathBuf, InviteError> {
    let cfg_dir = querymt_utils::providers::config_dir()
        .map_err(|e| InviteError::StoreError(format!("cannot determine config dir: {e}")))?;
    Ok(cfg_dir.join("invites.json"))
}

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
        (s, 1u64)
    };
    num_str.parse::<u64>().ok().map(|n| n * multiplier)
}

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

#[cfg(feature = "kameo-mesh")]
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
