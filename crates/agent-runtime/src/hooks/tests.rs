use super::engine::{
    ContextHookRequest, Hooks, PostToolUseRequest, PreCompactionRequest, PreToolUseRequest,
    StopRequest,
};
use super::schema;
use crate::hooks::config::{HookHandlerConfig, HooksConfig, MatcherGroupConfig};
use crate::hooks::schema::{
    PermissionRequestCommandOutputWire, PreCompactionCommandOutputWire,
    PreDelegationCommandOutputWire, PreToolUseCommandInput, StopCommandOutputWire,
};
use crate::hooks::{HookCommandConfig, HookEventConfig, HookMcpToolConfig};
use schemars::JsonSchema;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::TempDir;

#[tokio::test]
async fn delegation_hooks_disclose_parent_and_execute_in_allocated_child() {
    use super::engine::{DelegationStartRequest, PostDelegationRequest, PreDelegationRequest};
    let parent = TempDir::new().unwrap();
    let child = TempDir::new().unwrap();
    let group = MatcherGroupConfig {
        matcher: None,
        hooks: vec![command_hook(
            "cat > hook-input.json; pwd > hook-cwd; printf '{}'".into(),
        )],
    };
    let hooks = Hooks::new(HooksConfig {
        enabled: true,
        pre_delegation: vec![group.clone()],
        delegation_start: vec![group.clone()],
        post_delegation: vec![group],
        ..HooksConfig::default()
    })
    .unwrap();
    hooks
        .run_pre_delegation(PreDelegationRequest {
            session_id: "parent".into(),
            turn_id: "turn".into(),
            cwd: Some(parent.path().into()),
            model: "mock".into(),
            permission_mode: "default".into(),
            tool_use_id: "tool".into(),
            target_agent_id: "delegate".into(),
            objective: "edit".into(),
            context: None,
            constraints: None,
            expected_output: None,
            isolation: crate::worktree::DelegationIsolation::Isolated,
        })
        .await
        .unwrap();
    let pre: serde_json::Value =
        serde_json::from_slice(&std::fs::read(parent.path().join("hook-input.json")).unwrap())
            .unwrap();
    assert_eq!(pre["parent_session_id"], "parent");
    assert_eq!(pre["parent_cwd"], pre["cwd"]);
    assert!(!child.path().join("hook-input.json").exists());
    hooks
        .run_delegation_start(DelegationStartRequest {
            session_id: "parent".into(),
            parent_cwd: Some(parent.path().into()),
            turn_id: "turn".into(),
            cwd: Some(child.path().into()),
            model: "mock".into(),
            permission_mode: "default".into(),
            delegation_id: "delegation".into(),
            target_agent_id: "delegate".into(),
            objective: "edit".into(),
            context: None,
            constraints: None,
            expected_output: None,
            status: "requested".into(),
        })
        .await
        .unwrap();
    let start: serde_json::Value =
        serde_json::from_slice(&std::fs::read(child.path().join("hook-input.json")).unwrap())
            .unwrap();
    assert_eq!(start["parent_cwd"], parent.path().display().to_string());
    assert_eq!(start["cwd"], child.path().display().to_string());
    hooks
        .run_post_delegation(PostDelegationRequest {
            session_id: "parent".into(),
            parent_cwd: Some(parent.path().into()),
            turn_id: "turn".into(),
            cwd: Some(child.path().into()),
            model: "mock".into(),
            permission_mode: "default".into(),
            delegation_id: "delegation".into(),
            target_agent_id: "delegate".into(),
            child_session_id: "child".into(),
            objective: "edit".into(),
            status: "complete".into(),
            summary: "done".into(),
            verification_passed: true,
        })
        .await
        .unwrap();
    let post: serde_json::Value =
        serde_json::from_slice(&std::fs::read(child.path().join("hook-input.json")).unwrap())
            .unwrap();
    assert_eq!(post["child_session_id"], "child");
    assert_eq!(post["parent_session_id"], "parent");
    assert_eq!(post["parent_cwd"], parent.path().display().to_string());
    assert_eq!(post["cwd"], child.path().display().to_string());
    assert_eq!(
        std::fs::read_to_string(child.path().join("hook-cwd"))
            .unwrap()
            .trim(),
        child.path().canonicalize().unwrap().display().to_string()
    );
}

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
        schema::CONTEXT_INPUT_FIXTURE,
        schema::CONTEXT_OUTPUT_FIXTURE,
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
        schema::SESSION_END_INPUT_FIXTURE,
        schema::SESSION_END_OUTPUT_FIXTURE,
        schema::SUBAGENT_CHANGE_INPUT_FIXTURE,
        schema::SUBAGENT_CHANGE_OUTPUT_FIXTURE,
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
        mcp_server: None,
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
                id: None,
                command: "echo ok".to_string(),
                timeout_sec: Some(5),
                status_message: Some("check".to_string()),
                additional_context_limit: None,
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
    assert_eq!(HookEventConfig::Context.label(), "context");
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
    assert_eq!(HookEventConfig::SessionEnd.label(), "session_end");
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
                id: None,
                command: format!("sh {}", script.display()),
                timeout_sec: Some(5),
                status_message: None,
                additional_context_limit: None,
                env: BTreeMap::new(),
            })],
        }],
        ..HooksConfig::default()
    };

    let result = Hooks::new(config)
        .expect("hooks")
        .run_pre_tool_use(PreToolUseRequest {
            mcp_server: None,
            session_id: "s1".to_string(),
            mcp_tool_state: None,
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
                id: None,
                command: format!("sh {}", script.display()),
                timeout_sec: Some(5),
                status_message: None,
                additional_context_limit: None,
                env: BTreeMap::new(),
            })],
        }],
        ..HooksConfig::default()
    };

    let result = Hooks::new(config)
        .expect("hooks")
        .run_post_tool_use(PostToolUseRequest {
            mcp_server: None,
            session_id: "s1".to_string(),
            mcp_tool_state: None,
            turn_id: "t1".to_string(),
            cwd: Some(dir.path().to_path_buf()),
            model: "mock".to_string(),
            permission_mode: "default".to_string(),
            tool_name: "shell".to_string(),
            tool_input: json!({"command": "echo hi"}),
            content: vec![querymt::chat::ToolResultPart::text("ok")],
            is_error: false,
            execution_is_error: false,
            tool_source: "builtin".to_string(),
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
                id: None,
                command: format!("sh {}", script.display()),
                timeout_sec: Some(5),
                status_message: None,
                additional_context_limit: None,
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

#[test]
fn hooks_config_accepts_mcp_tool_hooks() {
    let config = HooksConfig {
        enabled: true,
        context: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![HookHandlerConfig::McpTool(HookMcpToolConfig {
                id: Some("retriever".into()),
                server: "memory".into(),
                tool: "retrieve".into(),
                input: Some(json!({"query":"$event.model"})),
                timeout_sec: Some(5),
                status_message: Some("Retrieving context".into()),
                additional_context_limit: Some(500),
            })],
        }],
        ..HooksConfig::default()
    };
    Hooks::new(config).expect("valid MCP hook config");
}

