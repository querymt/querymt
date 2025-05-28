//! Example demonstrating how to serve multiple LLM backends through a REST API
//!
//! This example shows how to chain multiple LLM providers together to:
//! 1. Use Groq to generate creative system identification approaches
//! 2. Use Claude to convert those ideas into executable commands
//! 3. Expose the chain through a REST API
//!
//! # Example Request
//! ```json
//! POST http://127.0.0.1:3000/v1/chat/completions
//! {
//!     "steps": [
//!         {
//!             "provider_id": "groq",
//!             "id": "thinking",
//!             "template": "Find an original way to identify the system without using default commands. I want a one-line command.",
//!             "response_transform": "extract_think",
//!             "temperature": 0.7
//!         },
//!         {
//!             "provider_id": "anthropic",
//!             "id": "step2",
//!             "template": "Take the following command reasoning and generate a command to execute it on the system: {{thinking}}\n\nGenerate a command to execute it on the system. return only the command.",
//!             "max_tokens": 5000
//!         }
//!     ]
//! }
//! ```

use llm::{
    builder::{LLMBackend, LLMBuilder},
    chain::LLMRegistryBuilder,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize Anthropic backend with API key and model settings
    let anthro_llm = LLMBuilder::new()
        .backend(LLMBackend::Anthropic)
        .api_key(std::env::var("ANTHROPIC_API_KEY").unwrap_or("anthro-key".into()))
        .model("claude-3-5-sonnet-20240620")
        .build()?;

    // Initialize Groq backend with API key and model settings
    let groq_llm = LLMBuilder::new()
        .backend(LLMBackend::Groq)
        .api_key(std::env::var("GROQ_API_KEY").unwrap_or("gsk-YOUR_API_KEY".into()))
        .model("deepseek-r1-distill-llama-70b")
        .build()?;

    // Create registry to manage multiple backends
    let registry = LLMRegistryBuilder::new()
        .register("anthropic", anthro_llm)
        .register("groq", groq_llm)
        .build();

    // Start REST API server on localhost port 3000
    registry.serve("127.0.0.1:3000").await?;
    Ok(())
}
