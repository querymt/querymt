//! ATIF (Agent Trajectory Interchange Format) v1.5 implementation.
//!
//! This module implements the ATIF specification for exporting agent trajectories
//! in a standardized JSON format. See: https://github.com/laude-institute/harbor

use crate::events::AgentEvent;
use crate::export::turns::{self, Turn};
use crate::session::projection::AuditView;
use querymt::chat::ReasoningEffort;
use serde::{Deserialize, Serialize};

/// ATIF Trajectory root object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ATIF {
    pub schema_version: String,
    pub session_id: String,
    pub agent: AtifAgent,
    pub steps: Vec<AtifStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_metrics: Option<AtifFinalMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continued_trajectory_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Agent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifAgent {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_definitions: Option<Vec<AtifToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Tool definition (OpenAI function calling schema)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifToolDefinition {
    #[serde(rename = "type")]
    pub type_field: String, // "function"
    pub function: AtifFunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Step source (who produced this step)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AtifSource {
    System,
    User,
    Agent,
}

/// A single step in the trajectory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifStep {
    pub step_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>, // ISO 8601
    pub source: AtifSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AtifToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<AtifObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<AtifMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Tool call within a step
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifToolCall {
    pub tool_call_id: String,
    pub function_name: String,
    pub arguments: serde_json::Value,
}

/// Observation (environment feedback)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifObservation {
    pub results: Vec<AtifObservationResult>,
}

/// A single observation result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifObservationResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_trajectory_ref: Option<Vec<AtifSubagentTrajectoryRef>>,
}

/// Reference to a subagent trajectory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifSubagentTrajectoryRef {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trajectory_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Per-step LLM metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_token_ids: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_token_ids: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Trajectory-level aggregate metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifFinalMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cached_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_steps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Export options for ATIF trajectory generation
#[derive(Default, Debug, Clone)]
pub struct AtifExportOptions {
    /// Custom agent name (defaults to "querymt")
    pub agent_name: Option<String>,
    /// Custom agent version (defaults to env!("QMT_BUILD_VERSION"))
    pub agent_version: Option<String>,
    /// Custom notes to include in trajectory
    pub notes: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert querymt tool definitions to ATIF tool definitions.
fn tools_to_atif(tools: &[querymt::chat::Tool]) -> Vec<AtifToolDefinition> {
    tools
        .iter()
        .map(|t| AtifToolDefinition {
            type_field: t.tool_type.clone(),
            function: AtifFunctionDef {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                parameters: t.function.parameters.clone(),
            },
        })
        .collect()
}

/// Builder for constructing ATIF trajectories from agent events
pub struct ATIFBuilder {
    session_id: String,
    agent_name: String,
    agent_version: String,
    model_name: Option<String>,
    tool_definitions: Option<Vec<AtifToolDefinition>>,
    steps: Vec<AtifStep>,
    notes: Option<String>,
    // Accumulator state
    current_step_id: u32,
    total_prompt_tokens: u32,
    total_completion_tokens: u32,
    total_cached_tokens: u32,
    total_cost_usd: f64,
}

impl ATIFBuilder {
    /// Create a new trajectory builder
    pub fn new(session_id: String, options: &AtifExportOptions) -> Self {
        Self {
            session_id,
            agent_name: options
                .agent_name
                .clone()
                .unwrap_or_else(|| "querymt".to_string()),
            agent_version: options
                .agent_version
                .clone()
                .unwrap_or_else(|| env!("QMT_BUILD_VERSION").to_string()),
            model_name: None,
            tool_definitions: None,
            steps: Vec::new(),
            notes: options.notes.clone(),
            current_step_id: 1,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_cached_tokens: 0,
            total_cost_usd: 0.0,
        }
    }

