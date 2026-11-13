//! Deterministic, read-only repository knowledge checks.
//!
//! [`RepositoryService::check`] reconciles the *local cache* (never the
//! repository) and then reports, with stable codes and repository-relative
//! paths:
//!
//! * malformed or conflicting inputs and incomplete coverage (index
//!   diagnostics, including file/byte budgets);
//! * broken or unsafe relative Markdown references;
//! * File/Purpose rows naming files that no longer exist (orphans);
//! * declared `knowledge_sources` / `knowledge_paths` that are invalid;
//! * evidence drift for acknowledged records, stale evidence entries, and
//!   orphaned evidence files;
//! * manifest `[orientation]` entrypoints that do not resolve.
//!
//! Checks are strictly non-mutating and offline: nothing is rewritten, no rows
//! are pruned, no link is followed over the network, and no rename is inferred
//! from similarity. Authored rows and references are preserved for review.
//!
//! # Strictness
//!
//! Curated corpora are valid. Missing optional listings, missing per-file rows,
//! unknown frontmatter keys and documents that were never acknowledged are not
//! findings. Per-file rows are required only for directories the caller
//! explicitly selects in [`CheckRequest::exhaustive`].
//!
//! A report *fails* when any finding is a warning or an error; informational
//! findings never fail it.

use super::document::DiagnosticSeverity::{Error, Info, Warning};
use super::evidence::Dependency;
use super::evidence::{
    DependencyCache, EvidenceLoad, evaluate_document, list_evidence_documents, load_evidence,
    parse_selector, record_specs, resolve_dependency,
};
use super::maintenance::{Finding, MaintenanceError};
use super::manifest::{MANIFEST_RELATIVE_PATH, ManifestOutcome, load_manifest};
use super::query::normalize_relative_path;
use super::reconcile::ReconcileStatus;
use super::references::{
    Reference, RejectReason, TargetStatus, codes as reference_codes, extract_links,
    resolve_reference, verify_local,
};
use super::service::RepositoryService;
use super::signals::EvidenceState;
use super::store::{CorpusEntry, SourceState, path_in_scope};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub mod codes {
    pub const REFERENCE_BROKEN: &str = "reference_broken";
    pub const FILE_TABLE_ORPHAN: &str = "file_table_orphan";
    pub const FILE_TABLE_MISSING_ROW: &str = "file_table_missing_row";
    pub const SOURCE_INVALID: &str = "knowledge_source_invalid";
    pub const SOURCE_MISSING: &str = "knowledge_source_missing";
    pub const SOURCE_UNRESOLVED: &str = "knowledge_source_unresolved";
    pub const PATH_INVALID: &str = "knowledge_path_invalid";
    pub const EVIDENCE_UNVERIFIED: &str = "evidence_unverified";
    pub const EVIDENCE_CHANGED: &str = "evidence_changed";
    pub const EVIDENCE_MISSING: &str = "evidence_missing";
    pub const EVIDENCE_ENTRY_ORPHANED: &str = "evidence_entry_orphaned";
    pub const EVIDENCE_FILE_ORPHANED: &str = "evidence_file_orphaned";
    pub const ENTRYPOINT_INVALID: &str = "entrypoint_invalid";
    pub const SCOPE_INVALID: &str = "exhaustive_scope_invalid";
}

/// What to check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckRequest {
    /// Repository-relative directories in which **every** selected code file
    /// must have a File/Purpose row. Empty means curated mode: no per-file
    /// rows are required anywhere.
    pub exhaustive: Vec<String>,
}

/// The outcome of a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// Local generation the corpus was read from. Not part of [`to_json`].
    ///
    /// [`to_json`]: CheckReport::to_json
    pub generation: u64,
    pub status: ReconcileStatus,
    /// Sorted, de-duplicated.
    pub findings: Vec<Finding>,
}

