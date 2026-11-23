//! Token accounting types shared by mesh hosts and durable stores.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::{GrantStoreError, PeerGrantKey, SharingProtocol, valid_id};

mod store;
pub use store::{AccessEdit, DispatchedAttempt, QuotaStore, ReservedAttempt};

/// Accumulates usage only when an upstream stream has reached a semantic terminal.
#[derive(Default)]
pub struct StreamMeter {
    usage: Option<querymt::Usage>,
    terminal: bool,
}

impl StreamMeter {
    pub fn observe(&mut self, chunk: &querymt::chat::StreamChunk) {
        use querymt::chat::{StreamChunk, StructuredStreamEvent, chunk_is_terminal};
        self.terminal |= chunk_is_terminal(chunk);
        match chunk {
            StreamChunk::Usage(usage) => {
                self.usage = Some(
                    self.usage
                        .take()
                        .map_or_else(|| usage.clone(), |current| current.merge_max(usage.clone())),
                );
            }
            StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                usage: Some(usage),
                ..
            }) => {
                // Unlike in-progress metadata, terminal usage is authoritative.
                self.usage = Some(usage.clone());
            }
            _ => {}
        }
    }

    pub fn has_complete_usage(&self) -> bool {
        matches!(self.complete_usage(), AttemptUsage::Observed(_))
    }

    pub fn complete_usage(&self) -> AttemptUsage {
        match (self.terminal, &self.usage) {
            (true, Some(usage)) => AttemptUsage::from_usage(usage),
            _ => AttemptUsage::Unknown,
        }
    }
}

/// A provider-wide or model-specific quota on a particular host and peer.
#[typeshare::typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaScope {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

impl QuotaScope {
    /// Whether this scope covers the resolved binding.
    pub fn covers(&self, provider: &str, model: &str) -> bool {
        self.provider_id == provider && self.model_id.as_deref().is_none_or(|id| id == model)
    }

    /// Check identifiers before using them as storage keys.
    pub fn validate(&self) -> Result<(), QuotaError> {
        if valid_id(&self.provider_id) && self.model_id.as_deref().is_none_or(valid_id) {
            Ok(())
        } else {
            Err(QuotaError::InvalidScope)
        }
    }
}

/// UTC calendar window of a finite allowance.
#[typeshare::typeshare]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindow {
    #[default]
    Manual,
    Daily,
    Weekly,
    Monthly,
}

/// Exact allowance in whole tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuotaLimit {
    #[default]
    Unlimited,
    Finite {
        #[serde(rename = "limit_tokens", with = "token_string")]
        tokens: i64,
        window: QuotaWindow,
    },
}

mod token_string {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(tokens: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(tokens)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        let value = String::deserialize(deserializer)?;
        value
            .parse::<i64>()
            .ok()
            .filter(|tokens| *tokens > 0 && value.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| D::Error::custom("expected a positive whole-token count"))
    }
}

impl QuotaLimit {
    /// Refuse negative and zero caps rather than interpreting them as unlimited.
    pub fn validate(self) -> Result<(), QuotaError> {
        match self {
            Self::Finite { tokens, .. } if tokens <= 0 => Err(QuotaError::InvalidTokens),
            _ => Ok(()),
        }
    }
}

/// Changing an existing amount does not implicitly start a new accounting period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaEdit {
    Update(QuotaLimit),
    Reconfigure(QuotaLimit),
    Reset,
}

/// Current durable usage for one quota scope. Timestamps are Unix seconds UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatus {
    pub scope: QuotaScope,
    pub limit: QuotaLimit,
    pub known_tokens: i64,
    pub estimated_tokens: i64,
    pub has_unknown_usage: bool,
    pub reserved_tokens: i64,
    pub available_tokens: Option<i64>,
    pub window_started_at: Option<i64>,
    pub next_reset_at: Option<i64>,
}

/// An authorized inference preparation. The caller must derive the peer from the
/// transport and use host-resolved IDs and a host-computed estimate.
#[derive(Debug, Clone)]
pub struct QuotaAttempt {
    pub peer: PeerGrantKey,
    /// Sharing protocol used to authorize the initiating peer.
    pub protocol: SharingProtocol,
    pub provider_id: String,
    pub model_id: String,
    /// Stable for retransmissions; a new upstream attempt needs a new ID.
    pub request_id: Uuid,
    pub estimate: QuotaEstimate,
    /// Host wall clock at admission, in Unix seconds UTC.
    pub now: i64,
}

/// Host-side estimate used for reservation and missing-usage accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaEstimate {
    pub reserved_tokens: i64,
    /// Stored host estimate; zero is a valid estimate, not an absent value.
    pub prompt_tokens: i64,
}

/// Handle returned by the store for a newly reserved upstream attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationId(pub Uuid);

/// One limiting scope reported by preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitingQuota {
    pub scope: QuotaScope,
    pub available_tokens: i64,
    pub next_reset_at: Option<i64>,
}

/// Accounting decision for a dispatched upstream attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptUsage {
    /// Complete provider-reported usage after normalization.
    Observed(i64),
    /// Incomplete or missing authoritative usage after dispatch.
    Unknown,
}

impl AttemptUsage {
    /// Sum normalized usage, counting cache reads and aggregate writes once.
    pub fn from_usage(usage: &querymt::Usage) -> Self {
        Self::Observed(
            [
                usage.input_tokens,
                usage.output_tokens,
                usage.reasoning_tokens,
                usage.cache_read,
                usage.cache_write,
            ]
            .into_iter()
            .map(i64::from)
            .sum(),
        )
    }
}

/// Code-only policy for charging missing usage in finite quota periods.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnknownUsageCharge {
    /// Charge the full reservation.
    #[default]
    Reserved,
    /// Charge the stored prompt estimate, including zero if estimated as zero.
    EstimatedPrompt,
}

/// Result of committing an attempt's settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSettlement {
    Recorded,
    AlreadyFinal,
}

/// A rejected quota mutation or inference admission.
#[derive(Debug, Error)]
pub enum QuotaError {
    #[error(transparent)]
    Store(#[from] GrantStoreError),
    #[error("invalid quota scope")]
    InvalidScope,
    #[error("invalid whole-token count")]
    InvalidTokens,
    #[error("invalid quota clock value")]
    InvalidTime,
    #[error("access denied")]
    Denied,
    #[error("quota exhausted")]
    Exhausted(Vec<LimitingQuota>),
    #[error("quota kind or window changes require explicit reconfiguration")]
    ReconfigureRequired,
    #[error("scope has outstanding attempts")]
    InFlight,
    #[error("quota is not finite")]
    NotFinite,
    #[error("request already has an accounting record")]
    DuplicateAttempt,
    #[error("invalid accounting attempt transition")]
    InvalidAttempt,
}

/// Details retained across the provider wire and promoted to the ACP error.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum QuotaDenialDetails {
    Limits { limits: Vec<LimitingQuota> },
}

const DENIAL_DETAILS_PREFIX: &str = "querymt.mesh.quota.v1:";

impl QuotaDenialDetails {
    /// Decode only the metadata matching this mesh policy error's code.
    pub fn from_provider_metadata(code: &str, error_type: Option<&str>) -> Option<Self> {
        if code != "mesh_quota_exhausted" {
            return None;
        }
        let payload = error_type?.strip_prefix(DENIAL_DETAILS_PREFIX)?;
        serde_json::from_str(payload).ok()
    }
}

impl QuotaError {
    /// Preserve the same classification and limiting scopes for direct and hosted dispatch.
    pub fn into_provider_error(self) -> querymt::error::LLMError {
        use querymt::error::{ProviderErrorKind, ProviderFailure};
        let (kind, code) = match &self {
            Self::Denied | Self::InvalidAttempt => {
                (ProviderErrorKind::Authentication, "mesh_access_denied")
            }
            Self::Exhausted(_) => (ProviderErrorKind::QuotaExceeded, "mesh_quota_exhausted"),
            Self::DuplicateAttempt => (
                ProviderErrorKind::InvalidRequest,
                "mesh_quota_duplicate_attempt",
            ),
            _ => {
                tracing::warn!(error = %self, "mesh quota operation failed");
                (
                    ProviderErrorKind::UnknownPermanent,
                    "mesh_access_unavailable",
                )
            }
        };
        let message = if code == "mesh_access_unavailable" {
            "mesh policy unavailable".into()
        } else {
            self.to_string()
        };
        let details = match self {
            Self::Exhausted(limits) => Some(QuotaDenialDetails::Limits { limits }),
            _ => None,
        };
        // Keep the native provider error layout unchanged. The existing optional
        // metadata field carries a namespaced wire value, not display text.
        let metadata = details
            .and_then(|details| serde_json::to_string(&details).ok())
            .map(|encoded| format!("{DENIAL_DETAILS_PREFIX}{encoded}"));
        ProviderFailure::new(kind, message)
            .with_code(Some(code.into()))
            .with_error_type(metadata)
            .into()
    }
}