#[test]
fn mcp_input_templates_only_resolve_explicit_event_paths() {
    let event = json!({"model":"test-model","nested":{"value":7}});
    let resolved = super::engine::resolve_mcp_input_template_for_test(
        &json!({"query":"$event.model","value":"$event.nested.value"}),
        &event,
    )
    .unwrap();
    assert_eq!(resolved, json!({"query":"test-model","value":7}));
    assert!(
        super::engine::resolve_mcp_input_template_for_test(
            &json!({"bad":"$event.missing"}),
            &event,
        )
        .is_err()
    );
}

fn command_hook(command: String) -> HookHandlerConfig {
    HookHandlerConfig::Command(HookCommandConfig {
        id: None,
        command,
        timeout_sec: Some(5),
        status_message: None,
        additional_context_limit: None,
        env: BTreeMap::new(),
    })
}

fn subagent_request(
    event: HookEventConfig,
    id: &str,
    cwd: Option<std::path::PathBuf>,
) -> super::engine::SubagentChangeRequest {
    super::engine::SubagentChangeRequest {
        event,
        subagent_id: id.into(),
        name: id.into(),
        description: "reviews".into(),
        capabilities: vec!["review".into()],
        origin: "runtime".into(),
        location: "local".into(),
        generation: 7,
        profile_id: None,
        cwd,
    }
}

