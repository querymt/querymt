//! Shared plumbing for the explicit maintenance operations: `check`,
//! `scaffold` and `acknowledge`.
//!
//! Every operation that can change the repository follows one shape:
//!
//! 1. **Plan** (read-only): work out exactly what would be written and record a
//!    *preimage* for every target, the SHA-256 of its current bytes or its
//!    absence.
//! 2. **Apply** (explicit): re-validate every preimage, and only if all of them
//!    still hold write each file atomically.
//!
//! Writes never go under `.git/` or `.agents/`, never through a symlink, and
//! never create commits. Indexing, retrieval and maps never call into this
//! module's apply path.

use super::document::DiagnosticSeverity;
use super::export::{ExportAction, ExportError, confine_target, digest, read_target, write_atomic};
use super::service::ServiceError;
use super::store::{IndexDiagnostic, StoreError};
use serde_json::{Value, json};
use std::path::Path;
use thiserror::Error;

/// One diagnostic with a stable code and a repository-relative path.
///
/// Findings never contain absolute paths, so equal repositories produce equal
/// findings on any machine.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub path: Option<String>,
    pub code: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

impl Finding {
    pub fn new(
        path: Option<&str>,
        code: &str,
        severity: DiagnosticSeverity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.map(str::to_owned),
            code: code.to_owned(),
            severity,
            message: message.into(),
        }
    }

    /// Whether this finding makes a check fail. Informational findings do not.
    pub fn is_failing(&self) -> bool {
        self.severity >= DiagnosticSeverity::Warning
    }

    pub fn severity_name(&self) -> &'static str {
        match self.severity {
            DiagnosticSeverity::Info => "info",
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "severity": self.severity_name(),
            "path": self.path,
            "message": self.message,
        })
    }
}

impl From<&IndexDiagnostic> for Finding {
    fn from(diagnostic: &IndexDiagnostic) -> Self {
        Self {
            path: diagnostic.path.clone(),
            code: diagnostic.code.clone(),
            severity: diagnostic.severity,
            message: diagnostic.message.clone(),
        }
    }
}

/// Why a maintenance operation could not be planned or applied.
#[derive(Debug, Error)]
pub enum MaintenanceError {
    #[error(transparent)]
    Target(#[from] ExportError),
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("`{path}` is not an indexed repository document")]
    NotFound { path: String },
    #[error("`{path}` changed since the plan was made; plan again")]
    Stale { path: String },
}

impl From<StoreError> for MaintenanceError {
    fn from(error: StoreError) -> Self {
        Self::Service(ServiceError::unavailable(error))
    }
}

impl MaintenanceError {
    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Target(error) => error.code(),
            Self::Service(error) => error.code(),
            Self::Invalid(_) => "request_invalid",
            Self::NotFound { .. } => "source_not_indexed",
            Self::Stale { .. } => "maintenance_stale",
        }
    }

    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

/// A file write that is only valid while its target is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    /// Normalized repository-relative target.
    pub path: String,
    /// SHA-256 (hex) of the target's bytes when planned; `None` if absent.
    pub preimage: Option<String>,
    /// The complete file content after the write.
    pub content: String,
}

/// Work out the write that makes `path` hold `content`, without touching it.
pub(super) fn plan_write(
    root: &Path,
    path: &str,
    content: String,
) -> Result<(PlannedWrite, ExportAction), MaintenanceError> {
    let (relative, absolute) = confine_target(root, path)?;
    let preimage = read_target(&absolute, &relative)?.map(|bytes| digest(&bytes));
    let action = match preimage.as_deref() {
        None => ExportAction::Create,
        Some(old) if old == digest(content.as_bytes()) => ExportAction::Unchanged,
        Some(_) => ExportAction::Replace,
    };
    Ok((
        PlannedWrite {
            path: relative,
            preimage,
            content,
        },
        action,
    ))
}

/// Apply planned writes.
///
/// Every target is re-confined and every preimage re-checked **before** the
/// first byte is written, so a stale plan changes nothing. Each file is then
/// replaced atomically. (Writes to several files are not one transaction; a
/// failure partway leaves earlier files written, and re-planning shows what
/// remains.) Returns the paths actually written.
pub(super) fn apply_writes(
    root: &Path,
    writes: &[PlannedWrite],
) -> Result<Vec<String>, MaintenanceError> {
    let mut pending = Vec::with_capacity(writes.len());
    for write in writes {
        let (relative, absolute) = confine_target(root, &write.path)?;
        let current = read_target(&absolute, &relative)?;
        if current.as_deref().map(digest) != write.preimage {
            return Err(MaintenanceError::Stale { path: relative });
        }
        if write.preimage.as_deref() != Some(digest(write.content.as_bytes()).as_str()) {
            pending.push((relative, absolute, write));
        }
    }
    let mut written = Vec::with_capacity(pending.len());
    for (relative, absolute, write) in pending {
        write_atomic(root, &absolute, write.content.as_bytes())
            .map_err(|error| ExportError::io(&relative, &error))?;
        written.push(relative);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_stale_preimage_changes_nothing_even_for_other_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.md"), "one").unwrap();
        let (first, _) = plan_write(root, "a.md", "ONE".into()).unwrap();
        let (second, _) = plan_write(root, "b.md", "TWO".into()).unwrap();
        fs::write(root.join("a.md"), "edited").unwrap();

        let error = apply_writes(root, &[second, first]).unwrap_err();
        assert_eq!(error.code(), "maintenance_stale");
        assert!(
            !root.join("b.md").exists(),
            "no write before all preimages hold"
        );
        assert_eq!(fs::read_to_string(root.join("a.md")).unwrap(), "edited");
    }

    #[test]
    fn writes_never_target_agents_or_git() {
        let tmp = tempfile::tempdir().unwrap();
        for path in [".agents/memories/x.md", ".git/config"] {
            let error = plan_write(tmp.path(), path, "x".into()).unwrap_err();
            assert_eq!(error.code(), "export_target_forbidden", "{path}");
        }
    }

    #[test]
    fn an_unchanged_target_is_not_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.md"), "same").unwrap();
        let (write, action) = plan_write(tmp.path(), "a.md", "same".into()).unwrap();
        assert_eq!(action, ExportAction::Unchanged);
        assert!(apply_writes(tmp.path(), &[write]).unwrap().is_empty());
    }

    #[test]
    fn only_warnings_and_errors_fail() {
        let info = Finding::new(None, "x", DiagnosticSeverity::Info, "m");
        let warning = Finding::new(None, "x", DiagnosticSeverity::Warning, "m");
        assert!(!info.is_failing());
        assert!(warning.is_failing());
    }
}
