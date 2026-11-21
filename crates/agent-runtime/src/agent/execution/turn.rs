//! State of one run of the execution loop, split by mutability.
//!
//! - [`TurnEnv`] is everything fixed when the run starts: identity, session
//!   handle, configuration snapshots and channels. Step functions take it by
//!   shared reference, including the concurrent tool wave.
//! - [`Turn`] is everything that changes while the run executes. It has one
//!   owner, the loop, and is borrowed `&mut` by the steps that change it, so
//!   it holds no lock, atomic or cell.
//!
//! The counters of [`TurnBudgets`] are private to this module and change
//! through methods: a new run starts from `TurnBudgets::default()`, and a
//! counter cannot overflow.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, SessionRuntime, ToolConfig, TurnSnapshot};
use crate::agent::execution::DelegationRecovery;
use crate::agent::objective::RunObjective;
use crate::agent::turn_control::{RunPhase, SteeringInbox};
use crate::knowledge::KnowledgeStore;
use crate::knowledge::repository::RepositoryKnowledgeRegistry;
use crate::knowledge::repository::budget::DeliveryBudget;
use crate::session::error::SessionResult;
use crate::session::provider::SessionHandle;
use crate::session::runtime::RuntimeContext;
use crate::session::store::{LLMConfig, SessionExecutionConfig};
use crate::tools::{AgentToolContext, ToolRegistry, ToolSet};
use querymt_agent_core::model::{AgentMessage, TurnId};
use querymt_agent_services::accounting::{BudgetWarnings, CycleAccounting};
use querymt_agent_services::event_sink::EventSink;
use querymt_agent_services::snapshot::{SnapshotId, SnapshotResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Describes the origin of an execution cycle.
///
/// Used to correlate scheduled executions with the `SchedulerActor` so that
/// terminal events (`ScheduledExecutionCompleted` / `ScheduledExecutionFailed`)
/// carry the correct `schedule_public_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionOrigin {
    /// Interactive prompt from a user or delegation.
    Interactive,
    /// Prompt fired by the scheduler for a recurring schedule.
    Scheduled {
        /// Public ID of the schedule that triggered this execution.
        schedule_public_id: String,
        /// Public ID of the recurring task that triggered this execution.
        task_public_id: String,
    },
}

/// What a run reads and never changes.
///
/// Built with a struct literal at its one production site
/// (`session_actor.rs` `execute_prompt_inner`), so a missing field is a
/// compile error and cannot fall back to a silent default.
pub(crate) struct TurnEnv {
    /// The session ID for this execution.
    pub session_id: String,
    /// The run this execution belongs to: hook payloads, run events and
    /// snapshot records all carry it. Read it through [`TurnEnv::turn_id`].
    pub turn_id: TurnId,
    /// Per-session runtime infrastructure (cwd, MCP tools, snapshots, etc.).
    pub runtime: Arc<SessionRuntime>,
    /// Turn-pinned session handle: avoids repeated DB lookups for
    /// session/config.
    pub session_handle: SessionHandle,
    /// Cancelled when the session receives a cancel signal. Propagated into
    /// tool contexts so tools can abort long-running work cooperatively.
    pub cancellation_token: CancellationToken,
    /// Per-session tool configuration captured at the start of the turn, so
    /// `SetAllowedTools`, `SetDeniedTools` and `SetToolPolicy` are honoured.
    pub tool_config: ToolConfig,
    /// Interactive (user/delegation) or scheduled.
    pub execution_origin: ExecutionOrigin,
    /// Optional knowledge store, propagated into tool contexts.
    pub knowledge_store: Option<Arc<dyn KnowledgeStore>>,
    /// Lazy per-checkout repository knowledge; `None` unless host-enabled.
    pub repository_knowledge: Option<Arc<RepositoryKnowledgeRegistry>>,
    /// Optional event sink, propagated into tool contexts.
    pub event_sink: Option<Arc<EventSink>>,
    /// Optional workspace query bridge for VS Code language intelligence.
    pub workspace_query_bridge: Option<ClientBridgeSender>,
    /// Agent mode captured at turn start.
    pub turn_mode: AgentMode,
    /// Inbox bound to this run for boundary-safe steering.
    pub steering: Option<Arc<SteeringInbox>>,
    /// Reports execution phase changes back to the owning session actor.
    pub phase_reporter: Option<UnboundedSender<Option<RunPhase>>>,
    /// Accounting handle for this execution cycle, created at the cycle
    /// boundary. Cycle spend starts at zero; session spend is retained by the
    /// session-owned projection it points at.
    pub cycle_accounting: CycleAccounting,
    /// The run's tool lookup: session tools over the agent registry.
    pub tool_set: ToolSet,
}

