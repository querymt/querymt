//! Adapters mapping protocol `.agents/memories/*.md` documents into knowledge.
//!
//! Protocol memories are declarative Markdown documents. This module converts a
//! resolved [`DotagentsMemory`] into a **neutral** [`DotagentsMemoryIngest`]
//! plan — the field values a later reconciliation phase would hand to the
//! knowledge store — without touching storage, starting processes, or importing
//! anything. Reconciliation (tasks 5.3–5.4) consumes the plan.
//!
//! Mapping rules:
//!
//! - the memory body is the primary text; when the body is empty the explicit
//!   `content` metadata is used instead (parsing guarantees at least one is
//!   present);
//! - the title, or a deterministic fallback derived from the text, becomes the
//!   summary;
//! - `tags` become knowledge topics;
//! - the protocol importance value is normalized onto the knowledge store's
//!   `0.0..=1.0` score, with unknown spellings reported rather than silently
//!   mis-scored;
//! - the source key is derived from the protocol layer and memory ID so
//!   unchanged reloads reconcile to the same entry.
//!
//! Everything that cannot be represented is reported as a source-aware
//! diagnostic instead of being dropped silently.

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::layer::{DotagentsLayer, DotagentsSource};
use super::manifest::{DotagentsManifest, DotagentsMemory};

/// Version tag embedded in protocol memory source keys.
const MEMORY_SOURCE_VERSION: &str = "v1";

/// Maximum characters of body text used for a deterministic summary fallback.
const SUMMARY_FALLBACK_CHARS: usize = 120;

/// The default knowledge score when no protocol importance is supplied.
const DEFAULT_IMPORTANCE: f64 = 0.5;

/// A neutral knowledge ingestion for one protocol memory.
///
/// Carries everything reconciliation needs to upsert (or later deactivate) the
/// memory as a protocol-owned knowledge entry, keyed by a stable source key.
#[derive(Debug, Clone, PartialEq)]
pub struct DotagentsMemoryIngest {
    /// Stable protocol source key, unique per scope and memory.
    pub source_key: String,
    /// The content fingerprint used for change detection.
    pub fingerprint: String,
    /// The knowledge source label.
    pub source: String,
    /// The text stored as the entry's raw text.
    pub raw_text: String,
    /// The deterministic summary stored on the entry.
    pub summary: String,
    /// Topics mapped from protocol `tags`.
    pub topics: Vec<String>,
    /// Normalized importance score in `0.0..=1.0`.
    pub importance: f64,
    /// Provenance of the contributing memory document.
    pub memory_source: DotagentsSource,
}

/// Derive the stable protocol source key for a memory.
///
/// The key is derived from the layer identity and the normalized memory ID, so
/// the same memory resolves to the same key across restarts regardless of
/// workspace path ordering or file mtimes. It never contains secret values,
/// because memories carry no secret fields.
pub fn memory_source_key(layer: DotagentsLayer, memory_id: &str) -> String {
    format!(
        "dotagents:{MEMORY_SOURCE_VERSION}:memory:{}:{}",
        layer.as_str(),
        super::merge::normalize_id(memory_id)
    )
}

/// Normalize a protocol importance spelling onto the knowledge score range.
///
/// Accepts the protocol's named levels (case-insensitive) and numeric values in
/// `0.0..=1.0`. Returns `None` for a numeric value outside the range or an
/// unrecognized spelling, so callers can report a diagnostic instead of
/// silently scoring the memory incorrectly.
pub fn normalize_importance(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    match trimmed.to_ascii_lowercase().as_str() {
        "trivial" | "lowest" => Some(0.1),
        "low" | "minor" => Some(0.3),
        "normal" | "medium" | "moderate" => Some(0.5),
        "high" | "major" => Some(0.8),
        "critical" | "highest" | "urgent" => Some(1.0),
        _ => trimmed
            .parse::<f64>()
            .ok()
            .filter(|score| (0.0..=1.0).contains(score)),
    }
}

/// The knowledge score for a memory, applying the default when unspecified.
fn importance_score(memory: &DotagentsMemory) -> (f64, Option<DotagentsDiagnostic>) {
    match memory.importance.as_deref() {
        None => (DEFAULT_IMPORTANCE, None),
        Some(raw) => match normalize_importance(raw) {
            Some(score) => (score, None),
            None => (
                DEFAULT_IMPORTANCE,
                Some(
                    DotagentsDiagnostic::warning(
                        DotagentsDiagnosticCode::Other,
                        format!(
                            "memory `{}` has unsupported importance `{}`; using the default score {}",
                            memory.id, raw.trim(), DEFAULT_IMPORTANCE
                        ),
                    )
                    .with_source(memory.source.clone()),
                ),
            ),
        },
    }
}

