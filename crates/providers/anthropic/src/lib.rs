//! Anthropic API client implementation for chat and completion functionality.
//!
//! This module provides integration with Anthropic's Claude models through their API.

mod auth;
mod config;
mod error;
mod factory;
mod model;
mod provider;
mod request;
mod response;
mod stream;
mod strict_schema;
mod thinking;
mod tool_names;
mod tools;
mod wire;

#[cfg(test)]
mod tests;

pub use config::{Anthropic, AuthType, detect_auth_type};
pub use factory::create_http_factory;
pub use wire::system::{
    AnthropicSystemPrompt, CacheControlEphemeral, CacheTTL, CitationCharLocationParam,
    CitationContentBlockLocationParam, CitationPageLocationParam,
    CitationSearchResultLocationParam, CitationWebSearchResultLocationParam, TextBlockParam,
    TextCitationParam,
};
