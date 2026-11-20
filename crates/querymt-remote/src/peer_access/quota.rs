//! Token accounting types shared by mesh hosts and durable stores.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::{GrantStoreError, PeerGrantKey, SharingProtocol, valid_id};

mod store;
pub use store::{DispatchedAttempt, QuotaStore, ReservedAttempt};

/// A provider-wide or model-specific quota on a particular host and peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaScope {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

impl QuotaScope {
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindow {
    #[default]
    Manual,
    Daily,
    Weekly,
    Monthly,
}

/// Exact token allowance. Finite token counts fit a signed SQLite integer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QuotaLimit {
    #[default]
    Unlimited,
    Finite {
        tokens: i64,
        window: QuotaWindow,
    },
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
/// transport and use host-resolved IDs and a host-observed upper bound.
#[derive(Debug, Clone)]
pub struct QuotaAttempt {
    pub peer: PeerGrantKey,
    /// Revision captured when the host authorized the initiating peer.
    pub grant_revision: Uuid,
    pub protocol: SharingProtocol,
    pub provider_id: String,
    pub model_id: String,
    /// Stable for retransmissions; a new upstream attempt needs a new ID.
    pub request_id: Uuid,
    /// `None` means the prepared request cannot be conservatively bounded.
    pub bound_tokens: Option<i64>,
    /// Host wall clock at admission, in Unix seconds UTC.
    pub now: i64,
}

/// Handle returned by the store for a newly reserved upstream attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationId(pub Uuid);

/// One limiting scope reported by preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Convert a confirmed normalized provider usage total, excluding cache tiers.
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

/// Result of committing an attempt's settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSettlement {
    Recorded,
    AlreadyFinal,
    /// Actual usage was recorded and the binding was suspended for finite requests.
    BoundExceeded,
}

/// A rejected quota mutation or inference admission.
#[derive(Debug, Error)]
pub enum QuotaError {
    #[error(transparent)]
    Store(#[from] GrantStoreError),
    #[error("invalid quota scope")]
    InvalidScope,
    #[error("invalid token count")]
    InvalidTokens,
    #[error("invalid quota clock value")]
    InvalidTime,
    #[error("access denied")]
    Denied,
    #[error("quota exhausted")]
    Exhausted(Vec<LimitingQuota>),
    #[error("finite quota cannot be enforced for this request")]
    Unsupported(QuotaScope),
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
