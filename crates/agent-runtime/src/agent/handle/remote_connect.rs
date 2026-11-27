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

/// Shared slot used to hand a prepared-but-uncommitted attachment candidate
/// back to the connect coordinator when the connect budget cancels the
/// in-flight recovery future. Parking the candidate keeps it reachable after
/// the future is dropped, so the coordinator can run the full async teardown
/// (remote forwarder unsubscribe + relay unlink) that plain `Drop` cannot.
type PreparedCandidateSlot = std::sync::Arc<
    std::sync::Mutex<Option<crate::agent::session_registry::PreparedRemoteAttachment>>,
>;

/// How long a first-time attach may spend looking up the owner's display
/// label; the node id is used when the lookup takes longer.
const REMOTE_NODE_LABEL_BUDGET: Duration = Duration::from_millis(1_000);

/// A candidate attachment that passed its health check and whose bookmark is
/// saved, waiting to be installed outside the connect budget.
struct VerifiedRemoteAttachment {
    prepared: crate::agent::session_registry::PreparedRemoteAttachment,
    expected_attachment_id: Option<u64>,
    outcome: RemoteConnectOutcome,
    bookmark: RemoteSessionBookmark,
    backfill_sink: std::sync::Arc<querymt_agent_services::event_sink::EventSink>,
    sync_cursor: Option<i64>,
    windowed: bool,
}

/// A session actor found on its owner, and how it was found.
struct LocatedRemoteActor {
    remote_ref: kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
    preferred_scope: Option<crate::agent::remote::scope::MeshScopeId>,
    outcome: RemoteConnectOutcome,
}

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
    /// Attaching the session a delegation opened on a remote member.
    Delegation,
    /// Extension-level attach request (`remote/attachSession`,
    /// `remote/createSession`). The attach of a session that already has a
    /// bookmark is a repair reconnect: it runs as [`Self::ExplicitReconnect`]
    /// with [`RemoteReplacePolicy::ReplaceCurrent`] (decided under the gate).
    ExtensionAttach,
}

impl RemoteConnectReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::ExplicitReconnect => "explicit_reconnect",
            Self::OperationRecovery => "operation_recovery",
            Self::StartupReattach => "startup_reattach",
            Self::Delegation => "delegation",
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
            Self::Open | Self::StartupReattach | Self::ExtensionAttach | Self::Delegation => {
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
            Self::Delegation => ("QUERYMT_REMOTE_CONNECT_DELEGATION_BUDGET_MS", 10_000),
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
#[derive(Debug, Clone)]
pub(crate) struct ConnectedRemoteSession {
    pub(crate) session_ref: SessionActorRef,
    /// Attachment generation (relay actor id) installed/observed by this call.
    pub(crate) attachment_id: u64,
    /// Durable identity. `None` only when the store lookup failed on the
    /// reuse path (logged there); fresh attaches always persist first.
    pub(crate) bookmark: Option<RemoteSessionBookmark>,
    pub(crate) outcome: RemoteConnectOutcome,
}

impl ConnectedRemoteSession {
    /// Whether `error`, returned by a call through this attachment, shows that
    /// a reused attachment no longer reaches a live actor (for example after
    /// the owner restarted) so that replacing it may help.
    ///
    /// A freshly installed attachment passed its health check moments ago;
    /// its failures are reported as they are instead of reconnecting again.
    pub(crate) fn reused_attachment_failed(&self, error: &AgentError) -> bool {
        self.outcome == RemoteConnectOutcome::Reused && attachment_unreachable(error)
    }
}

/// Whether `error` is a transport failure that a new attachment may fix.
pub(crate) fn attachment_unreachable(error: &AgentError) -> bool {
    error
        .transport_failure()
        .is_some_and(RemoteTransportFailure::is_retryable_kind)
}

/// Why [`LocalAgentHandle::call_through_remote_attachment`] failed.
#[derive(Debug)]
pub(crate) enum RemoteAttachmentCallError<E> {
    /// The dead attachment could not be replaced.
    Replace(RemoteSessionConnectError),
    /// The call failed (on the replacement, when there was one).
    Failed(E),
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
    /// The bookmark could not be persisted before the attachment was
    /// committed; refused rather than connecting without durable identity.
    BookmarkSaveFailed { session_id: String, message: String },
    /// Local durable/actor state conflicts with a remote attachment.
    LocationConflict { session_id: String, message: String },
    /// Mesh runtime is not bootstrapped locally, so no remote work is possible.
    MeshUnavailable { node_id: String },
    /// The owner rejected this peer's access to the session.
    AccessDenied,
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
    HealthCheckFailed {
        session_id: String,
        transport: Option<RemoteTransportFailure>,
        message: String,
    },
    /// The host is reachable but explicitly reports the session does not exist
    /// there (resume-side `RemoteSessionNotFound`). Kept distinct so callers
    /// get a not-found (-32002) response, not a transient-looking failure.
    SessionNotFoundOnHost {
        session_id: String,
        node_id: String,
        message: String,
    },
    /// Recent attempts for this session failed; the caller must wait before
    /// retrying so a dead remote session cannot be hammered in a hot loop.
    CoolingDown {
        session_id: String,
        retry_after_ms: u64,
        owner_unreachable: bool,
    },
}

impl RemoteSessionConnectError {
    /// Stable machine-readable code (future protocol codes per plan §13).
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::BookmarkMissing { .. } => "session_bookmark_missing",
            Self::BookmarkLookupFailed { .. } => "session_bookmark_lookup_failed",
            Self::BookmarkSaveFailed { .. } => "session_bookmark_save_failed",
            Self::LocationConflict { .. } => "session_location_conflict",
            Self::AccessDenied => "mesh_access_denied",
            Self::MeshUnavailable { .. } => "remote_node_unavailable",
            Self::NodeUnavailable { .. } => "remote_node_unavailable",
            Self::RecoveryFailed { .. } => "remote_recovery_failed",
            Self::TimedOut { .. } => "remote_recovery_timeout",
            Self::HealthCheckFailed { .. } => "remote_recovery_failed",
            Self::SessionNotFoundOnHost { .. } => "remote_session_not_found_on_host",
            Self::CoolingDown { .. } => "remote_connect_cooling_down",
        }
    }

    /// Whether this failure should extend the per-session retry cooldown.
    fn counts_toward_cooldown(&self) -> bool {
        matches!(
            self,
            Self::NodeUnavailable { .. }
                | Self::RecoveryFailed { .. }
                | Self::TimedOut { .. }
                | Self::HealthCheckFailed { .. }
        )
    }

    /// Whether the failure means the owner could not be reached, as opposed
    /// to the owner (or local state) giving a definite answer. A load may
    /// still be served from a local copy in the former case.
    pub(crate) fn owner_unreachable(&self) -> bool {
        match self {
            Self::MeshUnavailable { .. } | Self::TimedOut { .. } => true,
            Self::NodeUnavailable { transport, .. } => transport
                .as_ref()
                .is_none_or(RemoteTransportFailure::owner_unreachable),
            Self::RecoveryFailed { transport, .. } | Self::HealthCheckFailed { transport, .. } => {
                transport
                    .as_ref()
                    .is_some_and(RemoteTransportFailure::owner_unreachable)
            }
            Self::CoolingDown {
                owner_unreachable, ..
            } => *owner_unreachable,
            _ => false,
        }
    }

    /// Typed transport failure for retry/invalidation decisions (Phase 6).
    /// `None` when the failure is local/structural rather than transport.
    pub(crate) fn transport_failure(&self) -> Option<&RemoteTransportFailure> {
        match self {
            Self::NodeUnavailable { transport, .. }
            | Self::RecoveryFailed { transport, .. }
            | Self::HealthCheckFailed { transport, .. } => transport.as_ref(),
            _ => None,
        }
    }

    /// Whether a caller may reasonably retry this connect attempt. Local
    /// structural errors (missing identity, conflicts) are not retriable.
    pub(crate) fn is_retriable(&self) -> bool {
        match self {
            Self::BookmarkMissing { .. }
            | Self::BookmarkLookupFailed { .. }
            | Self::BookmarkSaveFailed { .. }
            | Self::LocationConflict { .. }
            | Self::SessionNotFoundOnHost { .. }
            | Self::AccessDenied => false,
            Self::MeshUnavailable { .. }
            | Self::NodeUnavailable { .. }
            | Self::RecoveryFailed { .. }
            | Self::TimedOut { .. }
            | Self::CoolingDown { .. }
            | Self::HealthCheckFailed { .. } => true,
        }
    }

    pub(crate) fn to_acp_error(&self) -> agent_client_protocol::Error {
        let code = match self {
            Self::BookmarkMissing { .. }
            | Self::LocationConflict { .. }
            | Self::SessionNotFoundOnHost { .. } => -32002,
            Self::AccessDenied => -32000,
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
            | Self::BookmarkSaveFailed { session_id, .. }
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
            Self::CoolingDown {
                session_id,
                retry_after_ms,
                ..
            } => {
                data["session_id"] = serde_json::Value::String(session_id.clone());
                data["retry_after_ms"] = serde_json::Value::from(*retry_after_ms);
            }
            Self::AccessDenied | Self::RecoveryFailed { .. } | Self::TimedOut { .. } => {}
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
            Self::BookmarkSaveFailed {
                session_id,
                message,
            } => write!(
                f,
                "failed to persist bookmark for remote session '{session_id}': {message}"
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
            Self::AccessDenied => write!(f, "mesh access denied"),
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
            Self::CoolingDown {
                session_id,
                retry_after_ms,
                ..
            } => write!(
                f,
                "remote session '{session_id}' recently failed to connect; retry in {retry_after_ms}ms"
            ),
            Self::HealthCheckFailed {
                session_id,
                message,
                ..
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
    pub(crate) peer_label: Option<&'a str>,
    pub(crate) preferred_scope: Option<crate::agent::remote::scope::MeshScopeId>,
    pub(crate) reason: RemoteConnectReason,
    pub(crate) replace: RemoteReplacePolicy,
    /// Pre-resolved handoff from a create/resume response. Skips DHT lookup
    /// and node-manager resume entirely.
    pub(crate) handoff: Option<crate::agent::remote::node_manager::SessionHandoff>,
    /// The caller will load a recent window: a fresh attachment fetches the
    /// recent tail first and fills older history afterwards.
    pub(crate) windowed: bool,
    /// Recorded on the bookmark when the attach is confirmed. `Delegation`
    /// marks a session a delegation opened.
    pub(crate) fork_origin: Option<crate::session::domain::ForkOrigin>,
}

// ── Per-session failure cooldown ────────────────────────────────────────

const CONNECT_COOLDOWN_BASE_MS: u64 = 2_000;
const CONNECT_COOLDOWN_MAX_MS: u64 = 60_000;
/// An explicit user-driven reconnect waits at most this long after a failure.
const EXPLICIT_RECONNECT_COOLDOWN_MAX_MS: u64 = 2_000;

/// Failure state for one session: when it last failed and how long to back off.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectCooldown {
    failures: u32,
    failed_at: Instant,
    wait: Duration,
    owner_unreachable: bool,
}

/// Per-session retry cooldowns, independent of the single-flight gates.
pub(crate) type RemoteConnectCooldownMap =
    Arc<parking_lot::Mutex<HashMap<String, ConnectCooldown>>>;

/// Exponential backoff (2s, 4s, ... capped at 60s) with up to 20% jitter.
fn connect_cooldown_wait(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    let base = CONNECT_COOLDOWN_BASE_MS
        .saturating_mul(1u64 << exponent)
        .min(CONNECT_COOLDOWN_MAX_MS);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = nanos % (base / 5 + 1);
    Duration::from_millis((base + jitter).min(CONNECT_COOLDOWN_MAX_MS))
}

// ── Per-session single-flight gate ──────────────────────────────────────

/// Per-session recovery gates. Keeps at most one connect/recovery attempt in
/// flight per session; unrelated sessions proceed concurrently. Distinct from
/// the registry mutex: the gate is held across network work, the registry
/// mutex never is (beyond the pre-existing registry internals — Phase 4).
pub(crate) type RemoteConnectGateMap =
    Arc<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

/// RAII holder for one acquisition of a per-session recovery gate.
/// References and the map entry are retired under the same lock so a woken
/// follower cannot race a still-dropping holder or a newly arriving waiter.
pub(crate) struct RemoteConnectGate {
    session_id: String,
    lock: Option<Arc<tokio::sync::Mutex<()>>>,
    map: RemoteConnectGateMap,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
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
            lock: Some(lock),
            map: map.clone(),
            guard: Some(owned_guard),
        }
    }
}