/// The primary text of a memory: the body, falling back to `content` metadata.
///
/// Parsing rejects documents that supply neither, so the fallback only matters
/// when a document has metadata content and an empty body.
pub fn memory_text(memory: &DotagentsMemory) -> String {
    let body = memory.body.trim();
    if !body.is_empty() {
        return body.to_string();
    }
    memory
        .content
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A deterministic summary: the title when present, otherwise the first
/// whitespace-collapsed characters of the text.
pub fn memory_summary(memory: &DotagentsMemory, text: &str) -> String {
    if let Some(title) = memory.title.as_deref()
        && !title.trim().is_empty()
    {
        return title.trim().to_string();
    }

    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SUMMARY_FALLBACK_CHARS {
        return collapsed;
    }
    let mut summary: String = collapsed.chars().take(SUMMARY_FALLBACK_CHARS).collect();
    summary.push('…');
    summary
}

/// Convert one resolved memory into a neutral knowledge ingestion.
///
/// Returns `Ok(None)` for disabled memories, which callers should skip without
/// a diagnostic. An unusable importance is reported through the returned
/// diagnostic while the memory still imports with the default score.
pub fn plan_memory_ingest(
    memory: &DotagentsMemory,
) -> Result<Option<(DotagentsMemoryIngest, Option<DotagentsDiagnostic>)>, Box<DotagentsDiagnostic>>
{
    if !memory.enabled {
        return Ok(None);
    }

    let text = memory_text(memory);
    if text.is_empty() {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::MissingField,
                format!(
                    "memory `{}` has neither a body nor `content` text",
                    memory.id
                ),
            )
            .with_source(memory.source.clone()),
        ));
    }

    let (importance, diagnostic) = importance_score(memory);
    let summary = memory_summary(memory, &text);

    Ok(Some((
        DotagentsMemoryIngest {
            source_key: memory_source_key(memory.source.layer, &memory.id),
            fingerprint: memory.fingerprint.clone(),
            source: memory_source_key(memory.source.layer, &memory.id),
            raw_text: text,
            summary,
            topics: memory.tags.clone(),
            importance,
            memory_source: memory.source.clone(),
        },
        diagnostic,
    )))
}

/// A memory that could not be converted, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsMemorySkip {
    /// The memory ID that was skipped.
    pub memory_id: String,
    /// The layer the memory was resolved from, so reconciliation can derive the
    /// same source key it would have used had the memory converted cleanly.
    pub layer: DotagentsLayer,
    /// The source-aware reason.
    pub diagnostic: DotagentsDiagnostic,
}

/// The complete memory ingestion plan for a resolved manifest.
///
/// Conversion is total: invalid memories are isolated in `skipped` and never
/// hide valid siblings, matching the protocol's diagnostic requirements.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DotagentsMemoryPlan {
    /// Ingestions to reconcile, ordered by source key.
    pub ingests: Vec<DotagentsMemoryIngest>,
    /// Non-fatal diagnostics emitted while planning (for example unusable
    /// importance values).
    pub diagnostics: Vec<DotagentsDiagnostic>,
    /// Memories that could not be converted at all.
    pub skipped: Vec<DotagentsMemorySkip>,
}

impl DotagentsMemoryPlan {
    /// Build a plan from every memory in a resolved manifest.
    pub fn from_manifest(manifest: &DotagentsManifest) -> Self {
        let mut plan = DotagentsMemoryPlan::default();

        for memory in manifest.memories.values() {
            match plan_memory_ingest(memory) {
                Ok(Some((ingest, diagnostic))) => {
                    plan.ingests.push(ingest);
                    if let Some(diagnostic) = diagnostic {
                        plan.diagnostics.push(diagnostic);
                    }
                }
                Ok(None) => {}
                Err(diagnostic) => plan.skipped.push(DotagentsMemorySkip {
                    memory_id: memory.id.clone(),
                    layer: memory.source.layer,
                    diagnostic: *diagnostic,
                }),
            }
        }

        // Deterministic ordering keeps repeated planning stable.
        plan.ingests.sort_by(|a, b| a.source_key.cmp(&b.source_key));
        plan.skipped.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
        plan.diagnostics.sort_by(|a, b| {
            let key = |d: &DotagentsDiagnostic| {
                (
                    d.code.as_str(),
                    d.source
                        .as_ref()
                        .map(|s| (s.layer, s.lexical_path.clone(), s.entry_id.clone())),
                    d.message.clone(),
                )
            };
            key(a).cmp(&key(b))
        });

        plan
    }
}

// ---------------------------------------------------------------------------
// Idempotent reconciliation
// ---------------------------------------------------------------------------

