//! Remote session connection coordinator (plan §2).
//!
//! One canonical recovery algorithm for remote session connectivity, shared
//! by UI attach, extension attach, and future operation-recovery callers.
//! The coordinator owns:
//!
//! - bookmark lookup and validation (durable identity),
//! - existing attachment reuse (no preflight RTT for healthy attachments),
//! - per-session single-flight synchronization (recovery gate),
//! - scoped DHT lookup with node-manager resume fallback,
//! - handoff resolution,
//! - bounded control health verification,
//! - merged bookmark persistence,
//! - structured outcomes and tracing.
//!
//! The registry mutex is never used as the recovery lock; recovery is
//! serialized per session by [`RemoteConnectGate`]. Transactional
//! prepare/commit/cleanup attachment internals arrive in Phase 4 — until
//! then all funnel through the single `SessionRegistry::attach_remote_session`
//! path used here.

use super::*;

use crate::error::AgentError;
use crate::session::store::{RemoteSessionBookmark, RemoteSessionBookmarkUpdate};
use querymt_remote::RemoteTransportFailure;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::Instrument;

// ── Reason / outcome / result types ─────────────────────────────────────

/// Why a connect attempt is running. Drives timeout budgets and default
/// attachment-replacement semantics.
//
// Some variants are only constructed by Phase 6/8 callers (operation recovery,
// offline-first open, startup reattach) and this module's own tests.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteConnectReason {
    /// Opening a session view (interactive, latency-sensitive).
    Open,
    /// User-driven reconnect; expected to fix a broken attachment.
    ExplicitReconnect,
    /// An operation failed against the current attachment.
    OperationRecovery,
    /// Startup reattachment of bookmarked sessions.
    StartupReattach,
    /// Extension-level attach request (`remote/attach_session`).
    ExtensionAttach,
}

impl RemoteConnectReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::ExplicitReconnect => "explicit_reconnect",
            Self::OperationRecovery => "operation_recovery",
            Self::StartupReattach => "startup_reattach",
            Self::ExtensionAttach => "extension_attach",
        }
    }

    /// Replacement semantics used by the plan-shaped 3-arg entry point.
    ///
    /// `ExplicitReconnect` always reinstalls (the user asked to fix the
    /// attachment). `OperationRecovery` also forces replacement — Phase 6
    /// callers should prefer [`RemoteReplacePolicy::ReplaceIfMatches`] to
    /// stay generation-exact. Open-time and first-attach paths reuse.
    pub(crate) fn default_replace_policy(self) -> RemoteReplacePolicy {
        match self {
            Self::Open | Self::StartupReattach | Self::ExtensionAttach => {
                RemoteReplacePolicy::ReuseIfPresent
            }
            Self::ExplicitReconnect | Self::OperationRecovery => {
                RemoteReplacePolicy::ReplaceCurrent
            }
        }
    }

    /// Total budget for the recovery stages (lookup/resume/attach/health).
    /// The reuse fast path is never budget-bound. Each reason is separately
    /// overridable via env so operators can tune per environment.
    pub(crate) fn connect_budget(self) -> Duration {
        let (env_var, default_ms) = match self {
            Self::Open => ("QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS", 5_000),
            Self::ExplicitReconnect => ("QUERYMT_REMOTE_CONNECT_RECONNECT_BUDGET_MS", 15_000),
            Self::OperationRecovery => ("QUERYMT_REMOTE_CONNECT_OPERATION_BUDGET_MS", 10_000),
            Self::StartupReattach => ("QUERYMT_REMOTE_CONNECT_STARTUP_BUDGET_MS", 10_000),
            Self::ExtensionAttach => ("QUERYMT_REMOTE_CONNECT_ATTACH_BUDGET_MS", 15_000),
        };
        let timeout_ms = std::env::var(env_var)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms);
        Duration::from_millis(timeout_ms)
    }
}

/// Whether an already-installed attachment may satisfy a connect request.
//
// `ReplaceCurrent`/`ReplaceIfMatches` are used by Phase 6 recovery callers and
// this module's own tests.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteReplacePolicy {
    /// Reuse the installed attachment, if any, without a network round trip.
    ReuseIfPresent,
    /// Tear down whatever is currently installed and re-attach.
    ReplaceCurrent,
    /// Invalidate only if the installed attachment is exactly `generation`
    /// (relay actor id); a newer generation satisfies the request.
    ReplaceIfMatches(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteConnectOutcome {
    /// Installed attachment reused as-is; no network work performed.
    Reused,
    /// Fresh attachment installed straight from a DHT publication.
    Reattached,
    /// Fresh attachment installed after node-manager resume (the handoff
    /// branch included — it originates from a create/resume response).
    Resumed,
}

impl RemoteConnectOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reused => "reused",
            Self::Reattached => "reattached",
            Self::Resumed => "resumed",
        }
    }
}

/// Result of a successful connect attempt. `session_ref`/`bookmark` are read
/// by Phase 6/8 callers; the current ext attach migration only needs
/// success/failure and the generation (`attachment_id`).
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ConnectedRemoteSession {
    pub(crate) session_ref: SessionActorRef,
    /// Attachment generation (relay actor id) installed/observed by this call.
    pub(crate) attachment_id: u64,
    /// Durable identity. `None` only when the store lookup failed on the
    /// reuse path (logged there); fresh attaches always persist first.
    pub(crate) bookmark: Option<RemoteSessionBookmark>,
    pub(crate) outcome: RemoteConnectOutcome,
}

// ── Connect error ───────────────────────────────────────────────────────

