//! Relevance gate v1: which authored sections are relevant to a user prompt.
//!
//! The gate is a fixed, explainable heuristic, not a calibrated confidence
//! score. It is fully specified here so its version and word lists can be
//! frozen before held-out evaluation. Anything that changes a threshold, a
//! limit or a word list requires a new [`GATE_VERSION`].
//!
//! # Specification (`gate_version = 1`)
//!
//! 1. **Normalize**: Unicode NFKC, truncated to [`MAX_PROMPT_BYTES`] (2 KiB) on
//!    a character boundary. Tokens are lowercased; camelCase boundaries are
//!    found before lowercasing, because afterwards they no longer exist.
//! 2. **Tokenize**: whitespace-separated words are trimmed of leading and
//!    trailing non-identifier characters. A word that contains `::`, `.`, `/`
//!    or `-` internally (an identifier or path) is kept whole as a token, and
//!    every word is also split on non-alphanumeric characters (so `_`, `::`,
//!    `.`, `/` and `-` separate parts) and at camelCase boundaries; both the
//!    parts and the part they came from are tokens.
//! 3. **Drop** tokens shorter than three characters, pure numbers, and tokens
//!    in [`STOPWORDS`] or [`GENERIC_VERBS`]. At most [`MAX_TOKENS`] (64)
//!    distinct tokens are kept, in order of first appearance.
//! 4. **Pass**: a section passes when
//!    * (a) the prompt names an indexed repository-relative path, or a
//!      qualified identifier (`a::b`) present among the extracted symbols, and
//!      the section belongs to a document that is that path or is directly
//!      associated with it; or
//!    * (b) at least [`METADATA_THRESHOLD`] (2) distinct tokens are words of its
//!      title, headings, tags or description, or at least [`BODY_THRESHOLD`] (3)
//!      are words of its body.
//! 5. **Rank**: path/identifier sections first (exact paths before identifiers,
//!    then path and ordinal), then the documentation-lane BM25 order with
//!    deterministic ties (path, ordinal).
//!
//! Word matching is exact and unstemmed. Full-text search only proposes
//! candidates (a superset, bounded by [`CANDIDATE_LIMIT`]); the thresholds are
//! applied to exact words. Non-English prompts mostly fail step 4b and produce
//! nothing, which is intended conservatism.
//!
//! The gate selects sections; whether a section may be delivered automatically
//! (lifecycle, evidence, budgets, duplicates) is decided by the delivery
//! stages, not here.

use super::query::{fts_expression, path_candidates, split_camel};
use super::reconcile::ReconcileStatus;
use super::service::{RepositoryService, ServiceError, SourceCheck};
use super::store::{SearchPlan, StoredDocument, StoredSection};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use unicode_normalization::UnicodeNormalization;

/// Version of the thresholds, limits and word lists below.
pub const GATE_VERSION: u32 = 1;
/// Longest prompt prefix considered.
pub const MAX_PROMPT_BYTES: usize = 2048;
/// Most distinct tokens kept.
pub const MAX_TOKENS: usize = 64;
/// Distinct metadata (title/heading/tag/description) tokens needed to pass.
pub const METADATA_THRESHOLD: usize = 2;
/// Distinct body tokens needed to pass.
pub const BODY_THRESHOLD: usize = 3;
/// Most full-text candidates examined.
pub const CANDIDATE_LIMIT: usize = 200;
/// Shortest token kept, in characters.
const MIN_TOKEN_CHARS: usize = 3;

/// Fixed English stopwords (lowercase). Part of gate v1.
pub const STOPWORDS: &[&str] = &[
    "about", "after", "again", "all", "also", "and", "any", "are", "because", "been", "before",
    "being", "between", "both", "but", "can", "could", "did", "does", "doing", "done", "down",
    "each", "few", "for", "from", "further", "had", "has", "have", "having", "her", "here", "hers",
    "him", "his", "how", "into", "its", "just", "more", "most", "must", "not", "off", "once",
    "only", "other", "our", "out", "over", "own", "same", "she", "should", "some", "such", "than",
    "that", "the", "their", "them", "then", "there", "these", "they", "this", "those", "through",
    "too", "under", "until", "very", "was", "were", "what", "when", "where", "which", "while",
    "who", "whom", "why", "will", "with", "would", "you", "your",
];

