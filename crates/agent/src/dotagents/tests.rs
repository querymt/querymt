//! Tests for the `.agents` Protocol domain model (task 1.1).
//!
//! These tests exercise the public API surface: constructing load options and
//! inspecting every supported manifest collection. They intentionally avoid
//! filesystem side effects so they cannot create `.agents` directories.

use super::*;
use std::collections::BTreeMap;

#[test]
fn load_options_default_is_disabled() {
    let options = DotagentsLoadOptions::default();
    assert!(!options.is_enabled());
    assert!(options.workspace().is_none());
    assert!(options.global_root_override().is_none());
    assert!(options.workspace_root_override().is_none());
    assert!(options.is_global_enabled());
    assert!(options.is_workspace_enabled());
    assert!(options.selected_model_preset().is_none());
    assert!(options.trusted_roots().is_empty());
    assert_eq!(options.strictness(), DotagentsStrictness::Compatibility);
}

#[test]
fn load_options_builders_round_trip() {
    let options = DotagentsLoadOptions::enabled()
        .with_workspace("/tmp/ws")
        .with_global_root("/tmp/global")
        .with_workspace_root("/tmp/ws/.agents")
        .with_global_enabled(false)
        .with_workspace_enabled(true)
        .with_selected_model_preset("fast")
        .with_trusted_root("/tmp/trusted")
        .with_strictness(DotagentsStrictness::Strict);

    assert!(options.is_enabled());
    assert_eq!(options.workspace().unwrap().to_str(), Some("/tmp/ws"));
    assert_eq!(
        options.global_root_override().unwrap().to_str(),
        Some("/tmp/global")
    );
    assert_eq!(
        options.workspace_root_override().unwrap().to_str(),
        Some("/tmp/ws/.agents")
    );
    assert!(!options.is_global_enabled());
    assert!(options.is_workspace_enabled());
    assert_eq!(options.selected_model_preset(), Some("fast"));
    assert_eq!(
        options.trusted_roots(),
        &[std::path::PathBuf::from("/tmp/trusted")]
    );
    assert!(options.strictness().is_strict());
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

    let quorum = crate::api::QuorumBuilder::new();
    assert!(quorum.configured_dotagents_options().is_none());
    assert!(quorum.configured_dotagents_manifest().is_none());

    let quorum = quorum
        .dotagents_options(DotagentsLoadOptions::enabled())
        .dotagents_manifest(DotagentsManifest::empty());
    assert!(quorum.configured_dotagents_options().unwrap().is_enabled());
    assert!(quorum.configured_dotagents_manifest().is_some());
}

#[test]
fn strictness_is_strict() {
    assert!(!DotagentsStrictness::Compatibility.is_strict());
    assert!(DotagentsStrictness::Strict.is_strict());
}

#[test]
fn manifest_empty_has_no_content_or_errors() {
    let manifest = DotagentsManifest::empty();
    assert!(manifest.is_empty());
    assert!(!manifest.has_any_layer());
    assert!(!manifest.has_errors());
    assert_eq!(manifest.diagnostics.len(), 0);
    assert!(manifest.agents_md.is_none());
    assert!(manifest.system_prompt.is_none());
}

