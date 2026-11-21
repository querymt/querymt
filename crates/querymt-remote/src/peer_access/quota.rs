//! Token accounting types shared by mesh hosts and durable stores.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::{GrantStoreError, PeerGrantKey, SharingProtocol, valid_id};

mod store;
pub use store::{DispatchedAttempt, QuotaStore, ReservedAttempt};

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
        let usage = match chunk {
            StreamChunk::Usage(usage) => Some(usage),
            StreamChunk::Structured(
                StructuredStreamEvent::ResponseMetadata { usage, .. }
                | StructuredStreamEvent::ResponseTerminal { usage, .. },
            ) => usage.as_ref(),
            _ => None,
        };
        if let Some(usage) = usage {
            self.usage = Some(
                self.usage
                    .take()
                    .map_or_else(|| usage.clone(), |current| current.merge_max(usage.clone())),
            );
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

/// Details retained across the provider wire and promoted to the ACP error.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum QuotaDenialDetails {
    Limits { limits: Vec<LimitingQuota> },
    Scope { scope: QuotaScope },
}

const DENIAL_DETAILS_PREFIX: &str = "querymt.mesh.quota.v1:";

impl QuotaDenialDetails {
    /// Decode only the metadata matching this mesh policy error's code.
    pub fn from_provider_metadata(code: &str, error_type: Option<&str>) -> Option<Self> {
        let payload = error_type?.strip_prefix(DENIAL_DETAILS_PREFIX)?;
        let details = serde_json::from_str(payload).ok()?;
        match (code, &details) {
            ("mesh_quota_exhausted", Self::Limits { .. })
            | ("mesh_quota_unsupported", Self::Scope { .. }) => Some(details),
            _ => None,
        }
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
            Self::Unsupported(_) => (
                ProviderErrorKind::UnsupportedOperation,
                "mesh_quota_unsupported",
            ),
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
            Self::Unsupported(scope) => Some(QuotaDenialDetails::Scope { scope }),
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
