use agent_client_protocol::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;
use typeshare::typeshare;

pub const ELICITATION_RECOVERY_VERSION: u32 = 1;
pub const ELICITATION_RECOVERY_AUTHORITY_NOTIFICATION: &str =
    "querymt/elicitation/recoveryAuthority";
pub const ELICITATION_RECOVERY_LIST_PENDING_METHOD: &str =
    "querymt/elicitation/listPendingSessions";
pub const ELICITATION_RECOVERY_ATTACH_METHOD: &str = "querymt/elicitation/attachSession";
pub const ELICITATION_RECOVERY_VALIDATION_FAILED_NOTIFICATION: &str =
    "querymt/elicitation/validationFailed";
pub const ELICITATION_RECOVERY_COMPLETED_NOTIFICATION: &str = "querymt/elicitation/completed";
pub const ELICITATION_RECOVERY_DENIED_CODE: i32 = -32003;

#[derive(Clone, Default)]
pub struct ElicitationRecoveryRegistry {
    inner: Arc<Mutex<ElicitationRecoveryRegistryState>>,
}

#[derive(Default)]
struct ElicitationRecoveryRegistryState {
    connections: HashMap<String, RecoveryConnection>,
    sessions: HashMap<String, SessionAuthority>,
}

struct RecoveryConnection {
    secure_transport: bool,
    supports_form_elicitation: bool,
}

struct SessionAuthority {
    verifier: [u8; 32],
    authorized_connections: HashSet<String>,
}

impl ElicitationRecoveryRegistry {
    pub async fn register_connection(&self, connection_id: String, secure_transport: bool) {
        self.inner.lock().await.connections.insert(
            connection_id,
            RecoveryConnection {
                secure_transport,
                supports_form_elicitation: false,
            },
        );
    }

    pub async fn remove_connection(&self, connection_id: &str) {
        self.inner.lock().await.connections.remove(connection_id);
    }

    pub async fn record_client_capabilities(
        &self,
        connection_id: &str,
        supports_form_elicitation: bool,
    ) {
        if let Some(connection) = self.inner.lock().await.connections.get_mut(connection_id) {
            connection.supports_form_elicitation = supports_form_elicitation;
        }
    }

    pub async fn issue_for_session(
        &self,
        connection_id: &str,
        session_id: &str,
        agent: &crate::agent::LocalAgentHandle,
    ) -> Option<String> {
        let mut state = self.inner.lock().await;
        let connection = state.connections.get(connection_id)?;
        if !connection.secure_transport || !connection.supports_form_elicitation {
            return None;
        }

        let (verifier, secret) = match state.sessions.get(session_id) {
            Some(authority) => {
                if !authority.authorized_connections.contains(connection_id) {
                    return None;
                }
                (authority.verifier, None)
            }
            None => {
                let secret = hex::encode(rand::random::<[u8; 32]>());
                let verifier = authority_verifier(&secret);
                state.sessions.insert(
                    session_id.to_string(),
                    SessionAuthority {
                        verifier,
                        authorized_connections: HashSet::from([connection_id.to_string()]),
                    },
                );
                (verifier, Some(secret))
            }
        };
        drop(state);

        crate::elicitation::assign_pending_elicitation_authority(
            agent,
            session_id,
            &hex::encode(verifier),
        )
        .await
        .then_some(secret)
        .flatten()
    }

