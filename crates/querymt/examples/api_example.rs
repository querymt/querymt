//! Example demonstrating how to serve multiple LLM backends through a REST API
//!
//! This example shows how to chain multiple LLM providers together to:
//! 1. Use Groq to perform initial calculation
//! 2. Use Claude to provide analysis and commentary
//! 3. Use GPT to improve and format the results
//! 4. Expose the chain through a REST API
//!
//! # Example Request
//! ```json
//! POST http://127.0.0.1:3000/v1/chat/completions
//! {
//!     "model": "groq:deepseek-r1-distill-llama-70b",
//!     "messages": [
//!         {"role": "user", "content": "calcule 1x20"}
//!     ],
//!     "steps": [
//!         {
//!             "provider_id": "anthropic",
//!             "id": "step1",
//!             "template": "Analyze and comment on this calculation: {{initial}}",
//!             "temperature": 0.7
//!         },
//!         {
//!             "provider_id": "openai",
//!             "id": "step2",
//!             "template": "Improve and expand upon this mathematical analysis: {{step1}}",
//!             "max_tokens": 500
//!         },
//!         {
//!             "provider_id": "openai",
//!             "id": "step3",
//!             "template": "Format the following into a clear report:\nCalculation: {{initial}}\nAnalysis: {{step1}}\nExpanded Analysis: {{step2}}"
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
    // Initialize OpenAI backend with API key and model settings
    let openai_llm = LLMBuilder::new()
        .backend(LLMBackend::OpenAI)
        .api_key(std::env::var("OPENAI_API_KEY").unwrap_or("sk-OPENAI".into()))
        .model("gpt-4")
        .build()?;

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
        .register("openai", openai_llm)
        .register("anthropic", anthro_llm)
        .register("groq", groq_llm)
        .build();

    // Start REST API server on localhost port 3000
    registry.serve("127.0.0.1:3000").await?;
    Ok(())
}