#[tokio::test]
async fn subagent_hooks_receive_the_change_and_match_on_the_id() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("seen.json");
    let command = format!("cat > '{}'", out.display());
    let config = HooksConfig {
        enabled: true,
        subagent_added: vec![MatcherGroupConfig {
            matcher: Some("^rev".into()),
            hooks: vec![command_hook(command.clone())],
        }],
        subagent_removed: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook(command)],
        }],
        ..HooksConfig::default()
    };
    let hooks = Hooks::new(config).unwrap();

    // A non-matching ID runs nothing.
    hooks
        .run_subagent_change(subagent_request(
            HookEventConfig::SubagentAdded,
            "coder",
            Some(dir.path().into()),
        ))
        .await
        .unwrap();
    assert!(!out.exists());

    hooks
        .run_subagent_change(subagent_request(
            HookEventConfig::SubagentAdded,
            "reviewer",
            Some(dir.path().into()),
        ))
        .await
        .unwrap();
    let seen: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_matches_schema::<crate::hooks::schema::SubagentChangeCommandInput>(&seen);
    assert_eq!(seen["hook_event_name"], "subagent_added");
    assert_eq!(seen["subagent_id"], "reviewer");
    assert_eq!(seen["origin"], "runtime");
    assert_eq!(seen["generation"], 7);
    assert_eq!(seen["capabilities"], json!(["review"]));
    assert!(seen["profile_id"].is_null());

    hooks
        .run_subagent_change(subagent_request(
            HookEventConfig::SubagentRemoved,
            "coder",
            Some(dir.path().into()),
        ))
        .await
        .unwrap();
    let seen: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(seen["hook_event_name"], "subagent_removed");
    assert_eq!(seen["subagent_id"], "coder");
}

#[tokio::test]
async fn subagent_hook_messages_become_notices_and_blocks_are_ignored() {
    let config = HooksConfig {
        enabled: true,
        subagent_added: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![
                command_hook("printf '%s' '{\"system_message\":\"welcome\"}'".into()),
                command_hook("echo no >&2; exit 2".into()),
            ],
        }],
        ..HooksConfig::default()
    };
    assert_matches_schema::<crate::hooks::schema::SubagentChangeCommandOutputWire>(
        &json!({"system_message":"welcome"}),
    );
    let result = Hooks::new(config)
        .unwrap()
        .run_subagent_change(subagent_request(HookEventConfig::SubagentAdded, "a", None))
        .await
        .unwrap();
    assert!(
        result
            .notices
            .iter()
            .any(|n| n.message == "welcome" && !n.is_error)
    );
    assert!(
        result
            .notices
            .iter()
            .any(|n| n.message.contains("observe-only")),
        "a block request is reported, not obeyed: {:?}",
        result.notices
    );
}

#[test]
fn subagent_events_have_labels_and_reject_mcp_handlers() {
    assert_eq!(HookEventConfig::SubagentAdded.label(), "subagent_added");
    assert_eq!(HookEventConfig::SubagentRemoved.label(), "subagent_removed");
    let config = HooksConfig {
        subagent_added: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![HookHandlerConfig::McpTool(HookMcpToolConfig {
                server: "s".into(),
                tool: "t".into(),
                ..Default::default()
            })],
        }],
        ..HooksConfig::default()
    };
    // These events run outside any session, so no MCP tool state exists.
    assert!(matches!(
        Hooks::new(config),
        Err(crate::hooks::HookError::Config(
            crate::hooks::HookConfigError::McpUnsupported {
                event: "subagent_added"
            }
        ))
    ));
}

