//! Handler for knowledge store read-only queries.

use super::super::ServerState;
use super::super::connection::{send_error, send_message};
use super::super::messages::{ConsolidationInfo, KnowledgeEntryInfo, UiServerMessage};
use crate::knowledge::{KnowledgeEntry, KnowledgeFilter, QueryOpts};
use tokio::sync::mpsc;

/// Convert a domain `KnowledgeEntry` to a UI `KnowledgeEntryInfo` DTO.
fn entry_to_info(e: &KnowledgeEntry) -> KnowledgeEntryInfo {
    let fmt = &time::format_description::well_known::Rfc3339;
    KnowledgeEntryInfo {
        public_id: e.public_id.clone(),
        scope: e.scope.clone(),
        source: e.source.clone(),
        summary: e.summary.clone(),
        entities: e.entities.clone(),
        topics: e.topics.clone(),
        importance: e.importance,
        consolidated_at: e.consolidated_at.and_then(|t| t.format(fmt).ok()),
        created_at: e.created_at.format(fmt).unwrap_or_default(),
    }
}

/// Handle `QueryKnowledge` — search the knowledge store.
pub async fn handle_query_knowledge(
    state: &ServerState,
    scope: &str,
    question: &str,
    limit: Option<u32>,
    tx: &mpsc::Sender<String>,
) {
    let store = match state.agent.config.knowledge_store() {
        Some(s) => s,
        None => {
            let _ = send_error(tx, "Knowledge store not available".to_string()).await;
            return;
        }
    };

    let opts = QueryOpts {
        limit: limit.map(|l| l as usize).unwrap_or(20),
        ..Default::default()
    };

    match store.query(scope, question, opts).await {
        Ok(result) => {
            let entries: Vec<KnowledgeEntryInfo> =
                result.entries.iter().map(entry_to_info).collect();
            let consolidations: Vec<ConsolidationInfo> = result
                .consolidations
                .iter()
                .map(|c| {
                    let fmt = &time::format_description::well_known::Rfc3339;
                    ConsolidationInfo {
                        public_id: c.public_id.clone(),
                        scope: c.scope.clone(),
                        summary: c.summary.clone(),
                        insight: c.insight.clone(),
                        source_count: c.source_entry_public_ids.len() as u32,
                        created_at: c.created_at.format(fmt).unwrap_or_default(),
                    }
                })
                .collect();
            let _ = send_message(
                tx,
                UiServerMessage::KnowledgeQueryResult {
                    entries,
                    consolidations,
                },
            )
            .await;
        }
        Err(e) => {
            let _ = send_error(tx, format!("Knowledge query failed: {}", e)).await;
        }
    }
}

/// Handle `ListKnowledge` — list entries for a scope with optional filter.
pub async fn handle_list_knowledge(
    state: &ServerState,
    scope: &str,
    filter_json: Option<&serde_json::Value>,
    tx: &mpsc::Sender<String>,
) {
    let store = match state.agent.config.knowledge_store() {
        Some(s) => s,
        None => {
            let _ = send_error(tx, "Knowledge store not available".to_string()).await;
            return;
        }
    };

    let filter: KnowledgeFilter = filter_json
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    match store.list(scope, filter).await {
        Ok(entries) => {
            let infos: Vec<KnowledgeEntryInfo> = entries.iter().map(entry_to_info).collect();
            let _ = send_message(tx, UiServerMessage::KnowledgeListResult { entries: infos }).await;
        }
        Err(e) => {
            let _ = send_error(tx, format!("Knowledge list failed: {}", e)).await;
        }
    }
}

/// Handle `KnowledgeStats` — get stats for a scope.
pub async fn handle_knowledge_stats(state: &ServerState, scope: &str, tx: &mpsc::Sender<String>) {
    let store = match state.agent.config.knowledge_store() {
        Some(s) => s,
        None => {
            let _ = send_error(tx, "Knowledge store not available".to_string()).await;
            return;
        }
    };

    match store.stats(scope).await {
        Ok(stats) => {
            let fmt = &time::format_description::well_known::Rfc3339;
            let _ = send_message(
                tx,
                UiServerMessage::KnowledgeStatsResult {
                    total_entries: stats.total_entries,
                    unconsolidated_entries: stats.unconsolidated_entries,
                    total_consolidations: stats.total_consolidations,
                    latest_entry_at: stats.latest_entry_at.and_then(|t| t.format(fmt).ok()),
                    latest_consolidation_at: stats
                        .latest_consolidation_at
                        .and_then(|t| t.format(fmt).ok()),
                },
            )
            .await;
        }
        Err(e) => {
            let _ = send_error(tx, format!("Knowledge stats failed: {}", e)).await;
        }
    }
}
