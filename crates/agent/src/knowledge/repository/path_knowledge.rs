//! Knowledge-for-path: the authored knowledge directly associated with one
//! indexed source file.
//!
//! This is the selector behind post-`index` enrichment. It is deterministic,
//! local, and exact:
//!
//! * [`RepositoryService::index_target`] resolves the file an `index` call
//!   actually read, from the call's *effective* arguments (`path`, optional
//!   `root`, session cwd), with the same root/path semantics as the tool, and
//!   returns its canonical repository-relative path. A file outside the
//!   authorized root, a directory, or a file that does not exist yields `None`.
//! * [`RepositoryService::path_knowledge`] then selects cards for that path
//!   from three sources only, each reported with distinct provenance
//!   ([`PathCardKind`]):
//!   1. File/Purpose rows naming the file itself, or the nearest enclosing
//!      directory row of each document;
//!   2. documents whose `knowledge_sources` name the file (or a symbol in it);
//!   3. documents whose `knowledge_paths` glob matches the file.
//! * There is no fuzzy basename matching and no scanning of document text for
//!   incidental path mentions: a note about `a/mod.rs` never attaches to
//!   `b/mod.rs`.
//! * Card text is the author's: a purpose cell, a document `description`, or a
//!   bounded verbatim excerpt. Signatures and other outline content are never
//!   repeated, and nothing is generated.
//! * Every document consulted is revalidated against its file before its text
//!   is used, so a card always reflects the current source. Its citation
//!   (relative path and line range) comes from that current text.
//!
//! Whether a card may be delivered automatically (lifecycle, evidence, budgets,
//! duplicates) is decided by the delivery stage; the lifecycle label is carried
//! on the card for it.

use super::delivery::{EvidenceLookup, apply_evidence, eligible};
use super::evidence::{row_record_key, section_record_key};
use super::orientation::{SummarySource, summarize};
use super::purpose::PurposeRow;
use super::query::{excerpt, normalize_relative_path};
use super::service::{RepositoryService, ServiceError};
use super::signals::EvidenceState;
use super::store::{AssociationKind, SourceState, StoredDocument};
use std::collections::BTreeMap;
use std::path::{Component, Path};

/// Cards returned when the caller does not say otherwise.
pub const DEFAULT_MAX_PATH_CARDS: usize = 2;

/// What to select knowledge for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathKnowledgeRequest {
    /// Canonical repository-relative path of the indexed file, as returned by
    /// [`RepositoryService::index_target`].
    pub file: String,
    /// Most cards to return.
    pub max_cards: usize,
}

impl PathKnowledgeRequest {
    pub fn new(file: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            max_cards: DEFAULT_MAX_PATH_CARDS,
        }
    }
}

/// Why a card applies to the file. Provenance is never merged: a document that
/// is associated in several ways produces one card per way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathCardKind {
    /// A File/Purpose row names this exact file.
    FilePurpose,
    /// A File/Purpose row names a directory containing the file.
    DirectoryPurpose,
    /// A document associated through its frontmatter.
    Note {
        /// Its `knowledge_sources` name the file.
        declared_source: bool,
        /// A `knowledge_paths` glob matches the file.
        applies: bool,
    },
}

impl PathCardKind {
    /// Most specific association first.
    fn rank(self) -> u8 {
        match self {
            Self::FilePurpose => 0,
            Self::DirectoryPurpose => 1,
            Self::Note {
                declared_source: true,
                ..
            } => 2,
            Self::Note { .. } => 3,
        }
    }
}

/// A cited, attributed piece of authored knowledge about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCard {
    /// The indexed file this card is about.
    pub file: String,
    pub doc_path: String,
    pub doc_id: String,
    pub doc_title: String,
    /// Portable ID of the cited record (a purpose row or a section), fetchable
    /// with `repo_knowledge_get`.
    pub retrieval_id: String,
    /// Heading breadcrumb (`A > B`) of the cited section; `None` for a row.
    pub selector: Option<String>,
    /// The path a File/Purpose row names, for purpose cards.
    pub target: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub summary: String,
    pub summary_source: SummarySource,
    /// The author's lifecycle label, verbatim.
    pub status: Option<String>,
    /// Evidence state of the cited record against current sources. When
    /// missing, `summary` is empty (see [`SummarySource::Withheld`]): the card
    /// is a cautionary pointer only.
    pub evidence: EvidenceState,
    pub kind: PathCardKind,
}