impl Drop for RemoteConnectGate {
    fn drop(&mut self) {
        let mut map = self.map.lock();
        // Release all of this holder's references while the map is locked.
        // A woken follower cannot retire the entry before these refs disappear.
        drop(self.guard.take());
        drop(self.lock.take());
        if map
            .get(&self.session_id)
            .is_some_and(|entry| Arc::strong_count(entry) == 1)
        {
            map.remove(&self.session_id);
        }
    }
}

/// What one connect attempt owes once it has a result: the span fields, the
/// cooldown bookkeeping and, last, the release of the session's gate.
///
/// The attempt owns it until the attachment is committed. The commit then
/// runs in a detached task (see [`LocalAgentHandle::commit_remote_attachment`])
/// and takes the scope along, so a caller that is cancelled meanwhile neither
/// frees the gate for a racing connect before the install is done nor loses
/// the outcome.
struct ConnectScope {
    session_id: String,
    started: Instant,
    span: tracing::Span,
    cooldowns: RemoteConnectCooldownMap,
    gate: RemoteConnectGate,
}

impl ConnectScope {
    /// Record the outcome before the gate is released; otherwise the next
    /// failed attempt can race ahead of its predecessor's cooldown.
    fn finish(self, result: &Result<ConnectedRemoteSession, RemoteSessionConnectError>) {
        self.span
            .record("elapsed_ms", self.started.elapsed().as_millis() as u64);
        record_connect_outcome(&self.cooldowns, &self.session_id, result);
        match result {
            Ok(connected) => {
                self.span.record("outcome", connected.outcome.as_str());
                self.span.record("attachment_id", connected.attachment_id);
            }
            Err(err) => {
                self.span.record("error", err.code());
            }
        }
        drop(self.gate);
    }
}

/// Success clears the cooldown; transport-level failures extend it.
fn record_connect_outcome(
    cooldowns: &RemoteConnectCooldownMap,
    session_id: &str,
    result: &Result<ConnectedRemoteSession, RemoteSessionConnectError>,
) {
    let mut cooldowns = cooldowns.lock();
    match result {
        Ok(connected) if connected.outcome != RemoteConnectOutcome::Reused => {
            cooldowns.remove(session_id);
        }
        Err(error) if error.counts_toward_cooldown() => {
            let failures = cooldowns
                .get(session_id)
                .map_or(1, |state| state.failures.saturating_add(1));
            cooldowns.insert(
                session_id.to_string(),
                ConnectCooldown {
                    failures,
                    failed_at: Instant::now(),
                    wait: connect_cooldown_wait(failures),
                    owner_unreachable: error.owner_unreachable(),
                },
            );
        }
        _ => {}
    }
}

// ── Coordinator ─────────────────────────────────────────────────────────

