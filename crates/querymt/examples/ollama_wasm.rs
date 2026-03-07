//! Ollama example from a synchronous main with an explicit Tokio runtime.
//!
//! Run:
//! ```sh
//! OLLAMA_URL="http://127.0.0.1:11434" \
//! OLLAMA_MODEL="qwen3:0.6b" \
//! cargo run -p querymt --example ollama_wasm
//! ```
//!
//! If the model is missing, pull it first:
//! ```sh
//! curl -sS "http://127.0.0.1:11434/api/pull" \
//!   -H "Content-Type: application/json" \
//!   -d '{"name":"qwen3:0.6b"}'
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    builder::LLMBuilder,
    chat::ChatMessage,
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
};
use std::error::Error;

fn build_registry() -> Result<PluginRegistry, Box<dyn Error>> {
    let cfg_path =
        std::env::var("PROVIDER_CONFIG").unwrap_or_else(|_| "providers.toml".to_string());
    let mut registry = PluginRegistry::from_path(std::path::PathBuf::from(cfg_path))?;
    registry.register_loader(Box::new(ExtismLoader));
    Ok(registry)
}

fn main() -> Result<(), Box<dyn Error>> {
    // The plugin loader needs a Tokio runtime even in a non-async main.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        // Get Ollama server URL from environment variable or use default localhost.
        let base_url =
            std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:0.6b".to_string());
        let registry = build_registry()?;

        // Initialize and configure the LLM client.
        let llm = LLMBuilder::new()
            .provider("ollama") // Use Ollama as the LLM provider.
            .base_url(base_url) // Set the Ollama server URL.
            .model(model)
            .max_tokens(1000) // Set maximum response length.
            .temperature(0.7) // Control response randomness (0.0-1.0).
            .stream(false) // Disable streaming responses.
            .build(&registry)
            .await?;

        // Prepare conversation history with example messages.
        let messages = vec![
            ChatMessage::user()
                .text("Hello, how do I run a local LLM in Rust?")
                .build(),
            ChatMessage::assistant()
                .text("One way is to use Ollama with a local model!")
                .build(),
            ChatMessage::user().text("Tell me more about that").build(),
        ];

        let chat_response = llm.chat(&messages).await?;

        // Print the response from the chat.
        println!("Ollama chat response:\n{}", chat_response);

        Ok::<(), Box<dyn Error>>(())
    })
}
