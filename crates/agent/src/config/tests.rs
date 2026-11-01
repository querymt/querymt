use super::*;

use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn system_cache_hints_survive_file_resolution_and_inline_config() {
    use querymt::chat::CacheHint;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("base.md"),
        "Stable {{ model }} instructions",
    )
    .unwrap();
    let agent = parse_agent(
        r#"system = [
        { file = "base.md", cache = { ephemeral = { ttl_seconds = 1800 } } },
        { text = "changing {{ provider }}", cache = { ephemeral = {} } },
        "legacy"
    ]"#,
    );
    let resolved = resolve_system_parts(&agent.system, dir.path(), "test")
        .await
        .unwrap();
    assert_eq!(resolved[0].text(), "Stable {{ model }} instructions");
    assert_eq!(
        resolved[0].cache(),
        Some(&CacheHint::Ephemeral {
            ttl_seconds: Some(1800)
        })
    );
    assert_eq!(
        resolved[1].cache(),
        Some(&CacheHint::Ephemeral { ttl_seconds: None })
    );
    assert!(resolved[2].cache().is_none());
    let source = r#"[agent]
provider = "openai"
model = "gpt-5.6"
system = [{ text = "stable", cache = { ephemeral = { ttl_seconds = 1800 } } }]
"#;
    let config = load_config(ConfigSource::Toml(source.into()))
        .await
        .unwrap();
    let Config::Single(config) = config else {
        panic!("single config");
    };
    assert_eq!(
        config.agent.system[0].resolved().unwrap().cache(),
        resolved[0].cache()
    );
    // Invalid metadata must not fall through the untagged enum to an unhinted file.
    assert!(
        serde_json::from_value::<SystemPart>(
            serde_json::json!({"file": "base.md", "cache": {"unknown": {}}})
        )
        .is_err()
    );
}

#[tokio::test]
async fn plain_file_and_inline_sections_receive_runtime_cache_defaults() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("base.md"), "stable instructions").unwrap();
    let agent = parse_agent(
        r#"system = [{ file = "base.md" }, "Dynamic instructions after the stable prefix."]"#,
    );
    let resolved = resolve_system_parts(&agent.system, dir.path(), "test")
        .await
        .unwrap();
    let mut config = serde_json::json!({"system": resolved, "api_mode": "responses"});
    crate::model_heuristics::ModelDefaults::for_model("openai", "gpt-5.6", None)
        .apply_to(&mut config, "session");
    assert_eq!(config["system"][0]["text"], "stable instructions");
    assert_eq!(
        config["system"][0]["cache"],
        serde_json::json!({"ephemeral": {}})
    );
    assert_eq!(
        config["system"][1]["cache"],
        serde_json::json!({"ephemeral": {}})
    );
}

#[test]
fn test_interpolate_env_vars() {
    unsafe {
        std::env::set_var("TEST_VAR", "test_value");
        std::env::set_var("TEST_VAR2", "value2");
    }

    let input = "provider = \"${TEST_VAR}\"\nmodel = \"${TEST_VAR2:-default}\"";
    let result = interpolate_env_vars(input).unwrap();
    assert_eq!(result, "provider = \"test_value\"\nmodel = \"value2\"");

    let with_default = "model = \"${MISSING_VAR:-gpt-4}\"";
    let result = interpolate_env_vars(with_default).unwrap();
    assert_eq!(result, "model = \"gpt-4\"");

    let missing = "model = \"${MISSING_REQUIRED}\"";
    assert!(interpolate_env_vars(missing).is_err());
}

/// Helper to deserialize an AgentSettings from a TOML fragment
fn parse_agent(toml: &str) -> AgentSettings {
    let full = format!(
        "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\n{}",
        toml
    );
    #[derive(Deserialize)]
    struct Wrapper {
        agent: AgentSettings,
    }
    toml::from_str::<Wrapper>(&full)
        .expect("Failed to parse TOML")
        .agent
}

fn make_temp_prompt(contents: &str) -> (PathBuf, PathBuf) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("querymt-prompt-{nanos}"));
    std::fs::create_dir_all(&dir).expect("Failed to create temp prompt dir");
    let file = PathBuf::from("prompt.md");
    std::fs::write(dir.join(&file), contents).expect("Failed to write temp prompt");
    (dir, file)
}

fn temp_config_path(filename: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time before epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("querymt-config-{filename}-{nanos}.toml"))
}

#[test]
fn test_system_absent() {
    let agent = parse_agent("");
    assert!(agent.system.is_empty());
    assert_eq!(agent.permissions, PermissionsConfig::default());
}

#[test]
fn test_permissions_section_parses() {
    let agent = parse_agent(
        "[agent.permissions]\nask = [\"shell\"]\nalways_ask = [\"ibkr.place_order\"]\ntimeout_sec = 120",
    );
    assert_eq!(agent.permissions.ask, vec!["shell".parse().unwrap()]);
    assert_eq!(
        agent.permissions.always_ask,
        vec!["ibkr.place_order".parse().unwrap()]
    );
    assert_eq!(agent.permissions.timeout_sec.map(|s| s.get()), Some(120));
}

#[test]
fn test_removed_permission_keys_fail_naming_the_key() {
    for (key, line) in [
        ("assume_mutating", "assume_mutating = false"),
        ("mutating_tools", "mutating_tools = [\"edit\"]"),
        ("confirm_tools", "confirm_tools = [\"shell\"]"),
        ("confirm_timeout_sec", "confirm_timeout_sec = 30"),
    ] {
        let err =
            toml::from_str::<AgentSettings>(&format!("provider = \"p\"\nmodel = \"m\"\n{line}"))
                .unwrap_err()
                .to_string();
        assert!(
            err.contains("unknown field") && err.contains(key),
            "{key}: {err}"
        );
    }
}

#[test]
fn test_system_single_string() {
    let agent = parse_agent("system = \"hello\"");
    assert_eq!(agent.system.len(), 1);
    assert!(matches!(&agent.system[0], SystemPart::Inline(s) if s == "hello"));
}

#[test]
fn test_system_array_of_strings() {
    let agent = parse_agent("system = [\"part1\", \"part2\"]");
    assert_eq!(agent.system.len(), 2);
    assert!(matches!(&agent.system[0], SystemPart::Inline(s) if s == "part1"));
    assert!(matches!(&agent.system[1], SystemPart::Inline(s) if s == "part2"));
}

#[test]
fn test_system_file_reference() {
    let agent = parse_agent("system = [{ file = \"prompts/coder.md\" }]");
    assert_eq!(agent.system.len(), 1);
    assert!(
        matches!(&agent.system[0], SystemPart::File { file } if file == Path::new("prompts/coder.md"))
    );
}

#[test]
fn test_system_mixed_inline_and_file() {
    let agent = parse_agent(
        r#"system = ["You are helpful.", { file = "prompts/rules.md" }, "Be concise."]"#,
    );
    assert_eq!(agent.system.len(), 3);
    assert!(matches!(&agent.system[0], SystemPart::Inline(s) if s == "You are helpful."));
    assert!(
        matches!(&agent.system[1], SystemPart::File { file } if file == Path::new("prompts/rules.md"))
    );
    assert!(matches!(&agent.system[2], SystemPart::Inline(s) if s == "Be concise."));
}

#[test]
fn test_system_multiple_file_references() {
    let agent =
        parse_agent(r#"system = [{ file = "prompts/base.md" }, { file = "prompts/extra.md" }]"#);
    assert_eq!(agent.system.len(), 2);
    assert!(
        matches!(&agent.system[0], SystemPart::File { file } if file == Path::new("prompts/base.md"))
    );
    assert!(
        matches!(&agent.system[1], SystemPart::File { file } if file == Path::new("prompts/extra.md"))
    );
}

#[tokio::test]
async fn test_resolve_system_parts_inline_only() {
    let parts = vec![
        SystemPart::Inline("hello".into()),
        SystemPart::Inline("world".into()),
    ];
    let resolved = resolve_system_parts(&parts, Path::new("."), "test")
        .await
        .unwrap();
    assert_eq!(resolved, vec!["hello", "world"]);
}

#[tokio::test]
async fn test_resolve_system_parts_file_not_found() {
    let parts = vec![SystemPart::File {
        file: PathBuf::from("nonexistent_prompt.md"),
    }];
    let result = resolve_system_parts(&parts, Path::new("."), "test").await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Failed to load test prompt")
    );
}

#[tokio::test]
async fn test_resolve_system_parts_file_env_vars() {
    unsafe {
        std::env::set_var("TEST_PROMPT_VAR", "resolved");
    }

    let (dir, file) = make_temp_prompt("Hello ${TEST_PROMPT_VAR}!");
    let parts = vec![SystemPart::File { file }];
    let resolved = resolve_system_parts(&parts, &dir, "test").await.unwrap();
    assert_eq!(resolved, vec!["Hello resolved!"]);
}

#[tokio::test]
async fn test_resolve_system_parts_file_env_default() {
    let (dir, file) = make_temp_prompt("Model ${MISSING_PROMPT_VAR:-gpt-4}");
    let parts = vec![SystemPart::File { file }];
    let resolved = resolve_system_parts(&parts, &dir, "test").await.unwrap();
    assert_eq!(resolved, vec!["Model gpt-4"]);
}