/// The repository-determined configuration state. Whether the local cache was
/// built, updated or unchanged is machine-local and deliberately not reported,
/// so equal repositories give byte-identical JSON.
fn status_name(status: ReconcileStatus) -> &'static str {
    match status {
        ReconcileStatus::NotConfigured => "not_configured",
        ReconcileStatus::ManifestInvalid => "manifest_invalid",
        _ => "configured",
    }
}

impl CheckReport {
    /// Whether any finding is a warning or an error.
    pub fn is_failing(&self) -> bool {
        self.findings.iter().any(Finding::is_failing)
    }

    /// Machine-readable report: stable codes and relative paths only, no
    /// generation numbers, timestamps or machine-local data, so equal
    /// repositories produce byte-identical output.
    pub fn to_json(&self) -> Value {
        let count = |severity| {
            self.findings
                .iter()
                .filter(|finding| finding.severity == severity)
                .count()
        };
        json!({
            "ok": !self.is_failing(),
            "status": status_name(self.status),
            "counts": { "error": count(Error), "warning": count(Warning), "info": count(Info) },
            "findings": self.findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
        })
    }
}

fn shown(text: &str) -> String {
    text.chars().take(120).collect()
}

fn reference_findings(root: &std::path::Path, entry: &CorpusEntry, out: &mut Vec<Finding>) {
    let path = entry.document.path.as_str();
    for section in entry.document.sections.iter().filter(|s| !s.from_metadata) {
        // A heading section's body starts on the line after its heading.
        let first_line = section.start_line + usize::from(section.level > 0);
        for link in extract_links(&section.body, first_line) {
            let (code, what) = match resolve_reference(path, &link.target) {
                Reference::Local { path: target, .. } => match verify_local(root, &target) {
                    TargetStatus::Missing => (codes::REFERENCE_BROKEN, "points to a missing path"),
                    TargetStatus::EscapesRoot => (
                        reference_codes::ESCAPES_ROOT,
                        "resolves outside the repository",
                    ),
                    _ => continue,
                },
                Reference::Rejected(RejectReason::EscapesRoot) => {
                    (reference_codes::ESCAPES_ROOT, "escapes the knowledge root")
                }
                Reference::Rejected(RejectReason::Unsafe) => {
                    (reference_codes::UNSAFE, "contains an unsafe path")
                }
                Reference::Rejected(RejectReason::Absolute) => {
                    (reference_codes::ABSOLUTE, "is a root-absolute path")
                }
                // External URLs are inert metadata; same-document links and
                // empty destinations are not file references.
                Reference::External { .. }
                | Reference::SameDocument { .. }
                | Reference::Rejected(RejectReason::NotAFile) => continue,
            };
            out.push(Finding::new(
                Some(path),
                code,
                Warning,
                format!("line {}: link `{}` {what}", link.line, shown(&link.target)),
            ));
        }
    }
}

fn row_findings(root: &std::path::Path, entry: &CorpusEntry, out: &mut Vec<Finding>) {
    for row in &entry.purposes {
        let (code, what) = match verify_local(root, &row.target_path) {
            TargetStatus::Missing => (
                codes::FILE_TABLE_ORPHAN,
                "names a path that no longer exists",
            ),
            TargetStatus::EscapesRoot => (
                reference_codes::ESCAPES_ROOT,
                "resolves outside the repository",
            ),
            _ => continue,
        };
        out.push(Finding::new(
            Some(&row.doc_path),
            code,
            Warning,
            format!(
                "line {}: row `{}` {what}",
                row.line,
                shown(&row.target_path)
            ),
        ));
    }
}

/// Whether `pattern` is a plain repository-relative glob.
fn valid_applicability(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with(['/', '~'])
        && !trimmed.contains(['\\', '\0'])
        && trimmed.split('/').all(|segment| segment != "..")
        && glob::Pattern::new(trimmed).is_ok()
}

