//! OpenAI embedding example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example openai_embedding_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    builder::LLMBuilder,
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
};

fn build_registry() -> Result<PluginRegistry, Box<dyn std::error::Error>> {
    let cfg_path =
        std::env::var("PROVIDER_CONFIG").unwrap_or_else(|_| "providers.toml".to_string());
    let mut registry = PluginRegistry::from_path(std::path::PathBuf::from(cfg_path))?;
    registry.register_loader(Box::new(ExtismLoader));
    Ok(registry)
}

/// Example demonstrating how to generate embeddings using OpenAI's API
///
/// This example shows how to:
/// - Configure an OpenAI LLM provider
/// - Generate embeddings for text input
/// - Access and display the resulting embedding vector
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = build_registry()?;

    // Initialize the LLM builder with OpenAI configuration
    let llm = LLMBuilder::new()
        .provider("openai") // .provider("ollama") or .provider("xai")
        // Get API key from environment variable or use test key
        .api_key(std::env::var("OPENAI_API_KEY").unwrap_or("sk-TESTKEY".to_string()))
        // Use OpenAI's text embedding model
        .model("text-embedding-ada-002") // .model("v1") or .model("all-minilm")
        // Optional: Uncomment to customize embedding format and dimensions
        // .embedding_encoding_format("base64")
        // .embedding_dimensions(1536)
        .build(&registry)
        .await?;

    // Generate embedding vector for sample text
    let vector = llm.embed(vec!["Hello world!".to_string()]).await?;

    // Print embedding statistics and data
    println!("Data: {:?}", &vector);

    Ok(())
}
