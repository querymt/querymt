//! xAI structured output example.
//!
//! Run:
//! ```sh
//! XAI_API_KEY="your-key" cargo run -p querymt --example xai_structured_output_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    chat::{ChatMessage, StructuredOutputFormat},
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
    // Get xAI API key from environment variable or use test key as fallback
    let api_key = std::env::var("XAI_API_KEY").expect("Set XAI_API_KEY to run this example");
    let registry = build_registry()?;

    // Define a simple JSON schema for structured output
    // For XAI, the schema must be provided in the property "schema"
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
    let llm = registry.builder("xai") // Use xAI as the LLM provider
        .api_key(api_key) // Set the API key
        .model("grok-4-1-fast-reasoning") // Use `grok-4-1-fast-reasoning` model
        .max_tokens(512) // Limit response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .system("You are a helpful AI assistant. Please generate a random student using the provided JSON schema.")
        .schema(schema) // Set JSON schema for structured output
        .build()
        .await?;

    // Prepare conversation history with example messages
    let messages = vec![
        ChatMessage::user()
            .text("Please generate a random student using the provided JSON schema.")
            .build(),
    ];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Chat response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
