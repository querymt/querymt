use schemars::generate::{SchemaGenerator, SchemaSettings};
use schemars::{JsonSchema, Schema, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const GENERATED_DIR: &str = "generated";
pub const PRE_TOOL_USE_INPUT_FIXTURE: &str = "pre-tool-use.command.input.schema.json";
pub const PRE_TOOL_USE_OUTPUT_FIXTURE: &str = "pre-tool-use.command.output.schema.json";
pub const PERMISSION_REQUEST_INPUT_FIXTURE: &str = "permission-request.command.input.schema.json";
pub const PERMISSION_REQUEST_OUTPUT_FIXTURE: &str = "permission-request.command.output.schema.json";
pub const POST_TOOL_USE_INPUT_FIXTURE: &str = "post-tool-use.command.input.schema.json";
pub const POST_TOOL_USE_OUTPUT_FIXTURE: &str = "post-tool-use.command.output.schema.json";
pub const USER_PROMPT_SUBMIT_INPUT_FIXTURE: &str = "user-prompt-submit.command.input.schema.json";
pub const USER_PROMPT_SUBMIT_OUTPUT_FIXTURE: &str = "user-prompt-submit.command.output.schema.json";
pub const SESSION_START_INPUT_FIXTURE: &str = "session-start.command.input.schema.json";
pub const SESSION_START_OUTPUT_FIXTURE: &str = "session-start.command.output.schema.json";
pub const STOP_INPUT_FIXTURE: &str = "stop.command.input.schema.json";
pub const STOP_OUTPUT_FIXTURE: &str = "stop.command.output.schema.json";
pub const PRE_COMPACTION_INPUT_FIXTURE: &str = "pre-compaction.command.input.schema.json";
pub const PRE_COMPACTION_OUTPUT_FIXTURE: &str = "pre-compaction.command.output.schema.json";
pub const POST_COMPACTION_INPUT_FIXTURE: &str = "post-compaction.command.input.schema.json";
pub const POST_COMPACTION_OUTPUT_FIXTURE: &str = "post-compaction.command.output.schema.json";
pub const PRE_DELEGATION_INPUT_FIXTURE: &str = "pre-delegation.command.input.schema.json";
pub const PRE_DELEGATION_OUTPUT_FIXTURE: &str = "pre-delegation.command.output.schema.json";
pub const DELEGATION_START_INPUT_FIXTURE: &str = "delegation-start.command.input.schema.json";
pub const DELEGATION_START_OUTPUT_FIXTURE: &str = "delegation-start.command.output.schema.json";
pub const POST_DELEGATION_INPUT_FIXTURE: &str = "post-delegation.command.input.schema.json";
pub const POST_DELEGATION_OUTPUT_FIXTURE: &str = "post-delegation.command.output.schema.json";
pub const DELEGATION_FAILURE_INPUT_FIXTURE: &str = "delegation-failure.command.input.schema.json";
pub const DELEGATION_FAILURE_OUTPUT_FIXTURE: &str = "delegation-failure.command.output.schema.json";

#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct NullableString(Option<String>);

impl NullableString {
    pub fn from_path(path: Option<PathBuf>) -> Self {
        Self(path.map(|path| path.display().to_string()))
    }

    pub fn from_string(value: Option<String>) -> Self {
        Self(value)
    }
}

impl JsonSchema for NullableString {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NullableString".into()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": ["string", "null"]
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookUniversalOutputWire {
    #[serde(default = "default_continue")]
    pub r#continue: bool,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub system_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum PreToolUseDecisionWire {
    #[serde(rename = "approve")]
    Approve,
    #[serde(rename = "block")]
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum PreToolUsePermissionDecisionWire {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "deny")]
    Deny,
    #[serde(rename = "ask")]
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum PermissionRequestBehaviorWire {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "deny")]
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum BlockDecisionWire {
    #[serde(rename = "block")]
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatedDelegationWire {
    #[serde(default)]
    pub target_agent_id: Option<String>,
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub constraints: Option<String>,
    #[serde(default)]
    pub expected_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreToolUseHookSpecificOutputWire {
    #[schemars(schema_with = "pre_tool_use_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub permission_decision: Option<PreToolUsePermissionDecisionWire>,
    #[serde(default)]
    pub permission_decision_reason: Option<String>,
    #[serde(default)]
    pub updated_input: Option<Value>,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequestDecisionWire {
    pub behavior: PermissionRequestBehaviorWire,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequestHookSpecificOutputWire {
    #[schemars(schema_with = "permission_request_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub decision: Option<PermissionRequestDecisionWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PostToolUseHookSpecificOutputWire {
    #[schemars(schema_with = "post_tool_use_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UserPromptSubmitHookSpecificOutputWire {
    #[schemars(schema_with = "user_prompt_submit_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionStartHookSpecificOutputWire {
    #[schemars(schema_with = "session_start_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopHookSpecificOutputWire {
    #[schemars(schema_with = "stop_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreCompactionHookSpecificOutputWire {
    #[schemars(schema_with = "pre_compaction_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PostCompactionHookSpecificOutputWire {
    #[schemars(schema_with = "post_compaction_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreDelegationHookSpecificOutputWire {
    #[schemars(schema_with = "pre_delegation_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub updated_delegation: Option<UpdatedDelegationWire>,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegationLifecycleHookSpecificOutputWire {
    #[schemars(schema_with = "delegation_lifecycle_hook_event_name_schema")]
    pub hook_event_name: String,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "pre-tool-use.command.output")]
pub struct PreToolUseCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<PreToolUseDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<PreToolUseHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "permission-request.command.output")]
pub struct PermissionRequestCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub hook_specific_output: Option<PermissionRequestHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "post-tool-use.command.output")]
pub struct PostToolUseCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<BlockDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<PostToolUseHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "user-prompt-submit.command.output")]
pub struct UserPromptSubmitCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<BlockDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<UserPromptSubmitHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "session-start.command.output")]
pub struct SessionStartCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub hook_specific_output: Option<SessionStartHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "stop.command.output")]
pub struct StopCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<BlockDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<StopHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "pre-compaction.command.output")]
pub struct PreCompactionCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<BlockDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<PreCompactionHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "post-compaction.command.output")]
pub struct PostCompactionCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub hook_specific_output: Option<PostCompactionHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "pre-delegation.command.output")]
pub struct PreDelegationCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<BlockDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<PreDelegationHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "delegation-start.command.output")]
pub struct DelegationStartCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub hook_specific_output: Option<DelegationLifecycleHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "post-delegation.command.output")]
pub struct PostDelegationCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub hook_specific_output: Option<DelegationLifecycleHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "delegation-failure.command.output")]
pub struct DelegationFailureCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub hook_specific_output: Option<DelegationLifecycleHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "pre-tool-use.command.input")]
pub struct PreToolUseCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "pre_tool_use_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "permission-request.command.input")]
pub struct PermissionRequestCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "permission_request_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "post-tool-use.command.input")]
pub struct PostToolUseCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "post_tool_use_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_response: Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "user-prompt-submit.command.input")]
pub struct UserPromptSubmitCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "user_prompt_submit_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "session-start.command.input")]
pub struct SessionStartCommandInput {
    pub session_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "session_start_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "stop.command.input")]
pub struct StopCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "stop_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub stop_reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "pre-compaction.command.input")]
pub struct PreCompactionCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "pre_compaction_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub trigger: String,
    pub token_estimate: u32,
    pub message_count: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "post-compaction.command.input")]