    pub async fn claim_live_delivery(
        &self,
        connection_id: &str,
        session_id: &str,
        elicitation_id: &str,
        agent: &crate::agent::LocalAgentHandle,
    ) -> Option<crate::elicitation::ClaimedElicitationDelivery> {
        let authority = {
            let state = self.inner.lock().await;
            state.sessions.get(session_id).map(|authority| {
                (
                    hex::encode(authority.verifier),
                    authority.authorized_connections.contains(connection_id),
                )
            })
        };
        match authority {
            Some((_, false)) => None,
            Some((verifier, true)) => {
                crate::elicitation::assign_pending_elicitation_authority(
                    agent, session_id, &verifier,
                )
                .await;
                crate::elicitation::claim_pending_elicitation_deliveries(
                    agent,
                    session_id,
                    Some(elicitation_id),
                    &verifier,
                    connection_id,
                )
                .await
                .into_iter()
                .next()
            }
            None => {
                crate::elicitation::claim_live_elicitation_delivery(
                    agent,
                    session_id,
                    elicitation_id,
                    connection_id,
                )
                .await
            }
        }
    }

    pub async fn list_pending_sessions(
        &self,
        connection_id: &str,
        request: &ListPendingElicitationSessionsRequest,
        agent: &crate::agent::LocalAgentHandle,
    ) -> Result<ListPendingElicitationSessionsResponse, Error> {
        self.verify_connection(connection_id, request.version)
            .await?;
        let verifier = authority_verifier(&request.resume_authority);
        let verifier_hex = hex::encode(verifier);
        let session_ids =
            crate::elicitation::pending_sessions_for_authority(agent, &verifier_hex).await;
        if session_ids.is_empty() {
            self.prune_authority(&verifier).await;
            return Err(elicitation_recovery_denied(
                ElicitationRecoveryDenialReason::AuthorityExpired,
            ));
        }
        Ok(ListPendingElicitationSessionsResponse {
            version: ELICITATION_RECOVERY_VERSION,
            session_ids,
        })
    }

    pub async fn authorize_attach(
        &self,
        connection_id: &str,
        request: &AttachPendingElicitationSessionRequest,
        agent: &crate::agent::LocalAgentHandle,
    ) -> Result<AttachPendingElicitationSessionResponse, Error> {
        self.verify_connection(connection_id, request.version)
            .await?;
        let verifier = authority_verifier(&request.resume_authority);
        let verifier_hex = hex::encode(verifier);
        let session_is_authorized = {
            let state = self.inner.lock().await;
            state
                .sessions
                .get(&request.session_id)
                .is_some_and(|authority| constant_time_eq(&authority.verifier, &verifier))
        };
        if !session_is_authorized {
            return Err(elicitation_recovery_denied(
                ElicitationRecoveryDenialReason::Unauthorized,
            ));
        }
        if let Some(authority) = self
            .inner
            .lock()
            .await
            .sessions
            .get_mut(&request.session_id)
        {
            authority
                .authorized_connections
                .insert(connection_id.to_string());
        }

        let mut elicitation_ids = crate::elicitation::claim_pending_elicitation_deliveries(
            agent,
            &request.session_id,
            None,
            &verifier_hex,
            connection_id,
        )
        .await
        .into_iter()
        .map(|claim| claim.elicitation_id)
        .collect::<Vec<_>>();
        elicitation_ids.sort();
        if elicitation_ids.is_empty() {
            self.inner.lock().await.sessions.remove(&request.session_id);
            return Err(elicitation_recovery_denied(
                ElicitationRecoveryDenialReason::AuthorityExpired,
            ));
        }
        Ok(AttachPendingElicitationSessionResponse {
            version: ELICITATION_RECOVERY_VERSION,
            session_id: request.session_id.clone(),
            elicitation_ids,
        })
    }

    async fn verify_connection(&self, connection_id: &str, version: u32) -> Result<(), Error> {
        if version != ELICITATION_RECOVERY_VERSION {
            return Err(elicitation_recovery_denied(
                ElicitationRecoveryDenialReason::CapabilityMismatch,
            ));
        }
        let state = self.inner.lock().await;
        let Some(connection) = state.connections.get(connection_id) else {
            return Err(elicitation_recovery_denied(
                ElicitationRecoveryDenialReason::Unauthorized,
            ));
        };
        if !connection.secure_transport {
            return Err(elicitation_recovery_denied(
                ElicitationRecoveryDenialReason::InsecureTransport,
            ));
        }
        if !connection.supports_form_elicitation {
            return Err(elicitation_recovery_denied(
                ElicitationRecoveryDenialReason::CapabilityMismatch,
            ));
        }
        Ok(())
    }

