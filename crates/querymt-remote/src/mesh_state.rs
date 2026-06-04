use crate::invite::{InviteError, MembershipToken, PeerEntry};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    use tempfile::tempdir;

    fn sample_peer(peer_id: &str, addr: &str) -> PeerEntry {
        PeerEntry {
            peer_id: peer_id.to_string(),
            addrs: vec![addr.to_string()],
        }
    }

    fn sample_token(mesh_id: &str, peer_id: &str, invite_id: &str) -> MembershipToken {
        MembershipToken {
            version: 1,
            mesh_id: mesh_id.to_string(),
            peer_id: peer_id.to_string(),
            admitted_by: "host-peer".to_string(),
            invite_id: invite_id.to_string(),
            permissions: crate::invite::InvitePermissions::default(),
            issued_at: 1,
            expires_at: 0,
            signature: "sig".to_string(),
        }
    }

    #[test]
    fn role_merge_escalates_host_and_member_to_both() {
        assert_eq!(
            MeshLocalRole::Host.merge(MeshLocalRole::Member),
            MeshLocalRole::Both
        );
        assert_eq!(
            MeshLocalRole::Member.merge(MeshLocalRole::Host),
            MeshLocalRole::Both
        );
        assert_eq!(
            MeshLocalRole::Both.merge(MeshLocalRole::Host),
            MeshLocalRole::Both
        );
    }

    #[test]
    fn hosted_and_joined_mesh_merge_into_both_role_and_persist() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let mesh_id = "host-peer:team-a";

        let mut store = MeshStateStore::load_or_create(&path).unwrap();
        store
            .upsert_hosted_mesh(
                mesh_id.to_string(),
                Some("Team A".to_string()),
                Some("invite-1".to_string()),
            )
            .unwrap();
        store
            .upsert_joined_mesh(
                sample_token(mesh_id, "member-1", "invite-1"),
                vec![sample_peer("member-1", "/p2p/member-1")],
            )
            .unwrap();

        let reloaded = MeshStateStore::load_or_create(&path).unwrap();
        let entry = reloaded.get(mesh_id).unwrap();
        assert_eq!(entry.role, MeshLocalRole::Both);
        assert_eq!(entry.status, MeshStatus::Active);
        assert_eq!(entry.name.as_deref(), Some("Team A"));
        assert_eq!(entry.invite_ids, vec!["invite-1".to_string()]);
        assert_eq!(entry.membership_token.as_ref().unwrap().peer_id, "member-1");
        assert!(entry.known_peers.contains_key("member-1"));
    }

    #[test]
    fn hosted_mesh_deduplicates_and_sorts_invite_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let mesh_id = "host-peer:team-a";

        let mut store = MeshStateStore::load_or_create(&path).unwrap();
        store
            .upsert_hosted_mesh(mesh_id.to_string(), None, Some("invite-b".to_string()))
            .unwrap();
        store
            .upsert_hosted_mesh(mesh_id.to_string(), None, Some("invite-a".to_string()))
            .unwrap();
        store
            .upsert_hosted_mesh(mesh_id.to_string(), None, Some("invite-b".to_string()))
            .unwrap();

        let entry = store.get(mesh_id).unwrap();
        assert_eq!(
            entry.invite_ids,
            vec!["invite-a".to_string(), "invite-b".to_string()]
        );
    }

    #[test]
    fn update_and_remove_known_peers_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let mesh_id = "host-peer:team-a";

        let mut store = MeshStateStore::load_or_create(&path).unwrap();
        store
            .upsert_hosted_mesh(mesh_id.to_string(), None, None)
            .unwrap();
        store
            .update_known_peers(
                mesh_id,
                vec![
                    sample_peer("peer-a", "/p2p/peer-a"),
                    sample_peer("peer-b", "/p2p/peer-b"),
                ],
            )
            .unwrap();

        let peers = store.known_peers_for_mesh(mesh_id);
        assert_eq!(peers.len(), 2);
        assert!(store.remove_known_peer(mesh_id, "peer-a").unwrap());
        assert!(!store.remove_known_peer(mesh_id, "missing").unwrap());

        let remaining = store.known_peers_for_mesh(mesh_id);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].peer_id, "peer-b");
    }

    #[test]
    fn reconnect_peers_include_known_and_admitted_without_duplicates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let mesh_id = "host-peer:team-a";

        let mut store = MeshStateStore::load_or_create(&path).unwrap();
        store
            .upsert_hosted_mesh(mesh_id.to_string(), None, None)
            .unwrap();
        store
            .update_known_peers(
                mesh_id,
                vec![sample_peer("peer-a", "/ip4/127.0.0.1/tcp/1/p2p/peer-a")],
            )
            .unwrap();
        store
            .add_admitted_peer(mesh_id, sample_token(mesh_id, "peer-a", "invite-a"))
            .unwrap();
        store
            .add_admitted_peer(mesh_id, sample_token(mesh_id, "peer-b", "invite-b"))
            .unwrap();

        let peers = store.reconnect_peers_for_mesh(mesh_id);
        assert_eq!(peers.len(), 2);
        assert!(peers.iter().any(|peer| peer.peer_id == "peer-a"
            && peer.addrs == vec!["/ip4/127.0.0.1/tcp/1/p2p/peer-a".to_string()]));
        assert!(
            peers
                .iter()
                .any(|peer| peer.peer_id == "peer-b"
                    && peer.addrs == vec!["/p2p/peer-b".to_string()])
        );
    }

    #[test]
    fn inactive_meshes_are_excluded_from_active_and_reconnect_views() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let mesh_id = "host-peer:team-a";

        let mut store = MeshStateStore::load_or_create(&path).unwrap();
        store
            .upsert_hosted_mesh(mesh_id.to_string(), None, None)
            .unwrap();
        store
            .add_admitted_peer(mesh_id, sample_token(mesh_id, "peer-a", "invite-a"))
            .unwrap();
        assert_eq!(store.active_mesh_ids(), vec![mesh_id.to_string()]);

        assert!(store.mark_left(mesh_id).unwrap());
        assert!(store.active_mesh_ids().is_empty());
        assert!(store.reconnect_peers_for_mesh(mesh_id).is_empty());
        assert!(store.all_reconnect_peers().is_empty());
    }

    #[test]
    fn all_reconnect_peers_returns_entries_per_active_mesh() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let mesh_a = "host-peer:team-a";
        let mesh_b = "host-peer:team-b";

        let mut store = MeshStateStore::load_or_create(&path).unwrap();
        store
            .upsert_hosted_mesh(mesh_a.to_string(), None, None)
            .unwrap();
        store
            .upsert_hosted_mesh(mesh_b.to_string(), None, None)
            .unwrap();
        store
            .add_admitted_peer(mesh_a, sample_token(mesh_a, "peer-a", "invite-a"))
            .unwrap();
        store
            .add_admitted_peer(mesh_b, sample_token(mesh_b, "peer-a", "invite-b"))
            .unwrap();

        let peers = store.all_reconnect_peers();
        assert_eq!(peers.len(), 2);
        assert!(
            peers
                .iter()
                .any(|(mesh_id, peer)| mesh_id == mesh_a && peer.peer_id == "peer-a")
        );
        assert!(
            peers
                .iter()
                .any(|(mesh_id, peer)| mesh_id == mesh_b && peer.peer_id == "peer-a")
        );
    }

    #[test]
    fn mesh_ids_for_host_only_returns_active_hosted_meshes_for_prefix() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");

        let mut store = MeshStateStore::load_or_create(&path).unwrap();
        store
            .upsert_hosted_mesh("host-a:team-a".to_string(), None, None)
            .unwrap();
        store
            .upsert_hosted_mesh("host-a:team-b".to_string(), None, None)
            .unwrap();
        store
            .upsert_hosted_mesh("host-b:team-c".to_string(), None, None)
            .unwrap();
        store.mark_left("host-a:team-b").unwrap();

        assert_eq!(
            store.mesh_ids_for_host("host-a"),
            vec!["host-a:team-a".to_string()]
        );
        assert_eq!(
            store.mesh_ids_for_host("host-b"),
            vec!["host-b:team-c".to_string()]
        );
    }
}
