//! Prompt orientation: candidate subsystem/source pointers for a user task
//! that names no file.
//!
//! [`RepositoryService::orient`] turns a prompt into a few cited *cards* using
//! only local, deterministic retrieval: the [relevance gate](super::gate) first
//! and, when nothing passes it and the caller says this is a first task (or the
//! first request after compaction), the manifest's reviewed
//! `[orientation] entrypoints`. It calls no model and no network.
//!
//! * A card is one document (its best-ranked section), cited by relative path
//!   and line range. Its text is the author's `description` when present,
//!   otherwise a bounded verbatim excerpt of the section, otherwise its title.
//!   Nothing is generated.
//! * Cards name candidate entry points, never a mandatory edit target.
//!   Entrypoint cards are labeled as general orientation, not as a confident
//!   match.
//! * The fallback reads only configured entrypoints. Absent configuration means
//!   no fallback, an invalid entrypoint is reported as a diagnostic and
//!   skipped, and synthesized directory catalogs are never used.
//!
//! Whether a card may actually be delivered (lifecycle, evidence, budgets,
//! duplicates) is decided by the delivery stages, not here; the document's
//! lifecycle label is carried on the card for them.

use super::browse::select_section;
use super::check::codes;
use super::delivery::{EvidenceLookup, apply_evidence, eligible};
use super::document::DiagnosticSeverity::Warning;
use super::evidence::section_record_key;
use super::gate::{GATE_VERSION, GateReason};
use super::maintenance::Finding;
use super::manifest::{MANIFEST_RELATIVE_PATH, ManifestOutcome, load_manifest};
use super::query::{excerpt, normalize_relative_path};
use super::service::{RepositoryService, ServiceError};
use super::signals::EvidenceState;
use super::store::StoredSection;
use std::collections::BTreeSet;

/// What to orient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrientationRequest {
    /// The accepted user prompt.
    pub prompt: String,
    /// The first task of the session, or the first request after compaction:
    /// configured entrypoints may stand in when nothing passes the gate.
    pub allow_entrypoint_fallback: bool,
    /// Most cards to return.
    pub max_cards: usize,
}

impl OrientationRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            allow_entrypoint_fallback: false,
            max_cards: 2,
        }
    }
}

/// Where a card came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardOrigin {
    /// The section passed the relevance gate.
    Gated(GateReason),
    /// A reviewed manifest entrypoint: general orientation, not a match.
    Entrypoint,
}

/// Which text a card shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummarySource {
    /// The document's authored `description`.
    Description,
    /// A bounded verbatim excerpt of the section.
    Excerpt,
    /// Neither existed; the section or document title.
    Title,
    /// The author's text in a File/Purpose table cell.
    PurposeRow,
    /// Withheld: the card's evidence is missing, so it is a cautionary pointer
    /// with no authored text.
    Withheld,
}

/// The text a card shows for `section`: the author's description, else a
/// bounded verbatim excerpt, else a title. Never generated.
pub(super) fn summarize(
    description: Option<&str>,
    section: &StoredSection,
    doc_title: &str,
) -> (String, SummarySource) {
    let described = description.map(str::trim).filter(|d| !d.is_empty());
    if let Some(description) = described {
        (description.to_string(), SummarySource::Description)
    } else if !section.body.trim().is_empty() {
        (excerpt(&section.body), SummarySource::Excerpt)
    } else {
        (
            section
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| doc_title.to_string()),
            SummarySource::Title,
        )
    }
}

/// A cited orientation pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrientationCard {
    pub doc_path: String,
    pub doc_id: String,
    pub doc_title: String,
    /// Portable key of the cited section (fetchable with `repo_knowledge_get`).
    pub section_key: String,
    /// Heading breadcrumb (`A > B`) of the cited section, if it has one.
    pub selector: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub summary: String,
    pub summary_source: SummarySource,
    /// The author's lifecycle label, verbatim.
    pub status: Option<String>,
    /// Evidence state of the cited section against current sources. When
    /// missing, `summary` is empty (see [`SummarySource::Withheld`]).
    pub evidence: EvidenceState,
    pub origin: CardOrigin,
}

/// The selector's answer for one prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct OrientationOutcome {
    pub gate_version: u32,
    pub generation: u64,
    pub cards: Vec<OrientationCard>,
    /// Documents passed over because their lifecycle (`draft`, `deprecated`)
    /// bars automatic delivery, among those considered before the card cap.
    pub ineligible: usize,
    /// Problems with configured entrypoints; never fatal.
    pub diagnostics: Vec<Finding>,
}