#[tokio::test]
async fn test_resolve_system_parts_file_env_missing() {
    let (dir, file) = make_temp_prompt("${MISSING_PROMPT_REQUIRED}");
    let parts = vec![SystemPart::File { file }];
    let result = resolve_system_parts(&parts, &dir, "test").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_load_config_inline_rejects_file_references() {
    let inline = r#"
[agent]
provider = "test"
model = "test-model"
tools = []
system = [{ file = "prompts/agent.md" }]
"#;

    let err = load_config(ConfigSource::Toml(inline.to_string()))
        .await
        .expect_err("inline TOML with file references should fail");
    let msg = err.to_string();
    assert!(msg.contains("inline TOML config"));
    assert!(msg.contains("agent.system"));
}

#[tokio::test]
async fn test_load_config_rejects_agent_db_field() {
    let inline = r#"
[agent]
db = "./old-agent.db"
provider = "test"
model = "test-model"
"#;

    let err = load_config(ConfigSource::Toml(inline.to_string()))
        .await
        .expect_err("agent db config field should be rejected");
    assert!(
        err.to_string()
            .contains("Failed to deserialize single agent config")
    );
}

#[tokio::test]
async fn test_load_config_rejects_quorum_db_field() {
    let inline = r#"
[quorum]
db = "./old-quorum.db"

[planner]
provider = "test"
model = "test-model"
system = "plan"
"#;

    let err = load_config(ConfigSource::Toml(inline.to_string()))
        .await
        .expect_err("quorum db config field should be rejected");
    assert!(
        err.to_string()
            .contains("Failed to deserialize quorum config")
    );
}

#[tokio::test]
async fn test_load_config_inline_accepts_inline_prompts() {
    let inline = r#"
[agent]
provider = "test"
model = "test-model"
tools = []
system = ["You are a test agent"]
"#;

    let cfg = load_config(ConfigSource::Toml(inline.to_string()))
        .await
        .expect("inline-only config should load");

    match cfg {
        Config::Single(single) => {
            assert!(matches!(
                &single.agent.system[0],
                SystemPart::Inline(s) if s == "You are a test agent"
            ));
        }
        Config::Multi(_) => panic!("expected single-agent config"),
    }
}

#[tokio::test]
async fn test_load_config_accepts_parallel_edit_guard_middleware() {
    // Mirrors the example in docs/docs/agent/configuration.md and middleware.md.
    let inline = r#"
[agent]
provider = "test"
model = "test-model"
tools = []
system = ["You are a test agent"]

[[middleware]]
type = "parallel_edit_guard"
mode = "deny"

[[middleware]]
type = "parallel_edit_guard"
enabled = false
"#;

    let cfg = load_config(ConfigSource::Toml(inline.to_string()))
        .await
        .expect("parallel_edit_guard config should load");

    match cfg {
        Config::Single(single) => {
            assert_eq!(single.middleware.len(), 2);
            assert!(
                single
                    .middleware
                    .iter()
                    .all(|m| m.middleware_type == "parallel_edit_guard")
            );
            assert_eq!(single.middleware[0].config["mode"], "deny");
            assert_eq!(single.middleware[1].config["enabled"], false);
        }
        Config::Multi(_) => panic!("expected single-agent config"),
    }
}

#[tokio::test]
async fn test_load_config_accepts_profile_metadata_for_single_and_quorum() {
    let single = r#"
[profile]
id = "friendly-single"
name = "Friendly Single"
tags = ["coding"]

[agent]
provider = "test"
model = "test-model"
tools = []
system = ["You are a test agent"]
"#;
    let cfg = load_config(ConfigSource::Toml(single.to_string()))
        .await
        .expect("single config with profile metadata should load");
    assert!(matches!(cfg, Config::Single(_)));

    let quorum = r#"
[profile]
id = "friendly-quorum"

[quorum]

[planner]
provider = "test"
model = "planner-model"
system = "plan"
"#;
    let cfg = load_config(ConfigSource::Toml(quorum.to_string()))
        .await
        .expect("quorum config with profile metadata should load");
    assert!(matches!(cfg, Config::Multi(_)));
}

#[tokio::test]
async fn test_load_config_rejects_unknown_agent_fields_with_profile_metadata() {
    let inline = r#"
[profile]
id = "friendly-single"

[agent]
provider = "test"
model = "test-model"
tools = []
system = ["You are a test agent"]
unknown = true
"#;

    let err = load_config(ConfigSource::Toml(inline.to_string()))
        .await
        .expect_err("unknown runtime fields should stay rejected");
    assert!(
        err.to_string()
            .contains("Failed to deserialize single agent config"),
        "message was: {err}"
    );
}

#[tokio::test]
async fn test_load_config_path_with_profile_metadata_resolves_file_references() {
    let (prompt_dir, prompt_file) = make_temp_prompt("Prompt from profile file");
    let prompt_path = prompt_dir.join(prompt_file);
    let config_path = temp_config_path("profile-single");
    let config = format!(
        "[profile]\nid = \"file-profile\"\n\n[agent]\nprovider = \"test\"\nmodel = \"test-model\"\ntools = []\nsystem = [{{ file = {} }}]\n",
        crate::test_utils::toml_path_value(&prompt_path)
    );
    std::fs::write(&config_path, config).expect("failed to write temp config");

    let cfg = load_config(&config_path)
        .await
        .expect("file config with profile metadata should load");
    match cfg {
        Config::Single(single) => {
            assert!(matches!(
                &single.agent.system[0],
                SystemPart::Inline(s) if s == "Prompt from profile file"
            ));
        }
        Config::Multi(_) => panic!("expected single-agent config"),
    }

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test]
async fn test_load_config_path_resolves_file_references() {
    let (prompt_dir, prompt_file) = make_temp_prompt("Prompt from file");
    let prompt_path = prompt_dir.join(prompt_file);
    let config_path = temp_config_path("single");
    let config = format!(
        "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\ntools = []\nsystem = [{{ file = {} }}]\n",
        crate::test_utils::toml_path_value(&prompt_path)
    );
    std::fs::write(&config_path, config).expect("failed to write temp config");

    let cfg = load_config(&config_path)
        .await
        .expect("file config should load");
    match cfg {
        Config::Single(single) => {
            assert!(matches!(
                &single.agent.system[0],
                SystemPart::Inline(s) if s == "Prompt from file"
            ));
        }
        Config::Multi(_) => panic!("expected single-agent config"),
    }

    let _ = std::fs::remove_file(&config_path);
    let _ = std::fs::remove_file(&prompt_path);
    let _ = std::fs::remove_dir_all(&prompt_dir);
}

#[test]
fn test_interpolate_toml_value_strings() {
    unsafe {
        std::env::set_var("TOML_TEST_VAR", "interpolated");
    }

    let toml_str = r#"
            provider = "${TOML_TEST_VAR}"
            model = "gpt-4"
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    crate::config::loader::interpolate_toml_value(&mut value).unwrap();

    let table = value.as_table().unwrap();
    assert_eq!(
        table.get("provider").unwrap().as_str().unwrap(),
        "interpolated"
    );
    assert_eq!(table.get("model").unwrap().as_str().unwrap(), "gpt-4");
}

#[test]
fn test_interpolate_toml_value_arrays() {
    unsafe {
        std::env::set_var("TOML_ARRAY_VAR", "value1");
    }

    let toml_str = r#"
            tools = ["${TOML_ARRAY_VAR}", "tool2"]
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    crate::config::loader::interpolate_toml_value(&mut value).unwrap();

    let table = value.as_table().unwrap();
    let tools = table.get("tools").unwrap().as_array().unwrap();
    assert_eq!(tools[0].as_str().unwrap(), "value1");
    assert_eq!(tools[1].as_str().unwrap(), "tool2");
}

#[test]
fn test_interpolate_toml_value_nested_tables() {
    unsafe {
        std::env::set_var("TOML_NESTED_VAR", "nested_value");
    }

    let toml_str = r#"
            [agent]
            provider = "${TOML_NESTED_VAR}"
            model = "gpt-4"
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    crate::config::loader::interpolate_toml_value(&mut value).unwrap();

    let table = value.as_table().unwrap();
    let agent = table.get("agent").unwrap().as_table().unwrap();
    assert_eq!(
        agent.get("provider").unwrap().as_str().unwrap(),
        "nested_value"
    );
}

#[test]
fn test_interpolate_toml_value_with_default() {
    let toml_str = r#"
            provider = "${TOML_MISSING_VAR:-default_provider}"
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    crate::config::loader::interpolate_toml_value(&mut value).unwrap();

    let table = value.as_table().unwrap();
    assert_eq!(
        table.get("provider").unwrap().as_str().unwrap(),
        "default_provider"
    );
}

#[test]
fn test_comments_with_env_vars_full_line() {
    // Full-line comments with ${VAR} should not cause errors
    let toml_str = r#"
            # This is a comment with ${SOME_VAR} that should be ignored
            provider = "anthropic"
            # Another comment: ${ANOTHER_VAR}
            model = "claude-3-5-sonnet-20241022"
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    // Should not error even though SOME_VAR and ANOTHER_VAR are not set
    assert!(crate::config::loader::interpolate_toml_value(&mut value).is_ok());

    let table = value.as_table().unwrap();
    assert_eq!(
        table.get("provider").unwrap().as_str().unwrap(),
        "anthropic"
    );
    assert_eq!(
        table.get("model").unwrap().as_str().unwrap(),
        "claude-3-5-sonnet-20241022"
    );
}

#[test]
fn test_comments_with_env_vars_inline() {
    // Inline comments with ${VAR} should not cause errors
    let toml_str = r#"
            provider = "anthropic"  # Uses ${API_KEY} for auth
            model = "claude-3-5-sonnet-20241022"  # Or use ${MODEL_OVERRIDE}
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    // Should not error even though API_KEY and MODEL_OVERRIDE are not set
    assert!(crate::config::loader::interpolate_toml_value(&mut value).is_ok());

    let table = value.as_table().unwrap();
    assert_eq!(
        table.get("provider").unwrap().as_str().unwrap(),
        "anthropic"
    );
    assert_eq!(
        table.get("model").unwrap().as_str().unwrap(),
        "claude-3-5-sonnet-20241022"
    );
}

#[test]
fn test_strings_still_interpolate_with_comments_present() {
    unsafe {
        std::env::set_var("TEST_PROVIDER_VAR", "openai");
        std::env::set_var("TEST_MODEL_VAR", "gpt-4");
    }

    let toml_str = r#"
            # Comment with ${UNSET_VAR}
            provider = "${TEST_PROVIDER_VAR}"  # Another ${COMMENT_VAR}
            model = "${TEST_MODEL_VAR}"
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    crate::config::loader::interpolate_toml_value(&mut value).unwrap();

    let table = value.as_table().unwrap();
    assert_eq!(table.get("provider").unwrap().as_str().unwrap(), "openai");
    assert_eq!(table.get("model").unwrap().as_str().unwrap(), "gpt-4");
}

#[test]
fn test_mixed_comments_and_interpolation() {
    unsafe {
        std::env::set_var("REAL_VAR", "real_value");
    }

    let toml_str = r#"
            # Top comment ${FAKE_VAR}
            [agent]
            # Section comment ${ANOTHER_FAKE}
            provider = "${REAL_VAR}"  # inline ${INLINE_FAKE}
            model = "test"
            # tools = ["${COMMENTED_OUT_VAR}"]
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    assert!(crate::config::loader::interpolate_toml_value(&mut value).is_ok());

    let table = value.as_table().unwrap();
    let agent = table.get("agent").unwrap().as_table().unwrap();
    assert_eq!(
        agent.get("provider").unwrap().as_str().unwrap(),
        "real_value"
    );
    assert_eq!(agent.get("model").unwrap().as_str().unwrap(), "test");
}

#[test]
fn test_interpolate_missing_var_in_string_still_errors() {
    let toml_str = r#"
            provider = "${DEFINITELY_MISSING_VAR}"
        "#;
    let mut value: toml::Value = toml::from_str(toml_str).unwrap();
    let result = crate::config::loader::interpolate_toml_value(&mut value);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("DEFINITELY_MISSING_VAR")
    );
}