fn declaration_findings(
    root: &std::path::Path,
    entry: &CorpusEntry,
    cache: &mut DependencyCache,
    out: &mut Vec<Finding>,
) {
    let path = entry.document.path.as_str();
    for selector in &entry.document.knowledge_sources {
        let (code, message) = if let Err(reason) = parse_selector(selector) {
            (
                codes::SOURCE_INVALID,
                format!("`{}`: {reason}", shown(selector)),
            )
        } else {
            match cache.resolve(root, selector) {
                Dependency::Hash(_) => continue,
                Dependency::Missing => (
                    codes::SOURCE_MISSING,
                    format!("`{}` does not exist", shown(selector)),
                ),
                Dependency::Unresolved(reason) => (
                    codes::SOURCE_UNRESOLVED,
                    format!("`{}` {reason}", shown(selector)),
                ),
            }
        };
        out.push(Finding::new(Some(path), code, Warning, message));
    }
    for pattern in &entry.document.knowledge_paths {
        if !valid_applicability(pattern) {
            out.push(Finding::new(
                Some(path),
                codes::PATH_INVALID,
                Warning,
                format!("`{}` is not a repository-relative glob", shown(pattern)),
            ));
        }
    }
}

fn evidence_findings(
    root: &std::path::Path,
    entry: &CorpusEntry,
    cache: &mut DependencyCache,
    out: &mut Vec<Finding>,
) {
    let evidence = evaluate_document(root, entry, cache);
    let path = evidence.path.as_str();
    if let Some((code, reason)) = &evidence.file_problem {
        out.push(Finding::new(
            Some(path),
            code,
            Error,
            format!("evidence file: {reason}"),
        ));
        return;
    }
    if !evidence.has_file {
        return; // never acknowledged: curated mode, not a finding
    }
    for record in evidence.records.iter().filter(|record| record.acknowledged) {
        let (code, what) = match record.state {
            EvidenceState::Unchanged => continue,
            EvidenceState::Unverified => (
                codes::EVIDENCE_UNVERIFIED,
                "changed since it was acknowledged",
            ),
            EvidenceState::Changed => (
                codes::EVIDENCE_CHANGED,
                "relies on source that changed since acknowledgement",
            ),
            EvidenceState::Missing => (
                codes::EVIDENCE_MISSING,
                "relies on source that is missing or unresolvable",
            ),
        };
        out.push(Finding::new(
            Some(path),
            code,
            Warning,
            format!("line {}: record {} {what}", record.line, shown(&record.key)),
        ));
    }
    // Acknowledged records the document no longer has (headings renamed or
    // removed) are stale entries, kept for review rather than deleted.
    if let EvidenceLoad::Loaded(file) = load_evidence(root, path) {
        let live: BTreeSet<String> = record_specs(entry)
            .into_iter()
            .map(|spec| spec.key)
            .collect();
        for key in file.entries.keys().filter(|key| !live.contains(*key)) {
            out.push(Finding::new(
                Some(path),
                codes::EVIDENCE_ENTRY_ORPHANED,
                Warning,
                format!(
                    "evidence entry {} matches no record in the document",
                    shown(key)
                ),
            ));
        }
    }
}

