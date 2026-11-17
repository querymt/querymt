//! All write-like RPCs in these tests go to disposable in-memory mock servers.

use std::process::{Output, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio_tungstenite::{accept_async, tungstenite::Message};

enum Reply {
    Result(Value),
    WithNotifications(Vec<Value>, Value),
    Error(i64, &'static str),
    Disconnect,
    Pending,
}

struct MockServer {
    url: String,
    task: JoinHandle<Vec<Value>>,
}

impl MockServer {
    async fn start(mut respond: impl FnMut(&Value) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/acp/ws", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut requests = Vec::new();
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let reply = if request["method"] == "initialize" {
                    Reply::Result(json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {"loadSession": true},
                        "agentInfo": {"name": "mock", "version": "1"},
                    }))
                } else {
                    respond(&request)
                };
                let response = match reply {
                    Reply::Result(result) => json!({
                        "jsonrpc": "2.0", "id": request["id"], "result": result,
                    }),
                    Reply::WithNotifications(notifications, result) => {
                        for notification in notifications {
                            socket
                                .send(Message::Text(notification.to_string().into()))
                                .await
                                .unwrap();
                        }
                        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                    }
                    Reply::Error(code, message) => json!({
                        "jsonrpc": "2.0", "id": request["id"],
                        "error": {"code": code, "message": message},
                    }),
                    Reply::Disconnect => {
                        requests.push(request);
                        break;
                    }
                    Reply::Pending => {
                        requests.push(request);
                        continue;
                    }
                };
                requests.push(request);
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
            requests
        });
        Self { url, task }
    }

    async fn finish(self) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
    }
}

fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qmacp"));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn invoke(command: &mut Command, input: Option<&str>) -> Output {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await.unwrap();
        drop(stdin);
    }
    tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

fn events(output: &Output) -> Vec<Value> {
    let text = std::str::from_utf8(&output.stdout).unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn runtime(phase: &str, steerable: bool) -> Reply {
    Reply::Result(json!({
        "phase": phase, "steerable": steerable, "active_run_id": "run-1",
        "observation": {"availability": "live", "epoch": "actor-1", "revision": 1},
    }))
}

fn accepted(request: &Value) -> Reply {
    Reply::Result(json!({
        "status": if request["method"] == "_querymt/session/steer" { "steered" } else { "queued" },
        "data": { "input_id": request["params"]["client_input_id"], "position": 1, "run_id": "run-1" },
    }))
}

fn unexpected(request: &Value) -> Reply {
    panic!("unexpected RPC: {}", request["method"])
}

#[tokio::test]
async fn queue_and_steer_preserve_explicit_stdin_payloads_without_loading_history() {
    for method in ["queue", "steer"] {
        let wire = format!("_querymt/session/{method}");
        let server = MockServer::start(move |request| {
            if request["method"] == wire {
                accepted(request)
            } else if request["method"] == "_querymt/session/runtimeState" {
                runtime("tools", true)
            } else {
                unexpected(request)
            }
        })
        .await;
        let payload = "---\ncallback_id: test\n---\nLiteral $HOME and 'quotes'\n";
        let output = invoke(
            cli().args(["--url", &server.url, "--quiet", method, "session-1", "-"]),
            Some(payload),
        )
        .await;
        assert!(output.status.success(), "{output:?}");
        let result = events(&output).pop().unwrap();
        assert_eq!(result["deliveryState"], "accepted");
        let requests = server.finish().await;
        let submit = requests.last().unwrap();
        assert_eq!(submit["params"]["session_id"], "session-1");
        assert_eq!(submit["params"]["prompt"][0]["text"], payload);
        assert_eq!(submit["params"]["client_input_id"], result["clientInputId"]);
        if method == "steer" {
            assert_eq!(submit["params"]["expected_run_id"], "run-1");
        }
    }
}

#[tokio::test]
async fn nonwaiting_prompts_emit_receipts_without_resume_load_or_prompt_rpcs() {
    for method in ["queue", "steer"] {
        let wire = format!("_querymt/session/{method}");
        let server = MockServer::start(move |request| {
            if request["method"] == wire {
                accepted(request)
            } else if request["method"] == "_querymt/session/runtimeState" {
                runtime("tools", true)
            } else {
                unexpected(request)
            }
        })
        .await;
        let output = invoke(
            cli().args([
                "--url",
                &server.url,
                "--quiet",
                "prompt",
                "--delivery",
                method,
                "--until-idle",
                "false",
                "session-1",
                "-",
            ]),
            Some("-"),
        )
        .await;
        assert!(output.status.success(), "{output:?}");
        let events = events(&output);
        let receipt = events.iter().find(|e| e["type"] == "submitted").unwrap();
        let done = events.last().unwrap();
        assert_eq!(done["type"], "done");
        assert_eq!(done["observationState"], "not_waited");
        assert_eq!(
            done["stopReason"],
            if method == "steer" {
                "steered"
            } else {
                "queued"
            }
        );
        assert_eq!(receipt["clientInputId"], done["clientInputId"]);
        let requests = server.finish().await;
        assert_eq!(requests.last().unwrap()["params"]["prompt"][0]["text"], "-");
    }
}

#[tokio::test]
async fn auto_delivery_to_a_busy_session_also_avoids_history_loading() {
    let server = MockServer::start(|request| match request["method"].as_str().unwrap() {
        "_querymt/session/runtimeState" => runtime("tools", true),
        "_querymt/session/steer" => accepted(request),
        _ => unexpected(request),
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "prompt",
            "--until-idle",
            "false",
            "session-1",
            "callback",
        ]),
        None,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(events(&output).last().unwrap()["delivery"], "steer");
    server.finish().await;
}

#[tokio::test]
async fn rejected_and_uncertain_submissions_keep_the_generated_input_id() {
    for (reply, state, code) in [
        (0, "unknown", 2),
        (-32602, "rejected", 3),
        (-32603, "unknown", 3),
    ] {
        let server = MockServer::start(move |request| {
            assert_eq!(request["method"], "_querymt/session/queue");
            if reply == 0 {
                Reply::Disconnect
            } else {
                Reply::Error(reply, "submit failed")
            }
        })
        .await;
        let output = invoke(
            cli().args([
                "--url",
                &server.url,
                "--quiet",
                "queue",
                "session-1",
                "work",
            ]),
            None,
        )
        .await;
        assert_eq!(output.status.code(), Some(code));
        let result = events(&output).pop().unwrap();
        assert_eq!(result["recovery"]["deliveryState"], state);
        assert_eq!(result["recovery"]["stage"], "submit");
        let requests = server.finish().await;
        assert_eq!(
            result["recovery"]["clientInputId"],
            requests.last().unwrap()["params"]["client_input_id"]
        );
    }
}

#[tokio::test]
async fn new_and_prompt_new_preserve_creation_on_configuration_failure() {
    for prompt in [false, true] {
        let server = MockServer::start(|request| match request["method"].as_str().unwrap() {
            "session/new" => Reply::Result(json!({"sessionId": "created-session"})),
            "session/set_mode" => Reply::Error(-32602, "invalid mode"),
            _ => unexpected(request),
        })
        .await;
        let mut command = cli();
        command.args(["--url", &server.url, "--quiet"]);
        if prompt {
            command.args(["prompt", "--new"]);
        } else {
            command.arg("new");
        }
        command.args(["--mode", "invalid"]);
        if prompt {
            command.arg("work");
        }
        let output = invoke(&mut command, None).await;
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        let events = events(&output);
        if prompt {
            assert_eq!(events[0]["type"], "session");
            assert_eq!(events[0]["sessionId"], "created-session");
        } else {
            assert_eq!(events.len(), 1);
        }
        let recovery = &events.last().unwrap()["recovery"];
        assert_eq!(recovery["sessionId"], "created-session");
        assert_eq!(recovery["creationState"], "created");
        assert_eq!(recovery["deliveryState"], "not_submitted");
        assert_eq!(recovery["stage"], "configure");
        server.finish().await;
    }
}

#[tokio::test]
async fn a_lost_creation_response_is_not_reported_as_no_session_created() {
    let server = MockServer::start(|request| {
        assert_eq!(request["method"], "session/new");
        Reply::Disconnect
    })
    .await;
    let output = invoke(cli().args(["--url", &server.url, "--quiet", "new"]), None).await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        events(&output).last().unwrap()["recovery"]["creationState"],
        "unknown"
    );
    server.finish().await;
}

