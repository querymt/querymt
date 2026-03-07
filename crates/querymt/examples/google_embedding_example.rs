//! Google embedding example.
//!
//! Run:
//! ```sh
//! GOOGLE_API_KEY="your-key" cargo run -p querymt --example google_embedding_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.
//! TODO: Google embedding support in the provider is not implemented yet.

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = build_registry()?;

    // Initialize the LLM builder with Google configuration
    let llm = LLMBuilder::new()
        .provider("google")
        .api_key(std::env::var("GOOGLE_API_KEY").expect("Set GOOGLE_API_KEY to run this example"))
        // Use Google's text embedding model
        .model("text-embedding-004")
        .build(&registry)
        .await?;

    // TODO: This call fails at runtime until provider embedding support is implemented
    // Generate embedding vector for sample text
    let vector = llm.embed(vec!["Hello world!".to_string()]).await?;

    // Print embedding statistics and data
    println!("Data: {:?}", &vector);

    Ok(())
}