impl TurnEnv {
    /// The run's tool lookup: the session-bound `skill` tool (when the agent
    /// has a skill catalog) over the agent's tool registry.
    pub(crate) fn build_tool_set(
        config: &AgentConfig,
        runtime: &SessionRuntime,
        tool_config: &ToolConfig,
    ) -> ToolSet {
        let mut session_tools = ToolRegistry::new();
        if let Some(catalog) = &config.skill_catalog {
            session_tools.add(Arc::new(catalog.bind(
                runtime.cwd.as_deref(),
                Arc::clone(&runtime.mcp_tool_state),
                tool_config.clone(),
                Arc::clone(&runtime.skill_grants),
            )));
        }
        ToolSet::new(Arc::clone(&config.tool_registry), session_tools)
    }

    /// The run's tool lookup.
    pub(crate) fn tool_set(&self) -> &ToolSet {
        &self.tool_set
    }

    /// Tell the owning session actor that its runtime status changed.
    pub(crate) fn report_runtime_change(&self) {
        if let Some(reporter) = &self.phase_reporter {
            let _ = reporter.send(None);
        }
    }

    /// Tell the owning session actor which phase the run entered.
    pub(crate) fn report_phase(&self, phase: RunPhase) {
        if let Some(reporter) = &self.phase_reporter {
            let _ = reporter.send(Some(phase));
        }
    }

    /// The session's working directory.
    pub(crate) fn cwd(&self) -> Option<&Path> {
        self.runtime.cwd.as_deref()
    }

    /// Persist an `AgentMessage` via the turn-pinned session handle.
    pub(crate) async fn add_message(&self, message: AgentMessage) -> SessionResult<()> {
        self.session_handle.add_message(message).await
    }

    /// The turn-pinned LLM config (provider name, model, params).
    pub(crate) fn llm_config(&self) -> Option<&LLMConfig> {
        self.session_handle.llm_config()
    }

    /// The turn-pinned execution config snapshot.
    pub(crate) fn execution_config(&self) -> Option<&SessionExecutionConfig> {
        self.session_handle.execution_config()
    }

    /// The id of the run this execution belongs to. Work that happens inside
    /// a run (native `/compact`, maintenance compaction) uses the run's id.
    pub(crate) fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// The permission-mode label hooks receive.
    pub(crate) fn permission_mode(&self) -> &'static str {
        crate::hooks::permission_mode_label(self.turn_mode)
    }

    /// The context a tool call runs with.
    pub(crate) fn tool_context(
        &self,
        elicitation_tx: Option<tokio::sync::mpsc::Sender<crate::tools::ElicitationRequest>>,
    ) -> AgentToolContext {
        let mut ctx = AgentToolContext::new(
            self.session_id.clone(),
            self.cwd().map(Path::to_path_buf),
            elicitation_tx,
        )
        .with_cancellation_token(self.cancellation_token.clone());

        if let Some(gate) = self.runtime.mutation_gate.get() {
            ctx = ctx.with_mutation_gate(gate.clone());
        }
        if let Some(ks) = &self.knowledge_store {
            ctx.with_knowledge_store(ks.clone());
        }
        ctx = ctx.with_repository_knowledge(self.repository_knowledge.clone());
        if let Some(sink) = &self.event_sink {
            ctx.with_event_sink(sink.clone());
        }
        if let Some(bridge) = &self.workspace_query_bridge {
            ctx = ctx.with_workspace_query_bridge(Arc::new(bridge.clone()));
        }
        ctx
    }
}

