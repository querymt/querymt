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
use crate::session::repo_schedule::ScheduleRepository;
use crate::session::error::SessionResult;
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use wake::{ActiveCycle, DeadlineQueue, EventAccumulator};

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

/// Handle for interacting with a running `SchedulerActor` from background tasks.
///
/// This is a lightweight handle that allows background loops (lease renewal,
/// event subscription, reconciliation) to call back into the scheduler actor.
#[derive(Clone)]
pub struct SchedulerHandle {
    inner: Arc<Mutex<SchedulerActor>>,
}

impl SchedulerHandle {
    /// Create a new handle wrapping a scheduler actor.
    pub fn new(actor: SchedulerActor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(actor)),
        }
    }

    /// Process an event from the fanout.
    pub async fn process_event(&self, envelope: EventEnvelope) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.process_event(&envelope).await;
        Ok(())
    }

    /// Run reconciliation.
    pub async fn reconcile(&self) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.reconcile().await;
        Ok(())
    }

    /// Fire a schedule by public ID (for TriggerNow).
    pub async fn trigger_now(&self, schedule_public_id: &str) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.handle_trigger_now(schedule_public_id).await;
        Ok(())
    }

    /// Add a schedule.
    pub async fn add_schedule(&self, schedule: Schedule) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.handle_add_schedule(schedule).await;
        Ok(())
    }

    /// Remove a schedule.
    pub async fn remove_schedule(&self, schedule_public_id: &str) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.handle_remove_schedule(schedule_public_id).await;
        Ok(())
    }

    /// Pause a schedule.
    pub async fn pause_schedule(&self, schedule_public_id: &str) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.handle_pause_schedule(schedule_public_id).await;
        Ok(())
    }

    /// Resume a schedule.
    pub async fn resume_schedule(&self, schedule_public_id: &str) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.handle_resume_schedule(schedule_public_id).await;
        Ok(())
    }

    /// List schedules for a session.
    pub async fn list_schedules(
        &self,
        session_public_id: Option<&str>,
    ) -> SessionResult<Vec<Schedule>> {
        let actor = self.inner.lock().await;
        Ok(actor.handle_list_schedules(session_public_id).await)
    }

    /// Handle cycle completed event.
    pub async fn handle_cycle_completed(
        &self,
        schedule_public_id: &str,
        turn_id: &str,
    ) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.handle_cycle_completed(schedule_public_id, turn_id).await;
        Ok(())
    }

    /// Handle deadline reached event (internal, called by deadline wake loop).
    async fn handle_deadline_reached_internal(
        &self,
        schedule_public_id: &str,
    ) -> SessionResult<()> {
        let mut actor = self.inner.lock().await;
        actor.handle_deadline_reached(schedule_public_id).await;
        Ok(())
    }

    /// Shutdown the scheduler and its background tasks.
    pub async fn shutdown(&self) {
        let mut actor = self.inner.lock().await;
        
        // Abort all background tasks
        if let Some(handle) = actor.wake_handle.take() {
            handle.abort();
        }
        if let Some(handle) = actor.reconcile_handle.take() {
            handle.abort();
        }
        if let Some(handle) = actor.lease_renew_handle.take() {
            handle.abort();
        }
        
        info!("SchedulerActor: shutdown complete");
    }
}

/// The scheduler actor. Manages deadline-based and event-driven schedule firing.
///
/// Not derived with `#[derive(Actor)]` because kameo derive may not be available;
/// instead we implement the actor message handlers manually and the actor is
/// managed as a plain struct with a message-processing loop.
pub struct SchedulerActor {
    schedule_store: Arc<dyn ScheduleRepository>,
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

    // Background task handles
    wake_handle: Option<JoinHandle<()>>,
    reconcile_handle: Option<JoinHandle<()>>,
    lease_renew_handle: Option<JoinHandle<()>>,
}

impl SchedulerActor {
    /// Create a new `SchedulerActor`.
    pub fn new(
        schedule_store: Arc<dyn ScheduleRepository>,
        session_registry: Arc<Mutex<SessionRegistry>>,
        config: Arc<AgentConfig>,
        scheduler_config: SchedulerConfig,
    ) -> Self {
        let owner_id = uuid::Uuid::now_v7().to_string();
        Self {
            schedule_store,
            session_registry,
            config,
            scheduler_config,
            owner_id,
            deadline_queue: DeadlineQueue::new(),
            event_counters: HashMap::new(),
            active_cycles: HashMap::new(),
            pending_deletes: HashSet::new(),
            processed_terminals: HashSet::new(),
            wake_handle: None,
            reconcile_handle: None,
            lease_renew_handle: None,
        }
    }

