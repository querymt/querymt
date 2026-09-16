//! Source-aware, secret-free diagnostics for `.agents` Protocol resolution.

use super::layer::DotagentsSource;

/// Severity of a protocol diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DotagentsSeverity {
    /// Informational message; no action required.
    Info,
    /// The affected entry was skipped or adjusted; unrelated entries continue.
    Warning,
    /// The affected singleton or activation cannot be applied safely.
    Error,
}

impl DotagentsSeverity {
    /// Stable lowercase identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            DotagentsSeverity::Info => "info",
            DotagentsSeverity::Warning => "warning",
            DotagentsSeverity::Error => "error",
        }
    }

    /// Whether this severity is fatal under a strict policy.
    pub fn is_error(self) -> bool {
        matches!(self, DotagentsSeverity::Error)
    }
}

impl std::fmt::Display for DotagentsSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Machine-readable classification of a protocol diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DotagentsDiagnosticCode {
    /// A root or expected path does not exist.
    NotFound,
    /// A file could not be read.
    IoError,
    /// Frontmatter or Markdown could not be parsed.
    ParseError,
    /// A required field is missing or empty.
    MissingField,
    /// Two entries share a normalized ID within one layer.
    DuplicateId,
    /// A reference escapes its allowed root.
    PathEscape,
    /// A path is not a regular file where one is required.
    NotRegularFile,
    /// A referenced model preset is unknown.
    UnknownPreset,
    /// A transport or connection type is unsupported.
    UnsupportedTransport,
    /// Supported fields conflict (for example `command` and `url`).
    ConflictingFields,
    /// A required environment reference could not be resolved.
    MissingEnvironment,
    /// A referenced entry is missing.
    UnresolvedReference,
    /// A collision occurred between an explicit target and a protocol target.
    Collision,
    /// The affected configuration was applied without approval (unsafe).
    UnsafePolicy,
    /// A general warning that does not fit another code.
    Other,
}

impl DotagentsDiagnosticCode {
    /// Stable snake_case identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            DotagentsDiagnosticCode::NotFound => "not_found",
            DotagentsDiagnosticCode::IoError => "io_error",
            DotagentsDiagnosticCode::ParseError => "parse_error",
            DotagentsDiagnosticCode::MissingField => "missing_field",
            DotagentsDiagnosticCode::DuplicateId => "duplicate_id",
            DotagentsDiagnosticCode::PathEscape => "path_escape",
            DotagentsDiagnosticCode::NotRegularFile => "not_regular_file",
            DotagentsDiagnosticCode::UnknownPreset => "unknown_preset",
            DotagentsDiagnosticCode::UnsupportedTransport => "unsupported_transport",
            DotagentsDiagnosticCode::ConflictingFields => "conflicting_fields",
            DotagentsDiagnosticCode::MissingEnvironment => "missing_environment",
            DotagentsDiagnosticCode::UnresolvedReference => "unresolved_reference",
            DotagentsDiagnosticCode::Collision => "collision",
            DotagentsDiagnosticCode::UnsafePolicy => "unsafe_policy",
            DotagentsDiagnosticCode::Other => "other",
        }
    }
}

impl std::fmt::Display for DotagentsDiagnosticCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single protocol diagnostic.
///
/// Diagnostics must never contain resolved secret values. Messages are built
/// from field names, references, and source identities only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsDiagnostic {
    /// Severity of the diagnostic.
    pub severity: DotagentsSeverity,
    /// Machine-readable classification.
    pub code: DotagentsDiagnosticCode,
    /// Human-readable, secret-free message.
    pub message: String,
    /// The source that produced the diagnostic, when known.
    pub source: Option<DotagentsSource>,
}

impl DotagentsDiagnostic {
    /// Construct a diagnostic.
    pub fn new(
        severity: DotagentsSeverity,
        code: DotagentsDiagnosticCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            code,
            message: message.into(),
            source: None,
        }
    }

    /// Attach a source reference.
    pub fn with_source(mut self, source: DotagentsSource) -> Self {
        self.source = Some(source);
        self
    }

    /// Construct an informational diagnostic.
    pub fn info(code: DotagentsDiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(DotagentsSeverity::Info, code, message)
    }

    /// Construct a warning diagnostic.
    pub fn warning(code: DotagentsDiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(DotagentsSeverity::Warning, code, message)
    }

    /// Construct an error diagnostic.
    pub fn error(code: DotagentsDiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(DotagentsSeverity::Error, code, message)
    }

    /// Whether this diagnostic is an error.
    pub fn is_error(&self) -> bool {
        self.severity.is_error()
    }
}

impl std::fmt::Display for DotagentsDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}:{}] {}", self.severity, self.code, self.message)?;
        if let Some(source) = &self.source {
            write!(f, " ({})", source)?;
        }
        Ok(())
    }
}
