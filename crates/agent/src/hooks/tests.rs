use super::engine::{Hooks, PostToolUseRequest, PreToolUseRequest, StopRequest};
use super::schema;
use crate::hooks::config::{HookHandlerConfig, HooksConfig, MatcherGroupConfig};
use crate::hooks::schema::{
    PermissionRequestCommandOutputWire, PreCompactionCommandOutputWire,
    PreDelegationCommandOutputWire, PreToolUseCommandInput, StopCommandOutputWire,
};
use crate::hooks::{HookCommandConfig, HookEventConfig};
use schemars::JsonSchema;
use serde_json::json;
use std::collections::BTreeMap;
use tempfile::TempDir;

#[test]
fn generated_hook_schemas_match_expected_fixtures() {
    let dir = TempDir::new().expect("tempdir");
    schema::write_schema_fixtures(dir.path()).expect("write schemas");

    let generated_dir = dir.path().join("generated");
    let expected_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/hooks/schema/generated");

    for file_name in [
        schema::PRE_TOOL_USE_INPUT_FIXTURE,
        schema::PRE_TOOL_USE_OUTPUT_FIXTURE,
        schema::PERMISSION_REQUEST_INPUT_FIXTURE,
        schema::PERMISSION_REQUEST_OUTPUT_FIXTURE,
        schema::POST_TOOL_USE_INPUT_FIXTURE,
        schema::POST_TOOL_USE_OUTPUT_FIXTURE,
        schema::USER_PROMPT_SUBMIT_INPUT_FIXTURE,
        schema::USER_PROMPT_SUBMIT_OUTPUT_FIXTURE,
        schema::SESSION_START_INPUT_FIXTURE,
        schema::SESSION_START_OUTPUT_FIXTURE,
        schema::STOP_INPUT_FIXTURE,
        schema::STOP_OUTPUT_FIXTURE,
        schema::PRE_COMPACTION_INPUT_FIXTURE,
        schema::PRE_COMPACTION_OUTPUT_FIXTURE,
        schema::POST_COMPACTION_INPUT_FIXTURE,
        schema::POST_COMPACTION_OUTPUT_FIXTURE,
        schema::PRE_DELEGATION_INPUT_FIXTURE,
        schema::PRE_DELEGATION_OUTPUT_FIXTURE,
        schema::DELEGATION_START_INPUT_FIXTURE,
        schema::DELEGATION_START_OUTPUT_FIXTURE,
        schema::POST_DELEGATION_INPUT_FIXTURE,
        schema::POST_DELEGATION_OUTPUT_FIXTURE,
        schema::DELEGATION_FAILURE_INPUT_FIXTURE,
        schema::DELEGATION_FAILURE_OUTPUT_FIXTURE,
    ] {
        let generated = std::fs::read(generated_dir.join(file_name)).expect("generated fixture");
        let expected = std::fs::read(expected_dir.join(file_name)).expect("expected fixture");
        assert_eq!(
            generated, expected,
            "schema fixture mismatch for {file_name}"
        );
    }
}

#[test]
#[ignore = "developer helper for regenerating committed schema fixtures"]
fn regenerate_committed_hook_schemas() {
    let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/hooks/schema");
    schema::write_schema_fixtures(&schema_dir).expect("rewrite committed schemas");
}