/// The snapshot taken at the start of the run for undo/redo.
#[derive(Debug, Default)]
pub(crate) enum PreTurnSnapshot {
    /// No snapshot backend, or the snapshot was already consumed or failed.
    #[default]
    None,
    /// Being taken off the critical path; resolved before the first mutating
    /// tool call or the post-turn snapshot.
    Pending(tokio::task::JoinHandle<(TurnId, SnapshotResult<SnapshotId>)>),
    /// Taken and recorded; the post-turn snapshot diffs against it.
    Ready(TurnSnapshot),
}

/// Everything that changes while a run executes. One owner, no locks.
pub(crate) struct Turn {
    /// Per-run domain state (active task, intent, progress recording).
    pub state: RuntimeContext,
    /// The single source of instruction authority for this run.
    pub run_objective: Option<RunObjective>,
    /// Typed-summary provenance, refreshed whenever effective history is rebuilt.
    pub compaction_summaries: Vec<querymt::chat::ChatMessage>,
    /// Delegations left running when steering interrupts a wait.
    pub suspended_delegations: HashSet<String>,
    /// Retains a supplementary terminal read across bounded BeforeModel
    /// reconciliation.
    pub suspended_recovery: Option<DelegationRecovery>,
    /// Per-run budget for native repository-knowledge delivery, created on
    /// first use from the host's delivery settings.
    pub delivery_budget: Option<DeliveryBudget>,
    /// Budget-uncertainty reasons already reported for this cycle, so
    /// repeated step-start snapshots never repeat a warning.
    pub budget_warnings: BudgetWarnings,
    /// The pre-turn snapshot of this run.
    pub snapshot: PreTurnSnapshot,
    /// Files the run's tool calls changed. It starts empty and goes with the
    /// run, so nothing of an earlier or another run is ever in it.
    pub changed_paths: querymt_agent_index::DiffPaths,
    /// Counters and flags that bound the run.
    pub(super) budgets: TurnBudgets,
}

impl Turn {
    /// A run that starts from `state` with nothing suspended, reported or
    /// counted.
    pub(crate) fn new(state: RuntimeContext) -> Self {
        Self {
            state,
            run_objective: None,
            compaction_summaries: Vec::new(),
            suspended_delegations: HashSet::new(),
            suspended_recovery: None,
            delivery_budget: None,
            budget_warnings: BudgetWarnings::default(),
            snapshot: PreTurnSnapshot::None,
            changed_paths: Default::default(),
            budgets: TurnBudgets::default(),
        }
    }
}

/// Which Stop-hook continuation budget a continuation is charged to.
///
/// Counted per stop reason so token-limit cut-offs do not use up the budget
/// for normal end-of-turn continuations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopHookKind {
    EndTurn,
    ModelTokenLimit,
}

/// Counters and flags that bound one run.
#[derive(Debug, Default)]
pub(crate) struct TurnBudgets {
    /// Stop-hook continuations after a normal end of turn.
    stop_hook_continuations: u32,
    /// Stop-hook continuations after a token-limit cut-off.
    token_limit_stop_hook_continuations: u32,
    /// Times the task-completion guard has sent the model back to work.
    completion_guard_continuations: u8,
    /// Times middleware has turned a stop into a continuation.
    stop_recoveries: u8,
    /// Main model requests issued so far.
    llm_requests: u32,
    /// Value of `llm_requests` at the last automatic compaction.
    last_compaction_request: Option<u32>,
    /// Step at which the last periodic progress review was queued.
    last_objective_checkpoint_step: usize,
    /// Whether the latest request offered the task-completion tool.
    task_completion_tool_available: bool,
}

