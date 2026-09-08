use super::*;
use crate::error::AgentError;
use futures_util::future::BoxFuture;
use querymt_remote::RemoteTransportFailure;
use tracing::Instrument;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionOperationSafety {
    Idempotent,
    IdempotentWithKey,
    NonIdempotent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionOperation {
    LegacyPrompt,
    SubmitInput { has_key: bool },
    RuntimeState,
    Cancel,
    GetMode,
    GetReasoningEffort,
    SetMode,
    SetReasoningEffort,
    SetModel,
    Undo,
    Redo,
    Fork,
    FileIndex,
    ReadFile,
    EventRefresh,
}

impl SessionOperation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LegacyPrompt => "legacy_prompt",
            Self::SubmitInput { .. } => "submit_input",
            Self::RuntimeState => "runtime_state",
            Self::Cancel => "cancel",
            Self::GetMode => "get_mode",
            Self::GetReasoningEffort => "get_reasoning_effort",
            Self::SetMode => "set_mode",
            Self::SetReasoningEffort => "set_reasoning_effort",
            Self::SetModel => "set_model",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::Fork => "fork",
            Self::FileIndex => "file_index",
            Self::ReadFile => "read_file",
            Self::EventRefresh => "event_refresh",
        }
    }

    pub(crate) fn safety(self) -> SessionOperationSafety {
        match self {
            Self::RuntimeState
            | Self::GetMode
            | Self::GetReasoningEffort
            | Self::FileIndex
            | Self::ReadFile
            | Self::EventRefresh => SessionOperationSafety::Idempotent,
            Self::SubmitInput { has_key: true } => SessionOperationSafety::IdempotentWithKey,
            Self::LegacyPrompt
            | Self::SubmitInput { has_key: false }
            | Self::Cancel
            | Self::SetMode
            | Self::SetReasoningEffort
            | Self::SetModel
            | Self::Undo
            | Self::Redo
            | Self::Fork => SessionOperationSafety::NonIdempotent,
        }
    }

    fn can_retry_after_recovery(self, failure: &RemoteTransportFailure, same_actor: bool) -> bool {
        self.can_retry(failure)
            && (failure.proven_not_delivered()
                || self.safety() == SessionOperationSafety::Idempotent
                || same_actor)
    }

    fn can_retry(self, failure: &RemoteTransportFailure) -> bool {
        failure.is_retryable_kind()
            && (matches!(
                self.safety(),
                SessionOperationSafety::Idempotent | SessionOperationSafety::IdempotentWithKey
            ) || failure.proven_not_delivered())
    }
}

#[derive(Clone)]
pub(crate) struct ResolvedSession {
    pub(crate) session_ref: SessionActorRef,
    pub(crate) attachment_id: Option<u64>,
}

#[derive(Debug)]
pub(crate) enum SessionOperationError {
    NotFound {
        session_id: String,
    },
    LocationConflict {
        session_id: String,
    },
    Storage {
        session_id: String,
        message: String,
    },
    Connect(remote_connect::RemoteSessionConnectError),
    Failed(AgentError),
    OutcomeUnknown {
        session_id: String,
        operation: SessionOperation,
        failure: RemoteTransportFailure,
    },
}

impl SessionOperationError {
    fn after_retry(
        session_id: &str,
        operation: SessionOperation,
        first_failure: &RemoteTransportFailure,
        error: AgentError,
    ) -> Self {
        match error.transport_failure() {
            Some(retry_failure)
                if !retry_failure.proven_not_delivered()
                    || !first_failure.proven_not_delivered() =>
            {
                Self::OutcomeUnknown {
                    session_id: session_id.to_owned(),
                    operation,
                    failure: if first_failure.proven_not_delivered() {
                        retry_failure.clone()
                    } else {
                        first_failure.clone()
                    },
                }
            }
            _ => Self::Failed(error),
        }
    }