#[test]
fn schema_examples_validate_against_generated_json_schemas() {
    let pre_tool_input = serde_json::to_value(PreToolUseCommandInput {
        session_id: "s1".to_string(),
        turn_id: "t1".to_string(),
        transcript_path: crate::hooks::schema::NullableString::from_string(None),
        cwd: "/repo".to_string(),
        hook_event_name: "pre_tool_use".to_string(),
        model: "mock".to_string(),
        permission_mode: "plan".to_string(),
        tool_name: "shell".to_string(),
        tool_input: json!({"command": "echo hi"}),
        tool_use_id: "tool1".to_string(),
    })
    .expect("serialize pre tool input");
    assert_matches_schema::<PreToolUseCommandInput>(&pre_tool_input);

    let permission_request_output = json!({
        "continue": true,
        "hook_specific_output": {
            "hook_event_name": "permission_request",
            "decision": {"behavior": "allow"}
        }
    });
    assert_matches_schema::<PermissionRequestCommandOutputWire>(&permission_request_output);

    let stop_output = json!({
        "continue": false,
        "reason": "need one more pass",
        "hook_specific_output": {
            "hook_event_name": "stop",
            "additional_context": "summarize validation"
        }
    });
    assert_matches_schema::<StopCommandOutputWire>(&stop_output);

    let pre_compaction_output = json!({
        "continue": true,
        "decision": "block",
        "reason": "preserve raw context",
        "hook_specific_output": {
            "hook_event_name": "pre_compaction",
            "additional_context": "wait for explicit confirmation"
        }
    });
    assert_matches_schema::<PreCompactionCommandOutputWire>(&pre_compaction_output);

    let pre_delegation_output = json!({
        "continue": true,
        "hook_specific_output": {
            "hook_event_name": "pre_delegation",
            "updated_delegation": {
                "target_agent_id": "coder",
                "objective": "narrow task"
            }
        }
    });
    assert_matches_schema::<PreDelegationCommandOutputWire>(&pre_delegation_output);

    let invalid = json!({
        "continue": true,
        "hook_specific_output": {
            "hook_event_name": "permission_request",
            "decision": {"behavior": "ask"}
        }
    });
    assert_schema_rejects::<PermissionRequestCommandOutputWire>(&invalid);
}

#[test]
fn hooks_config_rejects_empty_groups() {
    let config = HooksConfig {
        enabled: true,
        pre_tool_use: vec![MatcherGroupConfig {
            matcher: Some("^shell$".to_string()),
            hooks: Vec::new(),
        }],
        ..HooksConfig::default()
    };

    let err = Hooks::new(config).expect_err("empty groups should fail");
    assert!(err.to_string().contains("must contain at least one hook"));
}

#[test]
fn hooks_config_accepts_command_hooks() {
    let config = HooksConfig {
        enabled: true,
        session_start: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![HookHandlerConfig::Command(HookCommandConfig {
                command: "echo ok".to_string(),
                timeout_sec: Some(5),
                status_message: Some("check".to_string()),
                env: BTreeMap::new(),
            })],
        }],
        ..HooksConfig::default()
    };

    Hooks::new(config).expect("valid command hooks");
}

#[test]
fn hook_event_labels_are_stable() {
    assert_eq!(HookEventConfig::SessionStart.label(), "session_start");
    assert_eq!(
        HookEventConfig::UserPromptSubmit.label(),
        "user_prompt_submit"
    );
    assert_eq!(HookEventConfig::PreToolUse.label(), "pre_tool_use");
    assert_eq!(
        HookEventConfig::PermissionRequest.label(),
        "permission_request"
    );
    assert_eq!(HookEventConfig::PostToolUse.label(), "post_tool_use");
    assert_eq!(HookEventConfig::Stop.label(), "stop");
    assert_eq!(HookEventConfig::PreCompaction.label(), "pre_compaction");
    assert_eq!(HookEventConfig::PostCompaction.label(), "post_compaction");
    assert_eq!(HookEventConfig::PreDelegation.label(), "pre_delegation");
    assert_eq!(HookEventConfig::DelegationStart.label(), "delegation_start");
    assert_eq!(HookEventConfig::PostDelegation.label(), "post_delegation");
    assert_eq!(
        HookEventConfig::DelegationFailure.label(),
        "delegation_failure"
    );
}

#[tokio::test]
async fn pre_tool_use_hook_script_can_block() {
    let dir = TempDir::new().expect("tempdir");
    let script = dir.path().join("pre_tool.py");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '{"continue":true,"decision":"block","reason":"blocked from script"}'
"#,
    )
    .expect("write script");

    let config = HooksConfig {
        enabled: true,
        pre_tool_use: vec![MatcherGroupConfig {
            matcher: Some("^shell$".to_string()),
            hooks: vec![HookHandlerConfig::Command(HookCommandConfig {
                command: format!("sh {}", script.display()),
                timeout_sec: Some(5),
                status_message: None,
                env: BTreeMap::new(),
            })],
        }],
        ..HooksConfig::default()
    };

    let result = Hooks::new(config)
        .expect("hooks")
        .run_pre_tool_use(PreToolUseRequest {
            session_id: "s1".to_string(),
            turn_id: "t1".to_string(),
            cwd: Some(dir.path().to_path_buf()),
            model: "mock".to_string(),
            permission_mode: "plan".to_string(),
            tool_name: "shell".to_string(),
            tool_input: json!({"command": "rm -rf /tmp/nope"}),
            tool_use_id: "tool1".to_string(),
        })
        .await
        .expect("hook execution");

    assert!(result.should_block);
    assert_eq!(result.block_reason.as_deref(), Some("blocked from script"));
}