    /// Spawn the scheduler actor, initialize it, and start background loops.
    ///
    /// Returns `None` if the lease could not be acquired (another scheduler is active).
    /// Returns `Some(SchedulerHandle)` if this scheduler became the active leader.
    pub async fn spawn(
        schedule_store: Arc<dyn ScheduleRepository>,
        session_registry: Arc<Mutex<SessionRegistry>>,
        config: Arc<AgentConfig>,
        scheduler_config: SchedulerConfig,
    ) -> Option<SchedulerHandle> {
        let mut actor = Self::new(
            schedule_store,
            session_registry,
            config,
            scheduler_config,
        );

        // Try to acquire lease and initialize
        let acquired = actor.initialize().await;
        if !acquired {
            info!("SchedulerActor: not starting (lease not acquired)");
            return None;
        }

        // Create handle and start background loops
        let handle = SchedulerHandle::new(actor);
        
        // Start background loops
        {
            let mut actor_guard = handle.inner.lock().await;
            actor_guard.start_background_loops(handle.clone());
        }

        info!("SchedulerActor: started successfully");
        Some(handle)
    }

    /// Initialize the scheduler: acquire lease, recover state, populate queues.
    ///
    /// Should be called once after construction. Returns `true` if the lease
    /// was acquired (this scheduler is the active leader).
    pub async fn initialize(&mut self) -> bool {
        // 1. Acquire lease
        let acquired = match self
            .schedule_store
            .try_acquire_scheduler_lease(&self.owner_id, self.scheduler_config.lease_ttl_secs)
            .await
        {
            Ok(true) => {
                info!(
                    "SchedulerActor: lease acquired (owner={})",
                    self.owner_id
                );
                true
            }
            Ok(false) => {
                info!("SchedulerActor: lease not acquired, staying passive");
                return false;
            }
            Err(e) => {
                warn!("SchedulerActor: failed to acquire lease: {}", e);
                return false;
            }
        };

        if !acquired {
            return false;
        }

        // 2. Recover stale Running schedules
        self.recover_stale_running().await;

        // 3. Load all Armed schedules and populate queues
        self.rebuild_queues().await;

        true
    }

    /// Start background loops: lease renewal, event fanout subscription, reconciliation.
    ///
    /// Should be called after successful `initialize()`.
    pub fn start_background_loops(
        &mut self,
        self_handle: SchedulerHandle,
    ) {
        // Start lease renewal loop
        self.start_lease_renewal_loop(self_handle.clone());

        // Start event fanout subscription loop
        self.start_event_subscription_loop(self_handle.clone());

        // Start reconciliation loop
        self.start_reconciliation_loop(self_handle.clone());

        // Start deadline wake loop
        self.start_deadline_wake_loop(self_handle);
    }

    /// Start the lease renewal background task.
    fn start_lease_renewal_loop(&mut self, handle: SchedulerHandle) {
        let owner_id = self.owner_id.clone();
        let schedule_store = self.schedule_store.clone();
        let ttl_secs = self.scheduler_config.lease_ttl_secs;
        let renew_interval = ttl_secs / 2; // Renew at half TTL

        let renew_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(renew_interval)
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                match schedule_store
                    .renew_scheduler_lease(&owner_id, ttl_secs)
                    .await
                {
                    Ok(true) => {
                        debug!("SchedulerActor: lease renewed for {}", owner_id);
                    }
                    Ok(false) => {
                        warn!(
                            "SchedulerActor: lease renewal failed for {} (lost ownership)",
                            owner_id
                        );
                        // Lost lease — should stop processing
                        break;
                    }
                    Err(e) => {
                        warn!(
                            "SchedulerActor: lease renewal error for {}: {}",
                            owner_id, e
                        );
                        // Continue trying on transient errors
                    }
                }
            }

