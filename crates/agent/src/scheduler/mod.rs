//! SchedulerActor — kameo actor for autonomous scheduled work.
//!
//! The scheduler is primarily reactive (event-driven wakes) with a periodic
//! reconciliation sweep as a safety net. It fires schedules by sending
//! `ScheduledPrompt` messages to the appropriate `SessionActor`.
//!
//! ## Design principles
//!
//! - **Consistency:** Everything with lifecycle/state/messages is a kameo actor.
//! - **Controllability:** Pause/resume/trigger-now are just actor messages.
//! - **Observability:** Actors integrate with kameo supervision and shutdown.
//! - **At-least-once delivery:** All handlers are idempotent.

pub mod messages;
pub mod wake;

use crate::agent::agent_config::AgentConfig;
use crate::agent::messages::ScheduledPrompt;
use crate::agent::session_registry::SessionRegistry;
use crate::events::{AgentEventKind, EventEnvelope};
use crate::session::domain_schedule::{Schedule, ScheduleState, ScheduleTrigger};
use crate::session::error::SessionResult;
use crate::session::repo_schedule::ScheduleRepository;
use crate::session::store::SessionStore;
use kameo::Actor;
use kameo::actor::ActorRef;
use kameo::message::{Context, Message};
use log::{debug, info, warn};
use messages::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use wake::{ActiveCycle, DeadlineQueue, EventAccumulator};

// ── Metrics ──────────────────────────────────────────────────────────────────

/// Operational metrics for the scheduler actor.
///
/// All counters are monotonically increasing. Gauges reflect current state.
/// These are designed for export to Prometheus/StatsD or structured log sinks.
#[derive(Debug, Clone, Default)]
pub struct SchedulerMetrics {
    // ── Counters ──
    /// Total number of schedule fires (CAS Armed→Running succeeded).
    pub fires_total: u64,
    /// Total number of successful cycle completions.
    pub completions_total: u64,
    /// Total number of cycle failures (timeout, error, reconciliation recovery).
    pub failures_total: u64,
    /// Number of CAS transitions that returned false (already running/paused/terminal).
    pub cas_conflicts_total: u64,
    /// Number of times a duplicate terminal event was dropped (idempotency guard).
    pub idempotent_drops_total: u64,
    /// Number of state transition denials (invalid from→to).
    pub transition_denials_total: u64,
    /// Total reconciliation sweeps run.
    pub reconciliation_sweeps_total: u64,
    /// Number of stale schedules recovered during reconciliation.
    pub reconciliation_recoveries_total: u64,
    /// Number of overdue armed schedules fired during reconciliation.
    pub reconciliation_overdue_fires_total: u64,
    /// Lease acquisition attempts.
    pub lease_acquisitions_total: u64,
    /// Lease renewals (successful).
    pub lease_renewals_total: u64,
    /// Lease renewal failures (lost ownership).
    pub lease_losses_total: u64,

    // ── Gauges ──
    /// Number of currently active (in-flight) cycles.
    pub active_cycles: u64,
    /// Number of armed schedules in the deadline queue.
    pub armed_interval_schedules: u64,
    /// Number of event-driven schedules being tracked.
    pub armed_event_schedules: u64,
    /// Number of schedules pending deletion (running + delete requested).
    pub pending_deletes: u64,

    // ── Histograms (last value; for full histogram, use external sink) ──
    /// Last observed schedule lag in seconds (time between next_run_at and actual fire).
    pub last_fire_lag_secs: f64,
    /// Last observed cycle runtime in seconds.
    pub last_cycle_runtime_secs: f64,
}

/// Configuration for the scheduler actor.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// How often to run the reconciliation sweep (seconds).
    pub reconcile_interval_secs: u64,
    /// Scheduler lease TTL in seconds.
    pub lease_ttl_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            reconcile_interval_secs: 60,
            lease_ttl_secs: 120,
        }
    }
}

/// Handle for interacting with a running `SchedulerActor`.
///
/// Wraps a kameo `ActorRef<SchedulerActor>`. All methods send typed messages
/// through the actor mailbox, ensuring serialized access without explicit locks.
#[derive(Clone)]
pub struct SchedulerHandle {
    actor_ref: ActorRef<SchedulerActor>,
}

impl SchedulerHandle {
    /// Create a new handle wrapping a kameo actor ref.
    pub fn new(actor_ref: ActorRef<SchedulerActor>) -> Self {
        Self { actor_ref }
    }

    /// Process an event from the fanout.
    pub async fn process_event(&self, envelope: EventEnvelope) -> SessionResult<()> {
        self.actor_ref
            .tell(ProcessEvent { envelope })
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(())
    }

    /// Run reconciliation.
    pub async fn reconcile(&self) -> SessionResult<()> {
        self.actor_ref
            .tell(Reconcile)
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(())
    }

    /// Fire a schedule by public ID (for TriggerNow).
    pub async fn trigger_now(&self, schedule_public_id: &str) -> SessionResult<()> {
        self.actor_ref
            .tell(TriggerNow {
                schedule_public_id: schedule_public_id.to_string(),
            })
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(())
    }

    /// Add a schedule.
    pub async fn add_schedule(&self, schedule: Schedule) -> SessionResult<()> {
        self.actor_ref
            .tell(AddSchedule { schedule })
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(())
    }

    /// Remove a schedule.
    pub async fn remove_schedule(&self, schedule_public_id: &str) -> SessionResult<()> {
        self.actor_ref
            .tell(RemoveSchedule {
                schedule_public_id: schedule_public_id.to_string(),
            })
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(())
    }

    /// Pause a schedule.
    pub async fn pause_schedule(&self, schedule_public_id: &str) -> SessionResult<()> {
        self.actor_ref
            .tell(PauseSchedule {
                schedule_public_id: schedule_public_id.to_string(),
            })
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(())
    }

    /// Resume a schedule.
    pub async fn resume_schedule(&self, schedule_public_id: &str) -> SessionResult<()> {
        self.actor_ref
            .tell(ResumeSchedule {
                schedule_public_id: schedule_public_id.to_string(),
            })
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(())
    }

    /// List schedules for a session.
    pub async fn list_schedules(
        &self,
        session_public_id: Option<&str>,
    ) -> SessionResult<Vec<Schedule>> {
        let schedules = self
            .actor_ref
            .ask(ListSchedules {
                session_public_id: session_public_id.map(|s| s.to_string()),
            })
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(schedules)
    }

