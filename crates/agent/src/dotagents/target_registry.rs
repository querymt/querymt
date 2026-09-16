//! Lazy delegation-registry integration for protocol sub-agent plans.
//!
//! Tasks 3.2 and 3.3 extend the selected runtime's delegation targets with
//! protocol `delegation-target` profiles. Protocol targets are **lazy**: they
//! are advertised as [`AgentInfo`] metadata immediately, so delegation
//! discovery and planning can see them, but the underlying agent handle is
//! built only on first use and reused afterwards.
//!
//! Three invariants drive this module:
//!
//! 1. **Never enable delegation.** A runtime with delegation disabled keeps its
//!    targets inspectable in the manifest but registers nothing. Protocol
//!    loading never flips a disabled delegation setting to enabled.
//! 2. **Explicit QueryMT configuration wins.** Registry targets and quorum
//!    delegates that the host configured explicitly take precedence over a
//!    protocol target with the same normalized ID. The colliding protocol
//!    target is skipped and a provenance-bearing diagnostic is emitted
//!    (task 3.4).
//! 3. **Unsupported connections never materialize.** Plans carrying an error
//!    diagnostic (unsupported connection, unknown type) are never registered,
//!    so no process is launched (task 3.6).
//!
//! The registry does not build runtimes itself: it delegates construction to a
//! [`ProtocolTargetFactory`] supplied by the caller. That keeps plugin-registry
//! and storage wiring in the layer that already owns those services, and keeps
//! protocol parsing independent of runtime construction.

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::layer::DotagentsSource;
use super::subagent::{DotagentsSubAgentPlan, DotagentsSubAgentPlans};
use crate::agent::handle::AgentHandle as AgentHandleTrait;
use crate::delegation::{AgentInfo, AgentRegistry};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Builds the runtime handle for a protocol delegation target.
///
/// Implementations live with the runtime wiring that already owns the plugin
/// registry and storage, so this module never fabricates infrastructure.
pub trait ProtocolTargetFactory: Send + Sync {
    /// Construct a handle for `plan`.
    ///
    /// Returning `None` leaves the target advertised but unusable; callers
    /// should emit a diagnostic when that is unexpected.
    fn create(&self, plan: &DotagentsSubAgentPlan) -> Option<Arc<dyn AgentHandleTrait>>;
}

/// A lazily materialized protocol delegation target.
///
/// The plan is retained so the handle can be constructed on demand. The handle
/// is cached after first construction, so repeated delegation to the same
/// target reuses one runtime rather than rebuilding it (task 3.5).
struct LazyTarget {
    plan: DotagentsSubAgentPlan,
    handle: Mutex<Option<Arc<dyn AgentHandleTrait>>>,
}

impl std::fmt::Debug for LazyTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyTarget")
            .field("id", &self.plan.id)
            .field("materialized", &self.is_materialized())
            .finish()
    }
}

impl LazyTarget {
    fn new(plan: DotagentsSubAgentPlan) -> Self {
        Self {
            plan,
            handle: Mutex::new(None),
        }
    }

    fn is_materialized(&self) -> bool {
        self.handle
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    /// Build the target's runtime once and cache it.
    fn materialize(
        &self,
        factory: &dyn ProtocolTargetFactory,
    ) -> Option<Arc<dyn AgentHandleTrait>> {
        let mut slot = self.handle.lock().ok()?;
        if let Some(existing) = slot.as_ref() {
            return Some(existing.clone());
        }
        let handle = factory.create(&self.plan)?;
        *slot = Some(handle.clone());
        Some(handle)
    }
}

/// A delegation registry that exposes protocol targets lazily.
///
/// Wraps an optional base registry (explicit standalone targets, or
/// pre-registered remote agents) and adds protocol targets that do not collide
/// with it. Protocol target handles are resolved on first `get_handle` call.
pub struct DotagentsTargetRegistry {
    targets: Vec<Arc<LazyTarget>>,
    by_id: HashMap<String, Arc<LazyTarget>>,
    base: Option<Arc<dyn AgentRegistry + Send + Sync>>,
    factory: Arc<dyn ProtocolTargetFactory>,
    diagnostics: Vec<DotagentsDiagnostic>,
    materializations: Mutex<usize>,
}

impl std::fmt::Debug for DotagentsTargetRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut ids: Vec<&String> = self.by_id.keys().collect();
        ids.sort();
        f.debug_struct("DotagentsTargetRegistry")
            .field("targets", &ids)
            .field("has_base", &self.base.is_some())
            .finish()
    }
}

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
    explicit: Option<&(dyn AgentRegistry + Send + Sync)>,
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
        if let Some(registry) = explicit
            && let Some(existing) = registry.get_agent(id)
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
                        existing.id
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
/// [`DotagentsSource`] records the layer that supplied it.
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

