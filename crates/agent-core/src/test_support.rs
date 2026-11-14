//! Mocks of core traits (`#[cfg(test)]`; becomes the `test-support` module of
//! `querymt-agent-core`).

use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkInfo, ForkOrigin, IntentSnapshot, ProgressEntry, ProgressKind, Task,
    TaskStatus,
};
use crate::session::error::SessionResult;
use crate::session::store::{
    CustomModel, LLMConfig, RemoteSessionBookmark, Session, SessionExecutionConfig, SessionStore,
};
use crate::session_control::SessionControlState;
use async_trait::async_trait;
use mockall::mock;
use querymt::LLMParams;

// ============================================================================
// MockSessionStore
// ============================================================================

mock! {
    pub SessionStore {}

    #[async_trait]
    impl SessionStore for SessionStore {
        async fn create_session(&self, name: Option<String>, cwd: Option<std::path::PathBuf>, parent_session_id: Option<String>, fork_origin: Option<ForkOrigin>) -> SessionResult<Session>;
        async fn get_session(&self, session_id: &str) -> SessionResult<Option<Session>>;
        async fn list_sessions(&self) -> SessionResult<Vec<Session>>;
        async fn delete_session(&self, session_id: &str) -> SessionResult<()>;
        async fn get_history(&self, session_id: &str) -> SessionResult<Vec<crate::model::AgentMessage>>;
        async fn get_effective_history(&self, session_id: &str) -> SessionResult<Vec<crate::model::AgentMessage>>;
        async fn add_message(&self, session_id: &str, message: crate::model::AgentMessage) -> SessionResult<()>;
        async fn fork_session(
            &self,
            source_session_id: &str,
            target_message_id: &str,
            fork_origin: ForkOrigin,
        ) -> SessionResult<String>;
        async fn create_or_get_llm_config(&self, input: &LLMParams) -> SessionResult<LLMConfig>;
        async fn get_llm_config(&self, id: i64) -> SessionResult<Option<LLMConfig>>;
        async fn get_session_llm_config(&self, session_id: &str) -> SessionResult<Option<LLMConfig>>;
        async fn set_session_llm_config(&self, session_id: &str, config_id: i64) -> SessionResult<()>;
        async fn get_session_control(&self, session_id: &str) -> SessionResult<Option<SessionControlState>>;
        async fn commit_session_control(
            &self,
            session_id: &str,
            expected_revision: u64,
            state: &SessionControlState,
        ) -> SessionResult<SessionControlState>;
        async fn set_profile_binding<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            profile_id: &'c str,
        ) -> SessionResult<()>;
        async fn set_session_runtime_binding<'a, 'b, 'c, 'd, 'e, 'f, 'g>(
            &'a self,
            session_id: &'b str,
            profile_id: &'c str,
            agent_id: Option<&'d str>,
            profile_fingerprint: Option<&'e str>,
            provider_lock_digest: Option<&'f str>,
            provider_locks_json: Option<&'g str>,
        ) -> SessionResult<()>;
        async fn get_profile_binding<'a, 'b>(
            &'a self,
            session_id: &'b str,
        ) -> SessionResult<Option<String>>;
        async fn get_session_runtime_binding<'a, 'b>(
            &'a self,
            session_id: &'b str,
        ) -> SessionResult<Option<crate::session::store::SessionRuntimeBinding>>;
        async fn remove_profile_binding<'a, 'b>(
            &'a self,
            session_id: &'b str,
        ) -> SessionResult<()>;
        async fn set_session_provider_node_id<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            provider_node_id: Option<&'c str>,
        ) -> SessionResult<()>;
        async fn get_session_provider_node_id<'a, 'b>(
            &'a self,
            session_id: &'b str,
        ) -> SessionResult<Option<String>>;
        async fn set_session_execution_config<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            config: &'c SessionExecutionConfig,
        ) -> SessionResult<()>;
        async fn get_session_execution_config<'a, 'b>(
            &'a self,
            session_id: &'b str,
        ) -> SessionResult<Option<SessionExecutionConfig>>;
        async fn list_custom_models(&self, provider: &str) -> SessionResult<Vec<CustomModel>>;
        async fn get_custom_model(
            &self,
            provider: &str,
            model_id: &str,
        ) -> SessionResult<Option<CustomModel>>;
        async fn upsert_custom_model(&self, model: &CustomModel) -> SessionResult<()>;
        async fn delete_custom_model(&self, provider: &str, model_id: &str) -> SessionResult<()>;
        async fn set_current_intent_snapshot<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            snapshot_id: Option<&'c str>,
        ) -> SessionResult<()>;
        async fn set_active_task<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<()>;
        async fn get_session_fork_info(&self, session_id: &str) -> SessionResult<Option<ForkInfo>>;
        async fn get_session_ref_by_internal_id(
            &self,
            session_internal_id: i64,
        ) -> SessionResult<Option<(String, Option<std::path::PathBuf>)>>;
        async fn list_child_sessions(&self, parent_id: &str) -> SessionResult<Vec<String>>;
        async fn create_task(&self, task: Task) -> SessionResult<Task>;
        async fn get_task(&self, task_id: &str) -> SessionResult<Option<Task>>;
        async fn list_tasks(&self, session_id: &str) -> SessionResult<Vec<Task>>;
        async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> SessionResult<()>;
        async fn update_task(&self, task: Task) -> SessionResult<()>;
        async fn delete_task(&self, task_id: &str) -> SessionResult<()>;
        async fn create_intent_snapshot(&self, snapshot: IntentSnapshot) -> SessionResult<()>;
        async fn create_and_set_current_intent_snapshot(
            &self,
            session_id: &str,
            snapshot: IntentSnapshot,
        ) -> SessionResult<IntentSnapshot>;
        async fn get_intent_snapshot(&self, snapshot_id: &str) -> SessionResult<Option<IntentSnapshot>>;
        async fn list_intent_snapshots(&self, session_id: &str) -> SessionResult<Vec<IntentSnapshot>>;
        async fn get_initial_intent_snapshot(&self, session_id: &str) -> SessionResult<Option<IntentSnapshot>>;
        async fn get_current_intent_snapshot(&self, session_id: &str) -> SessionResult<Option<IntentSnapshot>>;
        async fn record_decision(&self, decision: Decision) -> SessionResult<()>;
        async fn record_alternative(&self, alternative: Alternative) -> SessionResult<()>;
        async fn get_decision(&self, decision_id: &str) -> SessionResult<Option<Decision>>;
        async fn list_decisions<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<Vec<Decision>>;
        async fn list_alternatives<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<Vec<Alternative>>;
        async fn update_decision_status(
            &self,
            decision_id: &str,
            status: DecisionStatus,
        ) -> SessionResult<()>;
        async fn update_alternative_status(
            &self,
            alternative_id: &str,
            status: AlternativeStatus,
        ) -> SessionResult<()>;
        async fn append_progress_entry(&self, entry: ProgressEntry) -> SessionResult<()>;
        async fn get_progress_entry(&self, entry_id: &str) -> SessionResult<Option<ProgressEntry>>;
        async fn list_progress_entries<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<Vec<ProgressEntry>>;
        async fn list_progress_by_kind(
            &self,
            session_id: &str,
            kind: ProgressKind,
        ) -> SessionResult<Vec<ProgressEntry>>;
        async fn record_artifact(&self, artifact: Artifact) -> SessionResult<()>;
        async fn get_artifact(&self, artifact_id: &str) -> SessionResult<Option<Artifact>>;
        async fn list_artifacts<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<Vec<Artifact>>;
        async fn list_artifacts_by_kind(
            &self,
            session_id: &str,
            kind: &str,
        ) -> SessionResult<Vec<Artifact>>;
        async fn create_delegation(&self, delegation: Delegation) -> SessionResult<Delegation>;
        async fn get_delegation(&self, delegation_id: &str) -> SessionResult<Option<Delegation>>;
        async fn list_delegations(&self, session_id: &str) -> SessionResult<Vec<Delegation>>;
        async fn claim_delegation(
            &self,
            delegation_id: &str,
        ) -> SessionResult<crate::session::domain::DelegationClaim>;
        async fn transition_delegation_status(
            &self,
            delegation_id: &str,
            expected: DelegationStatus,
            next: DelegationStatus,
        ) -> SessionResult<bool>;
        async fn update_delegation(&self, delegation: Delegation) -> SessionResult<()>;
        async fn set_delegation_planning_summary<'a, 'b, 'c>(
            &'a self,
            delegation_id: &'b str,
            planning_summary: &'c str,
        ) -> SessionResult<()>;
        async fn peek_revert_state(
            &self,
            session_id: &str,
        ) -> SessionResult<Option<crate::session::domain::RevertState>>;
        async fn push_revert_state(
            &self,
            session_id: &str,
            state: crate::session::domain::RevertState,
        ) -> SessionResult<()>;
        async fn pop_revert_state(
            &self,
            session_id: &str,
        ) -> SessionResult<Option<crate::session::domain::RevertState>>;
        async fn list_revert_states(
            &self,
            session_id: &str,
        ) -> SessionResult<Vec<crate::session::domain::RevertState>>;
        async fn clear_revert_states(&self, session_id: &str) -> SessionResult<()>;
        async fn delete_messages_after(
            &self,
            session_id: &str,
            message_id: &str,
        ) -> SessionResult<usize>;
        async fn mark_tool_results_compacted(
            &self,
            session_id: &str,
            call_ids: &[String],
        ) -> SessionResult<usize>;
        async fn save_remote_session_bookmark(&self, bookmark: &RemoteSessionBookmark) -> SessionResult<()>;
        async fn get_remote_session_bookmark(&self, session_id: &str) -> SessionResult<Option<RemoteSessionBookmark>>;
        async fn list_remote_session_bookmarks(&self) -> SessionResult<Vec<RemoteSessionBookmark>>;
        async fn remove_remote_session_bookmark(&self, session_id: &str) -> SessionResult<()>;
    }
}
