//! Google Gemini vision example.
//!
//! Run:
//! ```sh
//! GOOGLE_API_KEY="your-key" cargo run -p querymt --example google_vision_example
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
    // Get Google API key from environment variable or use test key as fallback
    let api_key = std::env::var("GOOGLE_API_KEY").expect("Set GOOGLE_API_KEY to run this example");
    let registry = build_registry()?;

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .provider("google") // Use Google as the LLM provider
        .api_key(api_key) // Set the API key
        .model("gemini-3-flash-preview") // Use Gemini Flash model
        .max_tokens(8512) // Limit response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .system("You are a helpful AI assistant.")
        .build(&registry)
        .await?;

    let image_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/image001.jpg");
    let content = fs::read(image_path)?;

    // Prepare conversation history asking about the image
    let messages = vec![
        ChatMessage::user()
            .text("Explain what you see in the image")
            .build(),
        ChatMessage::user().image("image/jpeg", content).build(),
    ];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Google Gemini response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