/// Fixed generic coding verbs and filler (lowercase). Part of gate v1.
pub const GENERIC_VERBS: &[&str] = &[
    "add",
    "bug",
    "change",
    "check",
    "code",
    "create",
    "delete",
    "feature",
    "file",
    "fix",
    "function",
    "get",
    "help",
    "implement",
    "issue",
    "look",
    "make",
    "modify",
    "need",
    "new",
    "please",
    "refactor",
    "remove",
    "set",
    "show",
    "support",
    "thing",
    "things",
    "update",
    "use",
    "using",
    "want",
    "work",
    "working",
    "works",
    "write",
];

/// Stable identity of the gate: version, thresholds, limits and both word
/// lists. A frozen evaluation records it; any change to the gate changes it.
pub fn gate_fingerprint() -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!(
        "qmt-gate\0{GATE_VERSION}\0{MAX_PROMPT_BYTES}\0{MAX_TOKENS}\0{METADATA_THRESHOLD}\0{BODY_THRESHOLD}\0{MIN_TOKEN_CHARS}\0"
    ));
    for word in STOPWORDS {
        hasher.update(word.as_bytes());
        hasher.update([0u8]);
    }
    hasher.update([1u8]);
    for word in GENERIC_VERBS {
        hasher.update(word.as_bytes());
        hasher.update([0u8]);
    }
    hex::encode(&hasher.finalize()[..16])
}

/// NFKC-normalize `prompt` and truncate it to [`MAX_PROMPT_BYTES`].
pub fn normalize_prompt(prompt: &str) -> String {
    let mut text: String = prompt.nfkc().collect();
    if text.len() > MAX_PROMPT_BYTES {
        let mut end = MAX_PROMPT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every token of `text` (lowercase, deduplicated, in order of first
/// appearance), before any dropping. See the module documentation.
fn expand(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |token: String| {
        if !token.is_empty() && seen.insert(token.clone()) {
            out.push(token);
        }
    };
    for chunk in text.split_whitespace() {
        let word = chunk.trim_matches(|c: char| !is_ident(c));
        if word.is_empty() {
            continue;
        }
        let compound = word.contains([':', '.', '/', '-']);
        if compound {
            push(word.to_lowercase());
        }
        for part in word.split(|c: char| !c.is_alphanumeric()) {
            if part.is_empty() {
                continue;
            }
            push(part.to_lowercase());
            let camel = split_camel(part);
            if camel.len() > 1 {
                for piece in camel {
                    push(piece.to_lowercase());
                }
            }
        }
    }
    out
}

fn dropped(token: &str) -> bool {
    token.chars().count() < MIN_TOKEN_CHARS
        || token.chars().all(char::is_numeric)
        || STOPWORDS.contains(&token)
        || GENERIC_VERBS.contains(&token)
}

/// Gate tokens of `prompt`.
pub fn gate_tokens(prompt: &str) -> Vec<String> {
    let mut tokens: Vec<String> = expand(&normalize_prompt(prompt))
        .into_iter()
        .filter(|token| !dropped(token))
        .collect();
    tokens.truncate(MAX_TOKENS);
    tokens
}

/// Words of indexed text, as the gate compares them.
fn words_of(text: &str) -> BTreeSet<String> {
    let normalized: String = text.nfkc().collect();
    expand(&normalized).into_iter().collect()
}

fn matched(tokens: &[String], words: &BTreeSet<String>) -> usize {
    tokens.iter().filter(|token| words.contains(*token)).count()
}

/// Why a section passed the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateReason {
    /// The prompt names this document's path, or a file it is associated with.
    ExactPath,
    /// The prompt names a qualified symbol of a file the document is
    /// associated with.
    QualifiedIdentifier,
    /// This many distinct tokens matched title/headings/tags/description.
    Metadata(usize),
    /// This many distinct tokens matched the body.
    Body(usize),
}

/// A section that passed the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct GateSection {
    pub doc_path: String,
    pub doc_id: String,
    pub doc_title: String,
    pub doc_status: Option<String>,
    pub doc_description: Option<String>,
    pub section: StoredSection,
    pub reason: GateReason,
}

/// The gate's decision for one prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct GateOutcome {
    pub gate_version: u32,
    pub generation: u64,
    pub status: ReconcileStatus,
    /// The kept tokens, for diagnostics and replay.
    pub tokens: Vec<String>,
    /// Passing sections in rank order.
    pub sections: Vec<GateSection>,
}

impl GateOutcome {
    fn empty(status: ReconcileStatus, generation: u64, tokens: Vec<String>) -> Self {
        Self {
            gate_version: GATE_VERSION,
            generation,
            status,
            tokens,
            sections: Vec::new(),
        }
    }
}