            info!("SchedulerActor: lease renewal loop exited");
        });

        self.lease_renew_handle = Some(renew_handle);
    }

    /// Start the event fanout subscription background task.
    fn start_event_subscription_loop(&mut self, handle: SchedulerHandle) {
        let mut event_rx = self.config.subscribe_events();

        let event_handle = tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(envelope) => {
                        // Forward event to handle for processing
                        if let Err(e) = handle.process_event(envelope).await {
                            warn!("SchedulerActor: failed to process event: {}", e);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("SchedulerActor: event fanout closed, exiting subscription loop");
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("SchedulerActor: event subscription lagged by {} events", n);
                        // Continue processing
                    }
                }
            }
        });

        // Note: We don't store this handle in the actor state because it
        // needs to run independently. It will be aborted when the actor is dropped.
        drop(event_handle);
    }

    /// Start the reconciliation background task.
    fn start_reconciliation_loop(&mut self, handle: SchedulerHandle) {
        let interval_secs = self.scheduler_config.reconcile_interval_secs;

        let reconcile_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(interval_secs)
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                if let Err(e) = handle.reconcile().await {
                    warn!("SchedulerActor: reconciliation failed: {}", e);
                }
            }
        });

        self.reconcile_handle = Some(reconcile_handle);
    }

    /// Start the deadline wake loop for interval-based schedules.
    ///
    /// This loop wakes when the next deadline is reached and fires the schedule.
    fn start_deadline_wake_loop(&mut self, handle: SchedulerHandle) {
        let wake_handle = tokio::spawn(async move {
            loop {
                // Get the next deadline from the queue
                let next_deadline = {
                    let actor = handle.inner.lock().await;
                    actor.deadline_queue.peek().map(|(dt, id)| (dt, id.to_string()))
                };

                match next_deadline {
                    Some((deadline_time, schedule_id)) => {
                        let now = OffsetDateTime::now_utc();
                        
                        if deadline_time <= now {
                            // Deadline has passed, fire immediately
                            if let Err(e) = handle.handle_deadline_reached_internal(&schedule_id).await {
                                warn!("SchedulerActor: failed to handle deadline: {}", e);
                            }
                        } else {
                            // Sleep until deadline
                            let duration = (deadline_time - now).max(time::Duration::ZERO);
                            let std_duration = std::time::Duration::from_secs(
                                duration.whole_seconds().max(0) as u64
                            );
                            
                            tokio::time::sleep(std_duration).await;
                            
                            // Fire the schedule
                            if let Err(e) = handle.handle_deadline_reached_internal(&schedule_id).await {
                                warn!("SchedulerActor: failed to handle deadline: {}", e);
                            }
                        }
                    }
                    None => {
                        // No deadlines, sleep for a while and check again
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    }
                }
            }
        });

        self.wake_handle = Some(wake_handle);
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
    fn enqueue_schedule(&mut self, schedule: &Schedule) {
        match &schedule.trigger {
            ScheduleTrigger::Interval { .. } => {
                if let Some(next) = schedule.next_run_at {
                    self.deadline_queue
                        .insert(next, schedule.public_id.clone());
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
            debug!(
                "SchedulerActor: CAS returned false for {} (already running, paused, or terminal)",
                schedule_public_id
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

        // Build prompt text from the task's expected_deliverable or a default
        let prompt_text = format!(
            "Scheduled execution cycle for schedule {}. Execute the recurring task.",
            schedule_public_id
        );

        // Send ScheduledPrompt to the SessionActor
        let scheduled_prompt = ScheduledPrompt {
            schedule_public_id: schedule_public_id.to_string(),
            prompt_text,
            execution_limits: schedule.config.execution_limits.clone(),
        };

        match actor_ref.tell_scheduled_prompt(scheduled_prompt).await {
            Ok(()) => {
                info!(
                    "SchedulerActor: fired schedule {} → session {}",
                    schedule_public_id, schedule.session_public_id
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

        // Set up timeout handle
        let max_runtime = schedule.config.max_runtime_seconds;
        let schedule_id_for_timeout = schedule_public_id.to_string();
        let config_for_timeout = self.config.clone();
        let session_id_for_timeout = schedule.session_public_id.clone();
        let timeout_handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(max_runtime)).await;
            // Emit a timeout failure event — the scheduler will pick this up
            // via the EventFanout subscription.
            config_for_timeout.emit_event(
                &session_id_for_timeout,
                AgentEventKind::ScheduledExecutionFailed {
                    schedule_public_id: schedule_id_for_timeout,
                    turn_id: None,
                    error: format!("cycle exceeded max_runtime_seconds ({})", max_runtime),
                },
            );
        });

        self.active_cycles.insert(
            schedule_public_id.to_string(),
            ActiveCycle {
                schedule_public_id: schedule_public_id.to_string(),
                session_public_id: schedule.session_public_id.clone(),
                started_at: OffsetDateTime::now_utc(),
                timeout_handle,
            },
        );

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
        let key = (
            schedule_public_id.to_string(),
            Some(turn_id.to_string()),
        );
        if self.processed_terminals.contains(&key) {
            debug!(
                "SchedulerActor: duplicate CycleCompleted for ({}, {}), ignoring",
                schedule_public_id, turn_id
            );
            return;
        }
        self.processed_terminals.insert(key);

        // Remove active cycle and abort timeout
        if let Some(cycle) = self.active_cycles.remove(schedule_public_id) {
            cycle.timeout_handle.abort();
        }

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
            if let Err(e) = self.schedule_store.delete_schedule(schedule_public_id).await {
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
            debug!(
                "SchedulerActor: duplicate CycleFailed for ({}, {:?}), ignoring",
                schedule_public_id, turn_id
            );
            return;
        }
        self.processed_terminals.insert(key);

        // Remove active cycle and abort timeout
        if let Some(cycle) = self.active_cycles.remove(schedule_public_id) {
            cycle.timeout_handle.abort();
        }

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

        let failure_threshold_reached = updated.consecutive_failures
            >= updated.config.max_consecutive_failures;

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
            if let Err(e) = self.schedule_store.delete_schedule(schedule_public_id).await {
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
            return;
        }

        // Remove from in-memory queues
        self.deadline_queue.remove(schedule_public_id);
        self.event_counters.remove(schedule_public_id);

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

        self.config.emit_event(
            "", // Session ID not easily available here; use empty for scheduler-level events
            AgentEventKind::SchedulePaused {
                schedule_public_id: schedule_public_id.to_string(),
            },
        );

        info!(
            "SchedulerActor: paused schedule {}",
            schedule_public_id
        );
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
        if let Ok(Some(schedule)) = self
            .schedule_store
            .get_schedule(schedule_public_id)
            .await
        {
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

        info!(
            "SchedulerActor: resumed schedule {}",
            schedule_public_id
        );
    }

    /// Handle an incoming event from EventFanout that may match an event-driven schedule.
    pub async fn handle_event_received(
        &mut self,
        schedule_public_id: &str,
        _event_kind: &str,
    ) {
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

                        // Spawn a debounce timer task
                        let schedule_id = schedule_public_id.to_string();
                        let config = self.config.clone();
                        let session_id = schedule.session_public_id.clone();
                        let debounce_handle = tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(
                                debounce_secs,
                            ))
                            .await;

                            // Emit a debounce completed event (internal)
                            config.emit_event(
                                &session_id,
                                AgentEventKind::ScheduleDebounceCompleted {
                                    schedule_public_id: schedule_id,
                                },
                            );
                        });

                        acc.debounce_handle = Some(debounce_handle);
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
        self.fire_schedule(schedule_public_id).await;
    }

    /// Handle a TriggerNow request — fire immediately regardless of deadline.
    pub async fn handle_trigger_now(&mut self, schedule_public_id: &str) {
        self.deadline_queue.remove(schedule_public_id);
        self.fire_schedule(schedule_public_id).await;
    }

    /// List schedules, optionally filtered by session.
    pub async fn handle_list_schedules(
        &self,
        session_public_id: Option<&str>,
    ) -> Vec<Schedule> {
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
        debug!("SchedulerActor: running reconciliation sweep");

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
                    warn!(
                        "SchedulerActor: reconciliation recovering stale schedule: {}",
                        schedule.public_id
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
                if let Some(next_run) = schedule.next_run_at {
                    if next_run <= now {
                        info!(
                            "SchedulerActor: reconciliation firing overdue schedule: {}",
                            schedule.public_id
                        );
                        self.fire_schedule(&schedule.public_id).await;
                    }
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
                self.handle_cycle_failed_inner(
                    schedule_public_id,
                    turn_id.as_deref(),
                    error,
                )
                .await;
            }
            AgentEventKind::ScheduleDebounceCompleted {
                schedule_public_id,
            } => {
                self.handle_debounce_completed(schedule_public_id).await;
            }
            // Match event-driven schedule triggers
            _ => {
                // Check if any event accumulator cares about this event kind
                let event_kind_name = event_kind_name(envelope.kind());
                
                // Check each event-driven schedule to see if this event matches its filter
                let schedule_ids: Vec<String> = self.event_counters.keys().cloned().collect();
                
                for schedule_id in schedule_ids {
                    // Load the schedule to check if the event matches its filter
                    if let Ok(Some(schedule)) = self.schedule_store.get_schedule(&schedule_id).await {
                        if let ScheduleTrigger::EventDriven { event_filter, .. } = &schedule.trigger {
                            // Check if event kind matches filter
                            if event_filter.event_kinds.contains(&event_kind_name) {
                                // Check session scope filter if specified
                                let session_matches = event_filter.session_public_id.as_ref()
                                    .map(|filter_session| filter_session == envelope.session_id())
                                    .unwrap_or(true); // None = any session
                                
                                if session_matches {
                                    self.handle_event_received(&schedule_id, &event_kind_name).await;
                                }
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
    if let Ok(value) = serde_json::to_value(kind) {
        if let Some(type_name) = value.get("type").and_then(|v| v.as_str()) {
            return type_name.to_string();
        }
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