#[tokio::test]
async fn session_end_system_message_becomes_notice() {
    let config = HooksConfig {
        enabled: true,
        session_end: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook(
                "printf '%s' '{\"system_message\":\"session closed\"}'".into(),
            )],
        }],
        ..HooksConfig::default()
    };

    let input = crate::hooks::schema::SessionEndCommandInput {
        session_id: "s".into(),
        transcript_path: crate::hooks::schema::NullableString::from_string(None),
        cwd: ".".into(),
        hook_event_name: "session_end".into(),
        model: "m".into(),
        permission_mode: "default".into(),
        reason: "close".into(),
    };
    assert_matches_schema::<crate::hooks::schema::SessionEndCommandInput>(
        &serde_json::to_value(input).unwrap(),
    );
    let output = json!({"system_message":"session closed"});
    assert_matches_schema::<crate::hooks::schema::SessionEndCommandOutputWire>(&output);

    let result = Hooks::new(config)
        .unwrap()
        .run_session_end(super::engine::SessionEndRequest {
            session_id: "s".into(),
            cwd: None,
            model: "m".into(),
            permission_mode: "default".into(),
            reason: "close".into(),
        })
        .await
        .unwrap();

    assert_eq!(result.notices.len(), 1);
    assert_eq!(result.notices[0].event_name, "session_end");
    assert_eq!(result.notices[0].message, "session closed");
    assert!(!result.notices[0].is_error);
}

#[tokio::test]
async fn exit_two_blocks_with_stderr_and_surfaces_system_message() {
    let config = HooksConfig {
        enabled: true,
        pre_tool_use: vec![MatcherGroupConfig {
            matcher: Some("^shell$".into()),
            hooks: vec![command_hook(
                "printf '{\"system_message\":\"ignored stdout\"}'; printf 'policy denied' >&2; exit 2"
                    .into(),
            )],
        }],
        ..HooksConfig::default()
    };
    let result = Hooks::new(config)
        .unwrap()
        .run_pre_tool_use(PreToolUseRequest {
            mcp_server: None,
            session_id: "s".into(),
            mcp_tool_state: None,
            turn_id: "t".into(),
            cwd: None,
            model: "m".into(),
            permission_mode: "default".into(),
            tool_name: "shell".into(),
            tool_input: json!({"command":"true"}),
            tool_use_id: "u".into(),
        })
        .await
        .unwrap();
    assert!(result.should_block);
    assert_eq!(result.block_reason.as_deref(), Some("policy denied"));
    assert_eq!(result.notices[0].message, "ignored stdout");
}

#[tokio::test]
async fn pre_tool_rewrites_chain_in_order() {
    let second = r#"input=$(cat); case "$input" in *first*) printf '%s' '{"hook_specific_output":{"hook_event_name":"pre_tool_use","permission_decision":"allow","updated_input":{"command":"second"}}}' ;; *) exit 9 ;; esac"#;
    let config = HooksConfig {
        enabled: true,
        pre_tool_use: vec![MatcherGroupConfig {
            matcher: Some("^shell$".into()),
            hooks: vec![
                command_hook("printf '%s' '{\"hook_specific_output\":{\"hook_event_name\":\"pre_tool_use\",\"permission_decision\":\"allow\",\"updated_input\":{\"command\":\"first\"}}}'".into()),
                command_hook(second.into()),
            ],
        }],
        ..HooksConfig::default()
    };
    let result = Hooks::new(config)
        .unwrap()
        .run_pre_tool_use(PreToolUseRequest {
            mcp_server: None,
            session_id: "s".into(),
            mcp_tool_state: None,
            turn_id: "t".into(),
            cwd: None,
            model: "m".into(),
            permission_mode: "default".into(),
            tool_name: "shell".into(),
            tool_input: json!({"command":"original"}),
            tool_use_id: "u".into(),
        })
        .await
        .unwrap();
    assert_eq!(result.updated_input, Some(json!({"command":"second"})));
}