// ── MCP config tests ─────────────────────────────────────────────────────

#[test]
fn test_single_agent_config_parses_mcp_section() {
    let toml = r#"
[agent]
provider = "anthropic"
model = "claude-3-5-sonnet"
tools = ["context7.*"]

[[mcp]]
name = "context7"
transport = "http"
url = "https://mcp.context7.com/mcp"
"#;
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[allow(dead_code)]
        agent: AgentSettings,
        #[serde(default)]
        mcp: Vec<McpServerConfig>,
    }
    let parsed: Wrapper = toml::from_str(toml).expect("TOML should parse");
    assert_eq!(parsed.mcp.len(), 1);
    assert!(
        matches!(&parsed.mcp[0], McpServerConfig::Http { name, url, .. } if name == "context7" && url == "https://mcp.context7.com/mcp")
    );
}

#[test]
fn test_mcp_http_headers_parsed() {
    let toml = r#"
[agent]
provider = "anthropic"
model = "claude-3-5-sonnet"
tools = ["context7.*"]

[[mcp]]
name = "context7"
transport = "http"
url = "https://mcp.context7.com/mcp"
[mcp.headers]
CONTEXT7_API_KEY = "my-api-key"
"#;
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[allow(dead_code)]
        agent: AgentSettings,
        #[serde(default)]
        mcp: Vec<McpServerConfig>,
    }
    #[allow(dead_code)]
    let parsed: Wrapper = toml::from_str(toml).expect("TOML should parse");
    assert_eq!(parsed.mcp.len(), 1);
    if let McpServerConfig::Http { headers, .. } = &parsed.mcp[0] {
        assert_eq!(
            headers.get("CONTEXT7_API_KEY").map(String::as_str),
            Some("my-api-key")
        );
    } else {
        panic!("Expected Http server config");
    }
}

