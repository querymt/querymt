//! OpenAI structured output example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example openai_structured_output_example
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
    // Get OpenAI API key from environment variable or use test key as fallback
    let api_key = std::env::var("OPENAI_API_KEY").expect("Set OPENAI_API_KEY to run this example");
    let registry = build_registry()?;

    // Define a simple JSON schema for structured output
    // Note: the schema has some odd requirements for OpenAI structured outputs
    // (see https://platform.openai.com/docs/guides/structured-outputs#supported-schemas)
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
                "required": ["name", "age", "is_student"],
                "additionalProperties": false
            }
        }
    "#;

    let schema: StructuredOutputFormat = serde_json::from_str(schema)?;

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .provider("openai") // Use OpenAI as the LLM provider
        .api_key(api_key) // Set the API key
        .model("gpt-4o") // Use GPT-4o model
        .max_tokens(512) // Limit response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .system("You are an AI assistant that can provide structured output to generate random students as example data. Respond in JSON format using the provided JSON schema.") // Set system description
        .schema(schema) // Set JSON schema for structured output
        .build(&registry)
        .await?;

    // Prepare conversation history with example messages
    let messages = vec![ChatMessage::user()
        .text("Generate a random student")
        .build()];

    // Send chat request and handle the response
    match llm.chat(&messages).await {
        Ok(text) => println!("Chat response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
