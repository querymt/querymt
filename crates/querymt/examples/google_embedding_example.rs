//! Google embedding example.
//!
//! Run:
//! ```sh
//! GOOGLE_API_KEY="your-key" cargo run -p querymt --example google_embedding_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.
//! TODO: Google embedding support in the provider is not implemented yet.

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

    // Initialize the LLM builder with Google configuration
    let llm = registry
        .builder("google")
        .api_key(std::env::var("GOOGLE_API_KEY").expect("Set GOOGLE_API_KEY to run this example"))
        // Use Google's text embedding model
        .model("text-embedding-004")
        .build()
        .await?;

    // TODO: This call fails at runtime until provider embedding support is implemented
    // Generate embedding vector for sample text
    let vector = llm.embed(vec!["Hello world!".to_string()]).await?;

    // Print embedding statistics and data
    println!("Data: {:?}", &vector);

    Ok(())
}