struct Source<'a> {
    path: &'a str,
    id: &'a str,
    title: &'a str,
    status: Option<&'a str>,
    description: Option<&'a str>,
}

fn card(
    source: Source<'_>,
    section: &StoredSection,
    origin: CardOrigin,
    evidence: EvidenceState,
) -> OrientationCard {
    let (mut summary, mut summary_source) = summarize(source.description, section, source.title);
    apply_evidence(evidence, &mut summary, &mut summary_source);
    OrientationCard {
        doc_path: source.path.to_string(),
        doc_id: source.id.to_string(),
        doc_title: source.title.to_string(),
        section_key: section.key.clone(),
        selector: (!section.breadcrumb.is_empty()).then(|| section.breadcrumb.join(" > ")),
        start_line: section.start_line,
        end_line: section.end_line,
        summary,
        summary_source,
        status: source.status.map(str::to_string),
        evidence,
        origin,
    }
}

impl RepositoryService {
    /// Select orientation cards for `request`. See the module documentation.
    pub fn orient(&self, request: &OrientationRequest) -> Result<OrientationOutcome, ServiceError> {
        let gated = self.gate(&request.prompt)?;
        let mut cards = Vec::new();
        let mut seen = BTreeSet::new();
        let mut passed_over = BTreeSet::new();
        let mut evidence = EvidenceLookup::default();
        for found in &gated.sections {
            if cards.len() >= request.max_cards {
                break;
            }
            // Lifecycle is checked before the cap and before the one-card-per-
            // document rule, so an ineligible document never takes a slot.
            if !eligible(found.doc_status.as_deref()) {
                passed_over.insert(found.doc_path.as_str());
                continue;
            }
            // One card per document: its best-ranked section.
            if !seen.insert(found.doc_path.as_str()) {
                continue;
            }
            let state = evidence.state(self, &found.doc_path, &section_record_key(&found.section));
            cards.push(card(
                Source {
                    path: &found.doc_path,
                    id: &found.doc_id,
                    title: &found.doc_title,
                    status: found.doc_status.as_deref(),
                    description: found.doc_description.as_deref(),
                },
                &found.section,
                CardOrigin::Gated(found.reason),
                state,
            ));
        }
        let mut ineligible = passed_over.len();

        // The fallback stands in when no *deliverable* card exists: sections
        // that passed the gate but belong to draft/deprecated documents give
        // the agent nothing, so the reviewed entrypoints still apply.
        let mut diagnostics = Vec::new();
        if cards.is_empty() && request.allow_entrypoint_fallback {
            ineligible += self.entrypoint_cards(request.max_cards, &mut cards, &mut diagnostics)?;
        }
        Ok(OrientationOutcome {
            gate_version: GATE_VERSION,
            generation: gated.generation,
            cards,
            ineligible,
            diagnostics,
        })
    }