    /// Get a single schedule by public ID.
    pub async fn get_schedule(&self, schedule_public_id: &str) -> SessionResult<Option<Schedule>> {
        let schedule = self
            .actor_ref
            .ask(GetSchedule {
                schedule_public_id: schedule_public_id.to_string(),
            })
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(schedule)
    }

    /// Get a snapshot of the current scheduler metrics.
    pub async fn metrics(&self) -> SchedulerMetrics {
        self.actor_ref.ask(GetMetrics).await.unwrap_or_default()
    }

    /// Shutdown the scheduler and its background tasks.
    pub async fn shutdown(&self) {
        let _ = self.actor_ref.tell(Shutdown).await;
        // Give the actor a moment to process shutdown before the ref is dropped
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The scheduler actor. Manages deadline-based and event-driven schedule firing.
///
/// A kameo actor — all interactions go through typed `Message<T>` handlers,
/// ensuring serialized access to mutable state without explicit locks.
#[derive(Actor)]
pub struct SchedulerActor {
    schedule_store: Arc<dyn ScheduleRepository>,
    session_store: Arc<dyn SessionStore>,
    session_registry: Arc<Mutex<SessionRegistry>>,
    config: Arc<AgentConfig>,
    scheduler_config: SchedulerConfig,

    // Leadership / ownership
    owner_id: String,

    // Runtime state (rebuilt from schedule_store on startup)
    deadline_queue: DeadlineQueue,
    event_counters: HashMap<String, EventAccumulator>,
    active_cycles: HashMap<String, ActiveCycle>,
    /// Schedules marked for deletion while running. Cleaned up after terminal event.
    pending_deletes: HashSet<String>,
    /// Set of `(schedule_public_id, turn_id)` pairs that have already been processed.
    /// Prevents double-application of completion/failure from duplicate events.
    processed_terminals: HashSet<(String, Option<String>)>,

    /// Self-reference set after spawn. Used to schedule deadline wake tasks
    /// that send `DeadlineReached` back to this actor.
    self_ref: Option<ActorRef<Self>>,
    /// Handle to the current deadline wake task (aborted when the queue head changes).
    wake_handle: Option<JoinHandle<()>>,

    /// Operational metrics for observability (Phase 6).
    metrics: SchedulerMetrics,
}

// ══════════════════════════════════════════════════════════════════════════
//  Message Handlers
// ══════════════════════════════════════════════════════════════════════════

// ── DeadlineReached (internal, from deadline wake loop) ──────────────────

impl Message<DeadlineReached> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: DeadlineReached,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_deadline_reached(&msg.schedule_public_id).await;
    }
}

// ── ProcessEvent (internal, from event subscription loop) ────────────────

impl Message<ProcessEvent> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: ProcessEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.process_event(&msg.envelope).await;
    }
}

// ── Reconcile (internal, from reconciliation loop) ───────────────────────

impl Message<Reconcile> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: Reconcile,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.reconcile().await;
    }
}

// ── CycleCompleted (internal, from process_event) ────────────────────────

impl Message<CycleCompleted> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: CycleCompleted,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_cycle_completed(&msg.schedule_public_id, &msg.turn_id)
            .await;
    }
}

// ── CycleFailed (internal, from process_event / timeout) ─────────────────

impl Message<CycleFailed> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: CycleFailed,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_cycle_failed_inner(&msg.schedule_public_id, msg.turn_id.as_deref(), &msg.error)
            .await;
    }
}

// ── DebounceCompleted (internal, from debounce timer) ────────────────────

impl Message<DebounceCompleted> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: DebounceCompleted,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_debounce_completed(&msg.schedule_public_id)
            .await;
    }
}

// ── TriggerNow (control) ─────────────────────────────────────────────────

impl Message<TriggerNow> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: TriggerNow,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_trigger_now(&msg.schedule_public_id).await;
    }
}

// ── AddSchedule (control) ────────────────────────────────────────────────

impl Message<AddSchedule> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: AddSchedule,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_add_schedule(msg.schedule).await;
    }
}

// ── RemoveSchedule (control) ─────────────────────────────────────────────

impl Message<RemoveSchedule> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: RemoveSchedule,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_remove_schedule(&msg.schedule_public_id).await;
    }
}

// ── PauseSchedule (control) ──────────────────────────────────────────────

impl Message<PauseSchedule> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: PauseSchedule,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_pause_schedule(&msg.schedule_public_id).await;
    }
}

// ── ResumeSchedule (control) ─────────────────────────────────────────────

impl Message<ResumeSchedule> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: ResumeSchedule,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_resume_schedule(&msg.schedule_public_id).await;
    }
}

// ── ListSchedules (query, uses ask) ──────────────────────────────────────

impl Message<ListSchedules> for SchedulerActor {
    type Reply = Result<Vec<Schedule>, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        msg: ListSchedules,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self
            .handle_list_schedules(msg.session_public_id.as_deref())
            .await)
    }
}

// ── GetSchedule (query, uses ask) ─────────────────────────────────────────

impl Message<GetSchedule> for SchedulerActor {
    type Reply = Result<Option<Schedule>, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        msg: GetSchedule,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self
            .schedule_store
            .get_schedule(&msg.schedule_public_id)
            .await
            .unwrap_or(None))
    }
}

// ── GetMetrics (query, uses ask) ─────────────────────────────────────────

impl Message<GetMetrics> for SchedulerActor {
    type Reply = Result<SchedulerMetrics, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        _msg: GetMetrics,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.metrics.clone())
    }
}

// ── Shutdown ─────────────────────────────────────────────────────────────

impl Message<Shutdown> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: Shutdown,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.abort_background_tasks();
        // Best-effort lease release so the next instance can acquire immediately
        // rather than waiting for TTL expiry. Errors are logged but don't block shutdown.
        match self
            .schedule_store
            .release_scheduler_lease(&self.owner_id)
            .await
        {
            Ok(true) => info!("SchedulerActor: lease released (owner={})", self.owner_id),
            Ok(false) => warn!(
                "SchedulerActor: lease release skipped — not owner or already expired (owner={})",
                self.owner_id
            ),
            Err(e) => warn!(
                "SchedulerActor: lease release failed (owner={}): {}",
                self.owner_id, e
            ),
        }
        // Stop the actor after this message is processed
        ctx.stop();
    }
}

