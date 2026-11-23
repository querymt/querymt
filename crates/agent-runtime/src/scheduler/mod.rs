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
use crate::session::domain_schedule::{Schedule, ScheduleState, ScheduleTrigger};
use crate::session::error::SessionResult;
use crate::session::repository::ScheduleRepository;
use crate::session::store::SessionStore;
use kameo::Actor;
use kameo::actor::{ActorRef, Spawn};
use kameo::message::{Context, Message};
use kameo_actors::scheduler::{Scheduler as TimerScheduler, SetTimeout};
use log::{debug, info, warn};
use messages::*;
use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;
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
    const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

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
            .ask(AddSchedule { schedule })
            .send()
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(())
    }

    /// Remove a schedule.
    pub async fn remove_schedule(&self, schedule_public_id: &str) -> SessionResult<()> {
        self.actor_ref
            .ask(RemoveSchedule {
                schedule_public_id: schedule_public_id.to_string(),
            })
            .send()
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))
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
            .mailbox_timeout(Self::CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(schedules)
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn peer_mutation(
        &self,
        peer: Arc<crate::agent::session_actor::PeerTurn>,
        mutation: PeerScheduleMutation,
    ) -> Result<(), kameo::error::SendError<(), crate::error::AgentError>> {
        self.actor_ref
            .ask(ApplyPeerScheduleMutation { peer, mutation })
            .mailbox_timeout(Self::CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.map_msg(|_| ()))
    }

    #[cfg(feature = "remote")]
    pub(crate) fn is_same_actor(&self, other: &Self) -> bool {
        self.actor_ref.id() == other.actor_ref.id()
    }

    /// Get a single schedule by public ID.
    pub async fn get_schedule(&self, schedule_public_id: &str) -> SessionResult<Option<Schedule>> {
        let schedule = self
            .actor_ref
            .ask(GetSchedule {
                schedule_public_id: schedule_public_id.to_string(),
            })
            .mailbox_timeout(Self::CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|e| crate::session::error::SessionError::Other(e.to_string()))?;
        Ok(schedule)
    }

    /// Get a snapshot of the current scheduler metrics.
    pub async fn metrics(&self) -> SchedulerMetrics {
        self.actor_ref
            .ask(GetMetrics)
            .mailbox_timeout(Self::CONTROL_TIMEOUT)
            .send()
            .await
            .unwrap_or_default()
    }

    /// Request scheduler shutdown without waiting for actor work to drain.
    pub fn request_shutdown(&self) {
        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            let _ = actor_ref.tell(Shutdown).await;
        });
    }

    /// Shutdown the scheduler and wait for the actor to process lease release.
    pub async fn shutdown(&self) {
        let _ = self
            .actor_ref
            .ask(Shutdown)
            .mailbox_timeout(Self::CONTROL_TIMEOUT)
            .send()
            .await;
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
    #[cfg(feature = "remote")]
    waiting_for_policy: HashSet<String>,
    event_counters: HashMap<String, EventAccumulator>,
    active_cycles: HashMap<String, ActiveCycle>,
    /// Schedules marked for deletion while running. Cleaned up after terminal event.
    pending_deletes: HashSet<String>,
    #[cfg(feature = "remote")]
    pending_peer_deletes: HashMap<String, Arc<crate::agent::session_actor::PeerTurn>>,
    /// Set of `(schedule_public_id, turn_id)` pairs that have already been processed.
    /// Prevents double-application of completion/failure from duplicate events.
    processed_terminals: HashSet<(String, Option<String>)>,

    /// Self-reference set after spawn. Used as the target for scheduled messages.
    self_ref: Option<ActorRef<Self>>,
    /// Utility actor responsible for one-shot timer messages, started after lease acquisition.
    timer_scheduler: Option<ActorRef<TimerScheduler>>,
    /// Handle to the current deadline wake message (aborted when the queue head changes).
    wake_handle: Option<AbortHandle>,

    /// Token cancelled during shutdown to immediately stop all background loops
    /// (lease renewal, event subscription, reconciliation).
    shutdown_token: CancellationToken,

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
    type Reply = Result<(), crate::session::error::SessionError>;

    async fn handle(
        &mut self,
        msg: AddSchedule,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_add_schedule(msg.schedule).await
    }
}

// ── RemoveSchedule (control) ─────────────────────────────────────────────

impl Message<RemoveSchedule> for SchedulerActor {
    type Reply = Result<(), crate::session::error::SessionError>;

    async fn handle(
        &mut self,
        msg: RemoveSchedule,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_remove_schedule(&msg.schedule_public_id).await
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
        if let Err(error) = self.handle_pause_schedule(&msg.schedule_public_id).await {
            warn!(
                "SchedulerActor: pause failed for {}: {error}",
                msg.schedule_public_id
            );
        }
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
        if let Err(error) = self.handle_resume_schedule(&msg.schedule_public_id).await {
            warn!(
                "SchedulerActor: resume failed for {}: {error}",
                msg.schedule_public_id
            );
        }
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
    type Reply = SessionResult<Option<Schedule>>;

    async fn handle(
        &mut self,
        msg: GetSchedule,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.schedule_store
            .get_schedule(&msg.schedule_public_id)
            .await
    }
}

#[cfg(feature = "remote")]
#[derive(Clone, Copy)]
pub(crate) enum ScheduleAction {
    Pause,
    Resume,
    Trigger,
    Delete,
}

#[cfg(feature = "remote")]
pub(crate) enum PeerScheduleMutation {
    Add(Box<Schedule>),
    Action(ScheduleAction, String),
}

#[cfg(feature = "remote")]
struct ApplyPeerScheduleMutation {
    peer: Arc<crate::agent::session_actor::PeerTurn>,
    mutation: PeerScheduleMutation,
}

#[cfg(feature = "remote")]
impl Message<ApplyPeerScheduleMutation> for SchedulerActor {
    type Reply = Result<(), crate::error::AgentError>;

