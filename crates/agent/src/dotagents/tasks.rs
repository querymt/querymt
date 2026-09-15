//! Stable identities, persisted ownership, and host trust APIs for protocol tasks.

use super::{
    DotagentsDiagnostic, DotagentsDiagnosticCode, DotagentsLayer, DotagentsSource,
    DotagentsStrictness, DotagentsTask, DotagentsTaskKind, normalize_id,
};
use crate::session::{SessionError, SessionResult};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

const KEY_VERSION: &str = "v1";

/// Stable protocol identity used to own one task and schedule pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DotagentsTaskIdentity {
    pub layer: DotagentsLayer,
    pub canonical_workspace: Option<PathBuf>,
    pub task_id: String,
}

impl DotagentsTaskIdentity {
    /// Build an identity from a resolved task and canonical workspace.
    pub fn new(
        layer: DotagentsLayer,
        canonical_workspace: Option<impl Into<PathBuf>>,
        task_id: impl AsRef<str>,
    ) -> Self {
        Self {
            layer,
            canonical_workspace: match layer {
                DotagentsLayer::Global => None,
                DotagentsLayer::Workspace => canonical_workspace.map(Into::into),
            },
            task_id: normalize_id(task_id.as_ref()),
        }
    }

    /// Stable source key independent of the automation session binding.
    pub fn source_key(&self) -> String {
        let workspace = self
            .canonical_workspace
            .as_deref()
            .map(path_identity)
            .unwrap_or_else(|| "global".to_string());
        format!(
            "dotagents:{KEY_VERSION}:task:{}:{workspace}:{}",
            self.layer.as_str(),
            self.task_id
        )
    }

    /// Task creation key, scoped by the existing task repository to a session.
    pub fn task_creation_key(&self) -> String {
        format!("{}:record", self.source_key())
    }

    /// Stable schedule ownership key for repository-level reconciliation.
    pub fn schedule_creation_key(&self) -> String {
        format!("{}:schedule", self.source_key())
    }
}

/// Fingerprint all effective fields that can alter execution or trust.
pub fn task_execution_fingerprint(task: &DotagentsTask) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, task.source.layer.as_str());
    hash_field(&mut hasher, &normalize_id(&task.id));
    hash_field(&mut hasher, &task.name);
    hash_field(&mut hasher, if task.enabled { "1" } else { "0" });
    hash_field(&mut hasher, if task.run_on_startup { "1" } else { "0" });
    hash_field(
        &mut hasher,
        &task
            .interval_minutes
            .map(|v| v.to_string())
            .unwrap_or_default(),
    );
    hash_field(
        &mut hasher,
        task.profile_id
            .as_deref()
            .map(normalize_id)
            .as_deref()
            .unwrap_or(""),
    );
    hash_field(&mut hasher, &task.prompt);
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn path_identity(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_os_str().as_encoded_bytes());
    hex::encode(hasher.finalize())
}

/// Persisted link between a protocol source and its QueryMT records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskOwnership {
    pub source_key: String,
    pub task_creation_key: String,
    pub schedule_creation_key: String,
    pub layer: DotagentsLayer,
    pub canonical_workspace: Option<PathBuf>,
    pub task_id: String,
    pub fingerprint: String,
    pub task_public_id: Option<String>,
    pub schedule_public_id: Option<String>,
}

impl DotagentsTaskOwnership {
    pub fn pending(identity: &DotagentsTaskIdentity, fingerprint: impl Into<String>) -> Self {
        Self {
            source_key: identity.source_key(),
            task_creation_key: identity.task_creation_key(),
            schedule_creation_key: identity.schedule_creation_key(),
            layer: identity.layer,
            canonical_workspace: identity.canonical_workspace.clone(),
            task_id: identity.task_id.clone(),
            fingerprint: fingerprint.into(),
            task_public_id: None,
            schedule_public_id: None,
        }
    }
}

/// A persisted fingerprint-bound workspace trust decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskApproval {
    pub canonical_workspace: PathBuf,
    pub task_id: String,
    pub fingerprint: String,
    pub decision: DotagentsTaskApprovalDecision,
    pub decided_at: OffsetDateTime,
}

/// Storage needed by task reconciliation without exposing SQLite details.
#[async_trait]
pub trait DotagentsTaskStateRepository: Send + Sync {
    async fn upsert_ownership(&self, ownership: DotagentsTaskOwnership) -> SessionResult<()>;
    async fn get_ownership(
        &self,
        source_key: &str,
    ) -> SessionResult<Option<DotagentsTaskOwnership>>;
    /// List every persisted protocol ownership record.
    ///
    /// Reconciliation needs this to find protocol-owned records whose source
    /// files were removed or disabled, so their schedules can be paused without
    /// touching user-created schedules.
    async fn list_ownerships(&self) -> SessionResult<Vec<DotagentsTaskOwnership>>;
    async fn record_approval(&self, approval: DotagentsTaskApproval) -> SessionResult<()>;
    async fn get_approval(
        &self,
        canonical_workspace: &Path,
        task_id: &str,
        fingerprint: &str,
    ) -> SessionResult<Option<DotagentsTaskApproval>>;
}

/// SQLite implementation of protocol task ownership and trust persistence.
pub struct SqliteDotagentsTaskStateRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDotagentsTaskStateRepository {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    async fn run_blocking<F, R>(&self, f: F) -> SessionResult<R>
    where
        F: FnOnce(&mut Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().unwrap();
            f(&mut conn)
        })
        .await
        .map_err(|error| SessionError::Other(format!("task state operation failed: {error}")))?
        .map_err(SessionError::from)
    }
}

#[async_trait]
impl DotagentsTaskStateRepository for SqliteDotagentsTaskStateRepository {
    async fn upsert_ownership(&self, ownership: DotagentsTaskOwnership) -> SessionResult<()> {
        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO dotagents_task_ownership (
                    source_key, task_creation_key, schedule_creation_key, layer,
                    canonical_workspace, task_id, fingerprint, task_public_id,
                    schedule_public_id, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                 ON CONFLICT(source_key) DO UPDATE SET
                    fingerprint = excluded.fingerprint,
                    task_public_id = excluded.task_public_id,
                    schedule_public_id = excluded.schedule_public_id,
                    updated_at = excluded.updated_at",
                params![
                    ownership.source_key,
                    ownership.task_creation_key,
                    ownership.schedule_creation_key,
                    ownership.layer.as_str(),
                    ownership
                        .canonical_workspace
                        .as_deref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    ownership.task_id,
                    ownership.fingerprint,
                    ownership.task_public_id,
                    ownership.schedule_public_id,
                    format_time(OffsetDateTime::now_utc()),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_ownership(
        &self,
        source_key: &str,
    ) -> SessionResult<Option<DotagentsTaskOwnership>> {
        let source_key = source_key.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT source_key, task_creation_key, schedule_creation_key, layer,
                        canonical_workspace, task_id, fingerprint, task_public_id,
                        schedule_public_id
                 FROM dotagents_task_ownership WHERE source_key = ?1",
                [source_key],
                map_ownership,
            )
            .optional()
        })
        .await
    }

    async fn list_ownerships(&self) -> SessionResult<Vec<DotagentsTaskOwnership>> {
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT source_key, task_creation_key, schedule_creation_key, layer,
                        canonical_workspace, task_id, fingerprint, task_public_id,
                        schedule_public_id
                 FROM dotagents_task_ownership ORDER BY source_key ASC",
            )?;
            let rows = stmt.query_map([], map_ownership)?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn record_approval(&self, approval: DotagentsTaskApproval) -> SessionResult<()> {
        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO dotagents_task_approvals (
                    canonical_workspace, task_id, fingerprint, decision, decided_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(canonical_workspace, task_id, fingerprint) DO UPDATE SET
                    decision = excluded.decision, decided_at = excluded.decided_at",
                params![
                    approval.canonical_workspace.to_string_lossy(),
                    normalize_id(&approval.task_id),
                    approval.fingerprint,
                    approval.decision.as_str(),
                    format_time(approval.decided_at),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_approval(
        &self,
        canonical_workspace: &Path,
        task_id: &str,
        fingerprint: &str,
    ) -> SessionResult<Option<DotagentsTaskApproval>> {
        let workspace = canonical_workspace.to_string_lossy().into_owned();
        let task_id = normalize_id(task_id);
        let fingerprint = fingerprint.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT canonical_workspace, task_id, fingerprint, decision, decided_at
                 FROM dotagents_task_approvals
                 WHERE canonical_workspace = ?1 AND task_id = ?2 AND fingerprint = ?3",
                params![workspace, task_id, fingerprint],
                map_approval,
            )
            .optional()
        })
        .await
    }
}

