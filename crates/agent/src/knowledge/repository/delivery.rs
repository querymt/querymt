//! Delivery eligibility shared by both automatic stages.
//!
//! Selection ([`orient`](super::orientation), [`path_knowledge`](super::path_knowledge))
//! decides what is *relevant*; this module decides what may be *delivered
//! unasked* and how it must be labeled:
//!
//! * **Lifecycle.** `draft` and `deprecated` documents never become automatic
//!   cards. They are filtered before any card cap applies, so an ineligible
//!   document cannot occupy a slot. They remain fully available to explicit
//!   retrieval (`query`, `get`, `list`), which never consults this module.
//! * **Evidence.** Evidence is evaluated per cited record (a section or a
//!   File/Purpose row) against current sources, never inferred from the
//!   lifecycle label.
//!   * `unchanged` - delivered as authored.
//!   * `unverified` - delivered, labeled: no acknowledgement vouches for it.
//!     (Most repositories have no evidence files, so this must stay
//!     deliverable.)
//!   * `changed` - delivered, labeled: a supporting source differs from its
//!     last acknowledgement. This is not a claim that the note is false.
//!   * `missing` - a supporting source vanished or cannot be resolved. Only a
//!     cautionary pointer is delivered: the citation and the label, with the
//!     authored text withheld so it cannot be read as a current invariant.
//! * Any failure to evaluate evidence reads as `unverified`, the state that
//!   never overclaims.

use super::document::Lifecycle;
use super::evidence::DocumentEvidence;
use super::orientation::SummarySource;
use super::service::RepositoryService;
use super::signals::EvidenceState;
use std::collections::BTreeMap;

/// Whether a document with this `status` label may become an automatic card.
pub fn eligible(status: Option<&str>) -> bool {
    Lifecycle::from_status(status).allows_automatic_delivery()
}

/// The explicit label a card with this evidence state must carry, if any.
/// `unchanged` evidence needs none.
pub fn evidence_caution(state: EvidenceState) -> Option<&'static str> {
    match state {
        EvidenceState::Unchanged => None,
        EvidenceState::Unverified => {
            Some("unverified: no acknowledgement vouches for this against current sources")
        }
        EvidenceState::Changed => Some(
            "changed evidence: a supporting source differs from its last acknowledgement; verify against the current source",
        ),
        EvidenceState::Missing => Some(
            "missing evidence: a supporting source is gone or unresolvable; a pointer only, not a current invariant",
        ),
    }
}

/// Whether the authored text must be withheld, leaving a cautionary pointer.
pub fn withholds_text(state: EvidenceState) -> bool {
    state == EvidenceState::Missing
}

/// Apply the evidence presentation rule to a card's summary.
pub(super) fn apply_evidence(
    state: EvidenceState,
    summary: &mut String,
    source: &mut SummarySource,
) {
    if withholds_text(state) {
        summary.clear();
        *source = SummarySource::Withheld;
    }
}

/// Per-document evidence, evaluated at most once per selection.
#[derive(Default)]
pub(super) struct EvidenceLookup {
    documents: BTreeMap<String, Option<DocumentEvidence>>,
}

impl EvidenceLookup {
    /// The evidence state of record `key` of `doc_path`; `unverified` when the
    /// document or record cannot be evaluated.
    pub(super) fn state(
        &mut self,
        service: &RepositoryService,
        doc_path: &str,
        key: &str,
    ) -> EvidenceState {
        self.documents
            .entry(doc_path.to_owned())
            .or_insert_with(|| service.document_evidence(doc_path).ok())
            .as_ref()
            .and_then(|evidence| evidence.records.iter().find(|record| record.key == key))
            .map_or(EvidenceState::Unverified, |record| record.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_draft_and_deprecated_are_ineligible_whatever_their_case() {
        for ok in [
            None,
            Some("usable"),
            Some("stable"),
            Some("reviewed"),
            Some(""),
        ] {
            assert!(eligible(ok), "{ok:?}");
        }
        for no in [Some("draft"), Some("Deprecated"), Some(" DRAFT ")] {
            assert!(!eligible(no), "{no:?}");
        }
    }

    #[test]
    fn labels_follow_the_evidence_state() {
        assert_eq!(evidence_caution(EvidenceState::Unchanged), None);
        for state in [
            EvidenceState::Unverified,
            EvidenceState::Changed,
            EvidenceState::Missing,
        ] {
            assert!(evidence_caution(state).is_some(), "{state:?}");
        }
        assert!(
            evidence_caution(EvidenceState::Changed)
                .unwrap()
                .contains("changed")
        );
        assert!(withholds_text(EvidenceState::Missing));
        for state in [
            EvidenceState::Unchanged,
            EvidenceState::Unverified,
            EvidenceState::Changed,
        ] {
            assert!(!withholds_text(state));
        }
    }
}
