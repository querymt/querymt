//! Protocol delegation targets built through the one member build function.

use super::member::{DelegateBuildParams, MemberBuildSpec, MemberContext, build_local_member};
use crate::agent::LocalAgentHandle;
use crate::config::SkillsConfig;
use crate::delegation::AgentInfo;
use crate::dotagents::{DotagentsLayer, DotagentsSource, DotagentsSubAgentPlan};
use crate::session::sqlite_storage::SqliteStorage;
use std::sync::Arc;

fn plan(id: &str, tools: Option<Vec<&str>>) -> DotagentsSubAgentPlan {
    DotagentsSubAgentPlan {
        id: id.into(),
        info: AgentInfo {
            isolation: Default::default(),
            id: crate::delegation::roster::AgentId::try_from(id).unwrap(),
            name: id.into(),
            description: "d".into(),
            capabilities: vec![],
            required_capabilities: vec![],
            meta: None,
        },
        system_prompt: String::new(),
        model: None,
        tools: tools.map(|t| t.into_iter().map(String::from).collect()),
        mcp_servers: vec![],
        source: DotagentsSource::entry(DotagentsLayer::Workspace, "a.md", id),
        diagnostics: vec![],
    }
}

async fn ctx_for(
    skills: Option<SkillsConfig>,
) -> (MemberContext, Option<SkillsConfig>, tempfile::TempDir) {
    let (registry, dir) = crate::test_utils::helpers::empty_plugin_registry().unwrap();
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
    let ctx = MemberContext::new(
        DelegateBuildParams {
            plugin_registry: Arc::new(registry),
            snapshot_policy: crate::agent::core::SnapshotPolicy::None,
            skills_project_root: dir.path().to_path_buf(),
            workspace_service: None,
            repository_knowledge: None,
        },
        Default::default(),
        storage,
        Arc::new(querymt_agent_services::event_fanout::EventFanout::new()),
    );
    (ctx, skills, dir)
}

fn handle_of(
    f: &(MemberContext, Option<SkillsConfig>, tempfile::TempDir),
    plan: &DotagentsSubAgentPlan,
) -> Arc<LocalAgentHandle> {
    let spec = MemberBuildSpec::from_protocol_plan(plan, f.1.as_ref()).expect("lowers");
    build_local_member(&spec, &f.0).expect("target is created")
}

fn catalog_of(h: &LocalAgentHandle) -> Option<Arc<crate::skills::SkillCatalog>> {
    h.config.skill_catalog.clone()
}

#[tokio::test]
async fn unrestricted_target_offers_skills() {
    let f = ctx_for(Some(SkillsConfig::default())).await;
    for tools in [None, Some(vec![]), Some(vec!["skill", "read_tool"])] {
        let h = handle_of(&f, &plan("t", tools.clone()));
        assert!(catalog_of(&h).is_some(), "catalog for {tools:?}");
    }
}

#[tokio::test]
async fn restricted_target_without_skill_gets_no_catalog() {
    let f = ctx_for(Some(SkillsConfig::default())).await;
    let h = handle_of(&f, &plan("t", Some(vec!["read_tool"])));
    assert!(catalog_of(&h).is_none());
}

#[tokio::test]
async fn parent_without_skills_yields_no_catalog() {
    let f = ctx_for(None).await;
    assert!(catalog_of(&handle_of(&f, &plan("t", None))).is_none());
    let disabled = SkillsConfig {
        enabled: false,
        ..SkillsConfig::default()
    };
    let f = ctx_for(Some(disabled)).await;
    assert!(catalog_of(&handle_of(&f, &plan("t", None))).is_none());
}

#[tokio::test]
async fn quorum_style_target_applies_the_registering_permissions() {
    let mut skills = SkillsConfig::default();
    skills
        .permissions
        .patterns
        .insert("docs.*".into(), crate::skills::PermissionLevel::Allow);
    let f = ctx_for(Some(skills)).await;
    let catalog = catalog_of(&handle_of(&f, &plan("t", None))).unwrap();
    assert_eq!(
        catalog.permissions().check_mcp("docs", "x"),
        crate::skills::PermissionLevel::Allow
    );
    assert_eq!(
        catalog.permissions().check_mcp("ops", "x"),
        crate::skills::PermissionLevel::Ask
    );
}

fn plan_with_servers(tools: Vec<&str>, servers: &[&str]) -> DotagentsSubAgentPlan {
    let mut p = plan("t", Some(tools));
    p.mcp_servers = servers
        .iter()
        .map(|name| crate::config::McpServerConfig::stdio(*name, "cmd"))
        .collect();
    p
}