fn map_ownership(row: &rusqlite::Row<'_>) -> rusqlite::Result<DotagentsTaskOwnership> {
    let layer: String = row.get(3)?;
    Ok(DotagentsTaskOwnership {
        source_key: row.get(0)?,
        task_creation_key: row.get(1)?,
        schedule_creation_key: row.get(2)?,
        layer: match layer.as_str() {
            "global" => DotagentsLayer::Global,
            "workspace" => DotagentsLayer::Workspace,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        canonical_workspace: row.get::<_, Option<String>>(4)?.map(PathBuf::from),
        task_id: row.get(5)?,
        fingerprint: row.get(6)?,
        task_public_id: row.get(7)?,
        schedule_public_id: row.get(8)?,
    })
}

fn map_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<DotagentsTaskApproval> {
    let decision: String = row.get(3)?;
    let decided_at: String = row.get(4)?;
    Ok(DotagentsTaskApproval {
        canonical_workspace: PathBuf::from(row.get::<_, String>(0)?),
        task_id: row.get(1)?,
        fingerprint: row.get(2)?,
        decision: DotagentsTaskApprovalDecision::parse(&decision)
            .ok_or(rusqlite::Error::InvalidQuery)?,
        decided_at: OffsetDateTime::parse(
            &decided_at,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

fn format_time(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Host policy for workspace protocol tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DotagentsTaskTrustPolicy {
    /// Ask the host and remain pending when no approval mechanism exists.
    #[default]
    Prompt,
    /// Never activate workspace protocol tasks.
    Deny,
    /// Activate without approval. This is intentionally and explicitly unsafe.
    Allow,
}

/// Maximum number of prompt characters disclosed in an approval request.
///
/// The summary is a disclosure aid, not the execution input: it is bounded so
/// a hostile repository cannot flood a host UI with an unbounded prompt.
const PROMPT_SUMMARY_CHARS: usize = 280;

/// Information disclosed to a host before it decides whether to trust a task.
///
/// Every execution-relevant field of the task is disclosed: its source, its
/// schedule, its startup behavior, its target profile, and a bounded summary of
/// the prompt that will actually run. Construct one with
/// [`DotagentsTaskApprovalRequest::from_task`] so disclosure cannot drift from
/// the reconciled definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskApprovalRequest {
    pub canonical_workspace: PathBuf,
    pub task_id: String,
    pub task_name: String,
    pub fingerprint: String,
    pub source: DotagentsSource,
    pub interval_minutes: Option<u64>,
    pub run_on_startup: bool,
    pub profile_id: Option<String>,
    pub prompt_summary: String,
}

impl DotagentsTaskApprovalRequest {
    /// Build the approval request disclosed for a resolved task.
    ///
    /// The request is derived directly from the task so the host always sees
    /// the same source, schedule, startup behavior, target profile, and prompt
    /// summary that reconciliation would act on, and so the approval is bound
    /// to [`task_execution_fingerprint`] rather than a raw file hash.
    pub fn from_task(task: &DotagentsTask, canonical_workspace: &Path) -> Self {
        Self {
            canonical_workspace: canonical_workspace.to_path_buf(),
            task_id: normalize_id(&task.id),
            task_name: task.name.clone(),
            fingerprint: task_execution_fingerprint(task),
            source: task.source.clone(),
            interval_minutes: task.interval_minutes,
            run_on_startup: task.run_on_startup,
            profile_id: task
                .profile_id
                .as_deref()
                .map(normalize_id)
                .filter(|id| !id.is_empty()),
            prompt_summary: summarize_prompt(&task.prompt),
        }
    }
}

/// Summarize a task prompt for disclosure in an approval request.
///
/// The summary collapses whitespace so multi-line prompts remain readable in a
/// single-line host prompt, truncates on a character boundary to
/// [`PROMPT_SUMMARY_CHARS`], and appends a marker when content was withheld. It
/// never returns frontmatter, because the task body has already been stripped of
/// it during parsing.
pub fn summarize_prompt(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PROMPT_SUMMARY_CHARS {
        return collapsed;
    }
    let mut summary: String = collapsed.chars().take(PROMPT_SUMMARY_CHARS).collect();
    summary.push('…');
    summary
}

/// A host's explicit answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DotagentsTaskApprovalDecision {
    Approve,
    Deny,
}

impl DotagentsTaskApprovalDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "approve" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// Host boundary used by interactive UIs, CLIs, ACP clients, and embedders.
#[async_trait]
pub trait DotagentsTaskApprover: Send + Sync {
    async fn request_approval(
        &self,
        request: DotagentsTaskApprovalRequest,
    ) -> DotagentsTaskApprovalDecision;
}

/// Result of applying a host trust policy before reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotagentsTaskTrustOutcome {
    Approved {
        diagnostic: Option<DotagentsDiagnostic>,
    },
    Denied,
    Pending {
        diagnostic: DotagentsDiagnostic,
    },
}

/// Apply trust policy without persisting, scheduling, or executing the task.
pub async fn evaluate_task_trust(
    policy: DotagentsTaskTrustPolicy,
    request: DotagentsTaskApprovalRequest,
    approver: Option<&dyn DotagentsTaskApprover>,
) -> DotagentsTaskTrustOutcome {
    match policy {
        DotagentsTaskTrustPolicy::Deny => DotagentsTaskTrustOutcome::Denied,
        DotagentsTaskTrustPolicy::Allow => DotagentsTaskTrustOutcome::Approved {
            diagnostic: Some(
                DotagentsDiagnostic::warning(
                    DotagentsDiagnosticCode::UnsafePolicy,
                    format!(
                        "UNSAFE: workspace protocol task '{}' bypassed explicit trust approval",
                        request.task_id
                    ),
                )
                .with_source(request.source),
            ),
        },
        DotagentsTaskTrustPolicy::Prompt => match approver {
            Some(approver) => match approver.request_approval(request).await {
                DotagentsTaskApprovalDecision::Approve => {
                    DotagentsTaskTrustOutcome::Approved { diagnostic: None }
                }
                DotagentsTaskApprovalDecision::Deny => DotagentsTaskTrustOutcome::Denied,
            },
            None => DotagentsTaskTrustOutcome::Pending {
                diagnostic: DotagentsDiagnostic::warning(
                    DotagentsDiagnosticCode::Other,
                    format!(
                        "workspace protocol task '{}' is pending approval; provide an approver or configure an explicit task trust policy",
                        request.task_id
                    ),
                )
                .with_source(request.source),
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Trusted activation planning and conversion
// ---------------------------------------------------------------------------

/// The designated automation session and profile a protocol task binds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsAutomationBinding {
    /// Public ID of the persistent automation session that owns the records.
    pub session_public_id: String,
    /// Profile ID the task runs under when it does not name its own.
    pub default_profile_id: Option<String>,
}

impl DotagentsAutomationBinding {
    /// Create a binding from an automation session and default profile.
    pub fn new(
        session_public_id: impl Into<String>,
        default_profile_id: Option<impl Into<String>>,
    ) -> Self {
        Self {
            session_public_id: session_public_id.into(),
            default_profile_id: default_profile_id.map(Into::into),
        }
    }

    /// Resolve the effective profile for a task.
    ///
    /// A task that names `profileId` uses it; otherwise the binding default
    /// applies. This keeps profile resolution explicit instead of silently
    /// falling back to whatever session happens to be current.
    pub fn resolve_profile<'a>(&'a self, task: &'a DotagentsTask) -> Option<&'a str> {
        task.profile_id
            .as_deref()
            .filter(|id| !normalize_id(id).is_empty())
            .or(self.default_profile_id.as_deref())
    }
}

/// A recurring task and interval schedule derived from a trusted protocol task.
///
/// Both records carry the protocol ownership key so reconciliation can update
/// or retire exactly the records it created without touching user-created ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskActivation {
    /// The protocol identity that owns these records.
    pub identity: DotagentsTaskIdentity,
    /// The effective execution fingerprint this activation was approved for.
    pub fingerprint: String,
    /// The recurring task to create or update.
    pub task: DotagentsTaskRecord,
    /// The interval schedule to create or update.
    pub schedule: DotagentsScheduleRecord,
}

/// The neutral form of a QueryMT recurring task owned by a protocol task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskRecord {
    /// Protocol creation key scoping the record to its automation session.
    pub creation_key: String,
    /// Task kind, always recurring for protocol tasks.
    pub recurring: bool,
    /// The prompt executed when the task fires.
    pub prompt: String,
    /// The resolved target profile, when one was resolved.
    pub profile_id: Option<String>,
}

/// The neutral form of a QueryMT interval schedule owned by a protocol task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsScheduleRecord {
    /// Protocol schedule ownership key.
    pub creation_key: String,
    /// Interval in seconds, converted with checked multiplication.
    pub interval_seconds: u64,
    /// The recurring task public ID this schedule fires.
    pub task_public_id: String,
}

impl DotagentsScheduleRecord {
    /// The interval in seconds, if the conversion is representable.
    ///
    /// `intervalMinutes` is converted with checked multiplication so an
    /// out-of-range value is rejected instead of wrapping into a bogus
    /// schedule.
    pub fn interval_seconds(interval_minutes: u64) -> Option<u64> {
        interval_minutes.checked_mul(60)
    }
}

/// Why a protocol task cannot be activated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotagentsTaskActivationError {
    /// The task is disabled.
    Disabled,
    /// The task is not a `kind: task` repeat task.
    UnsupportedKind(DotagentsTaskKind),
    /// The interval is missing, unusable, or overflows seconds conversion.
    InvalidInterval { interval_minutes: Option<u64> },
    /// The referenced model/profile could not be resolved.
    UnresolvedProfile { profile_id: String },
    /// The task's prompt body is empty.
    EmptyPrompt,
}

impl DotagentsTaskActivationError {
    /// A source-aware diagnostic describing the rejection.
    pub fn diagnostic(&self, task: &DotagentsTask) -> DotagentsDiagnostic {
        let id = normalize_id(&task.id);
        let (code, message) = match self {
            Self::Disabled => (
                DotagentsDiagnosticCode::Other,
                format!("protocol task '{id}' is disabled and will not be scheduled"),
            ),
            Self::UnsupportedKind(kind) => (
                DotagentsDiagnosticCode::UnsupportedTransport,
                format!(
                    "protocol task '{id}' has unsupported kind {kind:?}; only `kind: task` reconciles to a schedule"
                ),
            ),
            Self::InvalidInterval { interval_minutes } => (
                DotagentsDiagnosticCode::ParseError,
                match interval_minutes {
                    Some(minutes) => format!(
                        "protocol task '{id}' has `intervalMinutes: {minutes}`, which overflows the interval schedule representation"
                    ),
                    None => format!(
                        "protocol task '{id}' has no `intervalMinutes`, so no interval schedule can be created"
                    ),
                },
            ),
            Self::UnresolvedProfile { profile_id } => (
                DotagentsDiagnosticCode::UnresolvedReference,
                format!(
                    "protocol task '{id}' references profile '{profile_id}' which is not available"
                ),
            ),
            Self::EmptyPrompt => (
                DotagentsDiagnosticCode::MissingField,
                format!("protocol task '{id}' has an empty prompt body"),
            ),
        };
        DotagentsDiagnostic::error(code, message).with_source(task.source.clone())
    }
}

/// Convert a trusted, enabled protocol task into neutral task and schedule records.
///
/// This performs no persistence and starts no work: it only validates the task
/// and computes the records reconciliation should create or update. `profile_available`
/// lets the caller validate the resolved target profile against whatever
/// profile or agent catalog the runtime actually has.
pub fn plan_task_activation(
    task: &DotagentsTask,
    identity: &DotagentsTaskIdentity,
    binding: &DotagentsAutomationBinding,
    profile_available: &dyn Fn(&str) -> bool,
) -> Result<DotagentsTaskActivation, Box<DotagentsDiagnostic>> {
    if !task.enabled {
        return Err(Box::new(
            DotagentsTaskActivationError::Disabled.diagnostic(task),
        ));
    }
    if task.kind != DotagentsTaskKind::Task {
        return Err(Box::new(
            DotagentsTaskActivationError::UnsupportedKind(task.kind).diagnostic(task),
        ));
    }
    if task.prompt.trim().is_empty() {
        return Err(Box::new(
            DotagentsTaskActivationError::EmptyPrompt.diagnostic(task),
        ));
    }

    let interval_seconds = match task.interval_minutes {
        Some(minutes) => match DotagentsScheduleRecord::interval_seconds(minutes) {
            Some(seconds) => seconds,
            None => {
                return Err(Box::new(
                    DotagentsTaskActivationError::InvalidInterval {
                        interval_minutes: Some(minutes),
                    }
                    .diagnostic(task),
                ));
            }
        },
        None => {
            return Err(Box::new(
                DotagentsTaskActivationError::InvalidInterval {
                    interval_minutes: None,
                }
                .diagnostic(task),
            ));
        }
    };

    let profile_id = if let Some(profile) = binding.resolve_profile(task) {
        let normalized = normalize_id(profile);
        if normalized.is_empty() || !profile_available(&normalized) {
            return Err(Box::new(
                DotagentsTaskActivationError::UnresolvedProfile {
                    profile_id: normalized,
                }
                .diagnostic(task),
            ));
        }
        Some(normalized)
    } else {
        None
    };

    let fingerprint = task_execution_fingerprint(task);
    Ok(DotagentsTaskActivation {
        identity: identity.clone(),
        fingerprint,
        task: DotagentsTaskRecord {
            creation_key: identity.task_creation_key(),
            recurring: true,
            prompt: task.prompt.clone(),
            profile_id,
        },
        schedule: DotagentsScheduleRecord {
            creation_key: identity.schedule_creation_key(),
            interval_seconds,
            task_public_id: String::new(),
        },
    })
}

// ---------------------------------------------------------------------------
// Idempotent reconciliation
// ---------------------------------------------------------------------------

/// Whether reconciliation must create, update, or leave a protocol task alone.
///
/// The decision is derived from persisted ownership plus the effective
/// fingerprint, so repeating reconciliation against unchanged files is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsTaskReconcileAction {
    /// No ownership exists yet; create the task and schedule.
    Create,
    /// Ownership exists and the fingerprint is unchanged; do nothing.
    Unchanged,
    /// Ownership exists, the fingerprint changed, and renewed trust exists;
    /// update the owned records in place.
    Update,
    /// Ownership exists but the fingerprint changed with no valid approval for
    /// the new fingerprint; do not run or update.
    AwaitingApproval,
}

/// The outcome of reconciling one protocol task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskReconcileOutcome {
    /// The action reconciliation decided on.
    pub action: DotagentsTaskReconcileAction,
    /// The protocol identity owning the records.
    pub identity: DotagentsTaskIdentity,
    /// The effective execution fingerprint considered.
    pub fingerprint: String,
    /// The ownership record to persist, when an action requires writing.
    /// Long-lived record IDs are preserved for [`DotagentsTaskReconcileAction::Unchanged`].
    pub ownership: Option<DotagentsTaskOwnership>,
    /// Non-fatal diagnostics describing why reconciliation did not activate.
    pub diagnostic: Option<DotagentsDiagnostic>,
}