#[test]
fn manifest_exposes_every_supported_collection() {
    use std::collections::BTreeMap;

    let mut manifest = DotagentsManifest::default();

    let source = DotagentsSource::singleton(DotagentsLayer::Workspace, "/ws/.agents/agents.md");
    manifest.agents_md = Some(DotagentsPrompt {
        metadata: BTreeMap::new(),
        body: "instructions".to_string(),
        source: source.clone(),
        fingerprint: "fp-agents".to_string(),
    });
    manifest.system_prompt = Some(DotagentsPrompt {
        metadata: BTreeMap::new(),
        body: "system".to_string(),
        source: DotagentsSource::singleton(
            DotagentsLayer::Workspace,
            "/ws/.agents/system-prompt.md",
        ),
        fingerprint: "fp-system".to_string(),
    });

    manifest.mcp_servers.insert(
        "fs".to_string(),
        DotagentsMcpServer {
            name: "fs".to_string(),
            transport: DotagentsMcpTransport::Stdio,
            declared_transport: Some("stdio".to_string()),
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "server-fs".to_string()],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(DotagentsLayer::Workspace, "/ws/.agents/mcp.json", "fs"),
        },
    );

    manifest.model_presets.insert(
        "fast".to_string(),
        DotagentsModelPreset {
            name: "fast".to_string(),
            provider: "anthropic".to_string(),
            model: "claude".to_string(),
            credential: None,
            parameters: BTreeMap::new(),
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(
                DotagentsLayer::Workspace,
                "/ws/.agents/models.json",
                "fast",
            ),
        },
    );

    manifest.skills.insert(
        "review".to_string(),
        DotagentsSkill {
            id: "review".to_string(),
            name: "review".to_string(),
            description: "review code".to_string(),
            enabled: true,
            body: "body".to_string(),
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(
                DotagentsLayer::Workspace,
                "/ws/.agents/skills/review/skill.md",
                "review",
            ),
            fingerprint: "fp-skill".to_string(),
        },
    );

    manifest.agents.insert(
        "helper".to_string(),
        DotagentsAgent {
            id: "helper".to_string(),
            name: "helper".to_string(),
            description: "helps".to_string(),
            enabled: true,
            role: DotagentsAgentRole::DelegationTarget,
            connection: DotagentsAgentConnection::default(),
            capabilities: vec!["read".to_string()],
            config: DotagentsAgentConfig::default(),
            body: "prompt".to_string(),
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(
                DotagentsLayer::Workspace,
                "/ws/.agents/agents/helper/agent.md",
                "helper",
            ),
            fingerprint: "fp-agent".to_string(),
        },
    );

    manifest.tasks.insert(
        "digest".to_string(),
        DotagentsTask {
            id: "digest".to_string(),
            name: "digest".to_string(),
            kind: DotagentsTaskKind::Task,
            enabled: true,
            run_on_startup: false,
            interval_minutes: Some(60),
            profile_id: None,
            prompt: "summarize".to_string(),
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(
                DotagentsLayer::Workspace,
                "/ws/.agents/tasks/digest/task.md",
                "digest",
            ),
            fingerprint: "fp-task".to_string(),
        },
    );

    manifest.memories.insert(
        "note".to_string(),
        DotagentsMemory {
            id: "note".to_string(),
            title: Some("note".to_string()),
            tags: vec!["topic".to_string()],
            importance: Some("high".to_string()),
            content: None,
            body: "remember".to_string(),
            enabled: true,
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(
                DotagentsLayer::Workspace,
                "/ws/.agents/memories/note.md",
                "note",
            ),
            fingerprint: "fp-memory".to_string(),
        },
    );

    manifest.unsupported.push(DotagentsUnsupported {
        kind: DotagentsUnsupportedKind::Layouts,
        id: "layouts".to_string(),
        source: DotagentsSource::singleton(DotagentsLayer::Workspace, "/ws/.agents/layouts"),
    });

    manifest.diagnostics.push(
        DotagentsDiagnostic::warning(DotagentsDiagnosticCode::Other, "note").with_source(source),
    );

    assert!(!manifest.is_empty());
    assert!(manifest.agents_md.is_some());
    assert!(manifest.system_prompt.is_some());
    assert_eq!(manifest.mcp_servers.len(), 1);
    assert_eq!(manifest.model_presets.len(), 1);
    assert_eq!(manifest.skills.len(), 1);
    assert_eq!(manifest.agents.len(), 1);
    assert_eq!(manifest.tasks.len(), 1);
    assert_eq!(manifest.memories.len(), 1);
    assert_eq!(manifest.unsupported.len(), 1);
    assert_eq!(manifest.diagnostics.len(), 1);
    assert!(manifest.model_preset("fast").is_some());
}

#[test]
fn manifest_layers_report_existence() {
    let mut manifest = DotagentsManifest::default();
    manifest.layers.push(DotagentsSourceRef::new(
        DotagentsLayer::Global,
        "/home/u/.agents",
        false,
    ));
    assert!(!manifest.has_any_layer());

    manifest.layers.push(DotagentsSourceRef::new(
        DotagentsLayer::Workspace,
        "/ws/.agents",
        true,
    ));
    assert!(manifest.has_any_layer());
}

#[test]
fn manifest_has_errors_only_for_error_severity() {
    let mut manifest = DotagentsManifest::default();
    manifest.diagnostics.push(DotagentsDiagnostic::warning(
        DotagentsDiagnosticCode::Other,
        "w",
    ));
    assert!(!manifest.has_errors());

    manifest.diagnostics.push(DotagentsDiagnostic::error(
        DotagentsDiagnosticCode::ParseError,
        "e",
    ));
    assert!(manifest.has_errors());
    assert_eq!(manifest.errors().count(), 1);
}

#[test]
fn manifest_sort_is_deterministic() {
    let mut manifest = DotagentsManifest::default();
    manifest.diagnostics.push(DotagentsDiagnostic::error(
        DotagentsDiagnosticCode::ParseError,
        "b",
    ));
    manifest.diagnostics.push(DotagentsDiagnostic::error(
        DotagentsDiagnosticCode::ParseError,
        "a",
    ));
    manifest.sort_deterministically();
    assert_eq!(manifest.diagnostics[0].message, "a");
    assert_eq!(manifest.diagnostics[1].message, "b");
}

#[test]
fn transport_parsing_and_support() {
    assert_eq!(
        DotagentsMcpTransport::parse("stdio"),
        DotagentsMcpTransport::Stdio
    );
    assert_eq!(
        DotagentsMcpTransport::parse("streamable-http"),
        DotagentsMcpTransport::StreamableHttp
    );
    assert_eq!(
        DotagentsMcpTransport::parse("streamable_http"),
        DotagentsMcpTransport::StreamableHttp
    );
    assert_eq!(
        DotagentsMcpTransport::parse("websocket"),
        DotagentsMcpTransport::WebSocket
    );
    assert_eq!(
        DotagentsMcpTransport::parse("carrier-pigeon"),
        DotagentsMcpTransport::Unknown
    );
    assert!(DotagentsMcpTransport::Stdio.is_supported());
    assert!(DotagentsMcpTransport::StreamableHttp.is_supported());
    assert!(!DotagentsMcpTransport::WebSocket.is_supported());
    assert!(!DotagentsMcpTransport::Unknown.is_supported());
}