#[test]
fn test_mcp_env_var_interpolation_uses_braces_syntax() {
    unsafe {
        std::env::set_var("MCP_TEST_API_KEY", "secret-key-123");
    }
    // ${VAR} syntax should be interpolated
    let toml_with_braces = r#"provider = "${MCP_TEST_API_KEY}""#;
    let result = interpolate_env_vars(toml_with_braces).unwrap();
    assert_eq!(result, r#"provider = "secret-key-123""#);

    // $VAR syntax (no braces) should NOT be interpolated — literal string
    let toml_without_braces = r#"provider = "$MCP_TEST_API_KEY""#;
    let result = interpolate_env_vars(toml_without_braces).unwrap();
    assert_eq!(result, r#"provider = "$MCP_TEST_API_KEY""#);
}

// ── peer field validation ────────────────────────────────────────────────

#[tokio::test]
async fn test_peer_without_mesh_enabled_is_error() {
    // A delegate with `peer` set requires `[mesh] enabled = true`.
    let toml = r#"
[quorum]
cwd = "/tmp"

[planner]
provider = "openai"
model = "gpt-4"

[[delegates]]
id = "coder"
provider = "llama_cpp"
model = "qwen3"
peer = "gpu-node"
"#;
    // mesh.enabled defaults to false, so this must fail validation
    let result = load_config(ConfigSource::Toml(toml.to_string())).await;
    assert!(result.is_err(), "expected error but got Ok");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("peer") && msg.contains("mesh"),
        "error must mention 'peer' and 'mesh', got: {}",
        msg
    );
}

#[tokio::test]
async fn test_peer_with_mesh_enabled_is_ok() {
    // A delegate with `peer` set is valid when `[mesh] enabled = true`.
    let toml = r#"
[quorum]
cwd = "/tmp"

[planner]
provider = "openai"
model = "gpt-4"

[mesh]
enabled = true

[[delegates]]
id = "coder"
provider = "llama_cpp"
model = "qwen3"
peer = "gpu-node"
"#;
    let result = load_config(ConfigSource::Toml(toml.to_string())).await;
    assert!(result.is_ok(), "expected Ok but got: {:?}", result.err());
}

// ── peer field on DelegateConfig ────────────────────────────────────────

fn parse_delegate(toml: &str) -> DelegateConfig {
    #[derive(Deserialize)]
    struct Wrapper {
        delegates: Vec<DelegateConfig>,
    }
    toml::from_str::<Wrapper>(toml)
        .expect("Failed to parse TOML")
        .delegates
        .into_iter()
        .next()
        .expect("No delegates in TOML")
}

#[test]
fn test_delegate_config_peer_field_parses() {
    let toml = r#"
[[delegates]]
id = "coder"
provider = "llama_cpp"
model = "qwen3"
peer = "gpu-node"
"#;
    let delegate = parse_delegate(toml);
    assert_eq!(delegate.peer, Some("gpu-node".to_string()));
}

#[test]
fn test_delegate_config_peer_field_absent_defaults_none() {
    let toml = r#"
[[delegates]]
id = "coder"
provider = "openai"
model = "gpt-4"
"#;
    let delegate = parse_delegate(toml);
    assert_eq!(delegate.peer, None);
}

#[test]
fn test_delegate_config_peer_field_does_not_break_existing_configs() {
    // Configs without `peer` must still deserialize correctly.
    let toml = r#"
[[delegates]]
id = "writer"
provider = "anthropic"
model = "claude-3-haiku"
description = "Writing specialist"
capabilities = ["writing"]
"#;
    let delegate = parse_delegate(toml);
    assert_eq!(delegate.id, "writer");
    assert_eq!(delegate.peer, None);
    assert_eq!(delegate.description, Some("Writing specialist".to_string()));
}

// ── template validation in config loading ────────────────────────────────

/// File containing `{{ unknown_var }}` should error at load time.
#[tokio::test]
async fn test_resolve_system_parts_validates_templates_unknown_var() {
    let (dir, file) = make_temp_prompt("Hello {{ unknown_var }}");
    let parts = vec![SystemPart::File { file }];
    let result = resolve_system_parts(&parts, &dir, "test").await;
    assert!(result.is_err(), "unknown template var should error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("unknown_var"),
        "error should name the unknown variable: {msg}"
    );
}

/// File with `{{ model }}` is a known var — should load without error and
/// the literal template string must be preserved (not resolved yet).
#[tokio::test]
async fn test_resolve_system_parts_valid_template_preserved() {
    let (dir, file) = make_temp_prompt("You are {{ model }}");
    let parts = vec![SystemPart::File { file }];
    let resolved = resolve_system_parts(&parts, &dir, "test")
        .await
        .expect("valid template should load");
    // The string must still contain the template literal — NOT resolved.
    assert_eq!(resolved, vec!["You are {{ model }}"]);
}

/// Inline system string with `{{ bad_var }}` must error when loaded via
/// the `ConfigSource::Toml` path.
#[tokio::test]
async fn test_load_config_inline_template_validated() {
    let inline = r#"
[agent]
provider = "test"
model = "test-model"
tools = []
system = ["You are {{ bad_var }} helpful."]
"#;
    let result = load_config(ConfigSource::Toml(inline.to_string())).await;
    assert!(result.is_err(), "inline unknown template var should error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("bad_var"),
        "error should name the bad variable: {msg}"
    );
}

/// Inline `{{ model }}` is a known var and must load successfully.
#[tokio::test]
async fn test_load_config_inline_valid_template() {
    let inline = r#"
[agent]
provider = "test"
model = "test-model"
tools = []
system = ["You are powered by {{ provider }}/{{ model }}."]
"#;
    let cfg = load_config(ConfigSource::Toml(inline.to_string()))
        .await
        .expect("valid template vars should load");
    match cfg {
        Config::Single(single) => {
            let system_str = match &single.agent.system[0] {
                SystemPart::Inline(s) => s.as_str(),
                _ => panic!("expected inline"),
            };
            // Template must be preserved as a literal — not rendered yet.
            assert!(
                system_str.contains("{{ provider }}"),
                "template should be preserved: {system_str}"
            );
        }
        Config::Multi(_) => panic!("expected single-agent config"),
    }
}

#[tokio::test]
async fn dotagents_settings_default_disabled_for_single_and_quorum() {
    let single = load_config(ConfigSource::Toml(
        "[agent]\nprovider = \"test\"\nmodel = \"model\"\n".to_string(),
    ))
    .await
    .unwrap();
    let Config::Single(single) = single else {
        panic!("expected single config");
    };
    assert!(!single.dotagents.enabled);
    assert!(single.dotagents.global_enabled);
    assert!(single.dotagents.workspace_enabled);

    let quorum = load_config(ConfigSource::Toml(
        "[quorum]\n[planner]\nprovider = \"test\"\nmodel = \"model\"\n".to_string(),
    ))
    .await
    .unwrap();
    let Config::Multi(quorum) = quorum else {
        panic!("expected quorum config");
    };
    assert!(!quorum.dotagents.enabled);
}

#[tokio::test]
async fn dotagents_settings_parse_from_toml() {
    let toml = r#"
[agent]
provider = "test"
model = "model"

[dotagents]
enabled = true
workspace = "/workspace"
global_root = "/global"
workspace_root = "/workspace/.agents"
global_enabled = false
selected_model_preset = "fast"
trusted_roots = ["/shared"]
strictness = "strict"
"#;
    let config = load_config(ConfigSource::Toml(toml.to_string()))
        .await
        .unwrap();
    let Config::Single(config) = config else {
        panic!("expected single config");
    };
    assert!(config.dotagents.enabled);
    assert!(!config.dotagents.global_enabled);
    assert_eq!(
        config.dotagents.selected_model_preset.as_deref(),
        Some("fast")
    );
    assert_eq!(
        config.dotagents.strictness,
        crate::dotagents::DotagentsStrictness::Strict
    );
}

#[test]
fn dotagents_settings_are_present_in_generated_schemas() {
    let single = serde_json::to_value(schemars::schema_for!(SingleAgentConfig)).unwrap();
    let quorum = serde_json::to_value(schemars::schema_for!(QuorumConfig)).unwrap();
    assert!(single.pointer("/properties/dotagents").is_some());
    assert!(quorum.pointer("/properties/dotagents").is_some());
}

// ── Schema enrichment tests ──────────────────────────────────────────────

/// Verifies that the generated JSON Schema for `AgentSettings.tools` mentions
/// every built-in tool name in its description, so models generating configs
/// know exactly which values are valid.
#[test]
fn test_schema_tools_description_covers_all_builtins() {
    use crate::tools::builtins::all_builtin_tools;

    let schema = schemars::schema_for!(SingleAgentConfig);
    let schema_json = serde_json::to_value(&schema).unwrap();

    // Navigate to agent.properties.tools.description
    let description = schema_json
        .pointer("/$defs/AgentSettings/properties/tools/description")
        .and_then(|v| v.as_str())
        .expect("AgentSettings.tools must have a description in the schema");

    let builtin_names: Vec<String> = all_builtin_tools()
        .iter()
        .map(|t| t.name().to_string())
        .collect();

    let mut missing = Vec::new();
    for name in &builtin_names {
        if !description.contains(name.as_str()) {
            missing.push(name.as_str());
        }
    }

    assert!(
        missing.is_empty(),
        "The following built-in tool names are missing from the \
             AgentSettings.tools schema description: {missing:?}\n\
             Update the doc comment on `AgentSettings.tools` in config.rs \
             to include all built-in tool names."
    );
}

/// Verifies that the `MiddlewareEntry.type` field has an enum constraint
/// listing all known middleware types.
#[test]
fn test_schema_middleware_type_has_enum() {
    let schema = schemars::schema_for!(SingleAgentConfig);
    let schema_json = serde_json::to_value(&schema).unwrap();

    let enum_values = schema_json
        .pointer("/$defs/MiddlewareEntry/properties/type/enum")
        .and_then(|v| v.as_array())
        .expect("MiddlewareEntry.type must have an enum in the schema");

    let type_names: Vec<&str> = enum_values.iter().filter_map(|v| v.as_str()).collect();

    for expected in &["limits", "context", "dedup_check", "agent_mode"] {
        assert!(
            type_names.contains(expected),
            "middleware type {expected:?} missing from schema enum: {type_names:?}"
        );
    }
}

/// Verifies that `SnapshotBackendConfig.backend` has enum values `"git"` and `"none"`.
#[test]
fn test_schema_snapshot_backend_has_enum() {
    let schema = schemars::schema_for!(SingleAgentConfig);
    let schema_json = serde_json::to_value(&schema).unwrap();

    let enum_values = schema_json
        .pointer("/$defs/SnapshotBackendConfig/properties/backend/enum")
        .and_then(|v| v.as_array())
        .expect("SnapshotBackendConfig.backend must have an enum in the schema");

    let values: Vec<&str> = enum_values.iter().filter_map(|v| v.as_str()).collect();

    assert!(
        values.contains(&"git"),
        "\"git\" missing from backend enum: {values:?}"
    );
    assert!(
        values.contains(&"none"),
        "\"none\" missing from backend enum: {values:?}"
    );
}

/// Verifies the top-level `SingleAgentConfig` schema has at least one example.
#[test]
fn test_schema_single_agent_config_has_example() {
    let schema = schemars::schema_for!(SingleAgentConfig);
    let schema_json = serde_json::to_value(&schema).unwrap();

    let examples = schema_json
        .get("examples")
        .and_then(|v| v.as_array())
        .expect("SingleAgentConfig schema must have examples");

    assert!(
        !examples.is_empty(),
        "SingleAgentConfig schema must have at least one example"
    );

    // The example must have an "agent" key with "provider" and "model"
    let first = &examples[0];
    assert!(
        first.get("agent").and_then(|a| a.get("provider")).is_some(),
        "First example must have agent.provider"
    );
    assert!(
        first.get("agent").and_then(|a| a.get("model")).is_some(),
        "First example must have agent.model"
    );
}

#[test]
fn test_hooks_config_unknown_event_rejected() {
    let err = toml::from_str::<SingleAgentConfig>(
        r#"
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-sonnet"

[agent.hooks]
enabled = true
UnknownEvent = []
"#,
    )
    .expect("config should deserialize with flattened hook extra");

    let validation = err.agent.hooks.validate();
    assert!(validation.is_err());
    let err = validation.unwrap_err();
    assert!(
        matches!(err, crate::hooks::HookConfigError::UnsupportedEvents(ref keys) if keys == "UnknownEvent"),
        "unexpected error: {err}"
    );
    assert!(err.to_string().contains("unsupported hook event"));
}

// ── Multi-transport mesh config TOML parsing ────────────────────────────

#[test]
fn test_old_mesh_config_parses_without_new_fields() {
    let toml = r#"
[agent]
provider = "anthropic"
model = "claude-3-5-sonnet"

[mesh]
enabled = true
transport = "lan"
discovery = "mdns"
"#;
    let config: SingleAgentConfig = toml::from_str(toml).expect("TOML should parse");
    assert!(config.mesh.enabled);
    assert_eq!(config.mesh.transport, MeshTransportConfig::Lan);
    assert!(config.mesh.iroh_gso);
    assert_eq!(config.mesh.discovery, MeshDiscoveryConfig::Mdns);
    assert!(config.mesh.lan.is_none());
    assert!(config.mesh.iroh.is_empty());
}

#[test]
fn test_new_mesh_config_with_lan_subtable() {
    let toml = r#"
[agent]
provider = "anthropic"
model = "claude-3-5-sonnet"

[mesh]
enabled = true

[mesh.lan]
enabled = true
listen = "/ip4/0.0.0.0/tcp/0"
discovery = "mdns"
"#;
    let config: SingleAgentConfig = toml::from_str(toml).expect("TOML should parse");
    assert!(config.mesh.enabled);
    let lan = config.mesh.lan.as_ref().expect("lan should be present");
    assert!(lan.enabled);
    assert_eq!(lan.listen.as_deref(), Some("/ip4/0.0.0.0/tcp/0"));
    assert_eq!(lan.discovery, MeshDiscoveryConfig::Mdns);
}

#[test]
fn test_new_mesh_config_with_iroh_array() {
    let toml = r#"
[agent]
provider = "anthropic"
model = "claude-3-5-sonnet"

[mesh]
enabled = true

[[mesh.iroh]]
enabled = true
name = "personal"
invite = "test-invite-token"
"#;
    let config: SingleAgentConfig = toml::from_str(toml).expect("TOML should parse");
    assert!(config.mesh.enabled);
    assert_eq!(config.mesh.iroh.len(), 1);
    assert!(config.mesh.iroh[0].enabled);
    assert_eq!(config.mesh.iroh[0].name.as_deref(), Some("personal"));
    assert_eq!(
        config.mesh.iroh[0].invite.as_deref(),
        Some("test-invite-token")
    );
}

#[test]
fn test_new_mesh_config_lan_plus_iroh() {
    let toml = r#"
[agent]
provider = "anthropic"
model = "claude-3-5-sonnet"

[mesh]
enabled = true

[mesh.lan]
enabled = true
discovery = "mdns"

[[mesh.iroh]]
enabled = true
name = "team-a"

[[mesh.iroh]]
enabled = true
name = "team-b"
"#;
    let config: SingleAgentConfig = toml::from_str(toml).expect("TOML should parse");
    assert!(config.mesh.enabled);
    assert!(config.mesh.lan.as_ref().unwrap().enabled);
    assert_eq!(config.mesh.iroh.len(), 2);
    assert_eq!(config.mesh.iroh[0].name.as_deref(), Some("team-a"));
    assert_eq!(config.mesh.iroh[1].name.as_deref(), Some("team-b"));
}

#[test]
fn test_mesh_iroh_dns_parses_and_round_trips() {
    assert!(MeshTomlConfig::default().iroh_dns.is_none());
    let config: MeshTomlConfig = toml::from_str(
        r#"
        enabled = true
        transport = "iroh"
        [iroh_dns]
        system_defaults = false
        fallback = "never"
        [[iroh_dns.nameservers]]
        addr = "127.0.0.1:5353"
        protocol = "https"
        tls_server_name = "dns.internal"
    "#,
    )
    .unwrap();
    let dns = config.iroh_dns.as_ref().unwrap();
    assert!(dns.validate().is_ok());
    assert_eq!(
        dns.nameservers[0].tls_server_name.as_deref(),
        Some("dns.internal")
    );
    let round_trip: MeshTomlConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(round_trip.iroh_dns, config.iroh_dns);
    assert!(toml::from_str::<MeshTomlConfig>("[iroh_dns]\nfallback = 'typo'").is_err());
    assert!(toml::from_str::<MeshTomlConfig>("[iroh_dns]\nunknown = true").is_err());
}

#[test]
fn test_mesh_iroh_gso_parses_on_and_off() {
    assert!(MeshTomlConfig::default().iroh_gso);
    for enabled in [false, true] {
        let config: MeshTomlConfig = toml::from_str(&format!("iroh_gso = {enabled}")).unwrap();
        assert_eq!(config.iroh_gso, enabled);
        let round_trip: MeshTomlConfig =
            toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(round_trip.iroh_gso, enabled);
    }
}

#[test]
fn test_mesh_config_defaults_no_new_fields() {
    let toml = r#"
[agent]
provider = "anthropic"
model = "claude-3-5-sonnet"

[mesh]
enabled = false
"#;
    let config: SingleAgentConfig = toml::from_str(toml).expect("TOML should parse");
    assert!(!config.mesh.enabled);
    assert!(config.mesh.iroh_gso);
    assert!(config.mesh.lan.is_none());
    assert!(config.mesh.iroh.is_empty());
}

// ── [agent.workspace] / [quorum.workspace] / delegate isolation ──────────

#[test]
fn workspace_settings_default_to_disabled_and_absent_section_is_unchanged() {
    let single: SingleAgentConfig = toml::from_str(
        r#"
[agent]
provider = "p"
model = "m"
"#,
    )
    .unwrap();
    assert_eq!(single.agent.workspace, WorkspaceSettings::default());
    assert!(!single.agent.workspace.enabled);
    assert_eq!(
        single.agent.workspace.baseline,
        crate::worktree::BaselineMode::Commit
    );
}

#[test]
fn workspace_settings_round_trip_and_reject_unknown_keys() {
    let parsed: WorkspaceSettings = toml::from_str(
        "enabled = true\nstorage_root = \"/data/wt\"\nbaseline = \"current_working_tree\"\n",
    )
    .unwrap();
    assert!(parsed.enabled);
    assert_eq!(parsed.storage_root.as_deref(), Some(Path::new("/data/wt")));
    assert_eq!(
        parsed.baseline,
        crate::worktree::BaselineMode::CurrentWorkingTree
    );
    let back: WorkspaceSettings = toml::from_str(&toml::to_string(&parsed).unwrap()).unwrap();
    assert_eq!(back, parsed);

    let err = toml::from_str::<WorkspaceSettings>("enabled = true\nmode = \"always\"\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown field"), "{err}");
}

