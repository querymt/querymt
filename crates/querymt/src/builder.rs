//! Builder module for configuring and instantiating LLM providers.
//!
//! This module provides a flexible builder pattern for creating and configuring
//! LLM (Large Language Model) provider instances with various settings and options.

use crate::{
    LLMProvider,
    chat::{
        FunctionTool, ParameterProperty, ParametersSchema, ReasoningEffort, StructuredOutputFormat,
        Tool, ToolChoice,
    },
    error::LLMError,
    plugin::{LLMProviderFactory, host::PluginRegistry},
    provider_config::prune_config_by_schema,
    tool_decorator::{CallFunctionTool, ToolEnabledProvider},
};
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
#[cfg(feature = "tracing")]
use tracing::instrument;

/// A function type for validating LLM provider outputs.
/// Takes a response string and returns Ok(()) if valid, or Err with an error message if invalid.
pub type ValidatorFn = dyn Fn(&str) -> Result<(), String> + Send + Sync + 'static;

#[derive(Default, Serialize)]
pub struct Unbound;

pub struct BoundRegistry<'a> {
    registry: &'a PluginRegistry,
}

impl Serialize for BoundRegistry<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_unit_struct("BoundRegistry")
    }
}

/// Builder for configuring and instantiating LLM providers.
///
/// Provides a fluent interface for setting various configuration options
/// like model selection, API keys, generation parameters, etc.
#[derive(Serialize)]
pub struct LLMBuilder<State = Unbound> {
    #[serde(skip_serializing)]
    state: State,
    /// Selected backend provider
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    /// API key for authentication with the provider
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    /// Base URL for API requests (primarily for self-hosted instances)
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// Model identifier/name to use
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Maximum tokens to generate in responses
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    /// Temperature parameter for controlling response randomness (0.0-1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// System prompt/context to guide model behavior
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    system: Vec<String>,
    /// Request timeout duration in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    /// Whether to enable streaming responses
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// Top-p (nucleus) sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    /// Top-k sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    /// Format specification for embedding outputs
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_encoding_format: Option<String>,
    /// Vector dimensions for embedding outputs
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_dimensions: Option<u32>,
    /// Optional validation function for response content
    #[serde(skip_serializing)]
    validator: Option<Box<ValidatorFn>>,
    /// Number of retry attempts when validation fails
    validator_attempts: usize,
    /// Tool choice
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    /// Enable parallel tool use
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_parallel_tool_use: Option<bool>,
    /// Reasoning effort level
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    /// reasoning_budget_tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_budget_tokens: Option<u32>,
    /// JSON schema for structured output
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<StructuredOutputFormat>,
    #[serde(skip_serializing)]
    tool_registry: HashMap<String, Box<dyn CallFunctionTool>>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    custom_options: Option<HashMap<String, Value>>,
}

impl Default for LLMBuilder<Unbound> {
    fn default() -> Self {
        Self::new()
    }
}

impl LLMBuilder<Unbound> {
    /// Creates a new empty builder instance with default values.
    pub fn new() -> Self {
        Self {
            state: Unbound,
            provider: None,
            api_key: None,
            base_url: None,
            model: None,
            max_tokens: None,
            temperature: None,
            system: Vec::new(),
            timeout_seconds: None,
            stream: None,
            top_p: None,
            top_k: None,
            embedding_encoding_format: None,
            embedding_dimensions: None,
            validator: None,
            validator_attempts: 0,
            tool_choice: None,
            enable_parallel_tool_use: None,
            reasoning_effort: None,
            reasoning_budget_tokens: None,
            json_schema: None,
            tool_registry: HashMap::new(),
            custom_options: None,
        }
    }

    pub async fn build_with(
        self,
        registry: &PluginRegistry,
    ) -> Result<Box<dyn LLMProvider>, LLMError> {
        self.build_with_registry(registry).await
    }
}

impl<'a> LLMBuilder<BoundRegistry<'a>> {
    pub async fn build(self) -> Result<Box<dyn LLMProvider>, LLMError> {
        let registry = self.state.registry;
        self.build_with_registry(registry).await
    }
}

