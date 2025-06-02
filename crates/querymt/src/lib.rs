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

/// Error types and handling
pub mod error;

#[cfg(feature = "http-client")]
pub mod outbound;

#[cfg(feature = "mcp")]
pub mod mcp;

pub mod plugin;

pub mod tool_decorator;

/// Validation wrapper for LLM providers with retry capabilities
#[cfg(any(feature = "http-client", feature = "extism_host"))]
pub mod validated_llm;

/// Evaluator for LLM providers
pub mod evaluator;

pub mod pricing;

/// Core trait that all LLM providers must implement, combining chat, completion
/// and embedding capabilities into a unified interface
#[async_trait::async_trait]
pub trait LLMProvider:
    chat::BasicChatProvider
    + chat::ToolChatProvider
    + completion::CompletionProvider
    + embedding::EmbeddingProvider
{
    fn tools(&self) -> Option<&[Tool]> {
        None
    }

    async fn call_tool(&self, _name: &str, _args: Value) -> Result<String, error::LLMError> {
        Err(error::LLMError::ProviderError(
            "tool calling not supported".into(),
        ))
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