impl DotagentsTargetRegistry {
    /// Build a registry from plans merged against an explicit base registry.
    pub fn from_merge(
        merge: DotagentsTargetMerge,
        base: Option<Arc<dyn AgentRegistry + Send + Sync>>,
        factory: Arc<dyn ProtocolTargetFactory>,
    ) -> Self {
        let targets: Vec<Arc<LazyTarget>> = merge
            .accepted
            .into_iter()
            .map(|plan| Arc::new(LazyTarget::new(plan)))
            .collect();
        let by_id = targets
            .iter()
            .map(|target| (target.plan.id.clone(), target.clone()))
            .collect();
        Self {
            targets,
            by_id,
            base,
            factory,
            diagnostics: merge.diagnostics,
            materializations: Mutex::new(0),
        }
    }

    /// Convenience constructor from plans plus an explicit base registry.
    pub fn new(
        plans: &DotagentsSubAgentPlans,
        base: Option<Arc<dyn AgentRegistry + Send + Sync>>,
        factory: Arc<dyn ProtocolTargetFactory>,
    ) -> Self {
        let merge = merge_protocol_targets(plans, base.as_ref().map(|b| b.as_ref()));
        Self::from_merge(merge, base, factory)
    }

    /// Diagnostics produced during collision resolution.
    pub fn diagnostics(&self) -> &[DotagentsDiagnostic] {
        &self.diagnostics
    }

    /// How many target handles have been materialized.
    pub fn materialization_count(&self) -> usize {
        self.materializations.lock().map(|n| *n).unwrap_or(0)
    }

    /// IDs advertised by protocol targets, ordered deterministically.
    pub fn protocol_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.by_id.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Whether a protocol target with `id` is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    /// Whether a protocol target has already been materialized.
    pub fn is_materialized(&self, id: &str) -> bool {
        self.by_id
            .get(id)
            .map(|target| target.is_materialized())
            .unwrap_or(false)
    }
}

impl AgentRegistry for DotagentsTargetRegistry {
    fn list_agents(&self) -> Vec<AgentInfo> {
        // Explicit targets first so consumers see them as canonical.
        let mut agents: Vec<AgentInfo> = match &self.base {
            Some(base) => base.list_agents(),
            None => Vec::new(),
        };
        agents.extend(self.targets.iter().map(|target| target.plan.info.clone()));
        agents
    }

    fn get_agent(&self, id: &str) -> Option<AgentInfo> {
        // Explicit targets win over protocol targets with the same ID.
        if let Some(base) = &self.base
            && let Some(info) = base.get_agent(id)
        {
            return Some(info);
        }
        self.by_id.get(id).map(|target| target.plan.info.clone())
    }

    fn get_handle(&self, id: &str) -> Option<Arc<dyn AgentHandleTrait>> {
        if let Some(base) = &self.base
            && let Some(handle) = base.get_handle(id)
        {
            return Some(handle);
        }
        let target = self.by_id.get(id)?.clone();
        let was_materialized = target.is_materialized();
        let handle = target.materialize(self.factory.as_ref())?;
        // Only count actual construction, not cache hits, so callers can assert
        // "one handle per target" (task 3.5).
        if !was_materialized && let Ok(mut count) = self.materializations.lock() {
            *count += 1;
        }
        Some(handle)
    }
}

impl DotagentsSource {
    /// Human-readable provenance for diagnostics that reference a plan.
    pub fn describe(&self) -> String {
        self.to_string()
    }
}