fn anchored(document: &StoredDocument, reason: GateReason, out: &mut Vec<GateSection>) {
    for section in &document.sections {
        out.push(GateSection {
            doc_path: document.path.clone(),
            doc_id: document.id.clone(),
            doc_title: document.title.clone(),
            doc_status: document.status.clone(),
            doc_description: document.description.clone(),
            section: section.clone(),
            reason,
        });
    }
}

impl RepositoryService {
    /// Evaluate the relevance gate for `prompt` against the current corpus.
    ///
    /// Reconciles first (cheap when nothing changed), then revalidates every
    /// source it is about to return, like [`query`](Self::query). Performs no
    /// model call and no network request.
    pub fn gate(&self, prompt: &str) -> Result<GateOutcome, ServiceError> {
        let normalized = normalize_prompt(prompt);
        let tokens = gate_tokens(prompt);
        let candidates = path_candidates(&normalized);
        if tokens.is_empty() && candidates.is_empty() {
            let generation = self
                .store()
                .generation()
                .map_err(ServiceError::unavailable)?;
            return Ok(GateOutcome::empty(
                ReconcileStatus::Unchanged,
                generation,
                tokens,
            ));
        }
        let qualified: Vec<String> = tokens
            .iter()
            .filter(|token| token.contains("::"))
            .cloned()
            .collect();
        let searchable: Vec<String> = tokens
            .iter()
            .filter(|token| token.chars().all(char::is_alphanumeric))
            .cloned()
            .collect();
        let plan = SearchPlan {
            fts_expression: (!searchable.is_empty()).then(|| fts_expression(&searchable)),
            fts_limit: CANDIDATE_LIMIT,
            symbol_terms: qualified.clone(),
            ..SearchPlan::default()
        };

        let mut unstable = String::new();
        for _ in 0..self.attempts() {
            let report = self.refresh()?;
            let snapshot = self
                .store()
                .search(&plan)
                .map_err(ServiceError::unavailable)?;
            let mut sections: Vec<GateSection> = Vec::new();
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut push = |found: GateSection, sections: &mut Vec<GateSection>| {
                if seen.insert(found.section.key.clone()) {
                    sections.push(found);
                }
            };

            // (a) Paths and qualified identifiers the prompt names.
            let (_, named) = self
                .store()
                .existing_sources(&candidates)
                .map_err(ServiceError::unavailable)?;
            let identified: BTreeSet<String> = snapshot
                .symbols
                .iter()
                .filter(|found| {
                    found.record.qualified_name.contains("::")
                        && qualified.contains(&found.record.qualified_name.to_lowercase())
                })
                .map(|found| found.path.clone())
                .collect();
            let anchors = named
                .iter()
                .map(|path| (path.clone(), GateReason::ExactPath))
                .chain(
                    identified
                        .iter()
                        .map(|path| (path.clone(), GateReason::QualifiedIdentifier)),
                );
            for (file, reason) in anchors {
                let mut docs: BTreeSet<String> = self
                    .store()
                    .associations(&file)
                    .map_err(ServiceError::unavailable)?
                    .1
                    .into_iter()
                    .map(|association| association.doc_path)
                    .collect();
                docs.insert(file);
                for path in docs {
                    let (_, document) = self
                        .store()
                        .document(&path)
                        .map_err(ServiceError::unavailable)?;
                    if let Some(document) = document {
                        let mut found = Vec::new();
                        anchored(&document, reason, &mut found);
                        for section in found {
                            push(section, &mut sections);
                        }
                    }
                }
            }

            // (b) Token thresholds over full-text candidates, best first.
            for found in &snapshot.sections {
                let mut metadata = String::new();
                for part in found
                    .section
                    .title
                    .iter()
                    .chain(&found.section.breadcrumb)
                    .chain(std::iter::once(&found.doc_title))
                    .chain(&found.doc_tags)
                    .chain(found.doc_description.iter())
                {
                    metadata.push_str(part);
                    metadata.push('\n');
                }
                let in_metadata = matched(&tokens, &words_of(&metadata));
                let reason = if in_metadata >= METADATA_THRESHOLD {
                    GateReason::Metadata(in_metadata)
                } else {
                    let in_body = matched(&tokens, &words_of(&found.section.body));
                    if in_body >= BODY_THRESHOLD {
                        GateReason::Body(in_body)
                    } else {
                        continue;
                    }
                };
                push(
                    GateSection {
                        doc_path: found.doc_path.clone(),
                        doc_id: found.doc_id.clone(),
                        doc_title: found.doc_title.clone(),
                        doc_status: found.doc_status.clone(),
                        doc_description: found.doc_description.clone(),
                        section: found.section.clone(),
                        reason,
                    },
                    &mut sections,
                );
            }

            let paths: BTreeSet<&str> = sections.iter().map(|s| s.doc_path.as_str()).collect();
            let mut current = true;
            for path in paths {
                if !matches!(self.check_source(path)?, SourceCheck::Current) {
                    unstable = path.to_string();
                    current = false;
                    break;
                }
            }
            if current {
                return Ok(GateOutcome {
                    gate_version: GATE_VERSION,
                    generation: snapshot.generation,
                    status: report.status,
                    tokens,
                    sections,
                });
            }
        }
        Err(ServiceError::Updating { path: unstable })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepositoryKnowledgeSettings;
    use crate::knowledge::repository::reconcile::ReconcileOptions;
    use crate::knowledge::repository::root::{RootRequest, resolve_root};
    use crate::knowledge::repository::service::RefreshPolicy;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    const MANIFEST: &str = "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n";

    struct Fx {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        service: RepositoryService,
    }

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            fs::create_dir_all(repo.join(".git")).unwrap();
            let root = resolve_root(&RootRequest {
                cwd: Some(&repo),
                ..Default::default()
            })
            .unwrap();
            let service = RepositoryService::open(
                root,
                &tmp.path().join("cache/index.sqlite"),
                ReconcileOptions::from(&RepositoryKnowledgeSettings::default()),
                RefreshPolicy {
                    max_attempts: 20,
                    retry_delay: Duration::from_millis(1),
                    busy_timeout: Duration::from_secs(10),
                },
            )
            .unwrap();
            let fx = Self {
                _tmp: tmp,
                repo,
                service,
            };
            fx.put(".qmt/knowledge.toml", MANIFEST);
            fx
        }

