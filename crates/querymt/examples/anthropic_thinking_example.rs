//! Anthropic reasoning example.
//!
//! Run:
//! ```sh
//! ANTHROPIC_API_KEY="your-key" cargo run -p querymt --example anthropic_thinking_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    builder::LLMBuilder,
    chat::ChatMessage,
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
};

fn build_registry() -> Result<PluginRegistry, Box<dyn std::error::Error>> {
    let cfg_path =
        std::env::var("PROVIDER_CONFIG").unwrap_or_else(|_| "providers.toml".to_string());
    let mut registry = PluginRegistry::from_path(std::path::PathBuf::from(cfg_path))?;
    registry.register_loader(Box::new(ExtismLoader));
    Ok(registry)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get Anthropic API key from environment variable or use test key as fallback
    let api_key: String = std::env::var("ANTHROPIC_API_KEY").unwrap_or("anthro-key".into());
    let registry = build_registry()?;

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .provider("anthropic") // Use Anthropic (Claude) as the LLM provider
        .api_key(api_key) // Set the API key
        .model("claude-sonnet-4-6") // Use Claude Instant model
        .max_tokens(1500) // Limit response length
        .temperature(1.0) // Control response randomness (0.0-1.0)
        .reasoning(true)
        .reasoning_budget_tokens(1024)
        // Uncomment to set system prompt:
        // .system("You are a helpful assistant specialized in concurrency.")
        .build(&registry)
        .await?;

    // Prepare conversation history with example message about Rust concurrency
    let messages = vec![ChatMessage::user()
        .text("How much r in strawberry?")
        .build()];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => {
            if let Some(thinking) = text.thinking() {
                println!("Thinking: {}", thinking);
            }
            if let Some(text) = text.text() {
                println!("Text: {}", text);
            }
        }
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