    pub(crate) fn into_agent_error(self) -> AgentError {
        match self {
            Self::Failed(error) => error,
            Self::OutcomeUnknown {
                session_id,
                operation,
                failure,
            } => AgentError::TurnControl {
                kind: "submission_outcome_unknown".to_string(),
                message: format!(
                    "{} outcome is unknown for session {} ({}; delivery={})",
                    operation.as_str(),
                    session_id,
                    failure.kind.as_str(),
                    failure.delivery.as_str()
                ),
            },
            Self::NotFound { session_id } => AgentError::SessionNotFound { session_id },
            Self::LocationConflict { session_id } => AgentError::Internal(format!(
                "session_location_conflict: local row and remote bookmark both exist for {session_id}"
            )),
            Self::Storage {
                session_id,
                message,
            } => AgentError::Internal(format!("failed to resolve session {session_id}: {message}")),
            Self::Connect(error) => AgentError::TurnControl {
                kind: error.code().to_string(),
                message: error.to_string(),
            },
        }
    }

    pub(crate) fn into_acp_error(self) -> Error {
        match self {
            Self::Connect(error) => error.to_acp_error(),
            Self::Failed(error) => Error::from(error),
            Self::NotFound { session_id } => Error::invalid_params().data(serde_json::json!({
                "category": "session_operation",
                "code": "session_not_found",
                "session_id": session_id,
            })),
            Self::LocationConflict { session_id } => {
                Error::invalid_params().data(serde_json::json!({
                    "category": "session_operation",
                    "code": "session_location_conflict",
                    "session_id": session_id,
                }))
            }
            Self::Storage {
                session_id,
                message,
            } => Error::internal_error().data(serde_json::json!({
                "category": "session_operation",
                "code": "session_resolution_failed",
                "session_id": session_id,
                "message": message,
            })),
            Self::OutcomeUnknown {
                session_id,
                operation,
                failure,
            } => Error::internal_error().data(serde_json::json!({
                "category": "session_operation",
                "code": "submission_outcome_unknown",
                "session_id": session_id,
                "operation": operation.as_str(),
                "transport_kind": failure.kind.as_str(),
                "transport_delivery": failure.delivery.as_str(),
                "message": failure.message,
            })),
        }
    }
}

impl LocalAgentHandle {
    pub(crate) async fn refresh_remote_session_events(
        &self,
        session_id: &str,
    ) -> Result<(), SessionOperationError> {
        self.execute_session_operation(session_id, SessionOperation::EventRefresh, |session_ref| {
            let sink = self.config.event_sink.clone();
            let session_ref = session_ref.clone();
            let session_id = session_id.to_owned();
            Box::pin(async move {
                let Some(node_id) = session_ref.remote_node_id().map(str::to_owned) else {
                    return Ok(());
                };
                let cursor = sink
                    .journal()
                    .remote_sync_cursor(&session_id, &node_id)
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?;
                let peer_label = session_ref.node_label().to_owned();
                crate::agent::remote::event_backfill::backfill_remote_events(
                    sink,
                    session_ref,
                    session_id,
                    node_id,
                    peer_label,
                    0,
                    cursor,
                )
                .await
            })
        })
        .await
    }

