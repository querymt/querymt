//! Explicit, previewable evidence acknowledgement.
//!
//! Acknowledging says "a person reviewed this text against these exact
//! dependency bytes". It is never a side effect: indexing, retrieval,
//! scaffolding and maps do not call into this module.
//!
//! # Two steps
//!
//! 1. [`RepositoryService::plan_acknowledge`] computes the acknowledgement for
//!    the records the caller **explicitly selected** in one document, and the
//!    exact evidence-file bytes that would result. Nothing is written.
//! 2. [`RepositoryService::apply_acknowledge`] re-derives the plan from the
//!    repository as it is *now* (note text, every dependency, and the evidence
//!    file itself) and refuses with [`MaintenanceError::Stale`] unless it equals
//!    the preview. It then writes only bytes it computed itself, so a hand-built
//!    or tampered plan cannot write anything else, and newer, unseen contents
//!    are never certified.
//!
//! # Selection
//!
//! A request names one document and either specific record keys or the whole
//! document. There is deliberately no "acknowledge all drift in the repository"
//! form. A dependency that is missing or cannot be resolved unambiguously
//! cannot be acknowledged.
//!
//! One file per document keeps acknowledgements from different branches in
//! different files, so they merge without textual conflict.

use super::evidence::{
    Dependency, DependencyCache, EvidenceEntry, EvidenceFile, EvidenceLoad, evidence_path,
    load_evidence, record_specs, text_hash,
};
use super::export::ExportAction;
use super::maintenance::{MaintenanceError, PlannedWrite, apply_writes, plan_write};
use super::query::normalize_relative_path;
use super::service::{RepositoryService, ServiceError};
use std::collections::{BTreeMap, BTreeSet};

/// Which records of the document to acknowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Exactly these record keys. Must not be empty.
    Records(Vec<String>),
    /// Every record of this one document, chosen explicitly.
    WholeDocument,
}

/// What to acknowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcknowledgeRequest {
    /// Repository-relative path of an indexed document.
    pub document: String,
    pub selection: Selection,
}

/// A previewed acknowledgement. The repository is untouched until it is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcknowledgePlan {
    pub document: String,
    pub action: ExportAction,
    /// The acknowledgements this plan records, by record key.
    pub entries: BTreeMap<String, EvidenceEntry>,
    /// The evidence file write, bound to the file's current bytes.
    pub write: PlannedWrite,
}

/// The outcome of applying a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcknowledgeReceipt {
    /// The evidence file.
    pub path: String,
    /// `false` when the file already held the planned content.
    pub written: bool,
    pub records: usize,
}

impl RepositoryService {
    /// Preview acknowledging the selected records of one document.
    ///
    /// # Errors
    ///
    /// Fails if the document is not indexed, a selected key does not exist, the
    /// selection is empty, a dependency is missing or unresolvable, or the
    /// document's existing evidence file is unusable.
    pub fn plan_acknowledge(
        &self,
        request: &AcknowledgeRequest,
    ) -> Result<AcknowledgePlan, MaintenanceError> {
        let document = normalize_relative_path(&request.document).map_err(|reason| {
            MaintenanceError::invalid(format!("document path `{}`: {reason}", request.document))
        })?;
        let entries = self.reviewed_entries(&document, &request.selection)?;

        let root = self.root().root();
        let mut file = match load_evidence(root, &document) {
            EvidenceLoad::Absent => EvidenceFile::new(&document),
            EvidenceLoad::Loaded(file) => file,
            EvidenceLoad::Invalid { reason, .. } => {
                return Err(MaintenanceError::invalid(format!(
                    "the existing evidence file for `{document}` is unusable ({reason}); \
                     repair or remove it first"
                )));
            }
        };
        file.entries.extend(
            entries
                .iter()
                .map(|(key, entry)| (key.clone(), entry.clone())),
        );

        let (write, action) = plan_write(root, &evidence_path(&document), file.to_text())?;
        Ok(AcknowledgePlan {
            document,
            action,
            entries,
            write,
        })
    }

