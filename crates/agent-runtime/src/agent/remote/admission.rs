//! Testable invite admission discovery and handshake.
//!
//! Public mesh APIs stay small (`join(invite)`), while this module owns the
//! retryable mechanics: scoped dialing, per-peer node-manager lookup, and the
//! admission request itself.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use libp2p::PeerId;

use super::invite::PeerEntry;
use super::mesh::MeshHandle;
use super::node_manager::{
    AdmissionRejection, AdmissionRequest, AdmissionResponse, RemoteNodeManager,
};
use super::scope::{MeshScopeId, scoped_node_manager_for_peer};
use kameo::error::RemoteSendError;
use querymt_remote::mesh_ingress::SharingCallError;
use querymt_remote::peer_access::SharingProtocol;
use querymt_remote::request_timeout::{SharingAskError, ask_remote_with_timeout, ask_sharing_v1};
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub(crate) struct AdmissionPolicy {
    pub max_elapsed: Duration,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl AdmissionPolicy {
    pub(crate) fn production() -> Self {
        Self {
            max_elapsed: Duration::from_secs(15),
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(2),
        }
    }

    #[cfg(test)]
    fn test_fast() -> Self {
        Self {
            max_elapsed: Duration::from_secs(5),
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(400),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmissionCandidate {
    pub peer_id: PeerId,
    pub role: AdmissionCandidateRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionCandidateRole {
    Inviter,
    Fallback,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionError {
    #[error("invalid inviter peer id '{peer_id}': {reason}")]
    InvalidInviterPeer { peer_id: String, reason: String },

    #[error("invalid fallback peer id '{peer_id}': {reason}")]
    InvalidFallbackPeer { peer_id: String, reason: String },

    #[error("admission rejected: {reason}")]
    Rejected { reason: String },

    #[error(
        "could not join mesh: inviter peer {inviter_peer_id} was not reachable within {timeout:?} (mesh_id={mesh_id}, candidates={candidate_count}, attempts={attempts}, last_error={last_error})"
    )]
    NoReachablePeer {
        inviter_peer_id: String,
        mesh_id: String,
        candidate_count: usize,
        attempts: u32,
        timeout: Duration,
        last_error: String,
    },

    #[error("admission protocol selection failed before dispatch: {0}")]
    BeforeDispatch(#[source] querymt_remote::mesh_ingress::SharingCallError),

    #[error("admission transport error: {0}")]
    Transport(String),

    #[error("failed to persist mesh admission: {0}")]
    Persistence(String),
}

impl From<AdmissionRejection> for AdmissionError {
    fn from(rejection: AdmissionRejection) -> Self {
        Self::Rejected {
            reason: rejection.to_string(),
        }
    }
}

impl AdmissionError {
    pub(crate) fn transport(error: impl fmt::Display) -> Self {
        Self::Transport(error.to_string())
    }
}

#[async_trait]
pub(crate) trait AdmissionTransport {
    type Target: Clone + Send + Sync + 'static;

    async fn dial_peer(&self, peer_id: PeerId, scope: &MeshScopeId) -> Result<(), AdmissionError>;

    async fn lookup_node_manager(
        &self,
        peer_id: PeerId,
        scope: &MeshScopeId,
    ) -> Result<Option<Self::Target>, AdmissionError>;

    async fn send_admission(
        &self,
        target: Self::Target,
        request: AdmissionRequest,
        deadline: Instant,
    ) -> Result<AdmissionResponse, AdmissionError>;
}

#[derive(Clone)]
pub(crate) struct MeshAdmissionTransport {
    mesh: MeshHandle,
}

impl MeshAdmissionTransport {
    pub(crate) fn new(mesh: MeshHandle) -> Self {
        Self { mesh }
    }
}

#[async_trait]
impl AdmissionTransport for MeshAdmissionTransport {
    type Target = kameo::actor::RemoteActorRef<RemoteNodeManager>;

    async fn dial_peer(&self, peer_id: PeerId, scope: &MeshScopeId) -> Result<(), AdmissionError> {
        self.mesh.dial_peer_for_admission(&peer_id, scope.clone());
        Ok(())
    }

    async fn lookup_node_manager(
        &self,
        peer_id: PeerId,
        scope: &MeshScopeId,
    ) -> Result<Option<Self::Target>, AdmissionError> {
        let dht_name = scoped_node_manager_for_peer(scope, &peer_id.to_string());
        self.mesh
            .lookup_actor_no_retry::<RemoteNodeManager>(dht_name)
            .await
            .map(|target| target.filter(|actor| actor.id().peer_id() == Some(&peer_id)))
            .map_err(AdmissionError::transport)
    }

    async fn send_admission(
        &self,
        target: Self::Target,
        request: AdmissionRequest,
        deadline: Instant,
    ) -> Result<AdmissionResponse, AdmissionError> {
        let cap = Duration::from_secs(10);
        let peer = target
            .id()
            .peer_id()
            .copied()
            .ok_or(AdmissionError::BeforeDispatch(
                SharingCallError::Unavailable,
            ))?;
        let remaining = || deadline.saturating_duration_since(Instant::now()).min(cap);
        let protocol = self
            .mesh
            .select_sharing_protocol(peer, remaining())
            .await
            .map_err(AdmissionError::BeforeDispatch)?;
        let timeout = remaining();
        if timeout.is_zero() {
            return Err(AdmissionError::BeforeDispatch(SharingCallError::Transport(
                libp2p::request_response::OutboundFailure::Timeout,
            )));
        }
        // A timeout after selection may be an unanswered, committed admission.
        tokio::time::timeout_at(deadline, async {
            match protocol {
                SharingProtocol::V1 => ask_sharing_v1(&self.mesh, &target, &request, timeout).await,
                SharingProtocol::Legacy => {
                    Ok(ask_remote_with_timeout(&target, &request, timeout).await?)
                }
            }
        })
        .await
        .map_err(AdmissionError::transport)?
        .map_err(|error| match error {
            SharingAskError::Actor(RemoteSendError::HandlerError(rejection)) => {
                AdmissionError::from(rejection)
            }
            other => AdmissionError::transport(other),
        })
    }
}

pub(crate) struct AdmissionService<T> {
    transport: T,
    policy: AdmissionPolicy,
}

impl<T> AdmissionService<T> {
    pub(crate) fn new(transport: T, policy: AdmissionPolicy) -> Self {
        Self { transport, policy }
    }
}

impl<T> AdmissionService<T>
where
    T: AdmissionTransport + Send + Sync,
{
    pub(crate) async fn admit(
        &self,
        mesh_id: &str,
        scope: MeshScopeId,
        inviter_peer_id: &str,
        fallback_peers: &[PeerEntry],
        request: AdmissionRequest,
    ) -> Result<AdmissionResponse, AdmissionError> {
        let candidates = admission_candidates(inviter_peer_id, fallback_peers)?;
        let deadline = Instant::now() + self.policy.max_elapsed;
        let mut attempts = 0u32;
        let mut delay = self.policy.initial_delay;
        let mut last_error = String::from("no lookup attempts completed");

        'retry: while Instant::now() < deadline {
            attempts = attempts.saturating_add(1);

            for candidate in &candidates {
                if let Err(error) = self.transport.dial_peer(candidate.peer_id, &scope).await {
                    last_error = error.to_string();
                }
            }

            for candidate in &candidates {
                if Instant::now() >= deadline {
                    break 'retry;
                }
                let lookup = tokio::time::timeout_at(
                    deadline,
                    self.transport
                        .lookup_node_manager(candidate.peer_id, &scope),
                )
                .await;
                match lookup {
                    Err(error) => {
                        last_error = error.to_string();
                        break 'retry;
                    }
                    Ok(Ok(Some(target))) => {
                        if Instant::now() >= deadline {
                            break 'retry;
                        }
                        let response = match self
                            .transport
                            .send_admission(target, request.clone(), deadline)
                            .await
                        {
                            Err(error @ AdmissionError::BeforeDispatch(_)) => {
                                last_error = error.to_string();
                                continue;
                            }
                            result => result?,
                        };
                        if let AdmissionResponse::Rejected { reason } = response {
                            return Err(AdmissionError::Rejected { reason });
                        }
                        return Ok(response);
                    }
                    Ok(Ok(None)) => {
                        last_error = format!(
                            "no node manager for {} in scope {}",
                            candidate.peer_id, scope
                        );
                    }
                    Ok(Err(error)) => {
                        last_error = error.to_string();
                    }
                }
            }

            tokio::time::sleep_until(deadline.min(Instant::now() + delay)).await;
            delay = delay.saturating_mul(2).min(self.policy.max_delay);
        }
        Err(AdmissionError::NoReachablePeer {
            inviter_peer_id: inviter_peer_id.to_string(),
            mesh_id: mesh_id.to_string(),
            candidate_count: candidates.len(),
            attempts,
            timeout: self.policy.max_elapsed,
            last_error,
        })
    }
}

/// Persist admission on the runtime-owned store without reloading or replacing its path.
pub(crate) async fn persist_admission(
    store: std::sync::Arc<parking_lot::RwLock<querymt_remote::MeshStateStore>>,
    mesh_id: String,
    token: Option<querymt_remote::MembershipToken>,
    peers: Vec<PeerEntry>,
) -> Result<(), AdmissionError> {
    tokio::task::spawn_blocking(move || {
        let mut store = store.write();
        if let Some(token) = token {
            store.upsert_joined_mesh(token, peers)
        } else {
            let mut merged: std::collections::HashMap<_, _> = store
                .known_peers_for_mesh(&mesh_id)
                .into_iter()
                .map(|peer| (peer.peer_id.clone(), peer))
                .collect();
            merged.extend(peers.into_iter().map(|peer| (peer.peer_id.clone(), peer)));
            store.update_known_peers(&mesh_id, merged.into_values().collect())
        }
        .map_err(|error| AdmissionError::Persistence(error.to_string()))
    })
    .await
    .map_err(|error| AdmissionError::Persistence(error.to_string()))?
}

pub(crate) fn admission_candidates(
    inviter_peer_id: &str,
    fallback_peers: &[PeerEntry],
) -> Result<Vec<AdmissionCandidate>, AdmissionError> {
    let inviter =
        inviter_peer_id
            .parse::<PeerId>()
            .map_err(|e| AdmissionError::InvalidInviterPeer {
                peer_id: inviter_peer_id.to_string(),
                reason: e.to_string(),
            })?;

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    seen.insert(inviter);
    out.push(AdmissionCandidate {
        peer_id: inviter,
        role: AdmissionCandidateRole::Inviter,
    });

    for peer in fallback_peers {
        let peer_id =
            peer.peer_id
                .parse::<PeerId>()
                .map_err(|e| AdmissionError::InvalidFallbackPeer {
                    peer_id: peer.peer_id.clone(),
                    reason: e.to_string(),
                })?;
        if seen.insert(peer_id) {
            out.push(AdmissionCandidate {
                peer_id,
                role: AdmissionCandidateRole::Fallback,
            });
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {

    #[test]
    fn rejection_round_trips_over_msgpack_and_maps_to_rejected() {
        for rejection in [
            AdmissionRejection::NoMeshHandle,
            AdmissionRejection::NotAMember,
            AdmissionRejection::Denied("invite expired".into()),
        ] {
            let bytes = rmp_serde::to_vec(&rejection).unwrap();
            let back: AdmissionRejection = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(back, rejection);
            let error = AdmissionError::from(back);
            assert!(
                matches!(&error, AdmissionError::Rejected { reason } if *reason == rejection.to_string())
            );
        }
    }
    use super::*;
    use parking_lot::Mutex;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct FakeTarget(PeerId);

    #[derive(Clone, Default)]
    struct FakeAdmissionTransport {
        inner: Arc<Mutex<FakeInner>>,
    }

    #[derive(Default)]
    struct FakeInner {
        lookup_after_attempt: HashMap<PeerId, u32>,
        lookup_attempts: HashMap<PeerId, u32>,
        responses: HashMap<PeerId, AdmissionResponse>,
        unavailable_before_dispatch: HashSet<PeerId>,
        probe_delay: Duration,
        uncertain_after_dispatch: HashSet<PeerId>,
        dials: Vec<(PeerId, MeshScopeId)>,
        lookups: Vec<(PeerId, MeshScopeId)>,
        admissions: VecDeque<PeerId>,
    }

    impl FakeAdmissionTransport {
        fn make_visible_after(self, peer_id: PeerId, attempts: u32) -> Self {
            self.inner
                .lock()
                .lookup_after_attempt
                .insert(peer_id, attempts);
            self
        }

        fn with_response(self, peer_id: PeerId, response: AdmissionResponse) -> Self {
            self.inner.lock().responses.insert(peer_id, response);
            self
        }

        fn dials(&self) -> Vec<(PeerId, MeshScopeId)> {
            self.inner.lock().dials.clone()
        }

        fn admissions(&self) -> Vec<PeerId> {
            self.inner.lock().admissions.iter().copied().collect()
        }
    }

    #[async_trait]
    impl AdmissionTransport for FakeAdmissionTransport {
        type Target = FakeTarget;

        async fn dial_peer(
            &self,
            peer_id: PeerId,
            scope: &MeshScopeId,
        ) -> Result<(), AdmissionError> {
            self.inner.lock().dials.push((peer_id, scope.clone()));
            Ok(())
        }

        async fn lookup_node_manager(
            &self,
            peer_id: PeerId,
            scope: &MeshScopeId,
        ) -> Result<Option<Self::Target>, AdmissionError> {
            let mut inner = self.inner.lock();
            inner.lookups.push((peer_id, scope.clone()));
            let attempts = {
                let entry = inner.lookup_attempts.entry(peer_id).or_insert(0);
                *entry = entry.saturating_add(1);
                *entry
            };
            let visible_after = inner.lookup_after_attempt.get(&peer_id).copied();
            Ok(visible_after
                .filter(|threshold| attempts >= *threshold)
                .map(|_| FakeTarget(peer_id)))
        }

        async fn send_admission(
            &self,
            target: Self::Target,
            _request: AdmissionRequest,
            deadline: Instant,
        ) -> Result<AdmissionResponse, AdmissionError> {
            let delay = self.inner.lock().probe_delay;
            if !delay.is_zero() {
                tokio::time::sleep_until(deadline.min(Instant::now() + delay)).await;
            }
            let mut inner = self.inner.lock();
            if inner.unavailable_before_dispatch.contains(&target.0) {
                return Err(AdmissionError::BeforeDispatch(
                    querymt_remote::mesh_ingress::SharingCallError::Unavailable,
                ));
            }
            inner.admissions.push_back(target.0);
            if inner.uncertain_after_dispatch.contains(&target.0) {
                return Err(AdmissionError::transport("reply lost"));
            }
            Ok(inner
                .responses
                .get(&target.0)
                .cloned()
                .unwrap_or(AdmissionResponse::Readmitted {
                    existing_peers: Vec::new(),
                }))
        }
    }

    fn peer() -> PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    fn request(peer_id: PeerId) -> AdmissionRequest {
        AdmissionRequest::Invite {
            invite_id: "invite".to_string(),
            mesh_name: Some("mesh".to_string()),
            peer_id: peer_id.to_string(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn admission_retries_until_inviter_node_manager_appears() {
        let inviter = peer();
        let joiner = peer();
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };
        let transport = FakeAdmissionTransport::default().make_visible_after(inviter, 3);
        let service = AdmissionService::new(transport.clone(), AdmissionPolicy::test_fast());

        let result = service
            .admit(
                "mesh-a",
                scope.clone(),
                &inviter.to_string(),
                &[],
                request(joiner),
            )
            .await
            .expect("admission should eventually succeed");

        assert!(matches!(result, AdmissionResponse::Readmitted { .. }));
        assert_eq!(transport.admissions(), vec![inviter]);
        assert!(
            transport
                .dials()
                .iter()
                .filter(|(pid, dial_scope)| *pid == inviter && dial_scope == &scope)
                .count()
                >= 3
        );
    }

    #[tokio::test(start_paused = true)]
    async fn admission_falls_back_to_cached_peer_when_inviter_missing() {
        let inviter = peer();
        let fallback = peer();
        let joiner = peer();
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };
        let fallback_entry = PeerEntry {
            peer_id: fallback.to_string(),
            addrs: vec![format!("/p2p/{fallback}")],
        };
        let transport = FakeAdmissionTransport::default().make_visible_after(fallback, 1);
        let service = AdmissionService::new(transport.clone(), AdmissionPolicy::test_fast());

        service
            .admit(
                "mesh-a",
                scope,
                &inviter.to_string(),
                &[fallback_entry],
                request(joiner),
            )
            .await
            .expect("fallback admission should succeed");

        assert_eq!(transport.admissions(), vec![fallback]);
    }

    #[tokio::test(start_paused = true)]
    async fn admission_only_tries_another_owner_before_dispatch() {
        let inviter = peer();
        let fallback = peer();
        for probe_failed in [true, false] {
            let scope = MeshScopeId::Iroh {
                mesh_id: "mesh-a".into(),
            };
            let transport = FakeAdmissionTransport::default()
                .make_visible_after(inviter, 1)
                .make_visible_after(fallback, 1);
            if probe_failed {
                transport
                    .inner
                    .lock()
                    .unavailable_before_dispatch
                    .insert(inviter);
            } else {
                transport
                    .inner
                    .lock()
                    .uncertain_after_dispatch
                    .insert(inviter);
            }
            let service = AdmissionService::new(transport.clone(), AdmissionPolicy::test_fast());
            let result = service
                .admit(
                    "mesh-a",
                    scope,
                    &inviter.to_string(),
                    &[PeerEntry {
                        peer_id: fallback.to_string(),
                        addrs: vec![],
                    }],
                    request(peer()),
                )
                .await;
            if probe_failed {
                assert!(matches!(result, Ok(AdmissionResponse::Readmitted { .. })));
                assert_eq!(transport.admissions(), vec![fallback]);
            } else {
                assert!(matches!(result, Err(AdmissionError::Transport(_))));
                assert_eq!(transport.admissions(), vec![inviter]);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn admission_probe_stops_at_budget_without_trying_another_candidate() {
        let inviter = peer();
        let fallback = peer();
        let transport = FakeAdmissionTransport::default()
            .make_visible_after(inviter, 1)
            .make_visible_after(fallback, 1);
        let policy = AdmissionPolicy::test_fast();
        let budget = policy.max_elapsed;
        {
            let mut inner = transport.inner.lock();
            inner.unavailable_before_dispatch.insert(inviter);
            inner.probe_delay = budget * 2;
        }
        let service = AdmissionService::new(transport.clone(), policy);
        let started = Instant::now();
        let result = service
            .admit(
                "mesh-a",
                MeshScopeId::Iroh {
                    mesh_id: "mesh-a".into(),
                },
                &inviter.to_string(),
                &[PeerEntry {
                    peer_id: fallback.to_string(),
                    addrs: vec![],
                }],
                request(peer()),
            )
            .await;
        assert!(matches!(
            result,
            Err(AdmissionError::NoReachablePeer { .. })
        ));
        assert_eq!(started.elapsed(), budget);
        assert_eq!(transport.inner.lock().lookups.len(), 1);
        assert!(transport.admissions().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn admission_returns_rejected_without_waiting_for_timeout() {
        let inviter = peer();
        let joiner = peer();
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };
        let transport = FakeAdmissionTransport::default()
            .make_visible_after(inviter, 1)
            .with_response(
                inviter,
                AdmissionResponse::Rejected {
                    reason: "nope".to_string(),
                },
            );
        let service = AdmissionService::new(transport, AdmissionPolicy::test_fast());

        let error = service
            .admit("mesh-a", scope, &inviter.to_string(), &[], request(joiner))
            .await
            .expect_err("rejection should be returned as an error");

        assert!(matches!(error, AdmissionError::Rejected { reason } if reason == "nope"));
    }

    #[tokio::test]
    async fn admission_timeout_error_contains_debug_context() {
        let inviter = peer();
        let joiner = peer();
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };
        let service = AdmissionService::new(
            FakeAdmissionTransport::default(),
            AdmissionPolicy {
                max_elapsed: Duration::from_millis(300),
                initial_delay: Duration::from_millis(100),
                max_delay: Duration::from_millis(100),
            },
        );

        let error = service
            .admit("mesh-a", scope, &inviter.to_string(), &[], request(joiner))
            .await
            .expect_err("unreachable inviter should time out");
        let text = error.to_string();

        assert!(text.contains(&inviter.to_string()));
        assert!(text.contains("mesh-a"));
        assert!(text.contains("attempts="));
    }

    #[tokio::test]
    async fn admission_and_readmission_keep_identity_selected_store_on_restart() {
        use querymt_remote::{InvitePermissions, MembershipToken, MeshStateStore};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.key.mesh_state.json");
        let unrelated_path = dir.path().join("mesh_state.json");
        let unrelated = MeshStateStore::shared(&unrelated_path).unwrap();
        unrelated
            .write()
            .upsert_hosted_mesh("unrelated".into(), None, None)
            .unwrap();
        let original = std::fs::read(&unrelated_path).unwrap();
        let store = MeshStateStore::shared(&path).unwrap();
        let same_store = store.clone();
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let subject = peer();
        let token = MembershipToken::issue(
            "joined".into(),
            &subject.to_string(),
            &keypair,
            "invite".into(),
            InvitePermissions::default(),
            0,
        )
        .unwrap();
        let a = peer().to_string();
        let b = peer().to_string();
        persist_admission(
            store.clone(),
            "joined".into(),
            Some(token.clone()),
            vec![PeerEntry {
                peer_id: a.clone(),
                addrs: vec![],
            }],
        )
        .await
        .unwrap();
        assert!(same_store.read().get("joined").is_some());
        persist_admission(
            store,
            "joined".into(),
            None,
            vec![PeerEntry {
                peer_id: b.clone(),
                addrs: vec![],
            }],
        )
        .await
        .unwrap();
        drop(same_store);
        let reloaded = MeshStateStore::shared(&path).unwrap();
        let reloaded = reloaded.read();
        assert_eq!(
            reloaded.get("joined").unwrap().membership_token,
            Some(token)
        );
        let saved: HashSet<_> = reloaded
            .known_peers_for_mesh("joined")
            .into_iter()
            .map(|peer| peer.peer_id)
            .collect();
        assert_eq!(saved, HashSet::from([a, b]));
        assert!(reloaded.get("unrelated").is_none());
        assert_eq!(std::fs::read(&unrelated_path).unwrap(), original);
    }

    #[test]
    fn admission_candidates_dedupe_inviter_and_preserve_order() {
        let inviter = peer();
        let fallback = peer();
        let peers = vec![
            PeerEntry {
                peer_id: inviter.to_string(),
                addrs: Vec::new(),
            },
            PeerEntry {
                peer_id: fallback.to_string(),
                addrs: Vec::new(),
            },
        ];

        let candidates = admission_candidates(&inviter.to_string(), &peers).unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].peer_id, inviter);
        assert_eq!(candidates[0].role, AdmissionCandidateRole::Inviter);
        assert_eq!(candidates[1].peer_id, fallback);
        assert_eq!(candidates[1].role, AdmissionCandidateRole::Fallback);
    }
}