    /// Build trajectory from AuditView
    pub fn from_audit_view(view: &AuditView, options: &AtifExportOptions) -> Self {
        let mut builder = Self::new(view.session_id.clone(), options);

        let (materialized_turns, meta) = turns::materialize_turns(&view.events);

        builder.model_name = meta.initial_model;

        // Convert tool definitions from session metadata
        if let Some(tools) = meta.tool_definitions {
            builder.tool_definitions = Some(tools_to_atif(&tools));
        }

        // Add system steps for middleware-injected prompts
        for (i, prompt) in meta.system_prompts.iter().enumerate() {
            // Use a synthetic timestamp; system prompts precede turns
            let ts = materialized_turns
                .first()
                .map(|t| t.timestamp - 1)
                .unwrap_or(0);
            builder.add_system_step(ts - (meta.system_prompts.len() - i) as i64, prompt.clone());
        }

        // Convert each materialized turn into ATIF steps
        for turn in &materialized_turns {
            // User step (if there was a user message preceding this turn)
            if let Some(user) = &turn.user_content {
                builder.add_user_step(turn.timestamp, user.clone());
            }

            builder.add_agent_step_from_turn(turn);
        }

        builder
    }

    /// Set tool definitions
    pub fn with_tool_definitions(mut self, tools: Vec<querymt::chat::Tool>) -> Self {
        self.tool_definitions = Some(tools_to_atif(&tools));
        self
    }

    /// Process a sequence of events into steps.
    ///
    /// Delegates to the shared [`turns::materialize_turns`] implementation and
    /// maps the result into ATIF steps.
    pub fn process_events(&mut self, events: &[AgentEvent]) {
        let (materialized_turns, meta) = turns::materialize_turns(events);

        if self.model_name.is_none() {
            self.model_name = meta.initial_model;
        }

        if self.tool_definitions.is_none() && meta.tool_definitions.is_some() {
            self.tool_definitions = meta.tool_definitions.map(|tools| tools_to_atif(&tools));
        }

        for prompt in &meta.system_prompts {
            let ts = materialized_turns
                .first()
                .map(|t| t.timestamp - 1)
                .unwrap_or(0);
            self.add_system_step(ts, prompt.clone());
        }

        for turn in &materialized_turns {
            if let Some(user) = &turn.user_content {
                self.add_user_step(turn.timestamp, user.clone());
            }
            self.add_agent_step_from_turn(turn);
        }
    }

    /// Convert a materialized [`Turn`] into an ATIF agent step.
    fn add_agent_step_from_turn(&mut self, turn: &Turn) {
        // Build metrics from usage
        let metrics = turn.usage.as_ref().map(|u| {
            let m = AtifMetrics {
                prompt_tokens: Some(u.input_tokens),
                completion_tokens: Some(u.output_tokens),
                cached_tokens: None,
                cost_usd: turn.cost_usd,
                prompt_token_ids: None,
                completion_token_ids: None,
                logprobs: None,
                extra: None,
            };
            self.total_prompt_tokens += m.prompt_tokens.unwrap_or(0);
            self.total_completion_tokens += m.completion_tokens.unwrap_or(0);
            self.total_cost_usd += m.cost_usd.unwrap_or(0.0);
            m
        });

        // Build tool calls
        let tool_calls_opt = if turn.tool_calls.is_empty() {
            None
        } else {
            Some(
                turn.tool_calls
                    .iter()
                    .map(|tc| AtifToolCall {
                        tool_call_id: tc.id.clone(),
                        function_name: tc.name.clone(),
                        arguments: serde_json::from_str(&tc.arguments)
                            .unwrap_or(serde_json::json!({})),
                    })
                    .collect(),
            )
        };

        // Build observation results from tool results + delegations
        let mut observation_results: Vec<AtifObservationResult> = turn
            .tool_results
            .iter()
            .map(|tr| AtifObservationResult {
                source_call_id: Some(tr.call_id.clone()),
                content: Some(if tr.is_error {
                    format!("ERROR: {}", tr.content)
                } else {
                    tr.content.clone()
                }),
                subagent_trajectory_ref: None,
            })
            .collect();

        for d in &turn.delegations {
            observation_results.push(AtifObservationResult {
                source_call_id: None,
                content: d.result.clone(),
                subagent_trajectory_ref: Some(vec![AtifSubagentTrajectoryRef {
                    session_id: d.delegation_id.clone(),
                    trajectory_path: None,
                    extra: None,
                }]),
            });
        }

        let observation = if observation_results.is_empty() {
            None
        } else {
            Some(AtifObservation {
                results: observation_results,
            })
        };

        self.steps.push(AtifStep {
            step_id: self.current_step_id,
            timestamp: Some(self.format_timestamp(turn.timestamp)),
            source: AtifSource::Agent,
            model_name: turn.model.clone().or_else(|| self.model_name.clone()),
            reasoning_effort: None,
            message: turn.assistant_content.clone(),
            reasoning_content: turn.thinking.clone(),
            tool_calls: tool_calls_opt,
            observation,
            metrics,
            extra: None,
        });

        self.current_step_id += 1;
    }

