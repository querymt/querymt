//! Google Gemini chat example.
//!
//! Run:
//! ```sh
//! GOOGLE_API_KEY="your-key" cargo run -p querymt --example google_example
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
    // Get Google API key from environment variable or use test key as fallback
    let api_key = std::env::var("GOOGLE_API_KEY").unwrap_or("google-key".into());
    let registry = build_registry()?;

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .provider("google") // Use Google as the LLM provider
        .api_key(api_key) // Set the API key
        .model("gemini-3-flash-preview") // Use Gemini Flash model
        .max_tokens(8512) // Limit response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        // Optional: Set system prompt
        .system("You are a helpful AI assistant specialized in programming.")
        .build(&registry)
        .await?;

    // Prepare conversation history with example messages
    let messages = vec![
        ChatMessage::user()
            .text("Explain the concept of async/await in Rust")
            .build(),
        ChatMessage::assistant()
            .text("Async/await in Rust is a way to write asynchronous code...")
            .build(),
        ChatMessage::user()
            .text("Can you show me a simple example?")
            .build(),
    ];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Google Gemini response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