#[test]
fn role_and_connection_parsing() {
    assert_eq!(
        DotagentsAgentRole::parse("delegation-target"),
        DotagentsAgentRole::DelegationTarget
    );
    assert_eq!(
        DotagentsAgentRole::parse("mystery"),
        DotagentsAgentRole::Other
    );

    assert_eq!(
        DotagentsAgentConnectionType::parse("internal"),
        DotagentsAgentConnectionType::Internal
    );
    assert_eq!(
        DotagentsAgentConnectionType::parse("stdio"),
        DotagentsAgentConnectionType::Stdio
    );
    assert_eq!(
        DotagentsAgentConnectionType::parse("quantum"),
        DotagentsAgentConnectionType::Unknown
    );
}

#[test]
fn layer_precedence_orders_workspace_above_global() {
    assert!(DotagentsLayer::Workspace.precedence() > DotagentsLayer::Global.precedence());
}

#[test]
fn diagnostics_never_require_secret_values() {
    // Diagnostics are constructed from references, not resolved values.
    let diag = DotagentsDiagnostic::error(
        DotagentsDiagnosticCode::MissingEnvironment,
        "MCP server `fs` requires environment variable `GH_TOKEN`",
    );
    assert!(diag.is_error());
    assert!(diag.to_string().contains("GH_TOKEN"));
    assert!(diag.to_string().contains("missing_environment"));
}

fn mcp_server(transport: DotagentsMcpTransport, enabled: bool) -> DotagentsMcpServer {
    DotagentsMcpServer {
        name: "test".to_string(),
        transport,
        declared_transport: None,
        command: None,
        args: Vec::new(),
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        enabled,
        extensions: BTreeMap::new(),
        source: DotagentsSource::entry(DotagentsLayer::Workspace, "/ws/.agents/mcp.json", "test"),
    }
}

#[test]
fn mcp_stdio_conversion_uses_existing_process_config() {
    let mut server = mcp_server(DotagentsMcpTransport::Stdio, true);
    server.command = Some("npx".to_string());
    server.args = vec!["-y".to_string(), "server-fs".to_string()];
    server.env.insert("TOKEN".to_string(), "t".to_string());

    let config = adapters::convert_server(&server).expect("conversion succeeds");
    let Some(crate::config::McpServerConfig::Stdio {
        name,
        command,
        args,
        env,
    }) = config
    else {
        panic!("expected the existing stdio process configuration, got {config:?}");
    };
    assert_eq!(name, "test");
    assert_eq!(command, "npx");
    assert_eq!(args, vec!["-y".to_string(), "server-fs".to_string()]);
    assert_eq!(env.get("TOKEN").map(String::as_str), Some("t"));
}

#[test]
fn mcp_streamable_http_conversion_uses_existing_http_config() {
    let mut server = mcp_server(DotagentsMcpTransport::StreamableHttp, true);
    server.url = Some("https://mcp.example.com/mcp".to_string());
    server
        .headers
        .insert("Authorization".to_string(), "Bearer x".to_string());

    let config = adapters::convert_server(&server).expect("conversion succeeds");
    let Some(crate::config::McpServerConfig::Http { name, url, headers }) = config else {
        panic!("expected the existing streamable HTTP configuration, got {config:?}");
    };
    assert_eq!(name, "test");
    assert_eq!(url, "https://mcp.example.com/mcp");
    assert_eq!(
        headers.get("Authorization").map(String::as_str),
        Some("Bearer x")
    );
}

#[test]
fn mcp_plan_warns_for_non_loopback_http_headers() {
    let mut manifest = DotagentsManifest::default();
    let mut server = mcp_server(DotagentsMcpTransport::StreamableHttp, true);
    server.url = Some("http://example.com/mcp".to_string());
    server
        .headers
        .insert("Authorization".to_string(), "Bearer token".to_string());
    manifest.mcp_servers.insert("test".to_string(), server);

    let plan = adapters::DotagentsMcpPlan::from_manifest(&manifest);
    assert_eq!(plan.servers.len(), 1);
    assert_eq!(plan.diagnostics.len(), 1);
    assert_eq!(
        plan.diagnostics[0]
            .source
            .as_ref()
            .unwrap()
            .entry_id
            .as_deref(),
        Some("test")
    );
}

#[test]
fn mcp_plan_does_not_warn_for_safe_http_header_cases() {
    for url in [
        "http://localhost:8080/mcp",
        "http://127.0.0.1/mcp",
        "https://example.com/mcp",
    ] {
        let mut manifest = DotagentsManifest::default();
        let mut server = mcp_server(DotagentsMcpTransport::StreamableHttp, true);
        server.url = Some(url.to_string());
        server
            .headers
            .insert("X-Test".to_string(), "value".to_string());
        manifest.mcp_servers.insert("test".to_string(), server);
        assert!(
            adapters::DotagentsMcpPlan::from_manifest(&manifest)
                .diagnostics
                .is_empty()
        );
    }

    let mut manifest = DotagentsManifest::default();
    let mut server = mcp_server(DotagentsMcpTransport::StreamableHttp, true);
    server.url = Some("http://example.com/mcp".to_string());
    manifest.mcp_servers.insert("test".to_string(), server);
    assert!(
        adapters::DotagentsMcpPlan::from_manifest(&manifest)
            .diagnostics
            .is_empty()
    );
}