/// The selector's answer for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathKnowledge {
    pub file: String,
    pub generation: u64,
    /// The file is an indexed source of the authorized selected corpus. When
    /// `false` there are never any cards.
    pub in_corpus: bool,
    /// Cards, most specific association first.
    pub cards: Vec<PathCard>,
    /// Eligible cards that did not fit `max_cards`.
    pub omitted: usize,
    /// Associated documents skipped because their lifecycle (`draft`,
    /// `deprecated`) bars automatic delivery. They never count toward
    /// `omitted` and never take a slot.
    pub ineligible: usize,
}

#[derive(Default, Clone, Copy)]
struct Via {
    purpose: bool,
    source: bool,
    applies: bool,
}

fn card_base(file: &str, document: &StoredDocument) -> PathCard {
    PathCard {
        file: file.to_string(),
        doc_path: document.path.clone(),
        doc_id: document.id.clone(),
        doc_title: document.title.clone(),
        retrieval_id: String::new(),
        selector: None,
        target: None,
        start_line: 0,
        end_line: 0,
        summary: String::new(),
        summary_source: SummarySource::Title,
        status: document.status.clone(),
        evidence: EvidenceState::Unverified,
        kind: PathCardKind::FilePurpose,
    }
}

fn purpose_card(
    file: &str,
    document: &StoredDocument,
    row: &PurposeRow,
    kind: PathCardKind,
    evidence: EvidenceState,
) -> PathCard {
    let mut summary = excerpt(&row.purpose);
    let mut summary_source = SummarySource::PurposeRow;
    apply_evidence(evidence, &mut summary, &mut summary_source);
    PathCard {
        retrieval_id: row.key(),
        target: Some(row.target_path.clone()),
        start_line: row.line,
        end_line: row.line,
        summary,
        summary_source,
        evidence,
        kind,
        ..card_base(file, document)
    }
}

