//! SQLite-backed workspace repository exposed through the storage-backend
//! pattern.
//!
//! Persists [`WorkspaceRecord`]s (the workspace registry) and retained
//! [`ResultProvenance`]s. Ownership uniqueness (one writable session owner
//! per managed workspace) and checkout-path uniqueness (removed paths are
//! never reused) are enforced by partial unique indexes in the schema and
//! surfaced as explicit [`SessionError`] variants.

use crate::session::error::{SessionError, SessionResult};
use crate::worktree::domain::{
    Baseline, BaselineMode, ExecutionMode, ManagedBranch, ProvisioningPhase, RepositoryIdentity,
    ResultProvenance, ResultVerificationStatus, WorkspaceId, WorkspaceLifecycle,
    WorkspaceOwnership, WorkspaceRecord,
};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// Durable registry of managed workspaces.
#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    /// Insert a new workspace record. Fails if the checkout path or
    /// workspace id already exists.
    async fn insert_workspace(&self, record: WorkspaceRecord) -> SessionResult<WorkspaceRecord>;

    /// Fetch a workspace by id.
    async fn get_workspace(&self, id: &WorkspaceId) -> SessionResult<Option<WorkspaceRecord>>;

    /// Fetch a workspace by its checkout path (canonical absolute path).
    async fn get_workspace_by_checkout(
        &self,
        checkout_path: &Path,
    ) -> SessionResult<Option<WorkspaceRecord>>;

    /// Fetch the workspace owned by `session_id`, if any. At most one
    /// workspace can be owned by a session (unique writable-owner index),
    /// so this is the durable session→workspace binding used by load and
    /// resume.
    async fn get_workspace_by_session(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<WorkspaceRecord>>;

    /// The workspace allocated for one delegation, when it ran isolated.
    async fn get_workspace_by_delegation(
        &self,
        delegation_id: &str,
    ) -> SessionResult<Option<WorkspaceRecord>> {
        Ok(self
            .list_workspaces()
            .await?
            .into_iter()
            .find(|r| r.ownership.delegation_id.as_deref() == Some(delegation_id)))
    }

    /// List all workspaces, newest first.
    async fn list_workspaces(&self) -> SessionResult<Vec<WorkspaceRecord>>;

    /// List workspaces sharing one repository identity (canonical common
    /// directory), newest first.
    async fn list_workspaces_by_repository(
        &self,
        common_dir: &Path,
    ) -> SessionResult<Vec<WorkspaceRecord>>;

    /// List the workspaces of one parallel delegation batch.
    async fn list_workspaces_by_batch(&self, batch_id: &str)
    -> SessionResult<Vec<WorkspaceRecord>>;

    /// Full-row update of mutable workspace fields. The record must exist.
    async fn update_workspace(&self, record: WorkspaceRecord) -> SessionResult<()>;

    /// Compare-and-swap lifecycle transition. Returns `false` when the
    /// workspace is not in `from`.
    async fn transition_lifecycle(
        &self,
        id: &WorkspaceId,
        from: WorkspaceLifecycle,
        to: WorkspaceLifecycle,
    ) -> SessionResult<bool>;

    /// Bind the session that owns this workspace. Fails with
    /// [`SessionError::WorkspaceAlreadyOwned`] when another session already
    /// owns the workspace (unique writable-owner rule).
    async fn attach_owner(
        &self,
        id: &WorkspaceId,
        owner_session_id: &str,
    ) -> SessionResult<WorkspaceRecord>;

    /// Clear the session owner binding.
    async fn detach_owner(&self, id: &WorkspaceId) -> SessionResult<WorkspaceRecord>;

    /// Persist the captured immutable baseline for a workspace.
    async fn bind_baseline(&self, id: &WorkspaceId, baseline: &Baseline) -> SessionResult<()>;

    /// Advance the durable provisioning phase of a `provisioning` workspace.
    ///
    /// Forward-only: an attempt to move to an equal or earlier phase is a
    /// no-op (`Ok(false)`), so idempotent replay can safely re-record steps.
    /// Unknown workspace ids also yield `Ok(false)`.
    async fn record_provisioning_phase(
        &self,
        id: &WorkspaceId,
        phase: ProvisioningPhase,
    ) -> SessionResult<bool>;

    /// Persist retained result provenance for a workspace.
    async fn record_result(
        &self,
        id: &WorkspaceId,
        provenance: &ResultProvenance,
    ) -> SessionResult<()>;

    /// Retained results for a workspace, newest first.
    async fn list_results(&self, id: &WorkspaceId) -> SessionResult<Vec<ResultProvenance>>;
}

/// SQLite implementation of [`WorkspaceRepository`].
pub struct SqliteWorkspaceRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteWorkspaceRepository {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    async fn run_blocking<F, R>(&self, f: F) -> SessionResult<R>
    where
        F: FnOnce(&mut Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let conn_arc = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn_arc.lock().unwrap();
            f(&mut conn)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Workspace task execution failed: {}", e)))?
        .map_err(map_workspace_db_error)
    }
}

/// Map constraint violations on the workspace tables to explicit errors and
/// pass everything else through.
fn map_workspace_db_error(err: rusqlite::Error) -> SessionError {
    let msg = err.to_string();
    if msg.contains("idx_workspaces_owner") || msg.contains("workspaces.owner_session_id") {
        SessionError::InvalidOperation("workspace already has a writable session owner".to_string())
    } else if msg.contains("idx_workspaces_checkout") || msg.contains("workspaces.checkout_path") {
        SessionError::WorkspaceCheckoutTaken(msg)
    } else {
        SessionError::from(err)
    }
}