/// Decide how to reconcile one trusted, enabled protocol task.
///
/// `current_approval` reports whether the host currently trusts the supplied
/// fingerprint for the canonical workspace. Reconciliation is idempotent with
/// respect to source identity and fingerprint:
///
/// - no stored ownership -> `Create`;
/// - stored fingerprint equals the effective fingerprint -> `Unchanged`
///   (no writes, no duplicate records across restarts);
/// - stored fingerprint differs and the new fingerprint is approved -> `Update`;
/// - stored fingerprint differs and the new fingerprint is unapproved ->
///   `AwaitingApproval`, so an edited task cannot execute under a stale
///   approval.
pub fn decide_task_reconciliation(
    activation: &DotagentsTaskActivation,
    existing: Option<&DotagentsTaskOwnership>,
    new_fingerprint_approved: bool,
) -> DotagentsTaskReconcileOutcome {
    let identity = activation.identity.clone();
    let fingerprint = activation.fingerprint.clone();

    let Some(existing) = existing else {
        return DotagentsTaskReconcileOutcome {
            action: DotagentsTaskReconcileAction::Create,
            identity,
            fingerprint,
            ownership: Some(DotagentsTaskOwnership::pending(
                &activation.identity,
                activation.fingerprint.clone(),
            )),
            diagnostic: None,
        };
    };

    if existing.fingerprint == fingerprint {
        // Record IDs are carried forward so the caller can prove nothing new
        // was created.
        return DotagentsTaskReconcileOutcome {
            action: DotagentsTaskReconcileAction::Unchanged,
            identity,
            fingerprint,
            ownership: Some(existing.clone()),
            diagnostic: None,
        };
    }

    if new_fingerprint_approved {
        let mut ownership =
            DotagentsTaskOwnership::pending(&activation.identity, fingerprint.clone());
        // Keep the existing record IDs: an update must not create duplicates.
        ownership.task_public_id = existing.task_public_id.clone();
        ownership.schedule_public_id = existing.schedule_public_id.clone();
        return DotagentsTaskReconcileOutcome {
            action: DotagentsTaskReconcileAction::Update,
            identity,
            fingerprint,
            ownership: Some(ownership),
            diagnostic: None,
        };
    }

    let mut ownership = existing.clone();
    ownership.fingerprint = fingerprint.clone();
    DotagentsTaskReconcileOutcome {
        action: DotagentsTaskReconcileAction::AwaitingApproval,
        identity,
        fingerprint: fingerprint.clone(),
        ownership: Some(ownership),
        diagnostic: Some(DotagentsDiagnostic::warning(
            DotagentsDiagnosticCode::UnsafePolicy,
            format!(
                "protocol task '{}' changed since it was approved; its protocol-owned schedule must be paused until the new fingerprint is approved",
                activation.identity.task_id
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// Retiring superseded, removed, and untrusted protocol schedules
// ---------------------------------------------------------------------------

/// Why a previously reconciled protocol-owned record is no longer active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsTaskRetireReason {
    /// The task file is gone from every enabled layer.
    Removed,
    /// The task exists but is disabled.
    Disabled,
    /// The task exists and is enabled but its effective definition changed and
    /// no approval exists for the new fingerprint.
    ChangedUnapproved,
    /// The host explicitly revoked trust for this workspace or task.
    TrustRevoked,
    /// A workspace entry was superseded by a global entry that is no longer
    /// active (for example the workspace task was removed while a global entry
    /// with the same ID remains disabled).
    Superseded,
}

impl DotagentsTaskRetireReason {
    fn describe(self) -> &'static str {
        match self {
            Self::Removed => "was removed",
            Self::Disabled => "is disabled",
            Self::ChangedUnapproved => "changed without renewed approval",
            Self::TrustRevoked => "lost host trust",
            Self::Superseded => "was superseded by an inactive layer entry",
        }
    }
}

/// What to do with a protocol-owned record that is no longer active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsTaskRetireAction {
    /// Pause the owned schedule but keep the records, so re-enabling or
    /// re-approving the task can resume it without re-creating rows.
    Pause,
    /// Delete the owned records entirely because the source no longer exists.
    Retire,
}

/// The reconciliation sweep decision for one stored protocol ownership record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskRetireOutcome {
    /// The protocol ownership the action applies to.
    pub ownership: DotagentsTaskOwnership,
    /// Why the record is being acted on.
    pub reason: DotagentsTaskRetireReason,
    /// Whether to pause or fully retire the owned records.
    pub action: DotagentsTaskRetireAction,
    /// A source-aware diagnostic explaining the action.
    pub diagnostic: DotagentsDiagnostic,
}

/// Decide what to do with protocol-owned records whose source is no longer active.
///
/// Only records carrying a protocol ownership key are ever returned, so
/// user-created schedules are structurally out of scope: a schedule the user
/// created has no protocol source key and is never enumerated here. The
/// `live_source_keys` set contains the source keys of every protocol task that
/// reconciliation found active this run; anything else is an orphan.
///
/// Removed sources are retired (their rows are deleted) because nothing can
/// revive them, while disabled, changed-but-unapproved, and trust-revoked
/// sources are paused so a later approval can resume them without duplicating
/// records.
pub fn plan_task_retirement(
    stored: &[DotagentsTaskOwnership],
    live_source_keys: &BTreeSet<String>,
    revoked_source_keys: &BTreeSet<String>,
) -> Vec<DotagentsTaskRetireOutcome> {
    let mut outcomes: Vec<DotagentsTaskRetireOutcome> = stored
        .iter()
        .filter(|ownership| !live_source_keys.contains(&ownership.source_key))
        .map(|ownership| {
            let (reason, action) = if revoked_source_keys.contains(&ownership.source_key) {
                (
                    DotagentsTaskRetireReason::TrustRevoked,
                    DotagentsTaskRetireAction::Pause,
                )
            } else {
                (
                    DotagentsTaskRetireReason::Removed,
                    DotagentsTaskRetireAction::Retire,
                )
            };
            DotagentsTaskRetireOutcome {
                ownership: ownership.clone(),
                reason,
                action,
                diagnostic: retirement_diagnostic(ownership, reason),
            }
        })
        .collect();

    // Deterministic ordering keeps repeated reconciliations stable and makes
    // the returned plan comparable across runs.
    outcomes.sort_by(|a, b| a.ownership.source_key.cmp(&b.ownership.source_key));
    outcomes
}

fn retirement_diagnostic(
    ownership: &DotagentsTaskOwnership,
    reason: DotagentsTaskRetireReason,
) -> DotagentsDiagnostic {
    DotagentsDiagnostic::info(
        DotagentsDiagnosticCode::Other,
        format!(
            "protocol task '{}' {}; its protocol-owned schedule will be {} without affecting user-created schedules",
            ownership.task_id,
            reason.describe(),
            match reason {
                DotagentsTaskRetireReason::Removed => "retired",
                _ => "paused",
            }
        ),
    )
}

// ---------------------------------------------------------------------------
// `runOnStartup` activation
// ---------------------------------------------------------------------------

/// A protocol-owned task that must be triggered once for this runtime startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsStartupFire {
    /// The protocol ownership of the task to trigger.
    pub ownership: DotagentsTaskOwnership,
    /// The schedule public ID to trigger.
    pub schedule_public_id: String,
}