#[tokio::test]
async fn post_tool_patches_chain_and_preserve_execution_truth() {
    let second = r#"input=$(cat); case "$input" in *redacted*) printf '%s' '{"hook_specific_output":{"hook_event_name":"post_tool_use","updated_output":{"is_error":true}}}' ;; *) exit 9 ;; esac"#;
    let config = HooksConfig {
        enabled: true,
        post_tool_use: vec![MatcherGroupConfig {
            matcher: Some("^shell$".into()),
            hooks: vec![
                command_hook("printf '%s' '{\"hook_specific_output\":{\"hook_event_name\":\"post_tool_use\",\"updated_output\":{\"content\":[{\"type\":\"text\",\"text\":\"redacted\"}]}}}'".into()),
                command_hook(second.into()),
            ],
        }],
        ..HooksConfig::default()
    };
    let result = Hooks::new(config)
        .unwrap()
        .run_post_tool_use(PostToolUseRequest {
            mcp_server: None,
            session_id: "s".into(),
            mcp_tool_state: None,
            turn_id: "t".into(),
            cwd: None,
            model: "m".into(),
            permission_mode: "default".into(),
            tool_name: "shell".into(),
            tool_input: json!({}),
            content: vec![querymt::chat::ToolResultPart::text("secret")],
            is_error: false,
            execution_is_error: false,
            tool_source: "builtin".into(),
            tool_use_id: "u".into(),
        })
        .await
        .unwrap();
    assert_eq!(result.content.unwrap()[0].as_text(), Some("redacted"));
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn context_replacement_is_request_only_and_chained() {
    let config = HooksConfig {
        enabled: true,
        context: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook("printf '%s' '{\"hook_specific_output\":{\"hook_event_name\":\"context\",\"messages\":[{\"role\":\"User\",\"content\":[{\"type\":\"text\",\"text\":\"projected\"}],\"cache\":null}],\"additional_context\":\"memory\"}}'".into())],
        }],
        ..HooksConfig::default()
    };
    let stored = vec![querymt::chat::ChatMessage::user().text("stored").build()];
    let result = Hooks::new(config)
        .unwrap()
        .run_context(ContextHookRequest {
            session_id: "s".into(),
            mcp_tool_state: None,
            turn_id: "t".into(),
            cwd: None,
            model: "m".into(),
            permission_mode: "default".into(),
            trigger: "user_prompt".into(),
            context_window: 1000,
            messages: stored.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        stored[0].portable_input_parts()[0].as_text(),
        Some("stored")
    );
    let projected = result.messages.unwrap();
    assert_eq!(
        projected[0].portable_input_parts()[0].as_text(),
        Some("projected")
    );
    // `additional_context` is request-scoped: returned separately, never merged
    // into a durable message.
    assert_eq!(projected[0].portable_input_parts().len(), 1);
    assert_eq!(result.additional_context.len(), 1);
    assert!(result.additional_context[0].contains("memory"));
    assert!(result.additional_context[0].contains("<hook-context event=context"));
}

/// Runs one `context` hook that answers with `replacement` over `stored`.
async fn run_context_replacement(
    stored: Vec<querymt::chat::ChatMessage>,
    replacement: Vec<querymt::chat::ChatMessage>,
) -> super::engine::ContextHookResult {
    let output = json!({"hook_specific_output": {
        "hook_event_name": "context",
        "messages": replacement,
    }});
    let config = HooksConfig {
        enabled: true,
        context: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook(format!("printf '%s' '{output}'"))],
        }],
        ..HooksConfig::default()
    };
    Hooks::new(config)
        .unwrap()
        .run_context(ContextHookRequest {
            session_id: "s".into(),
            mcp_tool_state: None,
            turn_id: "t".into(),
            cwd: None,
            model: "m".into(),
            permission_mode: "default".into(),
            trigger: "user_prompt".into(),
            context_window: 1000,
            messages: stored,
        })
        .await
        .unwrap()
}

fn tool_change_message() -> querymt::chat::ChatMessage {
    let tool = querymt::chat::Tool {
        tool_type: "function".into(),
        function: querymt::chat::FunctionTool {
            name: "deploy".into(),
            description: "Deploy".into(),
            parameters: json!({"type": "object"}),
            strict: None,
        },
    };
    querymt::chat::ChatMessage::from_tool_change(querymt::chat::ToolChange::new(
        vec![tool],
        Vec::new(),
    ))
}