        fn put(&self, rel: &str, content: &str) {
            let path = self.repo.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn gate(&self, prompt: &str) -> GateOutcome {
            self.service.gate(prompt).unwrap()
        }
    }

    fn keys(outcome: &GateOutcome) -> Vec<String> {
        outcome
            .sections
            .iter()
            .map(|s| s.section.key.clone())
            .collect()
    }

    const ARCH: &str = "# Architecture\n## Scheduling\nThe scheduler dispatches queued tasks to workers using a priority queue.\n## Storage\nRows live in SQLite.\n";

    #[test]
    fn tokens_follow_the_specification() {
        let tokens = gate_tokens("How does SessionStore::compact work?");
        assert_eq!(
            tokens,
            [
                "sessionstore::compact",
                "sessionstore",
                "session",
                "store",
                "compact"
            ]
        );
        // NFKC folds compatibility forms before tokenizing.
        assert_eq!(gate_tokens("ＳＣＨＥＤＵＬＥＲ"), ["scheduler"]);
        // Short words, numbers, stopwords and generic verbs are dropped.
        assert_eq!(gate_tokens("please add 2024 to be of the queue"), ["queue"]);
        // Separators split identifiers and keep the original.
        assert_eq!(
            gate_tokens("fetch_user-profile"),
            ["fetch_user-profile", "fetch", "user", "profile"]
        );
        // At most 64 distinct tokens, in first-appearance order.
        let many = (0..100).map(|n| format!("word{n:03}")).collect::<Vec<_>>();
        let tokens = gate_tokens(&many.join(" "));
        assert_eq!(tokens.len(), MAX_TOKENS);
        assert_eq!(tokens[0], "word000");
        // The prompt is truncated to 2 KiB on a character boundary.
        let long = format!("{} tail", "é".repeat(MAX_PROMPT_BYTES));
        assert!(normalize_prompt(&long).len() <= MAX_PROMPT_BYTES);
        assert!(!gate_tokens(&long).contains(&"tail".to_string()));
    }

    #[test]
    fn the_frozen_gate_is_identified_by_a_stable_fingerprint() {
        assert_eq!(GATE_VERSION, 1);
        assert_eq!(gate_fingerprint(), "985e6fcb143a079c9aea1a8fe6ef8e00");
    }