/// Typed connect failure. Codes are stable strings aligned with plan §13 so
/// later protocol plumbing does not need string matching.
#[derive(Debug)]
pub(crate) enum RemoteSessionConnectError {
    /// No durable identity (bookmark) and no node hint — nothing to connect to.
    BookmarkMissing { session_id: String },
    /// The bookmark store itself failed; refused rather than guessing.
    BookmarkLookupFailed { session_id: String, message: String },
    /// Local durable/actor state conflicts with a remote attachment.
    LocationConflict { session_id: String, message: String },
    /// Mesh runtime is not bootstrapped locally, so no remote work is possible.
    MeshUnavailable { node_id: String },
    /// The target node could not be contacted at all.
    NodeUnavailable {
        node_id: String,
        transport: Option<RemoteTransportFailure>,
        message: String,
    },
    /// Node was reachable but resume/handoff resolution failed.
    RecoveryFailed {
        transport: Option<RemoteTransportFailure>,
        message: String,
    },
    /// The recovery budget elapsed before a connected attachment existed.
    TimedOut { budget_ms: u64 },
    /// A candidate attachment failed the bounded control health check.
    HealthCheckFailed { session_id: String, message: String },
    /// The host is reachable but explicitly reports the session does not exist
    /// there (resume-side `RemoteSessionNotFound`). Kept distinct so callers
    /// get a not-found (-32002) response, not a transient-looking failure.
    SessionNotFoundOnHost {
        session_id: String,
        node_id: String,
        message: String,
    },
}

impl RemoteSessionConnectError {
    /// Stable machine-readable code (future protocol codes per plan §13).
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::BookmarkMissing { .. } => "session_bookmark_missing",
            Self::BookmarkLookupFailed { .. } => "session_bookmark_lookup_failed",
            Self::LocationConflict { .. } => "session_location_conflict",
            Self::MeshUnavailable { .. } => "remote_node_unavailable",
            Self::NodeUnavailable { .. } => "remote_node_unavailable",
            Self::RecoveryFailed { .. } => "remote_recovery_failed",
            Self::TimedOut { .. } => "remote_recovery_timeout",
            Self::HealthCheckFailed { .. } => "remote_recovery_failed",
            Self::SessionNotFoundOnHost { .. } => "remote_session_not_found_on_host",
        }
    }

    /// Typed transport failure for retry/invalidation decisions (Phase 6).
    /// `None` when the failure is local/structural rather than transport.
    pub(crate) fn transport_failure(&self) -> Option<&RemoteTransportFailure> {
        match self {
            Self::NodeUnavailable { transport, .. } | Self::RecoveryFailed { transport, .. } => {
                transport.as_ref()
            }
            _ => None,
        }
    }

    /// Whether a caller may reasonably retry this connect attempt. Local
    /// structural errors (missing identity, conflicts) are not retriable.
    pub(crate) fn is_retriable(&self) -> bool {
        match self {
            Self::BookmarkMissing { .. }
            | Self::BookmarkLookupFailed { .. }
            | Self::LocationConflict { .. }
            | Self::SessionNotFoundOnHost { .. } => false,
            Self::MeshUnavailable { .. }
            | Self::NodeUnavailable { .. }
            | Self::RecoveryFailed { .. }
            | Self::TimedOut { .. }
            | Self::HealthCheckFailed { .. } => true,
        }
    }

    pub(crate) fn to_acp_error(&self) -> agent_client_protocol::Error {
        let code = match self {
            Self::BookmarkMissing { .. }
            | Self::LocationConflict { .. }
            | Self::SessionNotFoundOnHost { .. } => -32002,
            _ => -32603,
        };
        let mut data = serde_json::json!({
            "category": "remote_session_connect",
            "code": self.code(),
            "retriable": self.is_retriable(),
            "message": self.to_string(),
        });
        match self {
            Self::BookmarkMissing { session_id }
            | Self::BookmarkLookupFailed { session_id, .. }
            | Self::LocationConflict { session_id, .. }
            | Self::HealthCheckFailed { session_id, .. } => {
                data["session_id"] = serde_json::Value::String(session_id.clone());
            }
            Self::MeshUnavailable { node_id } | Self::NodeUnavailable { node_id, .. } => {
                data["node_id"] = serde_json::Value::String(node_id.clone());
            }
            Self::SessionNotFoundOnHost {
                session_id,
                node_id,
                ..
            } => {
                data["session_id"] = serde_json::Value::String(session_id.clone());
                data["node_id"] = serde_json::Value::String(node_id.clone());
            }
            Self::RecoveryFailed { .. } | Self::TimedOut { .. } => {}
        }
        if let Some(failure) = self.transport_failure() {
            data["transport_kind"] =
                serde_json::to_value(failure.kind).unwrap_or(serde_json::Value::Null);
            data["transport_delivery"] =
                serde_json::to_value(failure.delivery).unwrap_or(serde_json::Value::Null);
        }
        agent_client_protocol::Error::new(code, self.to_string()).data(data)
    }
}

impl std::fmt::Display for RemoteSessionConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BookmarkMissing { session_id } => write!(
                f,
                "remote session '{session_id}' has no bookmark and no node hint was provided"
            ),
            Self::BookmarkLookupFailed {
                session_id,
                message,
            } => write!(
                f,
                "failed to read bookmark for remote session '{session_id}': {message}"
            ),
            Self::LocationConflict {
                session_id,
                message,
            } => write!(
                f,
                "remote session '{session_id}' conflicts with local state: {message}"
            ),
            Self::MeshUnavailable { node_id } => write!(
                f,
                "remote mesh is not bootstrapped; cannot reach node '{node_id}'"
            ),
            Self::NodeUnavailable {
                node_id, message, ..
            } => {
                write!(f, "remote node '{node_id}' is unavailable: {message}")
            }
            Self::RecoveryFailed { message, .. } => {
                write!(f, "remote session recovery failed: {message}")
            }
            Self::SessionNotFoundOnHost {
                session_id,
                node_id,
                message,
            } => write!(
                f,
                "remote session '{session_id}' not found on host '{node_id}': {message}"
            ),
            Self::TimedOut { budget_ms } => write!(
                f,
                "remote session recovery exceeded the {budget_ms}ms budget"
            ),
            Self::HealthCheckFailed {
                session_id,
                message,
            } => write!(
                f,
                "remote session '{session_id}' attachment failed health verification: {message}"
            ),
        }
    }
}