#[tokio::test]
async fn restricted_target_with_mcp_servers_keeps_only_listed_tools() {
    use crate::agent::core::ToolPolicy;
    use crate::config::ToolRef;
    let f = ctx_for(None).await;
    let h = handle_of(
        &f,
        &plan_with_servers(vec!["read_tool", "docs.*"], &["docs"]),
    );
    let config = &h.config.tool_config;
    assert_eq!(config.policy, ToolPolicy::BuiltInAndProvider);
    assert!(config.allows(ToolRef::builtin("read_tool")));
    assert!(config.allows(ToolRef::mcp("docs", "search")));
    assert!(!config.allows(ToolRef::builtin("shell")));
    assert!(!config.allows(ToolRef::mcp("ops", "search")));
}

#[tokio::test]
async fn restricted_target_without_servers_is_builtin_only() {
    use crate::agent::core::ToolPolicy;
    use crate::config::ToolRef;
    let f = ctx_for(None).await;
    let h = handle_of(&f, &plan_with_servers(vec!["read_tool"], &[]));
    let config = &h.config.tool_config;
    assert_eq!(config.policy, ToolPolicy::BuiltInOnly);
    assert!(config.allows(ToolRef::builtin("read_tool")));
    assert!(!config.allows(ToolRef::builtin("shell")));
    assert!(!config.allows(ToolRef::mcp("docs", "search")));
}

mod member_spec {
    use crate::delegation::member::{MemberBuildSpec, MemberSpec, available_local_tool_names};
    use crate::delegation::roster::{AgentId, RosterError};
    use serde_json::json;

    fn parse(value: serde_json::Value) -> Result<MemberSpec, serde_json::Error> {
        serde_json::from_value(value)
    }

    fn local(extra: serde_json::Value) -> serde_json::Value {
        let mut base = json!({
            "kind": "local", "id": "coder", "provider": "anthropic", "model": "m"
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    #[test]
    fn local_member_parses() {
        let spec = parse(local(json!({"tools": ["read_tool"], "system": "be brief"}))).unwrap();
        assert!(matches!(spec, MemberSpec::Local(_)));
        assert_eq!(spec.raw_id(), "coder");
        spec.check_external(&AgentId::try_from(spec.raw_id()).unwrap())
            .unwrap();
    }

    #[test]
    fn remote_member_parses() {
        let spec = parse(json!({
            "kind": "remote", "id": "gpu", "name": "GPU", "peer": "dev-gpu",
            "capabilities": ["gpu"]
        }))
        .unwrap();
        assert!(matches!(spec, MemberSpec::Remote(_)));
        assert_eq!(spec.raw_id(), "gpu");
        spec.check_external(&AgentId::try_from(spec.raw_id()).unwrap())
            .unwrap();
    }

    #[test]
    fn member_without_or_with_unknown_kind_is_rejected() {
        assert!(parse(json!({"id": "x", "provider": "p", "model": "m"})).is_err());
        assert!(parse(json!({"kind": "carrier-pigeon", "id": "x"})).is_err());
    }

    #[test]
    fn unknown_field_is_rejected_inside_a_tagged_member() {
        assert!(parse(local(json!({"colour": "red"}))).is_err());
        assert!(
            parse(json!({"kind": "remote", "id": "g", "name": "G", "peer": "p", "colour": 1}))
                .is_err()
        );
    }

    #[test]
    fn file_prompt_part_is_not_accepted_from_a_client() {
        let spec = parse(local(json!({"system": [{"file": "/etc/passwd"}]}))).unwrap();
        let error = spec
            .check_external(&AgentId::try_from("coder").unwrap())
            .unwrap_err();
        assert!(
            matches!(error, RosterError::FilePromptPart { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("coder"), "{error}");
    }

    #[test]
    fn api_key_placeholder_is_not_expanded() {
        let spec = parse(local(json!({"api_key": "${QMT_TEST_SECRET_NOT_EXPANDED}"}))).unwrap();
        let MemberSpec::Local(delegate) = spec else {
            panic!("local expected")
        };
        let lowered =
            MemberBuildSpec::from_delegate_config(*delegate, &[], &available_local_tool_names())
                .unwrap();
        let params = lowered.llm_params().unwrap();
        assert_eq!(
            params.api_key.as_deref(),
            Some("${QMT_TEST_SECRET_NOT_EXPANDED}")
        );
    }
}
