//! Offline, deterministic evaluation of repository-knowledge retrieval and
//! automatic delivery (the M5 pilot).
//!
//! No model, provider, network or session database is involved. An evaluation
//! takes a frozen [`QuestionSet`] and a corpus *arm* (a manifest choosing which
//! repository files are indexed), copies exactly the files that manifest selects
//! into a scratch checkout, indexes them into a fresh cache, and then replays:
//!
//! * **retrieval** - every positive question through the `all` lane
//!   (recall@k, MRR, wrong-top-hit rate, response bytes);
//! * **orientation delivery** - every question's prompt through the relevance
//!   gate (cue hits, missed cues, wrong cards, false positives on negatives);
//! * **scenarios** - parallel index calls, a repeated prompt (duplicate
//!   suppression) and a compaction boundary, against the shared turn budget.
//!
//! The report separates a deterministic part (identical for identical corpus,
//! questions and gate) from [`Timing`], which is wall-clock and never compared.
//!
//! Held-out questions are only replayed when the gate recorded in the question
//! set equals the compiled gate, so thresholds and word lists cannot be tuned
//! after seeing held-out results without the report saying so.

use super::adapters::{
    BUILTIN_TOOL_SOURCE, INDEX_TOOL_NAME, IndexCall, ORIENTATION_HANDLER_ID, PromptInput,
    USER_PROMPT_EVENT, orientation_contributions, parse_mark, post_index_contributions,
    render_orientation_card,
};
use super::budget::{DeliveryBudget, estimate_tokens, serialized_len};
use super::code::CODE_EXTRACTOR_VERSION;
use super::diagnostics::reason_label;
use super::discovery::{DiscoveryOptions, discover};
use super::gate::{GATE_VERSION, gate_fingerprint};
use super::manifest::{KnowledgeManifest, MANIFEST_RELATIVE_PATH};
use super::map::MapRequest;
use super::orientation::{CardOrigin, OrientationRequest};
use super::query::{Lane, QueryRequest, normalize_relative_path};
use super::reconcile::ReconcileOptions;
use super::root::{RootRequest, resolve_root};
use super::service::{RefreshPolicy, RepositoryService};
use super::store::SCHEMA_VERSION;
use crate::config::{RepositoryKnowledgeDeliverySettings, RepositoryKnowledgeSettings};
use crate::hooks::HookContextContribution;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use thiserror::Error;

/// Lanes replayed for every positive question.
const RETRIEVAL_LANES: [Lane; 3] = [Lane::All, Lane::Documentation, Lane::Navigation];

/// Supported question-set format.
pub const QUESTION_SET_VERSION: u32 = 1;
/// Scenario budget identity; changing the default budget changes comparisons.
pub const DEFAULT_RECALL_K: usize = 5;

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("question set is invalid: {0}")]
    Questions(String),
    #[error("held-out replay refused: {0}")]
    NotFrozen(String),
    #[error("arm `{0}` is not defined in the question set")]
    UnknownArm(String),
    #[error("arm manifest `{path}`: {reason}")]
    Arm { path: String, reason: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Setup(String),
}

// --- question set ----------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionSet {
    pub version: u32,
    /// Source revision the expectations were written against.
    pub revision: String,
    /// Relevance-gate identity recorded when the set was frozen.
    pub gate_version: u32,
    pub gate_fingerprint: String,
    /// Corpus fingerprint per arm recorded at freeze time (optional).
    #[serde(default)]
    pub frozen_corpus: BTreeMap<String, String>,
    #[serde(default)]
    pub arm: Vec<Arm>,
    #[serde(default)]
    pub question: Vec<Question>,
    #[serde(default)]
    pub scenario: Vec<Scenario>,
}

/// A corpus variant: which repository manifest selects what is indexed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arm {
    pub name: String,
    /// Repository-relative manifest file. Absent: an empty corpus (the
    /// no-knowledge baseline, which only the model-backed comparison can score).
    #[serde(default)]
    pub manifest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    Dev,
    HeldOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryClass {
    /// Names a file, symbol or concept literally.
    Navigation,
    /// Asks how or why something works, in the repository's own vocabulary.
    Conceptual,
    /// Describes a task with no file names or identifiers.
    PathFree,
    NonEnglish,
    /// Must produce no card (unrelated, generic, outside the repository).
    Negative,
}

impl QueryClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Navigation => "navigation",
            Self::Conceptual => "conceptual",
            Self::PathFree => "path_free",
            Self::NonEnglish => "non_english",
            Self::Negative => "negative",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub split: Split,
    pub class: QueryClass,
    pub text: String,
    /// Acceptable repository-relative paths (source files or documents). Empty
    /// means no card is the intended outcome.
    #[serde(default)]
    pub expect: Vec<String>,
}

