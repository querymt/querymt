use crate::agent::execution::Stage;
use crate::middleware::{
    AgentStats, LlmResponse, PreparedModelRequest, Stop, ToolResult, WaitCondition, WaitReason,
};
use crate::test_utils::mock_tool_call;
use querymt::chat::{FinishReason, ToolResultPart};
use querymt_agent_core::events::StopType;
use std::sync::Arc;

#[test]
fn stage_names_all_variants() {
    let response = Box::new(LlmResponse::new(
        "".to_string(),
        vec![],
        None,
        Some(FinishReason::Stop),
    ));
    let stages = vec![
        (Stage::BeforeRequest, "BeforeRequest"),
        (
            Stage::CallModel(Box::new(PreparedModelRequest {
                messages: Arc::from([]),
                tools: Arc::from([]),
                offered_tools: Arc::from([]),
                estimated_tokens: 0,
                options: Default::default(),
            })),
            "CallModel",
        ),
        (Stage::AfterReply(response), "AfterReply"),
        (
            Stage::ToolCalls {
                calls: Vec::new(),
                results: Vec::new(),
            },
            "ToolCalls",
        ),
        (
            Stage::Waiting(WaitCondition::delegation("del-1".to_string())),
            "Waiting",
        ),
        (Stage::Ending, "Ending"),
        (
            Stage::Stopping(Stop::new(StopType::Other, "done")),
            "Stopping",
        ),
    ];

    for (stage, expected) in stages {
        assert_eq!(<&'static str>::from(&stage), expected);
    }
}

#[test]
fn test_llm_response_has_tool_calls() {
    let no_tools = LlmResponse::new("ok".to_string(), vec![], None, Some(FinishReason::Stop));
    assert!(!no_tools.has_tool_calls());

    let with_tools = LlmResponse::new(
        "ok".to_string(),
        vec![mock_tool_call("call-1", "tool", "{}")],
        None,
        Some(FinishReason::ToolCalls),
    );
    assert!(with_tools.has_tool_calls());
}

#[test]
fn test_tool_result_with_snapshot() {
    let result = ToolResult::new(
        "call-1".to_string(),
        vec![ToolResultPart::text("ok")],
        false,
        Some("tool".to_string()),
        Some("{}".to_string()),
    );

    assert!(result.snapshot_part.is_none());

    let updated = result.with_snapshot(querymt_agent_core::model::MessagePart::Snapshot {
        root_hash: querymt_agent_core::hash::RapidHash::new(b"hash"),
        changed_paths: querymt_agent_index::merkle::DiffPaths::default(),
    });

    assert!(updated.snapshot_part.is_some());
}

#[test]
fn test_wait_condition_merge() {
    let conditions = vec![
        WaitCondition::delegation("del-1".to_string()),
        WaitCondition::delegation("del-2".to_string()),
    ];
    let merged = WaitCondition::merge(conditions).unwrap();

    assert_eq!(merged.reason, WaitReason::Delegation);
    assert_eq!(merged.correlation_ids.len(), 2);
    assert!(merged.correlation_ids.contains(&"del-1".to_string()));
    assert!(merged.correlation_ids.contains(&"del-2".to_string()));

    assert!(WaitCondition::merge(vec![]).is_none());
}

#[test]
fn test_agent_stats_default() {
    let stats = AgentStats::default();
    assert_eq!(stats.steps, 0);
    assert_eq!(stats.turns, 0);
    assert_eq!(stats.total_input_tokens, 0);
    assert_eq!(stats.total_output_tokens, 0);
    assert_eq!(stats.context_tokens, 0);
}

// Helper functions moved to test_utils module

#[test]
fn recorded_costs_view_reports_components_without_repricing() {
    use querymt_agent_services::accounting::{ComponentSums, ScopeTotals};
    // Token counters at one rate must not influence the monetary view.
    let mut stats = AgentStats {
        total_input_tokens: 1_000_000,
        total_output_tokens: 1_000_000,
        ..Default::default()
    };
    let totals: ScopeTotals = serde_json::from_value(serde_json::json!({
        "known_subtotal_usd": 0.0057652,
        "budget_known_subtotal_usd": 0.0057652,
        "components": ComponentSums {
            input_usd: 0.00014,
            output_usd: 0.0056,
            cache_read_usd: 0.0000252,
            cache_write_usd: 0.0,
            cache_write_1h_usd: 0.0,
        },
        "priced_attempts": 1, "excluded_attempts": 0, "unresolved_attempts": 0,
        "incomplete_attempts": 0, "budget_incomplete_attempts": 0,
        "settled_reasons": [],
    }))
    .unwrap();
    stats.apply_recorded_costs(&totals);
    assert!((stats.input_cost_usd - 0.00014).abs() < 1e-12);
    assert!((stats.output_cost_usd - 0.0056).abs() < 1e-12);
    assert!((stats.cache_read_cost_usd - 0.0000252).abs() < 1e-12);
    assert!((stats.total_cost_usd - 0.0057652).abs() < 1e-12);

    // Applying an empty scope clears the view (a new cycle starts at zero).
    stats.apply_recorded_costs(&ScopeTotals::default());
    assert_eq!(stats.total_cost_usd, 0.0);
    assert_eq!(stats.output_cost_usd, 0.0);
}