#[test]
fn mcp_conversion_skips_disabled_entries_without_error() {
    let mut server = mcp_server(DotagentsMcpTransport::Stdio, false);
    server.command = Some("npx".to_string());
    assert!(
        adapters::convert_server(&server)
            .expect("disabled entries are skipped")
            .is_none()
    );
}

#[test]
fn mcp_conversion_reports_unsupported_transports_instead_of_starting() {
    for transport in [
        DotagentsMcpTransport::WebSocket,
        DotagentsMcpTransport::Unknown,
    ] {
        let server = mcp_server(transport, true);
        let err = adapters::convert_server(&server).expect_err("unsupported transport");
        assert!(err.is_error());
        assert_eq!(err.code, DotagentsDiagnosticCode::UnsupportedTransport);
    }
}

#[test]
fn mcp_plan_converts_enabled_servers_and_orders_by_name() {
    let mut manifest = DotagentsManifest::default();

    let mut disabled = mcp_server(DotagentsMcpTransport::Stdio, false);
    disabled.name = "off".to_string();
    disabled.command = Some("never".to_string());
    manifest.mcp_servers.insert("off".to_string(), disabled);

    let mut alpha = mcp_server(DotagentsMcpTransport::StreamableHttp, true);
    alpha.name = "alpha".to_string();
    alpha.url = Some("https://alpha.example.com/mcp".to_string());
    manifest.mcp_servers.insert("alpha".to_string(), alpha);

    let mut zulu = mcp_server(DotagentsMcpTransport::Stdio, true);
    zulu.name = "zulu".to_string();
    zulu.command = Some("uvx".to_string());
    manifest.mcp_servers.insert("zulu".to_string(), zulu);

    let plan = adapters::DotagentsMcpPlan::from_manifest(&manifest);
    assert!(plan.diagnostics.is_empty());
    let names: Vec<&str> = plan.servers.iter().map(|s| s.name()).collect();
    assert_eq!(names, vec!["alpha", "zulu"]);
    assert!(matches!(
        &plan.servers[0],
        crate::config::McpServerConfig::Http { .. }
    ));
    assert!(matches!(
        &plan.servers[1],
        crate::config::McpServerConfig::Stdio { .. }
    ));
}

#[test]
fn mcp_activation_interpolates_env_and_headers() {
    unsafe {
        std::env::set_var("QMT_DOTAGENTS_TEST_SECRET", "resolved-secret-1");
    }

    let mut stdio = mcp_server(DotagentsMcpTransport::Stdio, true);
    stdio.name = "stdio-srv".to_string();
    stdio.command = Some("npx".to_string());
    stdio.env.insert(
        "TOKEN".to_string(),
        "${QMT_DOTAGENTS_TEST_SECRET}".to_string(),
    );

    let mut http = mcp_server(DotagentsMcpTransport::StreamableHttp, true);
    http.name = "http-srv".to_string();
    http.url = Some("https://e.test/mcp".to_string());
    http.headers.insert(
        "Authorization".to_string(),
        "Bearer ${QMT_DOTAGENTS_TEST_SECRET}".to_string(),
    );

    let stdio_config = adapters::convert_server(&stdio).expect("converts").unwrap();
    let crate::config::McpServerConfig::Stdio { env, .. } = &stdio_config else {
        panic!("expected stdio config");
    };
    assert_eq!(
        env.get("TOKEN").map(String::as_str),
        Some("resolved-secret-1")
    );

    let http_config = adapters::convert_server(&http).expect("converts").unwrap();
    let crate::config::McpServerConfig::Http { headers, .. } = &http_config else {
        panic!("expected http config");
    };
    assert_eq!(
        headers.get("Authorization").map(String::as_str),
        Some("Bearer resolved-secret-1")
    );

    unsafe {
        std::env::remove_var("QMT_DOTAGENTS_TEST_SECRET");
    }
}

#[test]
fn mcp_activation_supports_default_fallback() {
    let mut server = mcp_server(DotagentsMcpTransport::Stdio, true);
    server.command = Some("npx".to_string());
    server.env.insert(
        "TOKEN".to_string(),
        "${QMT_DOTAGENTS_ABSENT_FALLBACK_VAR:-fallback-value}".to_string(),
    );

    let config = adapters::convert_server(&server)
        .expect("converts")
        .unwrap();
    let crate::config::McpServerConfig::Stdio { env, .. } = &config else {
        panic!("expected stdio config");
    };
    assert_eq!(env.get("TOKEN").map(String::as_str), Some("fallback-value"));
}

#[test]
fn mcp_missing_variable_error_is_actionable() {
    let mut server = mcp_server(DotagentsMcpTransport::Stdio, true);
    server.name = "secrets-srv".to_string();
    server.command = Some("npx".to_string());
    server.env.insert(
        "TOKEN".to_string(),
        "${QMT_DOTAGENTS_DEFINITELY_ABSENT_VAR}".to_string(),
    );
    server
        .env
        .insert("PLAIN".to_string(), "plain-value".to_string());

    let err = adapters::convert_server(&server).expect_err("missing env must be reported");
    assert_eq!(err.code, DotagentsDiagnosticCode::MissingEnvironment);
    // Actionable: names the server, the field, and the unresolved reference.
    assert!(err.message.contains("secrets-srv"));
    assert!(err.message.contains("TOKEN"));
    assert!(err.message.contains("QMT_DOTAGENTS_DEFINITELY_ABSENT_VAR"));
    // Never leaks resolved values from the same map.
    assert!(!err.message.contains("plain-value"));
    // Source-aware.
    assert!(err.source.is_some());
}

