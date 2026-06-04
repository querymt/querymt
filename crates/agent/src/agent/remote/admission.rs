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
use super::node_manager::{AdmissionRequest, AdmissionResponse, RemoteNodeManager};
use super::scope::{MeshScopeId, scoped_node_manager_for_peer};

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

    #[error("admission transport error: {0}")]
    Transport(String),
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
            .map_err(AdmissionError::transport)
    }

    async fn send_admission(
        &self,
        target: Self::Target,
        request: AdmissionRequest,
    ) -> Result<AdmissionResponse, AdmissionError> {
        target
            .ask::<AdmissionRequest>(&request)
            .await
            .map_err(AdmissionError::transport)
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
        let started = tokio::time::Instant::now();
        let mut attempts = 0u32;
        let mut delay = self.policy.initial_delay;
        let mut last_error = String::from("no lookup attempts completed");

        loop {
            attempts = attempts.saturating_add(1);

            for candidate in &candidates {
                if let Err(error) = self.transport.dial_peer(candidate.peer_id, &scope).await {
                    last_error = error.to_string();
                }
            }

            for candidate in &candidates {
                match self
                    .transport
                    .lookup_node_manager(candidate.peer_id, &scope)
                    .await
                {
                    Ok(Some(target)) => {
                        let response = self
                            .transport
                            .send_admission(target, request.clone())
                            .await?;
                        if let AdmissionResponse::Rejected { reason } = response {
                            return Err(AdmissionError::Rejected { reason });
                        }
                        return Ok(response);
                    }
                    Ok(None) => {
                        last_error = format!(
                            "no node manager for {} in scope {}",
                            candidate.peer_id, scope
                        );
                    }
                    Err(error) => {
                        last_error = error.to_string();
                    }
                }
            }

            let elapsed = started.elapsed();
            if elapsed >= self.policy.max_elapsed {
                return Err(AdmissionError::NoReachablePeer {
                    inviter_peer_id: inviter_peer_id.to_string(),
                    mesh_id: mesh_id.to_string(),
                    candidate_count: candidates.len(),
                    attempts,
                    timeout: self.policy.max_elapsed,
                    last_error,
                });
            }

            tokio::time::sleep(delay).await;
            delay = std::cmp::min(delay.saturating_mul(2), self.policy.max_delay);
        }
    }
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
        ) -> Result<AdmissionResponse, AdmissionError> {
            let mut inner = self.inner.lock();
            inner.admissions.push_back(target.0);
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
