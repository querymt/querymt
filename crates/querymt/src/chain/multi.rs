//! Module for chaining multiple LLM backends in a single prompt sequence.
//! Each step can reference a distinct provider_id ("openai", "anthro", etc.).

use crate::{
    chat::{ChatMessage, ChatRole, MessageType},
    completion::CompletionRequest,
    error::LLMError,
    LLMProvider, ToolCall,
};
use std::collections::HashMap;
use tracing::instrument;

/// Stores multiple LLM backends (OpenAI, Anthropic, etc.) identified by a key
#[derive(Default)]
pub struct LLMRegistry {
    pub backends: HashMap<String, Box<dyn LLMProvider>>,
}

impl LLMRegistry {
    pub fn new() -> Self {
        Self {
            backends: HashMap::new(),
        }
    }

    /// Inserts a backend under an identifier, e.g. "openai"
    pub fn insert(&mut self, id: impl Into<String>, llm: Box<dyn LLMProvider>) {
        self.backends.insert(id.into(), llm);
    }

    /// Retrieves a backend by its identifier
    pub fn get(&self, id: &str) -> Option<&dyn LLMProvider> {
        self.backends.get(id).map(|b| b.as_ref())
    }
}

/// Builder pattern for LLMRegistry
#[derive(Default)]
pub struct LLMRegistryBuilder {
    registry: LLMRegistry,
}

impl LLMRegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a backend under the given id
    pub fn register(mut self, id: impl Into<String>, llm: Box<dyn LLMProvider>) -> Self {
        self.registry.insert(id, llm);
        self
    }

    /// Builds the final LLMRegistry
    pub fn build(self) -> LLMRegistry {
        self.registry
    }
}

/// Response transformation function
type ResponseTransform = Box<dyn Fn(String) -> String + Send + Sync>;

/// Execution mode for a step: Chat or Completion
#[derive(Debug, Clone)]
pub enum MultiChainStepMode {
    Chat,
    Completion,
}

/// Multi-backend chain step
pub struct MultiChainStep {
    provider_id: String,
    id: String,
    template: String,
    mode: MultiChainStepMode,

    // Override parameters
    temperature: Option<f32>,
    max_tokens: Option<u32>,

    // Response transformation
    response_transform: Option<ResponseTransform>,
}

/// Builder for MultiChainStep (Stripe-style)
pub struct MultiChainStepBuilder {
    provider_id: Option<String>,
    id: Option<String>,
    template: Option<String>,
    mode: MultiChainStepMode,

    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    response_transform: Option<ResponseTransform>,
}

impl MultiChainStepBuilder {
    pub fn new(mode: MultiChainStepMode) -> Self {
        Self {
            provider_id: None,
            id: None,
            template: None,
            mode,
            temperature: None,
            top_p: None,
            max_tokens: None,
            response_transform: None,
        }
    }

    /// Backend identifier to use, e.g. "openai"
    pub fn provider_id(mut self, pid: impl Into<String>) -> Self {
        self.provider_id = Some(pid.into());
        self
    }

    /// Unique identifier for the step, e.g. "calc1"
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// The prompt or template (e.g. "2 * 4 = ?")
    pub fn template(mut self, tmpl: impl Into<String>) -> Self {
        self.template = Some(tmpl.into());
        self
    }

    // Parameters
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    pub fn top_p(mut self, p: f32) -> Self {
        self.top_p = Some(p);
        self
    }

    pub fn max_tokens(mut self, mt: u32) -> Self {
        self.max_tokens = Some(mt);
        self
    }

    pub fn response_transform<F>(mut self, func: F) -> Self
    where
        F: Fn(String) -> String + Send + Sync + 'static,
    {
        self.response_transform = Some(Box::new(func));
        self
    }

    /// Builds the step
    pub fn build(self) -> Result<MultiChainStep, LLMError> {
        let provider_id = self
            .provider_id
            .ok_or_else(|| LLMError::InvalidRequest("No provider_id set".into()))?;
        let id = self
            .id
            .ok_or_else(|| LLMError::InvalidRequest("No step id set".into()))?;
        let tmpl = self
            .template
            .ok_or_else(|| LLMError::InvalidRequest("No template set".into()))?;

        Ok(MultiChainStep {
            provider_id,
            id,
            template: tmpl,
            mode: self.mode,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            response_transform: self.response_transform,
        })
    }
}

/// The multi-backend chain
pub struct MultiPromptChain<'a> {
    registry: &'a LLMRegistry,
    steps: Vec<MultiChainStep>,
    memory: HashMap<String, String>, // stores responses
}

