//! `build_root`: when a runtime's root may delegate.

use super::member::{DelegateBuildParams, MemberBuildSpec};
use super::root::{RootDelegation, build_root};
use crate::agent::LocalAgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::SnapshotPolicy;
use crate::config::{ToolRef, ToolSpec};
use crate::session::sqlite_storage::SqliteStorage;
use crate::test_utils::{
    MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
};
use querymt::LLMParams;
use std::sync::Arc;

/// A root with one member `helper`, whose `tools` selection is `tools`.
async fn root_with_member(tools: &[&str]) -> (Arc<LocalAgentHandle>, tempfile::TempDir) {
    let factory = Arc::new(TestProviderFactory::new(SharedLlmProvider::new(
        MockLlmProvider::new(),
        vec![],
    )));
    let (registry, dir) = mock_plugin_registry(factory).expect("plugin registry");
    let registry = Arc::new(registry);
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());

    let llm = LLMParams::new().provider("mock").model("mock-model");
    let mut builder = AgentConfigBuilder::new(registry.clone(), storage, llm.clone());
    if !tools.is_empty() {
        let specs: Vec<ToolSpec> = tools.iter().map(|t| t.parse().unwrap()).collect();
        builder = builder.with_allowed_tools(specs);
    }

    let mut helper = MemberBuildSpec::new("helper");
    helper.llm_config = Some(llm);
    let root = build_root(
        builder,
        RootDelegation {
            enabled: true,
            settings: Default::default(),
            profile_id: None,
            cwd: None,
            members: vec![helper],
            member_params: DelegateBuildParams {
                plugin_registry: registry,
                snapshot_policy: SnapshotPolicy::None,
                skills_project_root: ".".into(),
                wait_policy: Default::default(),
                wait_timeout_secs: 120,
                cancel_grace_secs: 5,
                workspace_service: None,
                repository_knowledge: None,
            },
            preregistered: None,
            protocol_plans: None,
            skills: None,
            middleware_factories: vec![],
            middleware_entries: vec![],
            #[cfg(feature = "remote")]
            peer_delegates: vec![],
            #[cfg(feature = "remote")]
            mesh: None,
        },
    )
    .await
    .expect("root builds");
    (root, dir)
}

#[tokio::test]
async fn unrestricted_agent_can_delegate() {
    let (root, _dir) = root_with_member(&[]).await;
    assert!(
        root.config.tool_registry.find("delegate").is_some(),
        "root tool list must contain `delegate`"
    );
    assert!(root.config.tool_config.allows(ToolRef::builtin("delegate")));
    assert!(
        root.delegation_orchestrator().is_some(),
        "an unrestricted root must run a delegation service"
    );
    root.shutdown().await;
}

#[tokio::test]
async fn agent_whose_tools_exclude_delegate_cannot_delegate() {
    let (root, _dir) = root_with_member(&["read_tool"]).await;
    assert!(
        root.config.tool_registry.find("delegate").is_none(),
        "root tool list must not contain `delegate`"
    );
    assert!(!root.config.tool_config.allows(ToolRef::builtin("delegate")));
    assert!(
        root.delegation_orchestrator().is_none(),
        "a root that cannot call `delegate` must not run a service"
    );
    root.shutdown().await;
}
