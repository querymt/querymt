//! LLM (Rust LLM) is a unified interface for interacting with Large Language Model providers.
//!
//! # Overview
//! This crate provides a consistent API for working with different LLM backends by abstracting away
//! provider-specific implementation details. It supports:
//!
//! - Chat-based interactions
//! - Text completion
//! - Embeddings generation
//! - Multiple providers (OpenAI, Anthropic, etc.)
//! - Request validation and retry logic
//!
//! # Architecture
//! The crate is organized into modules that handle different aspects of LLM interactions:

use serde_json::Value;

use chat::Tool;
use serde::{Deserialize, Serialize};

#[cfg(feature = "http-client")]
pub mod adapters;

/// Builder pattern for configuring and instantiating LLM providers
#[cfg(any(feature = "http-client", feature = "extism_host"))]
pub mod builder;

/// Chain multiple LLM providers together for complex workflows
#[cfg(not(feature = "extism_plugin"))]
pub mod chain;

/// Chat-based interactions with language models (e.g. ChatGPT style)
pub mod chat;

/// Text completion capabilities (e.g. GPT-3 style completion)
pub mod completion;

/// Vector embeddings generation for text
pub mod embedding;

/// Speech to text transcription representations
pub mod stt;

/// Text to speech synthesis representations
pub mod tts;

/// Error types and handling
pub mod error;

/// Credential resolution for dynamic API keys (OAuth, token refresh)
pub mod auth;

#[cfg(feature = "http-client")]
pub mod outbound;

#[cfg(feature = "mcp")]
pub mod mcp;

pub mod plugin;

pub mod tool_decorator;

/// LLM configuration parameters
pub mod params;

/// Validation wrapper for LLM providers with retry capabilities
#[cfg(any(feature = "http-client", feature = "extism_host"))]
pub mod validated_llm;

/// Evaluator for LLM providers
pub mod evaluator;

pub mod providers;

// Re-export LLMParams for convenience
pub use params::LLMParams;

/// Core trait that all LLM providers must implement, combining chat, completion
/// and embedding capabilities into a unified interface
#[async_trait::async_trait]
pub trait LLMProvider:
    chat::ChatProvider + completion::CompletionProvider + embedding::EmbeddingProvider
{
    fn tools(&self) -> Option<&[Tool]> {
        None
    }

    async fn call_tool(&self, _name: &str, _args: Value) -> Result<String, error::LLMError> {
        Err(error::LLMError::ProviderError(
            "tool calling not supported".into(),
        ))
    }

    /// Returns the server name for a tool if available (e.g., for MCP tools).
    /// Returns None if the tool doesn't exist or doesn't have server information.
    fn tool_server_name(&self, _name: &str) -> Option<&str> {
        None
    }

    async fn transcribe(
        &self,
        _req: &stt::SttRequest,
    ) -> Result<stt::SttResponse, error::LLMError> {
        Err(error::LLMError::NotImplemented("STT not supported".into()))
    }

    async fn speech(&self, _req: &tts::TtsRequest) -> Result<tts::TtsResponse, error::LLMError> {
        Err(error::LLMError::NotImplemented("TTS not supported".into()))
    }

    /// Set an API key resolver for dynamic credential refresh (e.g., OAuth).
    /// Default implementation is a no-op for providers that don't support dynamic credentials.
    fn set_key_resolver(&mut self, _resolver: std::sync::Arc<dyn auth::ApiKeyResolver>) {
        // Default: no-op for providers that don't support dynamic credentials
    }

    /// Get the current key resolver, if any.
    fn key_resolver(&self) -> Option<&std::sync::Arc<dyn auth::ApiKeyResolver>> {
        None
    }
}

pub trait HTTPLLMProvider:
    chat::http::HTTPChatProvider
    + completion::http::HTTPCompletionProvider
    + embedding::http::HTTPEmbeddingProvider
    + Send
    + Sync
{
    fn tools(&self) -> Option<&[Tool]> {
        None
    }

    fn stt_request(
        &self,
        _req: &stt::SttRequest,
    ) -> Result<http::Request<Vec<u8>>, error::LLMError> {
        Err(error::LLMError::NotImplemented("STT not supported".into()))
    }

    fn parse_stt(
        &self,
        _resp: http::Response<Vec<u8>>,
    ) -> Result<stt::SttResponse, error::LLMError> {
        Err(error::LLMError::NotImplemented("STT not supported".into()))
    }

    fn tts_request(
        &self,
        _req: &tts::TtsRequest,
    ) -> Result<http::Request<Vec<u8>>, error::LLMError> {
        Err(error::LLMError::NotImplemented("TTS not supported".into()))
    }

    fn parse_tts(
        &self,
        _resp: http::Response<Vec<u8>>,
    ) -> Result<tts::TtsResponse, error::LLMError> {
        Err(error::LLMError::NotImplemented("TTS not supported".into()))
    }

    /// Returns the API key resolver for this provider, if one is set.
    ///
    /// The adapter layer calls this to obtain the resolver, then invokes
    /// [`ApiKeyResolver::resolve()`](auth::ApiKeyResolver::resolve) before
    /// each request to ensure credentials are fresh.
    fn key_resolver(&self) -> Option<&std::sync::Arc<dyn auth::ApiKeyResolver>> {
        None
    }

    /// Attach a key resolver for dynamic credential refresh.
    ///
    /// Called after provider construction (e.g., by the session layer) to
    /// enable OAuth token refresh without rebuilding the provider.
    fn set_key_resolver(&mut self, _resolver: std::sync::Arc<dyn auth::ApiKeyResolver>) {
        // Default: ignore. Providers that support dynamic credentials override this.
    }
}