/// Whether reconciliation must create, update, or leave a protocol memory alone.
///
/// The decision comes from the stored protocol-owned entry's fingerprint, so
/// reloading unchanged memory files is a no-op and editing one replaces its own
/// entry instead of appending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsMemoryReconcileAction {
    /// No protocol-owned entry exists for this source key yet.
    Create,
    /// A protocol-owned entry exists with the same fingerprint; do nothing.
    Unchanged,
    /// A protocol-owned entry exists with a different fingerprint; update it.
    Update,
    /// A protocol-owned entry exists but is deactivated; reactivate and update.
    Reactivate,
}

/// Why a protocol memory is being deactivated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotagentsMemoryDeactivationReason {
    /// The memory document is gone from every enabled layer.
    Removed,
    /// The memory exists but is disabled.
    Disabled,
}

impl DotagentsMemoryDeactivationReason {
    fn describe(self) -> &'static str {
        match self {
            Self::Removed => "was removed",
            Self::Disabled => "is disabled",
        }
    }
}

/// The reconciliation action for one protocol memory.
#[derive(Debug, Clone, PartialEq)]
pub struct DotagentsMemoryReconcileOutcome {
    /// The action to take.
    pub action: DotagentsMemoryReconcileAction,
    /// The ingestion to apply (absent when the memory is unchanged).
    pub ingest: Option<DotagentsMemoryIngest>,
    /// The public ID of the existing protocol-owned entry, when present.
    pub existing_public_id: Option<String>,
}

/// A stored protocol-owned entry, as seen by reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsStoredMemory {
    /// The protocol source key owning the entry.
    pub source_key: String,
    /// The stored content fingerprint.
    pub fingerprint: Option<String>,
    /// The entry's public ID.
    pub public_id: String,
    /// Whether the entry is currently active.
    pub active: bool,
}

impl DotagentsStoredMemory {
    /// Build a stored-memory view from a listed protocol entry.
    pub fn from_entry(entry: &crate::knowledge::KnowledgeEntry) -> Option<Self> {
        Some(Self {
            source_key: entry.protocol_source_key.clone()?,
            fingerprint: entry.protocol_fingerprint.clone(),
            public_id: entry.public_id.clone(),
            active: entry.protocol_active,
        })
    }
}

/// Decide how to reconcile one planned memory ingestion against stored state.
///
/// Idempotency is keyed on the source key plus fingerprint:
///
/// - no stored entry -> `Create`;
/// - stored fingerprint matches -> `Unchanged` (no write, no duplicate);
/// - stored fingerprint differs -> `Update` (replaces its own content);
/// - stored entry is deactivated -> `Reactivate`.
pub fn decide_memory_reconciliation(
    ingest: &DotagentsMemoryIngest,
    existing: Option<&DotagentsStoredMemory>,
) -> DotagentsMemoryReconcileOutcome {
    let Some(existing) = existing else {
        return DotagentsMemoryReconcileOutcome {
            action: DotagentsMemoryReconcileAction::Create,
            ingest: Some(ingest.clone()),
            existing_public_id: None,
        };
    };

    if !existing.active {
        return DotagentsMemoryReconcileOutcome {
            action: DotagentsMemoryReconcileAction::Reactivate,
            ingest: Some(ingest.clone()),
            existing_public_id: Some(existing.public_id.clone()),
        };
    }

    if existing.fingerprint.as_deref() == Some(ingest.fingerprint.as_str()) {
        return DotagentsMemoryReconcileOutcome {
            action: DotagentsMemoryReconcileAction::Unchanged,
            ingest: None,
            existing_public_id: Some(existing.public_id.clone()),
        };
    }

    DotagentsMemoryReconcileOutcome {
        action: DotagentsMemoryReconcileAction::Update,
        ingest: Some(ingest.clone()),
        existing_public_id: Some(existing.public_id.clone()),
    }
}

/// A protocol-owned entry that should be deactivated, with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsMemoryDeactivation {
    /// The source key to deactivate.
    pub source_key: String,
    /// The entry's public ID.
    pub public_id: String,
    /// Why the entry is no longer live.
    pub reason: DotagentsMemoryDeactivationReason,
    /// A source-aware diagnostic explaining the deactivation.
    pub diagnostic: DotagentsDiagnostic,
}