impl std::error::Error for RemoteSessionConnectError {}

// ── Full options form ───────────────────────────────────────────────────

/// Extended options for callers that already hold resolved pieces of remote
/// state (extension attach) or generation knowledge (operation recovery).
pub(crate) struct RemoteConnectOptions<'a> {
    pub(crate) node_hint: Option<&'a str>,
    pub(crate) reason: RemoteConnectReason,
    pub(crate) replace: RemoteReplacePolicy,
    /// Pre-resolved handoff from a create/resume response. Skips DHT lookup
    /// and node-manager resume entirely.
    pub(crate) handoff: Option<crate::agent::remote::node_manager::SessionHandoff>,
}

// ── Per-session single-flight gate ──────────────────────────────────────

/// Per-session recovery gates. Keeps at most one connect/recovery attempt in
/// flight per session; unrelated sessions proceed concurrently. Distinct from
/// the registry mutex: the gate is held across network work, the registry
/// mutex never is (beyond the pre-existing registry internals — Phase 4).
pub(crate) type RemoteConnectGateMap =
    Arc<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

/// RAII holder for one acquisition of a per-session recovery gate. The map
/// entry is removed once the last waiter drops (mirrors
/// `SessionSingleFlightGuard`): three strong refs remain at that point —
/// the map entry, this struct's `lock` field, and the `Arc` inside
/// `_guard`.
pub(crate) struct RemoteConnectGate {
    session_id: String,
    lock: Arc<tokio::sync::Mutex<()>>,
    map: RemoteConnectGateMap,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl RemoteConnectGate {
    async fn acquire(map: &RemoteConnectGateMap, session_id: &str) -> Self {
        let lock = {
            let mut guard = map.lock();
            guard
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let owned_guard = lock.clone().lock_owned().await;
        Self {
            session_id: session_id.to_string(),
            lock,
            map: map.clone(),
            _guard: owned_guard,
        }
    }
}

impl Drop for RemoteConnectGate {
    fn drop(&mut self) {
        if Arc::strong_count(&self.lock) == 3 {
            let mut map = self.map.lock();
            if map
                .get(&self.session_id)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.lock))
            {
                map.remove(&self.session_id);
            }
        }
    }
}

// ── Coordinator ─────────────────────────────────────────────────────────

