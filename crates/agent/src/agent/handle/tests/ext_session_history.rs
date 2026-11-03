use super::*;
use crate::events::AgentEventKind;

async fn session_with_runs(f: &RealStorageHandleFixture, runs: usize) -> String {
    let session = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            std::path::PathBuf::new(),
        ))
        .await
        .unwrap();
    let id = session.session_id.to_string();
    let sink = f.handle.config.event_sink.clone();
    for n in 0..runs {
        sink.emit_durable(
            &id,
            AgentEventKind::RunStarted {
                run_id: format!("r{n}"),
                origin: "interactive".into(),
            },
        )
        .await
        .unwrap();
        sink.emit_durable(
            &id,
            AgentEventKind::PromptReceived {
                content: format!("prompt {n}"),
                message_id: None,
            },
        )
        .await
        .unwrap();
        sink.emit_durable(
            &id,
            AgentEventKind::ToolCallEnd {
                tool_call_id: format!("t{n}"),
                tool_name: "shell".into(),
                is_error: false,
                result: format!("result {n} {}", "x".repeat(40 * 1024)),
            },
        )
        .await
        .unwrap();
    }
    id
}

async fn call(
    f: &RealStorageHandleFixture,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, agent_client_protocol::Error> {
    f.handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            method,
            raw_params(&params.to_string()),
        ))
        .await
        .map(|r| serde_json::from_str(r.0.get()).unwrap())
}

#[tokio::test]
async fn paging_to_the_start_returns_every_event_once_in_order() {
    let f = RealStorageHandleFixture::new().await;
    let id = session_with_runs(&f, 7).await;
    let window = serde_json::json!({"maxTurns": 2, "maxBytes": 4194304, "truncateBytes": 1024});
    let first = crate::session::load_snapshot::load_session_snapshot_windowed(
        &f.handle,
        f.storage.view_store().unwrap(),
        &id,
        serde_json::from_value(window.clone()).ok(),
    )
    .await
    .unwrap();
    // The load closes orphaned runs, so read the journal after it.
    let total = f
        .storage
        .event_journal()
        .load_session_stream(&id, None, None)
        .await
        .unwrap();
    let mut seqs: Vec<i64> = first.audit.events.iter().map(|e| e.seq).collect();
    let mut before = first.history.unwrap().before;
    let mut pages = 0;
    while let Some(token) = before.take() {
        let page = call(
            &f,
            "querymt/session/history",
            serde_json::json!({"sessionId": id, "before": token, "loadWindow": window}),
        )
        .await
        .unwrap();
        let mut older: Vec<i64> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_i64().unwrap())
            .collect();
        assert!(older.last().unwrap() < seqs.first().unwrap());
        older.extend(seqs);
        seqs = older;
        before = page["history"]["before"].as_str().map(str::to_owned);
        assert_eq!(before.is_none(), page["history"]["complete"] == true);
        pages += 1;
        assert!(pages < 20);
    }
    assert!(pages >= 2);
    assert_eq!(seqs, total.iter().map(|e| e.stream_seq).collect::<Vec<_>>());
}

#[tokio::test]
async fn undo_invalidates_an_old_history_token() {
    let f = RealStorageHandleFixture::new().await;
    let id = session_with_runs(&f, 5).await;
    let snapshot = crate::session::load_snapshot::load_session_snapshot_windowed(
        &f.handle,
        f.storage.view_store().unwrap(),
        &id,
        Some(crate::session::history_window::LoadWindow {
            max_turns: 1,
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    let token = snapshot.history.unwrap().before.unwrap();
    let params = serde_json::json!({"sessionId": id, "before": token});
    call(&f, "querymt/session/history", params.clone())
        .await
        .unwrap();
    f.storage
        .event_journal()
        .delete_session_events_from(&id, 7)
        .await
        .unwrap();
    let error = call(&f, "querymt/session/history", params)
        .await
        .unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "history_changed");
}

#[tokio::test]
async fn history_and_tool_content_errors_are_typed() {
    let f = RealStorageHandleFixture::new().await;
    let error = call(
        &f,
        "querymt/session/history",
        serde_json::json!({"sessionId": "nope", "before": "0.5"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "session_not_found");
    let id = session_with_runs(&f, 1).await;
    let error = call(
        &f,
        "querymt/session/toolContent",
        serde_json::json!({"sessionId": id, "toolCallId": "missing", "part": "result"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "tool_call_not_found");
    let error = call(
        &f,
        "querymt/session/toolContent",
        serde_json::json!({"sessionId": "nope", "toolCallId": "t0", "part": "result"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "session_not_found");
}

#[tokio::test]
async fn tool_content_returns_the_complete_result() {
    let f = RealStorageHandleFixture::new().await;
    let id = session_with_runs(&f, 2).await;
    let content = call(
        &f,
        "querymt/session/toolContent",
        serde_json::json!({"sessionId": id, "toolCallId": "t1", "part": "result"}),
    )
    .await
    .unwrap();
    assert_eq!(content["toolCallId"], "t1");
    assert_eq!(
        content["content"].as_str().unwrap().len(),
        "result 1 ".len() + 40 * 1024
    );
}
