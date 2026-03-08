//! Anthropic response validation example.
//!
//! Run:
//! ```sh
//! ANTHROPIC_API_KEY="your-key" cargo run -p querymt --example validator_example
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
    // Retrieve Anthropic API key from environment variable or use fallback
    let api_key =
        std::env::var("ANTHROPIC_API_KEY").expect("Set ANTHROPIC_API_KEY to run this example");
    let registry = build_registry()?;

    // Initialize and configure the LLM client with validation
    let llm = LLMBuilder::new()
        .provider("anthropic") // Use Anthropic's Claude model
        .model("claude-sonnet-4-6") // Use Claude Sonnet model
        .api_key(api_key) // Set API credentials
        .max_tokens(512) // Limit response length
        .temperature(0.7) // Control response randomness
        .stream(false) // Disable streaming responses
        .validator(|resp| {
            // Add JSON validation
            serde_json::from_str::<serde_json::Value>(resp)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .validator_attempts(3) // Allow up to 3 retries on validation failure
        .build(&registry)
        .await?;

    // Prepare the chat message requesting JSON output
    let messages = vec![
        ChatMessage::user().text("Please give me a valid JSON describing a cat named Garfield, color 'orange'. with format {name: string, color: string}. Return only the JSON, no other text").build(),
    ];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