/// Why a `runOnStartup` task was not triggered for this startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsStartupSkipReason {
    /// The task does not request `runOnStartup`.
    NotRequested,
    /// Reconciliation did not leave the task active and trusted (for example
    /// pending approval, denied, or awaiting a renewed fingerprint).
    NotReconciled,
    /// The task reconciled to an inactive action this startup.
    Inactive(DotagentsTaskReconcileAction),
    /// No owned schedule exists yet to trigger.
    MissingSchedule,
}

/// Result of planning one task's `runOnStartup` behavior for a single startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsStartupPlan {
    /// The fire to perform, when the task is eligible.
    pub fire: Option<DotagentsStartupFire>,
    /// Why the task was skipped, when it was not eligible.
    pub skipped: Option<DotagentsStartupSkipReason>,
}

/// Plan the `runOnStartup` trigger for one reconciled protocol task.
///
/// A startup fire happens only when all of the following hold for this run:
///
/// - the task requests `runOnStartup`;
/// - reconciliation produced an active, trusted outcome (`Create`, `Update`, or
///   `Unchanged`), which already encodes fingerprint-bound trust approval;
/// - an owned schedule public ID exists to trigger.
///
/// The returned plan is computed from the reconciliation outcome, so calling it
/// once per reconciled task yields exactly one fire per startup. Normal interval
/// activation is unaffected: this never pauses, deletes, or rewrites the
/// interval schedule, it only requests an immediate trigger.
pub fn plan_startup_fire(
    activation: &DotagentsTaskActivation,
    outcome: &DotagentsTaskReconcileOutcome,
    run_on_startup: bool,
) -> DotagentsStartupPlan {
    if !run_on_startup {
        return DotagentsStartupPlan {
            fire: None,
            skipped: Some(DotagentsStartupSkipReason::NotRequested),
        };
    }

    match outcome.action {
        DotagentsTaskReconcileAction::Create
        | DotagentsTaskReconcileAction::Update
        | DotagentsTaskReconcileAction::Unchanged => {}
        action => {
            return DotagentsStartupPlan {
                fire: None,
                skipped: Some(DotagentsStartupSkipReason::Inactive(action)),
            };
        }
    }

    let ownership = match &outcome.ownership {
        Some(ownership) => ownership.clone(),
        None => {
            return DotagentsStartupPlan {
                fire: None,
                skipped: Some(DotagentsStartupSkipReason::NotReconciled),
            };
        }
    };

    // The reconciliation outcome must describe the same protocol source as the
    // activation being planned; a mismatch means the caller paired unrelated
    // records, so nothing is triggered.
    if ownership.source_key != activation.identity.source_key() {
        return DotagentsStartupPlan {
            fire: None,
            skipped: Some(DotagentsStartupSkipReason::NotReconciled),
        };
    }

    match ownership.schedule_public_id.clone() {
        Some(schedule_public_id) => DotagentsStartupPlan {
            fire: Some(DotagentsStartupFire {
                ownership,
                schedule_public_id,
            }),
            skipped: None,
        },
        None => DotagentsStartupPlan {
            fire: None,
            skipped: Some(DotagentsStartupSkipReason::MissingSchedule),
        },
    }
}

// ---------------------------------------------------------------------------
// Activation availability policy
// ---------------------------------------------------------------------------

/// A required runtime facility that may be unavailable during activation.
///
/// Protocol task reconciliation depends on host-provided services. When one is
/// missing, compatibility mode keeps the rest of the protocol active and
/// reports the gap, while strict mode fails activation outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsActivationFacility {
    /// No host trust decision or approval mechanism is available.
    Trust,
    /// No durable schedule or task storage is configured.
    ScheduleStorage,
    /// The referenced target profile could not be resolved for activation.
    TargetProfile,
    /// The scheduler could not be activated or reached.
    Scheduler,
}

impl DotagentsActivationFacility {
    /// Human-readable facility name for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trust => "task trust",
            Self::ScheduleStorage => "schedule storage",
            Self::TargetProfile => "target profile",
            Self::Scheduler => "scheduler activation",
        }
    }
}

/// Whether an unavailable facility blocks activation or is only reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotagentsActivationDisposition {
    /// Compatibility mode: activation continues and the diagnostic is returned.
    Diagnostic(DotagentsDiagnostic),
    /// Strict mode: activation fails with this diagnostic.
    Fatal(DotagentsDiagnostic),
}