    #[test]
    fn a_path_free_conceptual_prompt_reaches_the_matching_section() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let outcome = fx.gate("the scheduler dispatches queued tasks to the workers");
        assert_eq!(outcome.gate_version, 1);
        let first = &outcome.sections[0];
        assert_eq!(first.doc_path, "docs/arch.md");
        assert!(first.section.body.contains("priority queue"));
        assert!(matches!(first.reason, GateReason::Body(n) if n >= BODY_THRESHOLD));
        assert!(
            outcome
                .sections
                .iter()
                .all(|s| !s.section.body.contains("SQLite"))
        );
    }

    #[test]
    fn metadata_needs_two_tokens_and_body_needs_three() {
        let fx = Fx::new();
        fx.put(
            "docs/queue.md",
            "---\ntags: dispatch\n---\n# Task scheduler\nWorkers pull jobs.\n",
        );
        // Two metadata words (title `task`/`scheduler`) pass.
        let two = fx.gate("task scheduler");
        assert!(matches!(two.sections[0].reason, GateReason::Metadata(2)));
        // One metadata word and no body words: nothing.
        assert!(fx.gate("scheduler elephants").sections.is_empty());
        // Two body words: nothing; three: passes.
        assert!(fx.gate("workers jobs").sections.is_empty());
        let body = fx.gate("workers pull jobs");
        assert!(matches!(body.sections[0].reason, GateReason::Body(3)));
    }

    #[test]
    fn generic_and_non_english_prompts_yield_nothing() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        assert!(
            fx.gate("please add a function to fix the code with queue")
                .sections
                .is_empty()
        );
        assert!(
            fx.gate("как работает планировщик задач")
                .sections
                .is_empty()
        );
        assert!(fx.gate("   ").sections.is_empty());
    }

    #[test]
    fn an_exact_path_bypasses_the_token_thresholds() {
        let fx = Fx::new();
        fx.put("src/session/store.rs", "pub fn open() {}\n");
        fx.put(
            "docs/unrelated.md",
            "---\nknowledge_paths: src/session/**\n---\n# Notes\nNothing that overlaps the prompt.\n",
        );
        fx.put("docs/other.md", "# Other\nAlso unrelated.\n");
        let outcome = fx.gate("look at src/session/store.rs");
        let docs: Vec<&str> = outcome
            .sections
            .iter()
            .map(|s| s.doc_path.as_str())
            .collect();
        assert_eq!(docs, ["docs/unrelated.md"]);
        assert_eq!(outcome.sections[0].reason, GateReason::ExactPath);
        // Naming a document by path selects its own sections.
        let doc = fx.gate("docs/other.md");
        assert_eq!(doc.sections[0].doc_path, "docs/other.md");
        // A same-named file that is not indexed selects nothing.
        assert!(fx.gate("src/elsewhere/store.rs").sections.is_empty());
    }

    #[test]
    fn a_qualified_identifier_in_the_index_selects_associated_documents() {
        let fx = Fx::new();
        fx.put(
            "src/lib.rs",
            "pub struct SessionStore;\nimpl SessionStore {\n    pub fn compact(&self) {}\n}\n",
        );
        fx.put(
            "docs/notes.md",
            "---\nknowledge_sources: src/lib.rs\n---\n# Notes\nUnrelated words.\n",
        );
        let hit = fx.gate("why does SessionStore::compact misbehave");
        assert!(
            hit.sections
                .iter()
                .any(|s| s.reason == GateReason::QualifiedIdentifier
                    && s.doc_path == "docs/notes.md"),
            "{hit:?}"
        );
        assert!(
            fx.gate("why does Missing::compact misbehave")
                .sections
                .is_empty()
        );
    }

    #[test]
    fn results_are_reproducible_and_stably_ordered_across_fresh_caches() {
        let fx = Fx::new();
        fx.put(
            "docs/a.md",
            "# Queue notes\nscheduler dispatch workers queue\n",
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
        let first = fx.gate(prompt);
        assert_eq!(keys(&first), keys(&fx.gate(prompt)));
        let docs: Vec<&str> = first.sections.iter().map(|s| s.doc_path.as_str()).collect();
        assert_eq!(
            docs,
            ["docs/a.md", "docs/b.md", "docs/c.md"],
            "ties by path"
        );

        let other = Fx::new();
        other.put(
            "docs/c.md",
            "# Queue notes\nscheduler dispatch workers queue\n",
        );
        other.put(
            "docs/a.md",
            "# Queue notes\nscheduler dispatch workers queue\n",
        );
        other.put(
            "docs/b.md",
            "# Queue notes\nscheduler dispatch workers queue\n",
        );
        assert_eq!(keys(&first), keys(&other.gate(prompt)));
    }

    #[test]
    fn edits_are_seen_without_any_hint() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        assert!(
            !fx.gate("scheduler dispatches queued tasks")
                .sections
                .is_empty()
        );
        fx.put("docs/arch.md", "# Architecture\nRows live in SQLite.\n");
        assert!(
            fx.gate("scheduler dispatches queued tasks")
                .sections
                .is_empty()
        );
    }
}
