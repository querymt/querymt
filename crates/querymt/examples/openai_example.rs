//! OpenAI chat example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example openai_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{chat::ChatMessage, dynamic::PluginRegistryDynamicExt, plugin::host::PluginRegistry};

fn build_registry() -> Result<PluginRegistry, Box<dyn std::error::Error>> {
    let cfg_path =
        std::env::var("PROVIDER_CONFIG").unwrap_or_else(|_| "providers.toml".to_string());
    let registry =
        PluginRegistry::from_path(std::path::PathBuf::from(cfg_path))?.with_dynamic_loaders();
    Ok(registry)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get OpenAI API key from environment variable or use test key as fallback
    let api_key = std::env::var("OPENAI_API_KEY").expect("Set OPENAI_API_KEY to run this example");
    let registry = build_registry()?;

    // Initialize and configure the LLM client
    let llm = registry
        .builder("openai")
        .api_key(api_key) // Set the API key
        .model("gpt-4o-mini") // Use GPT-4o Mini model
        .max_tokens(512) // Limit response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .build()
        .await?;

    // Prepare conversation history with example messages
    let messages = vec![
        ChatMessage::user()
            .text("Tell me that you love cats")
            .build(),
        ChatMessage::assistant()
            .text("I am an assistant, I cannot love cats but I can love dogs")
            .build(),
        ChatMessage::user()
            .text("Tell me that you love dogs in 2000 chars")
            .build(),
    ];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Chat response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