impl TurnBudgets {
    /// Stop-hook continuations already used for `kind`.
    pub(crate) fn stop_hook_continuations(&self, kind: StopHookKind) -> u32 {
        match kind {
            StopHookKind::EndTurn => self.stop_hook_continuations,
            StopHookKind::ModelTokenLimit => self.token_limit_stop_hook_continuations,
        }
    }

    /// Charge one Stop-hook continuation to `kind`.
    pub(crate) fn note_stop_hook_continuation(&mut self, kind: StopHookKind) {
        let counter = match kind {
            StopHookKind::EndTurn => &mut self.stop_hook_continuations,
            StopHookKind::ModelTokenLimit => &mut self.token_limit_stop_hook_continuations,
        };
        *counter = counter.saturating_add(1);
    }

    /// Continuations of the task-completion guard (0 or 1).
    pub(crate) fn completion_guard_continuations(&self) -> u8 {
        self.completion_guard_continuations
    }

    /// Charge one continuation to the task-completion guard.
    pub(crate) fn note_completion_guard_continuation(&mut self) {
        self.completion_guard_continuations = self.completion_guard_continuations.saturating_add(1);
    }

    /// Stop recoveries by middleware so far.
    pub(crate) fn stop_recoveries(&self) -> u8 {
        self.stop_recoveries
    }

    /// Record that middleware turned a stop into a continuation.
    pub(crate) fn note_stop_recovery(&mut self) {
        self.stop_recoveries = self.stop_recoveries.saturating_add(1);
    }

    /// Main model requests issued so far.
    pub(crate) fn llm_requests(&self) -> u32 {
        self.llm_requests
    }

    /// Record a main model request and whether it offered the completion tool.
    pub(crate) fn note_llm_request(&mut self, completion_tool_available: bool) {
        self.llm_requests = self.llm_requests.saturating_add(1);
        self.task_completion_tool_available = completion_tool_available;
    }

    /// Whether the latest request offered the task-completion tool.
    pub(crate) fn task_completion_tool_available(&self) -> bool {
        self.task_completion_tool_available
    }