impl RepositoryService {
    /// Run every check. Reconciles the local cache first; writes nothing to the
    /// repository.
    ///
    /// # Errors
    ///
    /// Only when the index itself is unavailable or a requested exhaustive
    /// scope is unsafe. Problems *in the repository* are findings.
    pub fn check(&self, request: &CheckRequest) -> Result<CheckReport, MaintenanceError> {
        let scopes = request
            .exhaustive
            .iter()
            .map(|scope| {
                normalize_relative_path(scope).map_err(|reason| {
                    MaintenanceError::invalid(format!(
                        "{}: `{scope}`: {reason}",
                        codes::SCOPE_INVALID
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let report = self.refresh()?;
        let root = self.root().root();
        let (generation, corpus) = self.store().corpus()?;
        let (_, sources) = self.store().sources()?;
        let (_, diagnostics) = self.store().index_diagnostics()?;

        let mut findings: Vec<Finding> = diagnostics.iter().map(Finding::from).collect();
        let mut cache = DependencyCache::default();
        for entry in &corpus {
            reference_findings(root, entry, &mut findings);
            row_findings(root, entry, &mut findings);
            declaration_findings(root, entry, &mut cache, &mut findings);
            evidence_findings(root, entry, &mut cache, &mut findings);
        }

        let documents: BTreeSet<&str> = corpus.iter().map(|e| e.document.path.as_str()).collect();
        let (evidence_documents, listing) = list_evidence_documents(root);
        findings.extend(listing);
        findings.extend(
            evidence_documents
                .iter()
                .filter(|document| !documents.contains(document.as_str()))
                .map(|document| {
                    Finding::new(
                        Some(document),
                        codes::EVIDENCE_FILE_ORPHANED,
                        Warning,
                        "an evidence file exists for a document that is not indexed (moved, deleted or excluded)",
                    )
                }),
        );

        if let Ok(ManifestOutcome::Loaded(manifest)) = load_manifest(root) {
            for entrypoint in &manifest.entrypoints {
                if let Some(problem) =
                    self.entrypoint_problem(&entrypoint.target, entrypoint.section.as_deref())?
                {
                    let label = entrypoint.section.as_deref().map_or_else(
                        || entrypoint.target.clone(),
                        |s| format!("{}#{s}", entrypoint.target),
                    );
                    findings.push(Finding::new(
                        Some(MANIFEST_RELATIVE_PATH),
                        codes::ENTRYPOINT_INVALID,
                        Warning,
                        format!("orientation entrypoint `{}` {problem}", shown(&label)),
                    ));
                }
            }
        }

        if !scopes.is_empty() {
            let covered: BTreeSet<&str> = corpus
                .iter()
                .flat_map(|e| e.purposes.iter().map(|row| row.target_path.as_str()))
                .collect();
            findings.extend(
                sources
                    .iter()
                    .filter(|s| s.is_code && s.state == SourceState::Code)
                    .filter(|s| scopes.iter().any(|scope| path_in_scope(&s.path, Some(scope))))
                    .filter(|s| !covered.contains(s.path.as_str()))
                    .map(|s| {
                        Finding::new(
                            Some(&s.path),
                            codes::FILE_TABLE_MISSING_ROW,
                            Warning,
                            "no File/Purpose row names this file (exhaustive coverage was requested for its directory)",
                        )
                    }),
            );
        }

        findings.sort();
        findings.dedup();
        Ok(CheckReport {
            generation,
            status: report.status,
            findings,
        })
    }

    /// Why an orientation entrypoint does not resolve, if it does not.
    fn entrypoint_problem(
        &self,
        target: &str,
        section: Option<&str>,
    ) -> Result<Option<&'static str>, MaintenanceError> {
        let found = if target.starts_with("id:") || target.starts_with("path:") {
            self.store().document_by_id(target)?.1
        } else {
            match normalize_relative_path(target) {
                Ok(path) => self.store().document(&path)?.1,
                Err(_) => return Ok(Some("is not a safe repository-relative path")),
            }
        };
        let Some(document) = found else {
            return Ok(Some("does not name an indexed document"));
        };
        Ok(section
            .is_some_and(|selector| super::browse::select_section(&document, selector).is_err())
            .then_some("names a section that is missing or ambiguous"))
    }
}

/// Resolve one selector without a cache; exposed for diagnostics tooling.
pub fn dependency_exists(root: &std::path::Path, selector: &str) -> bool {
    matches!(resolve_dependency(root, selector), Dependency::Hash(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::acknowledge::{AcknowledgeRequest, Selection};
    use crate::knowledge::repository::browse::tests::Fx;
    use std::fs;
    use std::path::Path;

    fn check(fx: &Fx) -> CheckReport {
        fx.service.check(&CheckRequest::default()).unwrap()
    }

    fn codes_of(report: &CheckReport) -> Vec<&str> {
        report.findings.iter().map(|f| f.code.as_str()).collect()
    }

    fn acknowledge(fx: &Fx, document: &str) {
        let plan = fx
            .service
            .plan_acknowledge(&AcknowledgeRequest {
                document: document.into(),
                selection: Selection::WholeDocument,
            })
            .unwrap();
        fx.service.apply_acknowledge(&plan).unwrap();
    }

    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
                if entry.path().is_dir() {
                    walk(&entry.path(), &format!("{name}/"), out);
                } else {
                    out.push((name, fs::read(entry.path()).unwrap_or_default()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, "", &mut out);
        out.sort();
        out
    }

    #[test]
    fn a_curated_corpus_without_listings_rows_or_evidence_passes() {
        let fx = Fx::new();
        fx.put("src/a.rs", "pub fn a() {}\n");
        fx.put(
            "docs/arch.md",
            "---\nowner: platform\nsomething_new: 3\n---\n# Arch\nSee [site](https://example.com/x) and [self](#arch).\n",
        );
        let report = check(&fx);
        assert!(!report.is_failing(), "{:?}", report.findings);
        assert!(report.findings.iter().all(|f| !f.is_failing()));
    }

    #[test]
    fn broken_references_are_reported_with_relative_paths_and_left_in_place() {
        let fx = Fx::new();
        fx.put("docs/exists.md", "# E\n");
        let text = "# Arch\nOk [e](exists.md), bad [b](gone.md), up [u](../../x.md), dir [d](.).\n";
        fx.put("docs/arch.md", text);
        let report = check(&fx);
        let broken: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == codes::REFERENCE_BROKEN)
            .collect();
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].path.as_deref(), Some("docs/arch.md"));
        assert!(broken[0].message.contains("gone.md") && broken[0].message.contains("line 2"));
        assert!(codes_of(&report).contains(&"reference_escapes_root"));
        assert!(report.is_failing());
        assert_eq!(
            fs::read_to_string(fx.repo.join("docs/arch.md")).unwrap(),
            text
        );
    }

    #[test]
    fn orphan_file_table_rows_are_reported_not_pruned() {
        let fx = Fx::new();
        fx.put("src/queue.rs", "pub struct Queue;\n");
        let text = "| File | Purpose |\n|---|---|\n| `../src/queue.rs` | Queue. |\n| `../src/old.rs` | Gone. |\n";
        fx.put("docs/files.md", text);
        let report = check(&fx);
        let orphans: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == codes::FILE_TABLE_ORPHAN)
            .collect();
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].message.contains("src/old.rs"));
        assert_eq!(
            fs::read_to_string(fx.repo.join("docs/files.md")).unwrap(),
            text
        );
    }

    #[test]
    fn missing_rows_are_findings_only_in_a_selected_exhaustive_scope() {
        let fx = Fx::new();
        fx.put("src/sched/a.rs", "pub fn a() {}\n");
        fx.put("src/sched/b.rs", "pub fn b() {}\n");
        fx.put("src/other/o.rs", "pub fn o() {}\n");
        fx.put(
            "docs/files.md",
            "| File | Purpose |\n|---|---|\n| `../src/sched/a.rs` | A. |\n",
        );
        assert!(!codes_of(&check(&fx)).contains(&codes::FILE_TABLE_MISSING_ROW));

        let report = fx
            .service
            .check(&CheckRequest {
                exhaustive: vec!["src/sched".into()],
            })
            .unwrap();
        let missing: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == codes::FILE_TABLE_MISSING_ROW)
            .filter_map(|f| f.path.as_deref())
            .collect();
        assert_eq!(
            missing,
            ["src/sched/b.rs"],
            "other directories are not required"
        );
        assert!(
            fx.service
                .check(&CheckRequest {
                    exhaustive: vec!["../x".into()]
                })
                .is_err()
        );
    }

    #[test]
    fn a_malformed_sibling_fails_the_check_but_not_its_neighbours() {
        let fx = Fx::new();
        fx.put("docs/bad.md", "---\ntitle: Oops\n# never closed\n");
        fx.put("docs/good.md", "# Good\nFine.\n");
        let report = check(&fx);
        assert!(report.is_failing());
        let bad: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.severity == Error)
            .collect();
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].path.as_deref(), Some("docs/bad.md"));
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.path.as_deref() != Some("docs/good.md"))
        );
    }

    #[test]
    fn conflicting_explicit_ids_are_reported() {
        let fx = Fx::new();
        fx.put("docs/a.md", "---\nid: same\n---\n# A\n");
        fx.put("docs/b.md", "---\nid: same\n---\n# B\n");
        let report = check(&fx);
        assert!(codes_of(&report).contains(&"document_id_duplicate"));
        assert!(report.is_failing());
    }

    #[test]
    fn invalid_declarations_are_findings() {
        let fx = Fx::new();
        fx.put("src/ok.rs", "pub fn ok() {}\n");
        fx.put(
            "docs/arch.md",
            "---\nknowledge_sources: src/ok.rs, src/gone.rs, ../escape.rs\nknowledge_paths: src/**, /abs/**, [bad\n---\n# Arch\nText.\n",
        );
        let report = check(&fx);
        let codes = codes_of(&report);
        assert!(codes.contains(&super::codes::SOURCE_MISSING));
        assert!(codes.contains(&super::codes::SOURCE_INVALID));
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == super::codes::PATH_INVALID)
                .count(),
            2
        );
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == super::codes::SOURCE_MISSING)
                .count(),
            1
        );
    }

    #[test]
    fn evidence_drift_is_reported_only_for_acknowledged_records() {
        let fx = Fx::new();
        fx.put("src/lib.rs", "pub fn run() {}\n");
        fx.put(
            "docs/arch.md",
            "---\nknowledge_sources: src/lib.rs\n---\n# Arch\nText.\n",
        );
        fx.put("docs/plain.md", "# Plain\nNever acknowledged.\n");
        acknowledge_after_refresh(&fx);
        assert!(!check(&fx).is_failing());

        fx.put("src/lib.rs", "pub fn run() { changed(); }\n");
        let report = check(&fx);
        let drift: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == codes::EVIDENCE_CHANGED)
            .collect();
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].path.as_deref(), Some("docs/arch.md"));
        assert!(report.is_failing());
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.path.as_deref() != Some("docs/plain.md"))
        );

        fs::remove_file(fx.repo.join("src/lib.rs")).unwrap();
        assert!(codes_of(&check(&fx)).contains(&codes::EVIDENCE_MISSING));

        fx.put("src/lib.rs", "pub fn run() {}\n");
        fx.put(
            "docs/arch.md",
            "---\nknowledge_sources: src/lib.rs\n---\n# Arch\nRewritten.\n",
        );
        assert!(codes_of(&check(&fx)).contains(&codes::EVIDENCE_UNVERIFIED));
    }

    fn acknowledge_after_refresh(fx: &Fx) {
        fx.service.refresh().unwrap();
        acknowledge(fx, "docs/arch.md");
    }

    #[test]
    fn moved_documents_leave_a_reported_orphan_evidence_file() {
        let fx = Fx::new();
        fx.put("docs/arch.md", "# Arch\nText.\n");
        fx.service.refresh().unwrap();
        acknowledge(&fx, "docs/arch.md");
        fs::rename(fx.repo.join("docs/arch.md"), fx.repo.join("docs/moved.md")).unwrap();

        let report = check(&fx);
        let orphan: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == codes::EVIDENCE_FILE_ORPHANED)
            .collect();
        assert_eq!(orphan.len(), 1);
        assert_eq!(orphan[0].path.as_deref(), Some("docs/arch.md"));
        assert!(
            fx.repo
                .join(".qmt/knowledge/evidence/docs/arch.md.json")
                .exists(),
            "orphans are reported, never deleted"
        );
    }

    #[test]
    fn renamed_headings_leave_reported_stale_evidence_entries() {
        let fx = Fx::new();
        fx.put("docs/arch.md", "# Arch\n## Old\nText.\n");
        fx.service.refresh().unwrap();
        acknowledge(&fx, "docs/arch.md");
        fx.put("docs/arch.md", "# Arch\n## New\nText.\n");
        assert!(codes_of(&check(&fx)).contains(&codes::EVIDENCE_ENTRY_ORPHANED));
    }

    #[test]
    fn unusable_evidence_files_are_errors() {
        let fx = Fx::new();
        fx.put("docs/arch.md", "# Arch\nText.\n");
        fx.put(".qmt/knowledge/evidence/docs/arch.md.json", "{ nope");
        let report = check(&fx);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code == "evidence_file_invalid" && f.severity == Error)
        );
    }

    #[test]
    fn orientation_entrypoints_must_resolve() {
        let fx = Fx::new();
        fx.put("docs/arch.md", "# Arch\n## Repository map\nText.\n");
        let manifest = |entries: &str| {
            fx.put(
                ".qmt/knowledge.toml",
                &format!("version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[orientation]\nentrypoints = [{entries}]\n"),
            );
        };
        manifest("\"docs/arch.md#Arch > Repository map\", \"path:docs/arch.md\"");
        assert!(!check(&fx).is_failing(), "{:?}", check(&fx).findings);

        manifest("\"docs/missing.md\", \"docs/arch.md#No such section\", \"../x.md\", \"id:nope\"");
        let report = check(&fx);
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|f| f.code == codes::ENTRYPOINT_INVALID)
                .count(),
            4
        );
        assert!(
            report
                .findings
                .iter()
                .filter(|f| f.code == codes::ENTRYPOINT_INVALID)
                .all(|f| f.path.as_deref() == Some(".qmt/knowledge.toml"))
        );
    }

    #[test]
    fn json_is_stable_relative_and_machine_independent() {
        let fx = Fx::new();
        fx.put("docs/arch.md", "# Arch\n[b](gone.md)\n");
        let first = check(&fx).to_json();
        let second = check(&fx).to_json();
        assert_eq!(first, second);
        let text = first.to_string();
        assert!(
            !text.contains(fx.repo.to_str().unwrap()),
            "no absolute paths"
        );
        assert!(!text.contains("generation"));
        assert_eq!(first["status"], "configured");
        assert_eq!(first["ok"], false);
        assert_eq!(first["findings"][0]["code"], "reference_broken");
        assert_eq!(first["findings"][0]["path"], "docs/arch.md");
        assert_eq!(first["counts"]["warning"], 1);
    }

    #[test]
    fn an_unconfigured_or_invalid_manifest_is_reported() {
        let fx = Fx::new();
        fs::remove_file(fx.repo.join(".qmt/knowledge.toml")).unwrap();
        let report = check(&fx);
        assert_eq!(report.to_json()["status"], "not_configured");
        assert!(!report.is_failing(), "not configured is informational");

        fx.put(".qmt/knowledge.toml", "version = 99\n");
        let report = check(&fx);
        assert!(report.is_failing());
        assert_eq!(report.to_json()["status"], "manifest_invalid");
    }

    #[test]
    fn checking_never_writes_to_the_repository() {
        let fx = Fx::new();
        fx.put("src/a.rs", "pub fn a() {}\n");
        fx.put(
            "docs/arch.md",
            "---\nknowledge_sources: src/a.rs, src/gone.rs\n---\n# Arch\n[b](gone.md)\n| File | Purpose |\n|---|---|\n| `../src/old.rs` | x |\n",
        );
        fx.put(".qmt/knowledge/evidence/docs/other.md.json", "{}");
        let before = snapshot(&fx.repo);
        let _ = check(&fx);
        let _ = check(&fx);
        assert_eq!(snapshot(&fx.repo), before);
        assert!(!fx.repo.join(".agents").exists());
    }
}