#[tokio::test]
async fn stdin_exec_batches_reject_payload_reads_before_any_earlier_write() {
    for commands in [
        "queue worker-a first\nqueue worker-b -\n",
        "queue worker-a first\nexec 'queue worker-b -'\n",
    ] {
        let server = MockServer::start(unexpected).await;
        let output = invoke(
            cli().args(["--url", &server.url, "--quiet", "exec"]),
            Some(commands),
        )
        .await;
        assert_eq!(output.status.code(), Some(1));
        assert!(server.finish().await.is_empty());
    }
}

#[tokio::test]
async fn blank_payloads_are_usage_errors_without_submission() {
    for (args, input) in [
        (vec!["queue", "session-1", "-"], Some(" \n")),
        (vec!["prompt", "session-1", " \n"], None),
    ] {
        let server = MockServer::start(unexpected).await;
        let output = invoke(
            cli().args(["--url", &server.url, "--quiet"]).args(args),
            input,
        )
        .await;
        assert_eq!(output.status.code(), Some(1));
        let error = events(&output).pop().unwrap();
        assert_eq!(error["code"], 1);
        assert!(server.finish().await.is_empty());
    }
}

fn listed(id: &str) -> Value {
    json!({"sessionId": id, "cwd": "/workspace", "title": id})
}

#[tokio::test]
async fn find_reports_the_page_cap_instead_of_silent_not_found() {
    let mut page = 0;
    let server = MockServer::start(move |request| {
        assert_eq!(request["method"], "session/list");
        if page > 0 {
            assert_eq!(request["params"]["cursor"], page.to_string());
        }
        page += 1;
        Reply::Result(json!({"sessions": [], "nextCursor": page.to_string()}))
    })
    .await;
    let output = invoke(
        cli().args(["--url", &server.url, "--quiet", "find", "--query", "absent"]),
        None,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let result = events(&output).pop().unwrap();
    assert_eq!(result["sessions"], json!([]));
    assert_eq!(result["pagesScanned"], qmacp::session::find_page_cap());
    assert_eq!(result["pageCapReached"], true);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["stopReason"], "page_cap");
    let requests = server.finish().await;
    let pages = requests
        .iter()
        .filter(|request| request["method"] == "session/list")
        .count();
    assert_eq!(result["pagesScanned"], pages);
}

