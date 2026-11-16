use crate::config::*;
use crate::dotagents::DotagentsManifest;

#[test]
fn repository_knowledge_builders_carry_settings_only_when_enabled() {
    use crate::api::Agent;

    // Disabled single-agent config: builder holds no settings at all.
    let disabled: SingleAgentConfig =
        toml::from_str("[agent]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
    let b = Agent::builder_from_config(disabled).unwrap();
    assert!(b.configured_repository_knowledge().is_none());

    // Enabled: settings round-trip into the builder.
    let enabled: SingleAgentConfig = toml::from_str(
        "[agent]\nprovider = \"p\"\nmodel = \"m\"\n[agent.repository_knowledge]\nenabled = true\nforce_hash = true\n",
    )
    .unwrap();
    let b = Agent::builder_from_config(enabled).unwrap();
    let got = b
        .configured_repository_knowledge()
        .expect("enabled settings");
    assert!(got.enabled && got.force_hash);

    // Programmatic builders.
    let b = crate::api::AgentBuilder::new()
        .repository_knowledge(RepositoryKnowledgeSettings::enabled());
    assert!(b.configured_repository_knowledge().unwrap().enabled);
    assert!(
        crate::api::AgentBuilder::new()
            .configured_repository_knowledge()
            .is_none()
    );
}

/// 7.3: the opt-in repository-knowledge example loads, lists the repository
/// tools its documented configuration relies on, enables both delivery stages,
/// and rolls back completely by disabling the feature.
#[tokio::test]
async fn repository_knowledge_coder_example_loads_and_rolls_back_by_disabling() {
    use crate::tools::builtins::REPOSITORY_TOOL_NAMES;

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/confs/repository_knowledge_coder.toml");
    let Config::Single(config) = load_config(ConfigSource::Path(path.clone())).await.unwrap()
    else {
        panic!("single config");
    };

    let rk = &config.agent.repository_knowledge;
    assert!(rk.enabled && rk.orientation_active() && rk.index_context_active());
    assert_eq!(rk.delivery.max_cards, 2);
    assert_eq!(rk.delivery.max_tokens, 600);
    assert_eq!(rk.delivery.max_bytes, 4096);

    // The allowlist is authoritative: every repository tool is listed explicitly.
    let tools = config.agent.tools.clone();
    assert!(!tools.is_empty(), "explicit allowlist");
    for name in REPOSITORY_TOOL_NAMES {
        assert!(tools.iter().any(|t| t.is_builtin(name)), "{name} listed");
    }
    // Legacy session-memory tools are not pulled in.
    assert!(
        !tools
            .iter()
            .any(|t| t.as_builtin().is_some_and(|n| n.starts_with("knowledge_")))
    );

    // Rollback: disabling the feature turns off reads and both delivery stages
    // even though the delivery flags remain set.
    let mut off = (*config).clone();
    off.agent.repository_knowledge.enabled = false;
    let rk = &off.agent.repository_knowledge;
    assert!(rk.if_enabled().is_none());
    assert!(!rk.orientation_active() && !rk.index_context_active());
    let builder = crate::api::Agent::builder_from_config(off).unwrap();
    assert!(builder.configured_repository_knowledge().is_none());
}

#[test]
fn programmatic_builders_default_disabled_and_accept_protocol_inputs() {
    let single = crate::api::AgentBuilder::new();
    assert!(single.configured_dotagents_options().is_none());
    assert!(single.configured_dotagents_manifest().is_none());

    let single = single
        .enable_dotagents()
        .dotagents_manifest(DotagentsManifest::empty());
    assert!(single.configured_dotagents_options().unwrap().is_enabled());
    assert!(single.configured_dotagents_manifest().is_some());
}