pub struct PostCompactionCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "post_compaction_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub trigger: String,
    pub summary: String,
    pub original_token_count: u32,
    pub summary_token_count: u32,
    pub message_count: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "pre-delegation.command.input")]
pub struct PreDelegationCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "pre_delegation_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub tool_use_id: String,
    pub target_agent_id: String,
    pub objective: String,
    pub context: NullableString,
    pub constraints: NullableString,
    pub expected_output: NullableString,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "delegation-start.command.input")]
pub struct DelegationStartCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "delegation_start_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub delegation_id: String,
    pub target_agent_id: String,
    pub objective: String,
    pub context: NullableString,
    pub constraints: NullableString,
    pub expected_output: NullableString,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "post-delegation.command.input")]
pub struct PostDelegationCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "post_delegation_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub delegation_id: String,
    pub target_agent_id: String,
    pub child_session_id: String,
    pub objective: String,
    pub status: String,
    pub summary: String,
    pub verification_passed: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "delegation-failure.command.input")]
pub struct DelegationFailureCommandInput {
    pub session_id: String,
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "delegation_failure_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub delegation_id: String,
    pub target_agent_id: String,
    pub objective: String,
    pub status: String,
    pub error: String,
    pub error_type: String,
}

pub fn write_schema_fixtures(schema_root: &Path) -> anyhow::Result<()> {
    let generated_dir = schema_root.join(GENERATED_DIR);
    ensure_empty_dir(&generated_dir)?;

    write_schema(
        &generated_dir.join(PRE_TOOL_USE_INPUT_FIXTURE),
        schema_json::<PreToolUseCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(PRE_TOOL_USE_OUTPUT_FIXTURE),
        schema_json::<PreToolUseCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(PERMISSION_REQUEST_INPUT_FIXTURE),
        schema_json::<PermissionRequestCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(PERMISSION_REQUEST_OUTPUT_FIXTURE),
        schema_json::<PermissionRequestCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(POST_TOOL_USE_INPUT_FIXTURE),
        schema_json::<PostToolUseCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(POST_TOOL_USE_OUTPUT_FIXTURE),
        schema_json::<PostToolUseCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(USER_PROMPT_SUBMIT_INPUT_FIXTURE),
        schema_json::<UserPromptSubmitCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(USER_PROMPT_SUBMIT_OUTPUT_FIXTURE),
        schema_json::<UserPromptSubmitCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(SESSION_START_INPUT_FIXTURE),
        schema_json::<SessionStartCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(SESSION_START_OUTPUT_FIXTURE),
        schema_json::<SessionStartCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(STOP_INPUT_FIXTURE),
        schema_json::<StopCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(STOP_OUTPUT_FIXTURE),
        schema_json::<StopCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(PRE_COMPACTION_INPUT_FIXTURE),
        schema_json::<PreCompactionCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(PRE_COMPACTION_OUTPUT_FIXTURE),
        schema_json::<PreCompactionCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(POST_COMPACTION_INPUT_FIXTURE),
        schema_json::<PostCompactionCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(POST_COMPACTION_OUTPUT_FIXTURE),
        schema_json::<PostCompactionCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(PRE_DELEGATION_INPUT_FIXTURE),
        schema_json::<PreDelegationCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(PRE_DELEGATION_OUTPUT_FIXTURE),
        schema_json::<PreDelegationCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(DELEGATION_START_INPUT_FIXTURE),
        schema_json::<DelegationStartCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(DELEGATION_START_OUTPUT_FIXTURE),
        schema_json::<DelegationStartCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(POST_DELEGATION_INPUT_FIXTURE),
        schema_json::<PostDelegationCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(POST_DELEGATION_OUTPUT_FIXTURE),
        schema_json::<PostDelegationCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(DELEGATION_FAILURE_INPUT_FIXTURE),
        schema_json::<DelegationFailureCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(DELEGATION_FAILURE_OUTPUT_FIXTURE),
        schema_json::<DelegationFailureCommandOutputWire>()?,
    )?;

    Ok(())
}

