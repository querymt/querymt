//! Ollama structured output example.
//!
//! Run:
//! ```sh
//! OLLAMA_URL="http://127.0.0.1:11434" \
//! OLLAMA_MODEL="qwen3:0.6b" \
//! cargo run -p querymt --example ollama_structured_output_example
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
    chat::{ChatMessage, StructuredOutputFormat},
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
    // Get Ollama server URL from environment variable or use default localhost
    let base_url = std::env::var("OLLAMA_URL").unwrap_or("http://127.0.0.1:11434".into());
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:0.6b".to_string());
    let registry = build_registry()?;

    // Define a simple JSON schema for structured output
    let schema = r#"
        {
            "name": "Student",
            "schema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string"
                    },
                    "age": {
                        "type": "integer"
                    },
                    "is_student": {
                        "type": "boolean"
                    }
                },
                "required": ["name", "age", "is_student"]
            }
        }
    "#;
    let schema: StructuredOutputFormat = serde_json::from_str(schema)?;

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .provider("ollama") // Use Ollama as the LLM provider
        .base_url(base_url) // Set the Ollama server URL
        .model(model)
        .max_tokens(1000) // Set maximum response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .schema(schema) // Set JSON schema for structured output
        .system("You are a helpful AI assistant. Please generate a random student using the provided JSON schema.")
        .build(&registry)
        .await?;

    // Prepare conversation history with example messages
    let messages = vec![ChatMessage::user()
        .text("Please generate a random student using the provided JSON schema.")
        .build()];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Ollama chat response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