    async fn prune_authority(&self, verifier: &[u8; 32]) {
        self.inner
            .lock()
            .await
            .sessions
            .retain(|_, authority| !constant_time_eq(&authority.verifier, verifier));
    }
}

fn authority_verifier(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

/// Advertised only when the agent has the complete recovery implementation enabled.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElicitationRecoveryCapability {
    pub version: u32,
    pub authority_notification: String,
    pub list_pending_method: String,
    pub attach_method: String,
}

impl Default for ElicitationRecoveryCapability {
    fn default() -> Self {
        Self {
            version: ELICITATION_RECOVERY_VERSION,
            authority_notification: ELICITATION_RECOVERY_AUTHORITY_NOTIFICATION.to_string(),
            list_pending_method: ELICITATION_RECOVERY_LIST_PENDING_METHOD.to_string(),
            attach_method: ELICITATION_RECOVERY_ATTACH_METHOD.to_string(),
        }
    }
}

/// Sent only on the protected connection that originally received the question.
///
/// Deliberately omits `Debug`: the authority is an in-memory bearer secret and must not be logged.
#[typeshare]
#[derive(Clone, Serialize, Deserialize)]
pub struct ElicitationRecoveryAuthorityNotification {
    pub version: u32,
    pub session_id: String,
    pub resume_authority: String,
}

/// Requests the pending session IDs authorized by a process-lifetime bearer secret.
///
/// Deliberately omits `Debug` to keep the authority out of ordinary logs.
#[typeshare]
#[derive(Clone, Serialize, Deserialize)]
pub struct ListPendingElicitationSessionsRequest {
    pub version: u32,
    pub resume_authority: String,
}

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPendingElicitationSessionsResponse {
    pub version: u32,
    pub session_ids: Vec<String>,
}

/// Attaches the current connection to one authorized pending session.
///
/// A successful response is followed by fresh standard ACP `elicitation/create` requests.
/// Deliberately omits `Debug` to keep the authority out of ordinary logs.
#[typeshare]
#[derive(Clone, Serialize, Deserialize)]
pub struct AttachPendingElicitationSessionRequest {
    pub version: u32,
    pub session_id: String,
    pub resume_authority: String,
}

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPendingElicitationSessionResponse {
    pub version: u32,
    pub session_id: String,
    /// Stable opaque identities used by the desktop to reconcile its inbox snapshot.
    pub elicitation_ids: Vec<String>,
}

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationRecoveryDenialReason {
    Unauthorized,
    AuthorityExpired,
    InsecureTransport,
    CapabilityMismatch,
}

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElicitationRecoveryErrorData {
    pub category: String,
    pub reason: ElicitationRecoveryDenialReason,
}