    /// Add a system step
    fn add_system_step(&mut self, timestamp: i64, message: String) {
        self.steps.push(AtifStep {
            step_id: self.current_step_id,
            timestamp: Some(self.format_timestamp(timestamp)),
            source: AtifSource::System,
            model_name: None,
            reasoning_effort: None,
            message,
            reasoning_content: None,
            tool_calls: None,
            observation: None,
            metrics: None,
            extra: None,
        });
        self.current_step_id += 1;
    }

    /// Add a user step
    fn add_user_step(&mut self, timestamp: i64, message: String) {
        self.steps.push(AtifStep {
            step_id: self.current_step_id,
            timestamp: Some(self.format_timestamp(timestamp)),
            source: AtifSource::User,
            model_name: None,
            reasoning_effort: None,
            message,
            reasoning_content: None,
            tool_calls: None,
            observation: None,
            metrics: None,
            extra: None,
        });
        self.current_step_id += 1;
    }

    /// Format Unix timestamp as ISO 8601
    fn format_timestamp(&self, timestamp: i64) -> String {
        use time::format_description::well_known::Rfc3339;
        time::OffsetDateTime::from_unix_timestamp(timestamp)
            .ok()
            .and_then(|dt| dt.format(&Rfc3339).ok())
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
    }

    /// Build the final trajectory
    pub fn build(self) -> ATIF {
        let final_metrics = if self.total_prompt_tokens > 0 || self.total_completion_tokens > 0 {
            Some(AtifFinalMetrics {
                total_prompt_tokens: Some(self.total_prompt_tokens),
                total_completion_tokens: Some(self.total_completion_tokens),
                total_cached_tokens: Some(self.total_cached_tokens),
                total_cost_usd: Some(self.total_cost_usd),
                total_steps: Some(self.steps.len() as u32),
                extra: None,
            })
        } else {
            None
        };

        ATIF {
            schema_version: "ATIF-v1.5".to_string(),
            session_id: self.session_id,
            agent: AtifAgent {
                name: self.agent_name,
                version: self.agent_version,
                model_name: self.model_name,
                tool_definitions: self.tool_definitions,
                extra: None,
            },
            steps: self.steps,
            notes: self.notes,
            final_metrics,
            continued_trajectory_ref: None,
            extra: None,
        }
    }
}

impl ATIF {
    /// Export trajectory as JSON string
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Export trajectory as JSON value
    pub fn to_json_value(&self) -> serde_json::Result<serde_json::Value> {
        serde_json::to_value(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{AgentEventKind, EventOrigin};

    // ── ATIF struct serialization ──────────────────────────────────────────

    #[test]
    fn atif_serialization_matches_schema() {
        let atif = ATIF {
            schema_version: "ATIF-v1.5".to_string(),
            session_id: "sess-test".to_string(),
            agent: AtifAgent {
                name: "querymt".to_string(),
                version: "0.1.0".to_string(),
                model_name: Some("gpt-4".to_string()),
                tool_definitions: None,
                extra: None,
            },
            steps: vec![],
            notes: Some("test notes".to_string()),
            final_metrics: None,
            continued_trajectory_ref: None,
            extra: None,
        };

        let json = serde_json::to_value(&atif).unwrap();
        assert_eq!(json["schema_version"], "ATIF-v1.5");
        assert_eq!(json["session_id"], "sess-test");
        assert_eq!(json["agent"]["name"], "querymt");
        assert_eq!(json["agent"]["version"], "0.1.0");
        assert_eq!(json["agent"]["model_name"], "gpt-4");
        assert_eq!(json["notes"], "test notes");
    }

    #[test]
    fn atif_from_empty_events_produces_valid_atif() {
        let events: Vec<AgentEvent> = vec![];
        let options = AtifExportOptions::default();
        let builder = ATIFBuilder::new("sess-empty".to_string(), &options);

        let mut builder_mut = builder;
        builder_mut.process_events(&events);
        let atif = builder_mut.build();

        assert_eq!(atif.session_id, "sess-empty");
        assert_eq!(atif.agent.name, "querymt");
        assert!(atif.steps.is_empty());
    }

    #[test]
    fn atif_from_events_with_tool_calls_produces_correct_steps() {
        let events = vec![
            AgentEvent {
                seq: 1,
                timestamp: 1234567890,
                session_id: "sess-1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::PromptReceived {
                    content: "test prompt".to_string(),
                    message_id: None,
                },
            },
            AgentEvent {
                seq: 2,
                timestamp: 1234567891,
                session_id: "sess-1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::LlmRequestStart { message_count: 1 },
            },
            AgentEvent {
                seq: 3,
                timestamp: 1234567892,
                session_id: "sess-1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::ToolCallStart {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "read_file".to_string(),
                    arguments: r#"{"path":"test.txt"}"#.to_string(),
                },
            },
            AgentEvent {
                seq: 4,
                timestamp: 1234567893,
                session_id: "sess-1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::LlmRequestEnd {
                    usage: None,
                    tool_calls: 1,
                    finish_reason: None,
                    cost_usd: None,
                    cumulative_cost_usd: None,
                    context_tokens: 100,
                    metrics: crate::events::ExecutionMetrics { steps: 1, turns: 1 },
                },
            },
        ];

