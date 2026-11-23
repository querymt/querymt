use crate::NodeId;
use crate::invite::{InviteError, MembershipToken, PeerEntry};
use crate::peer_access::quota::{QuotaLedger, QuotaRuntime, UnknownUsageCharge};
use crate::peer_access::{
    GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState, PolicyChange,
    PolicyEffect,
};
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use uuid::Uuid;

const STATE_VERSION: u32 = 2;
type SharedMeshState = Arc<RwLock<MeshStateStore>>;
type MeshStateOwners = BTreeMap<PathBuf, Weak<RwLock<MeshStateStore>>>;
static OWNERS: LazyLock<Mutex<MeshStateOwners>> = LazyLock::new(Mutex::default);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshState {
    pub version: u32,
    pub meshes: BTreeMap<String, MeshStateEntry>,
    #[serde(default)]
    peer_access: PeerAccessState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PeerAccessState {
    #[serde(default)]
    grants: BTreeMap<String, BTreeMap<String, PeerGrantState>>,
    #[serde(default)]
    pub(crate) quotas: QuotaLedger,
}

impl PeerAccessState {
    pub(crate) fn grant(&self, key: &PeerGrantKey) -> PeerGrantState {
        self.grants
            .get(&key.host.as_str())
            .and_then(|peers| peers.get(&key.peer.as_str()))
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn put_grant(&mut self, key: &PeerGrantKey, state: PeerGrantState) {
        self.grants
            .entry(key.host.as_str())
            .or_default()
            .insert(key.peer.as_str(), state);
    }
}

impl Default for MeshState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            meshes: BTreeMap::new(),
            peer_access: PeerAccessState::default(),
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
    quota_runtime: QuotaRuntime,
    changes: broadcast::Sender<PolicyChange>,
    usage_changes: broadcast::Sender<PeerGrantKey>,
}

impl MeshStateStore {
    /// Open the process-wide owner shared by membership and access writers.
    pub fn shared(path: &Path) -> Result<SharedMeshState, InviteError> {
        let result = (|| {
            let parent = state_parent(path);
            std::fs::create_dir_all(parent)?;
            match path.canonicalize() {
                Ok(canonical) => Ok(canonical),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let name = path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
                    Ok(parent.canonicalize()?.join(name))
                }
                Err(e) => Err(e),
            }
        })();
        let path = result.map_err(|e| {
            InviteError::StoreError(format!("failed to resolve {}: {e}", path.display()))
        })?;
        let mut owners = OWNERS.lock();
        if let Some(owner) = owners.get(&path).and_then(Weak::upgrade) {
            return Ok(owner);
        }
        owners.retain(|_, owner| owner.strong_count() != 0);
        let owner = Arc::new(RwLock::new(Self::load_or_create(&path)?));
        owners.insert(path, Arc::downgrade(&owner));
        Ok(owner)
    }

    pub(crate) fn load_or_create(path: &Path) -> Result<Self, InviteError> {
        let (mut state, initialize) = match std::fs::read(path) {
            Ok(data) => {
                let state: MeshState = serde_json::from_slice(&data).map_err(|e| {
                    InviteError::StoreError(format!("failed to parse {}: {e}", path.display()))
                })?;
                match state.version {
                    1 => (state, true),
                    STATE_VERSION => (state, false),
                    version => {
                        return Err(InviteError::StoreError(format!(
                            "unsupported mesh state version {version}"
                        )));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (MeshState::default(), true),
            Err(e) => {
                return Err(InviteError::StoreError(format!(
                    "failed to read {}: {e}",
                    path.display()
                )));
            }
        };
        if initialize {
            state.version = STATE_VERSION;
            state.peer_access = PeerAccessState::default();
        }
        let store = Self {
            path: path.to_path_buf(),
            state,
            quota_runtime: QuotaRuntime::default(),
            changes: broadcast::channel(64).0,
            usage_changes: broadcast::channel(64).0,
        };
        if initialize {
            store.save()?;
        }
        Ok(store)
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

    fn save(&self) -> Result<(), InviteError> {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let parent = state_parent(&self.path);
            std::fs::create_dir_all(parent)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer_pretty(&mut temp, &self.state)?;
            temp.as_file().sync_all()?;
            temp.persist(&self.path)?;
            Ok(())
        })();
        result.map_err(|e| {
            InviteError::StoreError(format!("failed to save {}: {e}", self.path.display()))
        })
    }

    fn save_access(&mut self, access: PeerAccessState) -> Result<(), GrantStoreError> {
        let before = std::mem::replace(&mut self.state.peer_access, access);
        if let Err(error) = self.save() {
            self.state.peer_access = before;
            return Err(store_error(error));
        }
        Ok(())
    }
}

fn state_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn store_error(error: impl std::error::Error + Send + Sync + 'static) -> GrantStoreError {
    GrantStoreError::Backend(Box::new(error))
}

/// Policy and quota interface over the same state owner used by mesh membership.
#[derive(Clone)]
pub struct MeshAccessStore {
    owner: SharedMeshState,
    pub(crate) changes: broadcast::Sender<PolicyChange>,
    pub(crate) usage_changes: broadcast::Sender<PeerGrantKey>,
    pub(crate) unknown_usage_charge: UnknownUsageCharge,
}

impl MeshAccessStore {
    pub fn new(owner: SharedMeshState) -> Self {
        let (changes, usage_changes) = {
            let state = owner.read();
            (state.changes.clone(), state.usage_changes.clone())
        };
        Self {
            owner,
            changes,
            usage_changes,
            unknown_usage_charge: UnknownUsageCharge::default(),
        }
    }

    #[must_use]
    pub fn with_unknown_usage_charge(mut self, charge: UnknownUsageCharge) -> Self {
        self.unknown_usage_charge = charge;
        self
    }

    pub(crate) async fn transact<T, E>(
        &self,
        f: impl FnOnce(&mut PeerAccessState, &mut QuotaRuntime) -> Result<(T, bool), E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<GrantStoreError> + Send + 'static,
    {
        self.transact_access(move |access, runtime| {
            let (value, persist) = f(access, runtime)?;
            Ok((value, persist, None))
        })
        .await
    }

    pub(crate) async fn transact_access<T, E>(
        &self,
        f: impl FnOnce(
            &mut PeerAccessState,
            &mut QuotaRuntime,
        ) -> Result<(T, bool, Option<PolicyChange>), E>
        + Send
        + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<GrantStoreError> + Send + 'static,
    {
        let owner = Arc::clone(&self.owner);
        tokio::task::spawn_blocking(move || {
            let mut state = owner.write();
            let mut access = state.state.peer_access.clone();
            let mut runtime = state.quota_runtime.clone();
            let (value, persist, change) = f(&mut access, &mut runtime)?;
            if persist {
                state.save_access(access)?;
            }
            state.quota_runtime = runtime;
            if let Some(change) = change {
                let _ = state.changes.send(change);
            }
            Ok(value)
        })
        .await
        .map_err(store_error)?
    }
}

#[async_trait]
impl GrantStore for MeshAccessStore {
    async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
        Ok(self.owner.read().state.peer_access.grant(key))
    }

    async fn replace_grant(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        grant: PeerGrant,
    ) -> Result<PeerGrantState, GrantStoreError> {
        grant.validate()?;
        let owner = Arc::clone(&self.owner);
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = owner.write();
            let current = state.state.peer_access.grant(&key);
            if expected != current.revision {
                return Err(GrantStoreError::Conflict {
                    expected,
                    actual: current.revision,
                });
            }
            if grant == current.grant {
                return Ok(current);
            }
            let mut access = state.state.peer_access.clone();
            let next = PeerGrantState {
                revision: Uuid::now_v7(),
                grant,
            };
            access.put_grant(&key, next.clone());
            state.save_access(access)?;
            let _ = state.changes.send(PolicyChange {
                key,
                effect: PolicyEffect::Grant(next.grant.clone()),
            });
            Ok(next)
        })
        .await
        .map_err(store_error)?
    }

    fn subscribe_changes(&self) -> Option<broadcast::Receiver<PolicyChange>> {
        Some(self.changes.subscribe())
    }

    async fn managed_peers(&self, host: &NodeId) -> Result<Vec<NodeId>, GrantStoreError> {
        let owner = self.owner.read();
        owner
            .state
            .peer_access
            .grants
            .get(&host.as_str())
            .into_iter()
            .flat_map(BTreeMap::keys)
            .map(|peer| {
                NodeId::parse(peer).map_err(|err| {
                    store_error(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
                })
            })
            .collect()
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

    #[tokio::test]
    async fn migrates_v1_and_shares_membership_and_policy_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let old = serde_json::json!({
            "version": 1,
            "meshes": {"host:fixture": {
                "mesh_id": "host:fixture", "name": "fixture", "role": "host", "status": "active",
                "membership_token": null, "admitted_peers": {"peer": sample_token("host:fixture", "peer", "invite")},
                "known_peers": {"peer": {"peer_id": "peer", "addrs": ["/ip4/127.0.0.1/tcp/2345"]}},
                "invite_ids": ["invite"], "created_at": 10, "updated_at": 11, "last_connected": null
            }}
        });
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let owner = MeshStateStore::shared(&path).unwrap();
        let same = MeshStateStore::shared(&dir.path().join("./mesh_state.json")).unwrap();
        assert!(Arc::ptr_eq(&owner, &same));
        let upgraded: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(upgraded["version"], 2);
        assert_eq!(upgraded["meshes"], old["meshes"]);
        let key = grant_key();
        let access = MeshAccessStore::new(same);
        assert_eq!(access.grant(&key).await.unwrap(), PeerGrantState::default());
        let next = access
            .replace_grant(&key, Uuid::nil(), PeerGrant::Full)
            .await
            .unwrap();
        owner
            .write()
            .upsert_hosted_mesh("host:fixture".into(), None, Some("second".into()))
            .unwrap();
        assert!(
            matches!(access.replace_grant(&key, Uuid::nil(), PeerGrant::None).await, Err(GrantStoreError::Conflict { actual, .. }) if actual == next.revision)
        );
        assert_eq!(
            access.managed_peers(&key.host).await.unwrap(),
            vec![key.peer.clone()]
        );
        drop(access);
        drop(owner);
        let reopened = MeshStateStore::shared(&path).unwrap();
        assert_eq!(
            reopened.read().get("host:fixture").unwrap().invite_ids,
            ["invite", "second"]
        );
        assert_eq!(
            MeshAccessStore::new(reopened).grant(&key).await.unwrap(),
            next
        );
    }

    fn grant_key() -> PeerGrantKey {
        PeerGrantKey {
            host: NodeId::parse("12D3KooWJ5MP5JUR1c8pF37dNHGGrVhCGHEHwrCYyowX6dPJtV6t").unwrap(),
            peer: NodeId::parse("12D3KooWQKkiEPk7f8Sp2LXj7S2BJMAMePSz23RQPYGVBCGiyhJE").unwrap(),
        }
    }

    #[tokio::test]
    async fn failed_save_does_not_commit_or_publish_grant() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mesh_state.json");
        let owner = MeshStateStore::shared(&path).unwrap();
        let access = MeshAccessStore::new(owner);
        let mut changes = access.subscribe_changes().unwrap();
        let key = grant_key();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            access
                .replace_grant(&key, Uuid::nil(), PeerGrant::Full)
                .await,
            Err(GrantStoreError::Backend(_))
        ));
        assert_eq!(access.grant(&key).await.unwrap(), PeerGrantState::default());
        assert!(matches!(
            changes.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
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