// ── SetSelfRef (internal, sent once after spawn) ─────────────────────────

impl Message<SetSelfRef> for SchedulerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetSelfRef,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.self_ref = Some(msg.actor_ref);
        // Now that we have a self-ref, schedule the first deadline wake
        self.reschedule_wake();
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  Construction, spawn, and background loops
// ══════════════════════════════════════════════════════════════════════════

impl SchedulerActor {
    /// Create a new `SchedulerActor`.
    pub fn new(
        schedule_store: Arc<dyn ScheduleRepository>,
        session_store: Arc<dyn SessionStore>,
        session_registry: Arc<Mutex<SessionRegistry>>,
        config: Arc<AgentConfig>,
        scheduler_config: SchedulerConfig,
    ) -> Self {
        let owner_id = uuid::Uuid::now_v7().to_string();
        Self {
            schedule_store,
            session_store,
            session_registry,
            config,
            scheduler_config,
            owner_id,
            deadline_queue: DeadlineQueue::new(),
            event_counters: HashMap::new(),
            active_cycles: HashMap::new(),
            pending_deletes: HashSet::new(),
            processed_terminals: HashSet::new(),
            self_ref: None,
            wake_handle: None,
            metrics: SchedulerMetrics::default(),
        }
    }

    /// Spawn the scheduler actor, initialize it, and start background loops.
    ///
    /// Returns `None` if the lease could not be acquired (another scheduler is active).
    /// Returns `Some(SchedulerHandle)` if this scheduler became the active leader.
    pub async fn spawn(
        schedule_store: Arc<dyn ScheduleRepository>,
        session_store: Arc<dyn SessionStore>,
        session_registry: Arc<Mutex<SessionRegistry>>,
        config: Arc<AgentConfig>,
        scheduler_config: SchedulerConfig,
    ) -> Option<SchedulerHandle> {
        let mut actor = Self::new(
            schedule_store,
            session_store,
            session_registry,
            config,
            scheduler_config,
        );

        // Try to acquire lease and initialize (before spawning the actor).
        // Direct mutable access is safe here — actor is not yet shared.
        let acquired = actor.initialize().await;
        if !acquired {
            info!("SchedulerActor: not starting (lease not acquired)");
            return None;
        }

        // Capture data needed by background loops before move into spawn.
        let agent_config = actor.config.clone();
        let sched_store = actor.schedule_store.clone();
        let sched_config = actor.scheduler_config.clone();
        let owner_id = actor.owner_id.clone();

        // Spawn as a kameo actor
        let actor_ref = kameo::actor::Spawn::spawn(actor);

        // Store self_ref in the actor so it can schedule deadline wakes.
        // We use tell() which is fine here — the actor processes it before
        // any background loop messages arrive.
        {
            let r = actor_ref.clone();
            let _ = actor_ref.tell(SetSelfRef { actor_ref: r }).await;
        }

        // Start background loops — each sends typed messages via ActorRef.
        Self::start_background_loops(
            &actor_ref,
            &agent_config,
            &sched_store,
            &sched_config,
            &owner_id,
        );

        info!("SchedulerActor: started successfully");
        Some(SchedulerHandle::new(actor_ref))
    }

    /// Initialize the scheduler: acquire lease, recover state, populate queues.
    ///
    /// Called before the actor is spawned (direct mutable access is safe here).
    /// Returns `true` if the lease was acquired (this scheduler is the active leader).
    async fn initialize(&mut self) -> bool {
        // 1. Acquire lease
        match self
            .schedule_store
            .try_acquire_scheduler_lease(&self.owner_id, self.scheduler_config.lease_ttl_secs)
            .await
        {
            Ok(true) => {
                info!("SchedulerActor: lease acquired (owner={})", self.owner_id);
            }
            Ok(false) => {
                info!("SchedulerActor: lease not acquired, staying passive");
                return false;
            }
            Err(e) => {
                warn!("SchedulerActor: failed to acquire lease: {}", e);
                return false;
            }
        }

        // 2. Recover stale Running schedules
        self.recover_stale_running().await;

        // 3. Load all Armed schedules and populate queues
        self.rebuild_queues().await;

        true
    }

    /// Start background loops that send typed messages to the actor.
    ///
    /// Each loop owns a clone of the `ActorRef` and uses `tell()` to deliver
    /// messages through the actor's mailbox. Data from the actor that the loops
    /// need is passed in explicitly (captured before the actor was spawned).
    fn start_background_loops(
        actor_ref: &ActorRef<Self>,
        agent_config: &Arc<AgentConfig>,
        schedule_store: &Arc<dyn ScheduleRepository>,
        scheduler_config: &SchedulerConfig,
        owner_id: &str,
    ) {
        Self::start_lease_renewal_loop(actor_ref, schedule_store, scheduler_config, owner_id);
        Self::start_event_subscription_loop(actor_ref, agent_config);
        Self::start_reconciliation_loop(actor_ref, scheduler_config);
        // Deadline wakes are handled by `reschedule_wake()` inside the actor
        // (triggered via the SetSelfRef message after spawn).
    }