/// Rows of `rows` describing `file`: the first non-empty exact row, and the
/// most specific non-empty enclosing-directory row. Rows without purpose text
/// say nothing and are skipped.
fn matching_rows<'a>(
    rows: &'a [PurposeRow],
    file: &str,
) -> (Option<&'a PurposeRow>, Option<&'a PurposeRow>) {
    let mut exact: Option<&PurposeRow> = None;
    let mut directory: Option<&PurposeRow> = None;
    for row in rows.iter().filter(|row| !row.purpose.trim().is_empty()) {
        if row.target_path == file {
            exact = exact.or(Some(row));
        } else if file
            .strip_prefix(row.target_path.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
            && directory.is_none_or(|best| row.target_path.len() > best.target_path.len())
        {
            directory = Some(row);
        }
    }
    (exact, directory)
}

impl RepositoryService {
    /// The canonical repository-relative path of the file an `index` call with
    /// these effective `args` reads, resolved with the tool's own root/path
    /// semantics against the session `cwd`.
    ///
    /// `None` when the arguments name no existing regular file inside the
    /// authorized root (including through a symlink that leaves it), so a
    /// same-named file elsewhere can never be mistaken for the indexed one.
    pub fn index_target(&self, args: &serde_json::Value, cwd: Option<&Path>) -> Option<String> {
        let raw = args.get("path")?.as_str()?;
        let root = crate::tools::effective_root(args, cwd)?;
        let target = crate::tools::resolve_arg_path(raw, Some(&root))?;
        let canonical = target.canonicalize().ok()?;
        if !canonical.is_file() {
            return None;
        }
        let relative = canonical.strip_prefix(self.root().root()).ok()?;
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str()?),
                _ => return None,
            }
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    /// Select the authored knowledge directly associated with
    /// `request.file`. See the module documentation.
    ///
    /// # Errors
    ///
    /// Service errors when the index is unavailable or a source keeps changing;
    /// the caller should then attach nothing.
    pub fn path_knowledge(
        &self,
        request: &PathKnowledgeRequest,
    ) -> Result<PathKnowledge, ServiceError> {
        let mut outcome = PathKnowledge {
            file: request.file.clone(),
            generation: 0,
            in_corpus: false,
            cards: Vec::new(),
            omitted: 0,
            ineligible: 0,
        };
        // Nothing is read for an unsafe path or an empty allowance.
        let Ok(file) = normalize_relative_path(&request.file) else {
            return Ok(outcome);
        };
        if request.max_cards == 0 {
            return Ok(outcome);
        }
        outcome.file = file.clone();

        self.refresh()?;
        let (generation, source) = self
            .store()
            .source(&file)
            .map_err(ServiceError::unavailable)?;
        outcome.generation = generation;
        if !source.is_some_and(|source| source.state != SourceState::Excluded) {
            return Ok(outcome);
        }
        outcome.in_corpus = true;

        let (_, associations) = self
            .store()
            .associations(&file)
            .map_err(ServiceError::unavailable)?;
        let mut documents: BTreeMap<String, Via> = BTreeMap::new();
        for association in associations {
            let via = documents.entry(association.doc_path).or_default();
            match association.kind {
                AssociationKind::Purpose => via.purpose = true,
                AssociationKind::Source => via.source = true,
                AssociationKind::Applies => via.applies = true,
            }
        }

        let mut candidates: Vec<PathCard> = Vec::new();
        let mut evidence = EvidenceLookup::default();
        for (doc_path, via) in documents {
            // The file's own text is the outline's business, not a note.
            if doc_path == file {
                continue;
            }
            match self.fresh_document(&doc_path) {
                Ok(_) => {}
                Err(ServiceError::NotIndexed { .. }) => continue,
                Err(error) => return Err(error),
            }
            let (generation, entry) = self
                .store()
                .corpus_entry(&doc_path)
                .map_err(ServiceError::unavailable)?;
            let Some(entry) = entry else { continue };
            outcome.generation = outcome.generation.max(generation);
            let document = &entry.document;
            // Lifecycle is decided before the cap, so a draft or deprecated
            // document can never take a slot from an eligible one.
            if !eligible(document.status.as_deref()) {
                outcome.ineligible += 1;
                continue;
            }

            if via.purpose {
                let (exact, directory) = matching_rows(&entry.purposes, &file);
                for (row, kind) in [
                    (exact, PathCardKind::FilePurpose),
                    (directory, PathCardKind::DirectoryPurpose),
                ] {
                    if let Some(row) = row {
                        let state = evidence.state(
                            self,
                            &doc_path,
                            &row_record_key(&row.target_path, row.occurrence),
                        );
                        candidates.push(purpose_card(&file, document, row, kind, state));
                    }
                }
            }
            if via.source || via.applies {
                let section = document
                    .sections
                    .iter()
                    .find(|section| !section.body.trim().is_empty())
                    .or_else(|| document.sections.first());
                let Some(section) = section else { continue };
                let (mut summary, mut summary_source) =
                    summarize(document.description.as_deref(), section, &document.title);
                let state = evidence.state(self, &doc_path, &section_record_key(section));
                apply_evidence(state, &mut summary, &mut summary_source);
                candidates.push(PathCard {
                    retrieval_id: section.key.clone(),
                    selector: (!section.breadcrumb.is_empty())
                        .then(|| section.breadcrumb.join(" > ")),
                    start_line: section.start_line,
                    end_line: section.end_line,
                    summary,
                    summary_source,
                    evidence: state,
                    kind: PathCardKind::Note {
                        declared_source: via.source,
                        applies: via.applies,
                    },
                    ..card_base(&file, document)
                });
            }
        }

        // Most specific first; deeper directory rows before shallower ones;
        // then a stable document/line order.
        candidates.sort_by(|a, b| {
            let depth = |card: &PathCard| card.target.as_ref().map_or(0, String::len);
            (
                a.kind.rank(),
                std::cmp::Reverse(depth(a)),
                &a.doc_path,
                a.start_line,
            )
                .cmp(&(
                    b.kind.rank(),
                    std::cmp::Reverse(depth(b)),
                    &b.doc_path,
                    b.start_line,
                ))
        });
        outcome.omitted = candidates.len().saturating_sub(request.max_cards);
        candidates.truncate(request.max_cards);
        outcome.cards = candidates;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::browse::tests::Fx;
    use serde_json::json;

    const MANIFEST: &str = "version = 1\n[documents]\ninclude = [\"docs/**/*.md\", \"src/**/AGENTS.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n";

    fn fixture() -> Fx {
        let fx = Fx::new();
        fx.put(".qmt/knowledge.toml", MANIFEST);
        fx.put("src/queue.rs", "pub fn push_task(task: Task) {}\n");
        fx.put("src/other.rs", "pub fn other() {}\n");
        fx.put("src/net/tcp.rs", "pub fn connect() {}\n");
        fx.put("src/net/queue.rs", "pub fn net_queue() {}\n");
        fx.put(
            "src/AGENTS.md",
            "# Source\n\n| File | Purpose |\n|---|---|\n| `queue.rs` | Priority queue. |\n| `net/` | Network layer. |\n| `other.rs` | |\n",
        );
        fx
    }

    fn select(fx: &Fx, file: &str, max: usize) -> PathKnowledge {
        fx.service
            .path_knowledge(&PathKnowledgeRequest {
                file: file.into(),
                max_cards: max,
            })
            .unwrap()
    }

    #[test]
    fn purpose_rows_apply_to_the_file_and_to_files_below_a_directory_row() {
        let fx = fixture();

        let file = select(&fx, "src/queue.rs", 5);
        assert!(file.in_corpus);
        assert_eq!(file.cards.len(), 1);
        let card = &file.cards[0];
        assert_eq!(card.kind, PathCardKind::FilePurpose);
        assert_eq!(card.doc_path, "src/AGENTS.md");
        assert_eq!(card.target.as_deref(), Some("src/queue.rs"));
        assert_eq!(card.summary, "Priority queue.");
        assert_eq!(card.summary_source, SummarySource::PurposeRow);
        assert_eq!((card.start_line, card.end_line), (5, 5));
        assert!(card.retrieval_id.starts_with("purpose:src/AGENTS.md#"));

        let below = select(&fx, "src/net/tcp.rs", 5);
        assert_eq!(below.cards.len(), 1);
        assert_eq!(below.cards[0].kind, PathCardKind::DirectoryPurpose);
        assert_eq!(below.cards[0].target.as_deref(), Some("src/net"));
        assert_eq!(below.cards[0].summary, "Network layer.");
        assert_eq!(below.cards[0].start_line, 6);

        // An empty purpose cell says nothing, so it yields no card.
        assert!(select(&fx, "src/other.rs", 5).cards.is_empty());
    }

    #[test]
    fn declared_sources_and_applicability_globs_attach_notes_with_their_own_provenance() {
        let fx = fixture();
        fx.put(
            "docs/sched.md",
            "---\ndescription: How tasks are queued.\nknowledge_sources: src/queue.rs#push_task\n---\n# Scheduling\nTasks are pushed to a priority queue.\n",
        );
        fx.put(
            "docs/net.md",
            "---\nknowledge_paths: src/net/**\n---\n# Net\n## Overview\nSockets are pooled.\n",
        );

        let queue = select(&fx, "src/queue.rs", 5);
        let kinds: Vec<_> = queue.cards.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                PathCardKind::FilePurpose,
                PathCardKind::Note {
                    declared_source: true,
                    applies: false
                }
            ]
        );
        let note = &queue.cards[1];
        assert_eq!(note.doc_path, "docs/sched.md");
        assert_eq!(note.summary, "How tasks are queued.");
        assert_eq!(note.summary_source, SummarySource::Description);
        assert!(note.target.is_none());

        let tcp = select(&fx, "src/net/tcp.rs", 5);
        assert_eq!(tcp.cards.len(), 2);
        assert_eq!(tcp.cards[0].kind, PathCardKind::DirectoryPurpose);
        let applies = &tcp.cards[1];
        assert_eq!(
            applies.kind,
            PathCardKind::Note {
                declared_source: false,
                applies: true
            }
        );
        assert_eq!(applies.summary_source, SummarySource::Excerpt);
        assert!(applies.summary.contains("Sockets are pooled"));
        assert_eq!(applies.selector.as_deref(), Some("Net > Overview"));
        assert!(applies.retrieval_id.starts_with(&applies.doc_id));
    }

    #[test]
    fn a_note_about_one_file_never_attaches_to_a_same_named_file_elsewhere() {
        let fx = Fx::new();
        fx.put(".qmt/knowledge.toml", MANIFEST);
        fx.put("src/a/mod.rs", "pub fn a() {}\n");
        fx.put("src/b/mod.rs", "pub fn b() {}\n");
        fx.put(
            "docs/a.md",
            "---\nknowledge_sources: src/a/mod.rs\n---\n# A\nAbout a. It is unlike src/b/mod.rs and mod.rs.\n",
        );
        fx.put(
            "docs/mention.md",
            "# Mentions\nThis page talks about src/b/mod.rs and b/mod.rs at length.\n",
        );
        fx.put(
            "src/AGENTS.md",
            "| File | Purpose |\n|---|---|\n| `a/mod.rs` | The a module. |\n",
        );

        let a = select(&fx, "src/a/mod.rs", 5);
        assert_eq!(a.cards.len(), 2, "purpose row and declared source");
        let b = select(&fx, "src/b/mod.rs", 5);
        assert!(b.in_corpus);
        assert!(b.cards.is_empty(), "no basename matching, no text search");
    }

    #[test]
    fn cards_are_ordered_by_specificity_and_capped_without_losing_count() {
        let fx = fixture();
        fx.put(
            "src/net/AGENTS.md",
            "| File | Purpose |\n|---|---|\n| `tcp.rs` | TCP transport. |\n",
        );
        fx.put(
            "docs/tcp.md",
            "---\nknowledge_sources: src/net/tcp.rs\n---\n# Tcp\nHandshake notes.\n",
        );
        fx.put(
            "docs/net.md",
            "---\nknowledge_paths: src/net/**\n---\n# Net\nPooling.\n",
        );

        let all = select(&fx, "src/net/tcp.rs", 10);
        let docs: Vec<(&str, u8)> = all
            .cards
            .iter()
            .map(|c| (c.doc_path.as_str(), c.kind.rank()))
            .collect();
        assert_eq!(
            docs,
            [
                ("src/net/AGENTS.md", 0),
                ("src/AGENTS.md", 1),
                ("docs/tcp.md", 2),
                ("docs/net.md", 3)
            ]
        );
        assert_eq!(all.omitted, 0);

        let capped = select(&fx, "src/net/tcp.rs", 2);
        assert_eq!(capped.cards, all.cards[..2]);
        assert_eq!(capped.omitted, 2);
        let none = select(&fx, "src/net/tcp.rs", 0);
        assert!(none.cards.is_empty() && !none.in_corpus);
    }

    #[test]
    fn a_nearer_directory_row_wins_within_one_document() {
        let fx = Fx::new();
        fx.put(".qmt/knowledge.toml", MANIFEST);
        fx.put("src/net/tcp.rs", "pub fn connect() {}\n");
        fx.put(
            "src/AGENTS.md",
            "| File | Purpose |\n|---|---|\n| `net/` | Networking. |\n| `net/tcp.rs` | TCP. |\n| `.` | Everything. |\n",
        );
        let found = select(&fx, "src/net/tcp.rs", 5);
        let summaries: Vec<&str> = found.cards.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(summaries, ["TCP.", "Networking."]);
    }

    #[test]
    fn cards_never_repeat_outline_signatures() {
        let fx = fixture();
        fx.put(
            "docs/sched.md",
            "---\nknowledge_sources: src/queue.rs\n---\n# Scheduling\nTasks are queued.\n",
        );
        for card in select(&fx, "src/queue.rs", 5).cards {
            assert!(!card.summary.contains("push_task"), "{}", card.summary);
            assert!(!card.summary.contains("fn "), "{}", card.summary);
        }
    }

    #[test]
    fn cards_reflect_the_current_documents() {
        let fx = fixture();
        fx.put(
            "docs/sched.md",
            "---\nknowledge_sources: src/queue.rs\n---\n# Scheduling\nOld explanation.\n",
        );
        let first = select(&fx, "src/queue.rs", 5);
        assert!(first.cards.iter().any(|c| c.summary.contains("Old")));

        fx.put(
            "docs/sched.md",
            "---\nknowledge_sources: src/queue.rs\n---\n# Scheduling\nIntro line.\n## Details\nNew explanation.\n",
        );
        let edited = select(&fx, "src/queue.rs", 5);
        let note = edited.cards.iter().find(|c| c.doc_path == "docs/sched.md");
        assert!(note.is_some_and(|c| c.summary.contains("Intro")));
        assert!(edited.generation > first.generation);

        fx.put(
            "src/AGENTS.md",
            "| File | Purpose |\n|---|---|\n| `queue.rs` | Reworded purpose. |\n",
        );
        let reworded = select(&fx, "src/queue.rs", 5);
        assert_eq!(reworded.cards[0].summary, "Reworded purpose.");

        std::fs::remove_file(fx.repo.join("docs/sched.md")).unwrap();
        std::fs::remove_file(fx.repo.join("src/AGENTS.md")).unwrap();
        let gone = select(&fx, "src/queue.rs", 5);
        assert!(gone.in_corpus && gone.cards.is_empty());
    }

    #[test]
    fn files_outside_the_selected_corpus_or_unsafe_paths_get_nothing() {
        let fx = fixture();
        fx.put("tools/x.rs", "pub fn x() {}\n");
        fx.put(
            "docs/all.md",
            "---\nknowledge_paths: tools/**\nknowledge_sources: tools/x.rs\n---\n# All\nEverything.\n",
        );
        let outside = select(&fx, "tools/x.rs", 5);
        assert!(!outside.in_corpus);
        assert!(outside.cards.is_empty());

        for unsafe_path in ["../repo/src/queue.rs", "/etc/passwd", "", "src/missing.rs"] {
            let found = select(&fx, unsafe_path, 5);
            assert!(!found.in_corpus, "{unsafe_path:?}");
            assert!(found.cards.is_empty());
        }
    }

    #[test]
    fn index_target_resolves_effective_arguments_like_the_tool() {
        let fx = fixture();
        let repo = fx.repo.clone();
        let svc = &fx.service;
        let target = |args: serde_json::Value, cwd: Option<&Path>| svc.index_target(&args, cwd);

        let expected = Some("src/queue.rs".to_string());
        assert_eq!(
            target(json!({"path": "src/queue.rs"}), Some(&repo)),
            expected
        );
        assert_eq!(
            target(json!({"path": "./src/../src/queue.rs"}), Some(&repo)),
            expected
        );
        // Starting from a crate-style subdirectory.
        assert_eq!(
            target(json!({"path": "queue.rs"}), Some(&repo.join("src"))),
            expected
        );
        // `root` is resolved against the cwd, then `path` against it.
        assert_eq!(
            target(json!({"path": "queue.rs", "root": "src"}), Some(&repo)),
            expected
        );
        assert_eq!(
            target(
                json!({"path": repo.join("src/queue.rs"), "root": "/nonexistent"}),
                None
            ),
            expected,
            "an absolute path ignores root"
        );
        // The same name under a different root is a different file.
        assert_eq!(
            target(json!({"path": "queue.rs", "root": "src/net"}), Some(&repo)),
            Some("src/net/queue.rs".to_string())
        );

        // Nothing that is not a regular file inside the root.
        let outside = repo.parent().unwrap().join("outside.rs");
        std::fs::write(&outside, "fn f() {}\n").unwrap();
        assert_eq!(target(json!({"path": outside}), Some(&repo)), None);
        assert_eq!(target(json!({"path": "../outside.rs"}), Some(&repo)), None);
        assert_eq!(target(json!({"path": "src"}), Some(&repo)), None);
        assert_eq!(target(json!({"path": "src/missing.rs"}), Some(&repo)), None);
        assert_eq!(target(json!({"path": "src/queue.rs"}), None), None);
        assert_eq!(target(json!({}), Some(&repo)), None);
        assert_eq!(target(json!({"path": 7}), Some(&repo)), None);
    }

    fn acknowledge(fx: &Fx, document: &str) {
        use crate::knowledge::repository::acknowledge::{AcknowledgeRequest, Selection};
        let plan = fx
            .service
            .plan_acknowledge(&AcknowledgeRequest {
                document: document.into(),
                selection: Selection::WholeDocument,
            })
            .unwrap();
        fx.service.apply_acknowledge(&plan).unwrap();
    }

    #[test]
    fn draft_and_deprecated_documents_never_become_cards_or_take_a_slot() {
        let fx = fixture();
        for (name, status) in [
            ("draft", "draft"),
            ("old", "Deprecated"),
            ("ok", "reviewed"),
        ] {
            fx.put(
                &format!("docs/{name}.md"),
                &format!(
                    "---\nstatus: {status}\nknowledge_sources: src/queue.rs\n---\n# {name}\nNotes {name}.\n"
                ),
            );
        }
        fx.put(
            "docs/a_draft.md",
            "---\nstatus: draft\nknowledge_paths: src/**\n---\n# A\nEarlier in order.\n",
        );
        let found = select(&fx, "src/queue.rs", 2);
        let docs: Vec<&str> = found.cards.iter().map(|c| c.doc_path.as_str()).collect();
        assert_eq!(
            docs,
            ["src/AGENTS.md", "docs/ok.md"],
            "cap of two still filled"
        );
        assert_eq!(found.ineligible, 3);
        assert_eq!(found.omitted, 0, "ineligible documents are not 'omitted'");

        // Explicit retrieval still serves them.
        let draft = fx.service.fresh_document("docs/draft.md").unwrap();
        assert_eq!(draft.document.status.as_deref(), Some("draft"));
        let hits = fx
            .service
            .query(&crate::knowledge::repository::query::QueryRequest {
                question: "Notes draft".into(),
                lane: crate::knowledge::repository::query::Lane::Documentation,
                path: None,
                limit: None,
            })
            .unwrap();
        assert!(hits.hits.iter().any(|h| h.path == "docs/draft.md"));
    }

    #[test]
    fn evidence_states_label_cards_and_missing_evidence_withholds_the_text() {
        let fx = fixture();
        fx.put("src/dep.rs", "pub fn dep() {}\n");
        fx.put(
            "docs/dep.md",
            "---\nknowledge_sources: src/dep.rs\nknowledge_paths: src/queue.rs\n---\n# Dep\nQueue relies on dep.\n",
        );
        let note = |fx: &Fx| {
            select(fx, "src/queue.rs", 5)
                .cards
                .into_iter()
                .find(|c| c.doc_path == "docs/dep.md")
                .unwrap()
        };

        // No acknowledgement: delivered, labeled unverified.
        let card = note(&fx);
        assert_eq!(card.evidence, EvidenceState::Unverified);
        assert!(card.summary.contains("relies on dep"));

        // Acknowledged and unchanged.
        acknowledge(&fx, "docs/dep.md");
        let card = note(&fx);
        assert_eq!(card.evidence, EvidenceState::Unchanged);
        assert!(card.summary.contains("relies on dep"));

        // Dependency changed: labeled, text still delivered, not called false.
        fx.put("src/dep.rs", "pub fn dep() { changed(); }\n");
        let card = note(&fx);
        assert_eq!(card.evidence, EvidenceState::Changed);
        assert!(card.summary.contains("relies on dep"));
        assert!(
            crate::knowledge::repository::evidence_caution(card.evidence)
                .unwrap()
                .contains("changed")
        );

        // Dependency gone: a cautionary pointer only, with citation kept.
        std::fs::remove_file(fx.repo.join("src/dep.rs")).unwrap();
        let card = note(&fx);
        assert_eq!(card.evidence, EvidenceState::Missing);
        assert_eq!(card.summary, "");
        assert_eq!(card.summary_source, SummarySource::Withheld);
        assert_eq!(card.doc_path, "docs/dep.md");
        assert!(card.start_line >= 1 && card.retrieval_id.starts_with(&card.doc_id));
    }

    #[test]
    fn purpose_rows_carry_their_own_evidence_state() {
        use crate::knowledge::repository::acknowledge::{AcknowledgeRequest, Selection};
        let fx = fixture();
        // Directory rows cannot be acknowledged, so select the file row only.
        let plan = fx
            .service
            .plan_acknowledge(&AcknowledgeRequest {
                document: "src/AGENTS.md".into(),
                selection: Selection::Records(vec!["row:src/queue.rs".into()]),
            })
            .unwrap();
        fx.service.apply_acknowledge(&plan).unwrap();
        let card = select(&fx, "src/queue.rs", 5).cards.remove(0);
        assert_eq!(card.evidence, EvidenceState::Unchanged);

        fx.put("src/queue.rs", "pub fn push_task(task: Task) { v2(); }\n");
        let card = select(&fx, "src/queue.rs", 5).cards.remove(0);
        assert_eq!(card.evidence, EvidenceState::Changed);
        assert_eq!(card.summary, "Priority queue.");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_leaving_the_root_is_not_an_index_target() {
        let fx = fixture();
        let outside = fx.repo.parent().unwrap().join("secret.rs");
        std::fs::write(&outside, "fn secret() {}\n").unwrap();
        std::os::unix::fs::symlink(&outside, fx.repo.join("src/link.rs")).unwrap();
        let args = json!({"path": "src/link.rs"});
        assert_eq!(fx.service.index_target(&args, Some(&fx.repo)), None);
    }
}