pub fn elicitation_recovery_denied(reason: ElicitationRecoveryDenialReason) -> Error {
    Error::new(
        ELICITATION_RECOVERY_DENIED_CODE,
        "Elicitation recovery denied",
    )
    .data(serde_json::json!(ElicitationRecoveryErrorData {
        category: "elicitation_recovery".to_string(),
        reason,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_fixture_has_stable_versioned_method_names() {
        let fixture = serde_json::to_value(ElicitationRecoveryCapability::default())
            .expect("serialize recovery capability");

        assert_eq!(
            fixture,
            serde_json::json!({
                "version": 1,
                "authority_notification": "querymt/elicitation/recoveryAuthority",
                "list_pending_method": "querymt/elicitation/listPendingSessions",
                "attach_method": "querymt/elicitation/attachSession"
            })
        );
    }

    #[test]
    fn authority_list_and_attach_fixtures_match_the_v1_contract() {
        let authority = serde_json::to_value(ElicitationRecoveryAuthorityNotification {
            version: ELICITATION_RECOVERY_VERSION,
            session_id: "session-1".to_string(),
            resume_authority: "secret".to_string(),
        })
        .expect("serialize authority notification");
        let list_request = serde_json::to_value(ListPendingElicitationSessionsRequest {
            version: ELICITATION_RECOVERY_VERSION,
            resume_authority: "secret".to_string(),
        })
        .expect("serialize pending-session request");
        let attach_response = serde_json::to_value(AttachPendingElicitationSessionResponse {
            version: ELICITATION_RECOVERY_VERSION,
            session_id: "session-1".to_string(),
            elicitation_ids: vec!["elicitation-1".to_string()],
        })
        .expect("serialize attach response");

        assert_eq!(
            authority,
            serde_json::json!({
                "version": 1,
                "session_id": "session-1",
                "resume_authority": "secret"
            })
        );
        assert_eq!(
            list_request,
            serde_json::json!({
                "version": 1,
                "resume_authority": "secret"
            })
        );
        assert_eq!(
            attach_response,
            serde_json::json!({
                "version": 1,
                "session_id": "session-1",
                "elicitation_ids": ["elicitation-1"]
            })
        );
    }

    async fn pending_fixture(
        session_id: &str,
        elicitation_id: &str,
    ) -> crate::test_utils::TestAgent {
        let fixture = crate::test_utils::TestAgent::new().await;
        let (sender, _receiver) = tokio::sync::oneshot::channel();
        crate::elicitation::insert_pending_elicitation(
            &fixture.handle.pending_elicitations(),
            elicitation_id.to_string(),
            session_id.to_string(),
            sender,
        )
        .await;
        fixture
    }

    async fn authorized_fixture(
        session_id: &str,
        elicitation_id: &str,
    ) -> (
        ElicitationRecoveryRegistry,
        crate::test_utils::TestAgent,
        String,
    ) {
        let registry = ElicitationRecoveryRegistry::default();
        let fixture = pending_fixture(session_id, elicitation_id).await;
        registry
            .register_connection("original".to_string(), true)
            .await;
        registry.record_client_capabilities("original", true).await;
        let secret = registry
            .issue_for_session("original", session_id, fixture.handle.as_ref())
            .await
            .expect("original secure connection receives authority");
        (registry, fixture, secret)
    }

    #[tokio::test]
    async fn authority_discovers_and_attaches_only_its_pending_session() {
        let (registry, fixture, secret) = authorized_fixture("session-a", "elicitation-a").await;
        registry
            .register_connection("replacement".to_string(), true)
            .await;
        registry
            .record_client_capabilities("replacement", true)
            .await;

        let list = registry
            .list_pending_sessions(
                "replacement",
                &ListPendingElicitationSessionsRequest {
                    version: ELICITATION_RECOVERY_VERSION,
                    resume_authority: secret.clone(),
                },
                fixture.handle.as_ref(),
            )
            .await
            .expect("valid authority lists pending session");
        assert_eq!(list.session_ids, vec!["session-a"]);

        let attach = registry
            .authorize_attach(
                "replacement",
                &AttachPendingElicitationSessionRequest {
                    version: ELICITATION_RECOVERY_VERSION,
                    session_id: "session-a".to_string(),
                    resume_authority: secret,
                },
                fixture.handle.as_ref(),
            )
            .await
            .expect("valid authority attaches pending session");
        assert_eq!(attach.elicitation_ids, vec!["elicitation-a"]);
    }

    #[tokio::test]
    async fn guessed_or_stolen_session_id_is_denied() {
        let (registry, fixture, secret_a) = authorized_fixture("session-a", "elicitation-a").await;
        let (sender, _receiver) = tokio::sync::oneshot::channel();
        crate::elicitation::insert_pending_elicitation(
            &fixture.handle.pending_elicitations(),
            "elicitation-b".to_string(),
            "session-b".to_string(),
            sender,
        )
        .await;
        registry
            .register_connection("attacker".to_string(), true)
            .await;
        registry.record_client_capabilities("attacker", true).await;
        assert!(
            registry
                .claim_live_delivery(
                    "attacker",
                    "session-a",
                    "elicitation-a",
                    fixture.handle.as_ref(),
                )
                .await
                .is_none()
        );

        for (session_id, authority) in [
            ("session-a", "guessed-secret"),
            ("session-b", secret_a.as_str()),
        ] {
            let error = registry
                .authorize_attach(
                    "attacker",
                    &AttachPendingElicitationSessionRequest {
                        version: ELICITATION_RECOVERY_VERSION,
                        session_id: session_id.to_string(),
                        resume_authority: authority.to_string(),
                    },
                    fixture.handle.as_ref(),
                )
                .await
                .expect_err("invalid authority must be denied");
            assert_eq!(
                error.code,
                agent_client_protocol::ErrorCode::Other(ELICITATION_RECOVERY_DENIED_CODE)
            );
        }
    }

    #[tokio::test]
    async fn stale_authority_is_denied_after_pending_entry_finishes() {
        let (registry, fixture, secret) = authorized_fixture("session-a", "elicitation-a").await;
        registry
            .register_connection("replacement".to_string(), true)
            .await;
        registry
            .record_client_capabilities("replacement", true)
            .await;
        fixture.handle.pending_elicitations().lock().await.clear();

        let error = registry
            .list_pending_sessions(
                "replacement",
                &ListPendingElicitationSessionsRequest {
                    version: ELICITATION_RECOVERY_VERSION,
                    resume_authority: secret,
                },
                fixture.handle.as_ref(),
            )
            .await
            .expect_err("finished authority must expire");
        assert_eq!(
            error.code,
            agent_client_protocol::ErrorCode::Other(ELICITATION_RECOVERY_DENIED_CODE)
        );
    }

    #[tokio::test]
    async fn capability_mismatch_and_insecure_transport_are_denied() {
        let (registry, fixture, secret) = authorized_fixture("session-a", "elicitation-a").await;
        registry
            .register_connection("legacy".to_string(), true)
            .await;
        registry
            .register_connection("remote-http".to_string(), false)
            .await;
        registry
            .record_client_capabilities("remote-http", true)
            .await;

        for (connection_id, version) in [
            ("legacy", ELICITATION_RECOVERY_VERSION),
            ("original", ELICITATION_RECOVERY_VERSION + 1),
            ("remote-http", ELICITATION_RECOVERY_VERSION),
        ] {
            let error = registry
                .list_pending_sessions(
                    connection_id,
                    &ListPendingElicitationSessionsRequest {
                        version,
                        resume_authority: secret.clone(),
                    },
                    fixture.handle.as_ref(),
                )
                .await
                .expect_err("unsupported recovery attempt must be denied");
            assert_eq!(
                error.code,
                agent_client_protocol::ErrorCode::Other(ELICITATION_RECOVERY_DENIED_CODE)
            );
        }
    }

    #[test]
    fn unauthorized_denial_fixture_reveals_no_session_or_authority() {
        let error = elicitation_recovery_denied(ElicitationRecoveryDenialReason::Unauthorized);
        let fixture = serde_json::to_value(error).expect("serialize recovery denial");

        assert_eq!(fixture["code"], ELICITATION_RECOVERY_DENIED_CODE);
        assert_eq!(fixture["message"], "Elicitation recovery denied");
        assert_eq!(
            fixture["data"],
            serde_json::json!({
                "category": "elicitation_recovery",
                "reason": "unauthorized"
            })
        );
        let encoded = fixture.to_string();
        assert!(!encoded.contains("session_id"));
        assert!(!encoded.contains("resume_authority"));
    }
}
