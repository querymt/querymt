//! Export functionality for agent trajectories in various formats.
//!
//! This module provides support for exporting agent session data to standardized
//! trajectory formats, enabling interoperability with other agent systems and
//! training pipelines.
//!
//! - [`atif`]: Agent Trajectory Interchange Format (ATIF v1.5)
//! - [`sft`]: SFT training data export (OpenAI chat / ShareGPT JSONL)
//! - [`turns`]: Shared turn materialization from event streams
//!
//! # Lossy by design
//!
//! Exports are **portable projections**, built from UI event streams rather
//! than canonical history. For item-aware (structured) turns they preserve
//! visible text, reasoning summaries, and tool call identities, but they
//! intentionally omit provider-only continuation state (encrypted reasoning,
//! signatures, opaque items) and cannot be used to restore complete
//! continuation. Authorized persistence (session storage) remains the only
//! lossless record.

pub mod atif;
pub mod sft;
pub mod turns;

pub use atif::{
    ATIF, ATIFBuilder, AtifAgent, AtifExportOptions, AtifFinalMetrics, AtifMetrics,
    AtifObservation, AtifObservationResult, AtifSource, AtifStep, AtifSubagentTrajectoryRef,
    AtifToolCall, AtifToolDefinition,
};

/// Export a session (including its child sessions) as ATIF, built from the
/// audit view of `view_store`.
pub async fn session_atif(
    view_store: &dyn crate::session::projection::ViewStore,
    session_id: &str,
    options: &AtifExportOptions,
) -> crate::session::error::SessionResult<ATIF> {
    let audit_view = view_store.get_audit_view(session_id, true).await?;
    Ok(ATIFBuilder::from_audit_view(&audit_view, options).build())
}

pub use sft::{SessionFilter, SftExportOptions, SftExportStats, SftFormat};
pub use turns::{SessionMeta, Turn, materialize_turns};

#[cfg(test)]
mod session_atif_tests {
    use super::*;
    use crate::session::projection::{EventJournal, NewDurableEvent, ViewStore};
    use crate::session::store::SessionStore;
    use querymt_agent_core::events::{AgentEventKind, EventOrigin, ExecutionMetrics};
    use querymt_agent_sqlite::session::sqlite_storage::SqliteStorage;

    fn ev(session_id: &str, kind: AgentEventKind) -> NewDurableEvent {
        NewDurableEvent {
            session_seq: None,
            session_id: session_id.to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            source_node_id: None,
            source_seq: None,
            kind,
        }
    }

    /// Replace volatile values (journal timestamps, generated session ids) so
    /// the export can be compared with a checked-in golden file.
    fn normalize(atif: &ATIF, ids: &[(&str, &str)]) -> serde_json::Value {
        let mut text = atif.to_json().unwrap();
        for (id, label) in ids {
            text = text.replace(id, label);
        }
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        fn mask(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, child) in map.iter_mut() {
                        if k == "timestamp" && child.is_string() {
                            *child = serde_json::Value::String("<timestamp>".into());
                        } else {
                            mask(child);
                        }
                    }
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(mask),
                _ => {}
            }
        }
        mask(&mut value);
        value
    }

    async fn fixture() -> (SqliteStorage, String, String) {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let parent = storage
            .create_session(Some("parent".into()), None, None, None)
            .await
            .unwrap()
            .public_id;
        let child = storage
            .create_session(Some("child".into()), None, Some(parent.clone()), None)
            .await
            .unwrap()
            .public_id;
        let journal: &dyn EventJournal = &storage;
        let metrics = ExecutionMetrics { steps: 1, turns: 1 };
        let events = vec![
            ev(
                &parent,
                AgentEventKind::ProviderChanged {
                    provider: "anthropic".into(),
                    model: "claude-opus-4-6".into(),
                    config_id: 1,
                    context_limit: Some(200_000),
                    provider_node_id: None,
                },
            ),
            ev(
                &parent,
                AgentEventKind::PromptReceived {
                    content: "read test.rs".into(),
                    message_id: None,
                },
            ),
            ev(
                &parent,
                AgentEventKind::LlmRequestStart { message_count: 1 },
            ),
            ev(
                &parent,
                AgentEventKind::ToolCallStart {
                    tool_call_id: "call-1".into(),
                    tool_name: "read_tool".into(),
                    arguments: r#"{"path":"test.rs"}"#.into(),
                },
            ),
            ev(
                &parent,
                AgentEventKind::ToolCallEnd {
                    tool_call_id: "call-1".into(),
                    tool_name: "read_tool".into(),
                    is_error: false,
                    result: "fn main() {}".into(),
                },
            ),
            ev(
                &parent,
                AgentEventKind::AssistantMessageStored {
                    content: "It is an empty main.".into(),
                    thinking: None,
                    reasoning_parts: Vec::new(),
                    message_id: None,
                },
            ),
            ev(
                &parent,
                AgentEventKind::LlmRequestEnd {
                    usage: None,
                    tool_calls: 1,
                    finish_reason: Some(querymt::chat::FinishReason::Stop),
                    cost_usd: None,
                    cumulative_cost_usd: None,
                    context_tokens: 50,
                    metrics,
                    accounting: None,
                    prefix_change: None,
                    idle_secs: None,
                },
            ),
            ev(
                &child,
                AgentEventKind::PromptReceived {
                    content: "child task".into(),
                    message_id: None,
                },
            ),
            ev(
                &child,
                AgentEventKind::AssistantMessageStored {
                    content: "child done".into(),
                    thinking: None,
                    reasoning_parts: Vec::new(),
                    message_id: None,
                },
            ),
            ev(
                &child,
                AgentEventKind::LlmRequestEnd {
                    usage: None,
                    tool_calls: 0,
                    finish_reason: Some(querymt::chat::FinishReason::Stop),
                    cost_usd: None,
                    cumulative_cost_usd: None,
                    context_tokens: 10,
                    metrics: ExecutionMetrics::default(),
                    accounting: None,
                    prefix_change: None,
                    idle_secs: None,
                },
            ),
        ];
        for event in &events {
            journal.append_durable(event).await.unwrap();
        }
        (storage, parent, child)
    }

    fn options() -> AtifExportOptions {
        AtifExportOptions {
            agent_name: Some("golden".into()),
            agent_version: Some("0.0.0".into()),
            notes: Some("fixture".into()),
        }
    }

    #[tokio::test]
    async fn session_atif_matches_previous_view_store_output() {
        let (storage, parent, child) = fixture().await;
        let view_store: &dyn ViewStore = &storage;
        let new = session_atif(view_store, &parent, &options()).await.unwrap();
        let actual = normalize(&new, &[(&parent, "PARENT"), (&child, "CHILD")]);

        let golden: serde_json::Value =
            serde_json::from_str(include_str!("testdata/session_atif_golden.json")).unwrap();
        assert_eq!(actual, golden);
    }
}
