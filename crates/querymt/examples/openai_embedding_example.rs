//! OpenAI embedding example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example openai_embedding_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{dynamic::PluginRegistryDynamicExt, plugin::host::PluginRegistry};

fn build_registry() -> Result<PluginRegistry, Box<dyn std::error::Error>> {
    let cfg_path =
        std::env::var("PROVIDER_CONFIG").unwrap_or_else(|_| "providers.toml".to_string());
    let registry =
        PluginRegistry::from_path(std::path::PathBuf::from(cfg_path))?.with_dynamic_loaders();
    Ok(registry)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = build_registry()?;

    // Initialize the LLM builder with OpenAI configuration
    let llm = registry
        .builder("openai")
        .api_key(std::env::var("OPENAI_API_KEY").expect("Set OPENAI_API_KEY to run this example"))
        .model("text-embedding-ada-002")
        // Optional: Uncomment to customize embedding format and dimensions
        // .embedding_encoding_format("base64")
        // .embedding_dimensions(1536)
        .build()
        .await?;

    // Generate embedding vector for sample text
    let vector = llm.embed(vec!["Hello world!".to_string()]).await?;

    // Print embedding statistics and data
    println!("Data: {:?}", &vector);

    Ok(())
}