#[tokio::test]
async fn context_replacement_that_drops_a_tool_change_is_ignored() {
    // A tool change is stored once and never generated again, so a request
    // without it would keep offering the old tools.
    let stored = vec![
        querymt::chat::ChatMessage::user().text("go").build(),
        tool_change_message(),
    ];
    let replacement = vec![querymt::chat::ChatMessage::user().text("projected").build()];

    let result = run_context_replacement(stored.clone(), replacement).await;

    assert_eq!(result.messages.unwrap(), stored);
    assert!(
        result
            .notices
            .iter()
            .any(|notice| notice.message.contains("tool-change")),
        "notices: {:?}",
        result.notices
    );
}

#[tokio::test]
async fn context_replacement_that_keeps_the_tool_change_is_applied() {
    let stored = vec![
        querymt::chat::ChatMessage::user().text("go").build(),
        tool_change_message(),
    ];
    let replacement = vec![
        querymt::chat::ChatMessage::user().text("projected").build(),
        tool_change_message(),
    ];

    let result = run_context_replacement(stored, replacement.clone()).await;

    assert_eq!(result.messages.unwrap(), replacement);
    assert!(result.notices.is_empty(), "notices: {:?}", result.notices);
}

#[tokio::test]
async fn context_hook_without_additional_context_returns_none() {
    let config = HooksConfig {
        enabled: true,
        context: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook("printf '{}'".into())],
        }],
        ..HooksConfig::default()
    };
    let result = Hooks::new(config)
        .unwrap()
        .run_context(ContextHookRequest {
            session_id: "s".into(),
            mcp_tool_state: None,
            turn_id: "t".into(),
            cwd: None,
            model: "m".into(),
            permission_mode: "default".into(),
            trigger: "user_prompt".into(),
            context_window: 1000,
            messages: vec![querymt::chat::ChatMessage::user().text("stored").build()],
        })
        .await
        .unwrap();
    assert!(result.additional_context.is_empty());
    assert_eq!(result.messages.unwrap().len(), 1);
}

#[tokio::test]
async fn pre_compaction_accepts_custom_summary() {
    let config = HooksConfig {
        enabled: true,
        pre_compaction: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook("printf '%s' '{\"hook_specific_output\":{\"hook_event_name\":\"pre_compaction\",\"compaction\":{\"summary\":\"custom summary\"}}}'".into())],
        }],
        ..HooksConfig::default()
    };
    let result = Hooks::new(config)
        .unwrap()
        .run_pre_compaction(PreCompactionRequest {
            session_id: "s".into(),
            turn_id: "t".into(),
            cwd: None,
            model: "m".into(),
            permission_mode: "default".into(),
            trigger: "context_threshold".into(),
            token_estimate: 100,
            message_count: 2,
            messages: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(result.custom_summary.as_deref(), Some("custom summary"));
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

fn post_request(tool: &str, server: Option<&str>, dir: &TempDir) -> PostToolUseRequest {
    PostToolUseRequest {
        mcp_server: server.map(Into::into),
        session_id: "s1".into(),
        mcp_tool_state: None,
        turn_id: "t1".into(),
        cwd: Some(dir.path().to_path_buf()),
        model: "mock".into(),
        permission_mode: "default".into(),
        tool_name: tool.into(),
        tool_input: json!({}),
        content: vec![querymt::chat::ToolResultPart::text("filled")],
        is_error: false,
        execution_is_error: false,
        tool_source: "mcp".into(),
        tool_use_id: "tool1".into(),
    }
}

fn pre_request(tool: &str, server: Option<&str>, dir: &TempDir) -> PreToolUseRequest {
    PreToolUseRequest {
        mcp_server: server.map(Into::into),
        session_id: "s1".into(),
        mcp_tool_state: None,
        turn_id: "t1".into(),
        cwd: Some(dir.path().to_path_buf()),
        model: "mock".into(),
        permission_mode: "default".into(),
        tool_name: tool.into(),
        tool_input: json!({}),
        tool_use_id: "tool1".into(),
    }
}

#[test]
fn hooks_are_enabled_unless_switched_off() {
    let on: HooksConfig = serde_json::from_value(json!({})).unwrap();
    assert!(on.enabled);
    let off: HooksConfig = serde_json::from_value(json!({"enabled": false})).unwrap();
    assert!(!off.enabled);
}

#[test]
fn invalid_matcher_regex_is_rejected_at_load() {
    let config = HooksConfig {
        pre_tool_use: vec![MatcherGroupConfig {
            matcher: Some("(".into()),
            hooks: vec![command_hook("true".into())],
        }],
        ..HooksConfig::default()
    };
    let err = Hooks::new(config).unwrap_err().to_string();
    assert!(err.contains("invalid matcher regex"), "{err}");
}

#[tokio::test]
async fn failing_post_tool_hook_keeps_the_tool_result() {
    let dir = TempDir::new().unwrap();
    let hooks = Hooks::new(HooksConfig {
        post_tool_use: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook("exit 1".into())],
        }],
        ..HooksConfig::default()
    })
    .unwrap();
    let result = hooks
        .run_post_tool_use(post_request("place_order", Some("ibkr"), &dir))
        .await
        .expect("a failing observer is not an error");
    assert!(!result.should_block);
    assert_eq!(result.is_error, Some(false));
    assert!(result.notices.iter().any(|n| n.is_error));
}