impl Question {
    pub fn is_positive(&self) -> bool {
        !self.expect.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioKind {
    /// An orientation prompt and several index calls in one turn.
    Parallel,
    /// The same turn twice with history carried over: nothing is re-sent.
    Repeat,
    /// Delivery, a compaction boundary, then re-evaluation in the new epoch.
    Compaction,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    pub split: Split,
    pub kind: ScenarioKind,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Repository-relative files of successful built-in `index` calls, in
    /// tool-call order.
    #[serde(default)]
    pub files: Vec<String>,
}

const FORBIDDEN_FRAGMENTS: &[&str] = &[
    "/users/",
    "/home/",
    "c:\\",
    "session_id",
    "tool_use_id",
    "api_key",
    "apikey",
    "password",
    "secret=",
    "begin private key",
    "sk-",
    "bearer ",
];

impl QuestionSet {
    pub fn parse(text: &str) -> Result<Self, EvalError> {
        let set: Self = toml::from_str(text).map_err(|e| EvalError::Questions(e.to_string()))?;
        set.validate()?;
        Ok(set)
    }

    /// Reject anything that could carry a transcript, a secret or a developer's
    /// absolute path into a committed fixture, and any malformed expectation.
    pub fn validate(&self) -> Result<(), EvalError> {
        let mut problems = Vec::new();
        if self.version != QUESTION_SET_VERSION {
            problems.push(format!("unsupported version {}", self.version));
        }
        if self.revision.trim().is_empty() {
            problems.push("revision is empty".into());
        }
        let mut ids = BTreeSet::new();
        let mut arms = BTreeSet::new();
        for arm in &self.arm {
            if !arms.insert(arm.name.as_str()) {
                problems.push(format!("duplicate arm `{}`", arm.name));
            }
            if let Some(manifest) = &arm.manifest
                && let Err(reason) = normalize_relative_path(manifest)
            {
                problems.push(format!("arm `{}` manifest: {reason}", arm.name));
            }
        }
        fn check_text(problems: &mut Vec<String>, what: &str, id: &str, text: &str) {
            let lower = text.to_lowercase();
            if let Some(bad) = FORBIDDEN_FRAGMENTS.iter().find(|f| lower.contains(**f)) {
                problems.push(format!("{what} `{id}` contains forbidden fragment `{bad}`"));
            }
            if text.len() > super::gate::MAX_PROMPT_BYTES {
                problems.push(format!("{what} `{id}` exceeds the gate's prompt cap"));
            }
        }
        for q in &self.question {
            if !ids.insert(q.id.as_str()) {
                problems.push(format!("duplicate id `{}`", q.id));
            }
            check_text(&mut problems, "question", &q.id, &q.text);
            if q.text.trim().is_empty() {
                problems.push(format!("question `{}` is empty", q.id));
            }
            if (q.class == QueryClass::Negative || q.class == QueryClass::NonEnglish)
                && q.is_positive()
            {
                problems.push(format!(
                    "question `{}` is {} but has expectations",
                    q.id,
                    q.class.as_str()
                ));
            }
            if matches!(
                q.class,
                QueryClass::Navigation | QueryClass::Conceptual | QueryClass::PathFree
            ) && !q.is_positive()
            {
                problems.push(format!(
                    "question `{}` is positive-class but expects nothing",
                    q.id
                ));
            }
            for path in &q.expect {
                match normalize_relative_path(path) {
                    Ok(normal) if normal == *path => {}
                    Ok(_) => problems.push(format!(
                        "question `{}` path `{path}` is not normalized",
                        q.id
                    )),
                    Err(reason) => {
                        problems.push(format!("question `{}` path `{path}`: {reason}", q.id))
                    }
                }
            }
        }
        for s in &self.scenario {
            if !ids.insert(s.id.as_str()) {
                problems.push(format!("duplicate id `{}`", s.id));
            }
            if let Some(prompt) = &s.prompt {
                check_text(&mut problems, "scenario", &s.id, prompt);
            }
            for file in &s.files {
                if normalize_relative_path(file).is_err() {
                    problems.push(format!("scenario `{}` file `{file}` is unsafe", s.id));
                }
            }
            if s.prompt.is_none() && s.files.is_empty() {
                problems.push(format!("scenario `{}` has neither prompt nor files", s.id));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(EvalError::Questions(problems.join("; ")))
        }
    }

    pub fn arm(&self, name: &str) -> Result<&Arm, EvalError> {
        self.arm
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| EvalError::UnknownArm(name.to_string()))
    }
}

// --- options ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitSelect {
    Dev,
    HeldOut,
    All,
}

impl SplitSelect {
    fn includes(self, split: Split) -> bool {
        matches!(
            (self, split),
            (Self::All, _) | (Self::Dev, Split::Dev) | (Self::HeldOut, Split::HeldOut)
        )
    }

    fn includes_held_out(self) -> bool {
        self != Self::Dev
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::HeldOut => "held_out",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub select: SplitSelect,
    /// Recall cutoff.
    pub k: usize,
    /// Replay held-out questions even when the gate differs from the frozen
    /// one. The report then says so.
    pub allow_unfrozen: bool,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            select: SplitSelect::Dev,
            k: DEFAULT_RECALL_K,
            allow_unfrozen: false,
        }
    }
}

// --- report -----------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub arm: String,
    /// Source revision the question expectations were written against.
    pub revision: String,
    pub corpus_fingerprint: String,
    /// The corpus differs from the one recorded at freeze time (`None`: nothing
    /// was recorded).
    pub corpus_drifted: Option<bool>,
    pub corpus_files: usize,
    pub index_format: u32,
    pub extractor_version: u32,
    pub gate_version: u32,
    pub gate_fingerprint: String,
    pub gate_frozen: bool,
    pub split: String,
    pub k: usize,
    pub retrieval: BTreeMap<String, RetrievalSummary>,
    pub delivery: BTreeMap<String, DeliverySummary>,
    pub maps: Vec<MapSize>,
    pub scenarios: Vec<ScenarioResult>,
    pub questions: Vec<QuestionResult>,
    /// Wall-clock measurements. Never part of a determinism comparison.
    pub timing: Option<Timing>,
}