/// Helper: format an `OffsetDateTime` as RFC 3339 for SQLite TEXT storage.
fn format_dt(dt: &OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Helper: parse an RFC 3339 string back into `OffsetDateTime`.
fn parse_dt(s: &str) -> Result<OffsetDateTime, rusqlite::Error> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map_err(|_| rusqlite::Error::InvalidQuery)
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

const SELECT_COLS: &str = "public_id, common_dir, origin_path, project_name, checkout_path, \
     branch, relative_cwd, execution, baseline_mode, source_head, baseline_commit, lifecycle, \
     owner_session_id, parent_workspace_id, parent_session_id, delegation_id, \
     delegation_batch_id, last_error, created_at, updated_at, \
     requested_base_ref, operation_id, provisioning_phase";

fn select_by_id_sql() -> String {
    format!("SELECT {SELECT_COLS} FROM workspaces WHERE public_id = ?1")
}

fn select_by_owner_sql() -> String {
    format!("SELECT {SELECT_COLS} FROM workspaces WHERE owner_session_id = ?1 LIMIT 1")
}

fn row_to_workspace(row: &rusqlite::Row<'_>) -> Result<WorkspaceRecord, rusqlite::Error> {
    let public_id: String = row.get("public_id")?;
    let common_dir: String = row.get("common_dir")?;
    let origin_path: String = row.get("origin_path")?;
    let project_name: String = row.get("project_name")?;
    let checkout_path: String = row.get("checkout_path")?;
    let branch: String = row.get("branch")?;
    let relative_cwd: String = row.get("relative_cwd")?;
    let execution: String = row.get("execution")?;
    let baseline_mode: String = row.get("baseline_mode")?;
    let source_head: Option<String> = row.get("source_head")?;
    let baseline_commit: Option<String> = row.get("baseline_commit")?;
    let lifecycle: String = row.get("lifecycle")?;
    let owner_session_id: Option<String> = row.get("owner_session_id")?;
    let parent_workspace_id: Option<String> = row.get("parent_workspace_id")?;
    let parent_session_id: Option<String> = row.get("parent_session_id")?;
    let delegation_id: Option<String> = row.get("delegation_id")?;
    let delegation_batch_id: Option<String> = row.get("delegation_batch_id")?;
    let last_error: Option<String> = row.get("last_error")?;
    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;
    let requested_base_ref: Option<String> = row.get("requested_base_ref")?;
    let operation_id: Option<String> = row.get("operation_id")?;
    let provisioning_phase: Option<String> = row.get("provisioning_phase")?;

    let mode = BaselineMode::from_str(&baseline_mode).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let baseline = match (source_head, baseline_commit) {
        (Some(source_head), Some(baseline_commit)) => Some(Baseline {
            mode,
            source_head,
            baseline_commit,
        }),
        // Capture pending: keep the creation-time mode without a baseline yet.
        _ => None,
    };

    Ok(WorkspaceRecord {
        id: WorkspaceId::from_raw(public_id),
        repository: RepositoryIdentity {
            common_dir: PathBuf::from(common_dir),
            origin_path: PathBuf::from(origin_path),
            project_name,
        },
        checkout_path: PathBuf::from(checkout_path),
        branch: ManagedBranch::from_raw(branch),
        relative_cwd: PathBuf::from(relative_cwd),
        execution: ExecutionMode::from_str(&execution)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        baseline,
        lifecycle: WorkspaceLifecycle::from_str(&lifecycle)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        ownership: WorkspaceOwnership {
            owner_session_id,
            parent_workspace_id: parent_workspace_id.map(WorkspaceId::from_raw),
            parent_session_id,
            delegation_id,
            delegation_batch_id,
        },
        last_error,
        requested_baseline_mode: mode,
        requested_base_ref,
        operation_id: operation_id.unwrap_or_default(),
        provisioning_phase: provisioning_phase
            .map(|p| ProvisioningPhase::from_str(&p))
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        created_at: parse_dt(&created_at)?,
        updated_at: parse_dt(&updated_at)?,
    })
}

#[async_trait]
impl WorkspaceRepository for SqliteWorkspaceRepository {
    async fn insert_workspace(&self, record: WorkspaceRecord) -> SessionResult<WorkspaceRecord> {
        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO workspaces (
                    public_id, common_dir, origin_path, project_name, checkout_path,
                    branch, relative_cwd, execution, baseline_mode, source_head,
                    baseline_commit, lifecycle, owner_session_id, parent_workspace_id,
                    parent_session_id, delegation_id, delegation_batch_id, last_error,
                    created_at, updated_at, requested_base_ref, operation_id,
                    provisioning_phase
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
                params![
                    record.id.as_str(),
                    path_text(&record.repository.common_dir),
                    path_text(&record.repository.origin_path),
                    record.repository.project_name,
                    path_text(&record.checkout_path),
                    record.branch.as_str(),
                    path_text(&record.relative_cwd),
                    record.execution.to_string(),
                    record
                        .baseline
                        .as_ref()
                        .map(|b| b.mode)
                        .unwrap_or(record.requested_baseline_mode)
                        .to_string(),
                    record.baseline.as_ref().map(|b| b.source_head.clone()),
                    record.baseline.as_ref().map(|b| b.baseline_commit.clone()),
                    record.lifecycle.to_string(),
                    record.ownership.owner_session_id,
                    record
                        .ownership
                        .parent_workspace_id
                        .as_ref()
                        .map(|id| id.to_string()),
                    record.ownership.parent_session_id,
                    record.ownership.delegation_id,
                    record.ownership.delegation_batch_id,
                    record.last_error,
                    format_dt(&record.created_at),
                    format_dt(&record.updated_at),
                    record.requested_base_ref,
                    record.operation_id,
                    record.provisioning_phase.map(|p| p.to_string()),
                ],
            )?;
            Ok(record)
        })
        .await
    }

    async fn get_workspace(&self, id: &WorkspaceId) -> SessionResult<Option<WorkspaceRecord>> {
        let id = id.as_str().to_string();
        self.run_blocking(move |conn| {
            conn.query_row(&select_by_id_sql(), params![id], row_to_workspace)
                .optional()
        })
        .await
    }

    async fn get_workspace_by_checkout(
        &self,
        checkout_path: &Path,
    ) -> SessionResult<Option<WorkspaceRecord>> {
        let checkout =
            std::fs::canonicalize(checkout_path).unwrap_or_else(|_| checkout_path.to_path_buf());
        let exact = path_text(checkout_path);
        let found = self
            .run_blocking(move |conn| {
                let sql = format!("SELECT {SELECT_COLS} FROM workspaces WHERE checkout_path = ?1");
                conn.query_row(&sql, params![exact], row_to_workspace)
                    .optional()
            })
            .await?;
        if found.is_some() {
            return Ok(found);
        }
        // Persisted paths may predate canonicalization (e.g. /var aliases on
        // macOS); aliases must not bypass the checkout's single-owner rule.
        Ok(self.list_workspaces().await?.into_iter().find(|record| {
            std::fs::canonicalize(&record.checkout_path)
                .unwrap_or_else(|_| record.checkout_path.clone())
                == checkout
        }))
    }

    async fn get_workspace_by_session(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<WorkspaceRecord>> {
        let owner = session_id.to_string();
        self.run_blocking(move |conn| {
            let sql = select_by_owner_sql();
            conn.query_row(&sql, params![owner], row_to_workspace)
                .optional()
        })
        .await
    }

    async fn get_workspace_by_delegation(
        &self,
        delegation_id: &str,
    ) -> SessionResult<Option<WorkspaceRecord>> {
        let delegation_id = delegation_id.to_string();
        self.run_blocking(move |conn| {
            let sql =
                format!("SELECT {SELECT_COLS} FROM workspaces WHERE delegation_id = ?1 LIMIT 1");
            conn.query_row(&sql, params![delegation_id], row_to_workspace)
                .optional()
        })
        .await
    }

    async fn list_workspaces(&self) -> SessionResult<Vec<WorkspaceRecord>> {
        self.run_blocking(move |conn| {
            let sql =
                format!("SELECT {SELECT_COLS} FROM workspaces ORDER BY updated_at DESC, id DESC");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([], row_to_workspace)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    async fn list_workspaces_by_repository(
        &self,
        common_dir: &Path,
    ) -> SessionResult<Vec<WorkspaceRecord>> {
        let common = path_text(common_dir);
        self.run_blocking(move |conn| {
            let sql = format!(
                "SELECT {SELECT_COLS} FROM workspaces WHERE common_dir = ?1 \
                 ORDER BY updated_at DESC, id DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![common], row_to_workspace)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    async fn list_workspaces_by_batch(
        &self,
        batch_id: &str,
    ) -> SessionResult<Vec<WorkspaceRecord>> {
        let batch = batch_id.to_string();
        self.run_blocking(move |conn| {
            let sql = format!(
                "SELECT {SELECT_COLS} FROM workspaces WHERE delegation_batch_id = ?1 \
                 ORDER BY id ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![batch], row_to_workspace)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    async fn update_workspace(&self, record: WorkspaceRecord) -> SessionResult<()> {
        let workspace_id = record.id.to_string();
        let changed = self
            .run_blocking(move |conn| {
                let changed = conn.execute(
                    "UPDATE workspaces SET
                    common_dir = ?2, origin_path = ?3, project_name = ?4,
                    checkout_path = ?5, branch = ?6, relative_cwd = ?7,
                    execution = ?8, baseline_mode = ?9, source_head = ?10,
                    baseline_commit = ?11, lifecycle = ?12, owner_session_id = ?13,
                    parent_workspace_id = ?14, parent_session_id = ?15,
                    delegation_id = ?16, delegation_batch_id = ?17, last_error = ?18,
                    created_at = ?19, updated_at = ?20, requested_base_ref = ?21,
                    operation_id = ?22, provisioning_phase = ?23
                 WHERE public_id = ?1",
                    params![
                        record.id.as_str(),
                        path_text(&record.repository.common_dir),
                        path_text(&record.repository.origin_path),
                        record.repository.project_name,
                        path_text(&record.checkout_path),
                        record.branch.as_str(),
                        path_text(&record.relative_cwd),
                        record.execution.to_string(),
                        record
                            .baseline
                            .as_ref()
                            .map(|b| b.mode)
                            .unwrap_or(record.requested_baseline_mode)
                            .to_string(),
                        record.baseline.as_ref().map(|b| b.source_head.clone()),
                        record.baseline.as_ref().map(|b| b.baseline_commit.clone()),
                        record.lifecycle.to_string(),
                        record.ownership.owner_session_id,
                        record
                            .ownership
                            .parent_workspace_id
                            .as_ref()
                            .map(|id| id.to_string()),
                        record.ownership.parent_session_id,
                        record.ownership.delegation_id,
                        record.ownership.delegation_batch_id,
                        record.last_error,
                        format_dt(&record.created_at),
                        format_dt(&record.updated_at),
                        record.requested_base_ref,
                        record.operation_id,
                        record.provisioning_phase.map(|p| p.to_string()),
                    ],
                )?;
                Ok(changed)
            })
            .await?;
        if changed == 0 {
            return Err(SessionError::WorkspaceNotFound(workspace_id));
        }
        Ok(())
    }

    async fn transition_lifecycle(
        &self,
        id: &WorkspaceId,
        from: WorkspaceLifecycle,
        to: WorkspaceLifecycle,
    ) -> SessionResult<bool> {
        let id = id.as_str().to_string();
        let from = from.to_string();
        let to = to.to_string();
        self.run_blocking(move |conn| {
            // Clear the provisioning phase when provisioning completes so
            // `Some(phase)` always means "provisioning still in progress".
            let changed = conn.execute(
                "UPDATE workspaces SET lifecycle = ?3, updated_at = ?4, \
                 provisioning_phase = CASE \
                   WHEN ?2 = 'provisioning' AND ?3 = 'ready' THEN NULL \
                   ELSE provisioning_phase END \
                 WHERE public_id = ?1 AND lifecycle = ?2 \
                 AND (?3 != 'removing' OR owner_session_id IS NULL OR lifecycle = 'archived')",
                params![id, from, to, format_dt(&OffsetDateTime::now_utc())],
            )?;
            Ok(changed > 0)
        })
        .await
    }

    async fn attach_owner(
        &self,
        id: &WorkspaceId,
        owner_session_id: &str,
    ) -> SessionResult<WorkspaceRecord> {
        let id = id.as_str().to_string();
        let owner = owner_session_id.to_string();
        let id_for_err = id.clone();
        // CAS attach: only succeeds when unowned or already owned by the
        // same session (re-attaching an existing owner is idempotent). The
        // single conditional UPDATE is atomic across processes, so two
        // sessions can never both bind as writable owners.
        let updated: Option<WorkspaceRecord> = self
            .run_blocking(move |conn| {
                let changed = conn.execute(
                    "UPDATE workspaces SET owner_session_id = ?2, updated_at = ?3 \
                     WHERE public_id = ?1 AND lifecycle = 'ready' \
                     AND (owner_session_id IS NULL OR owner_session_id = ?2)",
                    params![id, owner, format_dt(&OffsetDateTime::now_utc())],
                )?;
                if changed == 0 {
                    return Ok(None);
                }
                let sql = select_by_id_sql();
                let record = conn.query_row(&sql, params![id], row_to_workspace)?;
                Ok(Some(record))
            })
            .await?;
        match updated {
            Some(record) => Ok(record),
            None => {
                // Distinguish missing workspace from a conflicting owner.
                let existing = self
                    .get_workspace(&WorkspaceId::from_raw(id_for_err.clone()))
                    .await?;
                match existing {
                    None => Err(SessionError::WorkspaceNotFound(id_for_err)),
                    Some(record) if record.lifecycle != WorkspaceLifecycle::Ready => {
                        Err(SessionError::InvalidOperation(format!(
                            "workspace {id_for_err} is {} and cannot acquire an owner",
                            record.lifecycle
                        )))
                    }
                    Some(record) => Err(SessionError::WorkspaceAlreadyOwned {
                        workspace: id_for_err,
                        owner: record.ownership.owner_session_id.unwrap_or_default(),
                    }),
                }
            }
        }
    }

    async fn detach_owner(&self, id: &WorkspaceId) -> SessionResult<WorkspaceRecord> {
        let id = id.as_str().to_string();
        let id_for_err = id.clone();
        let updated: Option<WorkspaceRecord> = self
            .run_blocking(move |conn| {
                let changed = conn.execute(
                    "UPDATE workspaces SET owner_session_id = NULL, updated_at = ?2 \
                     WHERE public_id = ?1",
                    params![id, format_dt(&OffsetDateTime::now_utc())],
                )?;
                if changed == 0 {
                    return Ok(None);
                }
                let sql = select_by_id_sql();
                let record = conn.query_row(&sql, params![id], row_to_workspace)?;
                Ok(Some(record))
            })
            .await?;
        updated.ok_or(SessionError::WorkspaceNotFound(id_for_err))
    }

    async fn bind_baseline(&self, id: &WorkspaceId, baseline: &Baseline) -> SessionResult<()> {
        let id = id.as_str().to_string();
        let id_for_err = id.clone();
        let conflict_id = id.clone();
        let mode = baseline.mode.to_string();
        let source_head = baseline.source_head.clone();
        let baseline_commit = baseline.baseline_commit.clone();
        let changed = self
            .run_blocking(move |conn| {
                // Bind once, immutably: an unbound row takes the capture, a
                // row already bound to the same commit is an idempotent
                // no-op, and a row bound to a different commit is refused
                // rather than overwritten.
                let changed = conn.execute(
                    "UPDATE workspaces SET baseline_mode = ?2, source_head = ?3, \
                     baseline_commit = ?4, updated_at = ?5 \
                     WHERE public_id = ?1 \
                       AND (baseline_commit IS NULL OR baseline_commit = ?4)",
                    params![
                        id,
                        mode,
                        source_head,
                        baseline_commit,
                        format_dt(&OffsetDateTime::now_utc())
                    ],
                )?;
                Ok(changed)
            })
            .await?;
        if changed == 0 {
            let existing = self
                .get_workspace(&WorkspaceId::from_raw(conflict_id.clone()))
                .await?;
            if existing.is_none() {
                return Err(SessionError::WorkspaceNotFound(id_for_err));
            }
            return Err(SessionError::InvalidOperation(format!(
                "workspace {conflict_id} already has a bound baseline; \
                 refusing to replace it"
            )));
        }
        Ok(())
    }

    async fn record_provisioning_phase(
        &self,
        id: &WorkspaceId,
        phase: ProvisioningPhase,
    ) -> SessionResult<bool> {
        let id = id.as_str().to_string();
        let phase_text = phase.to_string();
        self.run_blocking(move |conn| {
            // Forward-only CAS: only advance from an earlier (or legacy
            // NULL) phase while the workspace is still provisioning.
            let changed = conn.execute(
                "UPDATE workspaces SET provisioning_phase = ?2, updated_at = ?3 \
                 WHERE public_id = ?1 AND lifecycle = 'provisioning' \
                 AND (CASE IFNULL(provisioning_phase, 'intent_persisted') \
                      WHEN 'intent_persisted' THEN 0 \
                      WHEN 'baseline_bound' THEN 1 \
                      WHEN 'baseline_ref_published' THEN 2 \
                      WHEN 'branch_created' THEN 3 \
                      WHEN 'checkout_created' THEN 4 \
                      ELSE 99 END) \
                   < (CASE ?2 \
                      WHEN 'intent_persisted' THEN 0 \
                      WHEN 'baseline_bound' THEN 1 \
                      WHEN 'baseline_ref_published' THEN 2 \
                      WHEN 'branch_created' THEN 3 \
                      WHEN 'checkout_created' THEN 4 \
                      ELSE -1 END)",
                params![id, phase_text, format_dt(&OffsetDateTime::now_utc())],
            )?;
            Ok(changed > 0)
        })
        .await
    }

    async fn record_result(
        &self,
        id: &WorkspaceId,
        provenance: &ResultProvenance,
    ) -> SessionResult<()> {
        let id = id.as_str().to_string();
        let baseline_commit = provenance.baseline_commit.clone();
        let result_commit = provenance.result_commit.clone();
        let result_ref = provenance.result_ref.clone();
        let verification = provenance.verification.map(|v| v.to_string());
        let captured_at = format_dt(&provenance.captured_at);
        let captured_head = provenance.captured_head.clone();
        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO workspace_results (
                    workspace_public_id, baseline_commit, result_commit, result_ref,
                    verification, captured_at, captured_head
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    baseline_commit,
                    result_commit,
                    result_ref,
                    verification,
                    captured_at,
                    captured_head
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_results(&self, id: &WorkspaceId) -> SessionResult<Vec<ResultProvenance>> {
        let id = id.as_str().to_string();
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT baseline_commit, result_commit, result_ref, verification, captured_at, \
                 captured_head FROM workspace_results WHERE workspace_public_id = ?1 \
                 ORDER BY captured_at DESC, id DESC",
            )?;
            let rows = stmt
                .query_map(params![id], |row| {
                    let baseline_commit: String = row.get(0)?;
                    let result_commit: Option<String> = row.get(1)?;
                    let result_ref: Option<String> = row.get(2)?;
                    let verification: Option<String> = row.get(3)?;
                    let captured_at: String = row.get(4)?;
                    let captured_head: Option<String> = row.get(5)?;
                    Ok(ResultProvenance {
                        baseline_commit,
                        result_commit,
                        result_ref,
                        verification: verification
                            .map(|v| ResultVerificationStatus::from_str(&v))
                            .transpose()
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        captured_at: parse_dt(&captured_at)?,
                        captured_head,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema;
    use time::macros::datetime;

    /// Create an in-memory SQLite DB with schema initialized.
    fn setup_db() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
        schema::init_schema(&mut conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn repo() -> SqliteWorkspaceRepository {
        SqliteWorkspaceRepository::new(setup_db())
    }

    /// Insert a prerequisite session row so owner FKs are satisfied.
    async fn insert_session(repo: &SqliteWorkspaceRepository, public_id: &str) {
        let public_id = public_id.to_string();
        let conn = repo.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (public_id, name, created_at, updated_at) \
                 VALUES (?1, 'test', '2026-01-15T00:00:00Z', '2026-01-15T00:00:00Z')",
                params![public_id],
            )
            .unwrap();
        })
        .await
        .unwrap();
    }

    fn make_record(id: &str, checkout: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            id: WorkspaceId::from_raw(id),
            repository: RepositoryIdentity {
                common_dir: PathBuf::from("/repo/.git"),
                origin_path: PathBuf::from("/repo"),
                project_name: "repo".into(),
            },
            checkout_path: PathBuf::from(checkout),
            branch: ManagedBranch::for_workspace(&WorkspaceId::from_raw(id)),
            relative_cwd: PathBuf::from("crates/agent"),
            execution: ExecutionMode::Worktree,
            baseline: None,
            lifecycle: WorkspaceLifecycle::Provisioning,
            ownership: WorkspaceOwnership::default(),
            last_error: None,
            requested_baseline_mode: BaselineMode::Commit,
            requested_base_ref: None,
            operation_id: format!("op-{id}"),
            provisioning_phase: Some(ProvisioningPhase::IntentPersisted),
            created_at: datetime!(2026-01-15 12:00:00 UTC),
            updated_at: datetime!(2026-01-15 12:00:00 UTC),
        }
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() {
        let repo = repo();
        let record = make_record("ws-1", "/store/wt/ws-1");
        repo.insert_workspace(record.clone()).await.unwrap();

        let loaded = repo
            .get_workspace(&WorkspaceId::from_raw("ws-1"))
            .await
            .unwrap()
            .expect("workspace should exist");
        assert_eq!(loaded, record);
        assert_eq!(
            loaded.session_cwd(),
            PathBuf::from("/store/wt/ws-1/crates/agent")
        );

        assert!(
            repo.get_workspace(&WorkspaceId::from_raw("ws-nope"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_by_checkout_path() {
        let repo = repo();
        repo.insert_workspace(make_record("ws-1", "/store/wt/ws-1"))
            .await
            .unwrap();

        let loaded = repo
            .get_workspace_by_checkout(Path::new("/store/wt/ws-1"))
            .await
            .unwrap()
            .expect("should find by checkout path");
        assert_eq!(loaded.id, WorkspaceId::from_raw("ws-1"));

        assert!(
            repo.get_workspace_by_checkout(Path::new("/store/wt/other"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_by_owner_session_returns_the_binding() {
        let repo = repo();
        insert_session(&repo, "sess-1").await;
        let mut record = make_record("ws-1", "/store/wt/ws-1");
        record.ownership.owner_session_id = Some("sess-1".into());
        repo.insert_workspace(record).await.unwrap();
        // A second, unowned workspace must not match the session query.
        repo.insert_workspace(make_record("ws-2", "/store/wt/ws-2"))
            .await
            .unwrap();

        let loaded = repo
            .get_workspace_by_session("sess-1")
            .await
            .unwrap()
            .expect("session binding is resolvable");
        assert_eq!(loaded.id, WorkspaceId::from_raw("ws-1"));

        assert!(
            repo.get_workspace_by_session("sess-none")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn lists_group_by_repository_identity_and_batch() {
        let repo = repo();
        let mut other_repo = make_record("ws-b", "/store/wt/ws-b");
        other_repo.repository = RepositoryIdentity {
            common_dir: PathBuf::from("/other/.git"),
            origin_path: PathBuf::from("/other"),
            project_name: "other".into(),
        };

        let mut ws1 = make_record("ws-1", "/store/wt/ws-1");
        ws1.ownership.delegation_batch_id = Some("batch-1".into());
        let mut ws2 = make_record("ws-2", "/store/wt/ws-2");
        ws2.ownership.delegation_batch_id = Some("batch-1".into());

        repo.insert_workspace(ws1).await.unwrap();
        repo.insert_workspace(ws2).await.unwrap();
        repo.insert_workspace(other_repo).await.unwrap();

        let project = repo
            .list_workspaces_by_repository(Path::new("/repo/.git"))
            .await
            .unwrap();
        let mut ids: Vec<String> = project.iter().map(|w| w.id.to_string()).collect();
        ids.sort();
        assert_eq!(ids, vec!["ws-1", "ws-2"]);

        let batch = repo.list_workspaces_by_batch("batch-1").await.unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].id, WorkspaceId::from_raw("ws-1"), "ordered by id");

        let all = repo.list_workspaces().await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn checkout_paths_are_unique() {
        let repo = repo();
        repo.insert_workspace(make_record("ws-1", "/store/wt/shared"))
            .await
            .unwrap();

        let err = repo
            .insert_workspace(make_record("ws-2", "/store/wt/shared"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SessionError::WorkspaceCheckoutTaken(_)),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn attach_owner_enforces_single_writable_owner() {
        let repo = repo();
        insert_session(&repo, "sess-a").await;
        insert_session(&repo, "sess-b").await;
        repo.insert_workspace(make_record("ws-1", "/store/wt/ws-1"))
            .await
            .unwrap();

        assert!(
            repo.attach_owner(&WorkspaceId::from_raw("ws-1"), "sess-a")
                .await
                .is_err(),
            "provisioning cannot acquire an owner"
        );
        repo.transition_lifecycle(
            &WorkspaceId::from_raw("ws-1"),
            WorkspaceLifecycle::Provisioning,
            WorkspaceLifecycle::Ready,
        )
        .await
        .unwrap();
        let owned = repo
            .attach_owner(&WorkspaceId::from_raw("ws-1"), "sess-a")
            .await
            .unwrap();
        assert_eq!(owned.ownership.owner_session_id.as_deref(), Some("sess-a"));

        // A second writable session attachment is rejected.
        let err = repo
            .attach_owner(&WorkspaceId::from_raw("ws-1"), "sess-b")
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SessionError::WorkspaceAlreadyOwned { ref owner, .. } if owner == "sess-a"
            ),
            "unexpected error: {err:?}"
        );

        // The original owner still holds the binding.
        let reloaded = repo
            .get_workspace(&WorkspaceId::from_raw("ws-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.ownership.owner_session_id.as_deref(),
            Some("sess-a")
        );

        assert!(
            !repo
                .transition_lifecycle(
                    &WorkspaceId::from_raw("ws-1"),
                    WorkspaceLifecycle::Ready,
                    WorkspaceLifecycle::Removing
                )
                .await
                .unwrap(),
            "owned workspace cannot be claimed for removal"
        );
        // Detach clears the binding; a new session can then attach.
        let detached = repo
            .detach_owner(&WorkspaceId::from_raw("ws-1"))
            .await
            .unwrap();
        assert_eq!(detached.ownership.owner_session_id, None);
        repo.attach_owner(&WorkspaceId::from_raw("ws-1"), "sess-b")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn attach_owner_to_missing_workspace_errors() {
        let repo = repo();
        let err = repo
            .attach_owner(&WorkspaceId::from_raw("ghost"), "sess-a")
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::WorkspaceNotFound(_)));
    }

    #[tokio::test]
    async fn lifecycle_transition_is_cas() {
        let repo = repo();
        repo.insert_workspace(make_record("ws-1", "/store/wt/ws-1"))
            .await
            .unwrap();
        let id = WorkspaceId::from_raw("ws-1");

        assert!(
            !repo
                .transition_lifecycle(&id, WorkspaceLifecycle::Ready, WorkspaceLifecycle::Removing)
                .await
                .unwrap(),
            "must not jump provisioning → removing"
        );
        assert!(
            repo.transition_lifecycle(
                &id,
                WorkspaceLifecycle::Provisioning,
                WorkspaceLifecycle::Ready
            )
            .await
            .unwrap()
        );
        let loaded = repo.get_workspace(&id).await.unwrap().unwrap();
        assert_eq!(loaded.lifecycle, WorkspaceLifecycle::Ready);
    }

    #[tokio::test]
    async fn update_workspace_roundtrip_and_missing_errors() {
        let repo = repo();
        let mut record = make_record("ws-1", "/store/wt/ws-1");
        repo.insert_workspace(record.clone()).await.unwrap();

        record.lifecycle = WorkspaceLifecycle::Ready;
        record.baseline = Some(Baseline {
            mode: BaselineMode::CurrentWorkingTree,
            source_head: "aaa111".into(),
            baseline_commit: "bbb222".into(),
        });
        // A bound baseline carries its mode: keep the requested policy in
        // sync the way the allocator does.
        record.requested_baseline_mode = BaselineMode::CurrentWorkingTree;
        record.ownership.delegation_batch_id = Some("batch-9".into());
        repo.update_workspace(record.clone()).await.unwrap();

        let loaded = repo.get_workspace(&record.id).await.unwrap().unwrap();
        assert_eq!(loaded, record);

        let mut ghost = make_record("ghost", "/store/wt/ghost");
        ghost.lifecycle = WorkspaceLifecycle::Failed;
        assert!(repo.update_workspace(ghost).await.is_err());
    }

    #[tokio::test]
    async fn bind_baseline_persists_capture() {
        let repo = repo();
        repo.insert_workspace(make_record("ws-1", "/store/wt/ws-1"))
            .await
            .unwrap();
        let id = WorkspaceId::from_raw("ws-1");

        repo.bind_baseline(
            &id,
            &Baseline {
                mode: BaselineMode::CurrentWorkingTree,
                source_head: "aaa111".into(),
                baseline_commit: "bbb222".into(),
            },
        )
        .await
        .unwrap();

        let loaded = repo.get_workspace(&id).await.unwrap().unwrap();
        let baseline = loaded.baseline.expect("baseline bound");
        assert_eq!(baseline.mode, BaselineMode::CurrentWorkingTree);
        assert_eq!(baseline.source_head, "aaa111");
        assert_eq!(baseline.baseline_commit, "bbb222");

        assert!(
            repo.bind_baseline(
                &WorkspaceId::from_raw("ghost"),
                &Baseline {
                    mode: BaselineMode::Commit,
                    source_head: "x".into(),
                    baseline_commit: "x".into(),
                }
            )
            .await
            .is_err()
        );
    }

    /// A bound baseline is immutable: rebinding the same commit is an
    /// idempotent no-op; binding a different commit is refused with an
    /// explicit conflict instead of being overwritten.
    #[tokio::test]
    async fn bind_baseline_is_immutable_and_conflicts_are_refused() {
        let repo = repo();
        repo.insert_workspace(make_record("ws-1", "/store/wt/ws-1"))
            .await
            .unwrap();
        let id = WorkspaceId::from_raw("ws-1");
        let bound = Baseline {
            mode: BaselineMode::Commit,
            source_head: "aaa111".into(),
            baseline_commit: "bbb222".into(),
        };
        repo.bind_baseline(&id, &bound).await.unwrap();

        // Same commit: idempotent.
        repo.bind_baseline(&id, &bound).await.unwrap();
        assert_eq!(
            repo.get_workspace(&id).await.unwrap().unwrap().baseline,
            Some(bound.clone())
        );

        // Different commit: refused, original retained.
        let conflicting = Baseline {
            mode: BaselineMode::Commit,
            source_head: "changed".into(),
            baseline_commit: "ccc333".into(),
        };
        let err = repo.bind_baseline(&id, &conflicting).await.unwrap_err();
        assert!(
            err.to_string().contains("refusing to replace"),
            "unexpected error: {err}"
        );
        assert_eq!(
            repo.get_workspace(&id).await.unwrap().unwrap().baseline,
            Some(bound)
        );
    }

    /// Provisioning phases only ever move forward while provisioning:
    /// idempotent replay re-recording an earlier step is a no-op.
    #[tokio::test]
    async fn provisioning_phase_advances_forward_only() {
        let repo = repo();
        repo.insert_workspace(make_record("ws-1", "/store/wt/ws-1"))
            .await
            .unwrap();
        let id = WorkspaceId::from_raw("ws-1");

        assert!(
            repo.record_provisioning_phase(&id, ProvisioningPhase::BaselineBound)
                .await
                .unwrap()
        );
        // Equal or earlier phases are no-ops.
        assert!(
            !repo
                .record_provisioning_phase(&id, ProvisioningPhase::BaselineBound)
                .await
                .unwrap()
        );
        assert!(
            !repo
                .record_provisioning_phase(&id, ProvisioningPhase::IntentPersisted)
                .await
                .unwrap()
        );
        assert!(
            repo.record_provisioning_phase(&id, ProvisioningPhase::CheckoutCreated)
                .await
                .unwrap()
        );
        let loaded = repo.get_workspace(&id).await.unwrap().unwrap();
        assert_eq!(
            loaded.provisioning_phase,
            Some(ProvisioningPhase::CheckoutCreated)
        );

        // Unknown workspace ids report "not advanced" instead of erroring.
        assert!(
            !repo
                .record_provisioning_phase(
                    &WorkspaceId::from_raw("ghost"),
                    ProvisioningPhase::BranchCreated
                )
                .await
                .unwrap()
        );
    }

    /// The true first persisted intent (no baseline yet) round-trips the
    /// requested policy, operation id, and phase without fabricating a
    /// capture, and survives a full database reopen.
    #[tokio::test]
    async fn first_persisted_intent_round_trips_without_a_baseline() {
        let db = tempfile::TempDir::new().unwrap();
        let db_path = db.path().join("sessions.db");
        let record = {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
            schema::init_schema(&mut conn).unwrap();
            let store = SqliteWorkspaceRepository::new(Arc::new(Mutex::new(conn)));
            let mut record = make_record("ws-intent", "/store/wt/ws-intent");
            record.requested_baseline_mode = BaselineMode::CurrentWorkingTree;
            record.requested_base_ref = None;
            record.operation_id = "op-first-intent".into();
            record.provisioning_phase = Some(ProvisioningPhase::IntentPersisted);
            store.insert_workspace(record.clone()).await.unwrap();

            let loaded = store
                .get_workspace(&record.id)
                .await
                .unwrap()
                .expect("intent row");
            assert!(loaded.baseline.is_none(), "no fabricated baseline");
            assert_eq!(
                loaded.requested_baseline_mode,
                BaselineMode::CurrentWorkingTree
            );
            assert_eq!(loaded.requested_base_ref, None);
            assert_eq!(loaded.operation_id, "op-first-intent");
            assert_eq!(
                loaded.provisioning_phase,
                Some(ProvisioningPhase::IntentPersisted)
            );
            record
        };
        // Reopen from disk: the intent survives without a baseline.
        let reopened_conn = Connection::open(&db_path).unwrap();
        let reopened = SqliteWorkspaceRepository::new(Arc::new(Mutex::new(reopened_conn)));
        let loaded = reopened
            .get_workspace(&record.id)
            .await
            .unwrap()
            .expect("intent row after reopen");
        assert!(loaded.baseline.is_none());
        assert_eq!(
            loaded.provisioning_phase,
            Some(ProvisioningPhase::IntentPersisted)
        );
    }

    #[tokio::test]
    async fn results_are_recorded_and_listed_newest_first() {
        let repo = repo();
        repo.insert_workspace(make_record("ws-1", "/store/wt/ws-1"))
            .await
            .unwrap();
        let id = WorkspaceId::from_raw("ws-1");

        let older = ResultProvenance {
            baseline_commit: "base".into(),
            result_commit: Some("res-old".into()),
            result_ref: Some("refs/qmt/results/ws-1/old".into()),
            verification: Some(ResultVerificationStatus::Failed),
            captured_at: datetime!(2026-01-15 10:00:00 UTC),
            captured_head: Some("head-old".into()),
        };
        let newer = ResultProvenance {
            baseline_commit: "base".into(),
            result_commit: None,
            result_ref: None,
            verification: Some(ResultVerificationStatus::Passed),
            captured_at: datetime!(2026-01-15 12:00:00 UTC),
            captured_head: None,
        };
        repo.record_result(&id, &older).await.unwrap();
        repo.record_result(&id, &newer).await.unwrap();

        let results = repo.list_results(&id).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], newer, "newest first");
        assert_eq!(results[1], older);

        // Retained result refs are unique.
        let dup = ResultProvenance {
            baseline_commit: "base".into(),
            result_commit: Some("res-dup".into()),
            result_ref: Some("refs/qmt/results/ws-1/old".into()),
            verification: None,
            captured_at: datetime!(2026-01-15 13:00:00 UTC),
            captured_head: Some("head-dup".into()),
        };
        assert!(repo.record_result(&id, &dup).await.is_err());
    }
}
