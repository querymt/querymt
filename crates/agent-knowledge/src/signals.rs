//! The three state signals reported with authored results.
//!
//! They answer different questions and are never derived from one another:
//!
//! * **index** — do the indexed bytes match the file on disk right now?
//! * **evidence** — does the source an authored statement relies on still match
//!   the last explicit acknowledgement? (Neither proves the statement true.)
//! * **lifecycle** — the author's own `status` label (`draft`, `deprecated`,
//!   anything else). It is a label, not a verdict: it can neither certify
//!   correctness nor clear evidence drift.
//!
//! Evidence acknowledgements arrive with the maintenance milestone. Until a
//! document has one, its evidence is honestly [`EvidenceState::Unverified`].

use std::fmt;

/// Whether indexed bytes match the current file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexState {
    /// Revalidated against the file before it was served.
    Current,
}

/// Acknowledged-evidence state of an authored document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EvidenceState {
    /// Nothing acknowledged, or the note changed since it was.
    #[default]
    Unverified,
    /// Note and dependencies match the acknowledgement.
    Unchanged,
    /// A dependency's bytes differ from the acknowledgement.
    Changed,
    /// A declared dependency no longer exists.
    Missing,
}

impl EvidenceState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Unchanged => "unchanged",
            Self::Changed => "changed",
            Self::Missing => "missing",
        }
    }
}

/// Lifecycle label shown when a document declares no `status`.
pub const DEFAULT_LIFECYCLE: &str = "usable";

/// Index, evidence and lifecycle, kept as separate fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSignals {
    pub index: IndexState,
    pub evidence: EvidenceState,
    /// The author's label verbatim; `None` means none was declared.
    pub lifecycle: Option<String>,
}

impl StateSignals {
    /// Signals for a document served from a just-validated generation.
    pub fn authored(status: Option<&str>) -> Self {
        Self {
            index: IndexState::Current,
            evidence: EvidenceState::default(),
            lifecycle: status.map(str::to_owned),
        }
    }

    /// Replace the evidence state. Lifecycle is left exactly as declared.
    #[must_use]
    pub fn with_evidence(mut self, evidence: EvidenceState) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn lifecycle_label(&self) -> &str {
        self.lifecycle.as_deref().unwrap_or(DEFAULT_LIFECYCLE)
    }
}

impl fmt::Display for StateSignals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let IndexState::Current = self.index;
        write!(
            f,
            "index: current · evidence: {} · lifecycle: {}",
            self.evidence.as_str(),
            self.lifecycle_label()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_undeclared_lifecycle_reads_as_usable_and_evidence_starts_unverified() {
        let signals = StateSignals::authored(None);
        assert_eq!(
            signals.to_string(),
            "index: current · evidence: unverified · lifecycle: usable"
        );
    }

    #[test]
    fn lifecycle_never_clears_or_alters_evidence_drift() {
        for status in [
            None,
            Some("draft"),
            Some("deprecated"),
            Some("verified"),
            Some(""),
        ] {
            for evidence in [
                EvidenceState::Unverified,
                EvidenceState::Unchanged,
                EvidenceState::Changed,
                EvidenceState::Missing,
            ] {
                let signals = StateSignals::authored(status).with_evidence(evidence);
                assert_eq!(signals.evidence, evidence, "status {status:?}");
                assert!(
                    signals
                        .to_string()
                        .contains(&format!("evidence: {}", evidence.as_str()))
                );
            }
        }
    }

    #[test]
    fn a_deprecated_document_with_unchanged_evidence_reports_both() {
        let line = StateSignals::authored(Some("deprecated"))
            .with_evidence(EvidenceState::Unchanged)
            .to_string();
        assert_eq!(
            line,
            "index: current · evidence: unchanged · lifecycle: deprecated"
        );
    }

    #[test]
    fn a_reassuring_status_does_not_improve_changed_evidence() {
        let line = StateSignals::authored(Some("verified"))
            .with_evidence(EvidenceState::Changed)
            .to_string();
        assert!(line.contains("evidence: changed"));
        assert!(line.contains("lifecycle: verified"));
    }
}