fn write_schema(path: &Path, json: Vec<u8>) -> anyhow::Result<()> {
    std::fs::write(path, json)?;
    Ok(())
}

fn ensure_empty_dir(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}

pub fn schema_json<T>() -> anyhow::Result<Vec<u8>>
where
    T: JsonSchema,
{
    let schema = schema_for_type::<T>();
    let value = serde_json::to_value(schema)?;
    let value = canonicalize_json(&value);
    Ok(serde_json::to_vec_pretty(&value)?)
}

fn schema_for_type<T>() -> Schema
where
    T: JsonSchema,
{
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<T>()
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            let mut sorted = Map::with_capacity(map.len());
            for (key, child) in entries {
                sorted.insert(key.clone(), canonicalize_json(child));
            }
            Value::Object(sorted)
        }
        _ => value.clone(),
    }
}

fn pre_tool_use_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("pre_tool_use")
}

fn permission_request_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("permission_request")
}

fn post_tool_use_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("post_tool_use")
}

fn user_prompt_submit_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("user_prompt_submit")
}

fn session_start_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("session_start")
}

fn stop_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("stop")
}

fn pre_compaction_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("pre_compaction")
}

fn post_compaction_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("post_compaction")
}

fn pre_delegation_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("pre_delegation")
}

fn delegation_start_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("delegation_start")
}

fn post_delegation_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("post_delegation")
}

fn delegation_failure_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("delegation_failure")
}

fn delegation_lifecycle_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_enum_schema(&["delegation_start", "post_delegation", "delegation_failure"])
}

fn permission_mode_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_enum_schema(&["default", "plan", "accept_edits"])
}

fn string_const_schema(value: &str) -> Schema {
    json_schema!({
        "type": "string",
        "const": value,
    })
}

fn string_enum_schema(values: &[&str]) -> Schema {
    let enum_values: Vec<String> = values.iter().map(|value| (*value).to_string()).collect();
    json_schema!({
        "type": "string",
        "enum": enum_values,
    })
}

fn default_continue() -> bool {
    true
}