#[test]
fn mcp_plan_isolates_missing_environment_failures() {
    let mut bad = mcp_server(DotagentsMcpTransport::Stdio, true);
    bad.name = "bad".to_string();
    bad.command = Some("npx".to_string());
    bad.env.insert(
        "TOKEN".to_string(),
        "${QMT_DOTAGENTS_DEFINITELY_ABSENT_VAR}".to_string(),
    );

    let mut good = mcp_server(DotagentsMcpTransport::StreamableHttp, true);
    good.name = "good".to_string();
    good.url = Some("https://e.test/mcp".to_string());

    let mut manifest = DotagentsManifest::default();
    manifest.mcp_servers.insert("bad".to_string(), bad);
    manifest.mcp_servers.insert("good".to_string(), good);

    let plan = adapters::DotagentsMcpPlan::from_manifest(&manifest);
    // The failing server is not activated; the valid sibling still is.
    let names: Vec<&str> = plan.servers.iter().map(|s| s.name()).collect();
    assert_eq!(names, vec!["good"]);
    assert_eq!(plan.diagnostics.len(), 1);
    assert_eq!(
        plan.diagnostics[0].code,
        DotagentsDiagnosticCode::MissingEnvironment
    );
}

#[test]
fn redacted_mcp_view_hides_secret_values() {
    let mut server = mcp_server(DotagentsMcpTransport::Stdio, true);
    server.command = Some("npx".to_string());
    server
        .env
        .insert("TOKEN".to_string(), "literal-secret-1".to_string());
    server.headers.insert(
        "Authorization".to_string(),
        "Bearer literal-secret-2".to_string(),
    );

    let view = server.redacted_view();
    let debug = format!("{view:?}");
    let display = format!("{view}");
    for rendered in [debug, display] {
        assert!(!rendered.contains("literal-secret-1"));
        assert!(!rendered.contains("literal-secret-2"));
        assert!(rendered.contains("***"));
        // Context for correcting errors remains.
        assert!(rendered.contains("TOKEN"));
        assert!(rendered.contains("Authorization"));
    }
}

#[test]
fn redacted_model_preset_view_hides_credential_and_parameters() {
    let preset = DotagentsModelPreset {
        name: "fast".to_string(),
        provider: "anthropic".to_string(),
        model: "claude".to_string(),
        credential: Some("sk-literal-secret".to_string()),
        parameters: BTreeMap::from([("api_key".to_string(), serde_json::json!("sk-param-secret"))]),
        extensions: BTreeMap::new(),
        source: DotagentsSource::entry(
            DotagentsLayer::Workspace,
            "/ws/.agents/models.json",
            "fast",
        ),
    };

    let view = preset.redacted_view();
    let debug = format!("{view:?}");
    let display = format!("{view}");
    for rendered in [debug, display] {
        assert!(!rendered.contains("sk-literal-secret"));
        assert!(!rendered.contains("sk-param-secret"));
        // Context remains inspectable.
        assert!(rendered.contains("anthropic"));
        assert!(rendered.contains("claude"));
        assert!(rendered.contains("api_key"));
    }
}

#[test]
fn raw_mcp_server_debug_and_display_hide_secret_values() {
    let mut server = mcp_server(DotagentsMcpTransport::Stdio, true);
    server.command = Some("npx".to_string());
    server
        .env
        .insert("TOKEN".to_string(), "raw-secret-env".to_string());
    server.headers.insert(
        "Authorization".to_string(),
        "Bearer raw-secret-header".to_string(),
    );

    // The stored type itself must redact: callers cannot leak by formatting it.
    for rendered in [format!("{server:?}"), format!("{server}")] {
        assert!(!rendered.contains("raw-secret-env"));
        assert!(!rendered.contains("raw-secret-header"));
        // Context for correcting errors remains.
        assert!(rendered.contains("TOKEN"));
        assert!(rendered.contains("Authorization"));
    }
    // Non-secret fields stay inspectable.
    assert!(format!("{server:?}").contains("npx"));
}

#[test]
fn raw_model_preset_debug_and_display_hide_secret_values() {
    let preset = DotagentsModelPreset {
        name: "fast".to_string(),
        provider: "anthropic".to_string(),
        model: "claude".to_string(),
        credential: Some("raw-secret-credential".to_string()),
        parameters: BTreeMap::from([(
            "api_key".to_string(),
            serde_json::json!("raw-secret-parameter"),
        )]),
        extensions: BTreeMap::new(),
        source: DotagentsSource::entry(
            DotagentsLayer::Workspace,
            "/ws/.agents/models.json",
            "fast",
        ),
    };

    for rendered in [format!("{preset:?}"), format!("{preset}")] {
        assert!(!rendered.contains("raw-secret-credential"));
        assert!(!rendered.contains("raw-secret-parameter"));
        assert!(rendered.contains("anthropic"));
        assert!(rendered.contains("claude"));
    }
}