/// Plan the complete reconciliation of a memory plan against stored entries.
///
/// Returns the per-memory actions and the deactivations for stored
/// protocol-owned entries whose source key is no longer live. Only entries
/// carrying a protocol source key are ever considered, so user-created
/// knowledge is structurally out of reach.
pub fn reconcile_memory_plan(
    plan: &DotagentsMemoryPlan,
    stored: &[DotagentsStoredMemory],
    disabled_source_keys: &[String],
) -> (
    Vec<DotagentsMemoryReconcileOutcome>,
    Vec<DotagentsMemoryDeactivation>,
) {
    let by_key: std::collections::BTreeMap<&str, &DotagentsStoredMemory> = stored
        .iter()
        .map(|entry| (entry.source_key.as_str(), entry))
        .collect();

    let outcomes: Vec<DotagentsMemoryReconcileOutcome> = plan
        .ingests
        .iter()
        .map(|ingest| {
            decide_memory_reconciliation(ingest, by_key.get(ingest.source_key.as_str()).copied())
        })
        .collect();

    let live: std::collections::BTreeSet<&str> = plan
        .ingests
        .iter()
        .map(|ingest| ingest.source_key.as_str())
        .collect();

    let mut deactivations: Vec<DotagentsMemoryDeactivation> = Vec::new();
    for entry in stored {
        if !entry.active {
            continue;
        }
        if live.contains(entry.source_key.as_str()) {
            continue;
        }
        let reason = if disabled_source_keys
            .iter()
            .any(|key| key == &entry.source_key)
        {
            DotagentsMemoryDeactivationReason::Disabled
        } else {
            DotagentsMemoryDeactivationReason::Removed
        };
        deactivations.push(DotagentsMemoryDeactivation {
            source_key: entry.source_key.clone(),
            public_id: entry.public_id.clone(),
            reason,
            diagnostic: DotagentsDiagnostic::info(
                DotagentsDiagnosticCode::Other,
                format!(
                    "protocol memory from `{}` {}; its knowledge entry is deactivated but retained for inspection",
                    entry.source_key,
                    reason.describe()
                ),
            ),
        });
    }

    // Deterministic ordering keeps repeated reconciliation stable.
    deactivations.sort_by(|a, b| a.source_key.cmp(&b.source_key));

    (outcomes, deactivations)
}

// ---------------------------------------------------------------------------
// Reconciliation orchestration
// ---------------------------------------------------------------------------

/// The result of reconciling protocol memories into a knowledge store.
///
/// The counts describe what was applied, and `diagnostics` carries every
/// non-fatal condition encountered — including a missing knowledge store, which
/// must not block the rest of the protocol from activating. Memories remain
/// inspectable through the manifest and the returned plan regardless of whether
/// a store was present.
#[derive(Debug, Clone, PartialEq)]
pub struct DotagentsMemoryReconcileReport {
    /// The plan that was (or would have been) applied. Always populated so
    /// callers can inspect memories even when nothing was imported.
    pub plan: DotagentsMemoryPlan,
    /// Knowledge entries created.
    pub created: usize,
    /// Knowledge entries updated in place.
    pub updated: usize,
    /// Knowledge entries left unchanged.
    pub unchanged: usize,
    /// Knowledge entries reactivated.
    pub reactivated: usize,
    /// Knowledge entries deactivated.
    pub deactivated: usize,
    /// Whether a knowledge store was available.
    pub store_available: bool,
    /// Every non-fatal diagnostic, deterministically ordered.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

impl DotagentsMemoryReconcileReport {
    /// Whether any memory was imported (created, updated, or reactivated).
    pub fn imported(&self) -> usize {
        self.created + self.updated + self.reactivated
    }
}

/// Reconcile protocol memories into an optional knowledge store.
///
/// When no store is configured, this returns a report whose `plan` still
/// describes every inspectable memory together with a non-fatal diagnostic
/// explaining that import was skipped. It never fails and never blocks other
/// protocol features: prompts, skills, and sub-agents activate independently.
pub async fn reconcile_memories(
    plan: DotagentsMemoryPlan,
    store: Option<&std::sync::Arc<dyn crate::knowledge::KnowledgeStore>>,
    scope: &str,
) -> DotagentsMemoryReconcileReport {
    let mut report = DotagentsMemoryReconcileReport {
        created: 0,
        updated: 0,
        unchanged: 0,
        reactivated: 0,
        deactivated: 0,
        store_available: store.is_some(),
        diagnostics: plan.diagnostics.clone(),
        plan,
    };

    let Some(store) = store else {
        // Nothing to import; a missing store is not worth reporting.
        if report.plan.ingests.is_empty() && report.plan.skipped.is_empty() {
            return report;
        }
        report.diagnostics.push(DotagentsDiagnostic::warning(
            DotagentsDiagnosticCode::Other,
            format!(
                "{} protocol memory(ies) were resolved but no knowledge store is configured; \
                 memories remain inspectable in the manifest and are not imported",
                report.plan.ingests.len()
            ),
        ));
        report.diagnostics.sort_by(diagnostic_order);
        return report;
    };

    let stored: Vec<DotagentsStoredMemory> = match store.list_protocol_sources(scope).await {
        Ok(entries) => entries
            .iter()
            .filter_map(DotagentsStoredMemory::from_entry)
            .collect(),
        Err(error) => {
            report.diagnostics.push(DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::Other,
                format!(
                    "could not read existing protocol memories from the knowledge store: {error}"
                ),
            ));
            report.diagnostics.sort_by(diagnostic_order);
            return report;
        }
    };

