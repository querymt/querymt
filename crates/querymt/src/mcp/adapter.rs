use crate::mcp::progress::{CURRENT_PROGRESS, McpNotificationRouter, ProgressSink, ProgressUpdate};
use crate::{
    chat::{FunctionTool, MediaKind, MediaPart, MediaSource, MediaType, Tool, ToolResultPart},
    tool_decorator::CallFunctionTool,
};
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use jsonschema::ValidationError;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelTaskParams,
    CancelledNotification, CancelledNotificationParam, ClientRequest, ContentBlock,
    CreateTaskResult, GetTaskParams, RequestId, ResourceContents, ServerResult, TaskPayload,
    Tool as RmcpTool,
};
use rmcp::service::{PeerRequestOptions, ServerSink, ServiceError};
use serde_json::Value;
use std::convert::TryFrom;
use std::{sync::Arc, time::Duration};
#[cfg(feature = "tracing")]
use tracing::instrument;

/// Error type for when the schema in the RMCP Tool doesn't match your ParametersSchema.
#[derive(thiserror::Error, Debug)]
pub enum AdapterError {
    #[error("failed to serialize RMCP schema to JSON for tool `{tool_name}`: {source}")]
    SerdeJson {
        tool_name: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to validate RMCP schema for tool `{tool_name}`: {source}")]
    ValidationError {
        tool_name: String,
        #[source] // Keep the original error!
        source: jsonschema::ValidationError<'static>,
    },
}

impl TryFrom<RmcpTool> for FunctionTool {
    type Error = AdapterError;

    #[cfg_attr(feature = "tracing", instrument(name = "from_mcp_tool", skip(r)))]
    fn try_from(r: RmcpTool) -> Result<Self, Self::Error> {
        let tool_name = r.name.to_string();
        log::debug!("adding mcp tool: {}", tool_name);

        let schema = r.schema_as_json_value();
        jsonschema::draft202012::meta::validate(&schema).map_err(|err| {
            // Because lifetimes can be tricky, we take ownership of the error
            let static_err: ValidationError<'static> = err.to_owned();
            AdapterError::ValidationError {
                tool_name: tool_name.clone(), // We need the tool_name here
                source: static_err,
            }
        })?;

        let mut description = r.description.clone().unwrap_or_default().to_string();
        if let Some(output_schema) = r.output_schema
            && let Ok(os_str) = serde_json::to_string(&output_schema)
        {
            description = format!("{}. Returns {}", description, os_str);
        }

        Ok(FunctionTool {
            name: tool_name,
            description,
            parameters: schema,
            strict: None,
        })
    }
}

impl TryFrom<RmcpTool> for Tool {
    type Error = AdapterError;