#[tokio::test]
async fn new_returns_one_result_or_an_opt_in_creation_stream() {
    for stream in [false, true] {
        let server = MockServer::start(|request| match request["method"].as_str().unwrap() {
            "session/new" => Reply::Result(json!({"sessionId": "created-session"})),
            "session/resume" | "session/load" => Reply::Result(json!({})),
            _ => unexpected(request),
        })
        .await;
        let mut command = cli();
        command.args(["--url", &server.url, "--quiet", "new"]);
        if stream {
            command.arg("--stream");
        }
        let output = invoke(&mut command, None).await;
        assert!(output.status.success(), "{output:?}");
        let result = if stream {
            let events = events(&output);
            assert_eq!(events[0]["type"], "session");
            assert_eq!(events.last().unwrap()["type"], "done");
            events.last().unwrap().clone()
        } else {
            serde_json::from_slice::<Value>(&output.stdout).expect("exactly one JSON value")
        };
        assert_eq!(result["sessionId"], "created-session");
        assert_eq!(result["ok"], true);
        server.finish().await;
    }
}

#[tokio::test]
async fn idle_auto_delivery_opens_the_session_before_prompting() {
    let mut opened = false;
    let server = MockServer::start(move |request| match request["method"].as_str().unwrap() {
        "_querymt/session/runtimeState" => runtime("idle", false),
        "session/resume" | "session/load" => {
            opened = true;
            Reply::Result(json!({}))
        }
        "session/prompt" => {
            assert!(opened, "idle delivery must open its target session");
            Reply::Result(json!({"stopReason": "end_turn"}))
        }
        _ => unexpected(request),
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "prompt",
            "--until-idle",
            "false",
            "session-1",
            "work",
        ]),
        None,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let done = events(&output).pop().unwrap();
    assert_eq!(done["delivery"], "prompt");
    assert_eq!(done["observationState"], "prompt_returned");
    server.finish().await;
}

#[tokio::test]
async fn creation_and_submission_receipts_are_flushed_before_pending_work_finishes() {
    for creation in [false, true] {
        let server = MockServer::start(move |request| match request["method"].as_str().unwrap() {
            "session/new" if creation => Reply::Result(json!({"sessionId": "created-session"})),
            "session/set_mode" if creation => Reply::Pending,
            "_querymt/session/queue" if !creation => accepted(request),
            "_querymt/session/runtimeState" if !creation => Reply::Pending,
            _ => unexpected(request),
        })
        .await;
        let mut command = cli();
        command.args(["--url", &server.url, "--quiet"]);
        if creation {
            command.args(["new", "--stream", "--mode", "build"]);
        } else {
            command.args(["prompt", "--delivery", "queue", "session-1", "work"]);
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let expected = if creation { "session" } else { "submitted" };
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let line = lines.next_line().await.unwrap().expect("early event");
                let event: Value = serde_json::from_str(&line).unwrap();
                if event["type"] == expected {
                    break;
                }
            }
        })
        .await
        .expect("receipt must arrive before the blocked operation completes");
        assert!(child.try_wait().unwrap().is_none());
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        server.finish().await;
    }
}

