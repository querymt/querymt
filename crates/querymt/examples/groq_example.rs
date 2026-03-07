//! Groq chat example.
//!
//! Run:
//! ```sh
//! GROQ_API_KEY="your-key" cargo run -p querymt --example groq_example
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
    // Get Groq API key from environment variable or use test key as fallback
    let api_key = std::env::var("GROQ_API_KEY").expect("Set GROQ_API_KEY to run this example");
    let registry = build_registry()?;

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .provider("groq") // Use Groq as the LLM provider
        .api_key(api_key) // Set the API key
        .model("openai/gpt-oss-20b") // Use openai/gpt-oss-20b model
        .max_tokens(512) // Limit response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .build(&registry)
        .await?;

    // Prepare conversation history with example messages
    let messages = vec![
        ChatMessage::user()
            .text("Tell me about quantum computing")
            .build(),
        ChatMessage::assistant()
            .text("Quantum computing is a type of computing that uses quantum phenomena...")
            .build(),
        ChatMessage::user().text("What are qubits?").build(),
    ];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Chat response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
