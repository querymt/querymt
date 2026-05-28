use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::invite::{InviteError, MembershipToken, PeerEntry};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshState {
    pub version: u32,
    pub meshes: BTreeMap<String, MeshStateEntry>,
}

impl Default for MeshState {
    fn default() -> Self {
        Self {
            version: 1,
            meshes: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshStateEntry {
    pub mesh_id: String,
    pub name: Option<String>,
    pub role: MeshLocalRole,
    pub status: MeshStatus,
    pub membership_token: Option<MembershipToken>,
    pub admitted_peers: BTreeMap<String, MembershipToken>,
    pub known_peers: BTreeMap<String, PeerEntry>,
    pub invite_ids: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_connected: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeshLocalRole {
    Host,
    Member,
    Both,
}

impl MeshLocalRole {
    pub fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Both, _) | (_, Self::Both) => Self::Both,
            (Self::Host, Self::Member) | (Self::Member, Self::Host) => Self::Both,
            (left, _) => left,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeshStatus {
    Active,
    Left,
    Revoked,
}

pub struct MeshStateStore {
    path: PathBuf,
    state: MeshState,
}

impl MeshStateStore {
    pub fn load_or_create(path: &Path) -> Result<Self, InviteError> {
        let state = if path.exists() {
            let data = std::fs::read_to_string(path).map_err(|e| {
                InviteError::StoreError(format!("failed to read {}: {e}", path.display()))
            })?;
            serde_json::from_str(&data).map_err(|e| {
                InviteError::StoreError(format!("failed to parse {}: {e}", path.display()))
            })?
        } else {
            MeshState::default()
        };
        Ok(Self {
            path: path.to_path_buf(),
            state,
        })
    }

    pub fn active_mesh_ids(&self) -> Vec<String> {
        self.state
            .meshes
            .values()
            .filter(|entry| entry.status == MeshStatus::Active)
            .map(|entry| entry.mesh_id.clone())
            .collect()
    }

    pub fn get(&self, mesh_id: &str) -> Option<&MeshStateEntry> {
        self.state.meshes.get(mesh_id)
    }

    pub fn all(&self) -> impl Iterator<Item = (&str, &MeshStateEntry)> {
        self.state
            .meshes
            .iter()
            .map(|(mesh_id, entry)| (mesh_id.as_str(), entry))
    }

    pub fn upsert_hosted_mesh(
        &mut self,
        mesh_id: String,
        name: Option<String>,
        invite_id: Option<String>,
    ) -> Result<(), InviteError> {
        let now = now_secs();
        let entry = self
            .state
            .meshes
            .entry(mesh_id.clone())
            .or_insert_with(|| MeshStateEntry {
                mesh_id: mesh_id.clone(),
                name: name.clone(),
                role: MeshLocalRole::Host,
                status: MeshStatus::Active,
                membership_token: None,
                admitted_peers: BTreeMap::new(),
                known_peers: BTreeMap::new(),
                invite_ids: Vec::new(),
                created_at: now,
                updated_at: now,
                last_connected: None,
            });
        entry.role = entry.role.merge(MeshLocalRole::Host);
        entry.status = MeshStatus::Active;
        entry.updated_at = now;
        if entry.name.is_none() {
            entry.name = name;
        }
        if let Some(invite_id) = invite_id
            && !entry.invite_ids.contains(&invite_id)
        {
            entry.invite_ids.push(invite_id);
            entry.invite_ids.sort();
        }
        self.save()
    }

    pub fn upsert_joined_mesh(
        &mut self,
        token: MembershipToken,
        known_peers: Vec<PeerEntry>,
    ) -> Result<(), InviteError> {
        let now = now_secs();
        let mesh_id = token.mesh_id.clone();
        let entry = self
            .state
            .meshes
            .entry(mesh_id.clone())
            .or_insert_with(|| MeshStateEntry {
                mesh_id: mesh_id.clone(),
                name: None,
                role: MeshLocalRole::Member,
                status: MeshStatus::Active,
                membership_token: None,
                admitted_peers: BTreeMap::new(),
                known_peers: BTreeMap::new(),
                invite_ids: Vec::new(),
                created_at: now,
                updated_at: now,
                last_connected: Some(now),
            });
        entry.role = entry.role.merge(MeshLocalRole::Member);
        entry.status = MeshStatus::Active;
        entry.membership_token = Some(token);
        entry.last_connected = Some(now);
        entry.updated_at = now;
        for peer in known_peers {
            entry.known_peers.insert(peer.peer_id.clone(), peer);
        }
        self.save()
    }

    pub fn add_admitted_peer(
        &mut self,
        mesh_id: &str,
        token: MembershipToken,
    ) -> Result<(), InviteError> {
        let now = now_secs();
        let entry = self
            .state
            .meshes
            .entry(mesh_id.to_string())
            .or_insert_with(|| MeshStateEntry {
                mesh_id: mesh_id.to_string(),
                name: None,
                role: MeshLocalRole::Host,
                status: MeshStatus::Active,
                membership_token: None,
                admitted_peers: BTreeMap::new(),
                known_peers: BTreeMap::new(),
                invite_ids: Vec::new(),
                created_at: now,
                updated_at: now,
                last_connected: None,
            });
        entry.role = entry.role.merge(MeshLocalRole::Host);
        entry.status = MeshStatus::Active;
        entry.updated_at = now;
        entry.admitted_peers.insert(token.peer_id.clone(), token);
        self.save()
    }

    pub fn update_known_peers(
        &mut self,
        mesh_id: &str,
        peers: Vec<PeerEntry>,
    ) -> Result<(), InviteError> {
        let Some(entry) = self.state.meshes.get_mut(mesh_id) else {
            return Ok(());
        };
        entry.known_peers = peers
            .into_iter()
            .map(|peer| (peer.peer_id.clone(), peer))
            .collect();
        entry.updated_at = now_secs();
        entry.last_connected = Some(now_secs());
        self.save()
    }

    pub fn remove_known_peer(&mut self, mesh_id: &str, peer_id: &str) -> Result<bool, InviteError> {
        let Some(entry) = self.state.meshes.get_mut(mesh_id) else {
            return Ok(false);
        };
        let removed = entry.known_peers.remove(peer_id).is_some();
        if removed {
            entry.updated_at = now_secs();
            self.save()?;
        }
        Ok(removed)
    }

    pub fn known_peers_for_mesh(&self, mesh_id: &str) -> Vec<PeerEntry> {
        self.state
            .meshes
            .get(mesh_id)
            .map(|entry| entry.known_peers.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn reconnect_peers_for_mesh(&self, mesh_id: &str) -> Vec<PeerEntry> {
        self.state
            .meshes
            .get(mesh_id)
            .filter(|entry| entry.status == MeshStatus::Active)
            .map(|entry| {
                let mut peers: BTreeMap<String, PeerEntry> = entry.known_peers.clone();
                if matches!(entry.role, MeshLocalRole::Host | MeshLocalRole::Both) {
                    for (peer_id, token) in &entry.admitted_peers {
                        peers.entry(peer_id.clone()).or_insert_with(|| PeerEntry {
                            peer_id: token.peer_id.clone(),
                            addrs: vec![format!("/p2p/{}", token.peer_id)],
                        });
                    }
                }
                peers.into_values().collect()
            })
            .unwrap_or_default()
    }

    pub fn all_reconnect_peers(&self) -> Vec<(String, PeerEntry)> {
        let mut seen = BTreeSet::new();
        let mut peers = Vec::new();
        for (mesh_id, entry) in &self.state.meshes {
            if entry.status != MeshStatus::Active {
                continue;
            }
            for peer in self.reconnect_peers_for_mesh(mesh_id) {
                let key = (mesh_id.clone(), peer.peer_id.clone());
                if seen.insert(key.clone()) {
                    peers.push((key.0, peer));
                }
            }
        }
        peers
    }

    pub fn mark_left(&mut self, mesh_id: &str) -> Result<bool, InviteError> {
        let Some(entry) = self.state.meshes.get_mut(mesh_id) else {
            return Ok(false);
        };
        entry.status = MeshStatus::Left;
        entry.updated_at = now_secs();
        self.save()?;
        Ok(true)
    }

    pub fn mesh_ids_for_host(&self, host_peer_id: &str) -> Vec<String> {
        self.state
            .meshes
            .values()
            .filter(|entry| {
                entry.status == MeshStatus::Active
                    && matches!(entry.role, MeshLocalRole::Host | MeshLocalRole::Both)
                    && entry.mesh_id.starts_with(&format!("{host_peer_id}:"))
            })
            .map(|entry| entry.mesh_id.clone())
            .collect()
    }

    pub fn save(&self) -> Result<(), InviteError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                InviteError::StoreError(format!(
                    "failed to create directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let data = serde_json::to_string_pretty(&self.state)
            .map_err(|e| InviteError::StoreError(format!("failed to serialize mesh state: {e}")))?;
        std::fs::write(&self.path, data).map_err(|e| {
            InviteError::StoreError(format!("failed to write {}: {e}", self.path.display()))
        })
    }
}

pub fn default_mesh_state_path() -> Result<PathBuf, InviteError> {
    let cfg_dir = querymt_utils::providers::config_dir()
        .map_err(|e| InviteError::StoreError(format!("cannot determine config dir: {e}")))?;
    Ok(cfg_dir.join("mesh_state.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::remote::invite::{InvitePermissions, MembershipToken};

    #[test]
    fn hosted_and_joined_meshes_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let mut store = MeshStateStore::load_or_create(&path).unwrap();

        let host_kp = libp2p::identity::Keypair::generate_ed25519();
        let host_peer_id = host_kp.public().to_peer_id().to_string();
        let joiner_kp = libp2p::identity::Keypair::generate_ed25519();
        let joiner_peer_id = joiner_kp.public().to_peer_id().to_string();

        let hosted_mesh_id = format!("{host_peer_id}:anon");
        let joined_mesh_id = "peer-b:team".to_string();

        store
            .upsert_hosted_mesh(hosted_mesh_id.clone(), None, Some("invite-1".to_string()))
            .unwrap();
        let token = MembershipToken::issue(
            joined_mesh_id.clone(),
            &joiner_peer_id,
            &host_kp,
            "invite-2".to_string(),
            InvitePermissions::default(),
            0,
        )
        .unwrap();
        store.upsert_joined_mesh(token, Vec::new()).unwrap();

        let active = store.active_mesh_ids();
        assert!(active.contains(&hosted_mesh_id));
        assert!(active.contains(&joined_mesh_id));
    }
}