#[tokio::test]
async fn invalid_hook_json_is_ignored_fail_open() {
    let dir = TempDir::new().expect("tempdir");
    let script = dir.path().join("invalid_hook.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf 'this is not json'
"#,
    )
    .expect("write script");

    let config = HooksConfig {
        enabled: true,
        post_tool_use: vec![MatcherGroupConfig {
            matcher: Some("^shell$".to_string()),
            hooks: vec![HookHandlerConfig::Command(HookCommandConfig {
                command: format!("sh {}", script.display()),
                timeout_sec: Some(5),
                status_message: None,
                env: BTreeMap::new(),
            })],
        }],
        ..HooksConfig::default()
    };

    let result = Hooks::new(config)
        .expect("hooks")
        .run_post_tool_use(PostToolUseRequest {
            session_id: "s1".to_string(),
            turn_id: "t1".to_string(),
            cwd: Some(dir.path().to_path_buf()),
            model: "mock".to_string(),
            permission_mode: "default".to_string(),
            tool_name: "shell".to_string(),
            tool_input: json!({"command": "echo hi"}),
            tool_response: json!("ok"),
            tool_use_id: "tool1".to_string(),
        })
        .await
        .expect("hook execution");

    assert!(!result.should_block);
    assert!(result.block_reason.is_none());
    assert_eq!(result.notices.len(), 1);
    assert_eq!(result.notices[0].event_name, "post_tool_use");
    assert!(result.notices[0].is_error);
    assert!(
        result.notices[0]
            .message
            .contains("Ignoring invalid post_tool_use hook output")
    );
}

#[tokio::test]
async fn stop_hook_script_can_request_continuation() {
    let dir = TempDir::new().expect("tempdir");
    let script = dir.path().join("stop_hook.py");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '{"continue":false,"reason":"continue please","hook_specific_output":{"hook_event_name":"stop","additional_context":"one more pass"}}'
"#,
    )
    .expect("write script");

    let config = HooksConfig {
        enabled: true,
        stop: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![HookHandlerConfig::Command(HookCommandConfig {
                command: format!("sh {}", script.display()),
                timeout_sec: Some(5),
                status_message: None,
                env: BTreeMap::new(),
            })],
        }],
        ..HooksConfig::default()
    };

    let result = Hooks::new(config)
        .expect("hooks")
        .run_stop(StopRequest {
            session_id: "s1".to_string(),
            turn_id: "t1".to_string(),
            cwd: Some(dir.path().to_path_buf()),
            model: "mock".to_string(),
            permission_mode: "accept_edits".to_string(),
            stop_reason: "EndTurn".to_string(),
        })
        .await
        .expect("hook execution");

    assert!(result.should_continue);
    assert_eq!(result.stop_reason.as_deref(), Some("continue please"));
    assert_eq!(
        result.additional_contexts,
        vec!["one more pass".to_string()]
    );
}

fn assert_matches_schema<T>(value: &serde_json::Value)
where
    T: JsonSchema,
{
    let schema = schema_value::<T>();
    let validator = jsonschema::validator_for(&schema).expect("compile schema");
    if let Err(err) = validator.validate(value) {
        panic!("value did not match schema: {err}\nvalue={value}\nschema={schema}");
    }
}

fn assert_schema_rejects<T>(value: &serde_json::Value)
where
    T: JsonSchema,
{
    let schema = schema_value::<T>();
    let validator = jsonschema::validator_for(&schema).expect("compile schema");
    assert!(
        validator.validate(value).is_err(),
        "value unexpectedly matched schema: {value}"
    );
}

fn schema_value<T>() -> serde_json::Value
where
    T: JsonSchema,
{
    serde_json::from_slice(&schema::schema_json::<T>().expect("schema json")).expect("schema value")
}