    pub(crate) async fn session_ref_for_operation(
        &self,
        session_id: &str,
        operation: SessionOperation,
    ) -> Result<ResolvedSession, SessionOperationError> {
        let span = tracing::info_span!(
            "remote.session.operation",
            session_id,
            operation = operation.as_str(),
            attachment_id = tracing::field::Empty,
        );
        async move {
            let store = self.config.provider.history_store();
            use crate::session::location::{SessionLocation, resolve_session_location};
            let location = resolve_session_location(store.as_ref(), session_id)
                .await
                .map_err(|error| SessionOperationError::Storage {
                    session_id: session_id.to_string(),
                    message: error.to_string(),
                })?;
            match location {
                SessionLocation::Conflict { .. } => Err(SessionOperationError::LocationConflict {
                    session_id: session_id.to_string(),
                }),
                SessionLocation::Local => self
                    .session_ref_for_agent_session(session_id)
                    .await
                    .map(|session_ref| ResolvedSession {
                        session_ref,
                        attachment_id: None,
                    })
                    .map_err(|error| SessionOperationError::Storage {
                        session_id: session_id.to_string(),
                        message: error.to_string(),
                    }),
                SessionLocation::Remote { bookmark } => {
                    let installed = {
                        let registry = self.registry.lock().await;
                        registry.remote_attachment(session_id)
                    };
                    if let Some(snapshot) = installed {
                        tracing::Span::current().record("attachment_id", snapshot.attachment_id);
                        return Ok(ResolvedSession {
                            session_ref: snapshot.session_ref,
                            attachment_id: Some(snapshot.attachment_id),
                        });
                    }
                    self.ensure_remote_session_connected(
                        session_id,
                        Some(&bookmark.node_id),
                        remote_connect::RemoteConnectReason::OperationRecovery,
                    )
                    .await
                    .map(|connected| ResolvedSession {
                        session_ref: connected.session_ref,
                        attachment_id: Some(connected.attachment_id),
                    })
                    .map_err(SessionOperationError::Connect)
                }
                SessionLocation::NotFound => Err(SessionOperationError::NotFound {
                    session_id: session_id.to_string(),
                }),
            }
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn execute_session_operation<T, F>(
        &self,
        session_id: &str,
        operation: SessionOperation,
        mut invoke: F,
    ) -> Result<T, SessionOperationError>
    where
        F: for<'a> FnMut(&'a SessionActorRef) -> BoxFuture<'a, Result<T, AgentError>>,
    {
        let resolved = self
            .session_ref_for_operation(session_id, operation)
            .await?;
        let error = match invoke(&resolved.session_ref).await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(failure) = error.transport_failure().cloned() else {
            return Err(SessionOperationError::Failed(error));
        };
        let Some(attachment_id) = resolved.attachment_id else {
            return Err(SessionOperationError::Failed(error));
        };
        if !failure.is_retryable_kind() {
            return Err(SessionOperationError::Failed(error));
        }

        // §17/§18: invalidation → single-flight recovery → retry decision is
        // one observable unit. `retry_decision` records whether the operation
        // was replayed once, refused replay (outcome unknown), or the recovery
        // itself failed.
        let retry_span = tracing::info_span!(
            "remote.session.operation.retry",
            session_id,
            operation = operation.as_str(),
            failure_kind = failure.kind.as_str(),
            delivery = failure.delivery.as_str(),
            previous_attachment_id = attachment_id,
            recovered_attachment_id = tracing::field::Empty,
            retry_decision = tracing::field::Empty,
        );
        async move {
            let stale = {
                let mut registry = self.registry.lock().await;
                registry.invalidate_remote_attachment_if_current(session_id, attachment_id)
            };
            if let Some(stale) = stale {
                crate::agent::session_registry::cleanup_installed_remote_attachment(stale, false)
                    .await;
            }

            let recovered = self
                .connect_remote_session(
                    session_id,
                    remote_connect::RemoteConnectOptions {
                        node_hint: resolved.session_ref.remote_node_id(),
                        peer_label: None,
                        preferred_scope: None,
                        reason: remote_connect::RemoteConnectReason::OperationRecovery,
                        replace: remote_connect::RemoteReplacePolicy::ReplaceIfMatches(attachment_id),
                        handoff: None,
                    },
                )
                .await
                .map_err(|error| {
                    tracing::Span::current().record("retry_decision", "recovery_failed");
                    if !failure.proven_not_delivered() {
                        SessionOperationError::OutcomeUnknown { session_id: session_id.to_owned(), operation, failure: failure.clone() }
                    } else { SessionOperationError::Connect(error) }
                })?;

            // Only ambiguous keyed submissions depend on an actor-local receipt.
            let same_remote_actor = match (&resolved.session_ref, &recovered.session_ref) {
                (
                    SessionActorRef::Remote {
                        actor_ref: failed_ref,
                        remote_node_id: failed_node,
                        ..
                    },
                    SessionActorRef::Remote {
                        actor_ref: recovered_ref,
                        remote_node_id: recovered_node,
                        ..
                    },
                ) => {
                    failed_ref.id().peer_id() == recovered_ref.id().peer_id()
                        && failed_ref.id().sequence_id() == recovered_ref.id().sequence_id()
                        && failed_node == recovered_node
                }
                _ => false,
            };
            if !operation.can_retry_after_recovery(&failure, same_remote_actor) {
                tracing::Span::current().record("retry_decision", "outcome_unknown_no_replay");
                log::warn!(
                    "remote session operation not replayed (session_id={}, operation={}, attachment_id={}, failure_kind={}, delivery={})",
                    session_id,
                    operation.as_str(),
                    attachment_id,
                    failure.kind.as_str(),
                    failure.delivery.as_str(),
                );
                return Err(SessionOperationError::OutcomeUnknown {
                    session_id: session_id.to_string(),
                    operation,
                    failure,
                });
            }

            tracing::Span::current().record("recovered_attachment_id", recovered.attachment_id);
            tracing::Span::current().record("retry_decision", "replayed_once");
            tracing::info!(
                session_id,
                operation = operation.as_str(),
                previous_attachment_id = attachment_id,
                attachment_id = recovered.attachment_id,
                "retrying remote session operation once after recovery"
            );
            invoke(&recovered.session_ref)
                .await
                .map_err(|error| SessionOperationError::after_retry(session_id, operation, &failure, error))
        }
        .instrument(retry_span)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::{SessionRuntime, ToolPolicy};
    use crate::agent::session_actor::SessionActor;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session::store::RemoteSessionBookmark;
    use crate::test_utils::{
        MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
    };
    use kameo::actor::Spawn;
    use querymt::LLMParams;
    use querymt_remote::{DeliveryCertainty, RemoteTransportFailureKind};
    use std::sync::Arc;

    fn failure(
        kind: RemoteTransportFailureKind,
        delivery: DeliveryCertainty,
    ) -> RemoteTransportFailure {
        RemoteTransportFailure::new(kind, delivery, "test")
    }

    async fn handle_with_real_storage() -> (LocalAgentHandle, Arc<SqliteStorage>, tempfile::TempDir)
    {
        let provider = Arc::new(tokio::sync::Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider,
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared));
        let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("storage"),
        );
        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                storage.clone(),
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .build(),
        );
        (LocalAgentHandle::from_config(config), storage, temp_dir)
    }

    fn bookmark(session_id: &str) -> RemoteSessionBookmark {
        RemoteSessionBookmark {
            session_id: session_id.to_string(),
            node_id: "node-offline".to_string(),
            peer_label: "offline".to_string(),
            cwd: None,
            created_at: 1,
            title: None,
        }
    }

    #[test]
    fn retry_after_actor_replacement_requires_receipt_only_for_ambiguous_keyed_input() {
        let unknown = failure(
            RemoteTransportFailureKind::ReplyTimeout,
            DeliveryCertainty::Unknown,
        );
        let not_delivered = failure(
            RemoteTransportFailureKind::ActorUnavailable,
            DeliveryCertainty::NotDelivered,
        );
        assert!(SessionOperation::RuntimeState.can_retry_after_recovery(&unknown, false));
        assert!(SessionOperation::LegacyPrompt.can_retry_after_recovery(&not_delivered, false));
        assert!(SessionOperation::Fork.can_retry_after_recovery(&not_delivered, false));
        assert!(
            SessionOperation::SubmitInput { has_key: true }
                .can_retry_after_recovery(&not_delivered, false)
        );
        assert!(
            !SessionOperation::SubmitInput { has_key: true }
                .can_retry_after_recovery(&unknown, false)
        );
        assert!(
            SessionOperation::SubmitInput { has_key: true }
                .can_retry_after_recovery(&unknown, true)
        );
        assert!(!SessionOperation::LegacyPrompt.can_retry_after_recovery(&unknown, true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_recovery_retries_idempotent_operation_after_actor_replacement() {
        assert_actor_replacement_retry(
            SessionOperation::RuntimeState,
            DeliveryCertainty::Unknown,
            true,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_recovery_retries_not_delivered_operation_after_actor_replacement() {
        assert_actor_replacement_retry(
            SessionOperation::LegacyPrompt,
            DeliveryCertainty::NotDelivered,
            true,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_recovery_does_not_replay_ambiguous_input_after_actor_replacement() {
        assert_actor_replacement_retry(
            SessionOperation::SubmitInput { has_key: true },
            DeliveryCertainty::Unknown,
            false,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_recovery_repeated_actor_replacements_do_not_stall_idle_mesh() {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            for _ in 0..3 {
                assert_actor_replacement_retry(
                    SessionOperation::RuntimeState,
                    DeliveryCertainty::Unknown,
                    true,
                )
                .await;
                assert_actor_replacement_retry(
                    SessionOperation::LegacyPrompt,
                    DeliveryCertainty::NotDelivered,
                    true,
                )
                .await;
                assert_actor_replacement_retry(
                    SessionOperation::SubmitInput { has_key: true },
                    DeliveryCertainty::Unknown,
                    false,
                )
                .await;
            }
        })
        .await
        .expect("repeated replacements must not require network activity to wake the mesh");
    }

    async fn assert_actor_replacement_retry(
        operation: SessionOperation,
        certainty: DeliveryCertainty,
        should_retry: bool,
    ) {
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        let handle = Arc::new(handle);
        let (host, _host_storage, _host_tmp) = handle_with_real_storage().await;
        handle.set_mesh(mesh.clone());
        let id = format!("operation-replacement-{}", uuid::Uuid::now_v7());
        let make_actor = || {
            SessionActor::spawn(
                SessionActor::new(
                    host.config.clone(),
                    id.clone(),
                    SessionRuntime::new(
                        None,
                        Default::default(),
                        crate::agent::core::McpToolState::empty(),
                    ),
                )
                .with_mesh(Some(mesh.clone())),
            )
        };
        let original = make_actor();
        let replacement = make_actor();
        let replacement_remote = replacement.clone().into_remote_ref().await;
        handle
            .attach_remote_session(
                id.clone(),
                original.into_remote_ref().await,
                "test-peer".into(),
                None,
                Some(mesh.peer_id().to_string()),
            )
            .await
            .unwrap();
        let mut attempts = 0;
        let result = handle
            .execute_session_operation(&id, operation, |sr| {
                attempts += 1;
                let attempt = attempts;
                let sr = sr.clone();
                let mesh = mesh.clone();
                let handle = handle.clone();
                let replacement = replacement.clone();
                let id = id.clone();
                Box::pin(async move {
                    if attempt == 1 {
                        // A concurrent recovery installs a new actor before
                        // the old in-flight operation reports its failure.
                        handle
                            .attach_remote_session(
                                id,
                                replacement.into_remote_ref().await,
                                "test-peer".into(),
                                None,
                                Some(mesh.peer_id().to_string()),
                            )
                            .await
                            .map_err(|e| AgentError::Internal(e.to_string()))?;
                        return Err(AgentError::from_transport_failure(failure(
                            RemoteTransportFailureKind::ConnectionClosed,
                            certainty,
                        )));
                    }
                    sr.get_mode().await
                })
            })
            .await;
        if should_retry {
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(attempts, 2);
        } else {
            assert!(matches!(
                result,
                Err(SessionOperationError::OutcomeUnknown { .. })
            ));
            assert_eq!(attempts, 1);
        }
        let installed = handle.registry.lock().await.get(&id).cloned().unwrap();
        match installed {
            SessionActorRef::Remote { actor_ref, .. } => {
                assert_eq!(actor_ref.id(), replacement_remote.id())
            }
            _ => panic!("recovery created a local replacement"),
        }
    }

    #[test]
    fn operation_safety_is_conservative() {
        assert_eq!(
            SessionOperation::RuntimeState.safety(),
            SessionOperationSafety::Idempotent
        );
        assert_eq!(
            SessionOperation::SubmitInput { has_key: true }.safety(),
            SessionOperationSafety::IdempotentWithKey
        );
        assert_eq!(
            SessionOperation::LegacyPrompt.safety(),
            SessionOperationSafety::NonIdempotent
        );
    }

    #[test]
    fn retry_policy_rejects_ambiguous_non_idempotent_and_protocol_failures() {
        assert!(!SessionOperation::LegacyPrompt.can_retry(&failure(
            RemoteTransportFailureKind::ReplyTimeout,
            DeliveryCertainty::Unknown,
        )));
        assert!(SessionOperation::LegacyPrompt.can_retry(&failure(
            RemoteTransportFailureKind::ActorUnavailable,
            DeliveryCertainty::NotDelivered,
        )));
        assert!(!SessionOperation::RuntimeState.can_retry(&failure(
            RemoteTransportFailureKind::ProtocolMismatch,
            DeliveryCertainty::NotDelivered,
        )));
        assert!(SessionOperation::RuntimeState.can_retry(&failure(
            RemoteTransportFailureKind::ReplyTimeout,
            DeliveryCertainty::Unknown,
        )));
        assert!(
            SessionOperation::SubmitInput { has_key: true }.can_retry(&failure(
                RemoteTransportFailureKind::ReplyTimeout,
                DeliveryCertainty::Unknown,
            ))
        );
        assert!(
            !SessionOperation::SubmitInput { has_key: false }.can_retry(&failure(
                RemoteTransportFailureKind::ReplyTimeout,
                DeliveryCertainty::Unknown,
            ))
        );
    }

    #[tokio::test]
    async fn resolver_returns_local_reference_without_remote_attachment() {
        let (handle, storage, _temp) = handle_with_real_storage().await;
        let store = storage.session_store();
        let session = store
            .create_session(None, None, None, None)
            .await
            .expect("create local session");
        let actor = SessionActor::spawn(SessionActor::new(
            handle.config.clone(),
            session.public_id.clone(),
            SessionRuntime::new(
                None,
                std::collections::HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        ));
        handle
            .registry
            .lock()
            .await
            .insert(session.public_id.clone(), actor.clone());

        let resolved = handle
            .session_ref_for_operation(&session.public_id, SessionOperation::RuntimeState)
            .await
            .expect("resolve local session");
        assert!(!resolved.session_ref.is_remote());
        assert_eq!(resolved.attachment_id, None);
    }

    #[tokio::test]
    async fn offline_bookmark_recovers_without_local_substitution() {
        let (handle, storage, _temp) = handle_with_real_storage().await;
        let session_id = "remote-offline";
        storage
            .session_store()
            .save_remote_session_bookmark(&bookmark(session_id))
            .await
            .expect("save bookmark");

        let error = handle
            .session_ref_for_operation(session_id, SessionOperation::RuntimeState)
            .await
            .err()
            .expect("offline node must fail recovery");
        assert!(matches!(error, SessionOperationError::Connect(_)));
        assert!(
            storage
                .session_store()
                .get_session(session_id)
                .await
                .expect("local lookup")
                .is_none()
        );
        assert!(
            storage
                .session_store()
                .get_remote_session_bookmark(session_id)
                .await
                .expect("bookmark lookup")
                .is_some()
        );
    }

    #[tokio::test]
    async fn resolver_reports_durable_location_conflict() {
        let (handle, storage, _temp) = handle_with_real_storage().await;
        let store = storage.session_store();
        let session = store
            .create_session(None, None, None, None)
            .await
            .expect("create local session");
        store
            .save_remote_session_bookmark(&bookmark(&session.public_id))
            .await
            .expect("save conflicting bookmark");

        let error = handle
            .session_ref_for_operation(&session.public_id, SessionOperation::GetMode)
            .await
            .err()
            .expect("conflict must fail");
        assert!(matches!(
            error,
            SessionOperationError::LocationConflict { .. }
        ));
    }

    #[test]
    fn final_retry_preserves_ambiguity_from_either_attempt() {
        for (first, retry, unknown) in [
            (
                DeliveryCertainty::Unknown,
                DeliveryCertainty::NotDelivered,
                true,
            ),
            (
                DeliveryCertainty::NotDelivered,
                DeliveryCertainty::Unknown,
                true,
            ),
            (DeliveryCertainty::Unknown, DeliveryCertainty::Unknown, true),
            (
                DeliveryCertainty::NotDelivered,
                DeliveryCertainty::NotDelivered,
                false,
            ),
        ] {
            let result = SessionOperationError::after_retry(
                "session",
                SessionOperation::SubmitInput { has_key: true },
                &failure(RemoteTransportFailureKind::ConnectionClosed, first),
                AgentError::from_transport_failure(failure(
                    RemoteTransportFailureKind::ConnectionClosed,
                    retry,
                )),
            );
            assert_eq!(
                matches!(result, SessionOperationError::OutcomeUnknown { .. }),
                unknown
            );
            if let SessionOperationError::OutcomeUnknown { failure, .. } = result {
                assert!(!failure.proven_not_delivered());
            }
        }
    }

    #[test]
    fn outcome_unknown_maps_to_structured_turn_control_error() {
        let error = SessionOperationError::OutcomeUnknown {
            session_id: "remote-session".to_string(),
            operation: SessionOperation::LegacyPrompt,
            failure: failure(
                RemoteTransportFailureKind::ReplyTimeout,
                DeliveryCertainty::Unknown,
            ),
        }
        .into_agent_error();
        assert!(matches!(
            error,
            AgentError::TurnControl { ref kind, .. } if kind == "submission_outcome_unknown"
        ));
    }
}