#[test]
fn workspace_storage_root_resolves_against_config_dir() {
    let rel = WorkspaceSettings {
        storage_root: Some("wt".into()),
        ..WorkspaceSettings::enabled()
    }
    .anchored_at(Path::new("/etc/qmt"));
    assert_eq!(rel.storage_root.as_deref(), Some(Path::new("/etc/qmt/wt")));

    let abs = WorkspaceSettings {
        storage_root: Some("/abs/wt".into()),
        ..WorkspaceSettings::enabled()
    }
    .anchored_at(Path::new("/etc/qmt"));
    assert_eq!(abs.storage_root.as_deref(), Some(Path::new("/abs/wt")));

    // `~/` is left for tilde expansion, never anchored to the config dir.
    let tilde = WorkspaceSettings {
        storage_root: Some("~/wt".into()),
        ..WorkspaceSettings::enabled()
    }
    .anchored_at(Path::new("/etc/qmt"));
    assert_eq!(tilde.storage_root.as_deref(), Some(Path::new("~/wt")));
}

#[test]
fn workspace_schema_is_generated_for_both_config_roots() {
    for schema in [
        serde_json::to_value(schemars::schema_for!(SingleAgentConfig)).unwrap(),
        serde_json::to_value(schemars::schema_for!(QuorumConfig)).unwrap(),
    ] {
        let props = schema
            .pointer("/$defs/WorkspaceSettings/properties")
            .and_then(|v| v.as_object())
            .expect("WorkspaceSettings must be in the schema");
        for key in ["enabled", "storage_root", "baseline"] {
            assert!(props.contains_key(key), "missing {key}");
        }
    }
}

fn quorum_toml(extra_quorum: &str, delegate_isolation: &str) -> String {
    format!(
        r#"
[quorum]
cwd = "."
{extra_quorum}

[planner]
provider = "p"
model = "m"

[[delegates]]
id = "coder"
provider = "p"
model = "m"
{delegate_isolation}
"#
    )
}

#[tokio::test]
async fn delegate_isolation_defaults_to_inherit_and_isolated_requires_the_service() {
    let ok = load_config(ConfigSource::Toml(quorum_toml("", "")))
        .await
        .unwrap();
    let Config::Multi(ok) = ok else {
        panic!("quorum")
    };
    assert_eq!(
        ok.delegates[0].isolation,
        crate::worktree::DelegationIsolation::Inherit
    );

    let err = load_config(ConfigSource::Toml(quorum_toml(
        "",
        "isolation = \"isolated\"",
    )))
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("coder"), "{err:#}");

    let enabled = quorum_toml("workspace = { enabled = true }", "isolation = \"isolated\"");
    // `workspace` must be a table below [quorum]; the inline form above is valid TOML.
    load_config(ConfigSource::Toml(enabled)).await.unwrap();

    // `shared` never needs the service.
    load_config(ConfigSource::Toml(quorum_toml(
        "",
        "isolation = \"shared\"",
    )))
    .await
    .unwrap();
}

/// The shipped `output_limit_recovery` example loads, and its middleware entry
/// decodes into the middleware's own config with the documented defaults.
#[tokio::test]
async fn output_limit_recovery_example_loads_with_the_documented_defaults() {
    use crate::middleware::OutputLimitConfig;

    let config = load_config(ConfigSource::Toml(
        include_str!("../../examples/confs/output_limit_recovery.toml").into(),
    ))
    .await
    .unwrap();
    let Config::Single(config) = config else {
        panic!("single config");
    };
    let entry = config
        .middleware
        .iter()
        .find(|entry| entry.middleware_type == "output_limit_recovery")
        .expect("example declares the middleware");

    let cfg: OutputLimitConfig = serde_json::from_value(entry.config.clone()).unwrap();
    // The example spells out every default, so it must equal them.
    assert_eq!(cfg, OutputLimitConfig::default());
}

/// The shipped OAuth MCP example loads and declares an OAuth HTTP server that
/// passes the shared server validation.
#[tokio::test]
async fn oauth_mcp_example_loads_with_an_oauth_server() {
    let config = load_config(ConfigSource::Toml(
        include_str!("../../examples/confs/oauth_mcp.toml").into(),
    ))
    .await
    .unwrap();
    let Config::Single(config) = config else {
        panic!("single config");
    };
    let linear = config
        .mcp
        .iter()
        .find(|s| s.name() == "linear")
        .expect("example declares the linear server");
    assert!(matches!(
        linear,
        McpServerConfig::Http {
            auth: Some(McpAuth::OAuth { scopes, redirect_port: None, .. }),
            ..
        } if scopes == &["read".to_string()]
    ));
    crate::agent::mcp::validate_mcp_server(linear).unwrap();
}