    let disabled: Vec<String> = report
        .plan
        .skipped
        .iter()
        .map(|skip| memory_source_key(skip.layer, &skip.memory_id))
        .collect();

    let (outcomes, deactivations) = reconcile_memory_plan(&report.plan, &stored, &disabled);

    for outcome in &outcomes {
        let Some(ingest) = &outcome.ingest else {
            report.unchanged += 1;
            continue;
        };

        let request = crate::knowledge::IngestRequest {
            source: ingest.source.clone(),
            raw_text: ingest.raw_text.clone(),
            summary: ingest.summary.clone(),
            entities: Vec::new(),
            topics: ingest.topics.clone(),
            connections: Vec::new(),
            importance: ingest.importance,
        };

        if let Err(error) = store
            .upsert_protocol_source(scope, &ingest.source_key, &ingest.fingerprint, request)
            .await
        {
            report.diagnostics.push(DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::Other,
                format!(
                    "could not import protocol memory `{}`: {error}",
                    ingest.source_key
                ),
            ));
            continue;
        }

        match outcome.action {
            DotagentsMemoryReconcileAction::Create => report.created += 1,
            DotagentsMemoryReconcileAction::Update => report.updated += 1,
            DotagentsMemoryReconcileAction::Reactivate => report.reactivated += 1,
            DotagentsMemoryReconcileAction::Unchanged => report.unchanged += 1,
        }
    }

    if !deactivations.is_empty() {
        // The store preserves the keys it is given, so it must receive the keys
        // that remain live — not the ones being deactivated. Passing the
        // deactivation set here would invert the operation and deactivate every
        // still-live protocol memory.
        let live_keys: Vec<String> = report
            .plan
            .ingests
            .iter()
            .map(|ingest| ingest.source_key.clone())
            .collect();
        match store.deactivate_protocol_sources(scope, &live_keys).await {
            Ok(deactivated) => {
                report.deactivated = deactivated.len();
                report
                    .diagnostics
                    .extend(deactivations.iter().map(|d| d.diagnostic.clone()));
            }
            Err(error) => report.diagnostics.push(DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::Other,
                format!("could not deactivate stale protocol memories: {error}"),
            )),
        }
    }

    report.diagnostics.sort_by(diagnostic_order);
    report
}

