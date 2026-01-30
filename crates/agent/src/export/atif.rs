//! ATIF (Agent Trajectory Interchange Format) v1.5 implementation.
//!
//! This module implements the ATIF specification for exporting agent trajectories
//! in a standardized JSON format. See: https://github.com/laude-institute/harbor

use crate::events::{AgentEvent, AgentEventKind};
use crate::session::projection::AuditView;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    pub reasoning_effort: Option<String>,
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
    /// Custom agent version (defaults to env!("CARGO_PKG_VERSION"))
    pub agent_version: Option<String>,
    /// Custom notes to include in trajectory
    pub notes: Option<String>,
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
    // Pending tool call tracking
    pending_tool_calls: HashMap<String, (String, String)>, // tool_call_id -> (tool_name, arguments)
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
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
            model_name: None,
            tool_definitions: None,
            steps: Vec::new(),
            notes: options.notes.clone(),
            current_step_id: 1,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_cached_tokens: 0,
            total_cost_usd: 0.0,
            pending_tool_calls: HashMap::new(),
        }
    }

    /// Build trajectory from AuditView
    pub fn from_audit_view(view: &AuditView, options: &AtifExportOptions) -> Self {
        let mut builder = Self::new(view.session_id.clone(), options);

        // Extract model name from events if available
        for event in &view.events {
            if let AgentEventKind::ProviderChanged { model, .. } = &event.kind {
                builder.model_name = Some(model.clone());
                break;
            }
        }

        // Process all events to build steps
        builder.process_events(&view.events);

        builder
    }

    /// Set tool definitions
    pub fn with_tool_definitions(mut self, tools: Vec<querymt::chat::Tool>) -> Self {
        self.tool_definitions = Some(
            tools
                .into_iter()
                .map(|t| AtifToolDefinition {
                    type_field: t.tool_type,
                    function: AtifFunctionDef {
                        name: t.function.name,
                        description: t.function.description,
                        parameters: t.function.parameters,
                    },
                })
                .collect(),
        );
        self
    }

    /// Process a sequence of events into steps
    pub fn process_events(&mut self, events: &[AgentEvent]) {
        let mut i = 0;
        while i < events.len() {
            let event = &events[i];
            match &event.kind {
                // System prompts or middleware injections -> system step
                AgentEventKind::SessionCreated => {
                    // Skip, this is metadata
                }
                AgentEventKind::SessionConfigured { .. } => {
                    // Skip, this is metadata
                }
                AgentEventKind::ToolsAvailable { tools, .. } => {
                    // Use first ToolsAvailable event if we don't have tools yet
                    if self.tool_definitions.is_none() {
                        self.tool_definitions = Some(
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
                                .collect(),
                        );
                    }
                    // Note: ATIF spec implies static tools per trajectory
                    // If tools change mid-session, we keep the initial set
                    // Could optionally track changes in trajectory.extra
                }
                AgentEventKind::MiddlewareInjected { message } => {
                    self.add_system_step(event.timestamp, message.clone());
                }

                // User messages -> user step
                AgentEventKind::PromptReceived { content, .. }
                | AgentEventKind::UserMessageStored { content } => {
                    self.add_user_step(event.timestamp, content.clone());
                }

                // LLM request/response -> agent step
                AgentEventKind::LlmRequestStart { .. } => {
                    // Find the corresponding LlmRequestEnd
                    if let Some(end_idx) = self.find_llm_request_end(events, i) {
                        self.process_llm_turn(events, i, end_idx);
                        i = end_idx; // Skip to end of this turn
                    }
                }

                // Track tool calls for observation correlation
                AgentEventKind::ToolCallStart {
                    tool_call_id,
                    tool_name,
                    arguments,
                } => {
                    self.pending_tool_calls
                        .insert(tool_call_id.clone(), (tool_name.clone(), arguments.clone()));
                }

                _ => {
                    // Other events (delegations, etc.) can be processed as needed
                }
            }
            i += 1;
        }
    }

    /// Find the LlmRequestEnd that matches a LlmRequestStart
    fn find_llm_request_end(&self, events: &[AgentEvent], start_idx: usize) -> Option<usize> {
        for (idx, event) in events.iter().enumerate().skip(start_idx + 1) {
            if matches!(event.kind, AgentEventKind::LlmRequestEnd { .. }) {
                return Some(idx);
            }
        }
        None
    }

    /// Process a complete LLM turn (from LlmRequestStart to LlmRequestEnd)
    fn process_llm_turn(&mut self, events: &[AgentEvent], start_idx: usize, end_idx: usize) {
        let end_event = &events[end_idx];

        // Extract metrics from LlmRequestEnd
        let (metrics, model_override) =
            if let AgentEventKind::LlmRequestEnd {
                usage, cost_usd, ..
            } = &end_event.kind
            {
                let metrics = usage.as_ref().map(|u| AtifMetrics {
                    prompt_tokens: Some(u.input_tokens),
                    completion_tokens: Some(u.output_tokens),
                    cached_tokens: None, // TODO: Add cache token tracking to Usage struct
                    cost_usd: *cost_usd,
                    prompt_token_ids: None,
                    completion_token_ids: None,
                    logprobs: None,
                    extra: None,
                });

                // Update totals
                if let Some(ref m) = metrics {
                    self.total_prompt_tokens += m.prompt_tokens.unwrap_or(0);
                    self.total_completion_tokens += m.completion_tokens.unwrap_or(0);
                    self.total_cached_tokens += m.cached_tokens.unwrap_or(0);
                    self.total_cost_usd += m.cost_usd.unwrap_or(0.0);
                }

                (metrics, None)
            } else {
                (None, None)
            };

        // Collect assistant message and tool calls from events in this turn
        let mut message_text = String::new();
        let reasoning_content: Option<String> = None;
        let mut tool_calls: Vec<AtifToolCall> = Vec::new();
        let mut observation_results: Vec<AtifObservationResult> = Vec::new();

        // Look for AssistantMessageStored and ToolCall events
        for event in &events[start_idx..=end_idx] {
            match &event.kind {
                AgentEventKind::AssistantMessageStored { content, .. } => {
                    message_text.push_str(content);
                }
                AgentEventKind::ToolCallStart {
                    tool_call_id,
                    tool_name,
                    arguments,
                } => {
                    // Parse arguments as JSON
                    let args = serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
                    tool_calls.push(AtifToolCall {
                        tool_call_id: tool_call_id.clone(),
                        function_name: tool_name.clone(),
                        arguments: args,
                    });
                }
                AgentEventKind::ToolCallEnd {
                    tool_call_id,
                    result,
                    is_error,
                    ..
                } => {
                    observation_results.push(AtifObservationResult {
                        source_call_id: Some(tool_call_id.clone()),
                        content: Some(if *is_error {
                            format!("ERROR: {}", result)
                        } else {
                            result.clone()
                        }),
                        subagent_trajectory_ref: None,
                    });
                }
                AgentEventKind::DelegationCompleted {
                    delegation_id,
                    result,
                } => {
                    // If this delegation was part of a tool call, add as subagent ref
                    observation_results.push(AtifObservationResult {
                        source_call_id: None, // TODO: correlate with tool call if applicable
                        content: result.clone(),
                        subagent_trajectory_ref: Some(vec![AtifSubagentTrajectoryRef {
                            session_id: delegation_id.clone(),
                            trajectory_path: None, // Could be populated if stored
                            extra: None,
                        }]),
                    });
                }
                _ => {}
            }
        }

        // Create the agent step
        let observation = if !observation_results.is_empty() {
            Some(AtifObservation {
                results: observation_results,
            })
        } else {
            None
        };

        let tool_calls_opt = if !tool_calls.is_empty() {
            Some(tool_calls)
        } else {
            None
        };

        self.steps.push(AtifStep {
            step_id: self.current_step_id,
            timestamp: Some(self.format_timestamp(end_event.timestamp)),
            source: AtifSource::Agent,
            model_name: model_override.or_else(|| self.model_name.clone()),
            reasoning_effort: None, // TODO: could extract from events if available
            message: message_text,
            reasoning_content,
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