/// 21.12: the shipped example/embedded profiles only gain commented-out
/// workspace blocks, so they still parse with the service disabled and every
/// delegate on the default `inherit` isolation.
#[tokio::test]
async fn example_profiles_keep_workspace_disabled_and_delegates_inheriting() {
    let single = include_str!("../../examples/confs/single_coder.toml");
    // `coder_agent.toml` has been invalid TOML since before this change (a
    // commented-out `[delegates.parameters]` header leaves duplicate keys), so
    // it can only be checked textually.
    let coder_agent = include_str!("../../examples/confs/coder_agent.toml");
    assert!(
        coder_agent
            .lines()
            .filter(|l| l.contains("workspace") || l.contains("isolation"))
            .all(|l| l.trim_start().starts_with('#')),
        "workspace/isolation blocks in coder_agent.toml must stay commented out"
    );
    let quorums = [include_str!("../../examples/confs/coder_delegate.toml")];
    // File-relative prompt references need a real path; only the sections
    // under test matter, so parse them as raw TOML tables first.
    let mut table: toml::Value = toml::from_str(single).unwrap();
    assert!(
        table["agent"].get("workspace").is_none(),
        "block stays commented out"
    );
    table.as_table_mut().unwrap().remove("profile");
    let single: SingleAgentConfig = table.try_into().unwrap();
    assert_eq!(single.agent.workspace, WorkspaceSettings::default());

    for raw in quorums {
        let table: toml::Value = toml::from_str(raw).unwrap();
        assert!(
            table["quorum"].get("workspace").is_none(),
            "block stays commented out"
        );
        let mut table = table;
        table.as_table_mut().unwrap().remove("profile");
        let quorum: QuorumConfig = table.try_into().unwrap();
        assert!(!quorum.quorum.workspace.enabled);
        assert!(
            quorum
                .delegates
                .iter()
                .all(|d| d.isolation == crate::worktree::DelegationIsolation::Inherit)
        );
    }
}

// ── MCP server settings (rmcp3 client features) ─────────────────────────

fn parse_mcp(toml: &str) -> Vec<McpServerConfig> {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        mcp: Vec<McpServerConfig>,
    }
    toml::from_str::<Wrapper>(toml)
        .expect("TOML should parse")
        .mcp
}

#[test]
fn mcp_config_without_new_keys_uses_defaults() {
    let servers = parse_mcp(
        r#"
[[mcp]]
name = "fs"
transport = "stdio"
command = "npx"

[[mcp]]
name = "remote"
transport = "http"
url = "https://example.test/mcp"
headers = { Authorization = "Bearer x" }
"#,
    );
    for server in &servers {
        assert!(!server.tasks_enabled());
        assert!(server.cache_enabled());
    }
    let McpServerConfig::Http {
        auth, socket_path, ..
    } = &servers[1]
    else {
        panic!("expected http");
    };
    assert!(auth.is_none() && socket_path.is_none());
}

#[test]
fn mcp_config_parses_every_new_key() {
    let servers = parse_mcp(
        r#"
[[mcp]]
name = "fs"
transport = "stdio"
command = "npx"
tasks = true
cache = false

[[mcp]]
name = "linear"
transport = "http"
url = "https://mcp.linear.app/mcp"
tasks = true
cache = false
auth = { type = "oauth", client_id = "abc", scopes = ["read", "write"], redirect_port = 5000 }

[[mcp]]
name = "sock"
transport = "http"
url = "http://localhost/mcp"
socket_path = "/run/mcp.sock"
"#,
    );
    assert!(servers[0].tasks_enabled() && !servers[0].cache_enabled());
    let McpServerConfig::Http {
        auth:
            Some(McpAuth::OAuth {
                client_id,
                scopes,
                redirect_port,
                ..
            }),
        tasks,
        cache,
        ..
    } = &servers[1]
    else {
        panic!("expected oauth http");
    };
    assert_eq!(client_id.as_deref(), Some("abc"));
    assert_eq!(scopes, &["read", "write"]);
    assert_eq!(*redirect_port, Some(5000));
    assert!(*tasks && !*cache);
    let McpServerConfig::Http { socket_path, .. } = &servers[2] else {
        panic!("expected http");
    };
    assert_eq!(
        socket_path.as_deref(),
        Some(std::path::Path::new("/run/mcp.sock"))
    );
}

#[test]
fn mcp_acp_round_trip_defaults_has_no_meta() {
    for config in [
        McpServerConfig::stdio("a", "cmd").with_args(["x"]),
        McpServerConfig::http("b", "https://e.test/mcp"),
    ] {
        let acp = config.to_acp();
        let meta = match &acp {
            McpServer::Stdio(s) => s.meta.as_ref(),
            McpServer::Http(s) => s.meta.as_ref(),
            _ => unreachable!(),
        };
        assert!(meta.is_none(), "defaults must not emit _meta");
        let back = McpServerConfig::from_acp(&acp).unwrap();
        assert_eq!(back.name(), config.name());
        assert!(!back.tasks_enabled() && back.cache_enabled());
    }
}

#[test]
fn mcp_acp_round_trip_preserves_new_fields() {
    let stdio = McpServerConfig::stdio("a", "cmd")
        .with_tasks(true)
        .with_cache(false);
    let back = McpServerConfig::from_acp(&stdio.to_acp()).unwrap();
    assert!(back.tasks_enabled() && !back.cache_enabled());

    let http = McpServerConfig::http("b", "https://e.test/mcp")
        .with_tasks(true)
        .with_cache(false)
        .with_socket_path("/run/x.sock");
    let back = McpServerConfig::from_acp(&http.to_acp()).unwrap();
    let McpServerConfig::Http {
        socket_path,
        tasks,
        cache,
        ..
    } = back
    else {
        panic!("expected http");
    };
    assert_eq!(
        socket_path.as_deref(),
        Some(std::path::Path::new("/run/x.sock"))
    );
    assert!(tasks && !cache);

    let oauth = McpServerConfig::http("c", "https://e.test/mcp").with_auth(McpAuth::OAuth {
        client_id: None,
        scopes: vec!["s".into()],
        redirect_port: Some(1234),
        strict_scopes: false,
    });
    let McpServerConfig::Http { auth, .. } = McpServerConfig::from_acp(&oauth.to_acp()).unwrap()
    else {
        panic!("expected http");
    };
    assert_eq!(
        auth,
        Some(McpAuth::OAuth {
            client_id: None,
            scopes: vec!["s".into()],
            redirect_port: Some(1234),
            strict_scopes: false,
        })
    );
}

#[test]
fn mcp_from_acp_tolerates_partial_and_rejects_malformed_meta() {
    let mut server = McpServerHttp::new("p", "https://e.test/mcp");
    server.meta = serde_json::from_value(serde_json::json!({"querymt": {"tasks": true}})).ok();
    let cfg = McpServerConfig::from_acp(&McpServer::Http(server.clone())).unwrap();
    assert!(cfg.tasks_enabled() && cfg.cache_enabled());

    server.meta = serde_json::from_value(serde_json::json!({"querymt": {"tasks": "yes"}})).ok();
    let err = McpServerConfig::from_acp(&McpServer::Http(server)).unwrap_err();
    assert!(err.to_string().contains("`p`"));
}

#[tokio::test]
async fn load_config_rejects_socket_path_with_oauth_naming_server() {
    let inline = r#"
[agent]
provider = "test"
model = "m"
tools = []

[[mcp]]
name = "bad-sock"
transport = "http"
url = "http://localhost/mcp"
socket_path = "/run/mcp.sock"
auth = { type = "oauth" }
"#;
    let err = load_config(ConfigSource::Toml(inline.to_string()))
        .await
        .expect_err("socket_path + oauth must be rejected");
    assert!(format!("{err:#}").contains("bad-sock"), "{err:#}");
}

/// The trading examples load from their real paths (prompt files resolve),
/// split read and order scopes between the two profiles, and the desk starts
/// disarmed in plan mode with its guard hooks wired.
#[tokio::test]
#[ignore = "trading example profiles are disabled in tests"]
async fn trading_examples_load_with_split_scopes_and_a_disarmed_desk() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/confs");
    let load = |name: &str| load_config(dir.join(name));
    let (Config::Single(analyst), Config::Single(desk)) = (
        load("trading_analyst.toml").await.unwrap(),
        load("trading_desk.toml").await.unwrap(),
    ) else {
        panic!("single configs");
    };

    let scopes = |config: &SingleAgentConfig| match &config.mcp[0] {
        McpServerConfig::Http {
            auth: Some(McpAuth::OAuth { scopes, .. }),
            ..
        } => scopes.clone(),
        other => panic!("expected an OAuth http server, got {other:?}"),
    };
    assert_eq!(scopes(&analyst), ["mcp.read"]);
    assert!(scopes(&desk).contains(&"mcp.orders.submit".to_string()));
    assert_ne!(analyst.mcp[0].name(), desk.mcp[0].name());

    // The analyst has no way to place orders or to edit code symbolically.
    let analyst_tools = &analyst.agent.tools;
    assert!(analyst_tools.contains(&"ibkr_read.*".parse().unwrap()));
    assert!(
        !analyst_tools
            .iter()
            .any(|t| t.server().is_some_and(|s| s.starts_with("ibkr_trade")))
    );
    assert!(!analyst_tools.contains(&"replace_symbol".parse().unwrap()));
    // The desk cannot reach around its guard.
    for forbidden in ["shell", "write_file", "edit", "browse", "web_fetch"] {
        assert!(
            !desk.agent.tools.contains(&forbidden.parse().unwrap()),
            "{forbidden}"
        );
    }

    assert_eq!(
        desk.agent.permissions.timeout_sec.map(|s| s.get()),
        Some(120)
    );
    assert_eq!(desk.agent.hooks.pre_tool_use.len(), 1);
    assert_eq!(desk.agent.hooks.pre_tool_use[0].matcher, None);
    assert_eq!(desk.agent.hooks.post_tool_use.len(), 1);
    assert!(desk.agent.hooks.base_dir.is_some());
    assert_eq!(desk.agent.mode, crate::agent::core::AgentMode::Plan);
}