#[tokio::test]
async fn prompt_new_preserves_creation_when_prompt_delivery_becomes_uncertain() {
    let server = MockServer::start(|request| match request["method"].as_str().unwrap() {
        "session/new" => Reply::Result(json!({"sessionId": "created-session"})),
        "session/prompt" => Reply::Disconnect,
        _ => unexpected(request),
    })
    .await;
    let output = invoke(
        cli().args(["--url", &server.url, "--quiet", "prompt", "--new", "work"]),
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    let error = events(&output).pop().unwrap();
    assert_eq!(error["recovery"]["sessionId"], "created-session");
    assert_eq!(error["recovery"]["creationState"], "created");
    assert_eq!(error["recovery"]["deliveryState"], "unknown");
    assert_eq!(error["recovery"]["stage"], "submit");
    server.finish().await;
}

#[tokio::test]
async fn find_reports_exhaustion_after_following_the_next_cursor() {
    let mut page = 0;
    let server = MockServer::start(move |request| {
        assert_eq!(request["method"], "session/list");
        page += 1;
        if page == 1 {
            Reply::Result(json!({"sessions": [listed("other")], "nextCursor": page.to_string()}))
        } else {
            Reply::Result(json!({"sessions": [listed("wanted")]}))
        }
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "find",
            "--query",
            "wanted",
            "--limit",
            "1",
        ]),
        None,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let result = events(&output).pop().unwrap();
    assert_eq!(result["sessions"][0]["sessionId"], "wanted");
    assert_eq!(result["pagesScanned"], 2);
    assert_eq!(result["pageCapReached"], false);
    assert_eq!(result["truncated"], false);
    assert_eq!(result["stopReason"], "exhausted");
    server.finish().await;
}

#[tokio::test]
async fn malformed_runtime_never_defaults_to_idle_and_sends_a_prompt() {
    for state in [json!({}), json!({"phase": "tools"})] {
        let server = MockServer::start(move |request| {
            assert_eq!(request["method"], "_querymt/session/runtimeState");
            Reply::Result(state.clone())
        })
        .await;
        let output = invoke(
            cli().args([
                "--url",
                &server.url,
                "--quiet",
                "prompt",
                "--until-idle",
                "false",
                "target",
                "work",
            ]),
            None,
        )
        .await;
        assert_eq!(output.status.code(), Some(3));
        let error = events(&output).pop().unwrap();
        assert_eq!(error["recovery"]["deliveryState"], "not_submitted");
        assert_eq!(server.finish().await.len(), 2);
    }
}

#[tokio::test]
async fn steer_without_an_active_run_does_not_submit() {
    let server = MockServer::start(|request| {
        assert_eq!(request["method"], "_querymt/session/runtimeState");
        Reply::Result(json!({"phase": "idle", "steerable": false, "active_run_id": null}))
    })
    .await;
    let output = invoke(
        cli().args(["--url", &server.url, "--quiet", "steer", "target", "work"]),
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        events(&output).last().unwrap()["recovery"]["deliveryState"],
        "not_submitted"
    );
    server.finish().await;
}

#[tokio::test]
async fn malformed_mismatched_or_failed_receipts_are_not_acceptance() {
    for variant in 0..4 {
        let server = MockServer::start(move |request| {
            assert_eq!(request["method"], "_querymt/session/queue");
            Reply::Result(match variant {
                0 => Value::Null,
                1 => json!({"status": "queued", "data": {"input_id": "another-input"}}),
                2 => json!({"status": "failed", "data": {"input_id": request["params"]["client_input_id"]}}),
                _ => json!({"data": {"input_id": request["params"]["client_input_id"]}}),
            })
        }).await;
        let output = invoke(
            cli().args([
                "--url",
                &server.url,
                "--quiet",
                "prompt",
                "--delivery",
                "queue",
                "--until-idle",
                "false",
                "target",
                "work",
            ]),
            None,
        )
        .await;
        assert_eq!(output.status.code(), Some(3));
        let events = events(&output);
        let error = events.last().unwrap();
        assert_eq!(error["recovery"]["deliveryState"], "unknown");
        assert!(error["recovery"]["clientInputId"].is_string());
        assert!(
            events
                .iter()
                .all(|event| event["type"] != "submitted" && event["type"] != "done")
        );
        server.finish().await;
    }
}

#[tokio::test]
async fn a_stalled_observation_times_out_with_the_accepted_receipt_intact() {
    let server = MockServer::start(|request| match request["method"].as_str().unwrap() {
        "_querymt/session/queue" => accepted(request),
        "_querymt/session/runtimeState" => Reply::Pending,
        _ => unexpected(request),
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "prompt",
            "--delivery",
            "queue",
            "--timeout",
            "1",
            "target",
            "work",
        ]),
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(4));
    let events = events(&output);
    let submitted = events
        .iter()
        .find(|event| event["type"] == "submitted")
        .unwrap();
    let recovery = &events.last().unwrap()["recovery"];
    assert_eq!(recovery["stage"], "observe");
    assert_eq!(recovery["deliveryState"], "accepted");
    assert_eq!(recovery["sessionId"], "target");
    assert_eq!(recovery["clientInputId"], submitted["clientInputId"]);
    assert_eq!(recovery["receipt"], submitted["receipt"]);
    server.finish().await;
}

#[tokio::test]
async fn idle_observation_does_not_replay_history_or_claim_task_completion() {
    let server = MockServer::start(|request| match request["method"].as_str().unwrap() {
        "_querymt/session/queue" => accepted(request),
        "_querymt/session/runtimeState" => runtime("idle", false),
        _ => unexpected(request),
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "prompt",
            "--delivery",
            "queue",
            "target",
            "work",
        ]),
        None,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let done = events(&output).pop().unwrap();
    assert_eq!(done["observationState"], "idle");
    assert_eq!(done["text"], "");
    server.finish().await;
}

fn delegation(state: &str, seq: u64) -> Value {
    json!({
        "version": 1, "sessionId": "parent", "delegationId": "delegation-1",
        "toolCallId": "dispatch-1", "childSessionId": "child-1", "targetAgentId": "coder",
        "objective": "fixture task", "state": state, "activity": "historical",
        "requestedAt": 1, "forkedAt": 2, "updatedAt": seq,
        "sourceEpoch": "workflow-1", "sessionSeq": seq,
    })
}

fn delegation_notification(method: &str, update: &Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": update})
}

fn assert_delegation_event(event: &Value, update: &Value) {
    assert_eq!(event["type"], "delegation");
    for (key, value) in update.as_object().unwrap() {
        assert_eq!(&event[key], value, "lost delegation field {key}");
    }
}