#[test]
fn manifest_debug_hides_protocol_secrets_recursively() {
    let mut manifest = DotagentsManifest::empty();

    let mut server = mcp_server(DotagentsMcpTransport::Stdio, true);
    server.command = Some("npx".to_string());
    server
        .env
        .insert("TOKEN".to_string(), "manifest-secret-env".to_string());
    server.headers.insert(
        "Authorization".to_string(),
        "Bearer manifest-secret-header".to_string(),
    );
    manifest.mcp_servers.insert(server.name.clone(), server);

    let preset = DotagentsModelPreset {
        name: "fast".to_string(),
        provider: "anthropic".to_string(),
        model: "claude".to_string(),
        credential: Some("manifest-secret-credential".to_string()),
        parameters: BTreeMap::from([(
            "api_key".to_string(),
            serde_json::json!("manifest-secret-parameter"),
        )]),
        extensions: BTreeMap::new(),
        source: DotagentsSource::entry(
            DotagentsLayer::Workspace,
            "/ws/.agents/models.json",
            "fast",
        ),
    };
    manifest.model_presets.insert(preset.name.clone(), preset);

    let rendered = format!("{manifest:?}");
    for secret in [
        "manifest-secret-env",
        "manifest-secret-header",
        "manifest-secret-credential",
        "manifest-secret-parameter",
    ] {
        assert!(
            !rendered.contains(secret),
            "manifest Debug leaked `{secret}`: {rendered}"
        );
    }
    // Effective configuration stays inspectable.
    assert!(rendered.contains("TOKEN"));
    assert!(rendered.contains("Authorization"));
    assert!(rendered.contains("anthropic"));
}

fn preset_manifest(provider: &str, credential: Option<&str>) -> DotagentsManifest {
    let mut manifest = DotagentsManifest::default();
    manifest.model_presets.insert(
        "fast".to_string(),
        DotagentsModelPreset {
            name: "fast".to_string(),
            provider: provider.to_string(),
            model: "claude-sonnet".to_string(),
            credential: credential.map(str::to_string),
            parameters: BTreeMap::from([
                ("temperature".to_string(), serde_json::json!(0.2)),
                ("num_ctx".to_string(), serde_json::json!(8192)),
            ]),
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(
                DotagentsLayer::Workspace,
                "/ws/.agents/models.json",
                "fast",
            ),
        },
    );
    manifest
}

#[test]
fn model_preset_selection_converts_to_overlay() {
    unsafe {
        std::env::set_var("QMT_DOTAGENTS_PRESET_KEY", "preset-key-1");
    }
    let manifest = preset_manifest("ollama", Some("${QMT_DOTAGENTS_PRESET_KEY}"));

    let overlay = adapters::select_model_overlay(&manifest, "fast").expect("preset is selectable");
    assert_eq!(overlay.provider, "ollama");
    assert_eq!(overlay.model, "claude-sonnet");
    assert_eq!(overlay.api_key.as_deref(), Some("preset-key-1"));
    // Unknown parameters are carried, not dropped.
    assert!(overlay.parameters.contains_key("num_ctx"));

    unsafe {
        std::env::remove_var("QMT_DOTAGENTS_PRESET_KEY");
    }
}

#[test]
fn model_preset_selection_unknown_preset_is_actionable() {
    let manifest = preset_manifest("ollama", None);
    let err =
        adapters::select_model_overlay(&manifest, "missing").expect_err("unknown preset must fail");
    assert_eq!(err.code, DotagentsDiagnosticCode::UnknownPreset);
    assert!(err.message.contains("missing"));
}

#[test]
fn model_preset_credential_missing_env_is_actionable() {
    let manifest = preset_manifest("ollama", Some("${QMT_DOTAGENTS_DEFINITELY_ABSENT_VAR}"));
    let err = adapters::select_model_overlay(&manifest, "fast")
        .expect_err("unresolvable credential must fail");
    assert_eq!(err.code, DotagentsDiagnosticCode::MissingEnvironment);
    assert!(err.message.contains("fast"));
    assert!(err.message.contains("credential"));
    assert!(err.message.contains("QMT_DOTAGENTS_DEFINITELY_ABSENT_VAR"));
}

#[test]
fn model_preset_overlay_preserves_unrelated_base_params() {
    let manifest = preset_manifest("ollama", None);
    let overlay = adapters::select_model_overlay(&manifest, "fast").unwrap();

    let mut base = querymt::LLMParams::new();
    base.name = Some("explicit-name".to_string());
    base.system = vec!["explicit system".to_string()];
    base.temperature = Some(0.5);
    base.custom = Some(std::collections::HashMap::from([(
        "existing_custom".to_string(),
        serde_json::json!("kept"),
    )]));

    overlay.apply_to(&mut base).expect("overlay applies");

    // Unrelated explicit settings survive.
    assert_eq!(base.name.as_deref(), Some("explicit-name"));
    assert_eq!(base.system, vec!["explicit system".to_string()]);
    // Represented fields are replaced / added.
    assert_eq!(base.provider.as_deref(), Some("ollama"));
    assert_eq!(base.model.as_deref(), Some("claude-sonnet"));
    assert_eq!(base.temperature, Some(0.2));
    // Recognized parameter mapped to a typed field; unknown to custom.
    let custom = base.custom.unwrap();
    assert_eq!(custom.get("num_ctx"), Some(&serde_json::json!(8192)));
    assert_eq!(
        custom.get("existing_custom"),
        Some(&serde_json::json!("kept"))
    );
}