/// Tool call represents a function call that an LLM wants to make.
/// This is a standardized structure used across all providers.
#[derive(Debug, Deserialize, Serialize, Clone, Eq, PartialEq)]
pub struct ToolCall {
    /// The ID of the tool call.
    pub id: String,
    /// The type of the tool call (usually "function").
    #[serde(rename = "type")]
    pub call_type: String,
    /// The function to call.
    pub function: FunctionCall,
}

/// FunctionCall contains details about which function to call and with what arguments.
#[derive(Debug, Deserialize, Serialize, Clone, Eq, PartialEq)]
pub struct FunctionCall {
    /// The name of the function to call.
    pub name: String,
    /// The arguments to pass to the function, typically serialized as a JSON string.
    pub arguments: String,
}

/// Represents the usage of tokens in a tool call, supporting multiple JSON formats.
#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq, Default)]
pub struct Usage {
    /// Number of input tokens.
    #[serde(
        default,
        alias = "prompt_tokens",     // OpenAI, xAI, DeepSeek, Mistral, OpenRouter, Alibaba
        alias = "input_tokens",      // Anthropic
        alias = "prompt_eval_count", // Ollama
        alias = "promptTokenCount"   // Google
    )]
    pub input_tokens: u32,
    /// Number of output tokens.
    #[serde(
        default,
        alias = "completion_tokens",   // OpenAI, xAI, DeepSeek, Mistral, OpenRouter, Alibaba
        alias = "output_tokens",       // Anthropic
        alias = "eval_count",          // Ollama
        alias = "candidatesTokenCount" // Google
    )]
    pub output_tokens: u32,

    /// Reasoning/thinking output tokens.
    #[serde(default)]
    pub reasoning_tokens: u32,

    /// Tokens served from a cached prefix.
    #[serde(default, alias = "cache_read_input_tokens")]
    pub cache_read: u32,

    /// Tokens used to create a new cache entry.
    #[serde(default, alias = "cache_creation_input_tokens")]
    pub cache_write: u32,
}

impl Usage {
    /// Merge two `Usage` values by taking the field-wise maximum.
    ///
    /// This is the correct strategy when a provider splits usage across multiple
    /// streaming events (e.g. Anthropic sends `input_tokens` in `message_start`
    /// and cumulative `output_tokens` in `message_delta`).  Taking the max of
    /// each field preserves whichever event carried the authoritative value for
    /// that field.
    pub fn merge_max(self, other: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.max(other.input_tokens),
            output_tokens: self.output_tokens.max(other.output_tokens),
            reasoning_tokens: self.reasoning_tokens.max(other.reasoning_tokens),
            cache_read: self.cache_read.max(other.cache_read),
            cache_write: self.cache_write.max(other.cache_write),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_max_combines_split_anthropic_usage() {
        // Simulates Anthropic's two-event streaming usage:
        //   message_start  → input_tokens present, output_tokens is a placeholder (1)
        //   message_delta  → input_tokens absent (0 by serde default), output_tokens is final
        let from_message_start = Usage {
            input_tokens: 25,
            output_tokens: 1,
            cache_read: 100,
            cache_write: 50,
            reasoning_tokens: 0,
        };
        let from_message_delta = Usage {
            input_tokens: 0,   // absent in JSON, defaults to 0
            output_tokens: 15, // cumulative final value
            cache_read: 0,
            cache_write: 0,
            reasoning_tokens: 0,
        };

        let merged = from_message_start.merge_max(from_message_delta);

        assert_eq!(merged.input_tokens, 25); // preserved from message_start
        assert_eq!(merged.output_tokens, 15); // taken from message_delta (cumulative)
        assert_eq!(merged.cache_read, 100); // preserved from message_start
        assert_eq!(merged.cache_write, 50); // preserved from message_start
        assert_eq!(merged.reasoning_tokens, 0);
    }

    #[test]
    fn test_merge_max_single_event_is_identity() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read: 5,
            cache_write: 3,
            reasoning_tokens: 7,
        };
        let merged = usage.clone().merge_max(Usage::default());
        assert_eq!(merged, usage);
    }

    #[test]
    fn test_merge_max_is_commutative_when_fields_dont_overlap() {
        let a = Usage {
            input_tokens: 25,
            output_tokens: 1,
            ..Usage::default()
        };
        let b = Usage {
            input_tokens: 0,
            output_tokens: 15,
            ..Usage::default()
        };
        assert_eq!(a.clone().merge_max(b.clone()), b.merge_max(a));
    }
}

// NOTE: We need this part to be a macro instead two separate function for specific implementations
// like native and wasm, because in other way functions need to be in each provider to get
// configuration from extism_pdk.
#[macro_export]
macro_rules! get_env_var {
    ($key:expr) => {{
        if cfg!(feature = "extism") {
            match extism_pdk::config::get($key) {
                Ok(value) => value,
                _ => None,
            }
        } else {
            std::env::var($key).ok()
        }
    }};
}