impl LocalAgentHandle {
    /// Plan §2 entry point: ensure a connected attachment exists for a remote
    /// session, recovering it when necessary, with the reason's default
    /// replacement policy (operation recovery, resume, history sync and
    /// startup reattach).
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
                peer_label: None,
                preferred_scope: None,
                reason,
                replace: reason.default_replace_policy(),
                handoff: None,
                windowed: false,
                fork_origin: None,
            },
        )
        .await
    }

    /// Replace an attachment that failed at the transport level.
    ///
    /// Removes generation `attachment_id` (if it is still the installed one)
    /// and connects again with [`RemoteReplacePolicy::ReplaceIfMatches`], so
    /// a newer generation installed meanwhile by another caller is reused
    /// instead of being torn down. Callers decide whether to repeat their
    /// request on the returned attachment.
    pub(crate) async fn replace_failed_remote_attachment(
        &self,
        session_id: &str,
        attachment_id: u64,
        node_hint: Option<&str>,
        windowed: bool,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        let stale = {
            let mut registry = self.registry.lock().await;
            registry.invalidate_remote_attachment_if_current(session_id, attachment_id)
        };
        if let Some(stale) = stale {
            crate::agent::session_registry::cleanup_installed_remote_attachment(stale, false).await;
        }
        self.connect_remote_session(
            session_id,
            RemoteConnectOptions {
                node_hint,
                peer_label: None,
                preferred_scope: None,
                reason: RemoteConnectReason::OperationRecovery,
                replace: RemoteReplacePolicy::ReplaceIfMatches(attachment_id),
                handoff: None,
                windowed,
                fork_origin: None,
            },
        )
        .await
    }

    /// Full coordinator entry point for callers with extra context
    /// (handoff from resume/create, explicit replacement policy).
    ///
    /// Every caller reaches the recovery stages through this one boxed
    /// future, so their own frames hold a pointer instead of the whole
    /// (large, in unoptimized builds) recovery state.
    pub(crate) fn connect_remote_session<'a>(
        &'a self,
        session_id: &'a str,
        options: RemoteConnectOptions<'a>,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<ConnectedRemoteSession, RemoteSessionConnectError>,
    > {
        Box::pin(self.connect_remote_session_unboxed(session_id, options))
    }

    async fn connect_remote_session_unboxed(
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
        let gate = RemoteConnectGate::acquire(&self.remote_connect_gates, session_id).await;
        let mut scope = Some(ConnectScope {
            session_id: session_id.to_string(),
            started,
            span: span.clone(),
            cooldowns: self.remote_connect_cooldowns.clone(),
            gate,
        });
        let result = self
            .connect_remote_session_gated(session_id, options, &mut scope)
            .instrument(span)
            .await;
        // Still ours unless the commit task took it over and finished it.
        if let Some(scope) = scope {
            scope.finish(&result);
        }
        result
    }

    /// Reject a connect attempt while the session is inside its failure
    /// cooldown. Explicit user reconnects only wait a short, capped time.
    fn check_connect_cooldown(
        &self,
        session_id: &str,
        reason: RemoteConnectReason,
    ) -> Result<(), RemoteSessionConnectError> {
        let Some(state) = self
            .remote_connect_cooldowns
            .lock()
            .get(session_id)
            .copied()
        else {
            return Ok(());
        };
        let wait = if reason == RemoteConnectReason::ExplicitReconnect {
            state
                .wait
                .min(Duration::from_millis(EXPLICIT_RECONNECT_COOLDOWN_MAX_MS))
        } else {
            state.wait
        };
        let elapsed = state.failed_at.elapsed();
        if elapsed >= wait {
            return Ok(());
        }
        let retry_after_ms = (wait - elapsed).as_millis() as u64;
        log::debug!(
            "remote session {} connect: cooling down after {} failures (reason={}, retry in {}ms)",
            session_id,
            state.failures,
            reason.as_str(),
            retry_after_ms,
        );
        Err(RemoteSessionConnectError::CoolingDown {
            session_id: session_id.to_string(),
            retry_after_ms,
            owner_unreachable: state.owner_unreachable,
        })
    }

    /// The recovery behind the gate. `scope` is taken (and finished by the
    /// commit task) when an attachment is committed; otherwise the caller
    /// finishes it with this result.
    async fn connect_remote_session_gated(
        &self,
        session_id: &str,
        mut options: RemoteConnectOptions<'_>,
        scope: &mut Option<ConnectScope>,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        use crate::session::location::{SessionLocation, resolve_session_location};
        let store = self.config.provider.history_store();
        let bookmark = match resolve_session_location(store.as_ref(), session_id)
            .await
            .map_err(|e| RemoteSessionConnectError::BookmarkLookupFailed {
                session_id: session_id.to_string(),
                message: e.to_string(),
            })? {
            SessionLocation::Local | SessionLocation::Conflict { .. } => {
                return Err(RemoteSessionConnectError::LocationConflict {
                    session_id: session_id.to_string(),
                    message: "a local session row exists under this remote session id".to_string(),
                });
            }
            SessionLocation::Remote { bookmark } => Some(bookmark),
            SessionLocation::NotFound => None,
        };
        if let (Some(bookmark), Some(hint)) = (&bookmark, options.node_hint)
            && bookmark.node_id != hint
        {
            return Err(RemoteSessionConnectError::LocationConflict {
                session_id: session_id.to_string(),
                message: "node hint disagrees with the durable remote owner".to_string(),
            });
        }
        let node_id = bookmark
            .as_ref()
            .map(|b| b.node_id.clone())
            .or_else(|| options.node_hint.map(str::to_string))
            .ok_or_else(|| RemoteSessionConnectError::BookmarkMissing {
                session_id: session_id.to_string(),
            })?;
        // A persisted remote identity makes an extension attach a repair
        // reconnect (plan §12): reinstall instead of reusing a possibly
        // broken attachment.
        if options.reason == RemoteConnectReason::ExtensionAttach && bookmark.is_some() {
            options.reason = RemoteConnectReason::ExplicitReconnect;
            options.replace = RemoteReplacePolicy::ReplaceCurrent;
            tracing::Span::current().record("reason", options.reason.as_str());
        }

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

            if let Some(snapshot) = reuse_snapshot
                && bookmark.is_some()
                && session_ref.remote_node_id() == Some(node_id.as_str())
            {
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

        let mesh = self
            .mesh()
            .ok_or_else(|| RemoteSessionConnectError::MeshUnavailable {
                node_id: node_id.clone(),
            })?;

        // Network work starts here: refuse if this session failed recently.
        self.check_connect_cooldown(session_id, options.reason)?;

        // ── Bounded recovery.
        let budget = options.reason.connect_budget();
        let prepared_slot: PreparedCandidateSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let state = self.config.event_sink.remote_sync(session_id, &node_id);
        let recovery = tokio::time::timeout(
            budget,
            self.recover_remote_attachment(
                session_id,
                options,
                mesh,
                node_id,
                bookmark,
                &prepared_slot,
            ),
        )
        .await;

        match recovery {
            Ok(Err(RemoteSessionConnectError::AccessDenied)) => {
                state.note_history_denial();
                Err(RemoteSessionConnectError::AccessDenied)
            }
            Ok(Err(error)) => Err(error),
            Ok(Ok(verified)) => {
                self.commit_remote_attachment(session_id, verified, scope.take())
                    .await
            }
            Err(_) => {
                // The budget cancelled the recovery future. Any candidate it
                // had already prepared is parked in the slot: abort it
                // explicitly so cancellation still unsubscribes the remote
                // forwarder and unlinks the relay (plain Drop only kills the
                // relay and deregisters its DHT names). The old generation
                // stays installed.
                let parked = prepared_slot
                    .lock()
                    .expect("prepared candidate slot poisoned")
                    .take();
                if let Some(prepared) = parked {
                    crate::agent::session_registry::abort_prepared_remote_attachment(prepared)
                        .await;
                }
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
        prepared_slot: &PreparedCandidateSlot,
    ) -> Result<VerifiedRemoteAttachment, RemoteSessionConnectError> {
        let peer_label = match bookmark
            .as_ref()
            .map(|b| b.peer_label.clone())
            .or_else(|| options.peer_label.map(str::to_owned))
        {
            Some(label) => label,
            // First-time attach without a label: look it up, bounded, since
            // the label is cosmetic and must not spend the connect budget.
            None => self.remote_node_label(&node_id).await,
        };

        // Pre-resolved handoff (extension attach from create/resume response).
        if let Some(handoff) = options.handoff {
            let remote_ref = self
                .resolve_handoff(session_id, &node_id, handoff)
                .await
                .map_err(|e| {
                    if e.code == agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND {
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
            let merged = Self::build_connect_bookmark(
                session_id,
                bookmark.as_ref(),
                RemoteSessionBookmarkUpdate {
                    node_id: Some(node_id.clone()),
                    peer_label: Some(peer_label.clone()),
                    fork_origin: options.fork_origin.clone(),
                    ..Default::default()
                },
                None,
            );
            return self
                .attach_and_verify_remote(
                    session_id,
                    LocatedRemoteActor {
                        remote_ref,
                        preferred_scope: options.preferred_scope,
                        outcome: RemoteConnectOutcome::Resumed,
                    },
                    &merged,
                    prepared_slot,
                    options.windowed,
                )
                .await;
        }

        // Stage 1: scoped DHT lookup.
        let lookup_span = tracing::info_span!(
            "remote.session.lookup",
            session_id,
            node_id = node_id.as_str(),
        );
        // A lookup error only means this stage found nothing; resume follows.
        let dht_hit = Self::lookup_remote_session_actor(&mesh, session_id, &node_id)
            .instrument(lookup_span)
            .await
            .ok()
            .flatten();

        if let Some((remote_ref, matched_scope)) = dht_hit {
            let merged = Self::build_connect_bookmark(
                session_id,
                bookmark.as_ref(),
                RemoteSessionBookmarkUpdate {
                    node_id: Some(node_id.clone()),
                    peer_label: Some(peer_label.clone()),
                    fork_origin: options.fork_origin.clone(),
                    ..Default::default()
                },
                None,
            );
            match self
                .attach_and_verify_remote(
                    session_id,
                    LocatedRemoteActor {
                        remote_ref,
                        preferred_scope: Some(matched_scope),
                        outcome: RemoteConnectOutcome::Reattached,
                    },
                    &merged,
                    prepared_slot,
                    options.windowed,
                )
                .await
            {
                Ok(verified) => return Ok(verified),
                Err(RemoteSessionConnectError::AccessDenied) => {
                    return Err(RemoteSessionConnectError::AccessDenied);
                }
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
                    message: e.message,
                }
            })?;
            self.resume_remote_session(&node_manager, session_id.to_string())
                .await
                .map_err(|e| {
                    // The host explicitly reports the session is absent (plain
                    // `SessionNotFound` from resume, or `RemoteSessionNotFound`
                    // from lookup paths): structural answer, not a transient
                    // recovery failure. Both map to ACP -32002 like the pre-
                    // coordinator pass-through did.
                    if matches!(e, AgentError::MeshAccessDenied) {
                        RemoteSessionConnectError::AccessDenied
                    } else if matches!(
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
            .resolve_handoff(session_id, &node_id, resumed.handoff)
            .await
            .map_err(|e| {
                if e.code == agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND {
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

        let merged = Self::build_connect_bookmark(
            session_id,
            bookmark.as_ref(),
            RemoteSessionBookmarkUpdate {
                node_id: Some(node_id.clone()),
                peer_label: Some(peer_label.clone()),
                cwd: resumed.cwd,
                title: resumed.title,
                fork_origin: options.fork_origin.clone(),
            },
            Some(resumed.created_at),
        );
        self.attach_and_verify_remote(
            session_id,
            LocatedRemoteActor {
                remote_ref,
                preferred_scope: None,
                outcome: RemoteConnectOutcome::Resumed,
            },
            &merged,
            prepared_slot,
            options.windowed,
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
        located: LocatedRemoteActor,
        bookmark: &RemoteSessionBookmark,
        prepared_slot: &PreparedCandidateSlot,
        windowed: bool,
    ) -> Result<VerifiedRemoteAttachment, RemoteSessionConnectError> {
        let LocatedRemoteActor {
            remote_ref,
            preferred_scope,
            outcome,
        } = located;
        if !Self::remote_actor_matches_node(&remote_ref, &bookmark.node_id) {
            return Err(RemoteSessionConnectError::LocationConflict {
                session_id: session_id.to_string(),
                message: "remote actor peer disagrees with the durable owner".to_string(),
            });
        }
        let (context, expected_attachment_id) = {
            let registry = self.registry.lock().await;
            (
                registry.remote_attachment_prepare_context(),
                registry.remote_attachment_id(session_id),
            )
        };
        let backfill_sink = context.event_sink.clone();
        // Read before the subscription starts: live events arriving after it
        // must not move the cursor past history the backfill still needs.
        let sync_cursor = backfill_sink
            .journal()
            .remote_sync_cursor(session_id, &bookmark.node_id)
            .await
            .map_err(|error| RemoteSessionConnectError::RecoveryFailed {
                transport: None,
                message: error.to_string(),
            })?;
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
        .map_err(|error| match error {
            AgentError::MeshAccessDenied => RemoteSessionConnectError::AccessDenied,
            error => RemoteSessionConnectError::HealthCheckFailed {
                session_id: session_id.to_string(),
                transport: None,
                message: error.to_string(),
            },
        })?;
        let session_ref = prepared.session_ref().clone();
        let attachment_id = prepared.attachment_id();

        // Park the candidate while the health check and the bookmark save are
        // awaited: if the connect budget cancels this future meanwhile, the
        // coordinator finds the candidate in the slot and runs the full async
        // teardown (remote unsubscribe + unlink) instead of the Drop-only
        // partial cleanup.
        {
            let mut parked = prepared_slot
                .lock()
                .expect("prepared candidate slot poisoned");
            debug_assert!(parked.is_none(), "prepared candidate slot must be empty");
            *parked = Some(prepared);
        }

        let health_timeout = Self::remote_connect_health_timeout();
        let state = backfill_sink.remote_sync(session_id, &bookmark.node_id);
        let health = tokio::time::timeout(
            health_timeout,
            Self::authorize_remote_history(&state, &session_ref),
        )
        .await;
        let saved = if matches!(health, Ok(Ok(_))) {
            self.config
                .provider
                .history_store()
                .save_remote_session_bookmark(bookmark)
                .await
        } else {
            Ok(())
        };
        // No await from here on: once the candidate leaves the slot it is
        // returned (or aborted) without another cancellation point.
        let prepared = prepared_slot
            .lock()
            .expect("prepared candidate slot poisoned")
            .take()
            .expect("prepared candidate parked before health check");
        let failure = match health {
            Ok(Ok(())) => match saved {
                Ok(()) => None,
                Err(error) => Some(RemoteSessionConnectError::BookmarkSaveFailed {
                    session_id: session_id.to_string(),
                    message: error.to_string(),
                }),
            },
            Ok(Err(AgentError::MeshAccessDenied)) => Some(RemoteSessionConnectError::AccessDenied),
            Ok(Err(error)) => {
                log::warn!(
                    "remote session {} connect: candidate attachment_id={} failed health check: {}",
                    session_id,
                    attachment_id,
                    error,
                );
                Some(RemoteSessionConnectError::HealthCheckFailed {
                    session_id: session_id.to_string(),
                    transport: error.transport_failure().cloned(),
                    message: error.to_string(),
                })
            }
            Err(_) => {
                log::warn!(
                    "remote session {} connect: candidate attachment_id={} health check timed out after {}ms",
                    session_id,
                    attachment_id,
                    health_timeout.as_millis(),
                );
                Some(RemoteSessionConnectError::HealthCheckFailed {
                    session_id: session_id.to_string(),
                    transport: Self::remote_health_check_timeout_error(health_timeout)
                        .transport_failure()
                        .cloned(),
                    message: format!(
                        "control health check timed out after {}ms",
                        health_timeout.as_millis()
                    ),
                })
            }
        };
        if let Some(error) = failure {
            // The candidate is out of the slot: a cancellation during this
            // teardown leaves only the Drop cleanup, as before installation.
            crate::agent::session_registry::abort_prepared_remote_attachment(prepared).await;
            return Err(error);
        }
        Ok(VerifiedRemoteAttachment {
            prepared,
            expected_attachment_id,
            outcome,
            bookmark: bookmark.clone(),
            backfill_sink,
            sync_cursor,
            windowed,
        })
    }

    /// Install a verified candidate and start its backfill.
    ///
    /// Runs after the connect budget: once the candidate is verified and its
    /// bookmark saved, the install, the release of the replaced generation and
    /// the backfill start always complete, so a slow cleanup can neither leave
    /// an installed attachment reported as timed out nor skip its backfill.
    ///
    /// The commit runs in its own task. The connect budget does not cover it,
    /// but a caller's own deadline (the history authorization, a request
    /// timeout) does cancel the caller's future; without the task, that could
    /// drop the candidate while it waits for the registry lock, leaving only
    /// the synchronous `Drop` cleanup (the owner's subscription and the relay
    /// link would stay behind), or drop the replaced generation's cleanup
    /// half-way. Dropping the caller's future detaches the task instead.
    async fn commit_remote_attachment(
        &self,
        session_id: &str,
        verified: VerifiedRemoteAttachment,
        scope: Option<ConnectScope>,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        // The task is part of this connect, so it keeps the connect span as
        // its parent; the long-running backfill it starts opens its own.
        let task = tokio::spawn(
            Self::commit_verified_attachment(
                self.registry.clone(),
                session_id.to_string(),
                verified,
                scope,
            )
            .in_current_span(),
        );
        task.await.unwrap_or_else(|error| {
            // The task died before finishing its scope; dropping it released
            // the gate, and the failure is reported to this caller only.
            Err(RemoteSessionConnectError::RecoveryFailed {
                transport: None,
                message: format!("attachment commit task failed: {error}"),
            })
        })
    }

    /// The body of [`Self::commit_remote_attachment`]; needs only the registry,
    /// so it can run detached from the handle.
    async fn commit_verified_attachment(
        registry: std::sync::Arc<
            tokio::sync::Mutex<crate::agent::session_registry::SessionRegistry>,
        >,
        session_id: String,
        verified: VerifiedRemoteAttachment,
        scope: Option<ConnectScope>,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        let result = Self::install_verified_attachment(registry, &session_id, verified).await;
        // Even when the caller is gone: record the outcome, then release the
        // gate, so a connect that was waiting on it sees the installed
        // generation instead of racing the commit.
        if let Some(scope) = scope {
            scope.finish(&result);
        }
        result
    }

    async fn install_verified_attachment(
        registry: std::sync::Arc<
            tokio::sync::Mutex<crate::agent::session_registry::SessionRegistry>,
        >,
        session_id: &str,
        verified: VerifiedRemoteAttachment,
    ) -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        let VerifiedRemoteAttachment {
            prepared,
            expected_attachment_id,
            outcome,
            bookmark,
            backfill_sink,
            sync_cursor,
            windowed,
        } = verified;
        let session_ref = prepared.session_ref().clone();
        let attachment_id = prepared.attachment_id();
        let commit_span = tracing::info_span!(
            "remote.session.attach.commit",
            session_id,
            node_id = bookmark.node_id.as_str(),
            attachment_id,
            previous_attachment_id = ?expected_attachment_id,
        );
        let commit = async {
            let mut registry = registry.lock().await;
            registry.install_remote_attachment(prepared, expected_attachment_id)
        }
        .instrument(commit_span)
        .await;
        let old = match commit {
            Ok(old) => old,
            Err(boxed) => {
                let (prepared, conflict) = *boxed;
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

        // Cursor-based backfill (plan §16): the live subscription was
        // established during prepare, so newly generated events cannot be
        // lost; historical pages after the last persisted source cursor are
        // recovered with overlap-safe deduplication. Spawn-and-forget so
        // connect latency never waits on backfill volume. It is not
        // instrumented with the connect span: it outlives the connect and
        // opens its own `remote.session.sync` root span.
        //
        // The sync is registered before the spawn so a load racing the task
        // start still waits on the barrier (see `RemoteSyncState::wait_synced`).
        let sync_guard = backfill_sink
            .remote_sync(session_id, &bookmark.node_id)
            .begin_sync();
        let backfill = crate::agent::remote::event_backfill::RemoteSync::new(
            backfill_sink,
            session_ref.clone(),
            session_id,
            &bookmark.node_id,
            &bookmark.peer_label,
        );
        tokio::spawn(backfill.backfill(attachment_id, sync_cursor, windowed, Some(sync_guard)));

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
        Ok(ConnectedRemoteSession {
            session_ref,
            attachment_id,
            bookmark: Some(bookmark),
            outcome,
        })
    }

    /// Run `call` through a connected remote session.
    ///
    /// A reused attachment can outlive the actor it points to (the owner
    /// restarted before the link was seen to die). When `call` fails on a
    /// reused attachment and `dead` says the failure shows that, the
    /// attachment is replaced once and `call` repeated on the replacement.
    /// A freshly installed attachment passed its health check moments ago, so
    /// its failures are returned as they are.
    ///
    /// Returns the value and the session it was obtained through.
    pub(crate) async fn call_through_remote_attachment<T, E, F, Fut>(
        &self,
        session_id: &str,
        purpose: &str,
        connected: ConnectedRemoteSession,
        windowed: bool,
        dead: impl Fn(&E) -> bool,
        mut call: F,
    ) -> Result<(T, ConnectedRemoteSession), RemoteAttachmentCallError<E>>
    where
        E: std::fmt::Display,
        F: FnMut(ConnectedRemoteSession) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        let error = match call(connected.clone()).await {
            Ok(value) => return Ok((value, connected)),
            Err(error) if connected.outcome == RemoteConnectOutcome::Reused && dead(&error) => {
                error
            }
            Err(error) => return Err(RemoteAttachmentCallError::Failed(error)),
        };
        log::info!(
            "remote session {session_id} {purpose}: reused attachment_id={} failed ({error}); \
             replacing it",
            connected.attachment_id,
        );
        let recovered = self
            .replace_failed_remote_attachment(
                session_id,
                connected.attachment_id,
                connected.session_ref.remote_node_id(),
                windowed,
            )
            .await
            .map_err(RemoteAttachmentCallError::Replace)?;
        // `recovered` may itself be `Reused` when another caller installed a
        // newer generation meanwhile; the call is not repeated a second time.
        match call(recovered.clone()).await {
            Ok(value) => Ok((value, recovered)),
            Err(error) => Err(RemoteAttachmentCallError::Failed(error)),
        }
    }

    /// Make a connected remote session's recent history local before a load
    /// reads the journal (the sync barrier).
    ///
    /// A reused attachment starts no backfill of its own, so an incremental
    /// chunk (`after = highest covered`) is requested here; a fresh attachment
    /// already has a backfill registered, which this waits on. Older history
    /// keeps filling in the background.
    pub(crate) async fn sync_remote_for_load(
        &self,
        session_id: &str,
        connected: &ConnectedRemoteSession,
        windowed: bool,
    ) -> Result<(), crate::error::AgentError> {
        let Some(bookmark) = connected.bookmark.as_ref() else {
            return Ok(());
        };
        let sink = self.config.event_sink.clone();
        let state = sink.remote_sync(session_id, &bookmark.node_id);
        if connected.outcome == RemoteConnectOutcome::Reused {
            let sync = crate::agent::remote::event_backfill::RemoteSync::new(
                sink,
                connected.session_ref.clone(),
                session_id,
                &bookmark.node_id,
                &bookmark.peer_label,
            );
            sync.open(windowed).await?;
            // The fill outlives this load: a root span of its own, linked to
            // the load that started it instead of keeping the load's span open.
            let fill_span = tracing::info_span!(
                parent: None,
                "remote.session.fill",
                session_id,
                node_id = bookmark.node_id.as_str(),
            );
            fill_span.follows_from(tracing::Span::current());
            tokio::spawn(
                async move {
                    if let Err(error) = sync.fill().await {
                        tracing::warn!(%error, "background history fill failed");
                    }
                }
                .instrument(fill_span),
            );
        }
        state.wait_synced().await;
        if connected.outcome != RemoteConnectOutcome::Reused {
            // A fresh attachment passed this check moments ago; a denial its
            // backfill met since then is recorded in `state`.
            return if state.history_denied() {
                Err(crate::error::AgentError::MeshAccessDenied)
            } else {
                Ok(())
            };
        }
        Self::authorize_remote_history(&state, &connected.session_ref).await
    }

    pub(super) async fn authorize_remote_history(
        state: &crate::session::remote_sync::RemoteSyncState,
        session_ref: &crate::agent::remote::SessionActorRef,
    ) -> Result<(), crate::error::AgentError> {
        use crate::error::AgentError;

        let generation = state.denial_generation();
        let result = session_ref.check_history_access().await;
        match &result {
            Ok(()) if session_ref.is_remote() => state.note_history_authorization(generation),
            Err(AgentError::MeshAccessDenied) => state.note_history_denial(),
            _ => {}
        }
        if result.is_ok() && state.history_denied() {
            return Err(AgentError::MeshAccessDenied);
        }
        result
    }

    /// Build candidate metadata without mutating durable state, from the
    /// bookmark the coordinator read under its gate. Persistence is performed
    /// only after attachment health validation, and before the attachment is
    /// reported connected (plan §6). Existing `created_at` is preserved by
    /// `merge_confirmed`; a first write uses the resume-reported creation
    /// time when available.
    fn build_connect_bookmark(
        session_id: &str,
        existing: Option<&RemoteSessionBookmark>,
        update: RemoteSessionBookmarkUpdate,
        fallback_created_at: Option<i64>,
    ) -> RemoteSessionBookmark {
        match existing {
            Some(bookmark) => {
                log::debug!(
                    "remote session {} connect: merging bookmark metadata",
                    session_id
                );
                bookmark.clone().merge_confirmed(update)
            }
            None => RemoteSessionBookmark {
                session_id: session_id.to_string(),
                node_id: update.node_id.unwrap_or_default(),
                peer_label: update.peer_label.unwrap_or_default(),
                cwd: update.cwd,
                created_at: fallback_created_at.unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                }),
                title: update.title,
                fork_origin: update.fork_origin,
            },
        }
    }

    fn remote_actor_matches_node(
        actor: &kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        node_id: &str,
    ) -> bool {
        actor
            .id()
            .peer_id()
            .is_some_and(|peer| peer.to_string() == node_id)
    }

    /// Scoped DHT lookup for a session actor across all active scopes.
    pub(super) async fn lookup_remote_session_actor(
        mesh: &crate::agent::remote::MeshHandle,
        session_id: &str,
        node_id: &str,
    ) -> Result<
        Option<(
            kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
            crate::agent::remote::scope::MeshScopeId,
        )>,
        String,
    > {
        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let mut last_error = None;
        for scope in runtime.active_scopes() {
            let dht_name = crate::agent::remote::scope::scoped_session(&scope, session_id);
            match runtime
                .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name)
                .await
            {
                Ok(Some(found)) => {
                    if !Self::remote_actor_matches_node(&found, node_id) {
                        tracing::warn!(session_id, node_id, actor = %found.id(), "ignoring DHT actor from another peer");
                        continue;
                    }
                    log::debug!(
                        "remote session {} connect: DHT publication found (scope matched)",
                        session_id
                    );
                    return Ok(Some((found, scope)));
                }
                Ok(None) => {}
                Err(e) => {
                    log::debug!(
                        "remote session {} connect: scoped DHT lookup error: {}; trying next scope",
                        session_id,
                        e
                    );
                    last_error = Some(e.to_string());
                }
            }
        }
        last_error.map_or(Ok(None), Err)
    }

    /// Whether a remote connect/recovery attempt is currently in flight for
    /// the session (a held or queued single-flight gate entry).
    pub(crate) fn remote_connect_in_flight(&self, session_id: &str) -> bool {
        self.remote_connect_gates.lock().contains_key(session_id)
    }

    /// Display label of a mesh node for a first-time attach: the cached node
    /// info when present, otherwise a mesh listing bounded by
    /// [`REMOTE_NODE_LABEL_BUDGET`]; the node id when neither answers.
    async fn remote_node_label(&self, node_id: &str) -> String {
        let cache_key = Self::peer_cache_key(node_id.parse().ok(), 0);
        if let Some(info) = self.get_cached_remote_node(&cache_key)
            && info.node_id.to_string() == node_id
        {
            return info.hostname;
        }
        tokio::time::timeout(REMOTE_NODE_LABEL_BUDGET, self.list_remote_nodes())
            .await
            .ok()
            .and_then(|nodes| {
                nodes
                    .into_iter()
                    .find(|n| n.node_id.to_string() == node_id)
                    .map(|n| n.hostname)
            })
            .unwrap_or_else(|| node_id.to_string())
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
            fork_origin: None,
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

    fn node_unavailable() -> Result<ConnectedRemoteSession, RemoteSessionConnectError> {
        Err(RemoteSessionConnectError::NodeUnavailable {
            node_id: "n".into(),
            transport: None,
            message: "down".into(),
        })
    }

    #[test]
    fn cooldown_wait_grows_exponentially_and_is_capped() {
        let w1 = connect_cooldown_wait(1);
        let w3 = connect_cooldown_wait(3);
        assert!(w1 >= Duration::from_millis(CONNECT_COOLDOWN_BASE_MS));
        assert!(w1 <= Duration::from_millis(CONNECT_COOLDOWN_BASE_MS * 6 / 5));
        assert!(w3 >= Duration::from_millis(CONNECT_COOLDOWN_BASE_MS * 4));
        assert_eq!(
            connect_cooldown_wait(1_000),
            Duration::from_millis(CONNECT_COOLDOWN_MAX_MS)
        );
    }

    #[test]
    fn cooling_down_error_is_retriable_but_does_not_extend_cooldown() {
        let err = RemoteSessionConnectError::CoolingDown {
            session_id: "s".into(),
            retry_after_ms: 1_500,
            owner_unreachable: true,
        };
        assert_eq!(err.code(), "remote_connect_cooling_down");
        assert!(err.is_retriable());
        assert!(!err.counts_toward_cooldown());
        let acp = err.to_acp_error();
        let data = acp.data.expect("data");
        assert_eq!(data["retry_after_ms"], 1_500);
        assert_eq!(data["retriable"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooldown_blocks_hot_retries_and_preserves_fallback_eligibility() {
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        assert!(
            handle
                .check_connect_cooldown("s1", RemoteConnectReason::Open)
                .is_ok()
        );

        record_connect_outcome(&handle.remote_connect_cooldowns, "s1", &node_unavailable());
        let err = handle
            .check_connect_cooldown("s1", RemoteConnectReason::Open)
            .unwrap_err();
        assert_eq!(err.code(), "remote_connect_cooling_down");
        assert!(err.owner_unreachable());

        record_connect_outcome(
            &handle.remote_connect_cooldowns,
            "s1",
            &Err(RemoteSessionConnectError::RecoveryFailed {
                transport: None,
                message: "owner rejected resume".into(),
            }),
        );
        let err = handle
            .check_connect_cooldown("s1", RemoteConnectReason::Open)
            .unwrap_err();
        assert!(!err.owner_unreachable());

        // Other sessions are unaffected.
        assert!(
            handle
                .check_connect_cooldown("s2", RemoteConnectReason::Open)
                .is_ok()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consecutive_failures_extend_the_cooldown() {
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        record_connect_outcome(&handle.remote_connect_cooldowns, "s1", &node_unavailable());
        let first = handle.remote_connect_cooldowns.lock()["s1"];
        record_connect_outcome(&handle.remote_connect_cooldowns, "s1", &node_unavailable());
        let second = handle.remote_connect_cooldowns.lock()["s1"];
        assert_eq!(first.failures, 1);
        assert_eq!(second.failures, 2);
        assert!(second.wait > first.wait);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn structural_errors_do_not_start_a_cooldown() {
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        record_connect_outcome(
            &handle.remote_connect_cooldowns,
            "s1",
            &Err(RemoteSessionConnectError::BookmarkMissing {
                session_id: "s1".into(),
            }),
        );
        assert!(handle.remote_connect_cooldowns.lock().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_reconnect_waits_only_a_short_capped_time() {
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        // Simulate a long-running failure streak.
        handle.remote_connect_cooldowns.lock().insert(
            "s1".into(),
            ConnectCooldown {
                failures: 10,
                failed_at: Instant::now()
                    - Duration::from_millis(EXPLICIT_RECONNECT_COOLDOWN_MAX_MS + 100),
                wait: Duration::from_millis(CONNECT_COOLDOWN_MAX_MS),
                owner_unreachable: true,
            },
        );
        // A background retry is still blocked; the user's explicit reconnect is not.
        assert!(
            handle
                .check_connect_cooldown("s1", RemoteConnectReason::Open)
                .is_err()
        );
        assert!(
            handle
                .check_connect_cooldown("s1", RemoteConnectReason::ExplicitReconnect)
                .is_ok()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reused_attachment_preserves_failure_cooldown() {
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        record_connect_outcome(&handle.remote_connect_cooldowns, "s1", &node_unavailable());
        let before = handle.remote_connect_cooldowns.lock()["s1"];
        let actor = SessionActor::spawn(SessionActor::new(
            handle.config.clone(),
            "s1".into(),
            SessionRuntime::new(
                None,
                std::collections::HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        ));
        let mut result = Ok(ConnectedRemoteSession {
            session_ref: SessionActorRef::from(actor.clone()),
            attachment_id: 1,
            bookmark: None,
            outcome: RemoteConnectOutcome::Reused,
        });
        record_connect_outcome(&handle.remote_connect_cooldowns, "s1", &result);
        let after = handle.remote_connect_cooldowns.lock()["s1"];
        assert_eq!(after.failures, before.failures);
        assert_eq!(after.failed_at, before.failed_at);
        assert_eq!(after.wait, before.wait);
        assert!(
            handle
                .check_connect_cooldown("s1", RemoteConnectReason::OperationRecovery)
                .is_err()
        );
        for outcome in [
            RemoteConnectOutcome::Resumed,
            RemoteConnectOutcome::Reattached,
        ] {
            record_connect_outcome(&handle.remote_connect_cooldowns, "s1", &node_unavailable());
            result.as_mut().unwrap().outcome = outcome;
            record_connect_outcome(&handle.remote_connect_cooldowns, "s1", &result);
            assert!(handle.remote_connect_cooldowns.lock().is_empty());
        }
        actor.kill();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooldown_expires_and_success_clears_it() {
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        handle.remote_connect_cooldowns.lock().insert(
            "s1".into(),
            ConnectCooldown {
                failures: 2,
                failed_at: Instant::now() - Duration::from_secs(120),
                wait: Duration::from_secs(4),
                owner_unreachable: true,
            },
        );
        assert!(
            handle
                .check_connect_cooldown("s1", RemoteConnectReason::Open)
                .is_ok(),
            "an elapsed cooldown no longer blocks"
        );

        // A successful outcome removes the failure streak entirely.
        let ok = Ok(ConnectedRemoteSession {
            session_ref: SessionActorRef::from(SessionActor::spawn(SessionActor::new(
                handle.config.clone(),
                "s1".to_string(),
                SessionRuntime::new(
                    None,
                    std::collections::HashMap::new(),
                    crate::agent::core::McpToolState::empty(),
                ),
            ))),
            attachment_id: 1,
            bookmark: None,
            outcome: RemoteConnectOutcome::Resumed,
        });
        record_connect_outcome(&handle.remote_connect_cooldowns, "s1", &ok);
        assert!(handle.remote_connect_cooldowns.lock().is_empty());
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
        assert_eq!(
            acp.code,
            agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_and_subscription_transfer_each_event_once_and_replay_matches() {
        use crate::session::projection::NewDurableEvent;
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        let (owner, _owner_storage, _t1) = handle_with_real_storage().await;
        let (viewer, viewer_storage, _t2) = handle_with_real_storage().await;
        viewer.set_mesh(mesh.clone());
        let session_id = "once-session";
        let journal = owner.config.event_sink.journal().clone();
        for n in 0..40 {
            journal
                .append_durable(&NewDurableEvent {
                    session_id: session_id.into(),
                    origin: querymt_agent_core::events::EventOrigin::Local,
                    source_node: None,
                    source_node_id: None,
                    source_seq: None,
                    session_seq: None,
                    kind: if n % 2 == 0 {
                        AgentEventKind::RunStarted {
                            run_id: format!("r{n}"),
                            origin: "interactive".into(),
                        }
                    } else {
                        AgentEventKind::PromptReceived {
                            slash_command: None,
                            content: format!("p{n}"),
                            message_id: None,
                        }
                    },
                })
                .await
                .unwrap();
        }
        let actor = SessionActor::spawn(
            SessionActor::new(
                owner.config.clone(),
                session_id.into(),
                SessionRuntime::new(
                    None,
                    Default::default(),
                    crate::agent::core::McpToolState::empty(),
                ),
            )
            .with_mesh(Some(mesh.clone())),
        );
        let remote = actor.clone().into_remote_ref().await;
        let peer = mesh.peer_id().to_string();
        viewer_storage
            .session_store()
            .save_remote_session_bookmark(&test_bookmark(session_id, &peer))
            .await
            .unwrap();
        let session_ref = SessionActorRef::remote(remote, "test-node".into());

        // Attach: one sync transfers the history.
        let sync = crate::agent::remote::event_backfill::RemoteSync::new(
            viewer.config.event_sink.clone(),
            session_ref.clone(),
            session_id,
            &peer,
            "test-node",
        );
        sync.sync_all(false, None).await.unwrap();
        let (requests, _, inserted) = sync.state().transfers();
        assert_eq!(inserted, 40);

        // Connection subscription / replay: read the journal, no new transfer.
        let first = viewer
            .remote_session_events_from_journal(session_id, &session_ref)
            .await
            .unwrap()
            .expect("remote session reads the journal");
        let second = viewer
            .remote_session_events_from_journal(session_id, &session_ref)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            sync.state().transfers().0,
            requests,
            "no extra mesh requests"
        );
        assert_eq!(sync.state().transfers().2, 40, "each event persisted once");
        assert_eq!(first.len(), 40);
        assert_eq!(second.len(), 40);

        // Non-snapshot clients get the same replay as the owner's stream.
        let owner_stream = session_ref.get_event_stream().await.unwrap();
        let strip = |events: &[querymt_agent_core::events::AgentEvent]| {
            events
                .iter()
                .map(|e| (e.seq, serde_json::to_value(&e.kind).unwrap()))
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&first), strip(&owner_stream));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unverified_candidates_cannot_write_bookmarks_or_replace_attachment() {
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        let (handle, storage, _tmp) = handle_with_real_storage().await;
        handle.set_mesh(mesh.clone());
        let session_id = "candidate-validation";
        let actor = SessionActor::spawn(
            SessionActor::new(
                handle.config.clone(),
                session_id.into(),
                SessionRuntime::new(
                    None,
                    Default::default(),
                    crate::agent::core::McpToolState::empty(),
                ),
            )
            .with_mesh(Some(mesh.clone())),
        );
        let remote = actor.clone().into_remote_ref().await;
        let peer = mesh.peer_id().to_string();
        let bookmark = test_bookmark(session_id, &peer);
        let slot = Arc::new(std::sync::Mutex::new(None));
        let wrong = test_bookmark(session_id, "another-peer");
        let error = handle
            .attach_and_verify_remote(
                session_id,
                LocatedRemoteActor {
                    remote_ref: remote.clone(),
                    preferred_scope: None,
                    outcome: RemoteConnectOutcome::Reattached,
                },
                &wrong,
                &slot,
                false,
            )
            .await
            .err()
            .expect("a candidate of another peer is rejected");
        assert_eq!(error.code(), "session_location_conflict");
        assert!(
            storage
                .session_store()
                .get_remote_session_bookmark(session_id)
                .await
                .unwrap()
                .is_none()
        );
        // DHT entries are scope/session keyed: an actor from a different node
        // must be ignored even if it advertises the requested session name.
        mesh.register_actor(
            actor.clone(),
            crate::agent::remote::scope::scoped_session(
                &crate::agent::remote::scope::MeshScopeId::lan_default(),
                session_id,
            ),
        )
        .await;
        assert!(matches!(
            LocalAgentHandle::lookup_remote_session_actor(&mesh, session_id, "another-peer").await,
            Ok(None)
        ));
        let attached = handle
            .attach_and_verify_remote(
                session_id,
                LocatedRemoteActor {
                    remote_ref: remote.clone(),
                    preferred_scope: None,
                    outcome: RemoteConnectOutcome::Reattached,
                },
                &bookmark,
                &slot,
                false,
            )
            .await
            .unwrap_or_else(|error| panic!("candidate verification failed: {error}"));
        let attached = handle
            .commit_remote_attachment(session_id, attached, None)
            .await
            .unwrap();
        let other_actor = SessionActor::spawn(
            SessionActor::new(
                handle.config.clone(),
                session_id.into(),
                SessionRuntime::new(
                    None,
                    Default::default(),
                    crate::agent::core::McpToolState::empty(),
                ),
            )
            .with_mesh(Some(mesh.clone())),
        );
        let dead = other_actor.clone().into_remote_ref().await;
        other_actor.kill();
        other_actor.wait_for_shutdown().await;
        let mut changed = bookmark.clone();
        changed.title = Some("must not persist".into());
        assert!(
            handle
                .attach_and_verify_remote(
                    session_id,
                    LocatedRemoteActor {
                        remote_ref: dead,
                        preferred_scope: None,
                        outcome: RemoteConnectOutcome::Reattached,
                    },
                    &changed,
                    &slot,
                    false
                )
                .await
                .is_err()
        );
        assert_eq!(
            storage
                .session_store()
                .get_remote_session_bookmark(session_id)
                .await
                .unwrap(),
            Some(bookmark)
        );
        assert_eq!(
            handle
                .registry
                .lock()
                .await
                .remote_attachment_id(session_id),
            Some(attached.attachment_id)
        );
    }

    /// A caller deadline that fires while the commit waits for the registry
    /// lock must not drop the verified candidate (which would leave only the
    /// synchronous `Drop` cleanup): the commit completes detached.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn commit_survives_caller_cancellation() {
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        handle.set_mesh(mesh.clone());
        let session_id = "commit-cancellation";
        let actor = SessionActor::spawn(
            SessionActor::new(
                handle.config.clone(),
                session_id.into(),
                SessionRuntime::new(
                    None,
                    Default::default(),
                    crate::agent::core::McpToolState::empty(),
                ),
            )
            .with_mesh(Some(mesh.clone())),
        );
        let remote = actor.clone().into_remote_ref().await;
        let bookmark = test_bookmark(session_id, &mesh.peer_id().to_string());
        let slot = Arc::new(std::sync::Mutex::new(None));
        let verified = handle
            .attach_and_verify_remote(
                session_id,
                LocatedRemoteActor {
                    remote_ref: remote,
                    preferred_scope: None,
                    outcome: RemoteConnectOutcome::Reattached,
                },
                &bookmark,
                &slot,
                false,
            )
            .await
            .unwrap_or_else(|error| panic!("candidate verification failed: {error}"));
        let attachment_id = verified.prepared.attachment_id();

        // Hold the registry lock so the commit blocks, then cancel the caller.
        let guard = handle.registry.lock().await;
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            handle.commit_remote_attachment(session_id, verified, None),
        )
        .await;
        assert!(cancelled.is_err(), "the commit must be blocked on the lock");
        drop(guard);

        let installed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if handle
                    .registry
                    .lock()
                    .await
                    .remote_attachment_id(session_id)
                    == Some(attachment_id)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            installed.is_ok(),
            "the detached commit installs the verified attachment"
        );
    }

    /// A verified candidate for a fresh actor of `session_id` on the test mesh.
    async fn verified_candidate(
        handle: &LocalAgentHandle,
        mesh: &crate::agent::remote::MeshHandle,
        session_id: &str,
    ) -> VerifiedRemoteAttachment {
        let actor = SessionActor::spawn(
            SessionActor::new(
                handle.config.clone(),
                session_id.into(),
                SessionRuntime::new(
                    None,
                    Default::default(),
                    crate::agent::core::McpToolState::empty(),
                ),
            )
            .with_mesh(Some(mesh.clone())),
        );
        let remote = actor.clone().into_remote_ref().await;
        let bookmark = test_bookmark(session_id, &mesh.peer_id().to_string());
        let slot = Arc::new(std::sync::Mutex::new(None));
        handle
            .attach_and_verify_remote(
                session_id,
                LocatedRemoteActor {
                    remote_ref: remote,
                    preferred_scope: None,
                    outcome: RemoteConnectOutcome::Reattached,
                },
                &bookmark,
                &slot,
                false,
            )
            .await
            .unwrap_or_else(|error| panic!("candidate verification failed: {error}"))
    }

    /// A caller cancelled while its commit waits for the registry lock must
    /// not free the session's gate: a connect racing the commit would reuse
    /// or replace the generation being installed. The detached commit
    /// records the outcome, then releases the gate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_commit_keeps_the_gate_until_installed() {
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        handle.set_mesh(mesh.clone());
        let session_id = "commit-gate";
        let verified = verified_candidate(&handle, &mesh, session_id).await;
        let attachment_id = verified.prepared.attachment_id();

        // A failure recorded earlier; a committed attachment clears it.
        handle.remote_connect_cooldowns.lock().insert(
            session_id.to_string(),
            ConnectCooldown {
                failures: 1,
                failed_at: Instant::now(),
                wait: Duration::from_secs(60),
                owner_unreachable: true,
            },
        );
        let gate = RemoteConnectGate::acquire(&handle.remote_connect_gates, session_id).await;
        let scope = ConnectScope {
            session_id: session_id.to_string(),
            started: Instant::now(),
            span: tracing::Span::none(),
            cooldowns: handle.remote_connect_cooldowns.clone(),
            gate,
        };

        let registry_guard = handle.registry.lock().await;
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            handle.commit_remote_attachment(session_id, verified, Some(scope)),
        )
        .await;
        assert!(cancelled.is_err(), "the commit must be blocked on the lock");

        // The caller is gone, the commit is pending: the gate stays held and
        // the outcome is not recorded yet.
        let raced = tokio::time::timeout(
            Duration::from_millis(100),
            RemoteConnectGate::acquire(&handle.remote_connect_gates, session_id),
        )
        .await;
        assert!(raced.is_err(), "the gate must outlive the cancelled caller");
        assert!(
            handle
                .remote_connect_cooldowns
                .lock()
                .contains_key(session_id)
        );
        drop(registry_guard);

        // The commit finishes: attachment installed, cooldown cleared, gate free.
        let _gate = tokio::time::timeout(
            Duration::from_secs(5),
            RemoteConnectGate::acquire(&handle.remote_connect_gates, session_id),
        )
        .await
        .expect("the commit releases the gate when it is done");
        assert_eq!(
            handle
                .registry
                .lock()
                .await
                .remote_attachment_id(session_id),
            Some(attachment_id),
            "a connect that waited on the gate sees the installed generation"
        );
        assert!(
            !handle
                .remote_connect_cooldowns
                .lock()
                .contains_key(session_id),
            "the detached commit records the outcome"
        );
    }

    /// The remote attach path must fit a 2 MiB thread stack in an unoptimized
    /// build, where every `.await` in an `async fn` gets its own stack slot
    /// and a `match` over awaited handlers sums them (the extension dispatcher
    /// once reached 673 KiB, overflowing on this path). Release builds
    /// overlap those slots, so only debug builds can regress; the test is
    /// gated accordingly. It drives the request on a dedicated thread, as
    /// the stdio / WebSocket transports do for their workers.
    #[cfg(debug_assertions)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_attach_fits_a_small_thread_stack() {
        const STACK_BYTES: usize = 2 * 1024 * 1024;

        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        handle.set_mesh(mesh.clone());
        let session_id = "stack-guard";
        // An installed attachment plus its saved bookmark: the attach then
        // takes the connect + load path end to end.
        let verified = verified_candidate(&handle, &mesh, session_id).await;
        handle
            .commit_remote_attachment(session_id, verified, None)
            .await
            .unwrap();

        let params = serde_json::json!({
            "session_id": session_id,
            "node_id": mesh.peer_id().to_string(),
        });
        let request = ExtRequest::new(
            "querymt/remote/attachSession",
            Arc::from(serde_json::value::RawValue::from_string(params.to_string()).unwrap()),
        );
        let runtime = tokio::runtime::Handle::current();
        let outcome = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("small-stack-attach".into())
                .stack_size(STACK_BYTES)
                .spawn_scoped(scope, || {
                    runtime.block_on(handle.handle_ext_method(request))
                })
                .expect("spawn small-stack thread")
                .join()
        });
        // A stack overflow aborts the process before reaching this point. The
        // attach itself fails here (the test mesh does not publish the owner),
        // which is fine: the extension dispatcher frame, the largest of the
        // regression, is on the stack whatever the outcome. Measured: this
        // passes down to 256 KiB and aborts at 128 KiB.
        let _attach_result = outcome.expect("the attach thread must not panic");
    }

    /// Records each new span's name and its parent's name.
    type SpanAndParent = (String, Option<String>);

    #[derive(Clone, Default)]
    struct SpanParents(Arc<parking_lot::Mutex<Vec<SpanAndParent>>>);

    impl<S> tracing_subscriber::Layer<S> for SpanParents
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let parent = ctx
                .span(id)
                .and_then(|span| span.parent())
                .map(|parent| parent.name().to_string());
            self.0
                .lock()
                .push((attrs.metadata().name().to_string(), parent));
        }
    }

    /// The detached commit is part of the connect: its span stays a child of
    /// the connect span instead of becoming a root.
    #[tokio::test]
    async fn commit_task_inherits_the_connect_span() {
        use tracing_subscriber::layer::SubscriberExt;

        let spans = SpanParents::default();
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(spans.clone()));
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        let (handle, _storage, _tmp) = handle_with_real_storage().await;
        handle.set_mesh(mesh.clone());
        let session_id = "commit-span";
        let verified = verified_candidate(&handle, &mesh, session_id).await;

        let connect = tracing::info_span!("remote.session.connect");
        handle
            .commit_remote_attachment(session_id, verified, None)
            .instrument(connect)
            .await
            .unwrap();

        let recorded = spans.0.lock().clone();
        let commit = recorded
            .iter()
            .find(|(name, _)| name == "remote.session.attach.commit")
            .unwrap_or_else(|| panic!("commit span not recorded: {recorded:?}"));
        assert_eq!(commit.1.as_deref(), Some("remote.session.connect"));
    }

    #[tokio::test]
    async fn reconnect_discovers_owner_and_replaces_only_the_failed_generation() {
        use crate::agent::remote::scope::{
            MeshScopeId, scoped_node_manager_for_peer, scoped_session,
        };
        use crate::agent::remote::test_helpers::fixtures::{NodeManagerFixture, get_test_mesh};

        let mesh = get_test_mesh().await;
        let owner = NodeManagerFixture::new_with_mesh().await;
        let (viewer, storage, _dir) = handle_with_real_storage().await;
        viewer.set_mesh(mesh.clone());
        let node_id = mesh.peer_id().to_string();
        let scope = MeshScopeId::lan_default();
        let manager_name = scoped_node_manager_for_peer(&scope, &node_id);
        mesh.register_actor(owner.actor_ref.clone(), manager_name.clone())
            .await;
        let session_id = owner
            .config
            .provider
            .history_store()
            .create_session(Some("owner session".into()), None, None, None)
            .await
            .unwrap()
            .public_id;
        storage
            .session_store()
            .save_remote_session_bookmark(&test_bookmark(&session_id, &node_id))
            .await
            .unwrap();
        let first_event = owner
            .config
            .emit_event_persisted(
                &session_id,
                AgentEventKind::PromptReceived {
                    slash_command: None,
                    content: "before attach".into(),
                    message_id: None,
                },
            )
            .await
            .unwrap();

        // A persisted owner session with no live publication must be resumed
        // through the manager registered under that owner's peer ID.
        let first = viewer
            .ensure_remote_session_connected(&session_id, None, RemoteConnectReason::Open)
            .await
            .unwrap();
        assert_eq!(first.outcome, RemoteConnectOutcome::Resumed);
        let current = viewer
            .ensure_remote_session_connected(
                &session_id,
                None,
                RemoteConnectReason::ExplicitReconnect,
            )
            .await
            .unwrap();
        assert_eq!(current.outcome, RemoteConnectOutcome::Reattached);
        assert_ne!(current.attachment_id, first.attachment_id);
        let stale_recovery = viewer
            .connect_remote_session(
                &session_id,
                RemoteConnectOptions {
                    node_hint: None,
                    peer_label: None,
                    preferred_scope: None,
                    reason: RemoteConnectReason::OperationRecovery,
                    replace: RemoteReplacePolicy::ReplaceIfMatches(first.attachment_id),
                    handoff: None,
                    windowed: false,
                    fork_origin: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(stale_recovery.outcome, RemoteConnectOutcome::Reused);
        assert_eq!(stale_recovery.attachment_id, current.attachment_id);

        viewer
            .sync_remote_for_load(&session_id, &current, false)
            .await
            .unwrap();

        // Overflow the forwarder's live queue after persisting a session event.
        // Only unrelated events remain in the queue, so no later matching event
        // can reveal the missing tail. The forwarder's lag notice must recover it.
        use querymt_agent_core::events::{EphemeralEvent, EventEnvelope, EventOrigin};
        let mut live = viewer.config.event_sink.fanout().subscribe();
        let missed = owner
            .config
            .emit_event_persisted(
                &session_id,
                AgentEventKind::PromptReceived {
                    slash_command: None,
                    content: "missed live event".into(),
                    message_id: None,
                },
            )
            .await
            .unwrap();
        let unrelated = EventEnvelope::Ephemeral(EphemeralEvent {
            session_id: "unrelated-session".into(),
            session_seq: 0,
            timestamp: missed.timestamp,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::Cancelled,
        });
        for _ in 0..4096 {
            owner.config.event_sink.fanout().publish(unrelated.clone());
        }
        let recovered = tokio::time::timeout(Duration::from_secs(3), live.recv())
            .await
            .expect("the relay should recover the dropped durable tail")
            .unwrap();
        assert!(matches!(
            recovered.kind(),
            AgentEventKind::PromptReceived { content, .. } if content == "missed live event"
        ));
        let replay = viewer
            .remote_session_events_from_journal(&session_id, &current.session_ref)
            .await
            .unwrap()
            .unwrap();
        let prompts = replay
            .iter()
            .filter_map(|entry| match &entry.kind {
                AgentEventKind::PromptReceived { content, .. } => {
                    Some((entry.seq, content.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prompts,
            vec![
                (first_event.stream_seq, "before attach"),
                (missed.stream_seq, "missed live event")
            ]
        );

        let attachment = viewer
            .registry
            .lock()
            .await
            .take_remote_attachment(&session_id)
            .unwrap();
        assert_eq!(attachment.attachment_id, current.attachment_id);
        crate::agent::session_registry::cleanup_installed_remote_attachment(attachment, true).await;
        mesh.deregister_actor(&scoped_session(&scope, &session_id));
        mesh.deregister_actor(&manager_name);
    }

    /// The owner restarts while this node still holds an attachment to the
    /// old session actor. Loading through that reused attachment must replace
    /// it and load again instead of failing with `remote_transfer_failed`.
    #[tokio::test]
    async fn load_replaces_reused_attachment_whose_owner_actor_is_gone() {
        use crate::agent::remote::scope::{MeshScopeId, scoped_session};
        use crate::agent::remote::test_helpers::fixtures::get_test_mesh;
        use crate::session::projection::NewDurableEvent;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // The owner's session is found through its DHT publication only, so
        // the test does not depend on the node-manager name that every test
        // on the shared mesh registers under the same peer id.
        let mesh = get_test_mesh().await;
        let (owner, _owner_storage, _t1) = handle_with_real_storage().await;
        let (viewer, storage, _t2) = handle_with_real_storage().await;
        viewer.set_mesh(mesh.clone());
        let node_id = mesh.peer_id().to_string();
        let session_id = format!("owner-restart-{}", uuid::Uuid::now_v7());
        let published = scoped_session(&MeshScopeId::lan_default(), &session_id);
        owner
            .config
            .event_sink
            .journal()
            .append_durable(&NewDurableEvent {
                session_id: session_id.clone(),
                origin: querymt_agent_core::events::EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                session_seq: None,
                kind: AgentEventKind::PromptReceived {
                    slash_command: None,
                    content: "before owner restart".into(),
                    message_id: None,
                },
            })
            .await
            .unwrap();
        let spawn_owner_actor = || {
            SessionActor::spawn(
                SessionActor::new(
                    owner.config.clone(),
                    session_id.clone(),
                    SessionRuntime::new(
                        None,
                        Default::default(),
                        crate::agent::core::McpToolState::empty(),
                    ),
                )
                .with_mesh(Some(mesh.clone())),
            )
        };
        let before_restart = spawn_owner_actor();
        mesh.register_actor(before_restart.clone(), published.clone())
            .await;
        storage
            .session_store()
            .save_remote_session_bookmark(&test_bookmark(&session_id, &node_id))
            .await
            .unwrap();

        let first = viewer
            .ensure_remote_session_connected(&session_id, None, RemoteConnectReason::Open)
            .await
            .unwrap();
        assert_eq!(first.outcome, RemoteConnectOutcome::Reattached);
        viewer
            .sync_remote_for_load(&session_id, &first, false)
            .await
            .unwrap();

        // "Restart" the owner: the old session actor is gone and a new one
        // is published under the session's name.
        mesh.deregister_actor(&published);
        before_restart.kill();
        before_restart.wait_for_shutdown().await;
        let after_restart = spawn_owner_actor();
        mesh.register_actor(after_restart.clone(), published.clone())
            .await;

        // The viewer still believes in the old generation (the link death
        // may never be observed across a fast restart).
        let stale = ConnectedRemoteSession {
            session_ref: first.session_ref.clone(),
            attachment_id: first.attachment_id,
            bookmark: first.bookmark.clone(),
            outcome: RemoteConnectOutcome::Reused,
        };
        let dead = stale.session_ref.get_mode().await.unwrap_err();
        assert_eq!(
            dead.transport_failure().map(|failure| failure.kind),
            Some(querymt_remote::RemoteTransportFailureKind::ActorUnavailable),
        );
        assert!(stale.reused_attachment_failed(&dead));
        let fresh = ConnectedRemoteSession {
            outcome: RemoteConnectOutcome::Resumed,
            ..stale
        };
        assert!(
            !fresh.reused_attachment_failed(&dead),
            "a freshly verified attachment is not replaced again"
        );
        assert!(!fresh.reused_attachment_failed(&AgentError::MeshAccessDenied));
        let stale = ConnectedRemoteSession {
            outcome: RemoteConnectOutcome::Reused,
            ..fresh
        };

        let config_calls = AtomicUsize::new(0);
        let (snapshot, (), config) = viewer
            .load_connected_remote_session(
                &session_id,
                stale,
                None,
                || async {},
                || async {
                    config_calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<Vec<u8>, agent_client_protocol::Error>(vec![1])
                },
            )
            .await
            .unwrap_or_else(|error| panic!("load after owner restart failed: {error:?}"));
        assert!(
            snapshot.to_string().contains("before owner restart"),
            "the snapshot carries the owner's history"
        );
        assert_eq!(config, vec![1]);
        assert_eq!(
            config_calls.load(Ordering::SeqCst),
            2,
            "the whole load is repeated on the replacement attachment"
        );
        let current = viewer
            .registry
            .lock()
            .await
            .remote_attachment_id(&session_id)
            .expect("a replacement attachment is installed");
        assert_ne!(current, first.attachment_id);

        let attachment = viewer
            .registry
            .lock()
            .await
            .take_remote_attachment(&session_id)
            .unwrap();
        crate::agent::session_registry::cleanup_installed_remote_attachment(attachment, true).await;
        mesh.deregister_actor(&published);
        after_restart.kill();
    }

    #[tokio::test]
    async fn conflicting_node_hint_cannot_change_bookmark() {
        let (handle, storage, _tmp) = handle_with_real_storage().await;
        let bookmark = test_bookmark("owner-check", "owner");
        storage
            .session_store()
            .save_remote_session_bookmark(&bookmark)
            .await
            .unwrap();
        let error = handle
            .ensure_remote_session_connected(
                "owner-check",
                Some("other"),
                RemoteConnectReason::Open,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "session_location_conflict");
        assert_eq!(
            storage
                .session_store()
                .get_remote_session_bookmark("owner-check")
                .await
                .unwrap(),
            Some(bookmark)
        );
    }

    // ── Coordinator error-path tests (no mesh) ──────────────────────────

    #[tokio::test]
    async fn owner_acp_loads_cold_peer_session_with_local_cwd() {
        use crate::session::domain_schedule::{ScheduleOrigin, SchedulePeerProtocol};

        let (handle, storage, tmp) = handle_with_real_storage().await;
        let origin = ScheduleOrigin::Peer {
            host: "owner".into(),
            peer: "creator".into(),
            protocol: SchedulePeerProtocol::V1,
        };
        let session = handle
            .config
            .provider
            .create_peer_session(origin.clone(), &handle.config.execution_config_snapshot())
            .await
            .unwrap();
        let session_id = &session.session().public_id;
        let request = || {
            crate::acp::protocol::LoadSessionRequest::new(session_id.clone(), tmp.path().to_owned())
        };
        let Err(error) = handle
            .session_materializer
            .prepare_peer_load_session(request(), &handle.registry)
            .await
        else {
            panic!("a peer cannot cold-load a confined session with a workspace");
        };
        assert_eq!(error.data.unwrap()["code"], "mesh_access_denied");

        handle.load_session(request()).await.unwrap();
        assert_eq!(
            storage
                .session_store()
                .peer_session_origin(session_id)
                .await
                .unwrap(),
            Some(origin),
        );
    }

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

        store
            .save_remote_session_bookmark(&test_bookmark(&session.public_id, "node-abc"))
            .await
            .expect("save conflicting bookmark");
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