    /// Allow one automatic compaction between main model requests, preventing
    /// non-progress loops.
    ///
    /// # Errors
    ///
    /// Fails when the previous automatic compaction happened at the current
    /// request count, which means compaction did not make the request fit.
    pub(crate) fn mark_automatic_compaction(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.last_compaction_request != Some(self.llm_requests),
            "The request still exceeds its context budget after automatic compaction. \
             Reduce tools or injected context, or use a larger-context model."
        );
        self.last_compaction_request = Some(self.llm_requests);
        Ok(())
    }

    /// Step of the last periodic progress review.
    pub(crate) fn last_objective_checkpoint_step(&self) -> usize {
        self.last_objective_checkpoint_step
    }

    /// Record the step at which a progress review was queued.
    pub(crate) fn set_last_objective_checkpoint_step(&mut self, step: usize) {
        self.last_objective_checkpoint_step = step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Failed admission after compaction must not issue another summarization request.
    #[test]
    fn automatic_compaction_requires_model_request_progress() {
        let mut budgets = TurnBudgets::default();
        budgets.mark_automatic_compaction().unwrap();
        let error = budgets.mark_automatic_compaction().unwrap_err();
        assert!(error.to_string().contains("after automatic compaction"));
        assert_eq!(budgets.last_compaction_request, Some(0));
    }

    /// A later model/tool exchange can legitimately require another compaction in the same turn.
    #[test]
    fn automatic_compaction_is_allowed_after_a_new_model_request() {
        let mut budgets = TurnBudgets::default();
        budgets.mark_automatic_compaction().unwrap();
        budgets.note_llm_request(false);
        budgets.mark_automatic_compaction().unwrap();
        assert_eq!(budgets.last_compaction_request, Some(1));
        assert!(budgets.mark_automatic_compaction().is_err());
    }

    #[test]
    fn stop_hook_budgets_are_counted_per_kind() {
        let mut budgets = TurnBudgets::default();
        budgets.note_stop_hook_continuation(StopHookKind::ModelTokenLimit);
        budgets.note_stop_hook_continuation(StopHookKind::ModelTokenLimit);
        budgets.note_stop_hook_continuation(StopHookKind::EndTurn);
        assert_eq!(budgets.stop_hook_continuations(StopHookKind::EndTurn), 1);
        assert_eq!(
            budgets.stop_hook_continuations(StopHookKind::ModelTokenLimit),
            2
        );
    }

    mod env_tests {
        use crate::test_utils::TestAgent;
        use crate::tools::context::ToolContext;
        use std::sync::Arc;

        #[tokio::test]
        async fn cwd_is_none_without_a_workspace() {
            let (env, _turn) = TestAgent::new().await.turn().await;
            assert!(env.cwd().is_none());
        }

        #[tokio::test]
        async fn cwd_is_the_runtime_workspace() {
            let cwd = std::path::PathBuf::from("/tmp/my-workspace");
            let (env, _turn) = TestAgent::new().await.turn_in(cwd.clone()).await;
            assert_eq!(env.cwd(), Some(cwd.as_path()));
        }

        #[tokio::test]
        async fn llm_config_is_the_one_pinned_to_the_session() {
            let (env, _turn) = TestAgent::new().await.turn().await;
            let config = env.llm_config().expect("session has an llm config");
            assert_eq!(config.provider, "mock");
            assert_eq!(config.model, "mock");
        }

        #[tokio::test]
        async fn tool_context_propagates_knowledge_store() {
            use crate::knowledge::KnowledgeStore;
            use crate::knowledge::sqlite::SqliteKnowledgeStore;
            use crate::test_utils::sqlite_conn_with_schema;

            let knowledge_store: Arc<dyn KnowledgeStore> =
                Arc::new(SqliteKnowledgeStore::new(sqlite_conn_with_schema()));
            let (mut env, _turn) = TestAgent::new().await.turn().await;
            env.knowledge_store = Some(knowledge_store);

            assert!(
                env.tool_context(None).knowledge_store().is_some(),
                "knowledge_store should be Some when set on the env"
            );
        }

        #[tokio::test]
        async fn tool_context_has_no_knowledge_store_when_not_set() {
            let (env, _turn) = TestAgent::new().await.turn().await;
            assert!(env.tool_context(None).knowledge_store().is_none());
        }

        #[tokio::test]
        async fn tool_context_propagates_checkout_mutation_gate() {
            let (env, _turn) = TestAgent::new().await.turn().await;
            let storage = tempfile::TempDir::new().unwrap();
            let checkout = tempfile::TempDir::new().unwrap();
            let gate = Arc::new(
                crate::worktree::CheckoutMutationGate::new(storage.path(), checkout.path())
                    .unwrap(),
            );
            env.runtime.mutation_gate.set(gate.clone()).unwrap();

            let tools = env.tool_context(None);
            assert!(Arc::ptr_eq(&gate, &tools.checkout_mutation_gate().unwrap()));
        }

        #[tokio::test]
        async fn tool_context_propagates_repository_knowledge_only_when_set() {
            use crate::config::RepositoryKnowledgeSettings;

            let (mut env, _turn) = TestAgent::new().await.turn().await;
            assert!(
                env.tool_context(None).repository_knowledge().is_none(),
                "absent unless host-enabled"
            );

            let enabled = crate::knowledge::host::registry_from_settings(
                &RepositoryKnowledgeSettings::enabled(),
            );
            env.repository_knowledge = enabled.clone();
            assert!(Arc::ptr_eq(
                &env.tool_context(None)
                    .repository_knowledge()
                    .expect("propagated"),
                enabled.as_ref().unwrap()
            ));
        }
    }

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let mut budgets = TurnBudgets {
            stop_recoveries: u8::MAX,
            llm_requests: u32::MAX,
            ..TurnBudgets::default()
        };
        budgets.note_stop_recovery();
        budgets.note_llm_request(true);
        assert_eq!(budgets.stop_recoveries(), u8::MAX);
        assert_eq!(budgets.llm_requests(), u32::MAX);
        assert!(budgets.task_completion_tool_available());
    }
}