/// First fenced `toml` block that follows `marker` in a docs page.
fn doc_toml_after(docs: &str, marker: &str) -> String {
    let start = docs
        .find(marker)
        .unwrap_or_else(|| panic!("missing {marker}"));
    docs[start..]
        .split("```toml")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("toml block")
        .to_string()
}

/// The `mode` snippet in `agent_modes.md` and the review profile in
/// `configuration.md` must load.
#[tokio::test]
async fn documented_mode_snippets_load() {
    let modes = include_str!("../../../../docs/docs/agent/agent_modes.md");
    let toml = doc_toml_after(modes, "Set the initial mode with the typed `mode` key");
    match load_config(ConfigSource::Toml(toml)).await.expect("loads") {
        Config::Single(single) => {
            assert_eq!(single.agent.mode, crate::agent::core::AgentMode::Plan)
        }
        _ => panic!("expected a single agent"),
    }

    let config = include_str!("../../../../docs/docs/agent/configuration.md");
    let start = config.find("mode = \"review\"").expect("review snippet");
    let block_start = config[..start].rfind("```toml").expect("fence") + "```toml".len();
    let block = config[block_start..].split("```").next().unwrap();
    match load_config(ConfigSource::Toml(block.to_string()))
        .await
        .expect("review profile loads")
    {
        Config::Single(single) => {
            assert_eq!(single.agent.mode, crate::agent::core::AgentMode::Review)
        }
        _ => panic!("expected a single agent"),
    }
}

#[tokio::test]
async fn malformed_tool_selector_fails_loading_naming_the_field() {
    for (section, toml) in [
        (
            "tools",
            "[agent]\nprovider = \"t\"\nmodel = \"m\"\ntools = [\"github.\"]\n",
        ),
        (
            "tools",
            "[quorum]\n[planner]\nprovider=\"t\"\nmodel=\"m\"\ntools = [\"a.*.b\"]\n",
        ),
        (
            "tools",
            "[quorum]\n[planner]\nprovider=\"t\"\nmodel=\"m\"\n[[delegates]]\nid=\"d\"\nprovider=\"t\"\nmodel=\"m\"\ntools = [\"\"]\n",
        ),
    ] {
        let err = load_config(ConfigSource::Toml(toml.to_string()))
            .await
            .expect_err("malformed selector must fail");
        let text = format!("{err:#}");
        assert!(text.contains(section), "{text}");
    }
}

const MCP_IBKR: &str =
    "[[mcp]]\ntransport = \"stdio\"\nname = \"ibkr_trade\"\ncommand = \"true\"\n";

#[tokio::test]
async fn permissions_naming_an_unconfigured_server_fail_loading() {
    let toml = format!(
        "[agent]\nprovider = \"t\"\nmodel = \"m\"\n[agent.permissions]\nalways_ask = [\"ibkr-trade.place_order\"]\n{MCP_IBKR}"
    );
    let err = load_config(ConfigSource::Toml(toml)).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("ibkr-trade") && text.contains("not configured"),
        "{text}"
    );

    let ok = format!(
        "[agent]\nprovider = \"t\"\nmodel = \"m\"\n[agent.permissions]\nalways_ask = [\"ibkr_trade.place_order\"]\n{MCP_IBKR}"
    );
    load_config(ConfigSource::Toml(ok)).await.unwrap();
}

#[tokio::test]
async fn permissions_outside_a_tools_selection_fail_naming_tools() {
    let toml = "[agent]\nprovider = \"t\"\nmodel = \"m\"\ntools = [\"read_tool\"]\n[agent.permissions]\nask = [\"shell\"]\n";
    let err = load_config(ConfigSource::Toml(toml.into()))
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("shell") && text.contains("`tools`"), "{text}");
}

#[tokio::test]
async fn planner_and_delegate_permissions_are_validated_with_their_servers() {
    let planner = "[quorum]\n[planner]\nprovider=\"t\"\nmodel=\"m\"\n[planner.permissions]\nask = [\"nope.x\"]\n";
    let err = load_config(ConfigSource::Toml(planner.into()))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("planner"));

    // A delegate-level server is visible to that delegate only.
    let delegate = |perm_server: &str| {
        format!(
            "[quorum]\n[planner]\nprovider=\"t\"\nmodel=\"m\"\n[[delegates]]\nid=\"d\"\nprovider=\"t\"\nmodel=\"m\"\n[delegates.permissions]\nask = [\"{perm_server}.x\"]\n[[delegates.mcp]]\ntransport = \"stdio\"\nname = \"own\"\ncommand = \"true\"\n"
        )
    };
    load_config(ConfigSource::Toml(delegate("own")))
        .await
        .unwrap();
    let err = load_config(ConfigSource::Toml(delegate("other")))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("delegate 'd'"));
}

/// The approval example in `configuration.md` must load.
#[tokio::test]
async fn documented_approval_snippet_loads() {
    let docs = include_str!("../../../../docs/docs/agent/configuration.md");
    let table = doc_toml_after(docs, "### Tool approval");
    let toml = format!(
        "[agent]\nprovider = \"t\"\nmodel = \"m\"\n{table}\n[[mcp]]\ntransport = \"stdio\"\nname = \"ibkr\"\ncommand = \"true\"\n"
    );
    let Config::Single(single) = load_config(ConfigSource::Toml(toml)).await.expect("loads") else {
        panic!("expected a single agent");
    };
    assert_eq!(single.agent.permissions.ask, vec!["shell".parse().unwrap()]);
    assert_eq!(
        single.agent.permissions.timeout_sec.map(|s| s.get()),
        Some(120)
    );
}

// ── [agent.repository_knowledge] / [quorum.repository_knowledge] ─────────