    async fn handle(
        &mut self,
        msg: ApplyPeerScheduleMutation,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::agent::session_actor::TurnAuthority;
        use crate::error::AgentError;

        let storage_error =
            |error: crate::session::error::SessionError| AgentError::Internal(error.to_string());
        match msg.mutation {
            PeerScheduleMutation::Add(schedule) => {
                if schedule.config.origin != msg.peer.schedule_origin() {
                    return Err(AgentError::MeshAccessDenied);
                }
                msg.peer.check_schedule_mutation().await?;
                self.handle_add_schedule(*schedule)
                    .await
                    .map_err(storage_error)?;
            }
            PeerScheduleMutation::Action(action, id) => {
                let schedule = self
                    .schedule_store
                    .get_schedule(&id)
                    .await
                    .map_err(storage_error)?
                    .ok_or(AgentError::MeshAccessDenied)?;
                if !TurnAuthority::Peer(msg.peer.clone()).owns_schedule(&schedule.config.origin) {
                    return Err(AgentError::MeshAccessDenied);
                }
                msg.peer.check_schedule_mutation().await?;
                match action {
                    ScheduleAction::Pause => self
                        .handle_pause_schedule(&id)
                        .await
                        .map_err(storage_error)?,
                    ScheduleAction::Resume => self
                        .handle_resume_schedule(&id)
                        .await
                        .map_err(storage_error)?,
                    ScheduleAction::Trigger => self.fire_loaded_schedule(&id, schedule).await,
                    ScheduleAction::Delete if self.active_cycles.contains_key(&id) => {
                        self.pending_peer_deletes.insert(id, msg.peer);
                        self.update_pending_delete_metrics();
                    }
                    ScheduleAction::Delete => {
                        self.delete_schedule(&id).await.map_err(storage_error)?
                    }
                }
            }
        }
        Ok(())
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
        self.reschedule_wake().await;
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
            #[cfg(feature = "remote")]
            waiting_for_policy: HashSet::new(),
            event_counters: HashMap::new(),
            active_cycles: HashMap::new(),
            pending_deletes: HashSet::new(),
            #[cfg(feature = "remote")]
            pending_peer_deletes: HashMap::new(),
            processed_terminals: HashSet::new(),
            self_ref: None,
            timer_scheduler: None,
            wake_handle: None,
            shutdown_token: CancellationToken::new(),
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

        let prepared_timer =
            TimerScheduler::prepare().reply_timeout(SchedulerHandle::CONTROL_TIMEOUT);
        let timer_scheduler = prepared_timer.actor_ref().clone();
        prepared_timer.spawn(TimerScheduler::new());
        actor.timer_scheduler = Some(timer_scheduler);

        // Capture data needed by background loops before move into spawn.
        let agent_config = actor.config.clone();
        let sched_store = actor.schedule_store.clone();
        let sched_config = actor.scheduler_config.clone();
        let owner_id = actor.owner_id.clone();
        let shutdown_token = actor.shutdown_token.clone();

        // Apply the common control-plane timeout to local asks by default.
        let prepared = Self::prepare().reply_timeout(SchedulerHandle::CONTROL_TIMEOUT);
        let actor_ref = prepared.actor_ref().clone();
        prepared.spawn(actor);

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
            &shutdown_token,
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
        shutdown_token: &CancellationToken,
    ) {
        Self::start_lease_renewal_loop(
            actor_ref,
            schedule_store,
            scheduler_config,
            owner_id,
            shutdown_token,
        );
        Self::start_event_subscription_loop(actor_ref, agent_config, shutdown_token);
        Self::start_reconciliation_loop(
            actor_ref,
            scheduler_config,
            shutdown_token,
            #[cfg(feature = "remote")]
            agent_config.provider.mesh.subscribe(),
        );
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
        shutdown_token: &CancellationToken,
    ) {
        let store = schedule_store.clone();
        let owner = owner_id.to_string();
        let ttl_secs = scheduler_config.lease_ttl_secs;
        let renew_interval = ttl_secs / 2;
        let actor = actor_ref.clone();
        let token = shutdown_token.clone();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(renew_interval));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = token.cancelled() => {
                        debug!("SchedulerActor: lease renewal loop cancelled by shutdown token");
                        break;
                    }
                }

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
    fn start_event_subscription_loop(
        actor_ref: &ActorRef<Self>,
        agent_config: &Arc<AgentConfig>,
        shutdown_token: &CancellationToken,
    ) {
        let mut event_rx = agent_config.subscribe_events();
        let actor = actor_ref.clone();
        let token = shutdown_token.clone();

        tokio::spawn(async move {
            loop {
                let envelope = tokio::select! {
                    result = event_rx.recv() => {
                        match result {
                            Ok(envelope) => envelope,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                info!("SchedulerActor: event fanout closed, exiting subscription loop");
                                break;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("SchedulerActor: event subscription lagged by {} events", n);
                                continue;
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        debug!("SchedulerActor: event subscription loop cancelled by shutdown token");
                        break;
                    }
                };

                if let Err(e) = actor.tell(ProcessEvent { envelope }).await {
                    warn!("SchedulerActor: failed to send ProcessEvent: {}", e);
                    break;
                }
            }
        });
    }

    /// Start the reconciliation background task.
    ///
    /// Periodically sends a `Reconcile` message to the actor.
    fn start_reconciliation_loop(
        actor_ref: &ActorRef<Self>,
        scheduler_config: &SchedulerConfig,
        shutdown_token: &CancellationToken,
        #[cfg(feature = "remote")] mut mesh: tokio::sync::watch::Receiver<
            Option<Arc<querymt_remote::MeshHandle>>,
        >,
    ) {
        let interval_secs = scheduler_config.reconcile_interval_secs;
        let actor = actor_ref.clone();
        let token = shutdown_token.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = async {
                        #[cfg(feature = "remote")]
                        if mesh.changed().await.is_ok() {
                            return;
                        }
                        std::future::pending::<()>().await;
                    } => {}
                    _ = interval.tick() => {}
                    _ = token.cancelled() => {
                        debug!("SchedulerActor: reconciliation loop cancelled by shutdown token");
                        break;
                    }
                }

                if let Err(e) = actor.tell(Reconcile).await {
                    warn!("SchedulerActor: failed to send Reconcile: {}", e);
                    break;
                }
            }
        });
    }

    /// Abort the current deadline wake task (if any).
    fn abort_background_tasks(&mut self) {
        // Cancel all background loops (lease renewal, event subscription, reconciliation).
        // Each loop selects on this token and exits immediately when cancelled.
        self.shutdown_token.cancel();

        // Abort one-shot messages and active-cycle timeouts.
        if let Some(handle) = self.wake_handle.take() {
            handle.abort();
        }
        for (_, cycle) in self.active_cycles.drain() {
            cycle.timeout_handle.abort();
        }
        for accumulator in self.event_counters.values_mut() {
            accumulator.reset();
        }
        if let Some(timer_scheduler) = self.timer_scheduler.take() {
            timer_scheduler.kill();
        }
        info!("SchedulerActor: background tasks aborted");
    }

    /// Schedule (or reschedule) the deadline wake task.
    ///
    /// Aborts the current wake message and schedules a new one for the earliest deadline.
    /// Called after any mutation to the deadline queue head.
    async fn reschedule_wake(&mut self) {
        if let Some(handle) = self.wake_handle.take() {
            handle.abort();
        }

        let Some(actor_ref) = self.self_ref.as_ref() else {
            return;
        };

        let Some((next_time, schedule_public_id)) = self
            .deadline_queue
            .peek()
            .map(|(dt, id)| (dt, id.to_string()))
        else {
            return;
        };

        let delay = (next_time - OffsetDateTime::now_utc()).max(time::Duration::ZERO);
        let timeout = SetTimeout::new(
            actor_ref.downgrade(),
            delay.unsigned_abs(),
            DeadlineReached { schedule_public_id },
        );
        let Some(timer_scheduler) = self.timer_scheduler.as_ref() else {
            return;
        };
        match timer_scheduler.ask(timeout).await {
            Ok(handle) => self.wake_handle = Some(handle),
            Err(error) => warn!("SchedulerActor: failed to schedule deadline wake: {error}"),
        }
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
                    self.enqueue_schedule(&schedule).await;
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
    async fn enqueue_schedule(&mut self, schedule: &Schedule) {
        match &schedule.trigger {
            ScheduleTrigger::Interval { .. } | ScheduleTrigger::OnceAt { .. } => {
                if let Some(next) = schedule.next_run_at {
                    self.deadline_queue.insert(next, schedule.public_id.clone());
                    // Queue head may have changed — reschedule wake
                    self.reschedule_wake().await;
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
        let schedule = match self.schedule_store.get_schedule(schedule_public_id).await {
            Ok(Some(schedule)) => schedule,
            Ok(None) => {
                #[cfg(feature = "remote")]
                self.waiting_for_policy.remove(schedule_public_id);
                return;
            }
            Err(error) => {
                warn!("SchedulerActor: failed to load {schedule_public_id}: {error}");
                return;
            }
        };
        self.fire_loaded_schedule(schedule_public_id, schedule)
            .await;
    }

    #[cfg(feature = "remote")]
    async fn retry_policy_deferred(&mut self) {
        let deferred: Vec<_> = self.waiting_for_policy.iter().cloned().collect();
        for id in deferred {
            self.fire_schedule(&id).await;
        }
    }

    async fn fire_loaded_schedule(&mut self, schedule_public_id: &str, schedule: Schedule) {
        if self.has_pending_delete(schedule_public_id) {
            return;
        }

        #[cfg(feature = "remote")]
        if schedule.state != ScheduleState::Armed {
            self.waiting_for_policy.remove(schedule_public_id);
        }

        // Leave a due peer schedule armed until the host's policy store is ready.
        // No cycle or timeout exists while it waits for reconciliation.
        #[cfg(feature = "remote")]
        if schedule.state == ScheduleState::Armed
            && matches!(
                schedule.config.origin,
                crate::session::domain_schedule::ScheduleOrigin::Peer { .. }
            )
            && !self
                .config
                .provider
                .mesh_handle()
                .is_some_and(|mesh| mesh.quota_store().is_some())
        {
            self.waiting_for_policy
                .insert(schedule_public_id.to_owned());
            return;
        }

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

        #[cfg(feature = "remote")]
        self.waiting_for_policy.remove(schedule_public_id);

        if !cas_ok {
            self.metrics.cas_conflicts_total += 1;
            debug!(
                "SchedulerActor: CAS conflict for {} (Armed→Running denied, schedule_public_id={}, cas_conflicts_total={})",
                schedule_public_id, schedule_public_id, self.metrics.cas_conflicts_total
            );
            return;
        }

        // The cycle's run id. The session runs the prompt under it, and every
        // terminal event of the cycle (its run's, or the timeout's) names it.
        let turn_id = querymt_agent_core::model::TurnId::new();

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
                Some(&turn_id.to_string()),
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
            turn_id,
            task_public_id: schedule.task_public_id.clone(),
            prompt_text,
            execution_limits: schedule.config.execution_limits.clone(),
            origin: schedule.config.origin.clone(),
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
                    Some(&turn_id.to_string()),
                    &format!("failed to send ScheduledPrompt: {}", e),
                )
                .await;
                return;
            }
        }

        // Schedule a timeout message for this active cycle.
        let max_runtime = schedule.config.max_runtime_seconds;
        let Some(actor_ref) = self.self_ref.as_ref() else {
            warn!(
                "SchedulerActor: cannot schedule timeout for {} without self_ref",
                schedule_public_id
            );
            return;
        };
        let timeout = SetTimeout::new(
            actor_ref.downgrade(),
            Duration::from_secs(max_runtime),
            CycleFailed {
                schedule_public_id: schedule_public_id.to_string(),
                turn_id: Some(turn_id.to_string()),
                error: format!("cycle exceeded max_runtime_seconds ({})", max_runtime),
            },
        );
        let Some(timer_scheduler) = self.timer_scheduler.as_ref() else {
            warn!("SchedulerActor: cycle timer scheduler is not running");
            return;
        };
        let timeout_handle = match timer_scheduler.ask(timeout).await {
            Ok(handle) => handle,
            Err(error) => {
                warn!(
                    "SchedulerActor: failed to schedule cycle timeout for {}: {}",
                    schedule_public_id, error
                );
                return;
            }
        };

        self.active_cycles.insert(
            schedule_public_id.to_string(),
            ActiveCycle {
                turn_id: turn_id.to_string(),
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

    /// Whether a terminal event naming `turn_id` belongs to an earlier cycle
    /// of the schedule than the one in flight: an earlier cycle that the
    /// scheduler already ended (its timeout fired) and whose run ended late.
    /// It must not end the current cycle or count against the schedule.
    fn ends_an_earlier_cycle(&self, schedule_public_id: &str, turn_id: Option<&str>) -> bool {
        match (self.active_cycles.get(schedule_public_id), turn_id) {
            (Some(cycle), Some(turn_id)) => cycle.turn_id != turn_id,
            _ => false,
        }
    }

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

        if self.ends_an_earlier_cycle(schedule_public_id, Some(turn_id)) {
            self.metrics.idempotent_drops_total += 1;
            debug!(
                "SchedulerActor: CycleCompleted of an earlier cycle dropped (schedule={}, turn={})",
                schedule_public_id, turn_id
            );
            return;
        }

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
        } else if updated.state == ScheduleState::Paused {
            if let Err(error) = self.schedule_store.update_schedule(updated).await {
                warn!(
                    "SchedulerActor: failed to record completion for {schedule_public_id}: {error}"
                );
            }
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
            self.enqueue_schedule(&updated).await;
        }

        self.apply_pending_delete(schedule_public_id).await;

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
        // Idempotency guard. A failure that names no run (startup recovery, a
        // reconciliation sweep) has nothing to be a duplicate of, and recording
        // it would drop every later such failure of the schedule.
        if let Some(turn_id) = turn_id {
            let key = (schedule_public_id.to_string(), Some(turn_id.to_string()));
            if self.processed_terminals.contains(&key) {
                self.metrics.idempotent_drops_total += 1;
                debug!(
                    "SchedulerActor: duplicate CycleFailed dropped (schedule={}, turn={}, idempotent_drops_total={})",
                    schedule_public_id, turn_id, self.metrics.idempotent_drops_total
                );
                return;
            }
            self.processed_terminals.insert(key);
        }

        if self.ends_an_earlier_cycle(schedule_public_id, turn_id) {
            self.metrics.idempotent_drops_total += 1;
            debug!(
                "SchedulerActor: CycleFailed of an earlier cycle dropped (schedule={}, turn={:?})",
                schedule_public_id, turn_id
            );
            return;
        }

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
        } else if updated.state == ScheduleState::Paused {
            if let Err(error) = self.schedule_store.update_schedule(updated).await {
                warn!("SchedulerActor: failed to record failure for {schedule_public_id}: {error}");
            }
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
            self.enqueue_schedule(&updated).await;
        }

        self.apply_pending_delete(schedule_public_id).await;

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
    pub async fn handle_add_schedule(&mut self, schedule: Schedule) -> SessionResult<()> {
        let public_id = schedule.public_id.clone();
        let session_public_id = schedule.session_public_id.clone();
        let task_public_id = schedule.task_public_id.clone();

        let created = self.schedule_store.create_schedule(schedule).await?;
        self.enqueue_schedule(&created).await;
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

        Ok(())
    }

    /// Remove a schedule. If running, defers deletion until after the terminal event.
    pub async fn handle_remove_schedule(&mut self, schedule_public_id: &str) -> SessionResult<()> {
        // Check if currently running
        if self.active_cycles.contains_key(schedule_public_id) {
            info!(
                "SchedulerActor: schedule {} is running, deferring delete",
                schedule_public_id
            );
            self.pending_deletes.insert(schedule_public_id.to_string());
            self.update_pending_delete_metrics();
            return Ok(());
        }
        self.delete_schedule(schedule_public_id).await
    }

    fn has_pending_delete(&self, id: &str) -> bool {
        #[cfg(feature = "remote")]
        if self.pending_peer_deletes.contains_key(id) {
            return true;
        }
        self.pending_deletes.contains(id)
    }

    fn update_pending_delete_metrics(&mut self) {
        let count = self.pending_deletes.len();
        #[cfg(feature = "remote")]
        let count = count
            + self
                .pending_peer_deletes
                .keys()
                .filter(|id| !self.pending_deletes.contains(*id))
                .count();
        self.metrics.pending_deletes = count as u64;
    }

    async fn apply_pending_delete(&mut self, id: &str) {
        if self.active_cycles.contains_key(id) {
            return;
        }
        if !self.pending_deletes.contains(id) {
            #[cfg(feature = "remote")]
            {
                let Some(peer) = self.pending_peer_deletes.get(id) else {
                    return;
                };
                match peer.check_schedule_mutation().await {
                    Ok(()) => {}
                    Err(crate::error::AgentError::MeshAccessDenied) => {
                        self.pending_peer_deletes.remove(id);
                        self.update_pending_delete_metrics();
                        return;
                    }
                    Err(error) => {
                        warn!(
                            "SchedulerActor: deferred delete authorization failed for {id}: {error}"
                        );
                        return;
                    }
                }
            }
            #[cfg(not(feature = "remote"))]
            return;
        }
        if let Err(error) = self.delete_schedule(id).await {
            // The schedule may have already disappeared (e.g. its session was deleted).
            if matches!(self.schedule_store.get_schedule(id).await, Ok(None)) {
                self.clear_pending_delete(id);
            } else {
                warn!("SchedulerActor: deferred delete failed for {id}: {error}");
            }
        }
    }

    fn clear_pending_delete(&mut self, id: &str) {
        self.pending_deletes.remove(id);
        #[cfg(feature = "remote")]
        self.pending_peer_deletes.remove(id);
        self.update_pending_delete_metrics();
    }

    async fn delete_schedule(&mut self, schedule_public_id: &str) -> SessionResult<()> {
        self.schedule_store
            .delete_schedule(schedule_public_id)
            .await?;

        // Remove from in-memory queues after the durable mutation.
        self.clear_pending_delete(schedule_public_id);
        #[cfg(feature = "remote")]
        self.waiting_for_policy.remove(schedule_public_id);
        self.deadline_queue.remove(schedule_public_id);
        self.event_counters.remove(schedule_public_id);
        self.metrics.armed_interval_schedules = self.deadline_queue.len() as u64;
        self.metrics.armed_event_schedules = self.event_counters.len() as u64;
        self.reschedule_wake().await;

        info!("SchedulerActor: removed schedule {}", schedule_public_id);
        Ok(())
    }

    /// Pause a schedule.
    pub async fn handle_pause_schedule(&mut self, schedule_public_id: &str) -> SessionResult<()> {
        // Try CAS from Armed -> Paused
        let from_armed = self
            .schedule_store
            .update_schedule_state(
                schedule_public_id,
                ScheduleState::Armed,
                ScheduleState::Paused,
            )
            .await?;

        if !from_armed {
            // Try CAS from Running -> Paused (pause requested; current turn completes)
            let from_running = self
                .schedule_store
                .update_schedule_state(
                    schedule_public_id,
                    ScheduleState::Running,
                    ScheduleState::Paused,
                )
                .await?;

            if !from_running {
                debug!(
                    "SchedulerActor: pause failed for {} (not armed or running)",
                    schedule_public_id
                );
                return Ok(());
            }
        }

        // Remove from in-memory queues
        #[cfg(feature = "remote")]
        self.waiting_for_policy.remove(schedule_public_id);
        self.deadline_queue.remove(schedule_public_id);
        if let Some(acc) = self.event_counters.get_mut(schedule_public_id) {
            acc.reset();
        }
        self.reschedule_wake().await;

        self.config.emit_event(
            "", // Session ID not easily available here; use empty for scheduler-level events
            AgentEventKind::SchedulePaused {
                schedule_public_id: schedule_public_id.to_string(),
            },
        );

        info!("SchedulerActor: paused schedule {}", schedule_public_id);
        Ok(())
    }

    /// Resume a paused schedule.
    pub async fn handle_resume_schedule(&mut self, schedule_public_id: &str) -> SessionResult<()> {
        let cas_ok = self
            .schedule_store
            .update_schedule_state(
                schedule_public_id,
                ScheduleState::Paused,
                ScheduleState::Armed,
            )
            .await?;

        if !cas_ok {
            debug!(
                "SchedulerActor: resume failed for {} (not paused)",
                schedule_public_id
            );
            return Ok(());
        }

        // Reload and re-enqueue
        if let Some(schedule) = self.schedule_store.get_schedule(schedule_public_id).await? {
            // Recompute next_run_at for interval schedules
            if let ScheduleTrigger::Interval { seconds } = &schedule.trigger {
                let mut updated = schedule.clone();
                updated.next_run_at = Some(wake::compute_next_run_at(
                    *seconds,
                    schedule.config.jitter_percent,
                    OffsetDateTime::now_utc(),
                ));
                updated.updated_at = OffsetDateTime::now_utc();
                self.schedule_store.update_schedule(updated.clone()).await?;
                self.enqueue_schedule(&updated).await;
            } else {
                self.enqueue_schedule(&schedule).await;
            }
        }

        self.config.emit_event(
            "",
            AgentEventKind::ScheduleResumed {
                schedule_public_id: schedule_public_id.to_string(),
            },
        );

        info!("SchedulerActor: resumed schedule {}", schedule_public_id);
        Ok(())
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

                        if let Some(actor_ref) = self.self_ref.as_ref() {
                            let timeout = SetTimeout::new(
                                actor_ref.downgrade(),
                                Duration::from_secs(debounce_secs),
                                DebounceCompleted {
                                    schedule_public_id: schedule_public_id.to_string(),
                                },
                            );
                            if let Some(timer_scheduler) = self.timer_scheduler.as_ref() {
                                match timer_scheduler.ask(timeout).await {
                                    Ok(handle) => acc.debounce_handle = Some(handle),
                                    Err(error) => warn!(
                                        "SchedulerActor: failed to schedule debounce for {}: {}",
                                        schedule_public_id, error
                                    ),
                                }
                            }
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
        let schedule = self.schedule_store.get_schedule(schedule_public_id).await;
        self.deadline_queue.remove(schedule_public_id);
        match schedule {
            Ok(Some(schedule)) if schedule.state == ScheduleState::Armed => {
                if let Some(next) = schedule.next_run_at {
                    if next > OffsetDateTime::now_utc() {
                        // A queued wake can outlive a trigger or a pause/resume.
                        self.enqueue_schedule(&schedule).await;
                        return;
                    }
                    self.fire_loaded_schedule(schedule_public_id, schedule)
                        .await;
                }
            }
            Err(error) => warn!("SchedulerActor: failed to load {schedule_public_id}: {error}"),
            _ => {}
        }
        self.reschedule_wake().await;
    }

    /// Handle a TriggerNow request — fire immediately regardless of deadline.
    pub async fn handle_trigger_now(&mut self, schedule_public_id: &str) {
        self.deadline_queue.remove(schedule_public_id);
        self.reschedule_wake().await;
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

        let pending = self.pending_deletes.clone();
        #[cfg(feature = "remote")]
        let pending = pending
            .into_iter()
            .chain(self.pending_peer_deletes.keys().cloned())
            .collect::<HashSet<_>>();
        for id in pending {
            self.apply_pending_delete(&id).await;
        }

        #[cfg(feature = "remote")]
        if self
            .config
            .provider
            .mesh_handle()
            .is_some_and(|mesh| mesh.quota_store().is_some())
        {
            self.retry_policy_deferred().await;
        }

        // 2. Check for armed schedules that may have been missed
        if let Ok(armed) = self.schedule_store.list_all_armed_schedules().await {
            let now = OffsetDateTime::now_utc();
            for schedule in armed {
                if let Some(next_run) = schedule.next_run_at
                    && next_run <= now
                    && !self.has_pending_delete(&schedule.public_id)
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

    #[derive(Actor)]
    struct TimerTarget(tokio::sync::oneshot::Sender<()>);

    struct TimerElapsed;

    impl Message<TimerElapsed> for TimerTarget {
        type Reply = ();

        async fn handle(
            &mut self,
            _msg: TimerElapsed,
            ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let (replacement, _rx) = tokio::sync::oneshot::channel();
            let sender = std::mem::replace(&mut self.0, replacement);
            let _ = sender.send(());
            ctx.stop();
        }
    }

    #[tokio::test]
    async fn timer_scheduler_sends_timeout_message() {
        let timer_scheduler = TimerScheduler::spawn(TimerScheduler::new());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let target = TimerTarget::spawn(TimerTarget(tx));

        let handle = timer_scheduler
            .ask(SetTimeout::new(
                target.downgrade(),
                Duration::from_millis(10),
                TimerElapsed,
            ))
            .await
            .expect("schedule timeout");

        tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("scheduled message timeout")
            .expect("scheduled message sender dropped");
        assert!(handle.is_finished());
        timer_scheduler.kill();
    }

    /// Verify that `abort_background_tasks()` cancels the shutdown token.
    ///
    /// This is the mechanism that makes all background loops (lease renewal,
    /// event subscription, reconciliation) exit immediately on shutdown instead
    /// of lingering until their next failed `tell()` attempt.
    #[tokio::test]
    async fn shutdown_token_is_cancelled_on_abort() {
        use crate::session::repo_schedule::SqliteScheduleRepository;
        use crate::session::schema;
        use crate::test_utils::{
            MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
        };
        use querymt::LLMParams;

        // Build a minimal SchedulerActor (not spawned — direct field access)
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::init_schema(&mut conn).unwrap();
        let db = std::sync::Arc::new(parking_lot::Mutex::new(conn));
        let schedule_store = Arc::new(SqliteScheduleRepository::new(db))
            as Arc<dyn crate::session::repository::ScheduleRepository>;

        let mut store = MockSessionStore::new();
        store
            .expect_list_sessions()
            .returning(|| Ok(vec![]))
            .times(0..);
        let session_store: Arc<dyn crate::session::store::SessionStore> = Arc::new(store);

        let provider = Arc::new(tokio::sync::Mutex::new(
            crate::test_utils::MockLlmProvider::new(),
        ));
        let shared = SharedLlmProvider {
            inner: provider,
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared));
        let (plugin_registry, _temp_dir) = mock_plugin_registry(factory).unwrap();
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create storage"),
        );
        let config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                storage.clone(),
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .build(),
        );

        let registry = Arc::new(tokio::sync::Mutex::new(
            crate::agent::session_registry::SessionRegistry::new(config.clone()),
        ));

        let mut actor = SchedulerActor::new(
            schedule_store,
            session_store,
            registry,
            config,
            SchedulerConfig::default(),
        );

        // Token should start uncancelled
        let token_clone = actor.shutdown_token.clone();
        assert!(
            !token_clone.is_cancelled(),
            "shutdown token should not be cancelled before abort"
        );

        // Abort should cancel the token
        actor.abort_background_tasks();
        assert!(
            token_clone.is_cancelled(),
            "shutdown token must be cancelled after abort_background_tasks()"
        );
    }

    /// Verify that a background loop using the same `select!` pattern as the
    /// scheduler loops exits immediately when the token is cancelled, rather
    /// than waiting for the next interval tick (which could be 60+ seconds).
    #[tokio::test]
    async fn background_loop_exits_on_token_cancellation() {
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Spawn a task that mimics the reconciliation loop pattern:
        // `select!` on a long interval vs token cancellation.
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600)); // 1 hour
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Consume the first immediate tick
            interval.tick().await;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Would normally send a message here
                    }
                    _ = token_clone.cancelled() => {
                        break;
                    }
                }
            }
        });

        // Cancel the token
        token.cancel();

        // The task must complete almost instantly (not wait for the 1h interval)
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), handle).await;
        assert!(
            result.is_ok(),
            "background loop must exit within 100ms of token cancellation, not wait for interval"
        );
        assert!(
            result.unwrap().is_ok(),
            "background loop task should complete without panic"
        );
    }

    /// A scheduler (not spawned) over an in-memory store holding one schedule,
    /// `"sched"`, in the `Running` state.
    async fn scheduler_with_running_schedule() -> (SchedulerActor, Arc<dyn ScheduleRepository>) {
        let (actor, store, _) = scheduler_with_db().await;
        (actor, store)
    }

    async fn scheduler_with_db() -> (
        SchedulerActor,
        Arc<dyn ScheduleRepository>,
        Arc<parking_lot::Mutex<rusqlite::Connection>>,
    ) {
        use crate::session::domain_schedule::ScheduleConfig;
        use crate::session::repo_schedule::SqliteScheduleRepository;
        use crate::session::schema;
        use crate::test_utils::{
            MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
        };
        use querymt::LLMParams;

        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::init_schema(&mut conn).unwrap();
        let (session_id, task_id) = {
            let now = OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            conn.execute(
                "INSERT INTO sessions (public_id, created_at, updated_at) VALUES (?, ?, ?)",
                rusqlite::params!["sess-1", now, now],
            )
            .unwrap();
            let session_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO tasks (public_id, session_id, kind, status, created_at, updated_at) VALUES (?, ?, 'recurring', 'active', ?, ?)",
                rusqlite::params!["task-1", session_id, now, now],
            )
            .unwrap();
            (session_id, conn.last_insert_rowid())
        };
        let db = Arc::new(parking_lot::Mutex::new(conn));
        let schedule_store = Arc::new(SqliteScheduleRepository::new(db.clone()))
            as Arc<dyn crate::session::repository::ScheduleRepository>;
        schedule_store
            .create_schedule(Schedule {
                id: 0,
                public_id: "sched".to_string(),
                task_id,
                task_public_id: "task-1".to_string(),
                session_id,
                session_public_id: "sess-1".to_string(),
                trigger: ScheduleTrigger::Interval { seconds: 3600 },
                state: ScheduleState::Running,
                last_run_at: None,
                next_run_at: None,
                run_count: 0,
                consecutive_failures: 0,
                config: ScheduleConfig::default(),
                created_at: OffsetDateTime::now_utc(),
                updated_at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();

        let session_store: Arc<dyn crate::session::store::SessionStore> =
            Arc::new(MockSessionStore::new());
        let provider = Arc::new(tokio::sync::Mutex::new(
            crate::test_utils::MockLlmProvider::new(),
        ));
        let shared = SharedLlmProvider {
            inner: provider,
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared));
        let (plugin_registry, _temp_dir) = mock_plugin_registry(factory).unwrap();
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create storage"),
        );
        let config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                storage,
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .build(),
        );
        let registry = Arc::new(tokio::sync::Mutex::new(
            crate::agent::session_registry::SessionRegistry::new(config.clone()),
        ));
        let actor = SchedulerActor::new(
            schedule_store.clone(),
            session_store,
            registry,
            config,
            SchedulerConfig::default(),
        );
        (actor, schedule_store, db)
    }

    #[cfg(feature = "remote")]
    use querymt_remote::{
        NodeId,
        peer_access::{
            GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState, PolicyChange,
            PolicyEffect,
        },
    };
    #[cfg(feature = "remote")]
    use tokio::sync::broadcast;

    #[cfg(feature = "remote")]
    struct ScheduleGrants {
        changes: broadcast::Sender<PolicyChange>,
        unavailable: std::sync::atomic::AtomicBool,
    }

    #[cfg(feature = "remote")]
    #[async_trait::async_trait]
    impl GrantStore for ScheduleGrants {
        async fn grant(&self, _: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
            if self.unavailable.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(GrantStoreError::Backend(
                    std::io::Error::other("test policy outage").into(),
                ));
            }
            Ok(PeerGrantState {
                revision: uuid::Uuid::nil(),
                grant: PeerGrant::Full,
            })
        }
        async fn replace_grant(
            &self,
            _: &PeerGrantKey,
            _: uuid::Uuid,
            _: PeerGrant,
        ) -> Result<PeerGrantState, GrantStoreError> {
            unreachable!()
        }
        async fn managed_peers(&self, _: &NodeId) -> Result<Vec<NodeId>, GrantStoreError> {
            unreachable!()
        }
        fn subscribe_changes(&self) -> Option<broadcast::Receiver<PolicyChange>> {
            Some(self.changes.subscribe())
        }
    }

    #[cfg(feature = "remote")]
    async fn grant_schedule(
        store: &dyn ScheduleRepository,
    ) -> (
        Arc<crate::agent::session_actor::PeerTurn>,
        Arc<ScheduleGrants>,
    ) {
        let grants = Arc::new(ScheduleGrants {
            changes: broadcast::channel(8).0,
            unavailable: false.into(),
        });
        let peer = crate::agent::session_actor::PeerTurn::test_peer(
            PeerGrantKey {
                host: NodeId::from_peer_id(libp2p::PeerId::random()),
                peer: NodeId::from_peer_id(libp2p::PeerId::random()),
            },
            PeerGrant::Full,
            grants.clone(),
        );
        let mut schedule = store.get_schedule("sched").await.unwrap().unwrap();
        schedule.config.origin = peer.schedule_origin();
        store.update_schedule(schedule).await.unwrap();
        (peer, grants)
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn queued_peer_schedule_mutation_rechecks_authority_and_preserves_storage_errors() {
        use crate::{agent::session_actor::PeerTurn, error::AgentError};
        use kameo::error::SendError;

        let (actor, store, db) = scheduler_with_db().await;
        let (admitted, grant_store) = grant_schedule(store.as_ref()).await;
        let key = admitted.key.clone();
        // The current grant is Full again, but the queued request crossed a revocation.
        grant_store
            .changes
            .send(PolicyChange {
                key: key.clone(),
                effect: PolicyEffect::Grant(PeerGrant::None),
            })
            .unwrap();
        let handle = SchedulerHandle::new(kameo::actor::Spawn::spawn(actor));
        let delete = || PeerScheduleMutation::Action(ScheduleAction::Delete, "sched".into());
        assert!(matches!(
            handle.peer_mutation(admitted, delete()).await,
            Err(SendError::HandlerError(AgentError::MeshAccessDenied))
        ));
        assert!(store.get_schedule("sched").await.unwrap().is_some());

        db.lock()
            .execute_batch("ALTER TABLE schedules RENAME TO schedules_unavailable")
            .unwrap();
        let fresh = PeerTurn::test_peer(key, PeerGrant::Full, grant_store);
        assert!(matches!(
            handle.peer_mutation(fresh, delete()).await,
            Err(SendError::HandlerError(AgentError::Internal(_)))
        ));
        handle.shutdown().await;
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn deferred_lookup_failure_preserves_firing_and_reports_storage_error() {
        let (mut actor, store, db) = scheduler_with_db().await;
        actor.waiting_for_policy.insert("sched".into());
        store
            .update_schedule_state("sched", ScheduleState::Running, ScheduleState::Armed)
            .await
            .unwrap();
        db.lock()
            .execute_batch("ALTER TABLE schedules RENAME TO schedules_unavailable")
            .unwrap();
        actor.retry_policy_deferred().await;
        assert!(actor.waiting_for_policy.contains("sched"));

        db.lock()
            .execute_batch("ALTER TABLE schedules_unavailable RENAME TO schedules")
            .unwrap();
        actor.retry_policy_deferred().await;
        assert!(!actor.waiting_for_policy.contains("sched"));
        assert_eq!(
            store
                .get_schedule("sched")
                .await
                .unwrap()
                .unwrap()
                .consecutive_failures,
            1
        );

        db.lock()
            .execute_batch("ALTER TABLE schedules RENAME TO schedules_unavailable")
            .unwrap();
        let handle = SchedulerHandle::new(kameo::actor::Spawn::spawn(actor));
        assert!(handle.get_schedule("sched").await.is_err());
        handle.shutdown().await;
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn a_started_cycle_clears_its_deferred_entry_before_backoff() {
        let (mut actor, store) = scheduler_with_running_schedule().await;
        actor.waiting_for_policy.insert("sched".into());
        store
            .update_schedule_state("sched", ScheduleState::Running, ScheduleState::Armed)
            .await
            .unwrap();
        actor.fire_schedule("sched").await;
        let backed_off = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(backed_off.state, ScheduleState::Armed);
        assert!(backed_off.next_run_at.unwrap() > OffsetDateTime::now_utc());
        actor.retry_policy_deferred().await;
        let after_retry = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(after_retry.consecutive_failures, 1);
        assert_eq!(after_retry.next_run_at, backed_off.next_run_at);
    }

    /// Put a cycle of `"sched"` in flight, running as `turn_id`.
    fn start_cycle(actor: &mut SchedulerActor, turn_id: &str) {
        actor.active_cycles.insert(
            "sched".to_string(),
            ActiveCycle {
                turn_id: turn_id.to_string(),
                started_at: OffsetDateTime::now_utc(),
                timeout_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            },
        );
    }

    /// An earlier cycle that the scheduler already ended (its timeout fired)
    /// and whose run ends late must not end the cycle that follows it.
    #[tokio::test]
    async fn a_late_failure_of_an_earlier_cycle_leaves_the_current_cycle_alone() {
        let (mut actor, store) = scheduler_with_running_schedule().await;
        start_cycle(&mut actor, "cycle-b");

        actor
            .handle_cycle_failed_inner("sched", Some("cycle-a"), "run of cycle a failed late")
            .await;

        assert!(
            actor.active_cycles.contains_key("sched"),
            "cycle b is still in flight"
        );
        let schedule = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(schedule.consecutive_failures, 0);
        assert_eq!(schedule.state, ScheduleState::Running);

        // Its own failure is applied.
        actor
            .handle_cycle_failed_inner("sched", Some("cycle-b"), "run of cycle b failed")
            .await;
        assert!(!actor.active_cycles.contains_key("sched"));
        let schedule = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(schedule.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn pause_survives_active_cycle_completion_and_failure_until_resumed() {
        let (mut actor, store) = scheduler_with_running_schedule().await;
        start_cycle(&mut actor, "completed");
        actor.handle_pause_schedule("sched").await.unwrap();
        actor.handle_cycle_completed("sched", "completed").await;
        let paused = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(paused.state, ScheduleState::Paused);
        assert_eq!(paused.run_count, 1);
        assert!(actor.deadline_queue.peek().is_none());

        actor.handle_resume_schedule("sched").await.unwrap();
        let resumed = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(resumed.state, ScheduleState::Armed);
        assert!(resumed.next_run_at.is_some());
        assert_eq!(actor.deadline_queue.peek().unwrap().1, "sched");

        store
            .update_schedule_state("sched", ScheduleState::Armed, ScheduleState::Running)
            .await
            .unwrap();
        start_cycle(&mut actor, "failed");
        actor.handle_pause_schedule("sched").await.unwrap();
        actor
            .handle_cycle_failed_inner("sched", Some("failed"), "test failure")
            .await;
        let paused = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(paused.state, ScheduleState::Paused);
        assert_eq!(paused.consecutive_failures, 1);
        assert!(actor.deadline_queue.peek().is_none());
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn peer_pause_and_resume_report_write_errors_without_treating_noops_as_errors() {
        use crate::error::AgentError;
        use kameo::error::SendError;

        let (actor, store, db) = scheduler_with_db().await;
        let (peer, _) = grant_schedule(store.as_ref()).await;
        let handle = SchedulerHandle::new(kameo::actor::Spawn::spawn(actor));
        let action = |action| PeerScheduleMutation::Action(action, "sched".into());
        let fail_write = "CREATE TRIGGER reject_state BEFORE UPDATE OF state ON schedules BEGIN SELECT RAISE(ABORT, 'test write outage'); END;";
        // The first pause CAS misses (Running); the fallback write must report the error.
        db.lock().execute_batch(fail_write).unwrap();
        assert!(matches!(
            handle
                .peer_mutation(peer.clone(), action(ScheduleAction::Pause))
                .await,
            Err(SendError::HandlerError(AgentError::Internal(_)))
        ));
        db.lock()
            .execute_batch("DROP TRIGGER reject_state;")
            .unwrap();
        handle
            .peer_mutation(peer.clone(), action(ScheduleAction::Pause))
            .await
            .unwrap();
        handle
            .peer_mutation(peer.clone(), action(ScheduleAction::Pause))
            .await
            .unwrap();
        db.lock().execute_batch(fail_write).unwrap();
        assert!(matches!(
            handle
                .peer_mutation(peer.clone(), action(ScheduleAction::Resume))
                .await,
            Err(SendError::HandlerError(AgentError::Internal(_)))
        ));
        assert_eq!(
            store.get_schedule("sched").await.unwrap().unwrap().state,
            ScheduleState::Paused
        );
        db.lock()
            .execute_batch("DROP TRIGGER reject_state;")
            .unwrap();
        handle
            .peer_mutation(peer, action(ScheduleAction::Resume))
            .await
            .unwrap();
        assert_eq!(
            store.get_schedule("sched").await.unwrap().unwrap().state,
            ScheduleState::Armed
        );
        handle.shutdown().await;
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn deferred_peer_delete_blocks_firing_and_rechecks_authority_on_retry() {
        use std::sync::atomic::Ordering::Relaxed;

        for revoke in [false, true] {
            let (mut actor, store, db) = scheduler_with_db().await;
            let (peer, grants) = grant_schedule(store.as_ref()).await;
            start_cycle(&mut actor, "cycle");
            let actor_ref = kameo::actor::Spawn::spawn(actor);
            let handle = SchedulerHandle::new(actor_ref.clone());
            handle
                .peer_mutation(
                    peer.clone(),
                    PeerScheduleMutation::Action(ScheduleAction::Delete, "sched".into()),
                )
                .await
                .unwrap();
            db.lock().execute_batch("CREATE TRIGGER reject_delete BEFORE DELETE ON schedules BEGIN SELECT RAISE(ABORT, 'test delete outage'); END;").unwrap();
            actor_ref
                .ask(CycleCompleted {
                    schedule_public_id: "sched".into(),
                    turn_id: "cycle".into(),
                })
                .await
                .unwrap();
            assert_eq!(handle.metrics().await.pending_deletes, 1);

            let mut schedule = store.get_schedule("sched").await.unwrap().unwrap();
            schedule.next_run_at = Some(OffsetDateTime::now_utc() - time::Duration::SECOND);
            store.update_schedule(schedule).await.unwrap();
            db.lock()
                .execute_batch("DROP TRIGGER reject_delete;")
                .unwrap();
            grants.unavailable.store(true, Relaxed);
            actor_ref.ask(Reconcile).await.unwrap();
            let metrics = handle.metrics().await;
            assert_eq!(metrics.pending_deletes, 1);
            assert_eq!(metrics.reconciliation_overdue_fires_total, 0);

            if revoke {
                grants
                    .changes
                    .send(PolicyChange {
                        key: peer.key.clone(),
                        effect: PolicyEffect::Grant(PeerGrant::None),
                    })
                    .unwrap();
            }
            grants.unavailable.store(false, Relaxed);
            actor_ref.ask(Reconcile).await.unwrap();
            assert_eq!(handle.metrics().await.pending_deletes, 0);
            assert_eq!(store.get_schedule("sched").await.unwrap().is_some(), revoke);
            handle.shutdown().await;
        }
    }

    #[tokio::test]
    async fn queued_deadline_does_not_fire_before_the_new_deadline() {
        let (mut actor, store) = scheduler_with_running_schedule().await;
        start_cycle(&mut actor, "triggered");
        actor.handle_cycle_completed("sched", "triggered").await;
        let mut armed = store.get_schedule("sched").await.unwrap().unwrap();
        actor.handle_deadline_reached("sched").await;
        let current = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(current.next_run_at, armed.next_run_at);
        assert_eq!(current.consecutive_failures, 0);
        assert_eq!(actor.deadline_queue.peek().unwrap().1, "sched");

        armed.next_run_at = Some(OffsetDateTime::now_utc() - time::Duration::SECOND);
        store.update_schedule(armed).await.unwrap();
        actor.handle_deadline_reached("sched").await;
        // The due cycle attempts to run but the fixture has no live session.
        assert_eq!(actor.metrics.failures_total, 1);
    }

    #[tokio::test]
    async fn a_late_completion_of_an_earlier_cycle_leaves_the_current_cycle_alone() {
        let (mut actor, store) = scheduler_with_running_schedule().await;
        start_cycle(&mut actor, "cycle-b");

        actor.handle_cycle_completed("sched", "cycle-a").await;

        assert!(actor.active_cycles.contains_key("sched"));
        let schedule = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(schedule.run_count, 0);
        assert_eq!(schedule.state, ScheduleState::Running);
    }

    /// A failure that names no run (startup recovery, a reconciliation sweep)
    /// is applied every time; it is not a duplicate of the one before it.
    #[tokio::test]
    async fn failures_that_name_no_run_are_each_applied() {
        let (mut actor, store) = scheduler_with_running_schedule().await;

        actor
            .handle_cycle_failed_inner("sched", None, "recovered by reconciliation sweep")
            .await;
        actor
            .handle_cycle_failed_inner("sched", None, "recovered by reconciliation sweep")
            .await;

        let schedule = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(schedule.consecutive_failures, 2);
    }

    /// The cycle's own run is reported once: a repeat of its terminal event
    /// is dropped.
    #[tokio::test]
    async fn the_terminal_event_of_a_cycle_is_applied_once() {
        let (mut actor, store) = scheduler_with_running_schedule().await;
        start_cycle(&mut actor, "cycle-a");

        for _ in 0..2 {
            actor
                .handle_cycle_failed_inner("sched", Some("cycle-a"), "failed")
                .await;
        }

        let schedule = store.get_schedule("sched").await.unwrap().unwrap();
        assert_eq!(schedule.consecutive_failures, 1);
    }
}

#[cfg(test)]
mod tests;