        let options = AtifExportOptions::default();
        let mut builder = ATIFBuilder::new("sess-1".to_string(), &options);
        builder.process_events(&events);
        let atif = builder.build();

        // Should have user step + agent step
        assert_eq!(atif.steps.len(), 2);
        assert_eq!(atif.steps[0].source, AtifSource::User);
        assert_eq!(atif.steps[1].source, AtifSource::Agent);
    }

    #[test]
    fn atif_tool_definition_type_field_renamed() {
        let tool_def = AtifToolDefinition {
            type_field: "function".to_string(),
            function: AtifFunctionDef {
                name: "test_tool".to_string(),
                description: "A test tool".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        };

        let json = serde_json::to_value(&tool_def).unwrap();
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "test_tool");
    }

    #[test]
    fn atif_final_metrics_optional_fields() {
        let metrics = AtifFinalMetrics {
            total_prompt_tokens: Some(100),
            total_completion_tokens: Some(50),
            total_cached_tokens: None,
            total_cost_usd: Some(0.01),
            total_steps: Some(5),
            extra: None,
        };

        let json = serde_json::to_value(&metrics).unwrap();
        assert_eq!(json["total_prompt_tokens"], 100);
        assert_eq!(json["total_completion_tokens"], 50);
        assert!(json.get("total_cached_tokens").is_none());
        assert_eq!(json["total_cost_usd"], 0.01);
        assert_eq!(json["total_steps"], 5);
    }

    #[test]
    fn atif_source_serializes_as_lowercase() {
        assert_eq!(
            serde_json::to_string(&AtifSource::System).unwrap(),
            r#""system""#
        );
        assert_eq!(
            serde_json::to_string(&AtifSource::User).unwrap(),
            r#""user""#
        );
        assert_eq!(
            serde_json::to_string(&AtifSource::Agent).unwrap(),
            r#""agent""#
        );
    }