#[tokio::test]
async fn follow_streams_delegation_lifecycle_separately_from_tool_dispatch_during_load() {
    let mut verifying = delegation("forked", 3);
    verifying["activity"] = json!("live");
    verifying["stage"] = json!("verifying");
    let mut terminal = delegation("completed", 4);
    terminal["finishedAt"] = json!(4);
    terminal["resultSummary"] = json!("verified");
    let expected = [verifying.clone(), terminal.clone()];
    let server = MockServer::start(move |request| match request["method"].as_str().unwrap() {
        "session/resume" => Reply::Error(-32601, "resume unsupported"),
        "session/load" => Reply::WithNotifications(vec![
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": "parent", "update": {"sessionUpdate": "tool_call_update", "toolCallId": "dispatch-1", "status": "completed"},
            }}),
            delegation_notification("querymt/session/delegationUpdate", &verifying),
            delegation_notification("querymt/session/delegationUpdate", &terminal),
        ], json!({"_meta": {"querymt/sessionLoadSnapshot.v1": {"delegationUpdates": [verifying]}}})),
        "_querymt/session/runtimeState" => runtime("idle", false),
        _ => unexpected(request),
    }).await;
    let output = invoke(
        cli().args(["--url", &server.url, "--quiet", "follow", "parent"]),
        None,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let events = events(&output);
    let tool = events.iter().find(|event| event["type"] == "tool").unwrap();
    assert_eq!(tool["id"], "dispatch-1");
    assert_eq!(tool["status"], "completed");
    let updates: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "delegation")
        .collect();
    assert_eq!(updates.len(), 2, "{events:?}");
    for (event, update) in updates.iter().zip(&expected) {
        assert_delegation_event(event, update);
    }
    assert_eq!(events.last().unwrap()["observationState"], "idle");
    server.finish().await;
}

#[tokio::test]
async fn exported_event_channel_delivers_terminal_delegation_update_buffered_during_load() {
    let historical = delegation("forked", 2);
    let mut terminal = delegation("cancelled", 4);
    terminal["finishedAt"] = json!(4);
    terminal["error"] = json!("fixture cancellation");
    let expected = terminal.clone();
    let server = MockServer::start(move |request| {
        assert_eq!(request["method"], "session/load");
        Reply::WithNotifications(
            vec![delegation_notification("_querymt/session/delegationUpdate", &terminal)],
            json!({"_meta": {"querymt/sessionLoadSnapshot.v1": {"delegationUpdates": [historical]}}}),
        )
    }).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let client = qmacp::client::AcpClient::connect_with_events(
        &server.url,
        "fixture",
        qmacp::policy::PermissionPolicy::Cancel,
        tx,
    )
    .await
    .unwrap();
    client.initialize().await.unwrap();
    let (_, raw) = client
        .load_session("parent".into(), "/tmp".into())
        .await
        .unwrap();
    assert_eq!(
        raw["_meta"]["querymt/sessionLoadSnapshot.v1"]["delegationUpdates"][0]["state"],
        "forked"
    );
    let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_delegation_event(&event, &expected);
    drop(client);
    server.finish().await;
}

fn enriched_remote_load(update: &Value) -> Value {
    let activity = if update["activity"] == "live" {
        vec![update.clone()]
    } else {
        Vec::new()
    };
    json!({"_meta": {
        "location": "remote", "nodeId": "fixture-peer",
        "querymt/sessionLoadSnapshot.v1": {
            "runtimeStatus": {
                "phase": "idle", "steerable": false, "active_run_id": null,
                "observation": {"availability": "live", "epoch": "remote-actor", "revision": 9},
                "delegation_activity": activity,
            },
            "delegationUpdates": [update],
            "audit": {"events": [{"kind": {"type": "tool_call_end", "data": {
                "tool_call_id": "dispatch-1", "tool_name": "delegate", "is_error": false, "result": "dispatched",
            }}}]},
        },
    }})
}