impl<'a> MultiPromptChain<'a> {
    pub fn new(registry: &'a LLMRegistry) -> Self {
        Self {
            registry,
            steps: vec![],
            memory: HashMap::new(),
        }
    }

    /// Adds a step
    pub fn step(mut self, step: MultiChainStep) -> Self {
        self.steps.push(step);
        self
    }

    /// Executes all steps
    #[instrument(name = "multi_prompt_chain.run", skip_all)]
    pub async fn run(mut self) -> Result<HashMap<String, String>, LLMError> {
        for step in &self.steps {
            // 1) Replace {{xyz}} in template with existing memory
            let prompt_text = self.replace_template(&step.template);

            // 2) Get the right backend (with lazy loading)
            let llm = self.registry.get(&step.provider_id).ok_or_else(|| {
                LLMError::InvalidRequest(format!(
                    "No provider with id '{}' found in registry",
                    step.provider_id
                ))
            })?;

            // 3) Execute
            let mut response = match step.mode {
                MultiChainStepMode::Chat => {
                    let mut step_messages = vec![ChatMessage {
                        role: ChatRole::User,
                        message_type: MessageType::Text,
                        content: prompt_text,
                        thinking: None,
                        cache: None,
                    }];

                    let mut final_response_text = String::new();
                    const MAX_TOOL_ITERATIONS: usize = 5;

                    for _ in 0..MAX_TOOL_ITERATIONS {
                        // Always use `chat_with_tools` to provide the tool definitions to the LLM.
                        let response = llm.chat_with_tools(&step_messages, llm.tools()).await?;

                        let response_text = response.text();
                        let tool_calls = response.tool_calls();

                        // Add the assistant's response to the conversation history for the next turn.
                        step_messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: response_text.clone().unwrap_or_default(),
                            thinking: response.thinking(),
                            cache: None,
                            message_type: if let Some(ref tcs) = tool_calls {
                                if tcs.is_empty() {
                                    MessageType::Text
                                } else {
                                    MessageType::ToolUse(tcs.clone())
                                }
                            } else {
                                MessageType::Text
                            },
                        });

                        // If there are tool calls, execute them. Otherwise, we're done.
                        if let Some(calls) = tool_calls {
                            if calls.is_empty() {
                                final_response_text = response_text.unwrap_or_default();
                                break;
                            }

                            let tool_futures = calls.iter().map(|call| async move {
                                let args: serde_json::Value =
                                    serde_json::from_str(&call.function.arguments)?;

                                let result_content =
                                    llm.call_tool(&call.function.name, args).await?;

                                // Repurpose `ToolCall` to carry the result. This is a workaround due to
                                // the existing `MessageType::ToolResult` definition. The result from the
                                // tool is placed into the `arguments` field.
                                Ok(ToolCall {
                                    id: call.id.clone(),
                                    call_type: "function".to_string(),
                                    function: crate::FunctionCall {
                                        name: call.function.name.clone(),
                                        arguments: result_content,
                                    },
                                })
                            });

                            let tool_results = futures::future::join_all(tool_futures)
                                .await
                                .into_iter()
                                .collect::<Result<Vec<ToolCall>, LLMError>>()?;

                            // Add tool results back into the conversation history. The 'User' role is
                            // used as a substitute for the standard 'tool' role, which is not defined.
                            step_messages.push(ChatMessage {
                                role: ChatRole::User,
                                content: String::new(),
                                message_type: MessageType::ToolResult(tool_results),
                                thinking: None,
                                cache: None,
                            });

                            // Continue the loop to allow the LLM to process the tool results.
                        } else {
                            // The model did not return any tool calls, so this is the final response.
                            final_response_text = response_text.unwrap_or_default();
                            break;
                        }
                    }
                    final_response_text
                }
                MultiChainStepMode::Completion => {
                    let mut req = CompletionRequest::new(prompt_text);
                    req.temperature = step.temperature;
                    req.max_tokens = step.max_tokens;
                    let c = llm.complete(&req).await?;
                    c.text.to_string()
                }
            };

            if let Some(transform) = &step.response_transform {
                response = transform(response);
            }

            // 4) Store the response
            self.memory.insert(step.id.clone(), response);
        }
        Ok(self.memory)
    }

    fn replace_template(&self, input: &str) -> String {
        let mut out = input.to_string();
        for (k, v) in &self.memory {
            let pattern = format!("{{{{{}}}}}", k);
            out = out.replace(&pattern, v);
        }
        out
    }

    /// Adds multiple steps at once
    pub fn chain(mut self, steps: Vec<MultiChainStep>) -> Self {
        self.steps.extend(steps);
        self
    }
}
