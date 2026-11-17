use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use qmacp::{client::AcpClient, error::CliError, policy::PermissionPolicy, url};

use crate::{
    commands::{self, Command},
    output::{self, EventLog, OperationState},
};

/// ACP connection settings shared by both MCP transports.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    url: String,
    endpoint: String,
    permission: PermissionPolicy,
}

impl BridgeConfig {
    /// Normalize and validate the ACP endpoint before starting either transport.
    pub fn new(endpoint: &str, allow_insecure: bool, permission: PermissionPolicy) -> Result<Self> {
        let url = url::normalize_acp_ws_url(endpoint, allow_insecure)?;
        let endpoint = url::safe_endpoint_label(&url)?;
        Ok(Self {
            url,
            endpoint,
            permission,
        })
    }
}

/// MCP server exposing qmacp operations over native ACP WebSocket connections.
#[derive(Clone)]
pub struct QmcpServer {
    config: Arc<BridgeConfig>,
    tools: Arc<Vec<Tool>>,
}

impl QmcpServer {
    /// Build the tool catalog without connecting to or mutating the ACP server.
    pub fn new(config: BridgeConfig) -> Result<Self> {
        Ok(Self {
            config: Arc::new(config),
            tools: Arc::new(tool_catalog()?),
        })
    }

    async fn execute(&self, command: Command, context: &RequestContext<RoleServer>) -> Value {
        self.execute_with_cancellation(command, context.ct.cancelled())
            .await
    }

    async fn execute_with_cancellation(
        &self,
        command: Command,
        cancelled: impl std::future::Future<Output = ()>,
    ) -> Value {
        // This absolute deadline includes the ACP handshake, not just RPC execution.
        let deadline = command
            .timeout()
            .map(|timeout| tokio::time::Instant::now() + timeout);
        let expired = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(cancelled, expired);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let client = tokio::select! {
            biased;
            _ = &mut cancelled => return output::failure(request_cancelled()),
            _ = &mut expired => return output::failure(commands::operation_timeout()),
            connected = AcpClient::connect_with_events(&self.config.url, &self.config.endpoint, self.config.permission, event_tx) => {
                match connected {
                    Ok(client) => client,
                    Err(error) => return output::failure(CliError::connection_with_cause(&self.config.endpoint, error)),
                }
            }
        };
        let mut log = EventLog::default();
        let mut state = OperationState::default();
        let result = {
            let operation = commands::run(&client, command, &mut state, deadline);
            tokio::pin!(operation);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut cancelled => break output::failure(request_cancelled()),
                    result = &mut operation => break result,
                    Some(event) = event_rx.recv() => log.push(event),
                }
            }
        };
        while let Ok(event) = event_rx.try_recv() {
            log.push(event);
        }
        let mut result = result;
        let truncated = result["textTruncated"] == true || client.text_truncated().await;
        result["textTruncated"] = json!(truncated);
        log.attach(state.finish(result))
    }
}

impl ServerHandler for QmcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("qmcp", env!("CARGO_PKG_VERSION")))
            .with_instructions("Control an existing QueryMT ACP WebSocket server. Discover capabilities, profiles, models, and session ids rather than inventing values. Keep returned sessionId values. Pass cwd for session operations. Timeouts and MCP cancellation do not cancel remote work: check runtime before retrying. Only close sessions when explicitly requested. Streaming ACP events are collected in each tool result.")
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if request.and_then(|r| r.cursor).is_some() {
            return Err(ErrorData::invalid_params(
                "the tool catalog is not paginated",
                None,
            ));
        }
        Ok(ListToolsResult {
            tools: self.tools.as_ref().clone(),
            ..Default::default()
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::method_not_found::<
                rmcp::model::CallToolRequestMethod,
            >());
        }
        let result = match commands::decode(&request.name, request.arguments.unwrap_or_default()) {
            Ok(command) => self.execute(command, &context).await,
            Err(error) => output::failure(CliError::Usage(format!("{error:#}"))),
        };
        let result = output::normalize(result);
        let response = if output::failed(&result) {
            CallToolResult::structured_error(result)
        } else {
            CallToolResult::structured(result)
        };
        Ok(CallToolResponse::Complete(response))
    }
}

fn request_cancelled() -> CliError {
    CliError::Timeout("MCP request cancelled; the remote turn is not automatically cancelled; inspect runtime before retrying".into())
}