#[test]
fn model_preset_invalid_typed_parameter_is_rejected() {
    let mut manifest = preset_manifest("ollama", None);
    manifest
        .model_presets
        .get_mut("fast")
        .unwrap()
        .parameters
        .insert("temperature".to_string(), serde_json::json!("hot"));

    let overlay = adapters::select_model_overlay(&manifest, "fast").unwrap();
    let mut params = querymt::LLMParams::new();
    let err = overlay.apply_to(&mut params).expect_err("invalid value");
    assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
    assert!(err.message.contains("temperature"));
}

// ══════════════════════════════════════════════════════════════════════════
//  Task 3.1 — neutral sub-agent runtime plans
// ══════════════════════════════════════════════════════════════════════════

fn agent_entry(id: &str) -> DotagentsAgent {
    DotagentsAgent {
        id: id.to_string(),
        name: id.to_string(),
        description: format!("{id} description"),
        enabled: true,
        role: DotagentsAgentRole::DelegationTarget,
        connection: DotagentsAgentConnection::default(),
        capabilities: vec!["read".to_string()],
        config: DotagentsAgentConfig::default(),
        body: format!("{id} body"),
        extensions: BTreeMap::new(),
        source: DotagentsSource::entry(
            DotagentsLayer::Workspace,
            format!("/ws/.agents/agents/{id}/agent.md"),
            id,
        ),
        fingerprint: format!("fp-{id}"),
    }
}

fn subagent_manifest() -> DotagentsManifest {
    let mut manifest = preset_manifest("anthropic", None);
    manifest.mcp_servers.insert(
        "fs".to_string(),
        DotagentsMcpServer {
            name: "fs".to_string(),
            transport: DotagentsMcpTransport::Stdio,
            declared_transport: Some("stdio".to_string()),
            command: Some("npx".to_string()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(DotagentsLayer::Workspace, "/ws/.agents/mcp.json", "fs"),
        },
    );
    manifest
}

#[test]
fn subagent_plan_maps_metadata_prompt_and_supported_config() {
    let mut manifest = subagent_manifest();
    let mut agent = agent_entry("helper");
    agent.config.model_preset = Some("fast".to_string());
    agent.config.tools = Some(vec!["read_tool".to_string()]);
    agent.config.mcp_servers = Some(vec!["fs".to_string()]);
    manifest.agents.insert("helper".to_string(), agent);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    assert_eq!(plans.plans.len(), 1);
    let plan = plans.get("helper").expect("helper is planned");

    // Metadata advertised to delegation consumers.
    assert_eq!(plan.info.id, "helper");
    assert_eq!(plan.info.name, "helper");
    assert_eq!(plan.info.description, "helper description");
    assert_eq!(plan.info.capabilities, vec!["read".to_string()]);
    assert_eq!(plan.system_prompt, "helper body");

    // Model preset becomes an overlay without dropping unrelated settings.
    let model = plan.model.as_ref().expect("preset resolves to an overlay");
    assert_eq!(model.provider, "anthropic");
    assert_eq!(model.model, "claude-sonnet");
    assert!(model.parameters.contains_key("num_ctx"));

    // Tool and MCP restrictions are carried through.
    assert_eq!(plan.tools, Some(vec!["read_tool".to_string()]));
    assert_eq!(plan.mcp_servers.len(), 1);
    assert_eq!(plan.mcp_servers[0].name(), "fs");
    assert!(plan.diagnostics.is_empty(), "{:?}", plan.diagnostics);
}

#[test]
fn subagent_plan_skips_disabled_profiles_without_diagnostics() {
    let mut manifest = subagent_manifest();
    let mut agent = agent_entry("helper");
    agent.enabled = false;
    manifest.agents.insert("helper".to_string(), agent);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    assert!(plans.is_empty());
    assert!(plans.diagnostics.is_empty());
}

#[test]
fn subagent_plan_skips_non_delegation_roles() {
    let mut manifest = subagent_manifest();
    let mut agent = agent_entry("helper");
    agent.role = DotagentsAgentRole::Other;
    manifest.agents.insert("helper".to_string(), agent);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    assert!(plans.is_empty());
    assert!(plans.diagnostics.is_empty());
}

#[test]
fn subagent_plan_reports_unknown_preset_and_keeps_target() {
    let mut manifest = subagent_manifest();
    let mut agent = agent_entry("helper");
    agent.config.model_preset = Some("missing".to_string());
    manifest.agents.insert("helper".to_string(), agent);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let plan = plans.get("helper").expect("target is retained");
    assert!(plan.model.is_none());
    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.code == DotagentsDiagnosticCode::UnknownPreset
                && d.message.contains("missing"))
    );
}

#[test]
fn subagent_plan_reports_unresolved_mcp_reference() {
    let mut manifest = subagent_manifest();
    let mut agent = agent_entry("helper");
    agent.config.mcp_servers = Some(vec!["absent".to_string()]);
    manifest.agents.insert("helper".to_string(), agent);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let plan = plans.get("helper").expect("target is retained");
    assert!(plan.mcp_servers.is_empty());
    assert!(plan.diagnostics.iter().any(|d| {
        d.code == DotagentsDiagnosticCode::UnresolvedReference && d.message.contains("absent")
    }));
}