#[tokio::test]
async fn matcher_accepts_plain_and_server_qualified_tool_names() {
    let dir = TempDir::new().unwrap();
    let block = command_hook("echo blocked >&2; exit 2".into());
    for matcher in ["^place_order$", "^ibkr\\.place_order$"] {
        let hooks = Hooks::new(HooksConfig {
            pre_tool_use: vec![MatcherGroupConfig {
                matcher: Some(matcher.into()),
                hooks: vec![block.clone()],
            }],
            ..HooksConfig::default()
        })
        .unwrap();
        let hit = hooks
            .run_pre_tool_use(pre_request("place_order", Some("ibkr"), &dir))
            .await
            .unwrap();
        assert!(hit.should_block, "{matcher}");
        let miss = hooks
            .run_pre_tool_use(pre_request("place_order", Some("other"), &dir))
            .await
            .unwrap();
        assert_eq!(miss.should_block, matcher == "^place_order$", "{matcher}");
    }
}

#[tokio::test]
async fn hook_commands_see_profile_dir_and_mcp_server() {
    let dir = TempDir::new().unwrap();
    let hooks = Hooks::new(HooksConfig {
        pre_tool_use: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook(
                "cat > input.json; printf %s \"$QMT_PROFILE_DIR\" > profile-dir".into(),
            )],
        }],
        base_dir: Some("/profiles".into()),
        ..HooksConfig::default()
    })
    .unwrap();
    hooks
        .run_pre_tool_use(pre_request("get_quote", Some("ibkr"), &dir))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("profile-dir")).unwrap(),
        "/profiles"
    );
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("input.json")).unwrap()).unwrap();
    assert_eq!(input["mcp_server"], "ibkr");
}

#[tokio::test]
async fn ask_decision_carries_the_hook_reason() {
    let dir = TempDir::new().unwrap();
    let hooks = Hooks::new(HooksConfig {
        pre_tool_use: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![command_hook(
                r#"printf '{"hook_specific_output":{"hook_event_name":"pre_tool_use","permission_decision":"ask","permission_decision_reason":"BUY 1 SPY"}}'"#.into(),
            )],
        }],
        ..HooksConfig::default()
    })
    .unwrap();
    let result = hooks
        .run_pre_tool_use(pre_request("place_order", Some("ibkr"), &dir))
        .await
        .unwrap();
    assert_eq!(
        result.permission_decision,
        Some(crate::hooks::engine::PreToolPermissionDecision::Ask {
            reason: Some("BUY 1 SPY".into())
        })
    );
}