impl ReplayReport {
    /// The report without wall-clock data; equal for equal inputs.
    pub fn deterministic(&self) -> Self {
        Self {
            timing: None,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RetrievalSummary {
    pub questions: usize,
    pub found_at_k: usize,
    pub recall_at_k: f64,
    pub mrr: f64,
    pub wrong_top_hit: usize,
    pub wrong_file_rate: f64,
    pub response_bytes: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeliverySummary {
    pub positives: usize,
    /// Positive prompts that received a card citing an expected path.
    pub cue_hits: usize,
    pub missed_cues: usize,
    pub negatives: usize,
    /// Negative prompts that received any gated card.
    pub false_positive_prompts: usize,
    pub false_positive_rate: f64,
    /// Cards that cite a path the question did not expect.
    pub wrong_cards: usize,
    /// Negatives that would receive the general entrypoint card on a first task.
    pub entrypoint_fallbacks: usize,
    pub cards: usize,
    pub added_tokens: usize,
    pub added_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapSize {
    pub scope: String,
    pub files_total: usize,
    pub files_shown: usize,
    pub bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetrievalResult {
    /// 0-based rank of the first hit citing an expected path.
    pub rank: Option<usize>,
    pub top_hit_wrong: bool,
    pub hits: usize,
    pub bytes: usize,
    /// `kind:path` of each returned hit, in rank order.
    pub top: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOutcome {
    CueHit,
    WrongCard,
    Miss,
    CorrectNone,
    FalsePositive,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuestionResult {
    pub id: String,
    pub split: Split,
    pub class: QueryClass,
    /// Per lane (`all`, `documentation`, `navigation`); empty for negatives.
    pub retrieval: BTreeMap<String, RetrievalResult>,
    pub outcome: DeliveryOutcome,
    /// `path > selector` of every gated card, in delivery order.
    pub cards: Vec<String>,
    pub tokens: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    pub id: String,
    pub kind: ScenarioKind,
    pub produced: usize,
    pub delivered: usize,
    pub refused: BTreeMap<String, usize>,
    pub bytes: usize,
    pub tokens: usize,
    pub within_limits: bool,
    pub duplicates: usize,
    /// Cards delivered on the second turn / new epoch, where one exists.
    pub second_turn_delivered: Option<usize>,
    pub ok: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshCost {
    pub millis: u128,
    pub selected: usize,
    pub hashed: usize,
    pub reused: usize,
    pub reparsed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Timing {
    pub cold_refresh: RefreshCost,
    pub warm_refresh: RefreshCost,
    pub query_micros_p50: u128,
    pub query_micros_max: u128,
    pub orientation_micros_p50: u128,
    pub orientation_micros_max: u128,
}

// --- arm materialization -----------------------------------------------------

/// A scratch checkout holding exactly the files an arm's manifest selects.
#[derive(Debug, Clone)]
pub struct Checkout {
    pub root: PathBuf,
    pub fingerprint: String,
    pub files: usize,
}

fn io_error(path: &Path, error: std::io::Error) -> EvalError {
    EvalError::Setup(format!("{}: {error}", path.display()))
}

/// Give a copied file a fixed, old modification time. A copy made seconds ago
/// would sit inside the racy-timestamp window and be re-hashed on every
/// refresh, which would measure the copy rather than the metadata fast path.
/// Identity comes from content hashes, so this changes no result.
fn settle(path: &Path) -> Result<(), EvalError> {
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(old))
        .map_err(|e| io_error(path, e))
}

/// Copy the files selected by `manifest_text` (evaluated against `source_root`)
/// into `dest`, write the manifest there, and fingerprint the result. `None`
/// produces an empty, unconfigured checkout.
pub fn materialize(
    source_root: &Path,
    manifest_text: Option<&str>,
    dest: &Path,
) -> Result<Checkout, EvalError> {
    fs::create_dir_all(dest.join(".git")).map_err(|e| io_error(dest, e))?;
    let mut hasher = Sha256::new();
    let Some(text) = manifest_text else {
        hasher.update(b"qmt-eval-corpus\0empty");
        return Ok(Checkout {
            root: dest.to_path_buf(),
            fingerprint: hex::encode(&hasher.finalize()[..16]),
            files: 0,
        });
    };
    let manifest = KnowledgeManifest::parse(text).map_err(|e| EvalError::Arm {
        path: MANIFEST_RELATIVE_PATH.into(),
        reason: e.to_string(),
    })?;
    let source = resolve_root(&RootRequest {
        host_root: Some(source_root),
        ..Default::default()
    })
    .map_err(|e| EvalError::Setup(e.to_string()))?;
    let discovery = discover(
        &source,
        &manifest,
        &DiscoveryOptions::from(&RepositoryKnowledgeSettings::default()),
    );
    hasher.update(b"qmt-eval-corpus\0");
    hasher.update(text.as_bytes());
    let mut files: Vec<_> = discovery.files.iter().collect();
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    for file in &files {
        let target = dest.join(&file.relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        let bytes =
            fs::read(&file.canonical_path).map_err(|e| io_error(&file.canonical_path, e))?;
        hasher.update([0u8]);
        hasher.update(file.relative_path.as_bytes());
        hasher.update([0u8]);
        hasher.update(Sha256::digest(&bytes));
        fs::write(&target, bytes).map_err(|e| io_error(&target, e))?;
        settle(&target)?;
    }
    let manifest_path = dest.join(MANIFEST_RELATIVE_PATH);
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
    }
    fs::write(&manifest_path, text).map_err(|e| io_error(&manifest_path, e))?;
    settle(&manifest_path)?;
    Ok(Checkout {
        root: dest.to_path_buf(),
        fingerprint: hex::encode(&hasher.finalize()[..16]),
        files: files.len(),
    })
}

/// The corpus fingerprint of `arm` against `source_root`. The corpus is copied
/// into `scratch`, which must be outside `source_root`.
pub fn corpus_fingerprint(
    source_root: &Path,
    arm: &Arm,
    scratch: &Path,
) -> Result<String, EvalError> {
    let text = arm_manifest(source_root, arm)?;
    Ok(materialize(source_root, text.as_deref(), &scratch.join("checkout"))?.fingerprint)
}

fn arm_manifest(source_root: &Path, arm: &Arm) -> Result<Option<String>, EvalError> {
    let Some(rel) = &arm.manifest else {
        return Ok(None);
    };
    let rel = normalize_relative_path(rel).map_err(|reason| EvalError::Arm {
        path: rel.clone(),
        reason: reason.into(),
    })?;
    fs::read_to_string(source_root.join(&rel))
        .map(Some)
        .map_err(|e| EvalError::Arm {
            path: rel,
            reason: e.to_string(),
        })
}

// --- replay -------------------------------------------------------------------

fn delivery_settings() -> RepositoryKnowledgeDeliverySettings {
    RepositoryKnowledgeDeliverySettings {
        orientation: true,
        index_context: true,
        ..Default::default()
    }
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        round4(n as f64 / d as f64)
    }
}

fn percentile50(values: &mut [u128]) -> u128 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0)
}

fn orientation_content_len(content: String) -> usize {
    serialized_len(&HookContextContribution {
        event_name: USER_PROMPT_EVENT.to_string(),
        handler_id: ORIENTATION_HANDLER_ID.to_string(),
        tool_use_id: None,
        content,
    })
}

fn cost(report: &super::reconcile::ReconcileReport, started: Instant) -> RefreshCost {
    RefreshCost {
        millis: started.elapsed().as_millis(),
        selected: report.selected,
        hashed: report.hashed,
        reused: report.reused,
        reparsed: report.reparsed,
    }
}

/// Evaluate one arm end to end inside `scratch` (created if needed). `scratch`
/// must be outside `source_root`; it receives the copied corpus and a fresh
/// cache, so two runs never share index state.
pub fn run(
    source_root: &Path,
    set: &QuestionSet,
    arm_name: &str,
    options: &ReplayOptions,
    scratch: &Path,
) -> Result<ReplayReport, EvalError> {
    let arm = set.arm(arm_name)?;
    let gate_frozen =
        set.gate_version == GATE_VERSION && set.gate_fingerprint == gate_fingerprint();
    if options.select.includes_held_out() && !gate_frozen && !options.allow_unfrozen {
        return Err(EvalError::NotFrozen(format!(
            "the gate differs from the one recorded in the question set (recorded version {} \
             `{}`, compiled version {GATE_VERSION} `{}`); re-freeze the set or pass \
             --allow-unfrozen to label the run",
            set.gate_version,
            set.gate_fingerprint,
            gate_fingerprint()
        )));
    }
    let text = arm_manifest(source_root, arm)?;
    let checkout = materialize(source_root, text.as_deref(), &scratch.join("checkout"))?;
    let root = resolve_root(&RootRequest {
        host_root: Some(&checkout.root),
        ..Default::default()
    })
    .map_err(|e| EvalError::Setup(e.to_string()))?;
    let cache = scratch.join("cache");
    fs::create_dir_all(&cache).map_err(|e| io_error(&cache, e))?;
    let service = RepositoryService::open(
        root,
        &cache.join("index.sqlite"),
        ReconcileOptions::from(&RepositoryKnowledgeSettings::default()),
        RefreshPolicy::default(),
    )
    .map_err(|e| EvalError::Setup(e.to_string()))?;

    let started = Instant::now();
    let cold = service
        .refresh()
        .map_err(|e| EvalError::Setup(e.to_string()))?;
    let cold_cost = cost(&cold, started);
    let started = Instant::now();
    let warm = service
        .refresh()
        .map_err(|e| EvalError::Setup(e.to_string()))?;
    let warm_cost = cost(&warm, started);

    let mut report = replay(&service, &checkout, set, arm_name, options)?;
    report.gate_frozen = gate_frozen;
    report.corpus_drifted = set
        .frozen_corpus
        .get(arm_name)
        .map(|frozen| *frozen != checkout.fingerprint);
    if let Some(timing) = report.timing.as_mut() {
        timing.cold_refresh = cold_cost;
        timing.warm_refresh = warm_cost;
    }
    Ok(report)
}

/// Replay `set` against an already indexed `service`.
pub fn replay(
    service: &RepositoryService,
    checkout: &Checkout,
    set: &QuestionSet,
    arm_name: &str,
    options: &ReplayOptions,
) -> Result<ReplayReport, EvalError> {
    let settings = delivery_settings();
    let max_cards = settings.effective_max_cards();
    let mut retrieval: BTreeMap<String, RetrievalSummary> = BTreeMap::new();
    let mut delivery: BTreeMap<String, DeliverySummary> = BTreeMap::new();
    let mut results = Vec::new();
    let mut query_times = Vec::new();
    let mut orient_times = Vec::new();

    for q in set
        .question
        .iter()
        .filter(|q| options.select.includes(q.split))
    {
        let expect: BTreeSet<&str> = q.expect.iter().map(String::as_str).collect();
        let class = q.class.as_str().to_string();

        // Retrieval per lane: positives only; a negative has nothing to find.
        let mut retrieved: BTreeMap<String, RetrievalResult> = BTreeMap::new();
        if q.is_positive() {
            for lane in RETRIEVAL_LANES {
                let started = Instant::now();
                let response = service.query(&QueryRequest {
                    question: q.text.clone(),
                    lane,
                    path: None,
                    limit: Some(options.k),
                });
                query_times.push(started.elapsed().as_micros());
                let summary = retrieval
                    .entry(format!("{}/{class}", lane.as_str()))
                    .or_default();
                summary.questions += 1;
                let result = match response {
                    Ok(response) => {
                        let bytes = serde_json::to_string(
                            &response
                                .hits
                                .iter()
                                .map(|h| json!([h.id, h.path, h.selector, h.title, h.excerpt]))
                                .collect::<Vec<_>>(),
                        )
                        .map(|s| s.len())
                        .unwrap_or(0);
                        let rank = response
                            .hits
                            .iter()
                            .position(|h| expect.contains(h.path.as_str()));
                        let top_wrong = response
                            .hits
                            .first()
                            .is_some_and(|h| !expect.contains(h.path.as_str()));
                        summary.response_bytes += bytes;
                        if let Some(rank) = rank {
                            summary.found_at_k += 1;
                            summary.mrr += 1.0 / (rank as f64 + 1.0);
                        }
                        summary.wrong_top_hit += usize::from(top_wrong);
                        RetrievalResult {
                            rank,
                            top_hit_wrong: top_wrong,
                            hits: response.hits.len(),
                            bytes,
                            top: response
                                .hits
                                .iter()
                                .map(|h| format!("{:?}:{}", h.kind, h.path))
                                .collect(),
                            error: None,
                        }
                    }
                    Err(error) => {
                        summary.errors += 1;
                        RetrievalResult {
                            rank: None,
                            top_hit_wrong: false,
                            hits: 0,
                            bytes: 0,
                            top: Vec::new(),
                            error: Some(error.code().to_string()),
                        }
                    }
                };
                retrieved.insert(lane.as_str().to_string(), result);
            }
        }

        // Orientation: the gate alone, no entrypoint fallback.
        let started = Instant::now();
        let outcome = service
            .orient(&OrientationRequest {
                prompt: q.text.clone(),
                allow_entrypoint_fallback: false,
                max_cards,
            })
            .map_err(|e| EvalError::Setup(e.to_string()))?;
        orient_times.push(started.elapsed().as_micros());
        let gated: Vec<_> = outcome
            .cards
            .iter()
            .filter(|c| matches!(c.origin, CardOrigin::Gated(_)))
            .collect();
        let mut tokens = 0;
        let mut bytes = 0;
        let mut cards = Vec::new();
        let mut wrong = 0;
        let mut right = 0;
        for card in &gated {
            let len = orientation_content_len(render_orientation_card(card, true));
            bytes += len;
            tokens += estimate_tokens(len);
            cards.push(match &card.selector {
                Some(s) => format!("{} > {s}", card.doc_path),
                None => card.doc_path.clone(),
            });
            if expect.contains(card.doc_path.as_str()) {
                right += 1;
            } else {
                wrong += 1;
            }
        }
        let summary = delivery.entry(class.clone()).or_default();
        summary.cards += gated.len();
        summary.added_tokens += tokens;
        summary.added_bytes += bytes;
        summary.wrong_cards += wrong;
        let outcome_kind = if q.is_positive() {
            summary.positives += 1;
            if right > 0 {
                summary.cue_hits += 1;
                DeliveryOutcome::CueHit
            } else {
                summary.missed_cues += 1;
                if gated.is_empty() {
                    DeliveryOutcome::Miss
                } else {
                    DeliveryOutcome::WrongCard
                }
            }
        } else {
            summary.negatives += 1;
            let fallback = service
                .orient(&OrientationRequest {
                    prompt: q.text.clone(),
                    allow_entrypoint_fallback: true,
                    max_cards,
                })
                .map_err(|e| EvalError::Setup(e.to_string()))?;
            if fallback
                .cards
                .iter()
                .any(|c| matches!(c.origin, CardOrigin::Entrypoint))
            {
                summary.entrypoint_fallbacks += 1;
            }
            if gated.is_empty() {
                DeliveryOutcome::CorrectNone
            } else {
                summary.false_positive_prompts += 1;
                DeliveryOutcome::FalsePositive
            }
        };
        results.push(QuestionResult {
            id: q.id.clone(),
            split: q.split,
            class: q.class,
            retrieval: retrieved,
            outcome: outcome_kind,
            cards,
            tokens,
        });
    }

    for summary in retrieval.values_mut() {
        summary.recall_at_k = ratio(summary.found_at_k, summary.questions);
        summary.mrr = round4(summary.mrr / summary.questions.max(1) as f64);
        summary.wrong_file_rate = ratio(summary.wrong_top_hit, summary.questions);
    }
    for summary in delivery.values_mut() {
        summary.false_positive_rate = ratio(summary.false_positive_prompts, summary.negatives);
    }
    // One overall row per lane, keyed `<lane>/all`.
    for lane in RETRIEVAL_LANES {
        let prefix = format!("{}/", lane.as_str());
        let mut total = RetrievalSummary::default();
        for (key, s) in retrieval.iter().filter(|(k, _)| k.starts_with(&prefix)) {
            let _ = key;
            total.questions += s.questions;
            total.found_at_k += s.found_at_k;
            total.wrong_top_hit += s.wrong_top_hit;
            total.response_bytes += s.response_bytes;
            total.errors += s.errors;
            total.mrr += s.mrr * s.questions as f64;
        }
        if total.questions == 0 {
            continue;
        }
        total.recall_at_k = ratio(total.found_at_k, total.questions);
        total.wrong_file_rate = ratio(total.wrong_top_hit, total.questions);
        total.mrr = round4(total.mrr / total.questions as f64);
        retrieval.insert(format!("{prefix}all"), total);
    }
    let mut all_delivery = DeliverySummary::default();
    for s in delivery.values() {
        all_delivery.positives += s.positives;
        all_delivery.cue_hits += s.cue_hits;
        all_delivery.missed_cues += s.missed_cues;
        all_delivery.negatives += s.negatives;
        all_delivery.false_positive_prompts += s.false_positive_prompts;
        all_delivery.wrong_cards += s.wrong_cards;
        all_delivery.entrypoint_fallbacks += s.entrypoint_fallbacks;
        all_delivery.cards += s.cards;
        all_delivery.added_tokens += s.added_tokens;
        all_delivery.added_bytes += s.added_bytes;
    }
    all_delivery.false_positive_rate =
        ratio(all_delivery.false_positive_prompts, all_delivery.negatives);
    delivery.insert("all".into(), all_delivery);

    let scenarios = set
        .scenario
        .iter()
        .filter(|s| options.select.includes(s.split))
        .map(|s| run_scenario(service, checkout, s, &settings))
        .collect();

    let maps = [
        "crates/agent/src/agent/execution",
        "crates/agent/src/session",
        "crates/agent/src/tools",
    ]
    .iter()
    .filter_map(|scope| {
        service
            .map(&MapRequest {
                path: Some((*scope).to_string()),
                max_bytes: None,
            })
            .ok()
            .map(|out| MapSize {
                scope: (*scope).to_string(),
                files_total: out.map.files_total,
                files_shown: out.map.files_shown,
                bytes: out.map.text.len(),
                truncated: out.map.truncated,
            })
    })
    .collect();

    let zero = RefreshCost {
        millis: 0,
        selected: 0,
        hashed: 0,
        reused: 0,
        reparsed: 0,
    };
    Ok(ReplayReport {
        arm: arm_name.to_string(),
        revision: set.revision.clone(),
        corpus_fingerprint: checkout.fingerprint.clone(),
        corpus_drifted: None,
        corpus_files: checkout.files,
        index_format: SCHEMA_VERSION,
        extractor_version: CODE_EXTRACTOR_VERSION,
        gate_version: GATE_VERSION,
        gate_fingerprint: gate_fingerprint(),
        gate_frozen: true,
        split: options.select.as_str().to_string(),
        k: options.k,
        retrieval,
        delivery,
        maps,
        scenarios,
        questions: results,
        timing: Some(Timing {
            cold_refresh: zero.clone(),
            warm_refresh: zero,
            query_micros_max: query_times.iter().copied().max().unwrap_or(0),
            query_micros_p50: percentile50(&mut query_times),
            orientation_micros_max: orient_times.iter().copied().max().unwrap_or(0),
            orientation_micros_p50: percentile50(&mut orient_times),
        }),
    })
}

// --- scenarios -----------------------------------------------------------------

fn turn(
    service: &RepositoryService,
    checkout: &Checkout,
    scenario: &Scenario,
    settings: &RepositoryKnowledgeDeliverySettings,
    existing: &[HookContextContribution],
) -> Vec<HookContextContribution> {
    let max_cards = settings.effective_max_cards();
    let mut out = Vec::new();
    if let Some(prompt) = &scenario.prompt {
        out.extend(orientation_contributions(
            service,
            PromptInput {
                prompt,
                first_task: false,
                max_cards,
                fetch_available: true,
            },
            existing,
        ));
    }
    for (index, file) in scenario.files.iter().enumerate() {
        let args = json!({ "path": file });
        let id = format!("call-{}", index + 1);
        out.extend(post_index_contributions(
            service,
            &IndexCall {
                tool_name: INDEX_TOOL_NAME,
                tool_source: BUILTIN_TOOL_SOURCE,
                execution_is_error: false,
                result_is_error: false,
                output_suppressed: false,
                tool_use_id: &id,
                args: &args,
                cwd: Some(&checkout.root),
            },
            max_cards,
            true,
            existing,
        ));
    }
    out
}

fn allocate(
    budget: &mut DeliveryBudget,
    contributions: Vec<HookContextContribution>,
) -> (Vec<HookContextContribution>, BTreeMap<String, usize>) {
    let (kept, refused) = budget.allocate(contributions);
    let mut reasons = BTreeMap::new();
    for r in refused {
        *reasons.entry(reason_label(r).to_string()).or_insert(0) += 1;
    }
    (kept, reasons)
}

fn run_scenario(
    service: &RepositoryService,
    checkout: &Checkout,
    scenario: &Scenario,
    settings: &RepositoryKnowledgeDeliverySettings,
) -> ScenarioResult {
    let mut budget = DeliveryBudget::new(settings);
    let produced = turn(service, checkout, scenario, settings, &[]);
    let produced_count = produced.len();
    let (kept, refused) = allocate(&mut budget, produced);
    let (cards, tokens, bytes) = budget.used();
    let keys: Vec<String> = kept
        .iter()
        .filter_map(|c| parse_mark(&c.content).map(|m| m.key()))
        .collect();
    let unique: BTreeSet<&String> = keys.iter().collect();
    let duplicates = keys.len() - unique.len();
    let within_limits = cards <= settings.effective_max_cards()
        && tokens <= settings.effective_max_tokens()
        && bytes <= settings.effective_max_bytes();

    let mut second = None;
    let mut ok = within_limits && duplicates == 0;
    let mut note = String::new();
    match scenario.kind {
        ScenarioKind::Parallel => {
            if kept.is_empty() {
                ok = false;
                note = "no card was produced".into();
            }
        }
        ScenarioKind::Repeat => {
            // Same turn again with the first turn's cards in effective history.
            let mut next = DeliveryBudget::new(settings);
            next.restore(kept.iter().map(|c| c.content.as_str()), 0);
            let again = turn(service, checkout, scenario, settings, &kept);
            let (kept_again, _) = allocate(&mut next, again);
            second = Some(kept_again.len());
            // A card omitted by the first turn's budget was never delivered, so
            // it may legitimately appear now; a delivered one must not.
            let resent = kept_again
                .iter()
                .filter_map(|c| parse_mark(&c.content).map(|m| m.key()))
                .filter(|key| keys.contains(key))
                .count();
            if kept.is_empty() {
                ok = false;
                note = "first turn produced no card, so suppression is untested".into();
            } else if resent > 0 {
                ok = false;
                note = "an unchanged card was delivered again".into();
            }
        }
        ScenarioKind::Compaction => {
            // The old cards left effective history: a new epoch re-selects from
            // the current corpus.
            let mut next = DeliveryBudget::new(settings);
            next.restore(std::iter::empty::<&str>(), 1);
            let again = turn(service, checkout, scenario, settings, &[]);
            let (kept_again, _) = allocate(&mut next, again);
            second = Some(kept_again.len());
            let stamped = kept_again
                .iter()
                .all(|c| parse_mark(&c.content).and_then(|m| m.epoch) == Some(1));
            if kept.is_empty() || kept_again.is_empty() {
                ok = false;
                note = "nothing was (re)delivered".into();
            } else if !stamped {
                ok = false;
                note = "re-delivered cards do not carry the new epoch".into();
            }
        }
    }
    ScenarioResult {
        id: scenario.id.clone(),
        kind: scenario.kind,
        produced: produced_count,
        delivered: kept.len(),
        refused,
        bytes,
        tokens,
        within_limits,
        duplicates,
        second_turn_delivered: second,
        ok,
        note,
    }
}

// --- optional model-backed trials ---------------------------------------------
//
// Default tests and `replay` never need a model. A model-backed comparison is
// run by whatever driver the developer chooses; it records one `TrialRecord`
// per run into a `TrialFile`, and `report_trials` turns that file into a
// report. Cells with no records are labeled `not_run`; nothing is inferred
// for them.

/// Supported trial-file format.
pub const TRIAL_FILE_VERSION: u32 = 1;
/// Default number of repeated trials per question and cell.
pub const DEFAULT_REPEATS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// No repository knowledge at all (the baseline arm).
    None,
    /// `repo_knowledge_*` tools only.
    PullOnly,
    /// Pull plus post-index enrichment.
    IndexEnriched,
    /// Pull plus index enrichment plus prompt orientation.
    Orientation,
}

impl DeliveryMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PullOnly => "pull_only",
            Self::IndexEnriched => "index_enriched",
            Self::Orientation => "orientation",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialRecord {
    pub arm: String,
    pub mode: DeliveryMode,
    pub question: String,
    /// 1-based repeat index.
    pub trial: u32,
    pub model: String,
    /// Free-form identity of the agent configuration (profile, tools, limits).
    pub configuration: String,
    pub tool_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 1-based index of the first tool call that inspected an expected path;
    /// absent when none did.
    #[serde(default)]
    pub first_relevant_inspection: Option<u32>,
    /// Task correctness as judged by the reviewer; absent when not judged.
    #[serde(default)]
    pub correct: Option<bool>,
}

/// Documentation effort for one arm, recorded by the people who did the work.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffortRecord {
    pub arm: String,
    pub authoring_minutes: f64,
    pub maintenance_minutes: f64,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialFile {
    pub version: u32,
    #[serde(default)]
    pub efforts: Vec<EffortRecord>,
    #[serde(default)]
    pub trials: Vec<TrialRecord>,
}

impl TrialFile {
    pub fn parse(text: &str) -> Result<Self, EvalError> {
        let file: Self =
            serde_json::from_str(text).map_err(|e| EvalError::Questions(e.to_string()))?;
        if file.version != TRIAL_FILE_VERSION {
            return Err(EvalError::Questions(format!(
                "unsupported trial file version {}",
                file.version
            )));
        }
        Ok(file)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CellPlan {
    pub arm: String,
    pub mode: DeliveryMode,
    /// Questions in the selected split times repeats.
    pub planned_trials: usize,
}

/// The comparison matrix: the baseline once, every corpus arm in each delivery
/// mode. Corpus effects compare arms at one mode; delivery effects compare
/// modes within one arm, so the two are never confused.
pub fn plan_trials(set: &QuestionSet, repeats: u32, select: SplitSelect) -> Vec<CellPlan> {
    let questions = set
        .question
        .iter()
        .filter(|q| select.includes(q.split))
        .count();
    let planned = questions * repeats as usize;
    let mut cells = Vec::new();
    for arm in &set.arm {
        let modes: &[DeliveryMode] = if arm.manifest.is_some() {
            &[
                DeliveryMode::PullOnly,
                DeliveryMode::IndexEnriched,
                DeliveryMode::Orientation,
            ]
        } else {
            &[DeliveryMode::None]
        };
        for mode in modes {
            cells.push(CellPlan {
                arm: arm.name.clone(),
                mode: *mode,
                planned_trials: planned,
            });
        }
    }
    cells
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    NotRun,
    Partial,
    Complete,
}

#[derive(Debug, Clone, Serialize)]
pub struct CellReport {
    pub arm: String,
    pub mode: DeliveryMode,
    pub planned: usize,
    pub recorded: usize,
    pub status: CellStatus,
    pub models: Vec<String>,
    pub configurations: Vec<String>,
    pub mean_tool_calls: Option<f64>,
    pub mean_input_tokens: Option<f64>,
    pub mean_output_tokens: Option<f64>,
    pub mean_first_relevant_inspection: Option<f64>,
    /// Trials in which no expected path was ever inspected.
    pub never_inspected: usize,
    pub judged: usize,
    pub correct: usize,
    pub correctness_rate: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    /// `corpus` (different arm, same mode) or `delivery` (same arm, different
    /// mode).
    pub kind: &'static str,
    pub from: String,
    pub to: String,
    pub tool_calls_delta: Option<f64>,
    pub tokens_delta: Option<f64>,
    pub correctness_delta: Option<f64>,
    /// Both cells complete, fully judged, and run with one model and
    /// configuration.
    pub conclusive: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrialReport {
    pub revision: String,
    pub gate_version: u32,
    pub gate_fingerprint: String,
    pub split: String,
    pub repeats: u32,
    pub cells: Vec<CellReport>,
    pub comparisons: Vec<Comparison>,
    pub efforts: Vec<EffortRecord>,
    /// Inconsistencies in the trial file (unknown arm or question, duplicates).
    pub problems: Vec<String>,
    pub caveats: Vec<String>,
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values.fold((0.0, 0usize), |(s, c), v| (s + v, c + 1));
    (count > 0).then(|| round4(sum / count as f64))
}

fn cell_label(arm: &str, mode: DeliveryMode) -> String {
    format!("{arm}/{}", mode.as_str())
}

/// Summarize recorded model-backed trials against the planned matrix.
pub fn report_trials(
    set: &QuestionSet,
    file: &TrialFile,
    repeats: u32,
    select: SplitSelect,
) -> TrialReport {
    let plan = plan_trials(set, repeats, select);
    let questions: BTreeSet<&str> = set
        .question
        .iter()
        .filter(|q| select.includes(q.split))
        .map(|q| q.id.as_str())
        .collect();
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    let mut by_cell: BTreeMap<(String, DeliveryMode), Vec<&TrialRecord>> = BTreeMap::new();
    for record in &file.trials {
        let cell = (record.arm.clone(), record.mode);
        if !plan
            .iter()
            .any(|c| c.arm == record.arm && c.mode == record.mode)
        {
            problems.push(format!(
                "trial for {} is outside the planned matrix",
                cell_label(&record.arm, record.mode)
            ));
            continue;
        }
        if !questions.contains(record.question.as_str()) {
            problems.push(format!(
                "trial question `{}` is not in the {} split",
                record.question,
                select.as_str()
            ));
            continue;
        }
        if record.trial == 0 || record.trial > repeats {
            problems.push(format!(
                "trial {} of `{}` is outside 1..={repeats}",
                record.trial, record.question
            ));
            continue;
        }
        if !seen.insert((cell.clone(), record.question.clone(), record.trial)) {
            problems.push(format!(
                "duplicate trial {} of `{}` in {}",
                record.trial,
                record.question,
                cell_label(&record.arm, record.mode)
            ));
            continue;
        }
        by_cell.entry(cell).or_default().push(record);
    }
    for effort in &file.efforts {
        if set.arm(&effort.arm).is_err() {
            problems.push(format!("effort recorded for unknown arm `{}`", effort.arm));
        }
    }

    let cells: Vec<CellReport> = plan
        .iter()
        .map(|cell| {
            let records = by_cell
                .get(&(cell.arm.clone(), cell.mode))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let judged: Vec<bool> = records.iter().filter_map(|r| r.correct).collect();
            let correct = judged.iter().filter(|c| **c).count();
            let status = if records.is_empty() {
                CellStatus::NotRun
            } else if records.len() >= cell.planned_trials {
                CellStatus::Complete
            } else {
                CellStatus::Partial
            };
            CellReport {
                arm: cell.arm.clone(),
                mode: cell.mode,
                planned: cell.planned_trials,
                recorded: records.len(),
                status,
                models: records
                    .iter()
                    .map(|r| r.model.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                configurations: records
                    .iter()
                    .map(|r| r.configuration.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                mean_tool_calls: mean(records.iter().map(|r| f64::from(r.tool_calls))),
                mean_input_tokens: mean(records.iter().map(|r| r.input_tokens as f64)),
                mean_output_tokens: mean(records.iter().map(|r| r.output_tokens as f64)),
                mean_first_relevant_inspection: mean(
                    records
                        .iter()
                        .filter_map(|r| r.first_relevant_inspection.map(f64::from)),
                ),
                never_inspected: records
                    .iter()
                    .filter(|r| r.first_relevant_inspection.is_none())
                    .count(),
                judged: judged.len(),
                correct,
                correctness_rate: (!judged.is_empty()).then(|| ratio(correct, judged.len())),
            }
        })
        .collect();

    let find =
        |arm: &str, mode: DeliveryMode| cells.iter().find(|c| c.arm == arm && c.mode == mode);
    let compare = |kind: &'static str, from: &CellReport, to: &CellReport| {
        let diff = |a: Option<f64>, b: Option<f64>| a.zip(b).map(|(a, b)| round4(b - a));
        let tokens = |c: &CellReport| {
            c.mean_input_tokens
                .zip(c.mean_output_tokens)
                .map(|(i, o)| i + o)
        };
        let like_for_like = from.models == to.models
            && from.configurations == to.configurations
            && from.models.len() == 1
            && from.configurations.len() == 1;
        let complete = from.status == CellStatus::Complete && to.status == CellStatus::Complete;
        let judged = from.judged == from.recorded && to.judged == to.recorded;
        let conclusive = like_for_like && complete && judged;
        let correctness_delta = diff(from.correctness_rate, to.correctness_rate);
        let mut note = String::new();
        if from.status == CellStatus::NotRun || to.status == CellStatus::NotRun {
            note.push_str("a cell was not run; nothing can be concluded. ");
        } else if !conclusive {
            note.push_str(
                "not conclusive: needs complete, fully judged cells run with one model and \
                 configuration. ",
            );
        }
        if correctness_delta.is_some_and(|d| d < 0.0) {
            note.push_str("correctness fell; fewer tool calls would not be an improvement. ");
        }
        note.push_str("No significance test is applied; treat small trial counts as indicative.");
        Comparison {
            kind,
            from: cell_label(&from.arm, from.mode),
            to: cell_label(&to.arm, to.mode),
            tool_calls_delta: diff(from.mean_tool_calls, to.mean_tool_calls),
            tokens_delta: diff(tokens(from), tokens(to)),
            correctness_delta,
            conclusive,
            note,
        }
    };
    let mut comparisons = Vec::new();
    let baseline = set.arm.iter().find(|a| a.manifest.is_none());
    let corpus_arms: Vec<&Arm> = set.arm.iter().filter(|a| a.manifest.is_some()).collect();
    if let Some(baseline) = baseline
        && let Some(from) = find(&baseline.name, DeliveryMode::None)
    {
        for arm in &corpus_arms {
            if let Some(to) = find(&arm.name, DeliveryMode::PullOnly) {
                comparisons.push(compare("corpus", from, to));
            }
        }
    }
    for (i, a) in corpus_arms.iter().enumerate() {
        for b in &corpus_arms[i + 1..] {
            if let (Some(from), Some(to)) = (
                find(&a.name, DeliveryMode::PullOnly),
                find(&b.name, DeliveryMode::PullOnly),
            ) {
                comparisons.push(compare("corpus", from, to));
            }
        }
        for pair in [
            (DeliveryMode::PullOnly, DeliveryMode::IndexEnriched),
            (DeliveryMode::IndexEnriched, DeliveryMode::Orientation),
        ] {
            if let (Some(from), Some(to)) = (find(&a.name, pair.0), find(&a.name, pair.1)) {
                comparisons.push(compare("delivery", from, to));
            }
        }
    }

    let mut caveats = vec![
        "Fewer tool calls or tokens alone is not an improvement; correctness is judged separately."
            .to_string(),
        "Corpus effects (arm vs arm at one delivery mode) and delivery effects (mode vs mode in \
         one arm) are reported separately."
            .to_string(),
    ];
    let not_run: Vec<String> = cells
        .iter()
        .filter(|c| c.status == CellStatus::NotRun)
        .map(|c| cell_label(&c.arm, c.mode))
        .collect();
    if !not_run.is_empty() {
        caveats.push(format!("Not run: {}.", not_run.join(", ")));
    }
    if file.efforts.is_empty() {
        caveats.push("No authoring/maintenance effort was recorded.".to_string());
    }
    TrialReport {
        revision: set.revision.clone(),
        gate_version: GATE_VERSION,
        gate_fingerprint: gate_fingerprint(),
        split: select.as_str().to_string(),
        repeats,
        cells,
        comparisons,
        efforts: file.efforts.clone(),
        problems,
        caveats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\ntitle: Scheduler notes\ndescription: How the scheduler dispatches queued tasks.\nknowledge_sources: [\"src/sched/queue.rs\"]\n---\n# Scheduler\n## Dispatch\nThe scheduler dispatches queued tasks to workers through the dispatch loop.\n";

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join(".qmt")).unwrap();
        fs::create_dir_all(repo.join("src/sched")).unwrap();
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(
            repo.join(".qmt/knowledge.toml"),
            "version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n",
        )
        .unwrap();
        fs::write(repo.join("docs/sched.md"), DOC).unwrap();
        fs::write(repo.join("src/sched/queue.rs"), "pub fn dispatch() {}\n").unwrap();
        tmp
    }

    fn set(gate: &str) -> QuestionSet {
        QuestionSet::parse(&format!(
            r#"
version = 1
revision = "test"
gate_version = 1
gate_fingerprint = "{gate}"
[[arm]]
name = "curated"
manifest = ".qmt/knowledge.toml"
[[arm]]
name = "baseline"
[[question]]
id = "p1"
split = "held_out"
class = "conceptual"
text = "how does the scheduler dispatch queued tasks to workers"
expect = ["docs/sched.md"]
[[question]]
id = "n1"
split = "held_out"
class = "negative"
text = "fix the typo in the changelog"
[[scenario]]
id = "s-repeat"
split = "held_out"
kind = "repeat"
prompt = "how does the scheduler dispatch queued tasks to workers"
files = ["src/sched/queue.rs"]
[[scenario]]
id = "s-compact"
split = "held_out"
kind = "compaction"
files = ["src/sched/queue.rs"]
"#
        ))
        .unwrap()
    }

    #[test]
    fn replay_is_deterministic_across_fresh_caches_and_scores_cues_and_negatives() {
        let tmp = fixture();
        let repo = tmp.path().join("repo");
        let set = set(&gate_fingerprint());
        let options = ReplayOptions {
            select: SplitSelect::HeldOut,
            ..Default::default()
        };
        let a = run(&repo, &set, "curated", &options, &tmp.path().join("a")).unwrap();
        let b = run(&repo, &set, "curated", &options, &tmp.path().join("b")).unwrap();
        assert_eq!(
            serde_json::to_value(a.deterministic()).unwrap(),
            serde_json::to_value(b.deterministic()).unwrap()
        );
        assert_eq!(a.retrieval["all/all"].recall_at_k, 1.0);
        assert_eq!(a.delivery["all"].cue_hits, 1);
        assert_eq!(a.delivery["all"].false_positive_prompts, 0);
        assert!(a.scenarios.iter().all(|s| s.ok), "{:?}", a.scenarios);
        let timing = a.timing.unwrap();
        assert!(timing.cold_refresh.hashed > 0);
        assert_eq!(
            timing.warm_refresh.hashed, 0,
            "warm refresh reads nothing unchanged"
        );
    }

    #[test]
    fn baseline_arm_is_an_empty_corpus_that_finds_nothing() {
        let tmp = fixture();
        let set = set(&gate_fingerprint());
        let report = run(
            &tmp.path().join("repo"),
            &set,
            "baseline",
            &ReplayOptions {
                select: SplitSelect::HeldOut,
                ..Default::default()
            },
            &tmp.path().join("s"),
        )
        .unwrap();
        assert_eq!(report.corpus_files, 0);
        assert_eq!(report.delivery["all"].cue_hits, 0);
    }

    #[test]
    fn held_out_replay_requires_the_frozen_gate() {
        let tmp = fixture();
        let set = set("not-the-gate");
        let held_out = ReplayOptions {
            select: SplitSelect::HeldOut,
            ..Default::default()
        };
        let refused = run(
            &tmp.path().join("repo"),
            &set,
            "curated",
            &held_out,
            &tmp.path().join("s"),
        );
        assert!(matches!(refused, Err(EvalError::NotFrozen(_))));
        let labeled = run(
            &tmp.path().join("repo"),
            &set,
            "curated",
            &ReplayOptions {
                allow_unfrozen: true,
                ..held_out
            },
            &tmp.path().join("t"),
        )
        .unwrap();
        assert!(!labeled.gate_frozen);
    }

    #[test]
    fn trial_reports_label_unrun_cells_and_refuse_unjudged_conclusions() {
        let set = set(&gate_fingerprint());
        let record = |arm: &str, mode, correct| TrialRecord {
            arm: arm.into(),
            mode,
            question: "p1".into(),
            trial: 1,
            model: "m".into(),
            configuration: "c".into(),
            tool_calls: 4,
            input_tokens: 100,
            output_tokens: 10,
            first_relevant_inspection: Some(2),
            correct,
        };
        let file = TrialFile {
            version: TRIAL_FILE_VERSION,
            efforts: vec![],
            trials: vec![
                record("baseline", DeliveryMode::None, None),
                record("curated", DeliveryMode::PullOnly, Some(true)),
                record("curated", DeliveryMode::PullOnly, Some(true)),
                record("curated", DeliveryMode::Orientation, Some(false)),
                record("ghost", DeliveryMode::PullOnly, None),
            ],
        };
        let report = report_trials(&set, &file, 1, SplitSelect::HeldOut);
        let cell = |arm: &str, mode| {
            report
                .cells
                .iter()
                .find(|c| c.arm == arm && c.mode == mode)
                .unwrap()
        };
        assert_eq!(
            cell("curated", DeliveryMode::IndexEnriched).status,
            CellStatus::NotRun
        );
        assert_eq!(
            cell("curated", DeliveryMode::PullOnly).recorded,
            1,
            "duplicate dropped"
        );
        assert_eq!(report.problems.len(), 2, "{:?}", report.problems);
        let corpus = report
            .comparisons
            .iter()
            .find(|c| c.kind == "corpus")
            .unwrap();
        assert!(!corpus.conclusive, "the baseline trial was never judged");
        assert!(report.caveats.iter().any(|c| c.starts_with("Not run")));
        assert!(
            report
                .comparisons
                .iter()
                .any(|c| c.kind == "delivery" && c.note.contains("not run"))
        );
    }

    #[test]
    fn fixtures_with_paths_secrets_or_transcripts_are_rejected() {
        for bad in [
            "text = \"look at /Users/someone/project\"",
            "text = \"use api_key abc\"",
            "text = \"see tool_use_id in the log\"",
        ] {
            let source = format!(
                "version = 1\nrevision = \"r\"\ngate_version = 1\ngate_fingerprint = \"x\"\n[[question]]\nid = \"q\"\nsplit = \"dev\"\nclass = \"negative\"\n{bad}\n"
            );
            assert!(QuestionSet::parse(&source).is_err(), "{bad}");
        }
        let escaping = "version = 1\nrevision = \"r\"\ngate_version = 1\ngate_fingerprint = \"x\"\n[[question]]\nid = \"q\"\nsplit = \"dev\"\nclass = \"navigation\"\ntext = \"find it\"\nexpect = [\"../outside.rs\"]\n";
        assert!(QuestionSet::parse(escaping).is_err());
    }
}
