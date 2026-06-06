use super::*;

use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn test_parse_tool_spec() {
    assert_eq!(parse_tool_spec("edit"), ToolSpec::Builtin("edit".into()));
    assert_eq!(
        parse_tool_spec("github.*"),
        ToolSpec::McpAll("github".into())
    );
    assert_eq!(
        parse_tool_spec("github.search_repos"),
        ToolSpec::McpSpecific("github".into(), "search_repos".into())
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
    assert!(agent.assume_mutating);
    assert!(agent.mutating_tools.is_empty());
}

#[test]
fn test_mutating_tool_settings_parse() {
    let agent = parse_agent(
        "assume_mutating = false\nmutating_tools = [\"edit\", \"write_file\", \"shell\"]",
    );
    assert!(!agent.assume_mutating);
    assert_eq!(agent.mutating_tools, vec!["edit", "write_file", "shell"]);
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
        "[profile]\nid = \"file-profile\"\n\n[agent]\nprovider = \"test\"\nmodel = \"test-model\"\ntools = []\nsystem = [{{ file = \"{}\" }}]\n",
        prompt_path.display()
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
        "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\ntools = []\nsystem = [{{ file = \"{}\" }}]\n",
        prompt_path.display()
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
    let msg = validation.unwrap_err().to_string();
    assert!(
        msg.contains("unsupported hook event"),
        "unexpected error: {msg}"
    );
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
    assert!(config.mesh.lan.is_none());
    assert!(config.mesh.iroh.is_empty());
}