/// Deterministic diagnostic ordering shared by memory reconciliation reporting.
fn diagnostic_order(a: &DotagentsDiagnostic, b: &DotagentsDiagnostic) -> std::cmp::Ordering {
    let key = |d: &DotagentsDiagnostic| {
        (
            d.code.as_str(),
            d.source
                .as_ref()
                .map(|s| (s.layer, s.lexical_path.clone(), s.entry_id.clone())),
            d.message.clone(),
        )
    };
    key(a).cmp(&key(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dotagents::{DotagentsMemory, DotagentsSeverity};

    fn source(id: &str) -> DotagentsSource {
        DotagentsSource::entry(
            DotagentsLayer::Workspace,
            format!("/workspace/.agents/memories/{id}.md"),
            id,
        )
    }

    fn memory(id: &str) -> DotagentsMemory {
        DotagentsMemory {
            id: id.into(),
            title: None,
            tags: Vec::new(),
            importance: None,
            content: None,
            body: "Deploys go through the staging gate.".into(),
            enabled: true,
            extensions: Default::default(),
            source: source(id),
            fingerprint: "fp-1".into(),
        }
    }

    #[test]
    fn source_key_is_stable_and_layer_scoped() {
        let key = memory_source_key(DotagentsLayer::Workspace, " Deploy Notes ");
        assert_eq!(key, "dotagents:v1:memory:workspace:deploy notes");
        assert_eq!(
            key,
            memory_source_key(DotagentsLayer::Workspace, "deploy notes")
        );
        assert_ne!(
            key,
            memory_source_key(DotagentsLayer::Global, "deploy notes")
        );
    }

    #[test]
    fn body_is_used_as_primary_text() {
        let mut memory = memory("deploy");
        memory.content = Some("Metadata content".into());
        let (ingest, _) = plan_memory_ingest(&memory).unwrap().unwrap();
        assert_eq!(ingest.raw_text, "Deploys go through the staging gate.");
    }

    #[test]
    fn content_metadata_is_used_when_body_is_empty() {
        let mut memory = memory("deploy");
        memory.body = "   \n\n ".into();
        memory.content = Some("Metadata content only".into());
        let (ingest, _) = plan_memory_ingest(&memory).unwrap().unwrap();
        assert_eq!(ingest.raw_text, "Metadata content only");
    }

    #[test]
    fn summary_prefers_title_over_body_fallback() {
        let mut with_title = memory("deploy");
        with_title.title = Some("Deploy policy".into());
        let (ingest, _) = plan_memory_ingest(&with_title).unwrap().unwrap();
        assert_eq!(ingest.summary, "Deploy policy");

        let without_title = memory("deploy");
        let (ingest, _) = plan_memory_ingest(&without_title).unwrap().unwrap();
        assert_eq!(ingest.summary, "Deploys go through the staging gate.");
    }

    #[test]
    fn summary_fallback_is_bounded_and_collapses_whitespace() {
        let mut memory = memory("long");
        memory.body = format!("{}\n\n  tail", "x".repeat(200));
        let (ingest, _) = plan_memory_ingest(&memory).unwrap().unwrap();
        assert_eq!(ingest.summary.chars().count(), SUMMARY_FALLBACK_CHARS + 1);
        assert!(ingest.summary.ends_with('…'));
    }

    #[test]
    fn tags_map_to_topics() {
        let mut memory = memory("deploy");
        memory.tags = vec!["ops".into(), "release".into()];
        let (ingest, _) = plan_memory_ingest(&memory).unwrap().unwrap();
        assert_eq!(ingest.topics, vec!["ops", "release"]);
    }

    #[test]
    fn each_named_importance_form_is_accepted() {
        for (raw, expected) in [
            ("trivial", 0.1),
            ("low", 0.3),
            ("normal", 0.5),
            ("medium", 0.5),
            ("high", 0.8),
            ("critical", 1.0),
            ("HIGH", 0.8),
            ("  Critical  ", 1.0),
        ] {
            assert_eq!(
                normalize_importance(raw),
                Some(expected),
                "importance `{raw}` should normalize to {expected}"
            );
        }
    }

    #[test]
    fn numeric_importance_is_accepted_within_range() {
        assert_eq!(normalize_importance("0.0"), Some(0.0));
        assert_eq!(normalize_importance("0.75"), Some(0.75));
        assert_eq!(normalize_importance("1"), Some(1.0));
        // Out of range and unrecognized values are rejected, not clamped.
        assert_eq!(normalize_importance("1.5"), None);
        assert_eq!(normalize_importance("-0.2"), None);
        assert_eq!(normalize_importance("bogus"), None);
        assert_eq!(normalize_importance("   "), None);
    }

    #[test]
    fn unsupported_importance_falls_back_with_a_diagnostic() {
        let mut memory = memory("deploy");
        memory.importance = Some("bogus".into());
        let (ingest, diagnostic) = plan_memory_ingest(&memory).unwrap().unwrap();
        assert_eq!(ingest.importance, DEFAULT_IMPORTANCE);
        let diagnostic = diagnostic.expect("unsupported importance must be reported");
        assert_eq!(diagnostic.severity, DotagentsSeverity::Warning);
        assert!(diagnostic.message.contains("bogus"));
    }

    #[test]
    fn unsupported_numeric_importance_is_reported() {
        let mut memory = memory("deploy");
        memory.importance = Some("2.0".into());
        let (ingest, diagnostic) = plan_memory_ingest(&memory).unwrap().unwrap();
        assert_eq!(ingest.importance, DEFAULT_IMPORTANCE);
        assert!(diagnostic.is_some());
    }

    #[test]
    fn importance_defaults_when_absent() {
        let memory = memory("deploy");
        let (ingest, diagnostic) = plan_memory_ingest(&memory).unwrap().unwrap();
        assert_eq!(ingest.importance, DEFAULT_IMPORTANCE);
        assert!(diagnostic.is_none());
    }

    #[test]
    fn disabled_memories_are_skipped_without_conversion() {
        let mut memory = memory("deploy");
        memory.enabled = false;
        assert!(plan_memory_ingest(&memory).unwrap().is_none());
    }

    #[test]
    fn memory_without_any_text_is_rejected() {
        let mut memory = memory("deploy");
        memory.body = "  ".into();
        memory.content = Some("".into());
        let error = plan_memory_ingest(&memory).unwrap_err();
        assert_eq!(error.code, DotagentsDiagnosticCode::MissingField);
    }

    #[test]
    fn fingerprint_is_carried_from_the_memory() {
        let memory = memory("deploy");
        let (ingest, _) = plan_memory_ingest(&memory).unwrap().unwrap();
        assert_eq!(ingest.fingerprint, "fp-1");
        assert_eq!(ingest.source_key, ingest.source);
    }

    // ---- 5.3 Idempotent import and update reconciliation ----

    fn ingest(id: &str) -> DotagentsMemoryIngest {
        let memory = memory(id);
        plan_memory_ingest(&memory).unwrap().unwrap().0
    }

    fn stored(id: &str, fingerprint: &str, public_id: &str, active: bool) -> DotagentsStoredMemory {
        DotagentsStoredMemory {
            source_key: memory_source_key(DotagentsLayer::Workspace, id),
            fingerprint: Some(fingerprint.into()),
            public_id: public_id.into(),
            active,
        }
    }

    #[test]
    fn first_import_creates_an_entry() {
        let ingest = ingest("deploy");
        let outcome = decide_memory_reconciliation(&ingest, None);
        assert_eq!(outcome.action, DotagentsMemoryReconcileAction::Create);
        assert!(outcome.ingest.is_some());
        assert!(outcome.existing_public_id.is_none());
    }

    #[test]
    fn unchanged_reload_does_not_write_or_duplicate() {
        let ingest = ingest("deploy");
        let existing = stored("deploy", "fp-1", "entry-1", true);

        let outcome = decide_memory_reconciliation(&ingest, Some(&existing));
        assert_eq!(outcome.action, DotagentsMemoryReconcileAction::Unchanged);
        // No write is emitted and the existing entry is preserved.
        assert!(outcome.ingest.is_none());
        assert_eq!(outcome.existing_public_id.as_deref(), Some("entry-1"));

        // Idempotent across repeated reloads.
        let again = decide_memory_reconciliation(&ingest, Some(&existing));
        assert_eq!(again.action, DotagentsMemoryReconcileAction::Unchanged);
    }

    #[test]
    fn edited_memory_updates_its_own_entry() {
        let mut memory = memory("deploy");
        memory.body = "Deploys now go through the canary gate.".into();
        memory.fingerprint = "fp-2".into();
        let edited = plan_memory_ingest(&memory).unwrap().unwrap().0;

        let existing = stored("deploy", "fp-1", "entry-1", true);
        let outcome = decide_memory_reconciliation(&edited, Some(&existing));
        assert_eq!(outcome.action, DotagentsMemoryReconcileAction::Update);
        // The edit replaces the same entry rather than adding a second one.
        assert_eq!(outcome.existing_public_id.as_deref(), Some("entry-1"));
        assert_eq!(outcome.ingest.unwrap().fingerprint, "fp-2");
    }

    #[test]
    fn deactivated_entry_is_reactivated() {
        let ingest = ingest("deploy");
        let existing = stored("deploy", "fp-1", "entry-1", false);
        let outcome = decide_memory_reconciliation(&ingest, Some(&existing));
        assert_eq!(outcome.action, DotagentsMemoryReconcileAction::Reactivate);
        assert!(outcome.ingest.is_some());
    }

    #[test]
    fn plan_reconciliation_marks_removed_sources_for_deactivation() {
        let plan = DotagentsMemoryPlan {
            ingests: vec![ingest("keep")],
            diagnostics: vec![],
            skipped: vec![],
        };
        let stored = vec![
            stored("keep", "fp-1", "entry-keep", true),
            stored("gone", "fp-1", "entry-gone", true),
        ];

        let (outcomes, deactivations) = reconcile_memory_plan(&plan, &stored, &[]);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].action,
            DotagentsMemoryReconcileAction::Unchanged
        );

        assert_eq!(deactivations.len(), 1);
        assert_eq!(deactivations[0].public_id, "entry-gone");
        assert_eq!(
            deactivations[0].reason,
            DotagentsMemoryDeactivationReason::Removed
        );
    }

    #[test]
    fn disabled_source_is_deactivated_with_a_disabled_reason() {
        let plan = DotagentsMemoryPlan::default();
        let stored = vec![stored("muted", "fp-1", "entry-muted", true)];
        let disabled = vec![memory_source_key(DotagentsLayer::Workspace, "muted")];

        let (_, deactivations) = reconcile_memory_plan(&plan, &stored, &disabled);
        assert_eq!(deactivations.len(), 1);
        assert_eq!(
            deactivations[0].reason,
            DotagentsMemoryDeactivationReason::Disabled
        );
        assert!(deactivations[0].diagnostic.message.contains("disabled"));
    }

    #[test]
    fn inactive_entries_are_not_repeatedly_deactivated() {
        // An already-inactive entry is not a live source, but it is also not
        // reported again: deactivation is idempotent.
        let plan = DotagentsMemoryPlan::default();
        let stored = vec![stored("gone", "fp-1", "entry-gone", false)];
        let (_, deactivations) = reconcile_memory_plan(&plan, &stored, &[]);
        assert!(deactivations.is_empty());
    }

    #[test]
    fn reconciliation_orders_deactivations_deterministically() {
        let plan = DotagentsMemoryPlan::default();
        let stored = vec![
            stored("zeta", "fp-1", "entry-zeta", true),
            stored("alpha", "fp-1", "entry-alpha", true),
        ];
        let (_, deactivations) = reconcile_memory_plan(&plan, &stored, &[]);
        let keys: Vec<&str> = deactivations
            .iter()
            .map(|d| d.source_key.as_str())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn stored_memory_view_ignores_user_entries() {
        // User-created entries carry no protocol source key and are never
        // reconciliation candidates.
        let user = crate::knowledge::KnowledgeEntry {
            id: 1,
            public_id: "user-entry".into(),
            scope: "scope".into(),
            source: "user".into(),
            raw_text: Some("text".into()),
            summary: "text".into(),
            entities: vec![],
            topics: vec![],
            connections: vec![],
            importance: 0.5,
            consolidated_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            protocol_source_key: None,
            protocol_fingerprint: None,
            protocol_active: true,
        };
        assert!(DotagentsStoredMemory::from_entry(&user).is_none());
    }

    // ---- 5.4 Missing knowledge store ----

    #[tokio::test]
    async fn missing_store_reports_diagnostic_and_keeps_memories_inspectable() {
        let plan = DotagentsMemoryPlan {
            ingests: vec![ingest("deploy"), ingest("release")],
            diagnostics: vec![],
            skipped: vec![],
        };

        let report = reconcile_memories(plan, None, "scope").await;

        assert!(!report.store_available);
        assert_eq!(report.imported(), 0);
        assert_eq!(report.deactivated, 0);
        // Memories stay inspectable through the retained plan.
        assert_eq!(report.plan.ingests.len(), 2);
        // Exactly one non-fatal diagnostic explains the skipped import.
        assert_eq!(report.diagnostics.len(), 1);
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.severity, DotagentsSeverity::Warning);
        assert!(
            diagnostic
                .message
                .contains("no knowledge store is configured")
        );
        assert!(diagnostic.message.contains("remain inspectable"));
    }

    #[tokio::test]
    async fn missing_store_with_no_memories_reports_nothing() {
        let report = reconcile_memories(DotagentsMemoryPlan::default(), None, "scope").await;
        assert!(!report.store_available);
        assert!(report.diagnostics.is_empty());
    }

    #[tokio::test]
    async fn empty_plan_deactivates_stale_protocol_memories() {
        use crate::knowledge::sqlite::SqliteKnowledgeStore;
        use std::sync::{Arc, Mutex};

        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::session::schema::init_schema(&mut conn).unwrap();
        let store: Arc<dyn crate::knowledge::KnowledgeStore> =
            Arc::new(SqliteKnowledgeStore::new(Arc::new(Mutex::new(conn))));

        let initial = DotagentsMemoryPlan {
            ingests: vec![ingest("stale")],
            diagnostics: vec![],
            skipped: vec![],
        };
        reconcile_memories(initial, Some(&store), "scope").await;

        let report =
            reconcile_memories(DotagentsMemoryPlan::default(), Some(&store), "scope").await;
        assert_eq!(report.deactivated, 1);
        let entries = store.list_protocol_sources("scope").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].protocol_active);
    }

    #[tokio::test]
    async fn available_store_imports_and_reports_counts() {
        use crate::knowledge::sqlite::SqliteKnowledgeStore;
        use std::sync::{Arc, Mutex};

        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::session::schema::init_schema(&mut conn).unwrap();
        let store: Arc<dyn crate::knowledge::KnowledgeStore> =
            Arc::new(SqliteKnowledgeStore::new(Arc::new(Mutex::new(conn))));

        let plan = DotagentsMemoryPlan {
            ingests: vec![ingest("deploy")],
            diagnostics: vec![],
            skipped: vec![],
        };

        let report = reconcile_memories(plan, Some(&store), "scope").await;
        assert!(report.store_available);
        assert_eq!(report.created, 1);
        assert!(report.diagnostics.is_empty());

        // Re-running unchanged is idempotent: no duplicate entries.
        let plan = DotagentsMemoryPlan {
            ingests: vec![ingest("deploy")],
            diagnostics: vec![],
            skipped: vec![],
        };
        let second = reconcile_memories(plan, Some(&store), "scope").await;
        assert_eq!(second.unchanged, 1);
        assert_eq!(second.imported(), 0);
        assert_eq!(store.list_protocol_sources("scope").await.unwrap().len(), 1);
    }
}