pub(crate) use ledger::{QuotaLedger, QuotaRuntime};

mod ledger {
    use std::collections::{BTreeSet, HashMap, VecDeque};
    use std::sync::Arc;

    use async_trait::async_trait;
    use parking_lot::Mutex;
    use time::{Date, Duration, Month, OffsetDateTime};
    use tokio::sync::broadcast;

    use super::*;
    use crate::NodeId;
    use crate::mesh_state::{AccessChange, MeshAccessStore, PeerAccessState};
    use crate::peer_access::{PeerGrant, PeerGrantState, PolicyChange, PolicyEffect};

    /// Durable allowances, settled usage and replay keys. No open attempt is serialized.
    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub(crate) struct QuotaLedger {
        scopes: Vec<ScopeLedger>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct ScopeLedger {
        host: NodeId,
        peer: NodeId,
        scope: QuotaScope,
        // Newest first. Scope slots are never removed, and generations never repeat
        // within a slot, so in-memory reservations can reference their old periods.
        periods: Vec<Period>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Period {
        generation: u64,
        limit: QuotaLimit,
        started_at: i64,
        next_reset_at: Option<i64>,
        known_tokens: i64,
        estimated_tokens: i64,
        has_unknown_usage: bool,
        #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
        replay_keys: BTreeSet<String>,
    }

    /// State shared by all quota operations under the mesh state owner's lock.
    #[derive(Clone, Default)]
    pub(crate) struct QuotaRuntime {
        open: HashMap<Uuid, OpenAttempt>,
        finalized: VecDeque<FinalizedAttempt>,
    }

    #[derive(Clone)]
    struct OpenAttempt {
        request: QuotaAttempt,
        dispatched: bool,
        reservations: Vec<Reservation>,
        policy: Arc<Mutex<AttemptPolicy>>,
    }

    #[derive(Debug)]
    struct AttemptPolicy {
        changes: broadcast::Receiver<PolicyChange>,
        require_sessions: bool,
        lost: bool,
    }

    impl AttemptPolicy {
        fn permits(&mut self, request: &QuotaAttempt, grant: &PeerGrant) -> bool {
            use broadcast::error::TryRecvError;

            let require_sessions = self.require_sessions;
            let allows = |grant: &PeerGrant| {
                grant.allows_model(request.protocol, &request.provider_id, &request.model_id)
                    && (!require_sessions || grant.allows_sessions(request.protocol))
            };
            self.lost |= !allows(grant);
            while !self.lost {
                match self.changes.try_recv() {
                    Ok(change) if change.key == request.peer => {
                        let PolicyEffect::Grant(grant) = change.effect;
                        self.lost = !allows(&grant);
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Lagged(_) | TryRecvError::Closed) => self.lost = true,
                    Err(TryRecvError::Empty) => break,
                }
            }
            !self.lost
        }
    }

    #[derive(Clone, Copy)]
    struct Reservation {
        slot: usize,
        generation: u64,
        reserved: Option<i64>,
    }

    #[derive(Clone)]
    struct FinalizedAttempt {
        id: Uuid,
        released: bool,
        // A Full grant has no ledger scopes. Its request deduplication is runtime-only.
        full_request: Option<(PeerGrantKey, Uuid)>,
    }

    impl QuotaRuntime {
        fn finalize(&mut self, id: Uuid, attempt: OpenAttempt, released: bool) {
            const MAX_FINALIZED: usize = 4096;
            if self.finalized.len() == MAX_FINALIZED {
                self.finalized.pop_front();
            }
            self.finalized.push_back(FinalizedAttempt {
                id,
                released,
                full_request: attempt
                    .reservations
                    .is_empty()
                    .then_some((attempt.request.peer, attempt.request.request_id)),
            });
        }

        fn finalized(&self, id: Uuid) -> Option<&FinalizedAttempt> {
            self.finalized.iter().rev().find(|entry| entry.id == id)
        }

        fn contains_request(&self, key: &PeerGrantKey, id: Uuid) -> bool {
            self.open
                .values()
                .any(|attempt| &attempt.request.peer == key && attempt.request.request_id == id)
                || self.finalized.iter().rev().any(|entry| {
                    entry
                        .full_request
                        .as_ref()
                        .is_some_and(|(peer, request)| peer == key && *request == id)
                })
        }

        fn reservations(&self, slot: usize, generation: u64) -> impl Iterator<Item = &Reservation> {
            self.open
                .values()
                .flat_map(|attempt| &attempt.reservations)
                .filter(move |r| r.slot == slot && r.generation == generation)
        }

        fn covers_open_attempt(&self, key: &PeerGrantKey, scope: &QuotaScope) -> bool {
            self.open.values().any(|attempt| {
                let req = &attempt.request;
                &req.peer == key && scope.covers(&req.provider_id, &req.model_id)
            })
        }
    }

    impl QuotaLedger {
        fn slot(&self, key: &PeerGrantKey, scope: &QuotaScope) -> Option<usize> {
            self.scopes
                .iter()
                .position(|entry| entry.belongs_to(key) && &entry.scope == scope)
        }

        fn ensure_slot(&mut self, key: &PeerGrantKey, scope: QuotaScope) -> usize {
            self.slot(key, &scope).unwrap_or_else(|| {
                let slot = self.scopes.len();
                self.scopes.push(ScopeLedger {
                    host: key.host.clone(),
                    peer: key.peer.clone(),
                    scope,
                    periods: Vec::new(),
                });
                slot
            })
        }

        fn contains_request(&self, key: &PeerGrantKey, request_id: Uuid) -> bool {
            let id = request_id.to_string();
            self.scopes
                .iter()
                .filter(|entry| entry.belongs_to(key))
                .any(|entry| {
                    entry
                        .periods
                        .iter()
                        .take(2)
                        .any(|period| period.replay_keys.contains(&id))
                })
        }

        fn record(
            &mut self,
            attempt: &OpenAttempt,
            usage: Option<AttemptUsage>,
            charge: UnknownUsageCharge,
        ) -> Result<bool, QuotaError> {
            let id = attempt.request.request_id.to_string();
            let mut changed = false;
            for reservation in &attempt.reservations {
                let entry = self
                    .scopes
                    .get_mut(reservation.slot)
                    .ok_or_else(missing_period)?;
                let (index, period) = entry
                    .periods
                    .iter_mut()
                    .enumerate()
                    .find(|(_, period)| period.generation == reservation.generation)
                    .ok_or_else(missing_period)?;
                if index < 2 {
                    changed |= period.replay_keys.insert(id.clone());
                }
                match usage {
                    Some(AttemptUsage::Observed(actual)) => {
                        period.known_tokens = period.known_tokens.saturating_add(actual);
                    }
                    Some(AttemptUsage::Unknown) => match reservation.reserved {
                        Some(reserved) => {
                            let estimate = match charge {
                                UnknownUsageCharge::Reserved => reserved,
                                UnknownUsageCharge::EstimatedPrompt => {
                                    attempt.request.estimate.prompt_tokens
                                }
                            };
                            period.estimated_tokens =
                                period.estimated_tokens.saturating_add(estimate);
                        }
                        None => period.has_unknown_usage = true,
                    },
                    None => {}
                }
                changed |= usage.is_some();
            }
            Ok(changed)
        }
    }

    fn missing_period() -> GrantStoreError {
        GrantStoreError::Backend(
            std::io::Error::other("quota reservation period is missing").into(),
        )
    }

    impl ScopeLedger {
        fn belongs_to(&self, key: &PeerGrantKey) -> bool {
            self.host == key.host && self.peer == key.peer
        }

        fn open_period(
            &mut self,
            limit: QuotaLimit,
            started_at: i64,
            next_reset_at: Option<i64>,
        ) -> Result<(), QuotaError> {
            let generation = match self.periods.first() {
                None => 0,
                Some(current) => current.generation.checked_add(1).ok_or_else(|| {
                    GrantStoreError::Backend(
                        std::io::Error::other("quota period sequence exhausted").into(),
                    )
                })?,
            };
            self.periods.insert(
                0,
                Period {
                    generation,
                    limit,
                    started_at,
                    next_reset_at,
                    known_tokens: 0,
                    estimated_tokens: 0,
                    has_unknown_usage: false,
                    replay_keys: BTreeSet::new(),
                },
            );
            Ok(())
        }

        fn prune(&mut self, runtime: &QuotaRuntime, slot: usize) -> bool {
            let mut index = 0;
            let mut changed = false;
            self.periods.retain_mut(|period| {
                index += 1;
                if index <= 2 {
                    return true;
                }
                if runtime
                    .reservations(slot, period.generation)
                    .next()
                    .is_none()
                {
                    changed = true;
                    return false;
                }
                changed |= !period.replay_keys.is_empty();
                period.replay_keys.clear();
                true
            });
            changed
        }

        fn roll_forward(
            &mut self,
            runtime: &QuotaRuntime,
            slot: usize,
            now: i64,
        ) -> Result<bool, QuotaError> {
            let mut changed = false;
            if let Some(period) = self.periods.first()
                && let QuotaLimit::Finite { window, .. } = period.limit
                && period.next_reset_at.is_some_and(|reset| now >= reset)
                && let Some((start, end)) = boundaries(window, now)?
            {
                let limit = period.limit;
                if period.next_reset_at.is_some_and(|reset| reset < start) {
                    // The last stored period predates the previous calendar window.
                    if let Some((previous, _)) = boundaries(window, start - 1)? {
                        self.open_period(limit, previous, Some(start))?;
                    }
                }
                self.open_period(limit, start, Some(end))?;
                changed = true;
            }
            Ok(changed | self.prune(runtime, slot))
        }
    }

    fn apply_quota_edit(
        state: &mut PeerAccessState,
        runtime: &QuotaRuntime,
        key: &PeerGrantKey,
        scope: QuotaScope,
        edit: QuotaEdit,
        now: i64,
    ) -> Result<(bool, bool), QuotaError> {
        let slot = state.quotas.ensure_slot(key, scope);
        let entry = &mut state.quotas.scopes[slot];
        let rolled = entry.roll_forward(runtime, slot, now)?;
        let current = entry.periods.first_mut();
        if let QuotaEdit::Update(limit) = edit {
            let Some(period) = current else {
                return Err(QuotaError::ReconfigureRequired);
            };
            if !matches!((period.limit, limit), (QuotaLimit::Finite { window, .. }, QuotaLimit::Finite { window: proposed, .. }) if window == proposed)
            {
                return Err(QuotaError::ReconfigureRequired);
            }
            let changed = period.limit != limit;
            period.limit = limit;
            return Ok((changed, rolled));
        }
        let clock = current
            .as_ref()
            .map_or(now, |period| now.max(period.started_at));
        let (limit, next) = match edit {
            QuotaEdit::Reset => match current {
                Some(period) if matches!(period.limit, QuotaLimit::Finite { .. }) => {
                    (period.limit, period.next_reset_at)
                }
                _ => return Err(QuotaError::NotFinite),
            },
            QuotaEdit::Reconfigure(limit) => {
                let next = match limit {
                    QuotaLimit::Unlimited => None,
                    QuotaLimit::Finite { window, .. } => {
                        boundaries(window, clock)?.map(|(_, end)| end)
                    }
                };
                (limit, next)
            }
            QuotaEdit::Update(_) => unreachable!(),
        };
        if runtime.covers_open_attempt(key, &entry.scope) {
            return Err(QuotaError::InFlight);
        }
        entry.open_period(limit, clock, next)?;
        entry.prune(runtime, slot);
        Ok((true, rolled))
    }

    fn utc_date(now: i64) -> Result<Date, QuotaError> {
        OffsetDateTime::from_unix_timestamp(now)
            .map(|dt| dt.date())
            .map_err(|_| QuotaError::InvalidTime)
    }

    fn boundaries(window: QuotaWindow, now: i64) -> Result<Option<(i64, i64)>, QuotaError> {
        let day = utc_date(now)?;
        let (start, end) = match window {
            QuotaWindow::Manual => return Ok(None),
            QuotaWindow::Daily => (day, day.next_day()),
            QuotaWindow::Weekly => {
                let monday = day
                    .checked_sub(Duration::days(i64::from(
                        day.weekday().number_days_from_monday(),
                    )))
                    .ok_or(QuotaError::InvalidTime)?;
                (monday, monday.checked_add(Duration::days(7)))
            }
            QuotaWindow::Monthly => {
                let first = Date::from_calendar_date(day.year(), day.month(), 1);
                let next = if day.month() == Month::December {
                    Date::from_calendar_date(day.year() + 1, Month::January, 1)
                } else {
                    Date::from_calendar_date(day.year(), day.month().next(), 1)
                };
                (first.map_err(|_| QuotaError::InvalidTime)?, next.ok())
            }
        };
        let end = end.ok_or(QuotaError::InvalidTime)?;
        Ok(Some((
            start.midnight().assume_utc().unix_timestamp(),
            end.midnight().assume_utc().unix_timestamp(),
        )))
    }

    fn status(
        scope: QuotaScope,
        period: &Period,
        runtime: &QuotaRuntime,
        slot: usize,
    ) -> QuotaStatus {
        let mut status = QuotaStatus {
            scope,
            limit: period.limit,
            known_tokens: period.known_tokens,
            estimated_tokens: period.estimated_tokens,
            has_unknown_usage: period.has_unknown_usage,
            reserved_tokens: 0,
            available_tokens: None,
            window_started_at: None,
            next_reset_at: None,
        };
        if let QuotaLimit::Finite { tokens, .. } = period.limit {
            let reserved = runtime
                .reservations(slot, period.generation)
                .filter_map(|r| r.reserved)
                .fold(0_i64, i64::saturating_add);
            status.reserved_tokens = reserved;
            // A saturated total already consumes any valid cap, so saturation
            // cannot cause admission after an overshoot past i64::MAX.
            status.available_tokens = Some(
                tokens
                    .saturating_sub(period.known_tokens)
                    .saturating_sub(period.estimated_tokens)
                    .saturating_sub(reserved)
                    .max(0),
            );
            status.window_started_at = Some(period.started_at);
            status.next_reset_at = period.next_reset_at;
        }
        status
    }

    fn snapshot_quotas(
        state: &mut PeerAccessState,
        runtime: &QuotaRuntime,
        key: &PeerGrantKey,
        now: i64,
    ) -> Result<(Vec<QuotaStatus>, bool), QuotaError> {
        let mut quotas = Vec::new();
        let mut persist = false;
        for (slot, entry) in state
            .quotas
            .scopes
            .iter_mut()
            .enumerate()
            .filter(|(_, entry)| entry.belongs_to(key))
        {
            persist |= entry.roll_forward(runtime, slot, now)?;
            if let Some(period) = entry.periods.first() {
                quotas.push(status(entry.scope.clone(), period, runtime, slot));
            }
        }
        quotas.sort_by(|a, b| {
            (&a.scope.provider_id, &a.scope.model_id)
                .cmp(&(&b.scope.provider_id, &b.scope.model_id))
        });
        Ok((quotas, persist))
    }

    #[async_trait]
    impl QuotaStore for MeshAccessStore {
        async fn access_snapshot(
            &self,
            key: &PeerGrantKey,
            now: i64,
        ) -> Result<(PeerGrantState, Vec<QuotaStatus>), QuotaError> {
            utc_date(now)?;
            let key = key.clone();
            self.transact(move |state, runtime| {
                let (quotas, persist) = snapshot_quotas(state, runtime, &key, now)?;
                Ok(((state.grant(&key), quotas), persist))
            })
            .await
        }

        async fn edit_access(
            &self,
            key: &PeerGrantKey,
            expected: Uuid,
            edit: AccessEdit,
            now: i64,
        ) -> Result<(PeerGrantState, Vec<QuotaStatus>), QuotaError> {
            utc_date(now)?;
            match &edit {
                AccessEdit::Grant(grant) => grant.validate().map_err(GrantStoreError::from)?,
                AccessEdit::Quota(scope, edit) => {
                    scope.validate()?;
                    if let QuotaEdit::Update(limit) | QuotaEdit::Reconfigure(limit) = *edit {
                        limit.validate()?;
                    }
                }
            }
            let key = key.clone();
            self.transact_access(move |state, runtime| {
                let mut grant = state.grant(&key);
                if expected != grant.revision {
                    return Err(GrantStoreError::Conflict {
                        expected,
                        actual: grant.revision,
                    }
                    .into());
                }
                let mut change = None;
                let (edited, rolled_target) = match edit {
                    AccessEdit::Grant(next) if next == grant.grant => (false, false),
                    AccessEdit::Grant(next) => {
                        change = Some(AccessChange::Policy(PolicyChange {
                            key: key.clone(),
                            effect: PolicyEffect::Grant(next.clone()),
                        }));
                        grant.grant = next;
                        (true, false)
                    }
                    AccessEdit::Quota(scope, edit) => {
                        apply_quota_edit(state, runtime, &key, scope, edit, now)?
                    }
                };
                if edited {
                    grant.revision = Uuid::now_v7();
                    state.put_grant(&key, grant.clone());
                }
                let (quotas, rolled) = snapshot_quotas(state, runtime, &key, now)?;
                Ok(((grant, quotas), edited || rolled || rolled_target, change))
            })
            .await
        }

        async fn reserve(&self, attempt: QuotaAttempt) -> Result<ReservationId, QuotaError> {
            let changes = self.peer_policy_history(&attempt.peer);
            self.reserve_with_policy(attempt, changes, false).await
        }

        async fn reserve_with_policy(
            &self,
            attempt: QuotaAttempt,
            changes: broadcast::Receiver<PolicyChange>,
            require_sessions: bool,
        ) -> Result<ReservationId, QuotaError> {
            utc_date(attempt.now)?;
            let model = QuotaScope {
                provider_id: attempt.provider_id.clone(),
                model_id: Some(attempt.model_id.clone()),
            };
            model.validate()?;
            let provider = QuotaScope {
                provider_id: attempt.provider_id.clone(),
                model_id: None,
            };
            self.transact_with_usage::<_, QuotaError>(move |state, runtime| {
                let grant = state.grant(&attempt.peer).grant;
                let mut policy = AttemptPolicy {
                    changes,
                    require_sessions,
                    lost: false,
                };
                if !policy.permits(&attempt, &grant) {
                    return Err(QuotaError::Denied);
                }
                let mut persist = false;
                // Request IDs are peer-scoped, not binding-scoped. Age every
                // scope that could have retained this request before checking replay.
                for (slot, entry) in state.quotas.scopes.iter_mut().enumerate() {
                    if entry.belongs_to(&attempt.peer) {
                        persist |= entry.roll_forward(runtime, slot, attempt.now)?;
                    }
                }
                if runtime.contains_request(&attempt.peer, attempt.request_id)
                    || state
                        .quotas
                        .contains_request(&attempt.peer, attempt.request_id)
                {
                    return Ok((Err(QuotaError::DuplicateAttempt), persist, None));
                }
                let mut reservations = Vec::new();
                let mut limiting = Vec::new();
                if grant != PeerGrant::Full {
                    for scope in [provider, model] {
                        let slot = state.quotas.ensure_slot(&attempt.peer, scope);
                        let entry = &mut state.quotas.scopes[slot];
                        if entry.periods.is_empty() {
                            entry.open_period(QuotaLimit::Unlimited, attempt.now, None)?;
                            persist = true;
                        }
                        let period = &entry.periods[0];
                        let mut reserved = None;
                        if matches!(period.limit, QuotaLimit::Finite { .. }) {
                            let status = status(entry.scope.clone(), period, runtime, slot);
                            if status.available_tokens == Some(0) {
                                limiting.push(LimitingQuota {
                                    scope: status.scope,
                                    available_tokens: 0,
                                    next_reset_at: status.next_reset_at,
                                });
                            }
                            reserved = Some(attempt.estimate.reserved_tokens);
                        }
                        reservations.push(Reservation {
                            slot,
                            generation: period.generation,
                            reserved,
                        });
                    }
                }
                if !limiting.is_empty() {
                    // Commit any observed rollover without recording either reservation.
                    return Ok((Err(QuotaError::Exhausted(limiting)), persist, None));
                }
                let finite = reservations.iter().any(|r| r.reserved.is_some());
                if finite
                    && (attempt.estimate.prompt_tokens < 0 || attempt.estimate.reserved_tokens <= 0)
                {
                    return Err(QuotaError::InvalidTokens);
                }
                let notify = finite.then(|| attempt.peer.clone());
                let id = Uuid::now_v7();
                runtime.open.insert(
                    id,
                    OpenAttempt {
                        request: attempt,
                        dispatched: false,
                        reservations,
                        policy: Arc::new(Mutex::new(policy)),
                    },
                );
                Ok((Ok(ReservationId(id)), persist, notify))
            })
            .await?
        }

        async fn mark_dispatched(&self, attempt: ReservationId) -> Result<(), QuotaError> {
            self.transact(move |state, runtime| {
                let open = runtime
                    .open
                    .get_mut(&attempt.0)
                    .filter(|open| !open.dispatched)
                    .ok_or(QuotaError::InvalidAttempt)?;
                let req = &open.request;
                let grant = state.grant(&req.peer).grant;
                // An attempt admitted as Full cannot start under a subsequent Custom grant.
                if !open.policy.lock().permits(req, &grant)
                    || (open.reservations.is_empty() && grant != PeerGrant::Full)
                {
                    return Err(QuotaError::InvalidAttempt);
                }
                open.dispatched = true;
                Ok(((), false))
            })
            .await
        }

        async fn release_unused(&self, attempt: ReservationId) -> Result<(), QuotaError> {
            let charge = self.unknown_usage_charge;
            self.transact_with_usage(move |state, runtime| {
                if runtime
                    .finalized(attempt.0)
                    .is_some_and(|entry| entry.released)
                {
                    return Ok(((), false, None));
                }
                let open = runtime
                    .open
                    .remove(&attempt.0)
                    .filter(|open| !open.dispatched)
                    .ok_or(QuotaError::InvalidAttempt)?;
                let notify = open
                    .reservations
                    .iter()
                    .any(|reservation| {
                        reservation.reserved.is_some()
                            && state
                                .quotas
                                .scopes
                                .get(reservation.slot)
                                .and_then(|entry| entry.periods.first())
                                .is_some_and(|p| p.generation == reservation.generation)
                            && runtime
                                .reservations(reservation.slot, reservation.generation)
                                .filter_map(|r| r.reserved)
                                .fold(0_i64, i64::saturating_add)
                                < i64::MAX
                    })
                    .then(|| open.request.peer.clone());
                let persist = state.quotas.record(&open, None, charge)?;
                runtime.finalize(attempt.0, open, true);
                Ok(((), persist, notify))
            })
            .await
        }

        fn subscribe_usage_changes(&self) -> Option<broadcast::Receiver<PeerGrantKey>> {
            Some(self.usage_changes.subscribe())
        }

        async fn settle(
            &self,
            attempt: ReservationId,
            usage: AttemptUsage,
        ) -> Result<QuotaSettlement, QuotaError> {
            let charge = self.unknown_usage_charge;
            self.transact_with_usage(move |state, runtime| {
                if runtime.finalized(attempt.0).is_some() {
                    return Ok((QuotaSettlement::AlreadyFinal, false, None));
                }
                let open = runtime
                    .open
                    .remove(&attempt.0)
                    .filter(|open| open.dispatched)
                    .ok_or(QuotaError::InvalidAttempt)?;
                if matches!(usage, AttemptUsage::Observed(actual) if actual < 0) {
                    return Err(QuotaError::InvalidTokens);
                }
                let persist = state.quotas.record(&open, Some(usage), charge)?;
                let notify = persist.then(|| open.request.peer.clone());
                runtime.finalize(attempt.0, open, false);
                Ok((QuotaSettlement::Recorded, persist, notify))
            })
            .await
        }

        async fn reconcile_startup(&self, host: &NodeId) -> Result<(), QuotaError> {
            let host = host.clone();
            let now = OffsetDateTime::now_utc().unix_timestamp();
            self.transact(move |state, runtime| {
                let mut persist = false;
                for (slot, entry) in state.quotas.scopes.iter_mut().enumerate() {
                    if entry.host == host {
                        persist |= entry.roll_forward(runtime, slot, now)?;
                    }
                }
                Ok(((), persist))
            })
            .await
        }
    }

    #[cfg(test)]
    mod tests {
        use std::path::{Path, PathBuf};
        use std::sync::Arc;

        use parking_lot::RwLock;
        use tokio::sync::broadcast::error::TryRecvError;

        use super::*;
        use crate::mesh_state::MeshStateStore;
        use crate::peer_access::{GrantStore, ModelSelection, ProviderGrant};

        const TUESDAY: i64 = 1_709_035_200; // 2024-02-27 12:00 UTC
        const WEDNESDAY: i64 = 1_709_078_400; // 2024-02-28 00:00 UTC
        const FRIDAY: i64 = 1_709_251_200; // 2024-03-01 00:00 UTC
        const MONDAY: i64 = 1_709_510_400; // 2024-03-04 00:00 UTC

        struct Fixture {
            dir: tempfile::TempDir,
            path: PathBuf,
            store: Arc<MeshAccessStore>,
            key: PeerGrantKey,
            custom: PeerGrant,
        }

        impl Fixture {
            async fn new() -> Self {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("mesh_state.json");
                let store = open(&path);
                let key = PeerGrantKey {
                    host: NodeId::parse("12D3KooWJ5MP5JUR1c8pF37dNHGGrVhCGHEHwrCYyowX6dPJtV6t")
                        .unwrap(),
                    peer: NodeId::parse("12D3KooWQKkiEPk7f8Sp2LXj7S2BJMAMePSz23RQPYGVBCGiyhJE")
                        .unwrap(),
                };
                let custom = PeerGrant::Custom {
                    sessions: false,
                    filesystem: false,
                    shell: false,
                    providers: vec![ProviderGrant {
                        provider_id: "provider".into(),
                        models: ModelSelection::All,
                    }],
                };
                store
                    .replace_grant(&key, Uuid::nil(), custom.clone())
                    .await
                    .unwrap();
                Self {
                    dir,
                    path,
                    store,
                    key,
                    custom,
                }
            }

            fn restart(self) -> Self {
                let Self {
                    dir,
                    path,
                    store,
                    key,
                    custom,
                } = self;
                drop(store);
                let store = open(&path);
                Self {
                    dir,
                    path,
                    store,
                    key,
                    custom,
                }
            }

            async fn revision(&self) -> Uuid {
                self.store.grant(&self.key).await.unwrap().revision
            }

            async fn set_grant(&self, grant: PeerGrant) {
                self.store
                    .replace_grant(&self.key, self.revision().await, grant)
                    .await
                    .unwrap();
            }

            async fn edit(
                &self,
                scope: &QuotaScope,
                edit: QuotaEdit,
                now: i64,
            ) -> Result<Uuid, QuotaError> {
                self.store
                    .edit_quota(&self.key, self.revision().await, scope, edit, now)
                    .await
            }

            async fn limit(&self, scope: &QuotaScope, tokens: i64, window: QuotaWindow) -> Uuid {
                self.edit(
                    scope,
                    QuotaEdit::Reconfigure(QuotaLimit::Finite { tokens, window }),
                    TUESDAY,
                )
                .await
                .unwrap()
            }

            fn request(&self, model: &str, reserved_tokens: i64, now: i64) -> QuotaAttempt {
                QuotaAttempt {
                    peer: self.key.clone(),
                    protocol: SharingProtocol::V1,
                    provider_id: "provider".into(),
                    model_id: model.into(),
                    request_id: Uuid::now_v7(),
                    estimate: QuotaEstimate {
                        reserved_tokens,
                        prompt_tokens: reserved_tokens / 2,
                    },
                    now,
                }
            }

            async fn quota(&self, scope: &QuotaScope, now: i64) -> QuotaStatus {
                let (_, quotas) = self.store.access_snapshot(&self.key, now).await.unwrap();
                quotas
                    .into_iter()
                    .find(|status| &status.scope == scope)
                    .unwrap()
            }

            async fn dispatch(&self, req: QuotaAttempt) -> ReservationId {
                let id = self.store.reserve(req).await.unwrap();
                self.store.mark_dispatched(id).await.unwrap();
                id
            }

            async fn history(&self, scope: &QuotaScope) -> Vec<i64> {
                let (key, scope) = (self.key.clone(), scope.clone());
                self.store
                    .transact::<_, QuotaError>(move |state, _| {
                        let slot = state.quotas.slot(&key, &scope).unwrap();
                        Ok((
                            state.quotas.scopes[slot]
                                .periods
                                .iter()
                                .map(|p| p.known_tokens)
                                .collect(),
                            false,
                        ))
                    })
                    .await
                    .unwrap()
            }
        }

        fn open(path: &Path) -> Arc<MeshAccessStore> {
            Arc::new(MeshAccessStore::new(Arc::new(RwLock::new(
                MeshStateStore::load_or_create(path).unwrap(),
            ))))
        }

        fn scope(model: Option<&str>) -> QuotaScope {
            QuotaScope {
                provider_id: "provider".into(),
                model_id: model.map(str::to_owned),
            }
        }

        #[tokio::test]
        async fn parallel_admission_allows_last_request_to_cross_both_caps() {
            let f = Fixture::new().await;
            let (provider, model) = (scope(None), scope(Some("a")));
            f.limit(&provider, 100, QuotaWindow::Manual).await;
            f.limit(&model, 100, QuotaWindow::Daily).await;
            let (a, b, c) = tokio::join!(
                f.store.reserve(f.request("a", 60, TUESDAY)),
                f.store.reserve(f.request("a", 60, TUESDAY)),
                f.store.reserve(f.request("a", 60, TUESDAY)),
            );
            let mut admitted = Vec::new();
            for result in [a, b, c] {
                match result {
                    Ok(id) => admitted.push(id),
                    Err(QuotaError::Exhausted(limits)) => assert_eq!(
                        limits,
                        [
                            LimitingQuota {
                                scope: provider.clone(),
                                available_tokens: 0,
                                next_reset_at: None
                            },
                            LimitingQuota {
                                scope: model.clone(),
                                available_tokens: 0,
                                next_reset_at: Some(WEDNESDAY)
                            },
                        ]
                    ),
                    Err(error) => panic!("unexpected admission failure: {error}"),
                }
            }
            assert_eq!(admitted.len(), 2);
            for scope in [&provider, &model] {
                let status = f.quota(scope, TUESDAY).await;
                assert_eq!(status.reserved_tokens, 120);
                assert_eq!(status.available_tokens, Some(0));
            }
            for id in admitted {
                f.store.mark_dispatched(id).await.unwrap();
                f.store
                    .settle(id, AttemptUsage::Observed(55))
                    .await
                    .unwrap();
            }
            for scope in [&provider, &model] {
                let status = f.quota(scope, TUESDAY).await;
                assert_eq!(status.known_tokens, 110);
                assert_eq!(status.reserved_tokens, 0);
            }
        }

        #[tokio::test]
        async fn independent_limits_do_not_leave_partial_reservations_on_denial() {
            let f = Fixture::new().await;
            let (provider, model) = (scope(None), scope(Some("a")));
            f.limit(&provider, 20, QuotaWindow::Weekly).await;
            f.limit(&model, 10, QuotaWindow::Daily).await;
            let a = f.dispatch(f.request("a", 11, TUESDAY)).await;
            assert!(matches!(f.store.reserve(f.request("a", 1, TUESDAY)).await,
                Err(QuotaError::Exhausted(limits)) if limits.len() == 1 && limits[0].scope == model));
            assert_eq!(f.quota(&provider, TUESDAY).await.reserved_tokens, 11);
            f.store.settle(a, AttemptUsage::Observed(11)).await.unwrap();
            let b = f.dispatch(f.request("b", 11, TUESDAY)).await;
            assert!(
                matches!(f.store.reserve(f.request("a", 1, WEDNESDAY)).await,
                Err(QuotaError::Exhausted(limits)) if limits.len() == 1 && limits[0].scope == provider)
            );
            // The denial still commits the daily rollover; a rollback of the clock cannot reopen it.
            let daily = f.quota(&model, TUESDAY).await;
            assert_eq!(daily.window_started_at, Some(WEDNESDAY));
            assert_eq!(daily.reserved_tokens, 0);
            f.store.settle(b, AttemptUsage::Unknown).await.unwrap();
            assert!(f.quota(&scope(Some("b")), TUESDAY).await.has_unknown_usage);
        }

        #[tokio::test]
        async fn utc_windows_do_not_reopen_and_late_settlement_charges_original_periods() {
            let f = Fixture::new().await;
            let (provider, daily, monthly) =
                (scope(None), scope(Some("a")), scope(Some("monthly")));
            f.limit(&provider, 30, QuotaWindow::Weekly).await;
            f.limit(&daily, 10, QuotaWindow::Daily).await;
            f.limit(&monthly, 20, QuotaWindow::Monthly).await;
            assert_eq!(
                f.quota(&provider, TUESDAY).await.next_reset_at,
                Some(MONDAY)
            );
            assert_eq!(
                f.quota(&daily, TUESDAY).await.next_reset_at,
                Some(WEDNESDAY)
            );
            assert_eq!(f.quota(&monthly, TUESDAY).await.next_reset_at, Some(FRIDAY));
            let id = f.dispatch(f.request("a", 10, TUESDAY)).await;
            assert_eq!(
                f.quota(&daily, WEDNESDAY - 1).await.available_tokens,
                Some(0)
            );
            assert_eq!(f.quota(&daily, WEDNESDAY).await.available_tokens, Some(10));
            f.store.settle(id, AttemptUsage::Observed(8)).await.unwrap();
            let f = f.restart();
            assert_eq!(
                f.quota(&daily, TUESDAY).await.window_started_at,
                Some(WEDNESDAY)
            );
            assert_eq!(f.history(&daily).await, [0, 8]);
            assert_eq!(f.quota(&provider, TUESDAY).await.known_tokens, 8);
            assert_eq!(
                f.quota(&monthly, MONDAY).await.window_started_at,
                Some(FRIDAY)
            );
            assert_eq!(f.quota(&provider, MONDAY).await.available_tokens, Some(30));
        }

        #[tokio::test]
        async fn replay_expires_only_after_both_scopes_age_out_and_old_open_periods_survive() {
            let f = Fixture::new().await;
            let (provider, model) = (scope(None), scope(Some("a")));
            f.limit(&provider, 30, QuotaWindow::Weekly).await;
            f.limit(&model, 30, QuotaWindow::Daily).await;
            let mut req = f.request("a", 10, TUESDAY);
            let settled = f.dispatch(req.clone()).await;
            f.store
                .settle(settled, AttemptUsage::Observed(8))
                .await
                .unwrap();
            let old_open = f.dispatch(f.request("a", 3, TUESDAY)).await;
            f.quota(&model, WEDNESDAY).await;
            f.quota(&model, FRIDAY).await;
            assert_eq!(f.history(&model).await, [0, 0, 8]);
            req.now = FRIDAY;
            assert!(matches!(
                f.store.reserve(req.clone()).await,
                Err(QuotaError::DuplicateAttempt)
            ));
            f.store
                .settle(old_open, AttemptUsage::Observed(3))
                .await
                .unwrap();
            assert_eq!(f.history(&model).await, [0, 0, 11]);
            f.quota(&model, MONDAY).await;
            assert_eq!(f.history(&model).await, [0, 0]);
            req.now = MONDAY;
            assert!(matches!(
                f.store.reserve(req.clone()).await,
                Err(QuotaError::DuplicateAttempt)
            ));
            let next_monday = MONDAY + 7 * 86400;
            // Admission, not a snapshot, must expire replay history for every binding.
            req.model_id = "b".into();
            req.now = next_monday;
            f.store
                .reserve(req)
                .await
                .expect("both original periods expired");
        }

        #[tokio::test]
        async fn settlement_validates_transitions_and_notifies_once_without_advancing_revision() {
            let f = Fixture::new().await;
            let provider = scope(None);
            let rev = f.limit(&provider, 20, QuotaWindow::Weekly).await;
            let mut hints = f.store.subscribe_usage_changes().unwrap();
            let req = f.request("a", 10, TUESDAY);
            let id = f.store.reserve(req.clone()).await.unwrap();
            assert_eq!(hints.try_recv().unwrap(), f.key);
            assert!(matches!(
                f.store.reserve(req).await,
                Err(QuotaError::DuplicateAttempt)
            ));
            assert!(matches!(
                f.store.settle(id, AttemptUsage::Observed(7)).await,
                Err(QuotaError::InvalidAttempt)
            ));
            f.store.mark_dispatched(id).await.unwrap();
            assert!(matches!(
                f.store.release_unused(id).await,
                Err(QuotaError::InvalidAttempt)
            ));
            assert!(matches!(
                f.store.settle(id, AttemptUsage::Observed(-1)).await,
                Err(QuotaError::InvalidTokens)
            ));
            assert_eq!(f.quota(&provider, TUESDAY).await.reserved_tokens, 10);
            assert!(matches!(hints.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(
                f.store.settle(id, AttemptUsage::Observed(7)).await.unwrap(),
                QuotaSettlement::Recorded
            );
            assert_eq!(hints.try_recv().unwrap(), f.key);
            assert_eq!(
                f.store.settle(id, AttemptUsage::Unknown).await.unwrap(),
                QuotaSettlement::AlreadyFinal
            );
            assert!(matches!(hints.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(f.revision().await, rev);
            assert_eq!(f.quota(&provider, TUESDAY).await.known_tokens, 7);
        }

        #[tokio::test]
        async fn dropped_leases_release_undispatched_work_and_charge_unknown_dispatched_work() {
            let f = Fixture::new().await;
            let provider = scope(None);
            f.limit(&provider, 20, QuotaWindow::Weekly).await;
            let reserved = ReservedAttempt::reserve(f.store.clone(), f.request("a", 5, TUESDAY))
                .await
                .unwrap();
            let dispatched = ReservedAttempt::reserve(f.store.clone(), f.request("b", 10, TUESDAY))
                .await
                .unwrap()
                .begin()
                .await
                .unwrap();
            assert_eq!(f.quota(&provider, TUESDAY).await.reserved_tokens, 15);
            let mut hints = f.store.subscribe_usage_changes().unwrap();
            std::thread::spawn(move || drop((reserved, dispatched)))
                .join()
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                assert_eq!(hints.recv().await.unwrap(), f.key);
                assert_eq!(hints.recv().await.unwrap(), f.key);
            })
            .await
            .unwrap();
            let status = f.quota(&provider, TUESDAY).await;
            assert_eq!(status.reserved_tokens, 0);
            assert_eq!(status.known_tokens, 0);
            assert_eq!(status.estimated_tokens, 10);
            assert_eq!(status.available_tokens, Some(10));
        }

        #[tokio::test]
        async fn cleanup_retries_the_exact_settlement_after_a_failed_save() {
            let f = Fixture::new().await;
            let provider = scope(None);
            f.limit(&provider, 20, QuotaWindow::Weekly).await;
            let mut reserved =
                ReservedAttempt::reserve(f.store.clone(), f.request("a", 10, TUESDAY))
                    .await
                    .unwrap();
            let done = reserved.completion();
            let dispatched = reserved.begin().await.unwrap();
            let mut hints = f.store.subscribe_usage_changes().unwrap();
            std::fs::remove_file(&f.path).unwrap();
            std::fs::create_dir(&f.path).unwrap();
            assert!(matches!(
                dispatched.settle(AttemptUsage::Observed(7)).await,
                Err(QuotaError::Store(GrantStoreError::Backend(_)))
            ));
            assert!(matches!(hints.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(f.quota(&provider, TUESDAY).await.reserved_tokens, 10);
            std::fs::remove_dir(&f.path).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), done)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(hints.try_recv().unwrap(), f.key);
            assert!(matches!(hints.try_recv(), Err(TryRecvError::Empty)));
            let status = f.quota(&provider, TUESDAY).await;
            assert_eq!(
                (
                    status.known_tokens,
                    status.estimated_tokens,
                    status.reserved_tokens
                ),
                (7, 0, 0)
            );
        }

        #[tokio::test]
        async fn startup_prunes_history_retained_for_pre_restart_reservations() {
            let f = Fixture::new().await;
            let (provider, model) = (scope(None), scope(Some("a")));
            f.limit(&provider, 50, QuotaWindow::Manual).await;
            f.limit(&model, 50, QuotaWindow::Daily).await;
            let charged = f.dispatch(f.request("a", 5, TUESDAY)).await;
            f.store
                .settle(charged, AttemptUsage::Observed(5))
                .await
                .unwrap();
            f.dispatch(f.request("a", 3, TUESDAY)).await;
            f.quota(&model, FRIDAY).await;
            assert_eq!(f.history(&model).await, [0, 0, 5]);
            let f = f.restart();
            f.store.reconcile_startup(&f.key.host).await.unwrap();
            let f = f.restart();
            assert_eq!(f.history(&model).await, [0, 0]);
            assert_eq!(f.history(&provider).await, [5]);
        }

        #[tokio::test]
        async fn deferred_quota_admission_uses_binding_history_not_the_edit_revision() {
            let f = Fixture::new().await;
            let provider = scope(None);
            let req = f.request("a", 10, TUESDAY);
            let uninterrupted = f.store.subscribe_changes().unwrap();
            f.limit(&provider, 20, QuotaWindow::Manual).await;
            // Both grants allow this binding; changing mode is not a revocation.
            f.set_grant(PeerGrant::Full).await;
            let id = f
                .store
                .reserve_with_policy(req.clone(), uninterrupted, false)
                .await
                .unwrap();
            f.store.mark_dispatched(id).await.unwrap();
            f.store
                .settle(id, AttemptUsage::Observed(10))
                .await
                .unwrap();
            assert_eq!(f.quota(&provider, TUESDAY).await.available_tokens, Some(20));

            let deferred = f.store.subscribe_changes().unwrap();
            f.set_grant(PeerGrant::None).await;
            f.set_grant(f.custom.clone()).await;
            assert!(matches!(
                f.store
                    .reserve_with_policy(f.request("a", 10, TUESDAY), deferred, false)
                    .await,
                Err(QuotaError::Denied)
            ));
            assert_eq!(f.quota(&provider, TUESDAY).await.available_tokens, Some(20));

            let pending = f.store.reserve(f.request("a", 10, TUESDAY)).await.unwrap();
            f.set_grant(PeerGrant::None).await;
            f.set_grant(f.custom.clone()).await;
            for _ in 0..2 {
                assert!(matches!(
                    f.store.mark_dispatched(pending).await,
                    Err(QuotaError::InvalidAttempt)
                ));
            }
            f.store.release_unused(pending).await.unwrap();
        }

        #[tokio::test]
        async fn unrelated_policy_burst_preserves_peer_watch_and_pending_quota() {
            use crate::peer_access::wait_for_policy_loss;
            use tokio::sync::broadcast::error::TryRecvError;

            let f = Fixture::new().await;
            let other = PeerGrantKey {
                host: f.key.host.clone(),
                peer: NodeId::parse("12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv")
                    .unwrap(),
            };
            let mut watch = f.store.subscribe_peer_changes(&f.key).unwrap();
            let pending = f.store.reserve(f.request("a", 10, TUESDAY)).await.unwrap();
            let mut revision = Uuid::nil();
            for i in 0..65 {
                let grant = if i % 2 == 0 {
                    PeerGrant::Full
                } else {
                    PeerGrant::None
                };
                revision = f
                    .store
                    .replace_grant(&other, revision, grant)
                    .await
                    .unwrap()
                    .revision;
            }
            assert!(matches!(watch.try_recv(), Err(TryRecvError::Empty)));
            f.store.mark_dispatched(pending).await.unwrap();
            f.store
                .settle(pending, AttemptUsage::Observed(10))
                .await
                .unwrap();

            let pending = f.store.reserve(f.request("a", 10, TUESDAY)).await.unwrap();
            f.set_grant(PeerGrant::None).await;
            f.set_grant(f.custom.clone()).await;
            assert_eq!(
                wait_for_policy_loss(
                    f.store.as_ref(),
                    &f.key,
                    Some(watch),
                    |grant| if *grant == PeerGrant::None {
                        Err("revoked")
                    } else {
                        Ok(())
                    },
                    || "history unavailable",
                )
                .await,
                "revoked"
            );
            assert!(matches!(
                f.store.mark_dispatched(pending).await,
                Err(QuotaError::InvalidAttempt)
            ));
        }

        #[tokio::test]
        async fn release_hints_only_for_committed_active_finite_reservations() {
            let f = Fixture::new().await;
            let provider = scope(None);
            let rev = f.limit(&provider, 20, QuotaWindow::Manual).await;
            let mut hints = f.store.subscribe_usage_changes().unwrap();
            let id = f.store.reserve(f.request("a", 10, TUESDAY)).await.unwrap();
            assert_eq!(hints.try_recv().unwrap(), f.key);
            assert_eq!(f.quota(&provider, TUESDAY).await.available_tokens, Some(10));
            std::fs::remove_file(&f.path).unwrap();
            std::fs::create_dir(&f.path).unwrap();
            assert!(matches!(
                f.store.release_unused(id).await,
                Err(QuotaError::Store(_))
            ));
            assert!(matches!(hints.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(f.quota(&provider, TUESDAY).await.available_tokens, Some(10));
            std::fs::remove_dir(&f.path).unwrap();
            f.store.release_unused(id).await.unwrap();
            assert_eq!(hints.try_recv().unwrap(), f.key);
            assert_eq!(f.quota(&provider, TUESDAY).await.available_tokens, Some(20));
            f.store.release_unused(id).await.unwrap();
            assert!(matches!(hints.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(f.revision().await, rev);
        }

        #[tokio::test]
        async fn restart_keeps_settled_usage_and_replay_but_forgets_open_attempts() {
            for (charge, expected) in [
                (UnknownUsageCharge::Reserved, 18),
                (UnknownUsageCharge::EstimatedPrompt, 9),
            ] {
                let mut f = Fixture::new().await;
                f.store = Arc::new(
                    Arc::try_unwrap(f.store)
                        .ok()
                        .unwrap()
                        .with_unknown_usage_charge(charge),
                );
                let (provider, model) = (scope(None), scope(Some("a")));
                f.limit(&provider, 50, QuotaWindow::Weekly).await;
                f.limit(&model, 50, QuotaWindow::Daily).await;
                let settled = f.request("a", 18, TUESDAY);
                let id = f.dispatch(settled.clone()).await;
                f.store.settle(id, AttemptUsage::Unknown).await.unwrap();
                let released = f.request("a", 1, TUESDAY);
                let released_id = f.store.reserve(released.clone()).await.unwrap();
                f.store.release_unused(released_id).await.unwrap();
                let pending = f.request("a", 5, TUESDAY);
                let pending_id = f.store.reserve(pending.clone()).await.unwrap();
                let dispatched = f.request("a", 10, TUESDAY);
                let dispatched_id = f.dispatch(dispatched.clone()).await;
                let revision = f.revision().await;
                let saved: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&f.path).unwrap()).unwrap();
                assert_eq!(
                    saved["peer_access"]["quotas"]["scopes"][0]["periods"][0]["limit"]["limit_tokens"],
                    "50"
                );
                let f = f.restart();
                assert_eq!(f.revision().await, revision);
                assert!(matches!(
                    f.store.mark_dispatched(pending_id).await,
                    Err(QuotaError::InvalidAttempt)
                ));
                assert!(matches!(
                    f.store.settle(dispatched_id, AttemptUsage::Unknown).await,
                    Err(QuotaError::InvalidAttempt)
                ));
                f.set_grant(PeerGrant::None).await;
                assert!(matches!(
                    f.store.reserve(pending.clone()).await,
                    Err(QuotaError::Denied)
                ));
                f.set_grant(f.custom.clone()).await;
                for scope in [&provider, &model] {
                    let status = f.quota(scope, TUESDAY).await;
                    assert_eq!(status.reserved_tokens, 0);
                    assert_eq!(status.estimated_tokens, expected);
                    assert_eq!(status.available_tokens, Some(50 - expected));
                    assert!(!status.has_unknown_usage);
                }
                for req in [settled, released] {
                    assert!(matches!(
                        f.store.reserve(req).await,
                        Err(QuotaError::DuplicateAttempt)
                    ));
                }
                for req in [pending, dispatched] {
                    f.store.reserve(req).await.unwrap();
                }
            }
        }

        #[tokio::test]
        async fn grant_edit_does_not_commit_or_publish_when_snapshot_rollover_fails() {
            let f = Fixture::new().await;
            let revision = f.limit(&scope(None), 50, QuotaWindow::Daily).await;
            let mut changes = f.store.subscribe_changes().unwrap();
            let last_day = Date::MAX.midnight().assume_utc().unix_timestamp();
            assert!(matches!(
                f.store
                    .edit_access(
                        &f.key,
                        revision,
                        AccessEdit::Grant(PeerGrant::Full),
                        last_day
                    )
                    .await,
                Err(QuotaError::InvalidTime)
            ));
            let grant = f.store.grant(&f.key).await.unwrap();
            assert_eq!(grant.revision, revision);
            assert_eq!(grant.grant, f.custom);
            assert!(matches!(changes.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(f.restart().revision().await, revision);
        }

        #[tokio::test]
        async fn edits_share_revision_and_reset_only_the_target_without_moving_renewal() {
            let f = Fixture::new().await;
            let (provider, model) = (scope(None), scope(Some("a")));
            let stale = f.limit(&provider, 50, QuotaWindow::Weekly).await;
            let revision = f.limit(&model, 25, QuotaWindow::Daily).await;
            assert!(
                matches!(f.store.edit_quota(&f.key, stale, &model, QuotaEdit::Reset, TUESDAY).await,
                Err(QuotaError::Store(GrantStoreError::Conflict { actual, .. })) if actual == revision)
            );
            let req = f.request("a", 15, TUESDAY);
            let id = f.dispatch(req.clone()).await;
            let waiting = f.store.reserve(f.request("a", 5, TUESDAY)).await.unwrap();
            assert!(matches!(
                f.edit(&model, QuotaEdit::Reset, TUESDAY).await,
                Err(QuotaError::InFlight)
            ));
            let mut effects = f.store.subscribe_changes().unwrap();
            let lower = QuotaLimit::Finite {
                tokens: 10,
                window: QuotaWindow::Daily,
            };
            let rev = f
                .edit(&model, QuotaEdit::Update(lower), TUESDAY)
                .await
                .unwrap();
            assert_ne!(rev, revision);
            assert_eq!(
                f.edit(&model, QuotaEdit::Update(lower), TUESDAY)
                    .await
                    .unwrap(),
                rev
            );
            assert!(matches!(effects.try_recv(), Err(TryRecvError::Empty)));
            assert!(matches!(
                f.store.reserve(f.request("a", 1, TUESDAY)).await,
                Err(QuotaError::Exhausted(_))
            ));
            f.store.mark_dispatched(waiting).await.unwrap();
            f.store
                .settle(id, AttemptUsage::Observed(12))
                .await
                .unwrap();
            f.store
                .settle(waiting, AttemptUsage::Observed(1))
                .await
                .unwrap();
            f.edit(&model, QuotaEdit::Reset, TUESDAY + 30)
                .await
                .unwrap();
            let reset = f.quota(&model, TUESDAY).await;
            assert_eq!(reset.window_started_at, Some(TUESDAY + 30));
            assert_eq!(reset.next_reset_at, Some(WEDNESDAY));
            assert_eq!(reset.available_tokens, Some(10));
            assert_eq!(f.quota(&provider, TUESDAY).await.known_tokens, 13);
            let f = f.restart();
            assert!(matches!(
                f.store.reserve(req).await,
                Err(QuotaError::DuplicateAttempt)
            ));
        }

        #[tokio::test]
        async fn actual_usage_above_the_estimate_does_not_suspend_the_binding() {
            let f = Fixture::new().await;
            let (provider, model) = (scope(None), scope(Some("a")));
            f.limit(&provider, 100, QuotaWindow::Weekly).await;
            f.limit(&model, 20, QuotaWindow::Daily).await;
            let first = f.dispatch(f.request("a", 5, TUESDAY)).await;
            let waiting = f.store.reserve(f.request("a", 5, TUESDAY)).await.unwrap();
            f.store
                .settle(first, AttemptUsage::Observed(8))
                .await
                .unwrap();
            assert_eq!(f.quota(&model, TUESDAY).await.known_tokens, 8);
            f.store.mark_dispatched(waiting).await.unwrap();
            let crossing = f.dispatch(f.request("a", 8, TUESDAY)).await;
            f.store
                .settle(waiting, AttemptUsage::Observed(4))
                .await
                .unwrap();
            f.store
                .settle(crossing, AttemptUsage::Observed(11))
                .await
                .unwrap();
            assert_eq!(f.quota(&provider, TUESDAY).await.known_tokens, 23);
            assert_eq!(f.quota(&model, TUESDAY).await.known_tokens, 23);
            assert!(matches!(f.store.reserve(f.request("a", 1, TUESDAY)).await,
                Err(QuotaError::Exhausted(limits)) if limits.len() == 1 && limits[0].scope == model));
            f.store.reserve(f.request("a", 1, WEDNESDAY)).await.unwrap();
        }

        #[tokio::test]
        async fn overshoot_saturates_totals_without_reopening_admission() {
            let f = Fixture::new().await;
            let provider = scope(None);
            f.limit(&provider, i64::MAX, QuotaWindow::Manual).await;
            let first = f.dispatch(f.request("a", i64::MAX - 1, TUESDAY)).await;
            let last = f.dispatch(f.request("a", i64::MAX, TUESDAY)).await;
            assert_eq!(f.quota(&provider, TUESDAY).await.reserved_tokens, i64::MAX);
            for id in [first, last] {
                f.store
                    .settle(id, AttemptUsage::Observed(i64::MAX))
                    .await
                    .unwrap();
            }
            let status = f.quota(&provider, TUESDAY).await;
            assert_eq!(status.known_tokens, i64::MAX);
            assert_eq!(status.available_tokens, Some(0));
            assert!(matches!(
                f.store.reserve(f.request("a", 1, TUESDAY)).await,
                Err(QuotaError::Exhausted(_))
            ));
        }

        #[tokio::test]
        async fn dispatch_rechecks_permission_and_full_access_does_not_spend_custom_caps() {
            let f = Fixture::new().await;
            let provider = scope(None);
            f.limit(&provider, 20, QuotaWindow::Manual).await;
            let pending = f.store.reserve(f.request("a", 20, TUESDAY)).await.unwrap();
            f.set_grant(PeerGrant::None).await;
            assert!(matches!(
                f.store.mark_dispatched(pending).await,
                Err(QuotaError::InvalidAttempt)
            ));
            f.store.release_unused(pending).await.unwrap();
            let denied = f.request("a", 20, TUESDAY);
            assert!(matches!(
                f.store.reserve(denied.clone()).await,
                Err(QuotaError::Denied)
            ));
            f.set_grant(f.custom.clone()).await;
            let charged = f.dispatch(denied).await;
            f.store
                .settle(charged, AttemptUsage::Observed(20))
                .await
                .unwrap();
            let exhausted = f.quota(&provider, TUESDAY).await;
            f.set_grant(PeerGrant::Full).await;
            let mut legacy = f.request("a", 0, TUESDAY);
            legacy.protocol = SharingProtocol::Legacy;
            let full = f.dispatch(legacy.clone()).await;
            f.store
                .settle(full, AttemptUsage::Observed(500))
                .await
                .unwrap();
            assert!(matches!(
                f.store.reserve(legacy).await,
                Err(QuotaError::DuplicateAttempt)
            ));
            assert_eq!(f.quota(&provider, TUESDAY).await, exhausted);
            let waiting = f.store.reserve(f.request("a", 0, TUESDAY)).await.unwrap();
            f.set_grant(f.custom.clone()).await;
            assert!(matches!(
                f.store.mark_dispatched(waiting).await,
                Err(QuotaError::InvalidAttempt)
            ));
            assert!(matches!(
                f.store.reserve(f.request("a", 1, TUESDAY)).await,
                Err(QuotaError::Exhausted(_))
            ));
        }

        #[tokio::test]
        async fn unlimited_unknown_usage_does_not_carry_into_a_reconfigured_period() {
            let f = Fixture::new().await;
            let provider = scope(None);
            let id = f.dispatch(f.request("a", 0, TUESDAY)).await;
            f.store.settle(id, AttemptUsage::Unknown).await.unwrap();
            let old = f.quota(&provider, TUESDAY).await;
            assert_eq!(old.limit, QuotaLimit::Unlimited);
            assert!(old.has_unknown_usage);
            assert_eq!(old.available_tokens, None);
            assert!(matches!(
                f.edit(&provider, QuotaEdit::Reset, TUESDAY).await,
                Err(QuotaError::NotFinite)
            ));
            let finite = QuotaLimit::Finite {
                tokens: 25,
                window: QuotaWindow::Manual,
            };
            assert!(matches!(
                f.edit(&provider, QuotaEdit::Update(finite), TUESDAY).await,
                Err(QuotaError::ReconfigureRequired)
            ));
            f.edit(&provider, QuotaEdit::Reconfigure(finite), TUESDAY)
                .await
                .unwrap();
            let fresh = f.quota(&provider, TUESDAY).await;
            assert!(!fresh.has_unknown_usage);
            assert_eq!(fresh.available_tokens, Some(25));
            let mut invalid = f.request("a", 0, TUESDAY);
            assert!(matches!(
                f.store.reserve(invalid.clone()).await,
                Err(QuotaError::InvalidTokens)
            ));
            invalid.estimate.reserved_tokens = 1;
            f.store.reserve(invalid).await.unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::error::{LLMError, ProviderErrorKind};

    #[test]
    fn stream_meter_distinguishes_start_metadata_from_authoritative_zero_output() {
        use querymt::chat::{ChatOutputStatus, StreamChunk, StructuredStreamEvent};

        let start = querymt::Usage {
            input_tokens: 12,
            ..Default::default()
        };
        for (final_usage, expected) in [
            (None, AttemptUsage::Unknown),
            (Some(start.clone()), AttemptUsage::Observed(12)),
        ] {
            let mut meter = StreamMeter::default();
            meter.observe(&StreamChunk::Structured(
                StructuredStreamEvent::ResponseMetadata {
                    response_id: None,
                    status: Some(ChatOutputStatus::InProgress),
                    usage: Some(start.clone()),
                    finish_reason: None,
                    provenance: None,
                },
            ));
            meter.observe(&StreamChunk::Structured(
                StructuredStreamEvent::ResponseTerminal {
                    status: ChatOutputStatus::Completed,
                    usage: final_usage,
                    finish_reason: None,
                    detail: None,
                },
            ));
            assert_eq!(meter.complete_usage(), expected);
        }
    }

    #[test]
    fn quota_denial_details_survive_the_provider_wire_and_stay_bound_to_the_error_code() {
        let limits = vec![LimitingQuota {
            scope: QuotaScope {
                provider_id: "fixture".into(),
                model_id: Some("small".into()),
            },
            available_tokens: 0,
            next_reset_at: Some(1_800_000_000),
        }];
        let code = "mesh_quota_exhausted";
        let payload = QuotaError::Exhausted(limits.clone())
            .into_provider_error()
            .to_payload();
        let LLMError::ProviderResponseError(failure) = LLMError::from_payload(payload) else {
            panic!("quota denial lost its provider failure type");
        };
        assert_eq!(failure.kind(), ProviderErrorKind::QuotaExceeded);
        assert_eq!(failure.code(), Some(code));
        let QuotaDenialDetails::Limits { limits: decoded } =
            QuotaDenialDetails::from_provider_metadata(code, failure.error_type()).unwrap();
        assert_eq!(decoded, limits);
        for other_code in ["mesh_access_denied", "mesh_quota_unsupported"] {
            assert!(
                QuotaDenialDetails::from_provider_metadata(other_code, failure.error_type())
                    .is_none()
            );
        }
        for metadata in [
            "rate_limited",
            "querymt.mesh.quota.v1:bad-json",
            r#"querymt.mesh.quota.v1:{"scope":{"provider_id":"fixture"}}"#,
        ] {
            assert!(QuotaDenialDetails::from_provider_metadata(code, Some(metadata)).is_none());
        }
    }

    #[test]
    fn quota_store_failures_hide_backend_diagnostics_without_hiding_replays() {
        let backend = QuotaError::Store(GrantStoreError::Backend(Box::new(std::io::Error::other(
            "fixture database path and internal SQL",
        ))));
        for (error, kind, code) in [
            (
                backend,
                ProviderErrorKind::UnknownPermanent,
                "mesh_access_unavailable",
            ),
            (
                QuotaError::DuplicateAttempt,
                ProviderErrorKind::InvalidRequest,
                "mesh_quota_duplicate_attempt",
            ),
            (
                QuotaError::InvalidAttempt,
                ProviderErrorKind::Authentication,
                "mesh_access_denied",
            ),
        ] {
            let payload = error.into_provider_error().to_payload();
            let LLMError::ProviderResponseError(error) = LLMError::from_payload(payload) else {
                panic!("quota error lost its wire classification");
            };
            assert_eq!(error.kind(), kind);
            assert_eq!(error.code(), Some(code));
            assert!(error.error_type().is_none());
            if code == "mesh_access_unavailable" {
                assert_eq!(error.message(), "mesh policy unavailable");
            }
        }
    }
}