impl LocalAgentHandle {
    /// Plan §2 entry point: ensure a connected attachment exists for a remote
    /// session, recovering it when necessary. Consumed by Phase 6 operation
    /// recovery and Phase 8 offline-first open; today exercised by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn ensure_remote_session_connected(
        &self,
        session_id: &str,
        node_hint: Option<&str>,
        reason: RemoteConnectReason,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        self.connect_remote_session(
            session_id,
            RemoteConnectOptions {
                node_hint,
                reason,
                replace: reason.default_replace_policy(),
                handoff: None,
            },
        )
        .await
    }

    /// Full coordinator entry point for callers with extra context
    /// (handoff from resume/create, explicit replacement policy).
    pub(crate) async fn connect_remote_session(
        &self,
        session_id: &str,
        options: RemoteConnectOptions<'_>,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        let started = Instant::now();
        let span = tracing::info_span!(
            "remote.session.connect",
            session_id,
            reason = options.reason.as_str(),
            node_hint = options.node_hint.unwrap_or(""),
            outcome = tracing::field::Empty,
            attachment_id = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
            error = tracing::field::Empty,
        );
        let result = self
            .connect_remote_session_gated(session_id, options)
            .instrument(span.clone())
            .await;
        span.record("elapsed_ms", started.elapsed().as_millis() as u64);
        match &result {
            Ok(connected) => {
                span.record("outcome", connected.outcome.as_str());
                span.record("attachment_id", connected.attachment_id);
            }
            Err(err) => {
                span.record("error", err.code());
            }
        }
        result
    }

    async fn connect_remote_session_gated(
        &self,
        session_id: &str,
        options: RemoteConnectOptions<'_>,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        // Single-flight per session; held for the whole attempt. Followers
        // that arrive while recovery is in flight re-check the registry after
        // acquiring the gate and reuse the freshly installed attachment.
        let _gate = RemoteConnectGate::acquire(&self.remote_connect_gates, session_id).await;

        // Attachment reuse/replacement decision. Replacement preparation keeps
        // this generation installed until the candidate is fully validated.
        let existing = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().map(|session_ref| {
                let attachment = registry.remote_attachment(session_id);
                (session_ref, attachment)
            })
        };

        if let Some((session_ref, attachment)) = existing {
            let attachment_id = attachment.as_ref().map(|value| value.attachment_id);
            if !session_ref.is_remote() {
                // Never overwrite a local actor with a remote attachment
                // (plan invariants 1 and 3).
                log::warn!(
                    "remote session {} connect: session id resolves to a local actor; \
                     refusing remote recovery (reason={})",
                    session_id,
                    options.reason.as_str(),
                );
                return Err(RemoteSessionConnectError::LocationConflict {
                    session_id: session_id.to_string(),
                    message: "a local actor is registered under this session id".to_string(),
                });
            }

            let reuse_snapshot = match (options.replace, attachment.as_ref()) {
                (RemoteReplacePolicy::ReuseIfPresent, Some(snapshot)) => Some(snapshot),
                (RemoteReplacePolicy::ReplaceIfMatches(failed), Some(snapshot))
                    if snapshot.attachment_id != failed =>
                {
                    // Stale failure: a newer generation is already installed
                    // (plan §18: "stale invalidation ignored").
                    log::info!(
                        "remote session {} connect: stale invalidation ignored — current \
                         attachment_id={} is newer than failed attachment_id={}",
                        session_id,
                        snapshot.attachment_id,
                        failed,
                    );
                    Some(snapshot)
                }
                // No complete attachment snapshot: fall through to the
                // replacement path instead of unwrapping.
                _ => None,
            };

            if let Some(snapshot) = reuse_snapshot {
                let id = snapshot.attachment_id;
                log::info!(
                    "remote session {} connect: existing attachment reused \
                     (attachment_id={}, remote_actor_id={}, node_id={}, peer_label={}, scope={:?}, reason={})",
                    session_id,
                    id,
                    snapshot.remote_actor_id,
                    snapshot.node_id.as_deref().unwrap_or(""),
                    snapshot.peer_label,
                    snapshot.matched_scope,
                    options.reason.as_str(),
                );
                debug_assert_eq!(snapshot.session_ref.is_remote(), session_ref.is_remote());
                let bookmark = self.load_bookmark_for_reuse(session_id, &session_ref).await;
                return Ok(ConnectedRemoteSession {
                    session_ref,
                    attachment_id: id,
                    bookmark,
                    outcome: RemoteConnectOutcome::Reused,
                });
            }

            log::info!(
                "remote session {} connect: preparing replacement while keeping current attachment \
                 (attachment_id={:?}, reason={})",
                session_id,
                attachment_id,
                options.reason.as_str(),
            );
        }

        // ── Durable identity.
        let store = self.config.provider.history_store();
        let bookmark = store
            .get_remote_session_bookmark(session_id)
            .await
            .map_err(|e| RemoteSessionConnectError::BookmarkLookupFailed {
                session_id: session_id.to_string(),
                message: e.to_string(),
            })?;

        // A local session row for the same id is a location conflict (plan
        // invariant 2). Checked only when we are about to recover/install an
        // attachment; pure reuse above never reaches here.
        if bookmark.is_none()
            && matches!(store.get_session(session_id).await, Ok(Some(_)))
            && options.handoff.is_none()
        {
            log::warn!(
                "remote session {} connect: local session row exists with no remote bookmark",
                session_id
            );
            return Err(RemoteSessionConnectError::LocationConflict {
                session_id: session_id.to_string(),
                message: "a local session row exists and no bookmark claims this id as remote"
                    .to_string(),
            });
        }

        let node_id = options
            .node_hint
            .map(str::to_string)
            .or_else(|| bookmark.as_ref().map(|b| b.node_id.clone()))
            .ok_or_else(|| RemoteSessionConnectError::BookmarkMissing {
                session_id: session_id.to_string(),
            })?;

        let mesh = self
            .mesh()
            .ok_or_else(|| RemoteSessionConnectError::MeshUnavailable {
                node_id: node_id.clone(),
            })?;

        // ── Bounded recovery.
        let budget = options.reason.connect_budget();
        let recovery = tokio::time::timeout(
            budget,
            self.recover_remote_attachment(session_id, options, mesh, node_id, bookmark),
        )
        .await;

        match recovery {
            Ok(connected) => connected,
            Err(_) => {
                // Dropping an in-flight prepared attachment synchronously kills
                // and deregisters its candidate relay; the old generation stays installed.
                Err(RemoteSessionConnectError::TimedOut {
                    budget_ms: budget.as_millis() as u64,
                })
            }
        }
    }

    /// Recovery stages: handoff short-circuit → scoped DHT lookup →
    /// node-manager resume. Each candidate attach is health-verified; a failed
    /// DHT candidate falls through to resume rather than failing the connect.
    async fn recover_remote_attachment(
        &self,
        session_id: &str,
        options: RemoteConnectOptions<'_>,
        mesh: crate::agent::remote::MeshHandle,
        node_id: String,
        bookmark: Option<RemoteSessionBookmark>,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        let peer_label = bookmark
            .as_ref()
            .map(|b| b.peer_label.clone())
            .unwrap_or_else(|| node_id.clone());
        // Only scan the mesh for a display label on first-time attaches that
        // lack a bookmark (the label is cosmetic and the scan is expensive).
        let peer_label = if bookmark.is_some() {
            peer_label
        } else {
            self.list_remote_nodes()
                .await
                .into_iter()
                .find(|n| n.node_id.to_string() == node_id)
                .map(|n| n.hostname)
                .unwrap_or(peer_label)
        };

        // Pre-resolved handoff (extension attach from create/resume response).
        if let Some(handoff) = options.handoff {
            let remote_ref = self
                .resolve_handoff(session_id, handoff)
                .await
                .map_err(|e| {
                    if e.code == agent_client_protocol::ErrorCode::ResourceNotFound {
                        RemoteSessionConnectError::SessionNotFoundOnHost {
                            session_id: session_id.to_string(),
                            node_id: node_id.clone(),
                            message: format!("handoff resolution failed: {}", e.message),
                        }
                    } else {
                        RemoteSessionConnectError::RecoveryFailed {
                            transport: None,
                            message: format!("handoff resolution failed: {}", e.message),
                        }
                    }
                })?;
            let merged = self
                .persist_connect_bookmark(session_id, &node_id, &peer_label, None, None, None)
                .await?;
            return self
                .attach_and_verify_remote(
                    session_id,
                    remote_ref,
                    None,
                    &merged,
                    RemoteConnectOutcome::Resumed,
                )
                .await;
        }

        // Stage 1: scoped DHT lookup.
        let lookup_span = tracing::info_span!(
            "remote.session.lookup",
            session_id,
            node_id = node_id.as_str(),
        );
        let dht_hit = Self::lookup_remote_session_actor(&mesh, session_id)
            .instrument(lookup_span)
            .await;

        if let Some((remote_ref, matched_scope)) = dht_hit {
            let merged = self
                .persist_connect_bookmark(session_id, &node_id, &peer_label, None, None, None)
                .await?;
            match self
                .attach_and_verify_remote(
                    session_id,
                    remote_ref,
                    Some(matched_scope),
                    &merged,
                    RemoteConnectOutcome::Reattached,
                )
                .await
            {
                Ok(connected) => return Ok(connected),
                Err(err) => {
                    log::warn!(
                        "remote session {} connect: DHT-published actor failed validation ({}); \
                         falling back to node-manager resume",
                        session_id,
                        err,
                    );
                }
            }
        }

        // Stage 2: node-manager resume.
        let resume_span = tracing::info_span!(
            "remote.session.resume",
            session_id,
            node_id = node_id.as_str(),
        );
        let resumed = async {
            let node_manager = self.find_node_manager(&node_id).await.map_err(|e| {
                RemoteSessionConnectError::NodeUnavailable {
                    node_id: node_id.clone(),
                    transport: None,
                    message: e.message.clone(),
                }
            })?;
            self.resume_remote_session_typed(&node_manager, session_id)
                .await
                .map_err(|e| {
                    // The host explicitly reports the session is absent (plain
                    // `SessionNotFound` from resume, or `RemoteSessionNotFound`
                    // from lookup paths): structural answer, not a transient
                    // recovery failure. Both map to ACP -32002 like the pre-
                    // coordinator pass-through did.
                    if matches!(
                        e,
                        AgentError::SessionNotFound { .. }
                            | AgentError::RemoteSessionNotFound { .. }
                    ) {
                        RemoteSessionConnectError::SessionNotFoundOnHost {
                            session_id: session_id.to_string(),
                            node_id: node_id.clone(),
                            message: e.to_string(),
                        }
                    } else {
                        let transport = e.transport_failure().cloned();
                        RemoteSessionConnectError::RecoveryFailed {
                            transport,
                            message: e.to_string(),
                        }
                    }
                })
        }
        .instrument(resume_span)
        .await?;

        log::info!(
            "remote session {} connect: node-manager resume returned handoff on node {} \
             (fresh cwd={}, fresh title={})",
            session_id,
            node_id,
            resumed.cwd.is_some(),
            resumed.title.is_some(),
        );

        let remote_ref = self
            .resolve_handoff(session_id, resumed.handoff)
            .await
            .map_err(|e| {
                if e.code == agent_client_protocol::ErrorCode::ResourceNotFound {
                    RemoteSessionConnectError::SessionNotFoundOnHost {
                        session_id: session_id.to_string(),
                        node_id: node_id.clone(),
                        message: format!("handoff resolution failed after resume: {}", e.message),
                    }
                } else {
                    RemoteSessionConnectError::RecoveryFailed {
                        transport: None,
                        message: format!("handoff resolution failed after resume: {}", e.message),
                    }
                }
            })?;

        let merged = self
            .persist_connect_bookmark(
                session_id,
                &node_id,
                &peer_label,
                resumed.cwd,
                resumed.title,
                Some(resumed.created_at),
            )
            .await?;
        self.attach_and_verify_remote(
            session_id,
            remote_ref,
            None,
            &merged,
            RemoteConnectOutcome::Resumed,
        )
        .await
    }

    /// Install an attachment via the registry and verify the control path
    /// with a bounded health check. On health failure the candidate is torn
    /// down (bookmark preserved) and a typed error returned, so a
    /// half-connected attachment is never reported as connected.
    async fn attach_and_verify_remote(
        &self,
        session_id: &str,
        remote_ref: kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        preferred_scope: Option<crate::agent::remote::scope::MeshScopeId>,
        bookmark: &RemoteSessionBookmark,
        outcome: RemoteConnectOutcome,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        let (context, expected_attachment_id) = {
            let registry = self.registry.lock().await;
            (
                registry.remote_attachment_prepare_context(),
                registry.remote_attachment_id(session_id),
            )
        };
        let backfill_sink = context.event_sink.clone();
        let prepare_span = tracing::info_span!(
            "remote.session.attach.prepare",
            session_id,
            node_id = bookmark.node_id.as_str(),
            previous_attachment_id = ?expected_attachment_id,
        );
        let prepared = crate::agent::session_registry::prepare_remote_attachment(
            context,
            session_id.to_string(),
            remote_ref,
            bookmark.peer_label.clone(),
            self.mesh(),
            preferred_scope,
            Some(bookmark.node_id.clone()),
        )
        .instrument(prepare_span)
        .await
        .map_err(|error| RemoteSessionConnectError::HealthCheckFailed {
            session_id: session_id.to_string(),
            message: error.to_string(),
        })?;
        let session_ref = prepared.session_ref().clone();
        let attachment_id = prepared.attachment_id();

        let health_timeout = Self::remote_connect_health_timeout();
        match tokio::time::timeout(health_timeout, session_ref.get_mode()).await {
            Ok(Ok(_mode)) => {}
            Ok(Err(error)) => {
                log::warn!(
                    "remote session {} connect: candidate attachment_id={} failed health check: {}",
                    session_id,
                    attachment_id,
                    error,
                );
                crate::agent::session_registry::abort_prepared_remote_attachment(prepared).await;
                return Err(RemoteSessionConnectError::HealthCheckFailed {
                    session_id: session_id.to_string(),
                    message: error.to_string(),
                });
            }
            Err(_) => {
                log::warn!(
                    "remote session {} connect: candidate attachment_id={} health check timed out after {}ms",
                    session_id,
                    attachment_id,
                    health_timeout.as_millis(),
                );
                crate::agent::session_registry::abort_prepared_remote_attachment(prepared).await;
                return Err(RemoteSessionConnectError::HealthCheckFailed {
                    session_id: session_id.to_string(),
                    message: format!(
                        "control health check timed out after {}ms",
                        health_timeout.as_millis()
                    ),
                });
            }
        }

        let commit_span = tracing::info_span!(
            "remote.session.attach.commit",
            session_id,
            node_id = bookmark.node_id.as_str(),
            attachment_id,
            previous_attachment_id = ?expected_attachment_id,
        );
        let commit = async {
            let mut registry = self.registry.lock().await;
            registry.install_remote_attachment(prepared, expected_attachment_id)
        }
        .instrument(commit_span)
        .await;
        let old = match commit {
            Ok(old) => old,
            Err((prepared, conflict)) => {
                crate::agent::session_registry::abort_prepared_remote_attachment(prepared).await;
                return Err(RemoteSessionConnectError::RecoveryFailed {
                    transport: None,
                    message: format!(
                        "attachment generation changed before commit (expected={:?}, current={:?})",
                        conflict.expected_attachment_id, conflict.current_attachment_id
                    ),
                });
            }
        };

        if let Some(old) = old {
            crate::agent::session_registry::cleanup_installed_remote_attachment(old, true).await;
        }
        log::info!(
            "remote session {} connect: attachment committed (attachment_id={}, outcome={}, node={})",
            session_id,
            attachment_id,
            outcome.as_str(),
            bookmark.node_id,
        );

        // Cursor-based backfill (plan §16): the live subscription was
        // established during prepare, so newly generated events cannot be
        // lost; historical pages after the last persisted source cursor are
        // recovered with overlap-safe deduplication. Spawn-and-forget so
        // connect latency never waits on backfill volume.
        tokio::spawn(
            crate::agent::remote::event_backfill::backfill_remote_events(
                backfill_sink,
                session_ref.clone(),
                session_id.to_string(),
                bookmark.node_id.clone(),
                bookmark.peer_label.clone(),
                attachment_id,
            ),
        );

        Ok(ConnectedRemoteSession {
            session_ref,
            attachment_id,
            bookmark: Some(bookmark.clone()),
            outcome,
        })
    }

    /// Merge confirmed node info into the durable bookmark and persist it.
    /// Awaited: first-time identity must be durable before the attachment is
    /// reported connected (plan §6). Existing `created_at` is preserved by
    /// `merge_confirmed`; a first write uses the resume-reported creation
    /// time when available.
    async fn persist_connect_bookmark(
        &self,
        session_id: &str,
        node_id: &str,
        peer_label: &str,
        cwd: Option<String>,
        title: Option<String>,
        fallback_created_at: Option<i64>,
    ) -> Result<RemoteSessionBookmark, RemoteSessionConnectError> {
        let store = self.config.provider.history_store();
        let existing = store
            .get_remote_session_bookmark(session_id)
            .await
            .map_err(|e| RemoteSessionConnectError::BookmarkLookupFailed {
                session_id: session_id.to_string(),
                message: e.to_string(),
            })?;
        let update = RemoteSessionBookmarkUpdate {
            node_id: Some(node_id.to_string()),
            peer_label: Some(peer_label.to_string()),
            cwd: cwd.clone(),
            title: title.clone(),
        };
        let merged = match existing {
            Some(bookmark) => {
                log::debug!(
                    "remote session {} connect: merging bookmark metadata",
                    session_id
                );
                bookmark.merge_confirmed(update)
            }
            None => RemoteSessionBookmark {
                session_id: session_id.to_string(),
                node_id: node_id.to_string(),
                peer_label: peer_label.to_string(),
                cwd,
                created_at: fallback_created_at.unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                }),
                title,
            },
        };
        // A persistence failure is local-storage, not transport: the control
        // path is healthy, so keep the attachment but log loudly.
        if let Err(e) = store.save_remote_session_bookmark(&merged).await {
            log::warn!(
                "remote session {} connect: bookmark persistence failed (attachment kept): {}",
                session_id,
                e
            );
        }
        Ok(merged)
    }

    /// Bookmark for the reuse fast path. The durable store is authoritative;
    /// a lookup failure or absence degrades to metadata synthesized from the
    /// installed attachment without rewriting durable state.
    async fn load_bookmark_for_reuse(
        &self,
        session_id: &str,
        session_ref: &SessionActorRef,
    ) -> Option<RemoteSessionBookmark> {
        let store = self.config.provider.history_store();
        match store.get_remote_session_bookmark(session_id).await {
            Ok(Some(bookmark)) => Some(bookmark),
            Ok(None) => match session_ref {
                SessionActorRef::Remote {
                    peer_label,
                    remote_node_id,
                    ..
                } => remote_node_id
                    .as_ref()
                    .map(|node_id| RemoteSessionBookmark {
                        session_id: session_id.to_string(),
                        node_id: node_id.clone(),
                        peer_label: peer_label.clone(),
                        cwd: None,
                        created_at: 0,
                        title: None,
                    }),
                SessionActorRef::Local(_) => None,
            },
            Err(e) => {
                log::warn!(
                    "remote session {} connect: bookmark lookup failed on reuse path \
                     (continuing with installed attachment): {}",
                    session_id,
                    e
                );
                None
            }
        }
    }

    /// Scoped DHT lookup for a session actor across all active scopes.
    async fn lookup_remote_session_actor(
        mesh: &crate::agent::remote::MeshHandle,
        session_id: &str,
    ) -> Option<(
        kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        crate::agent::remote::scope::MeshScopeId,
    )> {
        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        for scope in runtime.active_scopes() {
            let dht_name = crate::agent::remote::scope::scoped_session(&scope, session_id);
            match runtime
                .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name)
                .await
            {
                Ok(Some(found)) => {
                    log::debug!(
                        "remote session {} connect: DHT publication found (scope matched)",
                        session_id
                    );
                    return Some((found, scope));
                }
                Ok(None) => {}
                Err(e) => {
                    log::debug!(
                        "remote session {} connect: scoped DHT lookup error: {}; trying next scope",
                        session_id,
                        e
                    );
                }
            }
        }
        None
    }

    /// `ResumeRemoteSession` with Phase-2 typed failure classification so
    /// `RecoveryFailed` carries a real [`RemoteTransportFailure`] instead of a
    /// string.
    async fn resume_remote_session_typed(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: &str,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, AgentError> {
        use crate::agent::remote::ResumeRemoteSession;
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &ResumeRemoteSession {
                session_id: session_id.to_string(),
            },
            Self::remote_request_timeout(),
        )
        .await
        .map_err(
            |error| match querymt_remote::classify_remote_send_error(error) {
                Ok(failure) => AgentError::from_transport_failure(failure),
                Err(handler_error) => handler_error,
            },
        )
    }

    /// Whether a remote connect/recovery attempt is currently in flight for
    /// the session (a held or queued single-flight gate entry).
    pub(crate) fn remote_connect_in_flight(&self, session_id: &str) -> bool {
        self.remote_connect_gates.lock().contains_key(session_id)
    }

    /// Bounded health-check timeout for candidate attachments.
    pub(super) fn remote_connect_health_timeout() -> Duration {
        let timeout_ms = std::env::var("QUERYMT_REMOTE_CONNECT_HEALTH_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(5_000);
        Duration::from_millis(timeout_ms)
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
    use crate::test_utils::{
        MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
    };
    use kameo::actor::Spawn;
    use querymt::LLMParams;

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
        let mut builder = AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            storage.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .with_tool_policy(ToolPolicy::ProviderOnly);
        if let Some(repo) = storage.schedule_repository() {
            builder = builder.with_schedule_repository(repo);
        }
        let config = Arc::new(builder.build());
        (LocalAgentHandle::from_config(config), storage, temp_dir)
    }

    fn test_bookmark(session_id: &str, node_id: &str) -> RemoteSessionBookmark {
        RemoteSessionBookmark {
            session_id: session_id.to_string(),
            node_id: node_id.to_string(),
            peer_label: "test-node".to_string(),
            cwd: Some("/remote/work".to_string()),
            created_at: 1_700_000_000,
            title: Some("remote session".to_string()),
        }
    }

    fn gate_map_len(map: &RemoteConnectGateMap) -> usize {
        map.lock().len()
    }

    // ── Pure/type-level tests ───────────────────────────────────────────

    #[test]
    fn reason_codes_are_stable() {
        assert_eq!(RemoteConnectReason::Open.as_str(), "open");
        assert_eq!(
            RemoteConnectReason::ExplicitReconnect.as_str(),
            "explicit_reconnect"
        );
        assert_eq!(
            RemoteConnectReason::OperationRecovery.as_str(),
            "operation_recovery"
        );
        assert_eq!(
            RemoteConnectReason::StartupReattach.as_str(),
            "startup_reattach"
        );
        assert_eq!(
            RemoteConnectReason::ExtensionAttach.as_str(),
            "extension_attach"
        );
        assert_eq!(RemoteConnectOutcome::Reused.as_str(), "reused");
        assert_eq!(RemoteConnectOutcome::Reattached.as_str(), "reattached");
        assert_eq!(RemoteConnectOutcome::Resumed.as_str(), "resumed");
    }

    #[test]
    fn default_replace_policy_forces_only_reconnect_and_operation_recovery() {
        assert_eq!(
            RemoteReplacePolicy::ReuseIfPresent,
            RemoteConnectReason::Open.default_replace_policy()
        );
        assert_eq!(
            RemoteReplacePolicy::ReuseIfPresent,
            RemoteConnectReason::StartupReattach.default_replace_policy()
        );
        assert_eq!(
            RemoteReplacePolicy::ReuseIfPresent,
            RemoteConnectReason::ExtensionAttach.default_replace_policy()
        );
        assert_eq!(
            RemoteReplacePolicy::ReplaceCurrent,
            RemoteConnectReason::ExplicitReconnect.default_replace_policy()
        );
        assert_eq!(
            RemoteReplacePolicy::ReplaceCurrent,
            RemoteConnectReason::OperationRecovery.default_replace_policy()
        );
    }

    #[test]
    fn connect_budget_defaults_are_bounded_and_reason_specific() {
        // Only assert defaults when the operator overrides are absent.
        if std::env::var("QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS").is_err() {
            assert_eq!(
                RemoteConnectReason::Open.connect_budget(),
                Duration::from_millis(5_000)
            );
        }
        if std::env::var("QUERYMT_REMOTE_CONNECT_RECONNECT_BUDGET_MS").is_err()
            && std::env::var("QUERYMT_REMOTE_CONNECT_OPERATION_BUDGET_MS").is_err()
        {
            // Explicit reconnect gets a longer budget than open-time recovery.
            assert!(
                RemoteConnectReason::ExplicitReconnect.connect_budget()
                    > RemoteConnectReason::Open.connect_budget()
            );
            assert_eq!(
                RemoteConnectReason::OperationRecovery.connect_budget(),
                Duration::from_millis(10_000)
            );
        }
    }

    #[test]
    fn error_codes_are_stable_and_retriability_is_conservative() {
        assert_eq!(
            RemoteSessionConnectError::BookmarkMissing {
                session_id: "s".into()
            }
            .code(),
            "session_bookmark_missing"
        );
        assert_eq!(
            RemoteSessionConnectError::LocationConflict {
                session_id: "s".into(),
                message: "m".into()
            }
            .code(),
            "session_location_conflict"
        );
        assert_eq!(
            RemoteSessionConnectError::MeshUnavailable {
                node_id: "n".into()
            }
            .code(),
            "remote_node_unavailable"
        );
        assert_eq!(
            RemoteSessionConnectError::RecoveryFailed {
                transport: None,
                message: "m".into()
            }
            .code(),
            "remote_recovery_failed"
        );
        assert_eq!(
            RemoteSessionConnectError::TimedOut { budget_ms: 1_000 }.code(),
            "remote_recovery_timeout"
        );
        assert_eq!(
            RemoteSessionConnectError::SessionNotFoundOnHost {
                session_id: "s".into(),
                node_id: "n".into(),
                message: "m".into()
            }
            .code(),
            "remote_session_not_found_on_host"
        );

        // Structural failures must not be retried blindly.
        assert!(
            !RemoteSessionConnectError::BookmarkMissing {
                session_id: "s".into()
            }
            .is_retriable()
        );
        assert!(
            !RemoteSessionConnectError::LocationConflict {
                session_id: "s".into(),
                message: "m".into()
            }
            .is_retriable()
        );
        assert!(
            !RemoteSessionConnectError::SessionNotFoundOnHost {
                session_id: "s".into(),
                node_id: "n".into(),
                message: "m".into()
            }
            .is_retriable()
        );
        assert!(
            RemoteSessionConnectError::MeshUnavailable {
                node_id: "n".into()
            }
            .is_retriable()
        );
    }

    #[test]
    fn acp_error_carries_structured_payload() {
        let failure = RemoteTransportFailure {
            kind: querymt_remote::RemoteTransportFailureKind::ConnectionClosed,
            delivery: querymt_remote::DeliveryCertainty::Unknown,
            message: "closed".to_string(),
        };
        let err = RemoteSessionConnectError::NodeUnavailable {
            node_id: "node-1".into(),
            transport: Some(failure),
            message: "dial failed".into(),
        };
        let acp = err.to_acp_error();
        let data = acp.data.expect("structured data present");
        assert_eq!(data["category"], "remote_session_connect");
        assert_eq!(data["code"], "remote_node_unavailable");
        assert_eq!(data["retriable"], true);
        assert_eq!(data["transport_kind"], "connection_closed");
        assert_eq!(data["transport_delivery"], "unknown");
    }

    #[test]
    fn host_side_not_found_maps_to_resource_not_found_acp() {
        let err = RemoteSessionConnectError::SessionNotFoundOnHost {
            session_id: "s-1".into(),
            node_id: "n-1".into(),
            message: "Session not found: s-1".into(),
        };
        let acp = err.to_acp_error();
        assert_eq!(acp.code, agent_client_protocol::ErrorCode::ResourceNotFound);
        assert!(acp.message.contains("not found"));
        assert!(acp.message.contains("s-1"));
        let data = acp.data.expect("structured data present");
        assert_eq!(data["code"], "remote_session_not_found_on_host");
        assert_eq!(data["session_id"], "s-1");
        assert_eq!(data["node_id"], "n-1");
        assert_eq!(data["retriable"], false);
    }

    #[test]
    fn transport_failure_accessor_only_exposes_transport_variants() {
        let failure = RemoteTransportFailure {
            kind: querymt_remote::RemoteTransportFailureKind::ActorUnavailable,
            delivery: querymt_remote::DeliveryCertainty::NotDelivered,
            message: "m".into(),
        };
        let err = RemoteSessionConnectError::RecoveryFailed {
            transport: Some(failure),
            message: "m".into(),
        };
        let extracted = err.transport_failure().expect("transport present");
        assert!(extracted.proven_not_delivered());
        assert!(
            RemoteSessionConnectError::BookmarkMissing {
                session_id: "s".into()
            }
            .transport_failure()
            .is_none()
        );
    }

    // ── Single-flight gate tests ────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gate_serializes_same_session_and_cleans_up() {
        let map: RemoteConnectGateMap = Default::default();
        let gate_a = RemoteConnectGate::acquire(&map, "session-1").await;
        assert_eq!(gate_map_len(&map), 1);

        let map2 = map.clone();
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let waiter = tokio::spawn(async move {
            let _gate_b = RemoteConnectGate::acquire(&map2, "session-1").await;
            let _ = tx.send(());
        });

        // The follower must block while gate_a is held.
        let blocked = tokio::time::timeout(Duration::from_millis(100), &mut rx).await;
        assert!(blocked.is_err(), "follower must wait on the held gate");
        assert_eq!(gate_map_len(&map), 1, "session shares one gate entry");

        drop(gate_a);
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("follower acquires after release")
            .ok();
        waiter.await.expect("waiter task");
        // Both guards dropped: map entry removed.
        assert_eq!(gate_map_len(&map), 0, "gate entry removed after last drop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gate_allows_unrelated_sessions_concurrently() {
        let map: RemoteConnectGateMap = Default::default();
        let gate_a = RemoteConnectGate::acquire(&map, "session-1").await;
        // Acquiring a different session must not block.
        let gate_b = tokio::time::timeout(
            Duration::from_millis(200),
            RemoteConnectGate::acquire(&map, "session-2"),
        )
        .await
        .expect("unrelated session proceeds concurrently");
        assert_eq!(gate_map_len(&map), 2);
        drop(gate_a);
        drop(gate_b);
        assert_eq!(gate_map_len(&map), 0);
    }

    // ── Coordinator error-path tests (no mesh) ──────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_without_hint_or_bookmark_is_bookmark_missing() {
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        let err = handle
            .ensure_remote_session_connected("missing-session", None, RemoteConnectReason::Open)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "session_bookmark_missing");
        assert!(!err.is_retriable());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_without_mesh_is_typed_node_unavailable_and_preserves_bookmark() {
        let (handle, storage, _tmp) = handle_with_real_storage().await;
        let store = storage.session_store();
        let bookmark = test_bookmark("remote-sess-1", "node-abc");
        store
            .save_remote_session_bookmark(&bookmark)
            .await
            .expect("save bookmark");

        let err = handle
            .ensure_remote_session_connected("remote-sess-1", None, RemoteConnectReason::Open)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "remote_node_unavailable");
        assert!(err.is_retriable());

        // The durable identity survives every connect failure.
        let stored = store
            .get_remote_session_bookmark("remote-sess-1")
            .await
            .expect("lookup")
            .expect("bookmark preserved");
        assert_eq!(stored, bookmark);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_refuses_to_replace_local_actor_registration() {
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        let session_id = "occupied-session";
        let actor = SessionActor::new(
            handle.config.clone(),
            session_id.to_string(),
            SessionRuntime::new(
                None,
                std::collections::HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        );
        let actor_ref = SessionActor::spawn(actor);
        handle
            .registry
            .lock()
            .await
            .insert(session_id.to_string(), actor_ref);

        let err = handle
            .ensure_remote_session_connected(
                session_id,
                Some("node-abc"),
                RemoteConnectReason::Open,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "session_location_conflict");
        // The local registration is untouched.
        assert!(handle.registry.lock().await.get(session_id).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_refuses_local_row_bookmark_conflict() {
        let (handle, storage, _tmp) = handle_with_real_storage().await;
        let store = storage.session_store();
        let session = store
            .create_session(None, None, None, None)
            .await
            .expect("create local session");

        let err = handle
            .ensure_remote_session_connected(
                &session.public_id,
                Some("node-abc"),
                RemoteConnectReason::Open,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "session_location_conflict");
    }
}