#[test]
fn repository_knowledge_defaults_to_disabled_and_absent_section_is_unchanged() {
    let single: SingleAgentConfig = toml::from_str(
        r#"
[agent]
provider = "p"
model = "m"
"#,
    )
    .unwrap();
    let rk = &single.agent.repository_knowledge;
    assert_eq!(rk, &RepositoryKnowledgeSettings::default());
    assert!(!rk.enabled);
    assert!(rk.root.is_none());
    assert!(!rk.force_hash);
    assert!(!rk.allow_ignored);
    assert!(!rk.delivery.orientation);
    assert!(!rk.delivery.index_context);
    assert!(rk.if_enabled().is_none());

    let quorum: QuorumConfig =
        toml::from_str("[quorum]\n[planner]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
    assert_eq!(
        quorum.quorum.repository_knowledge,
        RepositoryKnowledgeSettings::default()
    );
}

#[test]
fn repository_knowledge_settings_round_trip_and_reject_unknown_keys() {
    let parsed: RepositoryKnowledgeSettings = toml::from_str(
        r#"
enabled = true
root = "/repo"
max_files = 10
max_file_bytes = 1024
max_total_bytes = 4096
max_response_bytes = 2048
default_result_limit = 5
force_hash = true
allow_ignored = true

[delivery]
orientation = true
index_context = false
max_cards = 1
max_tokens = 100
max_bytes = 512
"#,
    )
    .unwrap();
    assert!(parsed.enabled && parsed.force_hash && parsed.allow_ignored);
    assert_eq!(parsed.root.as_deref(), Some(Path::new("/repo")));
    assert_eq!(parsed.max_files, 10);
    assert_eq!(parsed.max_file_bytes, 1024);
    assert_eq!(parsed.max_total_bytes, 4096);
    assert!(parsed.delivery.orientation && !parsed.delivery.index_context);
    let back: RepositoryKnowledgeSettings =
        toml::from_str(&toml::to_string(&parsed).unwrap()).unwrap();
    assert_eq!(back, parsed);

    for bad in [
        "enabled = true\nmode = \"always\"\n",
        "[delivery]\nauto = true\n",
    ] {
        let err = toml::from_str::<RepositoryKnowledgeSettings>(bad)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }
}

#[test]
fn repository_knowledge_limits_are_capped_at_implementation_maxima() {
    let s = RepositoryKnowledgeSettings {
        max_response_bytes: usize::MAX,
        default_result_limit: 10_000,
        delivery: RepositoryKnowledgeDeliverySettings {
            max_cards: 99,
            max_tokens: 1_000_000,
            max_bytes: usize::MAX,
            ..Default::default()
        },
        ..RepositoryKnowledgeSettings::enabled()
    };
    assert_eq!(s.effective_max_response_bytes(), 64 * 1024);
    assert_eq!(s.effective_default_result_limit(), 50);
    assert_eq!(s.delivery.effective_max_cards(), 4);
    assert_eq!(s.delivery.effective_max_tokens(), 2_000);
    assert_eq!(s.delivery.effective_max_bytes(), 16 * 1024);

    let zero = RepositoryKnowledgeSettings {
        default_result_limit: 0,
        ..RepositoryKnowledgeSettings::enabled()
    };
    assert_eq!(zero.effective_default_result_limit(), 1);
}

#[test]
fn repository_knowledge_delivery_flags_never_enable_the_feature() {
    let s = RepositoryKnowledgeSettings {
        enabled: false,
        delivery: RepositoryKnowledgeDeliverySettings {
            orientation: true,
            index_context: true,
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(!s.orientation_active());
    assert!(!s.index_context_active());

    let only_orientation = RepositoryKnowledgeSettings {
        delivery: RepositoryKnowledgeDeliverySettings {
            orientation: true,
            ..Default::default()
        },
        ..RepositoryKnowledgeSettings::enabled()
    };
    assert!(only_orientation.orientation_active());
    assert!(!only_orientation.index_context_active());
}

#[test]
fn repository_knowledge_root_resolves_against_config_dir() {
    let rel = RepositoryKnowledgeSettings {
        root: Some("repo".into()),
        ..RepositoryKnowledgeSettings::enabled()
    }
    .anchored_at(Path::new("/etc/qmt"));
    assert_eq!(rel.root.as_deref(), Some(Path::new("/etc/qmt/repo")));

    let abs = RepositoryKnowledgeSettings {
        root: Some("/abs/repo".into()),
        allowed_roots: vec!["work".into(), "/srv/repos".into()],
        ..RepositoryKnowledgeSettings::enabled()
    }
    .anchored_at(Path::new("/etc/qmt"));
    assert_eq!(abs.root.as_deref(), Some(Path::new("/abs/repo")));
    assert_eq!(
        abs.allowed_roots,
        vec![PathBuf::from("/etc/qmt/work"), PathBuf::from("/srv/repos")]
    );
}

#[test]
fn repository_knowledge_schema_is_generated_for_both_config_roots() {
    for schema in [
        serde_json::to_value(schemars::schema_for!(SingleAgentConfig)).unwrap(),
        serde_json::to_value(schemars::schema_for!(QuorumConfig)).unwrap(),
    ] {
        let props = schema
            .pointer("/$defs/RepositoryKnowledgeSettings/properties")
            .and_then(|v| v.as_object())
            .expect("RepositoryKnowledgeSettings must be in the schema");
        for key in ["enabled", "root", "max_files", "force_hash", "delivery"] {
            assert!(props.contains_key(key), "missing {key}");
        }
    }
}

#[test]
fn repository_knowledge_builders_carry_settings_only_when_enabled() {
    use crate::api::{Agent, QuorumBuilder};

    // Disabled single-agent config: builder holds no settings at all.
    let disabled: SingleAgentConfig =
        toml::from_str("[agent]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
    let b = Agent::builder_from_config(disabled, None).unwrap();
    assert!(b.configured_repository_knowledge().is_none());

    // Enabled: settings round-trip into the builder.
    let enabled: SingleAgentConfig = toml::from_str(
        "[agent]\nprovider = \"p\"\nmodel = \"m\"\n[agent.repository_knowledge]\nenabled = true\nforce_hash = true\n",
    )
    .unwrap();
    let b = Agent::builder_from_config(enabled, None).unwrap();
    let got = b
        .configured_repository_knowledge()
        .expect("enabled settings");
    assert!(got.enabled && got.force_hash);

    // Programmatic builders.
    let b = crate::api::AgentBuilder::new()
        .repository_knowledge(RepositoryKnowledgeSettings::enabled());
    assert!(b.configured_repository_knowledge().unwrap().enabled);
    let q = QuorumBuilder::new().repository_knowledge(RepositoryKnowledgeSettings::enabled());
    assert!(q.configured_repository_knowledge().unwrap().enabled);
    assert!(
        QuorumBuilder::new()
            .configured_repository_knowledge()
            .is_none()
    );
}

#[tokio::test]
async fn config_mcp_servers_include_quorum_delegate_servers() {
    let toml = r#"
[quorum]
cwd = "/tmp"

[[mcp]]
name = "shared"
transport = "stdio"
command = "true"

[planner]
provider = "openai"
model = "gpt-4"

[[delegates]]
id = "coder"
provider = "openai"
model = "gpt-4"

[[delegates.mcp]]
name = "private"
transport = "http"
url = "https://private.example/mcp"
auth = { type = "oauth", scopes = ["read"] }
"#;
    let config = load_config(ConfigSource::Toml(toml.to_string()))
        .await
        .unwrap();
    let names: Vec<_> = config.mcp_servers().map(|s| s.name()).collect();
    assert_eq!(names, ["shared", "private"]);
}

// ==================== Snapshot retention / removed keys ====================

fn parse_snapshot(body: &str) -> Result<SnapshotBackendConfig, toml::de::Error> {
    toml::from_str::<SnapshotBackendConfig>(body)
}

#[test]
fn removed_max_file_bytes_key_is_rejected() {
    // Single agent: [agent.execution.snapshot]
    let err = toml::from_str::<AgentSettings>(
        "provider = \"p\"\nmodel = \"m\"\n[execution.snapshot]\nbackend = \"git\"\nmax_file_bytes = 1024\n",
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("unknown field") && err.contains("max_file_bytes"),
        "{err}"
    );

    // Quorum delegate: execution.snapshot
    let err = toml::from_str::<crate::config::QuorumConfig>(
        "[quorum]\ncwd = \"/tmp\"\n[planner]\nprovider = \"p\"\nmodel = \"m\"\n\
         [[delegates]]\nid = \"d\"\nprovider = \"p\"\nmodel = \"m\"\n\
         [delegates.execution.snapshot]\nbackend = \"git\"\nmax_file_bytes = 1024\n",
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("unknown field") && err.contains("max_file_bytes"),
        "{err}"
    );
}

#[test]
fn retention_keys_default_to_unset() {
    let cfg = parse_snapshot("backend = \"git\"").unwrap();
    assert_eq!(cfg.max_snapshots, None);
    assert_eq!(cfg.max_age_days, None);
    assert_eq!(SnapshotBackendConfig::default().max_snapshots, None);
    assert_eq!(SnapshotBackendConfig::default().max_age_days, None);
}

#[test]
fn retention_values_are_accepted() {
    let cfg = parse_snapshot("backend = \"git\"\nmax_snapshots = 5\nmax_age_days = 2").unwrap();
    assert_eq!(cfg.max_snapshots.map(|n| n.get()), Some(5));
    assert_eq!(cfg.max_age_days.map(|n| n.get()), Some(2));
}

#[test]
fn zero_retention_values_are_rejected() {
    for key in ["max_snapshots", "max_age_days"] {
        let err = parse_snapshot(&format!("backend = \"git\"\n{key} = 0"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(key), "{key}: {err}");
    }
    let err = parse_snapshot("max_snapshots = -1")
        .unwrap_err()
        .to_string();
    assert!(err.contains("max_snapshots"), "{err}");
}

#[test]
fn json_null_leaves_a_retention_key_unset() {
    let cfg: SnapshotBackendConfig =
        serde_json::from_str(r#"{"backend":"git","max_age_days":null,"max_snapshots":null}"#)
            .unwrap();
    assert_eq!(cfg.max_age_days, None);
    assert_eq!(cfg.max_snapshots, None);
}

#[test]
fn schema_retention_keys_have_minimum_one() {
    let schema = schemars::schema_for!(SingleAgentConfig);
    let json = serde_json::to_value(&schema).unwrap();
    for key in ["max_snapshots", "max_age_days"] {
        let minimum = json
            .pointer(&format!(
                "/$defs/SnapshotBackendConfig/properties/{key}/minimum"
            ))
            .and_then(|v| v.as_u64());
        assert_eq!(minimum, Some(1), "{key} schema: {json}");
    }
}

// ==================== Repository knowledge root anchoring ====================

#[tokio::test]
async fn repository_knowledge_root_is_anchored_to_config_file_directory() {
    let dir = tempfile::tempdir().unwrap();
    let single_path = dir.path().join("single.toml");
    std::fs::write(
        &single_path,
        "[agent]\nprovider = \"p\"\nmodel = \"m\"\n[agent.repository_knowledge]\nenabled = true\nroot = \"repo\"\n",
    )
    .unwrap();
    let Config::Single(single) = load_config(ConfigSource::Path(single_path)).await.unwrap() else {
        panic!("expected single config");
    };
    assert_eq!(
        single.agent.repository_knowledge.root.as_deref(),
        Some(dir.path().join("repo").as_path())
    );

    let quorum_path = dir.path().join("quorum.toml");
    std::fs::write(
        &quorum_path,
        "[quorum]\n[quorum.repository_knowledge]\nenabled = true\nroot = \"repo\"\n[planner]\nprovider = \"p\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let Config::Multi(quorum) = load_config(ConfigSource::Path(quorum_path)).await.unwrap() else {
        panic!("expected quorum config");
    };
    assert_eq!(
        quorum.quorum.repository_knowledge.root.as_deref(),
        Some(dir.path().join("repo").as_path())
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
        assert!(tools.iter().any(|t| t == name), "{name} listed");
    }
    // Legacy session-memory tools are not pulled in.
    assert!(!tools.iter().any(|t| t.starts_with("knowledge_")));

    // Rollback: disabling the feature turns off reads and both delivery stages
    // even though the delivery flags remain set.
    let mut off = (*config).clone();
    off.agent.repository_knowledge.enabled = false;
    let rk = &off.agent.repository_knowledge;
    assert!(rk.if_enabled().is_none());
    assert!(!rk.orientation_active() && !rk.index_context_active());
    let builder = crate::api::Agent::builder_from_config(off, None).unwrap();
    assert!(builder.configured_repository_knowledge().is_none());
}