    fn try_from(r: RmcpTool) -> Result<Self, Self::Error> {
        let func = FunctionTool::try_from(r)?;
        Ok(Tool {
            tool_type: "function".to_string(), // or whatever you want here
            function: func,
        })
    }
}

/// Failure of an MCP tool call, kept typed so callers can tell an in-band
/// tool error from a transport failure.
///
/// Travels inside the `anyhow::Error` returned by
/// [`CallFunctionTool::call`]; recover it with `downcast_ref`.
#[derive(thiserror::Error, Debug)]
pub enum McpToolError {
    /// The server returned a result with `isError: true`. `parts` is the
    /// server's content, mapped exactly like a successful result.
    #[error("{}", reported_message(.parts))]
    Reported { parts: Vec<ToolResultPart> },
    /// The server asked for additional client input (MRTR), which is unsupported.
    #[error("MCP server requested additional input, which is not supported")]
    InputRequiredUnsupported,
    /// A server-side task ended `failed` or `cancelled`, or needed input.
    #[error("MCP task {status}: {message}")]
    TaskFailed { status: String, message: String },
    /// Transport or protocol failure.
    #[error(transparent)]
    Service(#[from] ServiceError),
}

/// Concatenated text parts, or a generic message when there are none.
fn reported_message(parts: &[ToolResultPart]) -> String {
    let text: String = parts
        .iter()
        .filter_map(|p| match p {
            ToolResultPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if text.is_empty() {
        "MCP tool reported an error".to_owned()
    } else {
        text
    }
}

/// Map a final `CallToolResult` to tool-result parts.
///
/// When `content` is empty and `structuredContent` is a non-null value, the
/// latter is returned as JSON text; it is never added next to existing
/// content. rmcp keeps an explicit `null` as `Some(Null)`, which is treated
/// as absent. `isError: true` yields [`McpToolError::Reported`].
fn map_call_result(result: CallToolResult) -> Result<Vec<ToolResultPart>, McpToolError> {
    let CallToolResult {
        content,
        structured_content,
        is_error,
        ..
    } = result;
    let mut parts: Vec<ToolResultPart> = content
        .into_iter()
        .map(ToolResultPart::from_mcp_content)
        .collect();
    if parts.is_empty()
        && let Some(value) = structured_content.filter(|v| !v.is_null())
    {
        parts.push(ToolResultPart::Text {
            text: value.to_string(),
        });
    }
    if is_error == Some(true) {
        Err(McpToolError::Reported { parts })
    } else {
        Ok(parts)
    }
}

/// Poll interval when the server suggests none.
const DEFAULT_TASK_POLL: Duration = Duration::from_secs(1);
/// Upper bound on any server-suggested poll interval.
const MAX_TASK_POLL: Duration = Duration::from_secs(10);

pub struct McpToolAdapter {
    mcp_tool: RmcpTool,
    server: ServerSink,
    server_name: String,
    tool: Tool,
    router: Option<McpNotificationRouter>,
}

impl McpToolAdapter {
    pub fn try_new(
        mcp_tool: RmcpTool,
        server: ServerSink,
        server_name: String,
    ) -> Result<Self, Box<AdapterError>> {
        let tool = Tool::try_from(mcp_tool.clone())?;
        Ok(Self {
            mcp_tool,
            server,
            server_name,
            tool,
            router: None,
        })
    }

    /// Attach the router fed by this connection's client handler.
    ///
    /// Progress and task-status wake-ups only work with a router, because only
    /// the owner of the connection's `ClientHandler` can forward the server's
    /// notifications. Without one, calls send no progress token and tasks are
    /// polled on their interval alone.
    #[must_use]
    pub fn with_router(mut self, router: McpNotificationRouter) -> Self {
        self.router = Some(router);
        self
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Whether the server declares this tool read-only (`readOnlyHint: true`).
    /// A missing or false hint counts as mutating.
    pub fn is_read_only(&self) -> bool {
        declares_read_only(&self.mcp_tool)
    }
}

/// `annotations.readOnlyHint == Some(true)`.
fn declares_read_only(tool: &RmcpTool) -> bool {
    tool.annotations
        .as_ref()
        .and_then(|a| a.read_only_hint)
        .unwrap_or(false)
}

/// Convert an MCP `ContentBlock` value into a canonical bounded tool-result part.
///
/// The conversion preserves each block's order and metadata. Media blocks use
/// the validated attachment model, so malformed MIME declarations fail
/// explicitly instead of being silently dropped. An unparseable blob MIME
/// falls back to an opaque attachment rather than fabricating a concrete type.
impl ToolResultPart {
    pub fn from_mcp_content(block: ContentBlock) -> Self {
        let b64 = base64::engine::general_purpose::STANDARD;
        match block {
            ContentBlock::Text(t) => ToolResultPart::Text { text: t.text },
            ContentBlock::Image(img) => attachment_part(
                MediaKind::Image,
                Some(img.mime_type),
                b64.decode(&img.data).unwrap_or_default(),
                None,
            ),
            ContentBlock::Audio(a) => attachment_part(
                MediaKind::Audio,
                Some(a.mime_type),
                b64.decode(&a.data).unwrap_or_default(),
                None,
            ),
            ContentBlock::Resource(r) => match r.resource {
                ResourceContents::TextResourceContents { text, .. } => {
                    ToolResultPart::Text { text }
                }
                ResourceContents::BlobResourceContents {
                    blob, mime_type, ..
                } => attachment_part(
                    MediaKind::Document,
                    mime_type,
                    b64.decode(&blob).unwrap_or_default(),
                    None,
                ),
                // Non-exhaustive: preserve unknown variants as JSON.
                other => ToolResultPart::Text {
                    text: serde_json::to_string(&other).unwrap_or_default(),
                },
            },
            ContentBlock::ResourceLink(r) => {
                let kind = r
                    .mime_type
                    .as_deref()
                    .and_then(media_kind_for_mime)
                    .unwrap_or(MediaKind::Other);
                let media_type = r.mime_type.as_deref().and_then(|m| m.parse().ok());
                let mut media = MediaPart::new(kind, media_type, MediaSource::Url { url: r.uri })
                    .unwrap_or_else(|_| {
                        MediaPart::new(
                            MediaKind::Other,
                            None,
                            MediaSource::Url { url: String::new() },
                        )
                        .expect("opaque URL attachment without media type is always valid")
                    });
                media.filename = Some(r.name);
                media.detail = r.description;
                ToolResultPart::Attachment(Box::new(media))
            }
            // `ContentBlock` is non-exhaustive: preserve unknown future block
            // types as their JSON form rather than silently dropping them.
            other => ToolResultPart::Text {
                text: serde_json::to_string(&other).unwrap_or_default(),
            },
        }
    }
}

/// Classify a raw MIME string into a broad media kind when possible.
fn media_kind_for_mime(mime: &str) -> Option<MediaKind> {
    let parsed: mime::Mime = mime.parse().ok()?;
    Some(match parsed.type_().as_str() {
        "image" => MediaKind::Image,
        "audio" => MediaKind::Audio,
        "video" => MediaKind::Video,
        _ => return None,
    })
}

/// Build a canonical attachment part from MCP inline bytes.
///
/// A valid declared MIME type is preserved; an invalid or missing one yields an
/// opaque attachment rather than an invalid canonical value.
fn attachment_part(
    kind: MediaKind,
    mime: Option<String>,
    data: Vec<u8>,
    filename: Option<String>,
) -> ToolResultPart {
    let media_type: Option<MediaType> = mime.as_deref().and_then(|m| m.parse().ok());
    let mut media =
        MediaPart::new(kind, media_type, MediaSource::Inline { data }).unwrap_or_else(|_| {
            MediaPart::new(
                MediaKind::Other,
                None,
                MediaSource::Url { url: String::new() },
            )
            .expect("opaque attachment without media type is always valid")
        });
    media.filename = filename;
    ToolResultPart::Attachment(Box::new(media))
}

#[async_trait]
impl CallFunctionTool for McpToolAdapter {
    fn descriptor(&self) -> Tool {
        self.tool.clone()
    }

    fn server_name(&self) -> Option<&str> {
        Some(&self.server_name)
    }

    /// Calls the tool on the MCP server.
    ///
    /// An in-band `isError: true` result is returned as
    /// `Err(`[`McpToolError::Reported`]`)`.
    ///
    /// When the adapter has a router and the calling task has a
    /// [`ProgressSink`] in [`CURRENT_PROGRESS`], the server's progress
    /// notifications for this request are forwarded to the sink. (rmcp adds a
    /// progress token to every request itself, so the request is the same
    /// either way; only the forwarding is conditional.)
    ///
    /// A task created by the server is followed to a terminal state. Dropping
    /// the returned future sends `notifications/cancelled` for the in-flight
    /// request (rmcp >= 3.6 does the same for its own helpers) and, once a
    /// task id is known, `tasks/cancel`. Cancelling before the server has
    /// acknowledged the task sends only the former, so a task created
    /// concurrently may live until its TTL.
    #[cfg_attr(feature = "tracing", instrument(name = "mcp_tool.call", skip_all, fields(name = %self.mcp_tool.name)))]
    async fn call(&self, args: Value) -> Result<Vec<ToolResultPart>> {
        let mut params = CallToolRequestParams::new(self.mcp_tool.name.clone());
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        let sink = CURRENT_PROGRESS.try_with(Arc::clone).ok();
        Ok(self.run(params, sink.as_ref()).await?)
    }
}

impl McpToolAdapter {
    /// Send the call once and resolve whatever the server answers.
    async fn run(
        &self,
        params: CallToolRequestParams,
        sink: Option<&Arc<dyn ProgressSink>>,
    ) -> Result<Vec<ToolResultPart>, McpToolError> {
        let handle = self
            .server
            .send_request_with_option(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                PeerRequestOptions::no_options(),
            )
            .await?;
        // Dropped before a response: tell the server nobody is waiting.
        let mut abandoned = AbandonedRequestGuard::new(self.server.clone(), handle.id.clone());

        // The token is rmcp's, known only now; notifications that raced this
        // point are held by the router and handed over on subscribe.
        let mut subscription = self
            .router
            .as_ref()
            .zip(sink)
            .map(|(router, _)| router.subscribe_progress(handle.progress_token.clone()));

        let response = {
            let mut response = std::pin::pin!(handle.await_response());
            loop {
                let update = async {
                    match subscription.as_mut() {
                        Some(sub) => sub.recv().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    result = &mut response => break result,
                    Some(update) = update => {
                        if let Some(sink) = sink {
                            sink.report(update);
                        }
                    }
                }
            }
        };
        abandoned.disarm();
        // Reports that arrived before the response but were not yet forwarded
        // still belong to this call; later ones find no subscriber.
        if let (Some(sub), Some(sink)) = (subscription.as_mut(), sink) {
            while let Some(update) = sub.try_recv() {
                sink.report(update);
            }
        }
        drop(subscription);

        match response? {
            ServerResult::CallToolResult(result) => map_call_result(result),
            ServerResult::CreateTaskResult(created) => self.follow_task(created, sink).await,
            // MRTR rounds are not supported.
            ServerResult::InputRequiredResult(_) => Err(McpToolError::InputRequiredUnsupported),
            _ => Err(McpToolError::Service(ServiceError::UnexpectedResponse)),
        }
    }

    /// Poll a server-side task until it reaches a terminal state.
    async fn follow_task(
        &self,
        created: CreateTaskResult,
        sink: Option<&Arc<dyn ProgressSink>>,
    ) -> Result<Vec<ToolResultPart>, McpToolError> {
        let task_id = created.task.task_id;
        let mut interval = poll_interval(created.task.poll_interval_ms);
        let mut cancel = TaskCancelGuard::new(self.server.clone(), task_id.clone());
        let waker = self
            .router
            .as_ref()
            .map(|r| r.register_task(task_id.clone()));

        loop {
            match &waker {
                Some(waker) => {
                    tokio::select! {
                        () = tokio::time::sleep(interval) => {}
                        () = waker.notified() => {}
                    }
                }
                None => tokio::time::sleep(interval).await,
            }

            let detailed = self
                .server
                .get_task(GetTaskParams::new(task_id.clone()))
                .await?
                .task;
            interval = poll_interval(detailed.task.poll_interval_ms);

            match detailed.payload {
                TaskPayload::Completed { result } => {
                    cancel.disarm();
                    let result: CallToolResult = serde_json::from_value(Value::Object(result))
                        .map_err(|e| McpToolError::TaskFailed {
                            status: "completed".into(),
                            message: format!("undecodable task result: {e}"),
                        })?;
                    return map_call_result(result);
                }
                TaskPayload::Failed { error } => {
                    cancel.disarm();
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .map_or_else(|| Value::Object(error.clone()).to_string(), str::to_owned);
                    return Err(McpToolError::TaskFailed {
                        status: "failed".into(),
                        message,
                    });
                }
                TaskPayload::Cancelled => {
                    cancel.disarm();
                    return Err(McpToolError::TaskFailed {
                        status: "cancelled".into(),
                        message: detailed
                            .task
                            .status_message
                            .unwrap_or_else(|| "task was cancelled".into()),
                    });
                }
                TaskPayload::InputRequired { .. } => {
                    // The guard cancels the task when it drops on this return.
                    return Err(McpToolError::TaskFailed {
                        status: "input_required".into(),
                        message: "task requires input, which is not supported".into(),
                    });
                }
                // Working, or a status added later: report and keep polling.
                _ => {
                    if let Some(sink) = sink {
                        sink.report(ProgressUpdate {
                            message: Some(
                                detailed
                                    .task
                                    .status_message
                                    .unwrap_or_else(|| "working".into()),
                            ),
                            ..ProgressUpdate::default()
                        });
                    }
                }
            }
        }
    }
}

/// Server-suggested interval, capped at [`MAX_TASK_POLL`].
fn poll_interval(suggested_ms: Option<u64>) -> Duration {
    suggested_ms
        .map_or(DEFAULT_TASK_POLL, Duration::from_millis)
        .min(MAX_TASK_POLL)
}

/// Sends `notifications/cancelled` for a request that is dropped before its
/// response arrived (agent cancellation).
struct AbandonedRequestGuard {
    peer: ServerSink,
    id: Option<RequestId>,
}

impl AbandonedRequestGuard {
    const REASON: &'static str = "request abandoned by the caller";

    fn new(peer: ServerSink, id: RequestId) -> Self {
        Self { peer, id: Some(id) }
    }

    fn disarm(&mut self) {
        self.id.take();
    }
}

impl Drop for AbandonedRequestGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let notification = CancelledNotification::new(CancelledNotificationParam::new(
                Some(id),
                Some(AbandonedRequestGuard::REASON.to_owned()),
            ));
            let _ = peer.send_notification(notification.into()).await;
        });
    }
}

/// Sends `tasks/cancel` when dropped before the task reached a terminal state
/// (agent cancellation, an unsupported `input_required`, or a transport error).
struct TaskCancelGuard {
    peer: ServerSink,
    task_id: String,
    armed: bool,
}

impl TaskCancelGuard {
    fn new(peer: ServerSink, task_id: String) -> Self {
        Self {
            peer,
            task_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TaskCancelGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let peer = self.peer.clone();
        let task_id = std::mem::take(&mut self.task_id);
        runtime.spawn(async move {
            if let Err(e) = peer
                .cancel_task(CancelTaskParams::new(task_id.clone()))
                .await
            {
                log::debug!("tasks/cancel for {task_id} failed: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_with(annotations: Option<Value>) -> RmcpTool {
        let mut v = json!({"name": "t", "inputSchema": {"type": "object"}});
        if let Some(a) = annotations {
            v["annotations"] = a;
        }
        serde_json::from_value(v).expect("valid tool")
    }

    #[test]
    fn read_only_hint_true_is_read_only() {
        assert!(declares_read_only(&tool_with(Some(
            json!({"readOnlyHint": true})
        ))));
    }

    #[test]
    fn read_only_hint_false_or_absent_is_mutating() {
        assert!(!declares_read_only(&tool_with(Some(
            json!({"readOnlyHint": false})
        ))));
        assert!(!declares_read_only(&tool_with(Some(json!({"title": "x"})))));
        assert!(!declares_read_only(&tool_with(None)));
    }

    fn result(v: Value) -> CallToolResult {
        serde_json::from_value(v).expect("valid CallToolResult")
    }

    fn texts(parts: &[ToolResultPart]) -> Vec<&str> {
        parts
            .iter()
            .filter_map(|p| match p {
                ToolResultPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn is_error_becomes_reported_with_server_text() {
        let err = map_call_result(result(json!({
            "content": [{"type": "text", "text": "quota exceeded"}],
            "isError": true
        })))
        .unwrap_err();
        assert!(err.to_string().contains("quota exceeded"));
        let McpToolError::Reported { parts } = err else {
            panic!("expected Reported");
        };
        assert_eq!(texts(&parts), ["quota exceeded"]);
    }

    #[test]
    fn error_flag_absent_or_false_is_success() {
        for v in [
            json!({"content": [{"type": "text", "text": "ok"}]}),
            json!({"content": [{"type": "text", "text": "ok"}], "isError": false}),
        ] {
            assert_eq!(texts(&map_call_result(result(v)).unwrap()), ["ok"]);
        }
    }

    #[test]
    fn reported_without_text_has_generic_message() {
        let err = map_call_result(result(json!({"content": [], "isError": true}))).unwrap_err();
        assert_eq!(err.to_string(), "MCP tool reported an error");
    }

    #[test]
    fn structured_only_is_returned_as_json_text() {
        let parts = map_call_result(result(
            json!({"content": [], "structuredContent": {"temp": 21}}),
        ))
        .unwrap();
        assert_eq!(texts(&parts), [r#"{"temp":21}"#]);
    }

    #[test]
    fn structured_is_not_added_when_content_exists() {
        let parts = map_call_result(result(json!({
            "content": [{"type": "text", "text": "21 degrees"}],
            "structuredContent": {"temp": 21}
        })))
        .unwrap();
        assert_eq!(texts(&parts), ["21 degrees"]);
    }

    #[test]
    fn null_structured_content_is_empty_result() {
        let parts =
            map_call_result(result(json!({"content": [], "structuredContent": null}))).unwrap();
        assert!(parts.is_empty());
    }

    #[test]
    fn reported_parts_use_structured_fallback() {
        let err = map_call_result(result(json!({
            "content": [], "structuredContent": {"code": 7}, "isError": true
        })))
        .unwrap_err();
        assert!(err.to_string().contains(r#"{"code":7}"#));
    }

    // ── ToolResultPart::from_mcp_content ────────────────────────────────

    fn block(v: Value) -> ContentBlock {
        serde_json::from_value(v).expect("valid ContentBlock")
    }

    #[test]
    fn content_text() {
        let part = ToolResultPart::from_mcp_content(block(json!({"type": "text", "text": "hi"})));
        assert!(matches!(part, ToolResultPart::Text { text } if text == "hi"));
    }

    #[test]
    fn content_image_and_audio_decode_inline_bytes() {
        for (ty, kind) in [("image", MediaKind::Image), ("audio", MediaKind::Audio)] {
            let mime = format!("{ty}/x");
            let part = ToolResultPart::from_mcp_content(block(
                json!({"type": ty, "data": "aGk=", "mimeType": mime}),
            ));
            let ToolResultPart::Attachment(media) = part else {
                panic!("expected attachment for {ty}");
            };
            assert_eq!(media.kind, kind);
            assert!(matches!(media.source(), MediaSource::Inline { data } if data == b"hi"));
        }
    }

    #[test]
    fn content_embedded_text_resource_is_text() {
        let part = ToolResultPart::from_mcp_content(block(json!({
            "type": "resource",
            "resource": {"uri": "file:///a", "text": "body"}
        })));
        assert!(matches!(part, ToolResultPart::Text { text } if text == "body"));
    }

    #[test]
    fn content_embedded_blob_resource_is_document() {
        let part = ToolResultPart::from_mcp_content(block(json!({
            "type": "resource",
            "resource": {"uri": "file:///a", "blob": "aGk=", "mimeType": "application/pdf"}
        })));
        let ToolResultPart::Attachment(media) = part else {
            panic!("expected attachment");
        };
        assert_eq!(media.kind, MediaKind::Document);
        assert!(matches!(media.source(), MediaSource::Inline { data } if data == b"hi"));
    }

    #[test]
    fn content_resource_link_is_url_attachment() {
        let part = ToolResultPart::from_mcp_content(block(json!({
            "type": "resource_link", "uri": "https://e.test/a.png",
            "name": "a", "mimeType": "image/png", "description": "pic"
        })));
        let ToolResultPart::Attachment(media) = part else {
            panic!("expected attachment");
        };
        assert_eq!(media.kind, MediaKind::Image);
        assert_eq!(media.filename.as_deref(), Some("a"));
        assert_eq!(media.detail.as_deref(), Some("pic"));
        assert!(
            matches!(media.source(), MediaSource::Url { url } if url == "https://e.test/a.png")
        );
    }

    #[test]
    fn content_unknown_block_falls_back_to_json_text() {
        let Ok(unknown) =
            serde_json::from_value::<ContentBlock>(json!({"type": "hologram", "payload": 1}))
        else {
            // This rmcp build rejects unknown block types at the wire layer,
            // so the non-exhaustive fallback arm is unreachable from JSON.
            return;
        };
        let part = ToolResultPart::from_mcp_content(unknown);
        assert!(matches!(part, ToolResultPart::Text { text } if text.contains("hologram")));
    }
}