#[test]
fn subagent_plan_reports_unsupported_config_fields() {
    let mut manifest = subagent_manifest();
    let mut agent = agent_entry("helper");
    agent
        .config
        .extensions
        .insert("sandbox".to_string(), serde_json::json!(true));
    manifest.agents.insert("helper".to_string(), agent);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let plan = plans.get("helper").expect("target is retained");
    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.message.contains("sandbox") && d.message.contains("no effect"))
    );
}

#[test]
fn subagent_plan_rejects_executable_connection_without_launching() {
    let mut manifest = subagent_manifest();
    let mut agent = agent_entry("helper");
    agent.connection = DotagentsAgentConnection {
        connection_type: DotagentsAgentConnectionType::Stdio,
        declared_type: Some("stdio".to_string()),
        command: Some("totally-not-launched".to_string()),
    };
    manifest.agents.insert("helper".to_string(), agent);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let plan = plans
        .get("helper")
        .expect("target is retained for inspection");
    // No model overlay or MCP servers: must not be materialized.
    assert!(plan.model.is_none());
    assert!(plan.mcp_servers.is_empty());
    assert!(plan.tools.is_none());
    let diagnostic = plan
        .diagnostics
        .iter()
        .find(|d| d.code == DotagentsDiagnosticCode::UnsupportedTransport)
        .expect("unsupported connection is diagnosed");
    assert!(diagnostic.message.contains("helper"));
    assert!(diagnostic.message.contains("stdio"));
    assert!(diagnostic.message.contains("totally-not-launched"));
    assert!(diagnostic.is_error());
}

#[test]
fn subagent_plan_rejects_unknown_connection_type() {
    let mut manifest = subagent_manifest();
    let mut agent = agent_entry("helper");
    agent.connection = DotagentsAgentConnection {
        connection_type: DotagentsAgentConnectionType::Unknown,
        declared_type: Some("carrier-pigeon".to_string()),
        command: None,
    };
    manifest.agents.insert("helper".to_string(), agent);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let plan = plans.get("helper").expect("target is retained");
    let diagnostic = plan
        .diagnostics
        .iter()
        .find(|d| d.code == DotagentsDiagnosticCode::UnsupportedTransport)
        .expect("unknown connection is diagnosed");
    assert!(diagnostic.message.contains("carrier-pigeon"));
}

#[test]
fn subagent_plan_includes_all_enabled_mcp_servers_when_unrestricted() {
    let mut manifest = subagent_manifest();
    manifest.mcp_servers.insert(
        "off".to_string(),
        DotagentsMcpServer {
            name: "off".to_string(),
            transport: DotagentsMcpTransport::Stdio,
            declared_transport: Some("stdio".to_string()),
            command: Some("npx".to_string()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: false,
            extensions: BTreeMap::new(),
            source: DotagentsSource::entry(
                DotagentsLayer::Workspace,
                "/ws/.agents/mcp.json",
                "off",
            ),
        },
    );
    manifest
        .agents
        .insert("helper".to_string(), agent_entry("helper"));

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    let plan = plans.get("helper").expect("helper is planned");
    // Enabled shared server included; disabled one excluded.
    assert_eq!(plan.mcp_servers.len(), 1);
    assert_eq!(plan.mcp_servers[0].name(), "fs");
}

#[test]
fn subagent_plan_is_isolated_per_target() {
    let mut manifest = subagent_manifest();
    manifest
        .agents
        .insert("good".to_string(), agent_entry("good"));
    let mut bad = agent_entry("bad");
    bad.connection = DotagentsAgentConnection {
        connection_type: DotagentsAgentConnectionType::Unknown,
        declared_type: Some("mystery".to_string()),
        command: None,
    };
    manifest.agents.insert("bad".to_string(), bad);

    let plans = DotagentsSubAgentPlans::from_manifest(&manifest);
    assert_eq!(plans.plans.len(), 2);
    // The valid sibling is unaffected by the unsupported one.
    let good = plans.get("good").expect("valid sibling is planned");
    assert!(
        good.diagnostics.is_empty(),
        "valid sibling must not inherit diagnostics: {:?}",
        good.diagnostics
    );
    assert!(good.mcp_servers[0].name() == "fs");
    assert!(
        plans
            .get("bad")
            .unwrap()
            .diagnostics
            .iter()
            .any(|d| d.code == DotagentsDiagnosticCode::UnsupportedTransport)
    );
}

#[tokio::test]
async fn model_preset_provider_availability_is_validated() {
    // Unavailable provider: models.json does not install providers.
    let (empty_registry, _dir) = crate::test_utils::empty_plugin_registry().unwrap();
    let manifest = preset_manifest("not-installed", None);
    let err = adapters::validate_preset_provider(&empty_registry, &manifest, "fast")
        .await
        .expect_err("unavailable provider must fail");
    assert_eq!(err.code, DotagentsDiagnosticCode::UnresolvedReference);
    assert!(err.message.contains("not-installed"));
    assert!(err.message.contains("fast"));

    // Available provider validates successfully.
    let (mock_registry, _dir2) = crate::test_utils::empty_plugin_registry().unwrap();
    crate::test_utils::register_mock_provider(&mock_registry);
    let manifest = preset_manifest("mock", None);
    adapters::validate_preset_provider(&mock_registry, &manifest, "fast")
        .await
        .expect("available provider validates");
}