    /// Start the lease renewal background task.
    ///
    /// Talks directly to the schedule store — lease renewal does not need
    /// actor state mutation (the `Reconcile` handler also renews, but this
    /// dedicated loop runs at half the TTL for faster detection of lease loss).
    fn start_lease_renewal_loop(
        actor_ref: &ActorRef<Self>,
        schedule_store: &Arc<dyn ScheduleRepository>,
        scheduler_config: &SchedulerConfig,
        owner_id: &str,
    ) {
        let store = schedule_store.clone();
        let owner = owner_id.to_string();
        let ttl_secs = scheduler_config.lease_ttl_secs;
        let renew_interval = ttl_secs / 2;
        let actor = actor_ref.clone();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(renew_interval));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                match store.renew_scheduler_lease(&owner, ttl_secs).await {
                    Ok(true) => {
                        debug!("SchedulerActor: lease renewed for {}", owner);
                    }
                    Ok(false) => {
                        warn!("SchedulerActor: lease lost for {}", owner);
                        // Lost lease — shut down the actor
                        let _ = actor.tell(Shutdown).await;
                        break;
                    }
                    Err(e) => {
                        warn!("SchedulerActor: lease renewal error for {}: {}", owner, e);
                        // Continue trying on transient errors
                    }
                }
            }

            info!("SchedulerActor: lease renewal loop exited");
        });
    }

    /// Start the event fanout subscription background task.
    ///
    /// Subscribes to the `AgentConfig` event broadcast and forwards each
    /// envelope as a `ProcessEvent` message to the actor.
    fn start_event_subscription_loop(actor_ref: &ActorRef<Self>, agent_config: &Arc<AgentConfig>) {
        let mut event_rx = agent_config.subscribe_events();
        let actor = actor_ref.clone();

        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(envelope) => {
                        if let Err(e) = actor.tell(ProcessEvent { envelope }).await {
                            warn!("SchedulerActor: failed to send ProcessEvent: {}", e);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("SchedulerActor: event fanout closed, exiting subscription loop");
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("SchedulerActor: event subscription lagged by {} events", n);
                    }
                }
            }
        });
    }

    /// Start the reconciliation background task.
    ///
    /// Periodically sends a `Reconcile` message to the actor.
    fn start_reconciliation_loop(actor_ref: &ActorRef<Self>, scheduler_config: &SchedulerConfig) {
        let interval_secs = scheduler_config.reconcile_interval_secs;
        let actor = actor_ref.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                if let Err(e) = actor.tell(Reconcile).await {
                    warn!("SchedulerActor: failed to send Reconcile: {}", e);
                    break;
                }
            }
        });
    }

    /// Abort the current deadline wake task (if any).
    fn abort_background_tasks(&mut self) {
        if let Some(handle) = self.wake_handle.take() {
            handle.abort();
        }
        info!("SchedulerActor: background tasks aborted");
    }

    /// Schedule (or reschedule) the deadline wake task.
    ///
    /// Aborts the current wake task and spawns a new one that sleeps until the
    /// earliest deadline in the queue, then sends `DeadlineReached` to the actor.
    /// Called after any mutation to the deadline queue head.
    fn reschedule_wake(&mut self) {
        // Abort any existing wake task
        if let Some(handle) = self.wake_handle.take() {
            handle.abort();
        }

        let Some(actor_ref) = self.self_ref.clone() else {
            return; // self_ref not yet set (pre-spawn initialization)
        };

        let Some((next_time, schedule_public_id)) = self
            .deadline_queue
            .peek()
            .map(|(dt, id)| (dt, id.to_string()))
        else {
            return; // Queue is empty, nothing to schedule
        };

        let delay = (next_time - OffsetDateTime::now_utc()).max(time::Duration::ZERO);
        let std_delay = std::time::Duration::from_secs(delay.whole_seconds().max(0) as u64);

        self.wake_handle = Some(tokio::spawn(async move {
            tokio::time::sleep(std_delay).await;
            let _ = actor_ref.tell(DeadlineReached { schedule_public_id }).await;
        }));
    }

    /// Recover schedules stuck in `Running` state (from crashes/restarts).
    async fn recover_stale_running(&mut self) {
        // Consider anything running for longer than 2x the max runtime as stale
        let cutoff = OffsetDateTime::now_utc() - time::Duration::seconds(300);
        match self
            .schedule_store
            .list_running_schedules_older_than(cutoff)
            .await
        {
            Ok(stale) => {
                for schedule in stale {
                    info!(
                        "SchedulerActor: recovering stale running schedule: {}",
                        schedule.public_id
                    );
                    self.handle_cycle_failed_inner(
                        &schedule.public_id,
                        None,
                        "recovered from stale running state on startup",
                    )
                    .await;
                }
            }
            Err(e) => {
                warn!(
                    "SchedulerActor: failed to list stale running schedules: {}",
                    e
                );
            }
        }
    }

    /// Rebuild in-memory deadline queue and event counters from storage.
    async fn rebuild_queues(&mut self) {
        match self.schedule_store.list_all_armed_schedules().await {
            Ok(schedules) => {
                for schedule in schedules {
                    self.enqueue_schedule(&schedule);
                }
                info!(
                    "SchedulerActor: rebuilt queues with {} armed schedules",
                    self.deadline_queue.len() + self.event_counters.len()
                );
            }
            Err(e) => {
                warn!("SchedulerActor: failed to load armed schedules: {}", e);
            }
        }
    }

    /// Add a schedule to the appropriate in-memory queue based on its trigger.
    ///
    /// Automatically reschedules the deadline wake task if the queue head may
    /// have changed (interval schedules).
    fn enqueue_schedule(&mut self, schedule: &Schedule) {
        match &schedule.trigger {
            ScheduleTrigger::Interval { .. } | ScheduleTrigger::OnceAt { .. } => {
                if let Some(next) = schedule.next_run_at {
                    self.deadline_queue.insert(next, schedule.public_id.clone());
                    // Queue head may have changed — reschedule wake
                    self.reschedule_wake();
                }
            }
            ScheduleTrigger::EventDriven {
                event_filter,
                debounce_seconds: _,
            } => {
                self.event_counters.insert(
                    schedule.public_id.clone(),
                    EventAccumulator::new(event_filter.threshold),
                );
            }
        }
        self.metrics.armed_interval_schedules = self.deadline_queue.len() as u64;
        self.metrics.armed_event_schedules = self.event_counters.len() as u64;
    }

    // ── Firing flow ──────────────────────────────────────────────────────

    /// Attempt to fire a schedule. Performs CAS, sends ScheduledPrompt, sets up timeout.
    pub async fn fire_schedule(&mut self, schedule_public_id: &str) {
        // CAS: Armed -> Running
        let cas_ok = match self
            .schedule_store
            .update_schedule_state(
                schedule_public_id,
                ScheduleState::Armed,
                ScheduleState::Running,
            )
            .await
        {
            Ok(ok) => ok,
            Err(e) => {
                warn!(
                    "SchedulerActor: CAS failed for {}: {}",
                    schedule_public_id, e
                );
                return;
            }
        };

        if !cas_ok {
            self.metrics.cas_conflicts_total += 1;
            debug!(
                "SchedulerActor: CAS conflict for {} (Armed→Running denied, schedule_public_id={}, cas_conflicts_total={})",
                schedule_public_id, schedule_public_id, self.metrics.cas_conflicts_total
            );
            return;
        }

        // Load the schedule to get session info and config
        let schedule = match self.schedule_store.get_schedule(schedule_public_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                warn!(
                    "SchedulerActor: schedule {} not found after CAS",
                    schedule_public_id
                );
                return;
            }
            Err(e) => {
                warn!(
                    "SchedulerActor: failed to load schedule {}: {}",
                    schedule_public_id, e
                );
                return;
            }
        };

        // Look up SessionActorRef from registry
        let session_actor_ref = {
            let registry = self.session_registry.lock().await;
            registry.get(&schedule.session_public_id).cloned()
        };

        let Some(actor_ref) = session_actor_ref else {
            warn!(
                "SchedulerActor: session {} not found in registry for schedule {}",
                schedule.session_public_id, schedule_public_id
            );
            // Fail the cycle — session not available
            self.handle_cycle_failed_inner(
                schedule_public_id,
                None,
                "session not found in registry",
            )
            .await;
            return;
        };

        // Load the task to get the actual prompt the user entered.
        let prompt_text = match self.session_store.get_task(&schedule.task_public_id).await {
            Ok(Some(task)) => task
                .expected_deliverable
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "Execute the recurring task for schedule {}.",
                        schedule_public_id
                    )
                }),
            Ok(None) => {
                warn!(
                    "SchedulerActor: task {} not found for schedule {}",
                    schedule.task_public_id, schedule_public_id
                );
                format!(
                    "Execute the recurring task for schedule {}.",
                    schedule_public_id
                )
            }
            Err(e) => {
                warn!(
                    "SchedulerActor: failed to load task {} for schedule {}: {}",
                    schedule.task_public_id, schedule_public_id, e
                );
                format!(
                    "Execute the recurring task for schedule {}.",
                    schedule_public_id
                )
            }
        };

        // Send ScheduledPrompt to the SessionActor
        let scheduled_prompt = ScheduledPrompt {
            schedule_public_id: schedule_public_id.to_string(),
            prompt_text,
            execution_limits: schedule.config.execution_limits.clone(),
        };

        match actor_ref.tell_scheduled_prompt(scheduled_prompt).await {
            Ok(()) => {
                self.metrics.fires_total += 1;
                info!(
                    "SchedulerActor: fired schedule {} → session {} (fires_total={})",
                    schedule_public_id, schedule.session_public_id, self.metrics.fires_total
                );
            }
            Err(e) => {
                warn!(
                    "SchedulerActor: failed to send ScheduledPrompt to session {}: {}",
                    schedule.session_public_id, e
                );
                self.handle_cycle_failed_inner(
                    schedule_public_id,
                    None,
                    &format!("failed to send ScheduledPrompt: {}", e),
                )
                .await;
                return;
            }
        }

        // Set up timeout handle — sends CycleFailed directly to this actor
        let max_runtime = schedule.config.max_runtime_seconds;
        let schedule_id_for_timeout = schedule_public_id.to_string();
        let timeout_handle = if let Some(actor_ref) = self.self_ref.clone() {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(max_runtime)).await;
                let _ = actor_ref
                    .tell(CycleFailed {
                        schedule_public_id: schedule_id_for_timeout,
                        turn_id: None,
                        error: format!("cycle exceeded max_runtime_seconds ({})", max_runtime),
                    })
                    .await;
            })
        } else {
            // Fallback: no self_ref (should not happen in normal operation)
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(max_runtime)).await;
                warn!(
                    "SchedulerActor: timeout for {} but no self_ref to send CycleFailed",
                    schedule_id_for_timeout
                );
            })
        };

        self.active_cycles.insert(
            schedule_public_id.to_string(),
            ActiveCycle {
                started_at: OffsetDateTime::now_utc(),
                timeout_handle,
            },
        );
        self.metrics.active_cycles = self.active_cycles.len() as u64;

        // Emit ScheduleFired event
        self.config.emit_event(
            &schedule.session_public_id,
            AgentEventKind::ScheduleFired {
                schedule_public_id: schedule_public_id.to_string(),
                session_public_id: schedule.session_public_id.clone(),
            },
        );
    }

    // ── Completion flow ──────────────────────────────────────────────────

    /// Handle successful cycle completion.
    pub async fn handle_cycle_completed(&mut self, schedule_public_id: &str, turn_id: &str) {
        // Idempotency guard
        let key = (schedule_public_id.to_string(), Some(turn_id.to_string()));
        if self.processed_terminals.contains(&key) {
            self.metrics.idempotent_drops_total += 1;
            debug!(
                "SchedulerActor: duplicate CycleCompleted dropped (schedule={}, turn={}, idempotent_drops_total={})",
                schedule_public_id, turn_id, self.metrics.idempotent_drops_total
            );
            return;
        }
        self.processed_terminals.insert(key);

        // Remove active cycle, abort timeout, and track runtime
        if let Some(cycle) = self.active_cycles.remove(schedule_public_id) {
            cycle.timeout_handle.abort();
            let runtime = (OffsetDateTime::now_utc() - cycle.started_at).as_seconds_f64();
            self.metrics.last_cycle_runtime_secs = runtime;
        }
        self.metrics.active_cycles = self.active_cycles.len() as u64;
        self.metrics.completions_total += 1;

        // Load and update schedule
        let schedule = match self.schedule_store.get_schedule(schedule_public_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                warn!(
                    "SchedulerActor: schedule {} not found on completion",
                    schedule_public_id
                );
                return;
            }
            Err(e) => {
                warn!(
                    "SchedulerActor: failed to load schedule {} on completion: {}",
                    schedule_public_id, e
                );
                return;
            }
        };

        let mut updated = schedule.clone();
        updated.run_count += 1;
        updated.consecutive_failures = 0;
        updated.last_run_at = Some(OffsetDateTime::now_utc());
        updated.updated_at = OffsetDateTime::now_utc();

        // Check max_runs
        let exhausted = updated
            .config
            .max_runs
            .is_some_and(|max| updated.run_count >= max);

        if exhausted {
            updated.state = ScheduleState::Exhausted;
            updated.next_run_at = None;

            if let Err(e) = self.schedule_store.update_schedule(updated).await {
                warn!(
                    "SchedulerActor: failed to update exhausted schedule {}: {}",
                    schedule_public_id, e
                );
            }

            self.config.emit_event(
                &schedule.session_public_id,
                AgentEventKind::ScheduleExhausted {
                    schedule_public_id: schedule_public_id.to_string(),
                },
            );
        } else {
            // Compute next_run_at and transition Running -> Armed
            updated.state = ScheduleState::Armed;
            if let ScheduleTrigger::Interval { seconds } = &schedule.trigger {
                updated.next_run_at = Some(wake::compute_next_run_at(
                    *seconds,
                    schedule.config.jitter_percent,
                    OffsetDateTime::now_utc(),
                ));
            }

            if let Err(e) = self.schedule_store.update_schedule(updated.clone()).await {
                warn!(
                    "SchedulerActor: failed to re-arm schedule {}: {}",
                    schedule_public_id, e
                );
            }

            // Re-enqueue in deadline queue
            self.enqueue_schedule(&updated);
        }

        // Handle pending delete
        if self.pending_deletes.remove(schedule_public_id) {
            info!(
                "SchedulerActor: executing deferred delete for {}",
                schedule_public_id
            );
            if let Err(e) = self
                .schedule_store
                .delete_schedule(schedule_public_id)
                .await
            {
                warn!(
                    "SchedulerActor: deferred delete failed for {}: {}",
                    schedule_public_id, e
                );
            }
        }

        // Emit cycle completed event
        self.config.emit_event(
            &schedule.session_public_id,
            AgentEventKind::ScheduleCycleCompleted {
                schedule_public_id: schedule_public_id.to_string(),
                turn_id: turn_id.to_string(),
                run_count: schedule.run_count + 1,
            },
        );
    }

    // ── Failure flow ─────────────────────────────────────────────────────

    /// Handle cycle failure (timeout, explicit error, or reconciliation recovery).
    async fn handle_cycle_failed_inner(
        &mut self,
        schedule_public_id: &str,
        turn_id: Option<&str>,
        error: &str,
    ) {
        // Idempotency guard
        let key = (
            schedule_public_id.to_string(),
            turn_id.map(|s| s.to_string()),
        );
        if self.processed_terminals.contains(&key) {
            self.metrics.idempotent_drops_total += 1;
            debug!(
                "SchedulerActor: duplicate CycleFailed dropped (schedule={}, turn={:?}, idempotent_drops_total={})",
                schedule_public_id, turn_id, self.metrics.idempotent_drops_total
            );
            return;
        }
        self.processed_terminals.insert(key);

        // Remove active cycle, abort timeout, and track runtime
        if let Some(cycle) = self.active_cycles.remove(schedule_public_id) {
            cycle.timeout_handle.abort();
            let runtime = (OffsetDateTime::now_utc() - cycle.started_at).as_seconds_f64();
            self.metrics.last_cycle_runtime_secs = runtime;
        }
        self.metrics.active_cycles = self.active_cycles.len() as u64;
        self.metrics.failures_total += 1;

        // Load and update schedule
        let schedule = match self.schedule_store.get_schedule(schedule_public_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                warn!(
                    "SchedulerActor: schedule {} not found on failure",
                    schedule_public_id
                );
                return;
            }
            Err(e) => {
                warn!(
                    "SchedulerActor: failed to load schedule {} on failure: {}",
                    schedule_public_id, e
                );
                return;
            }
        };

        let mut updated = schedule.clone();
        updated.consecutive_failures += 1;
        updated.last_run_at = Some(OffsetDateTime::now_utc());
        updated.updated_at = OffsetDateTime::now_utc();

        let failure_threshold_reached =
            updated.consecutive_failures >= updated.config.max_consecutive_failures;

        if failure_threshold_reached {
            updated.state = ScheduleState::Failed;
            updated.next_run_at = None;

            if let Err(e) = self.schedule_store.update_schedule(updated).await {
                warn!(
                    "SchedulerActor: failed to update failed schedule {}: {}",
                    schedule_public_id, e
                );
            }

            self.config.emit_event(
                &schedule.session_public_id,
                AgentEventKind::ScheduleFailed {
                    schedule_public_id: schedule_public_id.to_string(),
                    consecutive_failures: schedule.consecutive_failures + 1,
                },
            );
        } else {
            // Backoff and re-arm
            updated.state = ScheduleState::Armed;
            let backoff = wake::compute_backoff(
                schedule.config.backoff_base_seconds,
                updated.consecutive_failures,
                schedule.config.jitter_percent,
            );
            updated.next_run_at = Some(OffsetDateTime::now_utc() + backoff);

            if let Err(e) = self.schedule_store.update_schedule(updated.clone()).await {
                warn!(
                    "SchedulerActor: failed to re-arm schedule {} after failure: {}",
                    schedule_public_id, e
                );
            }

            // Re-enqueue with backoff delay
            self.enqueue_schedule(&updated);
        }

        // Handle pending delete
        if self.pending_deletes.remove(schedule_public_id) {
            info!(
                "SchedulerActor: executing deferred delete after failure for {}",
                schedule_public_id
            );
            if let Err(e) = self
                .schedule_store
                .delete_schedule(schedule_public_id)
                .await
            {
                warn!(
                    "SchedulerActor: deferred delete failed for {}: {}",
                    schedule_public_id, e
                );
            }
        }

        // Emit cycle failed event
        self.config.emit_event(
            &schedule.session_public_id,
            AgentEventKind::ScheduleCycleFailed {
                schedule_public_id: schedule_public_id.to_string(),
                turn_id: turn_id.map(|s| s.to_string()),
                error: error.to_string(),
            },
        );
    }

    // ── Schedule management ──────────────────────────────────────────────

    /// Add a new schedule.
    pub async fn handle_add_schedule(&mut self, schedule: Schedule) {
        let public_id = schedule.public_id.clone();
        let session_public_id = schedule.session_public_id.clone();
        let task_public_id = schedule.task_public_id.clone();

        match self.schedule_store.create_schedule(schedule).await {
            Ok(created) => {
                self.enqueue_schedule(&created);
                info!(
                    "SchedulerActor: added schedule {} for task {}",
                    public_id, task_public_id
                );

                self.config.emit_event(
                    &session_public_id,
                    AgentEventKind::ScheduleCreated {
                        schedule_public_id: created.public_id.clone(),
                        session_public_id: session_public_id.clone(),
                        task_public_id,
                    },
                );
            }
            Err(e) => {
                warn!("SchedulerActor: failed to create schedule: {}", e);
            }
        }
    }

    /// Remove a schedule. If running, defers deletion until after the terminal event.
    pub async fn handle_remove_schedule(&mut self, schedule_public_id: &str) {
        // Check if currently running
        if self.active_cycles.contains_key(schedule_public_id) {
            info!(
                "SchedulerActor: schedule {} is running, deferring delete",
                schedule_public_id
            );
            self.pending_deletes.insert(schedule_public_id.to_string());
            self.metrics.pending_deletes = self.pending_deletes.len() as u64;
            return;
        }

        // Remove from in-memory queues
        self.deadline_queue.remove(schedule_public_id);
        self.event_counters.remove(schedule_public_id);
        self.metrics.armed_interval_schedules = self.deadline_queue.len() as u64;
        self.metrics.armed_event_schedules = self.event_counters.len() as u64;
        self.reschedule_wake();

        // Delete from store
        if let Err(e) = self
            .schedule_store
            .delete_schedule(schedule_public_id)
            .await
        {
            warn!(
                "SchedulerActor: failed to delete schedule {}: {}",
                schedule_public_id, e
            );
        } else {
            info!("SchedulerActor: removed schedule {}", schedule_public_id);
        }
    }

    /// Pause a schedule.
    pub async fn handle_pause_schedule(&mut self, schedule_public_id: &str) {
        // Try CAS from Armed -> Paused
        let from_armed = self
            .schedule_store
            .update_schedule_state(
                schedule_public_id,
                ScheduleState::Armed,
                ScheduleState::Paused,
            )
            .await
            .unwrap_or(false);

        if !from_armed {
            // Try CAS from Running -> Paused (pause requested; current turn completes)
            let from_running = self
                .schedule_store
                .update_schedule_state(
                    schedule_public_id,
                    ScheduleState::Running,
                    ScheduleState::Paused,
                )
                .await
                .unwrap_or(false);

            if !from_running {
                debug!(
                    "SchedulerActor: pause failed for {} (not armed or running)",
                    schedule_public_id
                );
                return;
            }
        }

        // Remove from in-memory queues
        self.deadline_queue.remove(schedule_public_id);
        if let Some(acc) = self.event_counters.get_mut(schedule_public_id) {
            acc.reset();
        }
        self.reschedule_wake();

        self.config.emit_event(
            "", // Session ID not easily available here; use empty for scheduler-level events
            AgentEventKind::SchedulePaused {
                schedule_public_id: schedule_public_id.to_string(),
            },
        );

        info!("SchedulerActor: paused schedule {}", schedule_public_id);
    }

    /// Resume a paused schedule.
    pub async fn handle_resume_schedule(&mut self, schedule_public_id: &str) {
        let cas_ok = self
            .schedule_store
            .update_schedule_state(
                schedule_public_id,
                ScheduleState::Paused,
                ScheduleState::Armed,
            )
            .await
            .unwrap_or(false);

        if !cas_ok {
            debug!(
                "SchedulerActor: resume failed for {} (not paused)",
                schedule_public_id
            );
            return;
        }

        // Reload and re-enqueue
        if let Ok(Some(schedule)) = self.schedule_store.get_schedule(schedule_public_id).await {
            // Recompute next_run_at for interval schedules
            if let ScheduleTrigger::Interval { seconds } = &schedule.trigger {
                let mut updated = schedule.clone();
                updated.next_run_at = Some(wake::compute_next_run_at(
                    *seconds,
                    schedule.config.jitter_percent,
                    OffsetDateTime::now_utc(),
                ));
                updated.updated_at = OffsetDateTime::now_utc();
                let _ = self.schedule_store.update_schedule(updated.clone()).await;
                self.enqueue_schedule(&updated);
            } else {
                self.enqueue_schedule(&schedule);
            }
        }

        self.config.emit_event(
            "",
            AgentEventKind::ScheduleResumed {
                schedule_public_id: schedule_public_id.to_string(),
            },
        );

        info!("SchedulerActor: resumed schedule {}", schedule_public_id);
    }

    /// Handle an incoming event from EventFanout that may match an event-driven schedule.
    pub async fn handle_event_received(&mut self, schedule_public_id: &str, _event_kind: &str) {
        let Some(acc) = self.event_counters.get_mut(schedule_public_id) else {
            return;
        };

        let threshold_met = acc.increment();

        if threshold_met {
            if acc.is_debouncing() {
                // Threshold met but still debouncing, wait for debounce timer
                debug!(
                    "SchedulerActor: threshold met for {} but debouncing",
                    schedule_public_id
                );
            } else {
                // Threshold met and no debounce active
                // Load the schedule to get debounce config
                let schedule = match self.schedule_store.get_schedule(schedule_public_id).await {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        warn!(
                            "SchedulerActor: schedule {} not found for event trigger",
                            schedule_public_id
                        );
                        return;
                    }
                    Err(e) => {
                        warn!(
                            "SchedulerActor: failed to load schedule {} for event trigger: {}",
                            schedule_public_id, e
                        );
                        return;
                    }
                };

                if let ScheduleTrigger::EventDriven {
                    event_filter: _,
                    debounce_seconds,
                } = &schedule.trigger
                {
                    let debounce_secs = *debounce_seconds; // Copy the value
                    if debounce_secs > 0 {
                        // Start debounce timer
                        let debounce_until = OffsetDateTime::now_utc()
                            + time::Duration::seconds(debounce_secs as i64);
                        acc.debounce_until = Some(debounce_until);

                        // Spawn a debounce timer that sends DebounceCompleted
                        // directly to the actor via self_ref.
                        if let Some(actor_ref) = self.self_ref.clone() {
                            let schedule_id = schedule_public_id.to_string();
                            let debounce_handle = tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(debounce_secs))
                                    .await;
                                let _ = actor_ref
                                    .tell(DebounceCompleted {
                                        schedule_public_id: schedule_id,
                                    })
                                    .await;
                            });
                            acc.debounce_handle = Some(debounce_handle);
                        }
                    } else {
                        // No debounce, fire immediately
                        acc.reset();
                        self.fire_schedule(schedule_public_id).await;
                    }
                } else {
                    // Not an event-driven schedule, shouldn't happen
                    warn!(
                        "SchedulerActor: schedule {} is not event-driven",
                        schedule_public_id
                    );
                }
            }
        }
    }

    /// Handle debounce completion for an event-driven schedule.
    pub async fn handle_debounce_completed(&mut self, schedule_public_id: &str) {
        let Some(acc) = self.event_counters.get_mut(schedule_public_id) else {
            return;
        };

        // Check if still at threshold (events may have stopped)
        if acc.threshold_met() {
            acc.reset();
            self.fire_schedule(schedule_public_id).await;
        } else {
            // Threshold no longer met, just reset debounce state
            acc.debounce_until = None;
        }
    }

    /// Process a deadline that has been reached.
    pub async fn handle_deadline_reached(&mut self, schedule_public_id: &str) {
        // Remove from queue (it was the top entry)
        self.deadline_queue.remove(schedule_public_id);
        self.reschedule_wake();
        self.fire_schedule(schedule_public_id).await;
    }

    /// Handle a TriggerNow request — fire immediately regardless of deadline.
    pub async fn handle_trigger_now(&mut self, schedule_public_id: &str) {
        self.deadline_queue.remove(schedule_public_id);
        self.reschedule_wake();
        self.fire_schedule(schedule_public_id).await;
    }

    /// List schedules, optionally filtered by session.
    pub async fn handle_list_schedules(&self, session_public_id: Option<&str>) -> Vec<Schedule> {
        match session_public_id {
            Some(spid) => self
                .schedule_store
                .list_schedules(spid)
                .await
                .unwrap_or_default(),
            None => self
                .schedule_store
                .list_all_armed_schedules()
                .await
                .unwrap_or_default(),
        }
    }

    /// Run the periodic reconciliation sweep.
    pub async fn reconcile(&mut self) {
        self.metrics.reconciliation_sweeps_total += 1;
        debug!(
            "SchedulerActor: running reconciliation sweep (sweep={})",
            self.metrics.reconciliation_sweeps_total
        );

        // 1. Recover stale running schedules
        let cutoff = OffsetDateTime::now_utc() - time::Duration::seconds(300);
        if let Ok(stale) = self
            .schedule_store
            .list_running_schedules_older_than(cutoff)
            .await
        {
            for schedule in stale {
                // Only recover if we don't have it as an active cycle
                if !self.active_cycles.contains_key(&schedule.public_id) {
                    self.metrics.reconciliation_recoveries_total += 1;
                    warn!(
                        "SchedulerActor: reconciliation recovering stale schedule: {} (recoveries_total={})",
                        schedule.public_id, self.metrics.reconciliation_recoveries_total
                    );
                    self.handle_cycle_failed_inner(
                        &schedule.public_id,
                        None,
                        "recovered by reconciliation sweep",
                    )
                    .await;
                }
            }
        }

        // 2. Check for armed schedules that may have been missed
        if let Ok(armed) = self.schedule_store.list_all_armed_schedules().await {
            let now = OffsetDateTime::now_utc();
            for schedule in armed {
                if let Some(next_run) = schedule.next_run_at
                    && next_run <= now
                {
                    self.metrics.reconciliation_overdue_fires_total += 1;
                    info!(
                        "SchedulerActor: reconciliation firing overdue schedule: {} (overdue_fires_total={})",
                        schedule.public_id, self.metrics.reconciliation_overdue_fires_total
                    );
                    self.fire_schedule(&schedule.public_id).await;
                }
            }
        }

        // 3. Renew lease
        if let Err(e) = self
            .schedule_store
            .renew_scheduler_lease(&self.owner_id, self.scheduler_config.lease_ttl_secs)
            .await
        {
            warn!("SchedulerActor: lease renewal failed: {}", e);
        }
    }

    /// Process an `EventEnvelope` from the fanout to detect scheduled terminal events.
    ///
    /// This is called by the background EventFanout subscriber task.
    pub async fn process_event(&mut self, envelope: &EventEnvelope) {
        match envelope.kind() {
            AgentEventKind::ScheduledExecutionCompleted {
                schedule_public_id,
                turn_id,
            } => {
                self.handle_cycle_completed(schedule_public_id, turn_id)
                    .await;
            }
            AgentEventKind::ScheduledExecutionFailed {
                schedule_public_id,
                turn_id,
                error,
            } => {
                self.handle_cycle_failed_inner(schedule_public_id, turn_id.as_deref(), error)
                    .await;
            }
            // ScheduleDebounceCompleted is now handled via direct DebounceCompleted
            // messages to the actor (no longer routed through the event fanout).
            AgentEventKind::ScheduleDebounceCompleted { .. } => {}
            // Match event-driven schedule triggers
            _ => {
                // Check if any event accumulator cares about this event kind
                let event_kind_name = event_kind_name(envelope.kind());

                // Check each event-driven schedule to see if this event matches its filter
                let schedule_ids: Vec<String> = self.event_counters.keys().cloned().collect();

                for schedule_id in schedule_ids {
                    // Load the schedule to check if the event matches its filter
                    if let Ok(Some(schedule)) = self.schedule_store.get_schedule(&schedule_id).await
                        && let ScheduleTrigger::EventDriven { event_filter, .. } = &schedule.trigger
                    {
                        // Check if event kind matches filter
                        if event_filter.event_kinds.contains(&event_kind_name) {
                            // Check session scope filter if specified
                            let session_matches = event_filter
                                .session_public_id
                                .as_ref()
                                .map(|filter_session| filter_session == envelope.session_id())
                                .unwrap_or(true); // None = any session

                            if session_matches {
                                self.handle_event_received(&schedule_id, &event_kind_name)
                                    .await;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Extract a snake_case name from an `AgentEventKind` for event filter matching.
fn event_kind_name(kind: &AgentEventKind) -> String {
    // Use serde serialization to get the tag name
    if let Ok(value) = serde_json::to_value(kind)
        && let Some(type_name) = value.get("type").and_then(|v| v.as_str())
    {
        return type_name.to_string();
    }
    "unknown".to_string()
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn scheduler_config_defaults() {
        let config = SchedulerConfig::default();
        assert_eq!(config.reconcile_interval_secs, 60);
        assert_eq!(config.lease_ttl_secs, 120);
    }

    #[test]
    fn event_kind_name_extracts_tag() {
        let kind = AgentEventKind::SessionCreated;
        assert_eq!(event_kind_name(&kind), "session_created");
    }

    #[test]
    fn event_kind_name_works_for_schedule_events() {
        let kind = AgentEventKind::ScheduleFired {
            schedule_public_id: "sched-1".to_string(),
            session_public_id: "sess-1".to_string(),
        };
        assert_eq!(event_kind_name(&kind), "schedule_fired");
    }
}

#[cfg(test)]
mod tests;
