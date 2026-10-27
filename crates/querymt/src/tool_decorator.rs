use crate::{
    LLMProvider, Tool,
    chat::{ChatCall, ChatCapabilities, ChatOutput, ChatProvider, ChatStream, ToolResultPart},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    stt, tts,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

/// Adapter interface for your host‐side implementations.
///
/// Tool implementations return canonical bounded [`ToolResultPart`] values to
/// support mixed content results (e.g., text + images from MCP tools) without
/// the legacy recursive content representation.
#[async_trait]
pub trait CallFunctionTool: Send + Sync {
    fn descriptor(&self) -> Tool;
    async fn call(&self, args: Value) -> anyhow::Result<Vec<ToolResultPart>>;

    /// Returns the server name for server-aware tools (e.g., MCP tools).
    /// Returns None for tools that don't have server information.
    fn server_name(&self) -> Option<&str> {
        None
    }
}

/*
#[derive(Clone, Serialize, JsonSchema)]
pub struct CallableFunctionTool {
    /// Flattened into the JSON so the LLM sees `name`, `description`, `parameters`
    #[serde(flatten)]
    pub meta: FunctionTool,

    /// Not serialized; the actual Rust callback logic
    #[serde(skip)]
    callback:
        Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync>,
}

impl CallableFunctionTool {
    /// Create a new one by giving it its metadata **and** an async function
    pub fn new<F, Fut>(meta: FunctionTool, f: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self {
            meta,
            callback: Arc::new(move |args| Box::pin(f(args))),
        }
    }
}

#[async_trait]
impl CallFunctionTool for CallableFunctionTool {
    fn descriptor(&self) -> FunctionTool {
        self.meta.clone()
    }

    async fn call(&self, args: Value) -> Result<String> {
        (self.callback)(args).await
    }
}
*/

pub struct ToolEnabledProvider {
    inner: Box<dyn LLMProvider + Send + Sync>,
    registry: HashMap<String, Box<dyn CallFunctionTool>>,
    tool_list: Vec<Tool>,
}

impl ToolEnabledProvider {
    pub fn new(
        inner: Box<dyn LLMProvider + Send + Sync>,
        registry: HashMap<String, Box<dyn CallFunctionTool>>,
    ) -> Self {
        let tool_list = registry.values().map(|t| t.descriptor()).collect();
        ToolEnabledProvider {
            inner,
            registry,
            tool_list,
        }
    }

    /// `call` with this provider's tools filled in when the caller set none.
    fn with_default_tools<'a>(&'a self, call: &ChatCall<'a>) -> ChatCall<'a> {
        call.maybe_tools(Some(call.tools.unwrap_or(&self.tool_list)))
    }

    /// Construct with a pre-built tool list.
    pub fn with_tool_list(
        inner: Box<dyn LLMProvider + Send + Sync>,
        registry: HashMap<String, Box<dyn CallFunctionTool>>,
        tool_list: Vec<Tool>,
    ) -> Self {
        ToolEnabledProvider {
            inner,
            registry,
            tool_list,
        }
    }
}

#[async_trait]
impl CompletionProvider for ToolEnabledProvider {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        self.inner.complete(req).await
    }
}

#[async_trait]
impl EmbeddingProvider for ToolEnabledProvider {
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        self.inner.embed(input).await
    }
}

#[async_trait]
impl ChatProvider for ToolEnabledProvider {
    fn capabilities(&self) -> ChatCapabilities {
        self.inner.capabilities()
    }

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        self.inner.chat(&self.with_default_tools(call)).await
    }

    async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
        self.inner.chat_stream(&self.with_default_tools(call)).await
    }
}

#[async_trait]
impl LLMProvider for ToolEnabledProvider {
    fn tools(&self) -> Option<&[Tool]> {
        Some(&self.tool_list)
    }

    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<Vec<ToolResultPart>, LLMError> {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| LLMError::InvalidRequest(format!("unknown tool `{}`", name)))?;
        tool.call(args)
            .await
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))
    }

    fn tool_server_name(&self, name: &str) -> Option<&str> {
        self.registry.get(name).and_then(|tool| tool.server_name())
    }

    async fn transcribe(&self, req: &stt::SttRequest) -> Result<stt::SttResponse, LLMError> {
        self.inner.transcribe(req).await
    }

    async fn speech(&self, req: &tts::TtsRequest) -> Result<tts::TtsResponse, LLMError> {
        self.inner.speech(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{GenerationOptionSupport, GenerationOptions};
    use crate::validated_llm::ValidatedLLM;
    use std::sync::{Arc, Mutex};

    /// What the innermost provider saw for each call.
    type Seen = Arc<Mutex<Vec<(GenerationOptions, usize)>>>;

    struct Recorder {
        seen: Seen,
        replies: Mutex<Vec<&'static str>>,
    }

    #[async_trait]
    impl ChatProvider for Recorder {
        async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
            self.seen
                .lock()
                .unwrap()
                .push((call.options, call.tools.map_or(0, <[Tool]>::len)));
            let text = self.replies.lock().unwrap().remove(0);
            Ok(ChatOutput::from_projections(
                None,
                Some(text.into()),
                None,
                None,
                None,
            ))
        }

        async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
            self.seen
                .lock()
                .unwrap()
                .push((call.options, call.tools.map_or(0, <[Tool]>::len)));
            Ok(Box::pin(futures::stream::empty()))
        }

        fn capabilities(&self) -> ChatCapabilities {
            ChatCapabilities::NONE
                .with_streaming(true)
                .with_options(GenerationOptionSupport::ALL)
        }
    }

    #[async_trait]
    impl CompletionProvider for Recorder {
        async fn complete(&self, _: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("unused".into()))
        }
    }

    #[async_trait]
    impl EmbeddingProvider for Recorder {
        async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("unused".into()))
        }
    }

    impl LLMProvider for Recorder {}

    /// `ToolEnabledProvider -> ValidatedLLM -> Recorder`, rejecting the first reply.
    fn decorated(seen: &Seen) -> ToolEnabledProvider {
        let recorder = Recorder {
            seen: Arc::clone(seen),
            replies: Mutex::new(vec!["bad", "good"]),
        };
        let validated = ValidatedLLM::new(
            Box::new(recorder),
            Box::new(|reply| {
                if reply == "good" {
                    Ok(())
                } else {
                    Err("not good".into())
                }
            }),
            3,
        );
        ToolEnabledProvider::new(Box::new(validated), HashMap::new())
    }

    #[tokio::test]
    async fn options_survive_the_decorator_chain_including_validation_retries() {
        let seen = Seen::default();
        let provider = decorated(&seen);
        let options = GenerationOptions::default().with_max_tokens(64_000);

        provider
            .chat(&ChatCall::new(&[]).options(options))
            .await
            .unwrap();

        // Both the rejected attempt and its retry carry the caller's options.
        assert_eq!(*seen.lock().unwrap(), [(options, 0), (options, 0)]);
    }

    #[tokio::test]
    async fn streaming_forwards_options_and_the_wrapper_reports_inner_capabilities() {
        let seen = Seen::default();
        let provider = decorated(&seen);
        let options = GenerationOptions::default().with_max_tokens(1_000);

        let _stream = provider
            .chat_stream(&ChatCall::new(&[]).options(options))
            .await
            .unwrap();

        assert_eq!(*seen.lock().unwrap(), [(options, 0)]);
        let caps = provider.capabilities();
        assert!(caps.streaming);
        assert_eq!(caps.options, GenerationOptionSupport::ALL);
    }
}
