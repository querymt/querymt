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

    async fn speech(
        &self,
        _req: &tts::TtsRequest,
    ) -> Result<tts::TtsResponse, error::LLMError> {
        Err(error::LLMError::NotImplemented("TTS not supported".into()))
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
#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct Usage {
    /// Number of input tokens.
    #[serde(
        alias = "prompt_tokens",     // OpenAI, xAI, DeepSeek, Mistral, OpenRouter, Alibaba
        alias = "input_tokens",      // Anthropic
        alias = "prompt_eval_count", // Ollama
        alias = "promptTokenCount"   // Google
    )]
    pub input_tokens: u32,
    /// Number of output tokens.
    #[serde(
        alias = "completion_tokens",   // OpenAI, xAI, DeepSeek, Mistral, OpenRouter, Alibaba
        alias = "output_tokens",       // Anthropic
        alias = "eval_count",          // Ollama
        alias = "candidatesTokenCount" // Google
    )]
    pub output_tokens: u32,
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