    /// Append entrypoint cards; returns how many entrypoints were skipped for
    /// lifecycle.
    fn entrypoint_cards(
        &self,
        limit: usize,
        cards: &mut Vec<OrientationCard>,
        diagnostics: &mut Vec<Finding>,
    ) -> Result<usize, ServiceError> {
        let mut ineligible = 0;
        let mut evidence = EvidenceLookup::default();
        let manifest = match load_manifest(self.root().root()) {
            Ok(ManifestOutcome::Loaded(manifest)) => manifest,
            Ok(ManifestOutcome::NotConfigured) => return Ok(0),
            Err(error) => {
                diagnostics.push(Finding::new(
                    Some(MANIFEST_RELATIVE_PATH),
                    error.code(),
                    Warning,
                    error.to_string(),
                ));
                return Ok(0);
            }
        };
        let mut seen = BTreeSet::new();
        for entry in &manifest.entrypoints {
            if cards.len() >= limit {
                break;
            }
            let label = entry.section.as_deref().map_or_else(
                || entry.target.clone(),
                |section| format!("{}#{section}", entry.target),
            );
            let invalid = |reason: &str| {
                Finding::new(
                    Some(MANIFEST_RELATIVE_PATH),
                    codes::ENTRYPOINT_INVALID,
                    Warning,
                    format!("orientation entrypoint `{label}` {reason}"),
                )
            };

            let path = if entry.target.starts_with("id:") || entry.target.starts_with("path:") {
                match self
                    .store()
                    .document_by_id(&entry.target)
                    .map_err(ServiceError::unavailable)?
                    .1
                {
                    Some(document) => document.path,
                    None => {
                        diagnostics.push(invalid("does not name an indexed document"));
                        continue;
                    }
                }
            } else {
                match normalize_relative_path(&entry.target) {
                    Ok(path) => path,
                    Err(_) => {
                        diagnostics.push(invalid("is not a safe repository-relative path"));
                        continue;
                    }
                }
            };
            // Revalidated against the file, like any other emitted source.
            let document = match self.fresh_document(&path) {
                Ok(current) => current.document,
                Err(ServiceError::NotIndexed { .. }) => {
                    diagnostics.push(invalid("does not name an indexed document"));
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !eligible(document.status.as_deref()) {
                ineligible += 1;
                diagnostics.push(invalid(
                    "names a draft or deprecated document, which is not delivered automatically",
                ));
                continue;
            }
            let section = match entry.section.as_deref() {
                Some(selector) => match select_section(&document, selector) {
                    Ok(section) => section,
                    Err(_) => {
                        diagnostics.push(invalid("names a section that is missing or ambiguous"));
                        continue;
                    }
                },
                None => {
                    match document
                        .sections
                        .iter()
                        .find(|section| !section.body.trim().is_empty())
                        .or_else(|| document.sections.first())
                    {
                        Some(section) => section,
                        None => {
                            diagnostics.push(invalid("names a document with no content"));
                            continue;
                        }
                    }
                }
            };
            if !seen.insert((document.path.clone(), section.key.clone())) {
                continue;
            }
            let state = evidence.state(self, &document.path, &section_record_key(section));
            cards.push(card(
                Source {
                    path: &document.path,
                    id: &document.id,
                    title: &document.title,
                    status: document.status.as_deref(),
                    description: document.description.as_deref(),
                },
                section,
                CardOrigin::Entrypoint,
                state,
            ));
        }
        Ok(ineligible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::browse::tests::Fx;

    const ARCH: &str = "# Architecture\n## Scheduling\nThe scheduler dispatches queued tasks to workers using a priority queue.\n## Storage\nRows live in SQLite.\n";

    fn manifest(entrypoints: &[&str]) -> String {
        let list = entrypoints
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[orientation]\nentrypoints = [{list}]\n"
        )
    }

    fn orient(fx: &Fx, prompt: &str, fallback: bool) -> OrientationOutcome {
        fx.service
            .orient(&OrientationRequest {
                prompt: prompt.to_string(),
                allow_entrypoint_fallback: fallback,
                max_cards: 2,
            })
            .unwrap()
    }

    #[test]
    fn a_path_free_task_gets_cited_cards_reusing_authored_text() {
        let fx = Fx::new();
        fx.put(
            "docs/scheduler.md",
            "---\ndescription: How queued work reaches workers.\n---\n# Scheduler\nThe scheduler dispatches queued tasks to workers using a priority queue.\n",
        );
        fx.put("docs/arch.md", ARCH);
        let outcome = orient(
            &fx,
            "the scheduler dispatches queued tasks to workers",
            false,
        );
        assert_eq!(outcome.gate_version, 1);
        assert!(outcome.diagnostics.is_empty());

        let authored = outcome
            .cards
            .iter()
            .find(|c| c.doc_path == "docs/scheduler.md")
            .expect("described document is a candidate");
        assert_eq!(authored.summary, "How queued work reaches workers.");
        assert_eq!(authored.summary_source, SummarySource::Description);
        assert!(matches!(authored.origin, CardOrigin::Gated(_)));
        assert!(authored.start_line >= 1 && authored.end_line >= authored.start_line);

        let plain = outcome
            .cards
            .iter()
            .find(|c| c.doc_path == "docs/arch.md")
            .expect("plain document is a candidate");
        assert_eq!(plain.summary_source, SummarySource::Excerpt);
        assert!(plain.summary.contains("priority queue"));
        assert!(!plain.summary.contains("SQLite"), "only the cited section");
        assert_eq!(plain.selector.as_deref(), Some("Architecture > Scheduling"));
        assert!(plain.section_key.starts_with(&plain.doc_id));
    }

    #[test]
    fn draft_and_deprecated_documents_are_never_cards_and_explicit_retrieval_still_works() {
        let fx = Fx::new();
        let body = "# Queue notes\nscheduler dispatch workers queue\n";
        fx.put(
            "docs/a_draft.md",
            &format!("---\nstatus: draft\n---\n{body}"),
        );
        fx.put(
            "docs/b_old.md",
            &format!("---\nstatus: deprecated\n---\n{body}"),
        );
        fx.put("docs/c.md", body);
        fx.put("docs/d.md", body);
        let outcome = orient(&fx, "scheduler dispatch workers queue notes", false);
        let docs: Vec<&str> = outcome.cards.iter().map(|c| c.doc_path.as_str()).collect();
        assert_eq!(docs, ["docs/c.md", "docs/d.md"], "ineligible took no slot");
        assert_eq!(outcome.ineligible, 2);

        let draft = fx.service.fresh_document("docs/a_draft.md").unwrap();
        assert_eq!(draft.document.status.as_deref(), Some("draft"));
    }

    #[test]
    fn a_draft_only_match_falls_back_to_entrypoints_and_a_draft_entrypoint_is_skipped() {
        let fx = Fx::new();
        fx.put(
            "docs/wip.md",
            "---\nstatus: draft\n---\n# Queue notes\nscheduler dispatch workers queue\n",
        );
        fx.put(
            "docs/map.md",
            "---\ndescription: Reviewed map.\n---\n# Map\nIntro.\n",
        );
        fx.put(
            ".qmt/knowledge.toml",
            &manifest(&["docs/wip.md", "docs/map.md"]),
        );
        let outcome = orient(&fx, "scheduler dispatch workers queue notes", true);
        let docs: Vec<&str> = outcome.cards.iter().map(|c| c.doc_path.as_str()).collect();
        assert_eq!(docs, ["docs/map.md"]);
        assert_eq!(outcome.cards[0].origin, CardOrigin::Entrypoint);
        assert_eq!(
            outcome.ineligible, 2,
            "gated draft section and draft entrypoint"
        );
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("docs/wip.md") && d.message.contains("draft"))
        );
    }

    #[test]
    fn evidence_labels_apply_and_missing_evidence_withholds_the_text() {
        use crate::knowledge::repository::acknowledge::{AcknowledgeRequest, Selection};
        let fx = Fx::new();
        fx.put("src/dep.rs", "pub fn dep() {}\n");
        fx.put(
            "docs/dep.md",
            "---\nknowledge_sources: src/dep.rs\n---\n# Dep\nThe scheduler dispatches queued tasks to workers.\n",
        );
        let prompt = "scheduler dispatches queued tasks workers";
        let card = |fx: &Fx| orient(fx, prompt, false).cards.remove(0);

        assert_eq!(card(&fx).evidence, EvidenceState::Unverified);
        let plan = fx
            .service
            .plan_acknowledge(&AcknowledgeRequest {
                document: "docs/dep.md".into(),
                selection: Selection::WholeDocument,
            })
            .unwrap();
        fx.service.apply_acknowledge(&plan).unwrap();
        assert_eq!(card(&fx).evidence, EvidenceState::Unchanged);

        fx.put("src/dep.rs", "pub fn dep() { changed(); }\n");
        let changed = card(&fx);
        assert_eq!(changed.evidence, EvidenceState::Changed);
        assert!(changed.summary.contains("scheduler"));

        std::fs::remove_file(fx.repo.join("src/dep.rs")).unwrap();
        let missing = card(&fx);
        assert_eq!(missing.evidence, EvidenceState::Missing);
        assert_eq!(missing.summary, "");
        assert_eq!(missing.summary_source, SummarySource::Withheld);
        assert_eq!(missing.doc_path, "docs/dep.md");
    }

    #[test]
    fn one_card_per_document_and_the_limit_is_honored() {
        let fx = Fx::new();
        fx.put(
            "docs/a.md",
            "# Queue notes\nscheduler dispatch workers queue\n## Queue detail\nscheduler dispatch workers queue\n",
        );
        fx.put(
            "docs/b.md",
            "# Queue notes\nscheduler dispatch workers queue\n",
        );
        fx.put(
            "docs/c.md",
            "# Queue notes\nscheduler dispatch workers queue\n",
        );
        let prompt = "scheduler dispatch workers queue notes";
        let outcome = orient(&fx, prompt, false);
        let docs: Vec<&str> = outcome.cards.iter().map(|c| c.doc_path.as_str()).collect();
        assert_eq!(
            docs,
            ["docs/a.md", "docs/b.md"],
            "stable order, capped at 2"
        );
        let unlimited = fx
            .service
            .orient(&OrientationRequest {
                max_cards: 10,
                ..OrientationRequest::new(prompt)
            })
            .unwrap();
        let docs: Vec<&str> = unlimited
            .cards
            .iter()
            .map(|c| c.doc_path.as_str())
            .collect();
        assert_eq!(docs, ["docs/a.md", "docs/b.md", "docs/c.md"]);
        let none = fx
            .service
            .orient(&OrientationRequest {
                max_cards: 0,
                ..OrientationRequest::new(prompt)
            })
            .unwrap();
        assert!(none.cards.is_empty());
    }

    #[test]
    fn no_match_and_no_entrypoints_means_no_card_even_when_fallback_is_allowed() {
        let fx = Fx::new();
        // Documents exist (a derived directory catalog could be synthesized
        // from them) but nothing is configured: nothing is injected.
        fx.put("docs/arch.md", ARCH);
        fx.put("docs/other.md", "# Other\nUnrelated.\n");
        for prompt in ["please fix the thing", "как работает планировщик", ""]
        {
            let outcome = orient(&fx, prompt, true);
            assert!(outcome.cards.is_empty(), "{prompt:?}");
            assert!(outcome.diagnostics.is_empty());
        }
    }

    #[test]
    fn entrypoints_back_a_first_task_only_when_nothing_passes_the_gate() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        fx.put(
            "docs/map.md",
            "---\nid: repo-map\ndescription: Reviewed repository map.\n---\n# Overview\nIntro.\n## Repository map\nruntime lives in src/runtime.\n",
        );
        fx.put(
            ".qmt/knowledge.toml",
            &manifest(&["docs/map.md#Overview > Repository map", "id:repo-map"]),
        );

        // Not a first task: no fallback.
        assert!(orient(&fx, "please fix the thing", false).cards.is_empty());

        let outcome = orient(&fx, "please fix the thing", true);
        let first = &outcome.cards[0];
        assert_eq!(first.origin, CardOrigin::Entrypoint);
        assert_eq!(first.doc_path, "docs/map.md");
        assert_eq!(first.selector.as_deref(), Some("Overview > Repository map"));
        assert_eq!(first.summary, "Reviewed repository map.");
        // The id form resolves to the same document and a different (first) section.
        assert_eq!(outcome.cards.len(), 2);
        assert_eq!(outcome.cards[1].doc_path, "docs/map.md");
        assert_ne!(outcome.cards[1].section_key, first.section_key);
        assert!(outcome.diagnostics.is_empty());

        // A gated match wins: entrypoints are not added alongside it.
        let gated = orient(
            &fx,
            "the scheduler dispatches queued tasks to workers",
            true,
        );
        assert!(
            gated
                .cards
                .iter()
                .all(|c| matches!(c.origin, CardOrigin::Gated(_)))
        );
        assert!(!gated.cards.is_empty());
    }

