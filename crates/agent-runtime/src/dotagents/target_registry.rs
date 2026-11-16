//! Collision handling for protocol sub-agent plans.
//!
//! Protocol `delegation-target` profiles extend a runtime's roster. They are
//! advertised immediately and built on first use (see
//! `delegation::roster::Member::pending`). Invariants:
//!
//! 1. **Never enable delegation.** A runtime with delegation disabled registers
//!    nothing.
//! 2. **Explicit QueryMT configuration wins.** Roster members that the host
//!    configured explicitly take precedence over a protocol target with the
//!    same normalized ID. The colliding target is skipped with a diagnostic.
//! 3. **Unsupported connections never materialize.** Plans carrying an error
//!    diagnostic are never registered, so no process is launched.

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::subagent::{DotagentsSubAgentPlan, DotagentsSubAgentPlans};
use crate::delegation::roster::RosterSnapshot;

/// Outcome of merging protocol plans against an explicit registry.
#[derive(Debug, Default)]
pub struct DotagentsTargetMerge {
    /// Protocol targets that survived collision resolution, ordered by ID.
    pub accepted: Vec<DotagentsSubAgentPlan>,
    /// Collision/skip diagnostics, ordered deterministically.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

/// Merge protocol plans with an explicit registry, applying collision rules.
///
/// Explicit QueryMT targets win. Every skipped protocol target yields a
/// diagnostic naming both the protocol source and the explicit target that won.
pub fn merge_protocol_targets(
    plans: &DotagentsSubAgentPlans,
    explicit: Option<&RosterSnapshot>,
) -> DotagentsTargetMerge {
    let layer_merge = resolve_protocol_precedence(plans);
    let mut merge = DotagentsTargetMerge {
        accepted: Vec::new(),
        diagnostics: layer_merge.diagnostics,
    };

    for plan in layer_merge.accepted {
        let id = &plan.id;
        // Unsupported connections must never be registered (task 3.6).
        if plan.diagnostics.iter().any(|d| d.is_error()) {
            continue;
        }
        if let Some(snapshot) = explicit
            && let Some(existing) = snapshot.get(id).map(|member| member.info())
        {
            merge.diagnostics.push(
                DotagentsDiagnostic::warning(
                    DotagentsDiagnosticCode::Collision,
                    format!(
                        "protocol delegation target `{id}` (from `{}`) collides with an explicitly \
                         configured QueryMT target `{}` (id `{}`); the explicit target wins and the \
                         protocol target is not registered",
                        plan.source.lexical_path.display(),
                        existing.name,
                        existing.id.as_str()
                    ),
                )
                .with_source(plan.source.clone()),
            );
            continue;
        }
        merge.accepted.push(plan);
    }

    merge.diagnostics.sort_by(|a, b| a.message.cmp(&b.message));
    merge
}

/// Resolve collisions between protocol layers before explicit config wins.
///
/// Protocol precedence follows [`DotagentsLayer::precedence`]: the workspace
/// layer overrides the global layer for a matching normalized ID, mirroring how
/// singleton documents resolve. The losing entry is dropped and a diagnostic
/// names both the winner and the loser with their lexical paths.
/// Resolve collisions between protocol layers before explicit config wins.
///
/// Cross-layer precedence is already settled upstream by
/// [`super::merge::merge_layers`], which applies layers in ascending order so a
/// later (workspace) layer replaces a matching ID from an earlier (global) one.
/// By the time plans exist, each ID has exactly one winning definition and its
/// [`super::layer::DotagentsSource`] records the layer that supplied it.
///
/// This function therefore does not re-apply precedence. It normalizes the plan
/// set into a deterministic, ID-sorted list and tags each entry with the layer
/// that won, so collision diagnostics against explicit QueryMT configuration can
/// name the contributing protocol source (task 3.4).
fn resolve_protocol_precedence(plans: &DotagentsSubAgentPlans) -> DotagentsTargetMerge {
    let mut merge = DotagentsTargetMerge {
        accepted: plans.plans.values().cloned().collect(),
        diagnostics: plans.diagnostics.clone(),
    };
    // `BTreeMap` iteration is already ID-ordered; sort defensively so callers
    // assembling plans from several manifests get deterministic output.
    merge.accepted.sort_by(|a, b| a.id.cmp(&b.id));
    merge
}

/// Diagnostic for a protocol-layer collision, naming both sources.
/// Diagnostic for a protocol-layer collision, naming both sources.
///
/// Kept for callers that merge plans assembled from multiple layer manifests
/// manually; the normal `resolve_for_builder` path cannot produce two plans for
/// one ID because `merge_layers` already applied layer precedence.
pub fn protocol_layer_collision(
    loser: &DotagentsSubAgentPlan,
    winner: &DotagentsSubAgentPlan,
) -> DotagentsDiagnostic {
    DotagentsDiagnostic::warning(
        DotagentsDiagnosticCode::Collision,
        format!(
            "protocol delegation target `{}` is defined by both the {} layer (`{}`) and the {} layer \
             (`{}`); the {} layer wins and the other definition is not registered",
            loser.id,
            loser.source.layer,
            loser.source.lexical_path.display(),
            winner.source.layer,
            winner.source.lexical_path.display(),
            winner.source.layer,
        ),
    )
    .with_source(loser.source.clone())
}
