//! A
//! Builder module for configuring and instantiating LLM providers.
//!
//! This module provides a flexible builder pattern for creating and configuring
//! LLM (Large Language Model) provider instances with various settings and options.

use crate::{
    chat::{
        FunctionTool, ParameterProperty, ParametersSchema, ReasoningEffort, StructuredOutputFormat,
        Tool, ToolChoice,
    },
    error::LLMError,
    plugin::{LLMProviderFactory, ProviderRegistry},
    tool_decorator::{CallFunctionTool, ToolEnabledProvider},
    LLMProvider,
};
use serde::Serialize;
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::Arc};

/// A function type for validating LLM provider outputs.
/// Takes a response string and returns Ok(()) if valid, or Err with an error message if invalid.
pub type ValidatorFn = dyn Fn(&str) -> Result<(), String> + Send + Sync + 'static;

fn prune_config_by_schema(cfg: &Value, schema: &Value) -> Value {
    match (cfg, schema.get("properties")) {
        (Value::Object(cfg_map), Some(Value::Object(props))) => {
            // Build a new object only with keys in props
            let mut out = Map::with_capacity(cfg_map.len());
            for (k, v) in cfg_map {
                if let Some(prop_schema) = props.get(k) {
                    // If the subschema has its own nested properties, recurse
                    let pruned_val = if prop_schema.get("properties").is_some() {
                        prune_config_by_schema(v, prop_schema)
                    } else {
                        v.clone()
                    };
                    out.insert(k.clone(), pruned_val);
                }
            }
            Value::Object(out)
        }
        // Not an object or no properties defined → return as-is
        _ => cfg.clone(),
    }
}

/// Builder for configuring and instantiating LLM providers.
///
/// Provides a fluent interface for setting various configuration options
/// like model selection, API keys, generation parameters, etc.
#[derive(Default, Serialize)]
pub struct LLMBuilder {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
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
    /// Function tools
    //    #[serde(skip_serializing_if = "Option::is_none")]
    //    tools: Option<Vec<Tool>>,
    /// Tool choice
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    /// Enable parallel tool use
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_parallel_tool_use: Option<bool>,
    /// Enable reasoning
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<bool>,
    /// Enable reasoning effort
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
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

impl LLMBuilder {
    /// Creates a new empty builder instance with default values.
    pub fn new() -> Self {
        Self::default()
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

    /// Sets the system prompt/context.
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets the reasoning flag.
    pub fn reasoning_effort(mut self, reasoning_effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(reasoning_effort.to_string());
        self
    }

    /// Sets the reasoning flag.
    pub fn reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = Some(reasoning);
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
    ///
    /// # Arguments
    ///
    /// * `f` - Function that takes a response string and returns Ok(()) if valid, or Err with error message if invalid
    pub fn validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    {
        self.validator = Some(Box::new(f));
        self
    }

    /// Sets the number of retry attempts for validation failures.
    ///
    /// # Arguments
    ///
    /// * `attempts` - Maximum number of times to retry generating a valid response
    pub fn validator_attempts(mut self, attempts: usize) -> Self {
        self.validator_attempts = attempts;
        self
    }

    /*
        /// Adds a function tool to the builder
        pub fn function(mut self, function_builder: FunctionBuilder) -> Self {
            if self.tools.is_none() {
                self.tools = Some(Vec::new());
            }
            if let Some(tools) = &mut self.tools {
                tools.push(function_builder.build());
            }
            self
        }

        pub fn tools(mut self, mut new_tools: Vec<Tool>) -> Self {
            if self.tools.is_none() {
                self.tools = Some(new_tools.clone());
            }
            if let Some(tools) = &mut self.tools {
                tools.append(new_tools.as_mut());
            }
            println!("did it set {:?}", self.tools);
            self
        }
    */
    /// Enable parallel tool use
    pub fn enable_parallel_tool_use(mut self, enable: bool) -> Self {
        self.enable_parallel_tool_use = Some(enable);
        self
    }

    /// Set tool choice.  Note that if the choice is given as Tool(name), and that
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
        let map = self.custom_options.get_or_insert_with(HashMap::new);
        map.insert(key.into(), value);
        self
    }

    /// Builds and returns a configured LLM provider instance.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No backend is specified
    /// - Required backend feature is not enabled
    /// - Required configuration like API keys are missing
    pub fn build(
        self,
        registry: &Box<dyn ProviderRegistry>,
    ) -> Result<Box<dyn LLMProvider>, LLMError> {
        //        let (tools, tool_choice) = self.validate_tool_config()?;

        let provider_name = self
            .provider
            .clone()
            .ok_or_else(|| LLMError::InvalidRequest("No provider specified".to_string()))?;
        let factory: Arc<dyn LLMProviderFactory> =
            registry.get(&provider_name).ok_or_else(|| {
                LLMError::InvalidRequest(format!("Unknown provider: {}", provider_name))
            })?;

        let full_cfg: Value = serde_json::to_value(&self)?;
        let schema = factory.config_schema();
        let pruned_cfg = prune_config_by_schema(&full_cfg, &schema);
        let base = factory.from_config(&pruned_cfg)?;
        let provider: Box<dyn LLMProvider> = if self.tool_registry.is_empty() {
            base
        } else {
            Box::new(ToolEnabledProvider::new(base, self.tool_registry))
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
}

impl FunctionBuilder {
    pub fn new<N: Into<String>>(name: N) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            parameters: Vec::new(),
            required: Vec::new(),
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

    pub fn build(self) -> Tool {
        let mut props = HashMap::new();
        for pb in self.parameters {
            let (key, prop) = pb.build();
            props.insert(key, prop);
        }

        let function = FunctionTool {
            name: self.name,
            description: self.description,
            parameters: ParametersSchema {
                schema_type: "object".to_string(),
                properties: props,
                required: self.required,
            },
        };

        Tool {
            tool_type: "function".to_string(),
            function,
        }
    }
}