impl<State> LLMBuilder<State> {
    pub(crate) fn bind(self, registry: &PluginRegistry) -> LLMBuilder<BoundRegistry<'_>> {
        LLMBuilder {
            state: BoundRegistry { registry },
            provider: self.provider,
            api_key: self.api_key,
            base_url: self.base_url,
            model: self.model,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            system: self.system,
            timeout_seconds: self.timeout_seconds,
            stream: self.stream,
            top_p: self.top_p,
            top_k: self.top_k,
            embedding_encoding_format: self.embedding_encoding_format,
            embedding_dimensions: self.embedding_dimensions,
            validator: self.validator,
            validator_attempts: self.validator_attempts,
            tool_choice: self.tool_choice,
            enable_parallel_tool_use: self.enable_parallel_tool_use,
            reasoning_effort: self.reasoning_effort,
            reasoning_budget_tokens: self.reasoning_budget_tokens,
            json_schema: self.json_schema,
            tool_registry: self.tool_registry,
            custom_options: self.custom_options,
        }
    }

    /// Sets the backend provider to use.
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Sets the API key for authentication.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Sets the base URL for API requests.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Sets the model identifier to use.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the maximum number of tokens to generate.
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the temperature for controlling response randomness (0.0-1.0).
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Appends a system prompt part. Can be called multiple times for multi-part prompts.
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system.push(system.into());
        self
    }

    /// Sets the reasoning effort level.
    /// Providers that support reasoning will map this to their own API format.
    pub fn reasoning_effort(mut self, reasoning_effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(reasoning_effort);
        self
    }

    /// Sets the reasoning budget tokens.
    pub fn reasoning_budget_tokens(mut self, reasoning_budget_tokens: u32) -> Self {
        self.reasoning_budget_tokens = Some(reasoning_budget_tokens);
        self
    }

    /// Sets the request timeout in seconds.
    pub fn timeout_seconds(mut self, timeout_seconds: u64) -> Self {
        self.timeout_seconds = Some(timeout_seconds);
        self
    }

    /// Enables or disables streaming responses.
    pub fn stream(mut self, stream: bool) -> Self {
        self.stream = Some(stream);
        self
    }

    /// Sets the top-p (nucleus) sampling parameter.
    pub fn top_p(mut self, top_p: f32) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Sets the top-k sampling parameter.
    pub fn top_k(mut self, top_k: u32) -> Self {
        self.top_k = Some(top_k);
        self
    }

    /// Sets the encoding format for embeddings.
    pub fn embedding_encoding_format(
        mut self,
        embedding_encoding_format: impl Into<String>,
    ) -> Self {
        self.embedding_encoding_format = Some(embedding_encoding_format.into());
        self
    }

    /// Sets the dimensions for embeddings.
    pub fn embedding_dimensions(mut self, embedding_dimensions: u32) -> Self {
        self.embedding_dimensions = Some(embedding_dimensions);
        self
    }

    /// Sets the JSON schema for structured output.
    pub fn schema(mut self, schema: impl Into<StructuredOutputFormat>) -> Self {
        self.json_schema = Some(schema.into());
        self
    }

    /// Sets a validation function to verify LLM responses.
    pub fn validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    {
        self.validator = Some(Box::new(f));
        self
    }

    /// Sets the number of retry attempts for validation failures.
    pub fn validator_attempts(mut self, attempts: usize) -> Self {
        self.validator_attempts = attempts;
        self
    }

    /// Enable parallel tool use
    pub fn enable_parallel_tool_use(mut self, enable: bool) -> Self {
        self.enable_parallel_tool_use = Some(enable);
        self
    }

    /// Set tool choice. Note that if the choice is given as Tool(name), and that
    /// tool isn't available, the builder will fail.
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Explicitly disable the use of tools, even if they are provided.
    pub fn disable_tools(mut self) -> Self {
        self.tool_choice = Some(ToolChoice::None);
        self
    }

    pub fn add_tool<T>(mut self, tool: T) -> Self
    where
        T: CallFunctionTool + 'static,
    {
        let name = tool.descriptor().function.name.clone();
        self.tool_registry.insert(name, Box::new(tool));
        self
    }

    pub fn parameter<K>(mut self, key: K, value: Value) -> Self
    where
        K: Into<String>,
    {
        self.custom_options
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value);
        self
    }

    pub fn parameters_from_value(mut self, value: &Value) -> Self {
        if let Some(obj) = value.as_object() {
            for (key, value) in obj {
                self = self.parameter(key.clone(), value.clone());
            }
        }
        self
    }

    #[cfg_attr(feature = "tracing", instrument(name = "llm_builder.build", skip(self, registry), fields(provider = self.provider.as_deref().unwrap_or("unknown"))))]
    async fn build_with_registry(
        self,
        registry: &PluginRegistry,
    ) -> Result<Box<dyn LLMProvider>, LLMError> {
        let provider_name = self
            .provider
            .clone()
            .ok_or_else(|| LLMError::InvalidRequest("No provider specified".to_string()))?;
        let factory: Arc<dyn LLMProviderFactory> =
            registry.get(&provider_name).await.ok_or_else(|| {
                LLMError::InvalidRequest(format!("Unknown provider: {}", provider_name))
            })?;

        let tool_list: Vec<Tool> = self
            .tool_registry
            .values()
            .map(|t| t.descriptor())
            .collect();

        let resolved_cfg = crate::provider_config::resolve_registry_provider_config(
            registry,
            &provider_name,
            factory.as_ref(),
        )?;

        let mut full_cfg = match resolved_cfg.full_config {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        let builder_cfg = match serde_json::to_value(&self)? {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        for (key, value) in builder_cfg {
            full_cfg.insert(key, value);
        }

        let schema: Value = serde_json::from_str(&factory.config_schema())?;
        let full_cfg = Value::Object(full_cfg);
        let pruned_cfg = prune_config_by_schema(&full_cfg, &schema);
        let pruned_cfg_str = serde_json::to_string(&pruned_cfg)?;
        let base = factory.from_config(&pruned_cfg_str)?;

        let provider: Box<dyn LLMProvider> = if self.tool_registry.is_empty() {
            base
        } else {
            Box::new(ToolEnabledProvider::with_tool_list(
                base,
                self.tool_registry,
                tool_list,
            ))
        };

        #[allow(unreachable_code)]
        if let Some(validator) = self.validator {
            Ok(Box::new(crate::validated_llm::ValidatedLLM::new(
                provider,
                validator,
                self.validator_attempts,
            )))
        } else {
            Ok(provider)
        }
    }

    /*
    // Validate that tool configuration is consistent and valid
    fn validate_tool_config(&self) -> Result<(Option<Vec<Tool>>, Option<ToolChoice>), LLMError> {
        match self.tool_choice {
            Some(ToolChoice::Tool(ref name)) => {
                match self.tool_registry.values().map(|t| t.descriptor().function.name == *name) {
                    Some(true) => Ok((self.tools.clone(), self.tool_choice.clone())),
                    _ => Err(LLMError::ToolConfigError(format!("Tool({}) cannot be tool choice: no tool with name {} found.  Did you forget to add it with .function?", name, name))),
                }
            }
            Some(_) if self.tools.is_none() => Err(LLMError::ToolConfigError(
                "Tool choice cannot be set without tools configured".to_string(),
            )),
            _ => Ok((self.tools.clone(), self.tool_choice.clone())),
        }
    }
    */
}

/// Builder for function parameters
pub struct ParamBuilder {
    name: String,
    property_type: String,
    description: String,
    items: Option<Box<ParamBuilder>>,
    enum_list: Option<Vec<String>>,
}

impl ParamBuilder {
    pub fn new<N: Into<String>>(name: N) -> Self {
        Self {
            name: name.into(),
            property_type: "string".to_string(),
            description: String::new(),
            items: None,
            enum_list: None,
        }
    }
    pub fn type_of<T: Into<String>>(mut self, type_str: T) -> Self {
        self.property_type = type_str.into();
        self
    }

    pub fn description<D: Into<String>>(mut self, desc: D) -> Self {
        self.description = desc.into();
        self
    }

    pub fn items(mut self, item_builder: ParamBuilder) -> Self {
        self.items = Some(Box::new(item_builder));
        self
    }

    pub fn enum_list<I, S>(mut self, vals: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.enum_list = Some(vals.into_iter().map(Into::into).collect());
        self
    }

    pub fn build(self) -> (String, ParameterProperty) {
        let items_prop = self.items.map(|b| Box::new(b.build().1));
        (
            self.name.clone(),
            ParameterProperty {
                property_type: self.property_type,
                description: self.description,
                items: items_prop,
                enum_list: self.enum_list,
            },
        )
    }
}

/// Builder for function tools
pub struct FunctionBuilder {
    name: String,
    description: String,
    parameters: Vec<ParamBuilder>,
    required: Vec<String>,
    raw_schema: Option<serde_json::Value>,
}

impl FunctionBuilder {
    pub fn new<N: Into<String>>(name: N) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            parameters: Vec::new(),
            required: Vec::new(),
            raw_schema: None,
        }
    }

    pub fn description<D: Into<String>>(mut self, desc: D) -> Self {
        self.description = desc.into();
        self
    }

    pub fn param(mut self, param: ParamBuilder) -> Self {
        self.parameters.push(param);
        self
    }

    pub fn required<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.required = names.into_iter().map(Into::into).collect();
        self
    }

    /// Provides a full JSON Schema for the parameters.  Using this method
    /// bypasses the DSL and allows arbitrary complex schemas (nested arrays,
    /// objects, oneOf, etc.).
    pub fn json_schema(mut self, schema: serde_json::Value) -> Self {
        self.raw_schema = Some(schema);
        self
    }

    pub fn build(self) -> Tool {
        let parameters_value = if let Some(schema) = self.raw_schema {
            schema
        } else {
            let mut properties = HashMap::new();
            for param in self.parameters {
                let (name, prop) = param.build();
                properties.insert(name, prop);
            }

            serde_json::to_value(ParametersSchema {
                schema_type: "object".to_string(),
                properties,
                required: self.required,
            })
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
        };

        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name,
                description: self.description,
                parameters: parameters_value,
            },
        }
    }
}
