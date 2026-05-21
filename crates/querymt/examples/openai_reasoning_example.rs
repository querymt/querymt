//! OpenAI reasoning effort example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example openai_reasoning_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    chat::{ChatMessage, ReasoningEffort},
    dynamic::PluginRegistryDynamicExt,
    plugin::host::PluginRegistry,
};

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
        .builder("openai") // Use OpenAI as the LLM provider
        .api_key(api_key) // Set the API key
        .model("gpt-5.2") // Use GPT-5.2 model
        .stream(false) // Disable streaming responses
        .reasoning_effort(ReasoningEffort::High) // Set reasoning effort level
        .build()
        .await?;

    // Prepare conversation history with example messages
    let messages = vec![
        ChatMessage::user()
            .text("How many r's is in `strawberry`?")
            .build(),
    ];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Chat response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