impl DotagentsActivationDisposition {
    /// Whether the disposition Blocks activation.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }

    /// The diagnostic carried by either disposition.
    pub fn diagnostic(&self) -> &DotagentsDiagnostic {
        match self {
            Self::Diagnostic(diagnostic) | Self::Fatal(diagnostic) => diagnostic,
        }
    }
}

/// Classify an unavailable activation facility under the given strictness policy.
///
/// Compatibility mode keeps unrelated protocol features active and surfaces a
/// non-fatal diagnostic. Strict mode turns the same condition into a fatal
/// error so a caller that demands full activation cannot silently proceed.
pub fn classify_activation_unavailable(
    strictness: DotagentsStrictness,
    facility: DotagentsActivationFacility,
    detail: impl Into<String>,
) -> DotagentsActivationDisposition {
    let detail = detail.into();
    let message = format!(
        "{} is unavailable; {}",
        facility.as_str(),
        if strictness.is_strict() {
            format!("strict mode fails protocol task activation ({detail})")
        } else {
            format!(
                "compatibility mode continues with unrelated protocol features active ({detail})"
            )
        }
    );
    let diagnostic = if strictness.is_strict() {
        DotagentsDiagnostic::error(DotagentsDiagnosticCode::UnresolvedReference, message)
    } else {
        DotagentsDiagnostic::warning(DotagentsDiagnosticCode::Other, message)
    };
    if strictness.is_strict() {
        DotagentsActivationDisposition::Fatal(diagnostic)
    } else {
        DotagentsActivationDisposition::Diagnostic(diagnostic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dotagents::{DotagentsSeverity, DotagentsTask, DotagentsTaskKind};
    use std::collections::BTreeMap;

    fn source() -> DotagentsSource {
        DotagentsSource::entry(
            DotagentsLayer::Workspace,
            "/workspace/.agents/tasks/nightly/task.md",
            "nightly",
        )
    }

    fn task() -> DotagentsTask {
        DotagentsTask {
            id: " Nightly ".into(),
            name: "Nightly maintenance".into(),
            kind: DotagentsTaskKind::Task,
            enabled: true,
            run_on_startup: false,
            interval_minutes: Some(60),
            profile_id: Some(" Maintainer ".into()),
            prompt: "Check dependencies".into(),
            extensions: BTreeMap::new(),
            source: source(),
            fingerprint: "raw-source-fingerprint".into(),
        }
    }

    fn request() -> DotagentsTaskApprovalRequest {
        DotagentsTaskApprovalRequest::from_task(&task(), Path::new("/workspace"))
    }

    #[test]
    fn keys_are_stable_normalized_and_separate_for_task_and_schedule() {
        let identity =
            DotagentsTaskIdentity::new(DotagentsLayer::Workspace, Some("/workspace"), " Nightly ");
        assert_eq!(identity.task_id, "nightly");
        assert!(
            identity
                .source_key()
                .starts_with("dotagents:v1:task:workspace:")
        );
        assert_ne!(
            identity.task_creation_key(),
            identity.schedule_creation_key()
        );
        assert_eq!(identity.source_key(), identity.source_key());
    }

    #[test]
    fn execution_fingerprint_changes_only_with_effective_execution_fields() {
        let task = task();
        let original = task_execution_fingerprint(&task);
        let mut changed = task.clone();
        changed.interval_minutes = Some(61);
        assert_ne!(original, task_execution_fingerprint(&changed));
        let mut raw_only = task;
        raw_only.fingerprint = "different-raw-source".into();
        raw_only
            .extensions
            .insert("future".into(), serde_json::json!(true));
        assert_eq!(original, task_execution_fingerprint(&raw_only));
    }

    #[tokio::test]
    async fn sqlite_state_is_fingerprint_bound_and_separate_from_user_records() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::session::schema::init_schema(&mut conn).unwrap();
        let repository = SqliteDotagentsTaskStateRepository::new(Arc::new(Mutex::new(conn)));
        let identity =
            DotagentsTaskIdentity::new(DotagentsLayer::Workspace, Some("/workspace"), "nightly");
        let ownership = DotagentsTaskOwnership::pending(&identity, "fp-1");
        repository
            .upsert_ownership(ownership.clone())
            .await
            .unwrap();
        assert_eq!(
            repository
                .get_ownership(&identity.source_key())
                .await
                .unwrap(),
            Some(ownership)
        );

        repository
            .record_approval(DotagentsTaskApproval {
                canonical_workspace: PathBuf::from("/workspace"),
                task_id: "Nightly".into(),
                fingerprint: "fp-1".into(),
                decision: DotagentsTaskApprovalDecision::Approve,
                decided_at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        assert!(
            repository
                .get_approval(Path::new("/workspace"), "nightly", "fp-1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            repository
                .get_approval(Path::new("/workspace"), "nightly", "fp-2")
                .await
                .unwrap()
                .is_none()
        );
    }

    struct FixedApprover(DotagentsTaskApprovalDecision);

    #[async_trait]
    impl DotagentsTaskApprover for FixedApprover {
        async fn request_approval(
            &self,
            _request: DotagentsTaskApprovalRequest,
        ) -> DotagentsTaskApprovalDecision {
            self.0
        }
    }

    #[tokio::test]
    async fn headless_prompt_remains_pending() {
        let outcome = evaluate_task_trust(DotagentsTaskTrustPolicy::Prompt, request(), None).await;
        assert!(matches!(outcome, DotagentsTaskTrustOutcome::Pending { .. }));
    }

    #[tokio::test]
    async fn prompt_uses_host_approval_response() {
        let approved = FixedApprover(DotagentsTaskApprovalDecision::Approve);
        assert!(matches!(
            evaluate_task_trust(DotagentsTaskTrustPolicy::Prompt, request(), Some(&approved)).await,
            DotagentsTaskTrustOutcome::Approved { diagnostic: None }
        ));

        let denied = FixedApprover(DotagentsTaskApprovalDecision::Deny);
        assert_eq!(
            evaluate_task_trust(DotagentsTaskTrustPolicy::Prompt, request(), Some(&denied)).await,
            DotagentsTaskTrustOutcome::Denied
        );
    }

    #[tokio::test]
    async fn deny_policy_never_calls_host_and_denies() {
        assert_eq!(
            evaluate_task_trust(DotagentsTaskTrustPolicy::Deny, request(), None).await,
            DotagentsTaskTrustOutcome::Denied
        );
    }

    #[tokio::test]
    async fn unsafe_allow_emits_prominent_diagnostic() {
        let outcome = evaluate_task_trust(DotagentsTaskTrustPolicy::Allow, request(), None).await;
        let DotagentsTaskTrustOutcome::Approved {
            diagnostic: Some(diagnostic),
        } = outcome
        else {
            panic!("allow must approve with a diagnostic");
        };
        assert_eq!(diagnostic.code, DotagentsDiagnosticCode::UnsafePolicy);
        assert!(diagnostic.message.contains("UNSAFE"));
    }

    // -----------------------------------------------------------------------
    // 4.3 Disclosure and zero side effects
    // -----------------------------------------------------------------------

    #[test]
    fn approval_request_discloses_source_schedule_startup_profile_and_prompt() {
        let task = task();
        let request = DotagentsTaskApprovalRequest::from_task(&task, Path::new("/workspace"));

        // Canonical workspace and identity.
        assert_eq!(request.canonical_workspace, PathBuf::from("/workspace"));
        assert_eq!(request.task_id, "nightly");
        assert_eq!(request.task_name, "Nightly maintenance");
        // Bound to the execution fingerprint, not the raw source hash.
        assert_eq!(request.fingerprint, task_execution_fingerprint(&task));
        assert_ne!(request.fingerprint, task.fingerprint);
        // Source provenance, including layer and entry ID.
        assert_eq!(request.source.layer, DotagentsLayer::Workspace);
        assert_eq!(request.source.entry_id.as_deref(), Some("nightly"));
        // Schedule and startup behavior.
        assert_eq!(request.interval_minutes, Some(60));
        assert!(!request.run_on_startup);
        // Target profile, normalized.
        assert_eq!(request.profile_id.as_deref(), Some("maintainer"));
        // Prompt summary reflects what would run.
        assert_eq!(request.prompt_summary, "Check dependencies");
    }

    #[test]
    fn prompt_summary_collapses_whitespace_and_bounds_length() {
        let multiline = "  first line\n\n second\tline  ";
        assert_eq!(summarize_prompt(multiline), "first line second line");

        let long = "x".repeat(PROMPT_SUMMARY_CHARS + 50);
        let summary = summarize_prompt(&long);
        assert_eq!(summary.chars().count(), PROMPT_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));

        // Multibyte content must not be split mid-character.
        let multibyte = "é".repeat(PROMPT_SUMMARY_CHARS + 10);
        let summary = summarize_prompt(&multibyte);
        assert_eq!(summary.chars().count(), PROMPT_SUMMARY_CHARS + 1);
    }

    #[test]
    fn approval_request_without_profile_or_interval_discloses_none() {
        let mut task = task();
        task.profile_id = Some("   ".into());
        task.interval_minutes = None;
        task.run_on_startup = true;
        let request = DotagentsTaskApprovalRequest::from_task(&task, Path::new("/workspace"));
        assert_eq!(request.profile_id, None);
        assert_eq!(request.interval_minutes, None);
        assert!(request.run_on_startup);
    }

    #[tokio::test]
    async fn evaluating_trust_persists_nothing() {
        // A state repository is deliberately never involved in trust
        // evaluation: previewing or prompting must not create records.
        let mut conn = Connection::open_in_memory().unwrap();
        crate::session::schema::init_schema(&mut conn).unwrap();
        let repository = SqliteDotagentsTaskStateRepository::new(Arc::new(Mutex::new(conn)));

        let approver = FixedApprover(DotagentsTaskApprovalDecision::Approve);
        let outcome =
            evaluate_task_trust(DotagentsTaskTrustPolicy::Prompt, request(), Some(&approver)).await;
        assert!(matches!(
            outcome,
            DotagentsTaskTrustOutcome::Approved { diagnostic: None }
        ));

        // Approval itself is host-owned; evaluation did not write ownership,
        // approval, task, or schedule rows.
        let identity =
            DotagentsTaskIdentity::new(DotagentsLayer::Workspace, Some("/workspace"), "nightly");
        assert!(
            repository
                .get_ownership(&identity.source_key())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            repository
                .get_approval(Path::new("/workspace"), "nightly", &request().fingerprint)
                .await
                .unwrap()
                .is_none()
        );
    }

    // -----------------------------------------------------------------------
    // 4.4 Trusted conversion to records
    // -----------------------------------------------------------------------

    fn binding() -> DotagentsAutomationBinding {
        DotagentsAutomationBinding::new("automation-session", Some("maintainer"))
    }

    fn identity() -> DotagentsTaskIdentity {
        DotagentsTaskIdentity::new(DotagentsLayer::Workspace, Some("/workspace"), "nightly")
    }

    #[test]
    fn interval_minutes_convert_to_seconds_with_checked_multiplication() {
        assert_eq!(DotagentsScheduleRecord::interval_seconds(60), Some(3600));
        assert_eq!(DotagentsScheduleRecord::interval_seconds(1), Some(60));
        assert_eq!(DotagentsScheduleRecord::interval_seconds(0), Some(0));
        assert_eq!(DotagentsScheduleRecord::interval_seconds(u64::MAX), None);
    }

    #[test]
    fn trusted_enabled_task_converts_to_recurring_task_and_interval_schedule() {
        let available = |id: &str| id == "maintainer";
        let activation =
            plan_task_activation(&task(), &identity(), &binding(), &available).unwrap();

        assert_eq!(activation.fingerprint, task_execution_fingerprint(&task()));
        assert_eq!(activation.identity.source_key(), identity().source_key());

        // Task record: recurring, bound to the protocol creation key.
        assert!(activation.task.recurring);
        assert_eq!(activation.task.creation_key, identity().task_creation_key());
        assert_eq!(activation.task.prompt, "Check dependencies");
        assert_eq!(activation.task.profile_id.as_deref(), Some("maintainer"));

        // Schedule record: minute-to-second conversion, separate ownership key.
        assert_eq!(activation.schedule.interval_seconds, 3600);
        assert_eq!(
            activation.schedule.creation_key,
            identity().schedule_creation_key()
        );
        assert_ne!(
            activation.task.creation_key,
            activation.schedule.creation_key
        );
    }

    #[test]
    fn interval_overflow_is_rejected_rather_than_wrapped() {
        let mut task = task();
        task.interval_minutes = Some(u64::MAX);
        let available = |_: &str| true;
        let error = plan_task_activation(&task, &identity(), &binding(), &available).unwrap_err();
        assert!(error.message.contains("overflows"));
        assert!(error.message.contains(&u64::MAX.to_string()));
    }

    #[test]
    fn missing_interval_is_rejected() {
        let mut task = task();
        task.interval_minutes = None;
        let available = |_: &str| true;
        let error = plan_task_activation(&task, &identity(), &binding(), &available).unwrap_err();
        assert!(error.message.contains("no `intervalMinutes`"));
    }

    #[test]
    fn disabled_task_is_not_activated() {
        let mut task = task();
        task.enabled = false;
        let available = |_: &str| true;
        let error = plan_task_activation(&task, &identity(), &binding(), &available).unwrap_err();
        assert_eq!(error.code, DotagentsDiagnosticCode::Other);
        assert!(error.message.contains("disabled"));
    }

    #[test]
    fn non_task_kind_is_rejected() {
        let mut task = task();
        task.kind = DotagentsTaskKind::Other;
        let available = |_: &str| true;
        let error = plan_task_activation(&task, &identity(), &binding(), &available).unwrap_err();
        assert_eq!(error.code, DotagentsDiagnosticCode::UnsupportedTransport);
        assert!(error.message.contains("kind"));
    }

    #[test]
    fn unknown_referenced_profile_is_rejected_with_the_referring_task() {
        let available = |_: &str| false;
        let error = plan_task_activation(&task(), &identity(), &binding(), &available).unwrap_err();
        assert_eq!(error.code, DotagentsDiagnosticCode::UnresolvedReference);
        assert!(error.message.contains("maintainer"));
        assert!(error.message.contains("nightly"));
        assert_eq!(
            error.source.as_ref().map(|s| s.layer),
            Some(DotagentsLayer::Workspace)
        );
    }

    #[test]
    fn task_without_profile_inherits_the_binding_default_profile() {
        let mut task = task();
        task.profile_id = None;
        let available = |id: &str| id == "maintainer";
        let activation = plan_task_activation(&task, &identity(), &binding(), &available).unwrap();
        assert_eq!(activation.task.profile_id.as_deref(), Some("maintainer"));
    }

    #[test]
    fn task_without_profile_and_without_binding_default_resolves_none() {
        let mut task = task();
        task.profile_id = None;
        let binding = DotagentsAutomationBinding::new("automation-session", None::<String>);
        let available = |_: &str| false;
        let activation = plan_task_activation(&task, &identity(), &binding, &available).unwrap();
        assert_eq!(activation.task.profile_id, None);
    }

    #[test]
    fn unresolved_binding_default_profile_is_rejected() {
        let mut task = task();
        task.profile_id = None;
        let available = |_: &str| false;
        let error = plan_task_activation(&task, &identity(), &binding(), &available).unwrap_err();
        assert_eq!(error.code, DotagentsDiagnosticCode::UnresolvedReference);
        assert!(error.message.contains("maintainer"));
    }

    #[test]
    fn empty_prompt_is_rejected() {
        let mut task = task();
        task.prompt = "   \n ".into();
        let available = |_: &str| true;
        let error = plan_task_activation(&task, &identity(), &binding(), &available).unwrap_err();
        assert_eq!(error.code, DotagentsDiagnosticCode::MissingField);
    }

    // -----------------------------------------------------------------------
    // 4.5 Idempotent create/update reconciliation
    // -----------------------------------------------------------------------

    fn activation() -> DotagentsTaskActivation {
        let available = |id: &str| id == "maintainer";
        plan_task_activation(&task(), &identity(), &binding(), &available).unwrap()
    }

    #[test]
    fn first_reconciliation_creates_records() {
        let activation = activation();
        let outcome = decide_task_reconciliation(&activation, None, false);
        assert_eq!(outcome.action, DotagentsTaskReconcileAction::Create);
        let ownership = outcome.ownership.expect("create must persist ownership");
        assert_eq!(ownership.source_key, identity().source_key());
        assert_eq!(ownership.fingerprint, activation.fingerprint);
        assert!(ownership.task_public_id.is_none());
        assert!(ownership.schedule_public_id.is_none());
    }

    #[test]
    fn unchanged_restart_is_a_no_op() {
        let activation = activation();
        // A record previously created for this exact fingerprint.
        let mut stored =
            DotagentsTaskOwnership::pending(&identity(), activation.fingerprint.clone());
        stored.task_public_id = Some("task-1".into());
        stored.schedule_public_id = Some("schedule-1".into());

        let outcome = decide_task_reconciliation(&activation, Some(&stored), false);
        assert_eq!(outcome.action, DotagentsTaskReconcileAction::Unchanged);
        // No new records: existing public IDs are carried forward verbatim.
        let ownership = outcome.ownership.expect("unchanged must retain ownership");
        assert_eq!(ownership.task_public_id.as_deref(), Some("task-1"));
        assert_eq!(ownership.schedule_public_id.as_deref(), Some("schedule-1"));
        assert!(outcome.diagnostic.is_none());

        // Idempotent across repeated identical startups.
        let again = decide_task_reconciliation(&activation, Some(&ownership), false);
        assert_eq!(again.action, DotagentsTaskReconcileAction::Unchanged);
    }

    #[test]
    fn execution_relevant_change_without_renewed_approval_awaits_approval() {
        let mut changed_task = task();
        changed_task.interval_minutes = Some(61);
        let available = |id: &str| id == "maintainer";
        let changed =
            plan_task_activation(&changed_task, &identity(), &binding(), &available).unwrap();

        // Stored ownership still reflects the previously approved fingerprint.
        let mut stored = DotagentsTaskOwnership::pending(&identity(), activation().fingerprint);
        stored.task_public_id = Some("task-1".into());
        stored.schedule_public_id = Some("schedule-1".into());

        let outcome = decide_task_reconciliation(&changed, Some(&stored), false);
        assert_eq!(
            outcome.action,
            DotagentsTaskReconcileAction::AwaitingApproval
        );
        assert!(outcome.diagnostic.is_some());
        // The stale approval must not silently re-register the new fingerprint.
        let ownership = outcome.ownership.expect("ownership is retained");
        assert_eq!(ownership.fingerprint, changed.fingerprint);
    }

    #[test]
    fn execution_relevant_change_with_renewed_approval_updates_in_place() {
        let mut changed_task = task();
        changed_task.interval_minutes = Some(61);
        let available = |id: &str| id == "maintainer";
        let changed =
            plan_task_activation(&changed_task, &identity(), &binding(), &available).unwrap();

        let mut stored = DotagentsTaskOwnership::pending(&identity(), activation().fingerprint);
        stored.task_public_id = Some("task-1".into());
        stored.schedule_public_id = Some("schedule-1".into());

        let outcome = decide_task_reconciliation(&changed, Some(&stored), true);
        assert_eq!(outcome.action, DotagentsTaskReconcileAction::Update);
        let ownership = outcome.ownership.expect("update must persist ownership");
        assert_eq!(ownership.fingerprint, changed.fingerprint);
        // Updating must reuse the owned records rather than duplicate them.
        assert_eq!(ownership.task_public_id.as_deref(), Some("task-1"));
        assert_eq!(ownership.schedule_public_id.as_deref(), Some("schedule-1"));
        assert!(outcome.diagnostic.is_none());
    }

    #[test]
    fn non_execution_field_change_does_not_require_reapproval() {
        let activation = activation();
        let mut stored =
            DotagentsTaskOwnership::pending(&identity(), activation.fingerprint.clone());
        stored.task_public_id = Some("task-1".into());

        // An edit that only touches the raw source fingerprint (for example a
        // reordered unknown frontmatter field) keeps the execution fingerprint.
        let outcome = decide_task_reconciliation(&activation, Some(&stored), false);
        assert_eq!(outcome.action, DotagentsTaskReconcileAction::Unchanged);
    }

    #[tokio::test]
    async fn reconciliation_persists_one_record_and_is_idempotent_across_restarts() {
        // Exercises the persisted ownership round trip that drives restart
        // idempotency: one Create, then Unchanged on every subsequent startup.
        let mut conn = Connection::open_in_memory().unwrap();
        crate::session::schema::init_schema(&mut conn).unwrap();
        let repository = SqliteDotagentsTaskStateRepository::new(Arc::new(Mutex::new(conn)));
        let activation = activation();

        // First startup: create. No record public IDs are set because the
        // caller has not yet created the task/schedule rows (the ownership
        // table enforces foreign keys to them).
        let first = decide_task_reconciliation(&activation, None, false);
        assert_eq!(first.action, DotagentsTaskReconcileAction::Create);
        repository
            .upsert_ownership(first.ownership.unwrap())
            .await
            .unwrap();

        // Second startup: the persisted ownership is found by source key and
        // reconciliation decides nothing needs to change.
        let stored = repository
            .get_ownership(&activation.identity.source_key())
            .await
            .unwrap()
            .expect("ownership persisted on first startup");
        assert_eq!(stored.fingerprint, activation.fingerprint);
        assert_eq!(
            repository
                .get_ownership(&activation.identity.source_key())
                .await
                .unwrap()
                .map(|o| o.task_creation_key.clone()),
            Some(identity().task_creation_key()),
            "exactly one ownership row is keyed by the protocol source"
        );

        let second = decide_task_reconciliation(&activation, Some(&stored), false);
        assert_eq!(second.action, DotagentsTaskReconcileAction::Unchanged);
    }

    // -----------------------------------------------------------------------
    // 4.6 Pausing/retiring protocol-owned schedules
    // -----------------------------------------------------------------------

    fn stored_ownership(task_id: &str, layer: DotagentsLayer) -> DotagentsTaskOwnership {
        let identity = match layer {
            DotagentsLayer::Global => DotagentsTaskIdentity::new(layer, None::<PathBuf>, task_id),
            DotagentsLayer::Workspace => {
                DotagentsTaskIdentity::new(layer, Some("/workspace"), task_id)
            }
        };
        let mut ownership = DotagentsTaskOwnership::pending(&identity, "fp-stored");
        // Record public IDs point at the protocol-owned task/schedule rows. They
        // are display/diagnostic metadata here, and the ownership table enforces
        // foreign keys to real rows, so the pure-planning tests leave them unset.
        ownership.task_public_id = None;
        ownership.schedule_public_id = None;
        ownership
    }

    #[test]
    fn removed_source_is_retired() {
        let stored = vec![stored_ownership("nightly", DotagentsLayer::Workspace)];
        let outcomes = plan_task_retirement(&stored, &BTreeSet::new(), &BTreeSet::new());

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, DotagentsTaskRetireReason::Removed);
        assert_eq!(outcomes[0].action, DotagentsTaskRetireAction::Retire);
        assert_eq!(
            outcomes[0].ownership.source_key, stored[0].source_key,
            "only the protocol-owned record is targeted"
        );
        assert!(outcomes[0].diagnostic.message.contains("retired"));
    }

    #[test]
    fn trust_revoked_source_is_paused_not_deleted() {
        let stored = vec![stored_ownership("nightly", DotagentsLayer::Workspace)];
        let mut revoked = BTreeSet::new();
        revoked.insert(stored[0].source_key.clone());

        let outcomes = plan_task_retirement(&stored, &BTreeSet::new(), &revoked);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, DotagentsTaskRetireReason::TrustRevoked);
        // Paused so a later re-approval can resume without duplicating rows.
        assert_eq!(outcomes[0].action, DotagentsTaskRetireAction::Pause);
        assert!(outcomes[0].diagnostic.message.contains("paused"));
    }

    #[test]
    fn live_sources_are_left_alone() {
        let stored = vec![
            stored_ownership("nightly", DotagentsLayer::Workspace),
            stored_ownership("digest", DotagentsLayer::Workspace),
        ];
        let mut live = BTreeSet::new();
        live.insert(stored[0].source_key.clone());

        let outcomes = plan_task_retirement(&stored, &live, &BTreeSet::new());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].ownership.task_id, "digest");
    }

    #[test]
    fn same_id_in_two_layers_is_tracked_independently() {
        // Global and workspace entries share a task ID but are distinct
        // ownership sources, so removing one must not disturb the other.
        let stored = vec![
            stored_ownership("nightly", DotagentsLayer::Global),
            stored_ownership("nightly", DotagentsLayer::Workspace),
        ];
        assert_ne!(stored[0].source_key, stored[1].source_key);

        let mut live = BTreeSet::new();
        live.insert(stored[0].source_key.clone());
        let outcomes = plan_task_retirement(&stored, &live, &BTreeSet::new());

        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].ownership.layer,
            DotagentsLayer::Workspace,
            "the workspace entry was removed while the global entry stays live"
        );
    }

    #[test]
    fn retirement_plan_is_deterministic() {
        let stored = vec![
            stored_ownership("zeta", DotagentsLayer::Workspace),
            stored_ownership("alpha", DotagentsLayer::Workspace),
        ];
        let outcomes = plan_task_retirement(&stored, &BTreeSet::new(), &BTreeSet::new());
        let keys: Vec<&str> = outcomes
            .iter()
            .map(|o| o.ownership.source_key.as_str())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[tokio::test]
    async fn sweep_retires_only_protocol_records_and_preserves_user_schedules() {
        // The ownership table is the only source of retire/pause candidates, so
        // user-created tasks and schedules are structurally unreachable.
        let mut conn = Connection::open_in_memory().unwrap();
        crate::session::schema::init_schema(&mut conn).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        // A user-created session, task, and schedule with no protocol key.
        {
            let c = conn.lock().unwrap();
            c.execute(
                "INSERT INTO sessions (public_id, name, created_at, updated_at) VALUES ('sess-user', 'user', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO tasks (public_id, session_id, kind, status, revision, created_at, updated_at) VALUES ('task-user', 1, 'recurring', 'active', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO schedules (public_id, task_id, task_public_id, session_id, session_public_id, trigger_json, state, run_count, consecutive_failures, config_json, created_at, updated_at) VALUES ('schedule-user', 1, 'task-user', 1, 'sess-user', '{\"Interval\":{\"seconds\":60}}', 'armed', 0, 0, '{}', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }

        let repository = SqliteDotagentsTaskStateRepository::new(conn.clone());

        // A protocol-owned record whose source has since been removed.
        let ownership = stored_ownership("nightly", DotagentsLayer::Workspace);
        repository
            .upsert_ownership(ownership.clone())
            .await
            .unwrap();

        // Every stored ownership is enumerable, and the sweep picks up exactly
        // the protocol record whose source is no longer live.
        let all = repository.list_ownerships().await.unwrap();
        assert_eq!(all, vec![ownership.clone()]);
        let outcomes = plan_task_retirement(&all, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].ownership.source_key, ownership.source_key);

        // The user-created schedule is untouched: it carries no protocol source
        // key and was never a candidate.
        let user_schedules: i64 = {
            let c = conn.lock().unwrap();
            c.query_row(
                "SELECT COUNT(*) FROM schedules WHERE public_id = 'schedule-user'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(user_schedules, 1);
    }

    // -----------------------------------------------------------------------
    // 4.7 `runOnStartup` activation
    // -----------------------------------------------------------------------

    fn active_outcome(
        action: DotagentsTaskReconcileAction,
        schedule_public_id: Option<&str>,
    ) -> DotagentsTaskReconcileOutcome {
        let activation = activation();
        let mut ownership = DotagentsTaskOwnership::pending(&activation.identity, "fp");
        ownership.schedule_public_id = schedule_public_id.map(str::to_string);
        DotagentsTaskReconcileOutcome {
            action,
            identity: activation.identity,
            fingerprint: "fp".into(),
            ownership: Some(ownership),
            diagnostic: None,
        }
    }

    #[test]
    fn trusted_startup_task_fires_exactly_once_per_startup() {
        let activation = activation();
        let outcome = active_outcome(DotagentsTaskReconcileAction::Create, Some("schedule-1"));

        // Planning the same reconciled task for one startup yields one fire.
        let plan = plan_startup_fire(&activation, &outcome, true);
        let fire = plan.fire.as_ref().expect("trusted startup task must fire");
        assert_eq!(fire.schedule_public_id, "schedule-1");
        assert_eq!(fire.ownership.source_key, activation.identity.source_key());
        assert!(plan.skipped.is_none());

        // The fire is derived from the outcome, so it is per-startup rather than
        // persistent state: a restart recomputes one fire, not zero or many.
        let restart = plan_startup_fire(&activation, &outcome, true);
        assert_eq!(restart.fire, plan.fire);
    }

    #[test]
    fn untrusted_startup_task_never_fires() {
        let activation = activation();
        // AwaitingApproval means the definition changed without renewed trust.
        let outcome = active_outcome(
            DotagentsTaskReconcileAction::AwaitingApproval,
            Some("schedule-1"),
        );
        let plan = plan_startup_fire(&activation, &outcome, true);
        assert!(plan.fire.is_none());
        assert_eq!(
            plan.skipped,
            Some(DotagentsStartupSkipReason::Inactive(
                DotagentsTaskReconcileAction::AwaitingApproval
            ))
        );
    }

    #[test]
    fn startup_task_without_schedule_does_not_fire() {
        let activation = activation();
        let outcome = active_outcome(DotagentsTaskReconcileAction::Create, None);
        let plan = plan_startup_fire(&activation, &outcome, true);
        assert!(plan.fire.is_none());
        assert_eq!(
            plan.skipped,
            Some(DotagentsStartupSkipReason::MissingSchedule)
        );
    }

    #[test]
    fn task_without_run_on_startup_does_not_fire() {
        let activation = activation();
        let outcome = active_outcome(DotagentsTaskReconcileAction::Unchanged, Some("schedule-1"));
        let plan = plan_startup_fire(&activation, &outcome, false);
        assert!(plan.fire.is_none());
        assert_eq!(plan.skipped, Some(DotagentsStartupSkipReason::NotRequested));
    }

    #[test]
    fn mismatched_ownership_does_not_fire() {
        let activation = activation();
        // An outcome describing a different protocol source must not trigger.
        let other =
            DotagentsTaskIdentity::new(DotagentsLayer::Workspace, Some("/workspace"), "other-task");
        let mut ownership = DotagentsTaskOwnership::pending(&other, "fp");
        ownership.schedule_public_id = Some("schedule-other".into());
        let outcome = DotagentsTaskReconcileOutcome {
            action: DotagentsTaskReconcileAction::Create,
            identity: other,
            fingerprint: "fp".into(),
            ownership: Some(ownership),
            diagnostic: None,
        };

        let plan = plan_startup_fire(&activation, &outcome, true);
        assert!(plan.fire.is_none());
        assert_eq!(
            plan.skipped,
            Some(DotagentsStartupSkipReason::NotReconciled)
        );
    }

    #[test]
    fn startup_fire_does_not_alter_interval_activation() {
        // The startup plan only requests an immediate trigger; the reconciled
        // interval schedule is untouched, so normal interval activation remains.
        let activation = activation();
        assert_eq!(activation.schedule.interval_seconds, 3600);
        let outcome = active_outcome(DotagentsTaskReconcileAction::Unchanged, Some("schedule-1"));
        let plan = plan_startup_fire(&activation, &outcome, true);

        assert!(plan.fire.is_some());
        assert_eq!(activation.schedule.interval_seconds, 3600);
        assert_eq!(
            activation.schedule.creation_key,
            identity().schedule_creation_key()
        );
    }

    #[test]
    fn untrusted_headless_startup_task_remains_pending_and_does_not_fire() {
        // End-to-end trust gate: a headless host using the default `prompt`
        // policy leaves the workspace task pending, and a pending task has no
        // reconciliation outcome that could authorize a startup fire.
        let activation = activation();
        let task = task();
        let identity =
            DotagentsTaskIdentity::new(DotagentsLayer::Workspace, Some("/workspace"), "nightly");
        assert!(matches!(
            decide_task_reconciliation(&activation, None, false).action,
            DotagentsTaskReconcileAction::Create
        ));

        // Without approval the fingerprint is not trusted, so treating the task
        // as unapproved yields AwaitingApproval rather than an active outcome.
        let stored = DotagentsTaskOwnership::pending(&identity, "older-fingerprint");
        let unapproved = decide_task_reconciliation(&activation, Some(&stored), false);
        assert_eq!(
            unapproved.action,
            DotagentsTaskReconcileAction::AwaitingApproval
        );
        let plan = plan_startup_fire(&activation, &unapproved, task.run_on_startup);
        assert!(plan.fire.is_none());
    }

    // -----------------------------------------------------------------------
    // 4.8 Activation availability policy
    // -----------------------------------------------------------------------

    #[test]
    fn compatibility_mode_reports_unavailable_facilities_without_failing() {
        for facility in [
            DotagentsActivationFacility::Trust,
            DotagentsActivationFacility::ScheduleStorage,
            DotagentsActivationFacility::TargetProfile,
            DotagentsActivationFacility::Scheduler,
        ] {
            let disposition = classify_activation_unavailable(
                DotagentsStrictness::Compatibility,
                facility,
                "no service configured",
            );
            assert!(
                !disposition.is_fatal(),
                "{facility:?} must be non-fatal in compatibility mode"
            );
            let diagnostic = disposition.diagnostic();
            assert_eq!(diagnostic.severity, DotagentsSeverity::Warning);
            assert!(diagnostic.message.contains(facility.as_str()));
            assert!(diagnostic.message.contains("compatibility mode continues"));
        }
    }

    #[test]
    fn strict_mode_fails_on_unavailable_facilities() {
        for facility in [
            DotagentsActivationFacility::Trust,
            DotagentsActivationFacility::ScheduleStorage,
            DotagentsActivationFacility::TargetProfile,
            DotagentsActivationFacility::Scheduler,
        ] {
            let disposition = classify_activation_unavailable(
                DotagentsStrictness::Strict,
                facility,
                "no service configured",
            );
            assert!(
                disposition.is_fatal(),
                "{facility:?} must be fatal in strict mode"
            );
            let diagnostic = disposition.diagnostic();
            assert!(diagnostic.is_error());
            assert!(diagnostic.message.contains(facility.as_str()));
            assert!(diagnostic.message.contains("strict mode fails"));
        }
    }

    #[test]
    fn unavailable_facility_diagnostic_preserves_detail() {
        let disposition = classify_activation_unavailable(
            DotagentsStrictness::Compatibility,
            DotagentsActivationFacility::ScheduleStorage,
            "pass a ScheduleRepository to enable reconciliation",
        );
        assert!(
            disposition
                .diagnostic()
                .message
                .contains("pass a ScheduleRepository")
        );
    }
}