    #[test]
    fn invalid_entrypoints_are_diagnosed_and_skipped_never_dumped() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        fx.put("docs/good.md", "# Good\n## Map\nStart here.\n");
        fx.put(
            ".qmt/knowledge.toml",
            &manifest(&[
                "docs/missing.md",
                "../escape.md",
                "id:nobody",
                "docs/arch.md#No such heading",
                "docs/good.md#Good > Map",
            ]),
        );
        let outcome = orient(&fx, "please fix the thing", true);
        let cited: Vec<&str> = outcome.cards.iter().map(|c| c.doc_path.as_str()).collect();
        assert_eq!(cited, ["docs/good.md"], "only the valid entrypoint is used");
        assert_eq!(outcome.diagnostics.len(), 4);
        assert!(
            outcome
                .diagnostics
                .iter()
                .all(|f| f.code == codes::ENTRYPOINT_INVALID)
        );

        // Entrypoints that all fail yield no cards rather than a repository dump.
        fx.put(".qmt/knowledge.toml", &manifest(&["docs/missing.md"]));
        let outcome = orient(&fx, "please fix the thing", true);
        assert!(outcome.cards.is_empty());
        assert_eq!(outcome.diagnostics.len(), 1);
    }

    #[test]
    fn cards_reflect_the_current_document_after_an_edit() {
        let fx = Fx::new();
        fx.put("docs/map.md", "# Map\nOld pointer.\n");
        fx.put(".qmt/knowledge.toml", &manifest(&["docs/map.md"]));
        assert!(
            orient(&fx, "please fix the thing", true).cards[0]
                .summary
                .contains("Old pointer")
        );
        fx.put("docs/map.md", "# Map\nNew pointer.\n");
        assert!(
            orient(&fx, "please fix the thing", true).cards[0]
                .summary
                .contains("New pointer")
        );
        std::fs::remove_file(fx.repo.join("docs/map.md")).unwrap();
        let gone = orient(&fx, "please fix the thing", true);
        assert!(gone.cards.is_empty());
        assert_eq!(gone.diagnostics.len(), 1);
    }
}