#[tokio::test]
async fn exported_load_preserves_enriched_initial_snapshot_without_notifications() {
    let mut verifying = delegation("forked", 3);
    verifying["activity"] = json!("live");
    verifying["stage"] = json!("verifying");
    let expected = enriched_remote_load(&verifying);
    let response = expected.clone();
    let server = MockServer::start(move |request| {
        assert_eq!(request["method"], "session/load");
        Reply::Result(response.clone())
    })
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let client = qmacp::client::AcpClient::connect_with_events(
        &server.url,
        "fixture",
        qmacp::policy::PermissionPolicy::Cancel,
        tx,
    )
    .await
    .unwrap();
    client.initialize().await.unwrap();
    let (loaded, raw) = tokio::time::timeout(
        Duration::from_secs(5),
        client.load_session("parent".into(), "/tmp".into()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(loaded.meta.is_some());
    assert_eq!(raw, expected);
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    drop(client);
    let requests = server.finish().await;
    assert_eq!(
        requests.len(),
        2,
        "only initialize and load, no runtime query"
    );
}

#[tokio::test]
async fn inspect_keeps_initial_remote_delegation_separate_from_dispatch_on_reconnect() {
    for state in ["forked", "failed"] {
        let mut update = delegation(state, if state == "forked" { 3 } else { 4 });
        if state == "forked" {
            update["activity"] = json!("live");
            update["stage"] = json!("verifying");
        } else {
            update["finishedAt"] = json!(4);
            update["error"] = json!("verification failed");
        }
        let expected = enriched_remote_load(&update);
        for full in [false, true] {
            let response = expected.clone();
            let server = MockServer::start(move |request| {
                assert_eq!(request["method"], "session/load");
                Reply::Result(response.clone())
            })
            .await;
            let mut command = cli();
            command.args(["--url", &server.url, "--quiet", "inspect", "parent"]);
            if full {
                command.arg("--full");
            }
            let output = invoke(&mut command, None).await;
            assert!(output.status.success(), "{output:?}");
            let result_events = events(&output);
            assert_eq!(
                result_events.len(),
                1,
                "snapshot only, no live notifications"
            );
            let result = &result_events[0];
            if full {
                assert_eq!(result["raw"], expected);
            } else {
                assert_eq!(result["tools"][0]["id"], "dispatch-1");
                assert_eq!(result["tools"][0]["status"], "ok");
                let updates = result["delegations"]
                    .as_array()
                    .expect("compact delegation snapshot");
                assert_eq!(updates.len(), 1);
                assert_delegation_event(&updates[0], &update);
            }
            assert_eq!(server.finish().await.len(), 2, "no catchup runtime query");
        }
    }
}

#[tokio::test]
async fn unavailable_runtime_prevents_auto_delivery() {
    for state in [
        json!({"phase": "unknown", "steerable": false, "observation": {"availability": "unavailable", "reason": "owner_unavailable"}}),
        json!({"phase": "idle", "steerable": false, "observation": {"availability": "unavailable"}}),
        json!({"phase": "idle", "steerable": false}),
    ] {
        let server = MockServer::start(move |request| match request["method"].as_str().unwrap() {
            "_querymt/session/runtimeState" => Reply::Result(state.clone()),
            "session/resume" | "session/load" => Reply::Result(json!({})),
            "session/prompt" => Reply::Result(json!({"stopReason": "end_turn"})),
            "_querymt/session/queue" => accepted(request),
            _ => unexpected(request),
        })
        .await;
        let output = invoke(
            cli().args([
                "--url",
                &server.url,
                "--quiet",
                "prompt",
                "--until-idle",
                "false",
                "target",
                "work",
            ]),
            None,
        )
        .await;
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        let error = events(&output).pop().unwrap();
        assert_eq!(error["recovery"]["deliveryState"], "not_submitted");
        assert_eq!(error["recovery"]["observationState"], "unavailable");
        assert_eq!(server.finish().await.len(), 2, "must not open or submit");
    }
}

#[tokio::test]
async fn unavailable_observation_fails_follow_and_watch_instead_of_reporting_completion() {
    for command in ["follow", "watch"] {
        let state = json!({"phase": "unknown", "steerable": false, "observation": {
            "availability": "unavailable", "reason": "owner_unavailable",
        }});
        let expected = state.clone();
        let server = MockServer::start(move |request| match request["method"].as_str().unwrap() {
            "session/resume" | "session/load" => Reply::Result(json!({})),
            "_querymt/session/runtimeState" => Reply::Result(state.clone()),
            _ => unexpected(request),
        })
        .await;
        let mut cli = cli();
        cli.args([
            "--url",
            &server.url,
            "--quiet",
            command,
            "target",
            "--timeout",
            "1",
        ]);
        if command == "watch" {
            cli.args(["--idle", "false"]);
        }
        let output = invoke(&mut cli, None).await;
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        let events = events(&output);
        assert!(events.iter().all(|event| event["type"] != "done"));
        let recovery = &events.last().unwrap()["recovery"];
        assert_eq!(recovery["sessionId"], "target");
        assert_eq!(recovery["stage"], "observe");
        assert_eq!(recovery["observationState"], "unavailable");
        assert_eq!(recovery["runtime"], expected);
        server.finish().await;
    }
}

#[tokio::test]
async fn self_idle_waits_preserve_dependency_activity_without_waiting_for_children() {
    for command in ["prompt", "follow", "watch"] {
        let state = json!({
            "phase": "idle", "steerable": false, "active_run_id": null,
            "observation": {"availability": "inactive"},
            "delegation_activity": [{"delegationId": "delegate-1", "stage": "verifying", "activity": "live", "sourceEpoch": "workflow-epoch", "sessionSeq": 4}],
        });
        let expected = state.clone();
        let server = MockServer::start(move |request| match request["method"].as_str().unwrap() {
            "session/resume" | "session/load" => Reply::Result(json!({})),
            "_querymt/session/queue" => accepted(request),
            "_querymt/session/runtimeState" => Reply::Result(state.clone()),
            _ => unexpected(request),
        })
        .await;
        let mut cli = cli();
        cli.args(["--url", &server.url, "--quiet", command]);
        if command == "prompt" {
            cli.args(["--delivery", "queue", "--timeout", "1", "target", "work"]);
        } else {
            cli.args(["target", "--timeout", "1"]);
        }
        let output = invoke(&mut cli, None).await;
        assert!(output.status.success(), "{output:?}");
        let done = events(&output).pop().unwrap();
        assert_eq!(done["runtime"], expected);
        if command != "watch" {
            assert_eq!(done["observationState"], "idle");
        }
        server.finish().await;
    }
}

#[tokio::test]
async fn watch_timeout_bounds_a_stalled_runtime_request() {
    let server = MockServer::start(|request| {
        assert_eq!(request["method"], "_querymt/session/runtimeState");
        Reply::Pending
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "watch",
            "target",
            "--timeout",
            "1",
        ]),
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(
        events(&output).last().unwrap()["recovery"]["sessionId"],
        "target"
    );
    server.finish().await;
}

#[tokio::test]
async fn accepted_prompt_observation_disconnect_preserves_receipt_without_replaying() {
    let mut observed = false;
    let server = MockServer::start(move |request| match request["method"].as_str().unwrap() {
        "_querymt/session/queue" => accepted(request),
        "_querymt/session/runtimeState" if !observed => {
            observed = true;
            runtime("tools", false)
        }
        "_querymt/session/runtimeState" => Reply::Disconnect,
        _ => unexpected(request),
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "prompt",
            "--delivery",
            "queue",
            "target",
            "work",
        ]),
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let events = events(&output);
    let submitted = events
        .iter()
        .find(|event| event["type"] == "submitted")
        .unwrap();
    let recovery = &events.last().unwrap()["recovery"];
    assert_eq!(recovery["deliveryState"], "accepted");
    assert_eq!(recovery["clientInputId"], submitted["clientInputId"]);
    assert_eq!(recovery["receipt"], submitted["receipt"]);
    assert_eq!(recovery["observationState"], "unavailable");
    assert_eq!(recovery["runtime"]["active_run_id"], "run-1");
    assert_eq!(recovery["runtime"]["phase"], "tools");
    assert!(events.iter().all(|event| event["type"] != "done"));
    assert_eq!(server.finish().await.len(), 4);
}

#[tokio::test]
async fn accepted_queue_and_steer_recovery_does_not_claim_prompt_outcome() {
    for delivery in ["queue", "steer"] {
        let mut submitted = false;
        let server = MockServer::start(move |request| match request["method"].as_str().unwrap() {
            "_querymt/session/queue" | "_querymt/session/steer" => {
                submitted = true;
                accepted(request)
            }
            "_querymt/session/runtimeState" if !submitted => runtime("tools", true),
            "_querymt/session/runtimeState" => Reply::Result(json!({
                "phase": "unknown", "observation": {"availability": "unavailable", "reason": "owner_unavailable"},
            })),
            _ => unexpected(request),
        }).await;
        let output = invoke(
            cli().args([
                "--url",
                &server.url,
                "--quiet",
                "prompt",
                "--delivery",
                delivery,
                "--timeout",
                "1",
                "target",
                "work",
            ]),
            None,
        )
        .await;
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        let events = events(&output);
        let submitted = events
            .iter()
            .find(|event| event["type"] == "submitted")
            .unwrap();
        let recovery = &events.last().unwrap()["recovery"];
        assert_eq!(recovery["sessionId"], "target");
        assert_eq!(recovery["stage"], "observe");
        assert_eq!(recovery["delivery"], delivery);
        assert_eq!(recovery["deliveryState"], "accepted");
        assert_eq!(recovery["clientInputId"], submitted["clientInputId"]);
        assert_eq!(recovery["receipt"], submitted["receipt"]);
        assert_eq!(recovery["observationState"], "unavailable");
        assert_eq!(
            recovery["runtime"]["observation"]["reason"],
            "owner_unavailable"
        );
        assert!(events.iter().all(|event| event["type"] != "done"));
        assert!(
            recovery.get("stopReason").is_none(),
            "{delivery}: {recovery}"
        );
        let requests = server.finish().await;
        assert!(
            requests
                .iter()
                .all(|request| request["method"] != "session/prompt")
        );
        assert_eq!(requests.len(), if delivery == "steer" { 4 } else { 3 });
    }
}

#[tokio::test]
async fn returned_prompt_keeps_its_stop_reason_when_later_observation_fails() {
    let server = MockServer::start(|request| match request["method"].as_str().unwrap() {
        "session/new" => Reply::Result(json!({"sessionId": "created-session"})),
        "session/prompt" => Reply::Result(json!({"stopReason": "cancelled"})),
        "_querymt/session/runtimeState" => Reply::Result(json!({
            "phase": "unknown", "observation": {"availability": "unavailable"},
        })),
        _ => unexpected(request),
    })
    .await;
    let output = invoke(
        cli().args(["--url", &server.url, "--quiet", "prompt", "--new", "work"]),
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let events = events(&output);
    let recovery = &events.last().unwrap()["recovery"];
    assert_eq!(recovery["sessionId"], "created-session");
    assert_eq!(recovery["creationState"], "created");
    assert_eq!(recovery["deliveryState"], "accepted");
    assert_eq!(recovery["stopReason"], "cancelled");
    assert_eq!(recovery["observationState"], "unavailable");
    assert!(events.iter().all(|event| event["type"] != "done"));
    server.finish().await;
}

#[tokio::test]
async fn discovery_preserves_unknown_observations_and_filters_only_known_phases() {
    let remote_runtime = json!({
        "phase": "tools", "active_run_id": "remote-run",
        "observation": {"availability": "live", "epoch": "remote-actor", "revision": 9},
    });
    let listed = vec![
        json!({"sessionId": "missing", "cwd": "/tmp"}),
        json!({"sessionId": "offline", "cwd": "/tmp", "_meta": {"runtimeStatus": {
            "phase": "unknown", "observation": {"availability": "unavailable", "reason": "owner_unavailable"},
        }}}),
        json!({"sessionId": "unobserved", "cwd": "/tmp", "_meta": {"runtimeStatus": {"phase": "idle"}}}),
        json!({"sessionId": "parent", "cwd": "/tmp", "_meta": {"runtimeStatus": {
            "phase": "idle", "observation": {"availability": "inactive"},
            "delegation_activity": [{"stage": "verifying", "activity": "live"}],
        }}}),
        json!({"sessionId": "busy", "cwd": "/tmp", "_meta": {"runtimeStatus": {
            "phase": "tools", "active_run_id": "run-B", "observation": {"availability": "live", "epoch": "actor-1", "revision": 8},
        }}}),
        json!({"sessionId": "null-phase", "cwd": "/tmp", "_meta": {"runtimeStatus": {"phase": null}}}),
        json!({"sessionId": "remote-busy", "cwd": "/tmp", "_meta": {"runtimeStatus": remote_runtime}}),
    ];
    for phase in [None, Some("idle"), Some("unknown"), Some("tools")] {
        let listed = listed.clone();
        let server = MockServer::start(move |request| {
            assert_eq!(request["method"], "session/list");
            Reply::Result(json!({"sessions": listed}))
        })
        .await;
        let mut cli = cli();
        cli.args(["--url", &server.url, "--quiet"]);
        if let Some(phase) = phase {
            cli.args(["find", "--phase", phase]);
        } else {
            cli.arg("sessions");
        }
        let output = invoke(&mut cli, None).await;
        assert!(output.status.success(), "{output:?}");
        let result = events(&output).pop().unwrap();
        let sessions = result["sessions"].as_array().unwrap();
        let ids: Vec<_> = sessions
            .iter()
            .map(|session| session["sessionId"].as_str().unwrap())
            .collect();
        match phase {
            Some("idle") => assert_eq!(ids, ["parent"]),
            Some("unknown") => assert_eq!(ids, ["offline", "unobserved"]),
            Some("tools") => assert_eq!(ids, ["busy", "remote-busy"]),
            None => {
                assert!(sessions[0]["phase"].is_null());
                assert_eq!(
                    sessions[1]["runtime"]["observation"]["availability"],
                    "unavailable"
                );
                assert_eq!(
                    sessions[3]["runtime"]["delegation_activity"][0]["stage"],
                    "verifying"
                );
                assert_eq!(sessions[4]["runtime"]["active_run_id"], "run-B");
                assert_eq!(sessions[4]["runtime"]["observation"]["revision"], 8);
                assert!(sessions[5]["phase"].is_null());
                assert_eq!(sessions[5]["runtime"], json!({"phase": null}));
                assert_eq!(sessions[6]["phase"], "tools");
                assert!(sessions[6]["messageCount"].is_null());
                assert_eq!(sessions[6]["runtime"], remote_runtime);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            server.finish().await.len(),
            2,
            "listing must not issue per-session runtime queries"
        );
    }
}

#[tokio::test]
async fn cancel_reports_only_a_sent_notification_without_waiting_for_termination() {
    let server = MockServer::start(|request| {
        assert_eq!(request["method"], "session/cancel");
        assert!(
            request.get("id").is_none(),
            "cancel must remain a notification"
        );
        assert_eq!(request["params"], json!({"sessionId": "target"}));
        Reply::Pending
    })
    .await;
    let output = invoke(
        cli().args(["--url", &server.url, "--quiet", "cancel", "target"]),
        None,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let result = events(&output).pop().unwrap();
    assert_eq!(result["cancellationState"], "notification_sent");
    assert_eq!(result["sessionId"], "target");
    assert_eq!(result["ok"], true);
    assert!(result.get("cancelled").is_none());
    assert_eq!(server.finish().await.len(), 2);
}

#[tokio::test]
async fn follow_does_not_report_done_when_opening_the_session_fails() {
    let server = MockServer::start(|request| match request["method"].as_str().unwrap() {
        "session/resume" => Reply::Error(-32601, "resume unsupported"),
        "session/load" => Reply::Error(-32603, "session observation failed"),
        "_querymt/session/runtimeState" => runtime("idle", false),
        _ => unexpected(request),
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "follow",
            "target",
            "--timeout",
            "1",
        ]),
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let events = events(&output);
    assert!(events.iter().all(|event| event["type"] != "done"));
    let recovery = &events.last().unwrap()["recovery"];
    assert_eq!(recovery["sessionId"], "target");
    assert_eq!(recovery["stage"], "open");
    assert_eq!(recovery["observationState"], "unavailable");
    let requests = server.finish().await;
    assert!(
        requests
            .iter()
            .all(|request| request["method"] != "_querymt/session/runtimeState")
    );
}

#[tokio::test]
async fn find_distinguishes_the_result_limit_from_the_page_cap() {
    let cap = qmacp::session::find_page_cap();
    let mut page = 0;
    let server = MockServer::start(move |request| {
        assert_eq!(request["method"], "session/list");
        page += 1;
        Reply::Result(json!({
            "sessions": if page < cap { vec![listed("other")] }
                else { vec![listed("wanted"), listed("unscanned")] },
            "nextCursor": page.to_string(),
        }))
    })
    .await;
    let output = invoke(
        cli().args([
            "--url",
            &server.url,
            "--quiet",
            "find",
            "--query",
            "wanted",
            "--limit",
            "1",
        ]),
        None,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let result = events(&output).pop().unwrap();
    assert_eq!(result["pagesScanned"], cap);
    assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["stopReason"], "limit");
    assert_eq!(result["pageCapReached"], false);
    server.finish().await;
}