    /// Apply a previewed acknowledgement if nothing it depends on has changed.
    ///
    /// # Errors
    ///
    /// [`MaintenanceError::Stale`] if the note, any dependency, or the evidence
    /// file differs from the preview; nothing is written then.
    pub fn apply_acknowledge(
        &self,
        plan: &AcknowledgePlan,
    ) -> Result<AcknowledgeReceipt, MaintenanceError> {
        let stale = || MaintenanceError::Stale {
            path: plan.document.clone(),
        };
        // Re-derive everything from the repository as it is now.
        let fresh = self
            .plan_acknowledge(&AcknowledgeRequest {
                document: plan.document.clone(),
                selection: Selection::Records(plan.entries.keys().cloned().collect()),
            })
            .map_err(|error| match error {
                // The document or a record vanished, or a dependency is gone.
                MaintenanceError::Invalid(_) | MaintenanceError::NotFound { .. } => stale(),
                other => other,
            })?;
        if fresh != *plan {
            return Err(stale());
        }
        let written = apply_writes(self.root().root(), std::slice::from_ref(&fresh.write))?;
        Ok(AcknowledgeReceipt {
            path: fresh.write.path,
            written: !written.is_empty(),
            records: fresh.entries.len(),
        })
    }

    /// The acknowledgement entries the current repository would produce for the
    /// selected records.
    fn reviewed_entries(
        &self,
        document: &str,
        selection: &Selection,
    ) -> Result<BTreeMap<String, EvidenceEntry>, MaintenanceError> {
        self.fresh_document(document).map_err(|error| match error {
            ServiceError::NotIndexed { path } => MaintenanceError::NotFound { path },
            other => other.into(),
        })?;
        let (_, entry) = self.store().corpus_entry(document)?;
        let entry = entry.ok_or_else(|| MaintenanceError::NotFound {
            path: document.to_owned(),
        })?;
        let specs = record_specs(&entry);

        let wanted: Option<BTreeSet<&str>> = match selection {
            Selection::WholeDocument => None,
            Selection::Records(keys) if keys.is_empty() => {
                return Err(MaintenanceError::invalid(
                    "no records selected; name the records to acknowledge or choose the whole \
                     document explicitly",
                ));
            }
            Selection::Records(keys) => Some(keys.iter().map(String::as_str).collect()),
        };
        if let Some(wanted) = &wanted
            && let Some(unknown) = wanted
                .iter()
                .find(|key| !specs.iter().any(|spec| spec.key == **key))
        {
            return Err(MaintenanceError::invalid(format!(
                "`{document}` has no record `{unknown}`"
            )));
        }

        let root = self.root().root();
        let mut cache = DependencyCache::default();
        let mut entries = BTreeMap::new();
        for spec in specs.iter().filter(|spec| {
            wanted
                .as_ref()
                .is_none_or(|keys| keys.contains(spec.key.as_str()))
        }) {
            let mut dependencies = BTreeMap::new();
            for selector in &spec.selectors {
                match cache.resolve(root, selector) {
                    Dependency::Hash(hash) => {
                        dependencies.insert(selector.clone(), hash.clone());
                    }
                    Dependency::Missing => {
                        return Err(MaintenanceError::invalid(format!(
                            "dependency `{selector}` of record `{}` does not exist; it cannot be \
                             acknowledged",
                            spec.key
                        )));
                    }
                    Dependency::Unresolved(reason) => {
                        return Err(MaintenanceError::invalid(format!(
                            "dependency `{selector}` of record `{}` {reason}; it cannot be \
                             acknowledged",
                            spec.key
                        )));
                    }
                }
            }
            entries.insert(
                spec.key.clone(),
                EvidenceEntry {
                    dependencies,
                    text: text_hash(&spec.text),
                },
            );
        }
        if entries.is_empty() {
            return Err(MaintenanceError::invalid(format!(
                "`{document}` has no records to acknowledge"
            )));
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::browse::tests::Fx;
    use crate::knowledge::repository::evidence::EVIDENCE_DIR;
    use crate::knowledge::repository::signals::EvidenceState;
    use std::fs;
    use std::path::Path;

    const NOTE: &str =
        "# Architecture\nThe scheduler dispatches tasks.\n## Storage\nRows live in SQLite.\n";

    fn fixture() -> Fx {
        let fx = Fx::new();
        fx.put("src/lib.rs", "pub fn run() {}\n");
        fx.put(
            "docs/arch.md",
            &format!("---\nknowledge_sources: src/lib.rs\n---\n{NOTE}"),
        );
        fx.service.refresh().unwrap();
        fx
    }

    fn request(document: &str, selection: Selection) -> AcknowledgeRequest {
        AcknowledgeRequest {
            document: document.to_owned(),
            selection,
        }
    }

    fn whole(fx: &Fx, document: &str) -> AcknowledgePlan {
        fx.service
            .plan_acknowledge(&request(document, Selection::WholeDocument))
            .unwrap()
    }

    fn state(fx: &Fx, document: &str) -> EvidenceState {
        fx.service.document_evidence(document).unwrap().state
    }

    fn files_under(dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
                if entry.path().is_dir() {
                    walk(&entry.path(), &format!("{name}/"), out);
                } else {
                    out.push(name);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, "", &mut out);
        out.sort();
        out
    }

    #[test]
    fn previewing_writes_nothing_and_applying_makes_the_evidence_unchanged() {
        let fx = fixture();
        let plan = whole(&fx, "docs/arch.md");
        assert_eq!(plan.action, ExportAction::Create);
        assert_eq!(plan.write.path, ".qmt/knowledge/evidence/docs/arch.md.json");
        assert!(
            !fx.repo.join(EVIDENCE_DIR).exists(),
            "preview must not write"
        );
        assert_eq!(state(&fx, "docs/arch.md"), EvidenceState::Unverified);

        let receipt = fx.service.apply_acknowledge(&plan).unwrap();
        assert!(receipt.written);
        assert_eq!(receipt.records, plan.entries.len());
        assert_eq!(state(&fx, "docs/arch.md"), EvidenceState::Unchanged);
        assert!(!fx.repo.join(".agents").exists());

        let text = fs::read_to_string(fx.repo.join(&plan.write.path)).unwrap();
        assert!(text.ends_with("}\n"));
        assert!(
            !text.contains(fx.repo.to_str().unwrap()),
            "no absolute paths"
        );
    }

    #[test]
    fn re_acknowledging_unchanged_content_is_idempotent() {
        let fx = fixture();
        fx.service
            .apply_acknowledge(&whole(&fx, "docs/arch.md"))
            .unwrap();
        let again = whole(&fx, "docs/arch.md");
        assert_eq!(again.action, ExportAction::Unchanged);
        assert!(!fx.service.apply_acknowledge(&again).unwrap().written);
    }

    #[test]
    fn only_the_selected_records_are_acknowledged() {
        let fx = fixture();
        let evidence = fx.service.document_evidence("docs/arch.md").unwrap();
        let first = evidence.records[0].key.clone();
        let plan = fx
            .service
            .plan_acknowledge(&request(
                "docs/arch.md",
                Selection::Records(vec![first.clone()]),
            ))
            .unwrap();
        assert_eq!(plan.entries.keys().collect::<Vec<_>>(), vec![&first]);
        fx.service.apply_acknowledge(&plan).unwrap();

        let after = fx.service.document_evidence("docs/arch.md").unwrap();
        assert_eq!(after.records[0].state, EvidenceState::Unchanged);
        assert!(
            after.records[1..]
                .iter()
                .all(|r| r.state == EvidenceState::Unverified)
        );
        assert_eq!(after.state, EvidenceState::Unverified, "worst record wins");
    }

    #[test]
    fn an_empty_or_unknown_selection_is_refused_never_widened() {
        let fx = fixture();
        for selection in [
            Selection::Records(vec![]),
            Selection::Records(vec!["section:[\"Nope\"]".into()]),
        ] {
            let error = fx
                .service
                .plan_acknowledge(&request("docs/arch.md", selection))
                .unwrap_err();
            assert_eq!(error.code(), "request_invalid");
        }
        assert!(!fx.repo.join(EVIDENCE_DIR).exists());
    }

    #[test]
    fn unsafe_unknown_or_unindexed_documents_are_refused() {
        let fx = fixture();
        for bad in ["../x.md", "/etc/passwd", ""] {
            let error = fx
                .service
                .plan_acknowledge(&request(bad, Selection::WholeDocument))
                .unwrap_err();
            assert_eq!(error.code(), "request_invalid", "{bad}");
        }
        let error = fx
            .service
            .plan_acknowledge(&request("docs/nope.md", Selection::WholeDocument))
            .unwrap_err();
        assert_eq!(error.code(), "source_not_indexed");
    }

    #[test]
    fn a_missing_or_ambiguous_dependency_cannot_be_acknowledged() {
        let fx = fixture();
        fs::remove_file(fx.repo.join("src/lib.rs")).unwrap();
        let error = fx
            .service
            .plan_acknowledge(&request("docs/arch.md", Selection::WholeDocument))
            .unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
        assert!(!fx.repo.join(EVIDENCE_DIR).exists());
    }

    #[test]
    fn a_note_edit_after_preview_invalidates_the_plan() {
        let fx = fixture();
        let plan = whole(&fx, "docs/arch.md");
        fx.put(
            "docs/arch.md",
            &format!("---\nknowledge_sources: src/lib.rs\n---\n{NOTE}Unreviewed addition.\n"),
        );
        let error = fx.service.apply_acknowledge(&plan).unwrap_err();
        assert_eq!(error.code(), "maintenance_stale");
        assert!(
            !fx.repo.join(EVIDENCE_DIR).exists(),
            "newer bytes are not certified"
        );
    }

    #[test]
    fn a_dependency_edit_after_preview_invalidates_the_plan() {
        let fx = fixture();
        let plan = whole(&fx, "docs/arch.md");
        fx.put("src/lib.rs", "pub fn run() { new_behavior(); }\n");
        let error = fx.service.apply_acknowledge(&plan).unwrap_err();
        assert_eq!(error.code(), "maintenance_stale");
        assert!(!fx.repo.join(EVIDENCE_DIR).exists());
    }

    #[test]
    fn whitespace_only_edits_after_preview_do_not_invalidate_the_plan() {
        let fx = fixture();
        let plan = whole(&fx, "docs/arch.md");
        fx.put("src/lib.rs", "pub fn run() {}   \r\n");
        assert!(fx.service.apply_acknowledge(&plan).is_ok());
    }

    #[test]
    fn a_concurrent_acknowledgement_invalidates_an_older_plan() {
        let fx = fixture();
        let stale = whole(&fx, "docs/arch.md");
        let newer = whole(&fx, "docs/arch.md");
        fx.service.apply_acknowledge(&newer).unwrap();
        // The evidence file now exists, so the old preimage ("absent") is gone.
        let error = fx.service.apply_acknowledge(&stale).unwrap_err();
        assert_eq!(error.code(), "maintenance_stale");
    }

    #[test]
    fn a_tampered_plan_cannot_write_anything_else() {
        let fx = fixture();
        let mut plan = whole(&fx, "docs/arch.md");
        plan.write.path = "docs/arch.md".into();
        plan.write.content = "overwritten\n".into();
        let before = fs::read_to_string(fx.repo.join("docs/arch.md")).unwrap();
        assert!(fx.service.apply_acknowledge(&plan).is_err());
        assert_eq!(
            fs::read_to_string(fx.repo.join("docs/arch.md")).unwrap(),
            before
        );
    }

    #[test]
    fn an_unusable_existing_evidence_file_is_never_overwritten_silently() {
        let fx = fixture();
        fx.put(".qmt/knowledge/evidence/docs/arch.md.json", "not json");
        let error = fx
            .service
            .plan_acknowledge(&request("docs/arch.md", Selection::WholeDocument))
            .unwrap_err();
        assert!(error.to_string().contains("unusable"), "{error}");
        assert_eq!(
            fs::read_to_string(fx.repo.join(".qmt/knowledge/evidence/docs/arch.md.json")).unwrap(),
            "not json"
        );
    }

    #[test]
    fn acknowledging_one_record_preserves_other_documents_records() {
        let fx = fixture();
        fx.service
            .apply_acknowledge(&whole(&fx, "docs/arch.md"))
            .unwrap();
        // Add a record, then acknowledge only it: earlier entries stay.
        fx.put(
            "docs/arch.md",
            &format!("---\nknowledge_sources: src/lib.rs\n---\n{NOTE}## Extra\nNew.\n"),
        );
        fx.service.refresh().unwrap();
        let extra = fx
            .service
            .document_evidence("docs/arch.md")
            .unwrap()
            .records
            .into_iter()
            .find(|record| record.key.contains("Extra"))
            .unwrap();
        let plan = fx
            .service
            .plan_acknowledge(&request(
                "docs/arch.md",
                Selection::Records(vec![extra.key]),
            ))
            .unwrap();
        fx.service.apply_acknowledge(&plan).unwrap();
        assert_eq!(state(&fx, "docs/arch.md"), EvidenceState::Unchanged);
    }

    #[test]
    fn different_documents_acknowledge_into_different_files() {
        let fx = fixture();
        fx.put("docs/other.md", "# Other\nText.\n");
        fx.service.refresh().unwrap();
        fx.service
            .apply_acknowledge(&whole(&fx, "docs/arch.md"))
            .unwrap();
        let before =
            fs::read_to_string(fx.repo.join(".qmt/knowledge/evidence/docs/arch.md.json")).unwrap();
        fx.service
            .apply_acknowledge(&whole(&fx, "docs/other.md"))
            .unwrap();

        assert_eq!(
            files_under(&fx.repo.join(EVIDENCE_DIR)),
            vec![
                "docs/arch.md.json".to_owned(),
                "docs/other.md.json".to_owned()
            ]
        );
        assert_eq!(
            fs::read_to_string(fx.repo.join(".qmt/knowledge/evidence/docs/arch.md.json")).unwrap(),
            before,
            "acknowledging another note never touches this file"
        );
    }

    #[test]
    fn crlf_checkouts_keep_acknowledgements_valid() {
        let fx = fixture();
        fx.service
            .apply_acknowledge(&whole(&fx, "docs/arch.md"))
            .unwrap();
        let crlf = format!("---\nknowledge_sources: src/lib.rs\n---\n{NOTE}").replace('\n', "\r\n");
        fx.put("docs/arch.md", &crlf);
        fx.put("src/lib.rs", "pub fn run() {}\r\n");
        assert_eq!(state(&fx, "docs/arch.md"), EvidenceState::Unchanged);
    }

    #[test]
    fn hand_edited_records_never_acknowledge_hidden_agents_paths() {
        let fx = fixture();
        fx.put(
            ".qmt/knowledge.toml",
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\", \".agents/memories/*.md\"]\n",
        );
        fx.put(
            ".agents/memories/m.md",
            "---\nid: m\ntitle: M\n---\nRemember.\n",
        );
        fx.service.refresh().unwrap();
        let plan = whole(&fx, ".agents/memories/m.md");
        // Evidence lives under `.qmt/`, never under `.agents/`.
        assert!(
            plan.write
                .path
                .starts_with(".qmt/knowledge/evidence/.agents/")
        );
        fx.service.apply_acknowledge(&plan).unwrap();
        assert_eq!(
            files_under(&fx.repo.join(".agents")),
            vec!["memories/m.md".to_owned()]
        );
    }
}
