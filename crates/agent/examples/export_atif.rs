//! Example demonstrating ATIF trajectory export
//!
//! This example shows how to export an agent session to ATIF format.

use querymt_agent::events::{AgentEvent, AgentEventKind, ExecutionMetrics};
use querymt_agent::export::{ATIFBuilder, AtifExportOptions};

fn main() {
    // Create a simple example trajectory with mock events
    let events = vec![
        AgentEvent {
            seq: 1,
            timestamp: 1704067200, // 2024-01-01 00:00:00 UTC
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::SessionCreated,
        },
        AgentEvent {
            seq: 2,
            timestamp: 1704067201,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::PromptReceived {
                content: "What is the current price of GOOGL stock?".to_string(),
                message_id: None,
            },
        },
        AgentEvent {
            seq: 3,
            timestamp: 1704067202,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::LlmRequestStart { message_count: 2 },
        },
        AgentEvent {
            seq: 4,
            timestamp: 1704067203,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::AssistantMessageStored {
                content: "I'll look up the current stock price for you.".to_string(),
                thinking: None,
                message_id: None,
            },
        },
        AgentEvent {
            seq: 5,
            timestamp: 1704067204,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::ToolCallStart {
                tool_call_id: "call_abc123".to_string(),
                tool_name: "search_stock_price".to_string(),
                arguments: r#"{"ticker": "GOOGL"}"#.to_string(),
            },
        },
        AgentEvent {
            seq: 6,
            timestamp: 1704067205,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::ToolCallEnd {
                tool_call_id: "call_abc123".to_string(),
                tool_name: "search_stock_price".to_string(),
                is_error: false,
                result: "GOOGL is currently trading at $142.50".to_string(),
            },
        },
        AgentEvent {
            seq: 7,
            timestamp: 1704067206,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::LlmRequestEnd {
                usage: Some(querymt::Usage {
                    input_tokens: 150,
                    output_tokens: 45,
                    ..Default::default()
                }),
                tool_calls: 1,
                finish_reason: Some(querymt::chat::FinishReason::Stop),
                cost_usd: Some(0.00123),
                cumulative_cost_usd: Some(0.00123),
                context_tokens: 195,
                metrics: ExecutionMetrics { steps: 1, turns: 1 },
            },
        },
        AgentEvent {
            seq: 8,
            timestamp: 1704067207,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::LlmRequestStart { message_count: 4 },
        },
        AgentEvent {
            seq: 9,
            timestamp: 1704067208,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::AssistantMessageStored {
                content: "The current price of Alphabet (GOOGL) is $142.50.".to_string(),
                thinking: None,
                message_id: None,
            },
        },
        AgentEvent {
            seq: 10,
            timestamp: 1704067209,
            session_id: "example-session-123".to_string(),
            kind: AgentEventKind::LlmRequestEnd {
                usage: Some(querymt::Usage {
                    input_tokens: 180,
                    output_tokens: 25,
                    ..Default::default()
                }),
                tool_calls: 0,
                finish_reason: Some(querymt::chat::FinishReason::Stop),
                cost_usd: Some(0.00078),
                cumulative_cost_usd: Some(0.00201),
                context_tokens: 205,
                metrics: ExecutionMetrics { steps: 2, turns: 1 },
            },
        },
    ];

    // Create export options
    let options = AtifExportOptions {
        agent_name: Some("querymt-example".to_string()),
        agent_version: Some("0.1.0".to_string()),
        notes: Some("Example ATIF export demonstrating stock price query".to_string()),
    };

    // Build the trajectory
    let mut builder = ATIFBuilder::new("example-session-123".to_string(), &options);
    builder.process_events(&events);
    let trajectory = builder.build();

    // Export to JSON
    match trajectory.to_json() {
        Ok(json) => {
            println!("ATIF Trajectory Export:");
            println!("{}", json);
            println!("\n✓ Successfully exported trajectory to ATIF v1.5 format");
        }
        Err(e) => {
            eprintln!("Error exporting trajectory: {}", e);
        }
    }
}