    #[test]
    fn atif_step_with_tool_calls_and_observation() {
        let step = AtifStep {
            step_id: 1,
            timestamp: Some("2024-01-01T00:00:00Z".to_string()),
            source: AtifSource::Agent,
            model_name: Some("gpt-4".to_string()),
            reasoning_effort: None,
            message: "Calling tool".to_string(),
            reasoning_content: None,
            tool_calls: Some(vec![AtifToolCall {
                tool_call_id: "call-1".to_string(),
                function_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "test.txt"}),
            }]),
            observation: Some(AtifObservation {
                results: vec![AtifObservationResult {
                    source_call_id: Some("call-1".to_string()),
                    content: Some("file contents".to_string()),
                    subagent_trajectory_ref: None,
                }],
            }),
            metrics: None,
            extra: None,
        };

        let json = serde_json::to_value(&step).unwrap();
        assert_eq!(json["step_id"], 1);
        assert_eq!(json["source"], "agent");
        assert!(json["tool_calls"].is_array());
        assert!(json["observation"].is_object());
    }

    #[test]
    fn atif_builder_with_tool_definitions() {
        let options = AtifExportOptions::default();
        let builder = ATIFBuilder::new("sess-1".to_string(), &options);

        let tools = vec![querymt::chat::Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: "test_tool".to_string(),
                description: "Test".to_string(),
                parameters: serde_json::json!({}),
            },
        }];

        let builder = builder.with_tool_definitions(tools);
        let atif = builder.build();

        assert!(atif.agent.tool_definitions.is_some());
        assert_eq!(atif.agent.tool_definitions.unwrap().len(), 1);
    }

    #[test]
    fn atif_to_json_produces_valid_json_string() {
        let atif = ATIF {
            schema_version: "ATIF-v1.5".to_string(),
            session_id: "sess-json".to_string(),
            agent: AtifAgent {
                name: "querymt".to_string(),
                version: "0.1.0".to_string(),
                model_name: None,
                tool_definitions: None,
                extra: None,
            },
            steps: vec![],
            notes: None,
            final_metrics: None,
            continued_trajectory_ref: None,
            extra: None,
        };

        let json_str = atif.to_json().unwrap();
        assert!(json_str.contains("ATIF-v1.5"));
        assert!(json_str.contains("sess-json"));

        // Verify it's valid JSON by parsing it back
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["schema_version"], "ATIF-v1.5");
    }

    #[test]
    fn atif_builder_custom_agent_name_and_version() {
        let options = AtifExportOptions {
            agent_name: Some("custom-agent".to_string()),
            agent_version: Some("2.0.0".to_string()),
            notes: Some("custom notes".to_string()),
        };

        let builder = ATIFBuilder::new("sess-custom".to_string(), &options);
        let atif = builder.build();

        assert_eq!(atif.agent.name, "custom-agent");
        assert_eq!(atif.agent.version, "2.0.0");
        assert_eq!(atif.notes, Some("custom notes".to_string()));
    }

    #[test]
    fn atif_metrics_accumulation() {
        let events = vec![
            AgentEvent {
                seq: 1,
                timestamp: 1234567890,
                session_id: "sess-1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::LlmRequestStart { message_count: 1 },
            },
            AgentEvent {
                seq: 2,
                timestamp: 1234567891,
                session_id: "sess-1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::LlmRequestEnd {
                    usage: Some(querymt::Usage {
                        input_tokens: 100,
                        output_tokens: 50,
                        reasoning_tokens: 0,
                        cache_read: 0,
                        cache_write: 0,
                    }),
                    tool_calls: 0,
                    finish_reason: None,
                    cost_usd: Some(0.01),
                    cumulative_cost_usd: Some(0.01),
                    context_tokens: 150,
                    metrics: crate::events::ExecutionMetrics { steps: 1, turns: 1 },
                },
            },
        ];

        let options = AtifExportOptions::default();
        let mut builder = ATIFBuilder::new("sess-1".to_string(), &options);
        builder.process_events(&events);
        let atif = builder.build();

        let metrics = atif.final_metrics.unwrap();
        assert_eq!(metrics.total_prompt_tokens, Some(100));
        assert_eq!(metrics.total_completion_tokens, Some(50));
        assert_eq!(metrics.total_cost_usd, Some(0.01));
    }
}