fn tool_catalog() -> Result<Vec<Tool>> {
    let inputs = commands::input_schemas()?;
    let mut tools = Vec::with_capacity(inputs.len());
    for (name, mut input) in inputs {
        let description = input
            .remove("description")
            .and_then(|v| v.as_str().map(str::to_owned))
            .context("tool must have a description")?;
        let read_only = matches!(
            name.as_str(),
            "caps"
                | "profiles"
                | "models"
                | "sessions"
                | "find"
                | "get_lifecycle"
                | "inspect"
                | "status"
                | "runtime"
                | "follow"
                | "watch"
        );
        let idempotent = read_only
            || matches!(
                name.as_str(),
                "set_lifecycle" | "pin" | "unpin" | "set_mode" | "set_model" | "set_effort"
            );
        let annotations = ToolAnnotations::new()
            .read_only(read_only)
            .idempotent(idempotent)
            .open_world(true);
        tools.push(Tool::new(name, description, input).with_annotations(annotations));
    }
    if tools.is_empty() {
        bail!("empty tool catalog");
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_during_connection_setup_releases_the_socket_promptly() {
        use std::time::Duration;
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/acp/ws", listener.local_addr().unwrap());
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            ready_tx.send(()).unwrap();
            let mut rest = Vec::new();
            socket.read_to_end(&mut rest).await.unwrap();
        });
        let server =
            QmcpServer::new(BridgeConfig::new(&url, false, PermissionPolicy::Cancel).unwrap())
                .unwrap();
        let cancel = async {
            ready_rx.await.unwrap();
        };
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            server.execute_with_cancellation(Command::Caps {}, cancel),
        )
        .await
        .expect("cancellation must not wait for the ACP connection timeout");
        assert_eq!(result["ok"], false);
        assert_eq!(result["code"], 4);
        assert!(result["error"].as_str().unwrap().contains("cancelled"));
        tokio::time::timeout(Duration::from_secs(1), peer)
            .await
            .expect("cancelled handshake socket must close")
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_keeps_batch_results_and_acknowledged_submission_recovery() {
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio_tungstenite::{accept_async, tungstenite::Message};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/acp/ws", listener.local_addr().unwrap());
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(socket).await.unwrap();
            let mut input_id = Value::Null;
            let mut ready = Some(ready_tx);
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap();
                let result = match method {
                    "initialize" => {
                        json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true, "sessionCapabilities": {"resume": {}}}})
                    }
                    "session/resume" | "session/load" => json!({}),
                    "_querymt/session/runtimeState" if input_id.is_null() => {
                        json!({"phase": "tools", "steerable": true})
                    }
                    "_querymt/session/runtimeState" => {
                        ready.take().unwrap().send(()).unwrap();
                        continue;
                    }
                    "_querymt/session/queue" => {
                        input_id = request["params"]["client_input_id"].clone();
                        json!({"status": "queued", "data": {"input_id": input_id, "position": 1}})
                    }
                    "session/cancel" | "session/close" => {
                        panic!("MCP cancellation must not cancel or close remote work")
                    }
                    _ => panic!("unexpected fixture request: {request}"),
                };
                socket
                    .send(Message::Text(
                        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
            input_id
        });
        let server =
            QmcpServer::new(BridgeConfig::new(&url, false, PermissionPolicy::Cancel).unwrap())
                .unwrap();
        let command = commands::decode("exec", json!({"calls": [
            {"name": "runtime", "arguments": {"session_id": "fixture-session"}},
            {"name": "prompt", "arguments": {"session_id": "fixture-session", "delivery": "queue", "text": "next"}},
            {"name": "close", "arguments": {"session_id": "fixture-session"}},
        ]}).as_object().unwrap().clone()).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            server.execute_with_cancellation(command, async {
                ready_rx.await.unwrap();
            }),
        )
        .await
        .expect("cancellation after submission must not wait for runtime polling");
        assert_eq!(result["ok"], false);
        assert_eq!(result["code"], 4);
        assert_eq!(result["failedCall"], "prompt");
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["result"]["ok"], true);
        assert_eq!(result["recovery"]["submission"]["state"], "acknowledged");
        assert_eq!(result["recovery"]["deliveryState"], "accepted");
        let input_id = tokio::time::timeout(Duration::from_secs(1), peer)
            .await
            .expect("the cancelled call must drop its ACP client")
            .unwrap();
        assert!(input_id.is_string());
        assert_eq!(result["recovery"]["submission"]["clientInputId"], input_id);
        assert_eq!(
            result["recovery"]["submission"]["acknowledgement"]["input_id"],
            input_id
        );
    }

    #[test]
    fn advertised_schemas_match_decoder_boundaries() {
        let tools = tool_catalog().unwrap();
        let validators: std::collections::HashMap<_, _> = tools
            .iter()
            .map(|tool| {
                let schema = json!(tool.input_schema);
                (
                    tool.name.as_ref(),
                    jsonschema::validator_for(&schema).unwrap(),
                )
            })
            .collect();
        let check = |name: &str, arguments: Value| {
            let schema_accepts = validators[name].is_valid(&arguments);
            let decoder_accepts =
                commands::decode(name, arguments.as_object().unwrap().clone()).is_ok();
            assert_eq!(
                schema_accepts, decoder_accepts,
                "schema/decoder disagree for {name}: {arguments}"
            );
        };
        for new in [
            None,
            Some(Value::Null),
            Some(json!(false)),
            Some(json!(true)),
        ] {
            for session in [None, Some(Value::Null), Some(json!("s")), Some(json!(""))] {
                let mut args = json!({"text": "literal"});
                if let Some(value) = new.clone() {
                    args["new"] = value;
                }
                if let Some(value) = session {
                    args["session_id"] = value;
                }
                check("prompt", args);
            }
        }
        for resolved in [
            None,
            Some(Value::Null),
            Some(json!(false)),
            Some(json!(true)),
        ] {
            for archived in [
                None,
                Some(Value::Null),
                Some(json!(false)),
                Some(json!(true)),
            ] {
                let mut args = json!({"session_id": "s"});
                if let Some(value) = resolved.clone() {
                    args["resolved"] = value;
                }
                if let Some(value) = archived {
                    args["archived"] = value;
                }
                check("set_lifecycle", args);
            }
        }
        for name in ["prompt", "follow", "watch"] {
            for value in [
                Value::Null,
                json!(0),
                json!(1),
                json!(86400),
                json!(86401),
                json!(1.5),
                json!("1"),
                json!(u64::MAX),
            ] {
                let mut args = if name == "prompt" {
                    json!({"new": true, "text": "x"})
                } else {
                    json!({"session_id": "s"})
                };
                args["timeout"] = value.clone();
                check(name, args);
                if name != "prompt" {
                    check(name, json!({"session_id": "s", "interval": value}));
                }
            }
        }
        for text in [
            "",
            " ",
            "\t\n",
            "\u{85}\u{a0}\u{3000}",
            "\u{feff}",
            "\u{200b}",
            "x",
            "\n-x\n",
            "-",
            "\u{3000}x",
        ] {
            check("prompt", json!({"new": true, "text": text}));
            check("steer", json!({"session_id": "s", "text": text}));
            check("queue", json!({"session_id": "s", "text": text}));
        }
        check("exec", json!({"calls": []}));
        check("exec", json!({"calls": vec![json!({"name": "caps"}); 100]}));
        check("exec", json!({"calls": vec![json!({"name": "caps"}); 101]}));
        for entry in [
            json!({"name": "caps"}),
            json!({"name": "profiles", "arguments": {}}),
            json!({"name": "caps", "arguments": {"extra": true}}),
            json!({"name": "runtime"}),
            json!({"name": "runtime", "arguments": {"session_id": "s"}}),
            json!({"name": "exec", "arguments": {"calls": [{"name": "caps"}]}}),
            json!({"name": "does_not_exist"}),
            json!({"name": "caps", "extra": true}),
            json!({"name": "prompt", "arguments": {"new": true, "text": "x"}}),
            json!({"name": "prompt", "arguments": {"new": true, "session_id": "s", "text": "x"}}),
            json!({"name": "set_lifecycle", "arguments": {"session_id": "s", "resolved": false}}),
        ] {
            check("exec", json!({"calls": [entry]}));
        }
        check("prompt", json!({"new": true, "text": "x", "extra": true}));
        check("caps", json!({"extra": true}));
    }

    #[test]
    fn catalog_contains_self_contained_object_schemas_and_native_prompt_fields() {
        let tools = tool_catalog().unwrap();
        assert_eq!(tools.len(), 25);
        for tool in &tools {
            assert_eq!(tool.input_schema["type"], "object");
            assert!(
                !tool.input_schema["properties"]
                    .as_object()
                    .unwrap()
                    .contains_key("command")
            );
            assert!(
                !serde_json::to_string(&tool.input_schema)
                    .unwrap()
                    .contains("\"$ref\"")
            );
        }
        let prompt = tools.iter().find(|t| t.name == "prompt").unwrap();
        assert_eq!(prompt.input_schema["properties"]["text"]["type"], "string");
        assert_eq!(
            prompt.input_schema["properties"]["until_idle"]["default"],
            true
        );
    }
}
