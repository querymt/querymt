//! OpenAI vision example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example openai_vision_example
//! ```
//!
//! Uses `examples/image001.jpg`.
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use std::fs;
use std::path::Path;

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
    // Get OpenAI API key from environment variable or use test key as fallback
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or("openai-key".into());
    let registry = build_registry()?;

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .provider("openai") // Use OpenAI as the LLM provider
        .api_key(api_key) // Set the API key
        .model("gpt-4o") // Use GPT-4o model
        .max_tokens(1024) // Limit response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .build(&registry)
        .await?;

    let image_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/image001.jpg");
    let content = fs::read(image_path)?;

    // Prepare conversation history with example messages
    let messages = vec![
        ChatMessage::user().image_url("https://media.istockphoto.com/id/1443562748/fr/photo/mignon-chat-gingembre.jpg?s=612x612&w=0&k=20&c=ygNVVnqLk9V8BWu4VQ0D21u7-daIyHUoyKlCcx3K1E8=").build(),
        ChatMessage::user().image("image/jpeg", content).build(),
        ChatMessage::user()
            .text("What is in this image (image 1 and 2)?")
            .build(),
    ];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Chat response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
