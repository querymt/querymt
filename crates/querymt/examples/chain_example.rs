//! Prompt chain example (single provider).
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example chain_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    builder::LLMBuilder,
    chain::{ChainStepBuilder, ChainStepMode, PromptChain},
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

    // Initialize the LLM with OpenAI provider and configuration.
    let llm = LLMBuilder::new()
        .provider("openai")
        .api_key(std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "sk-TESTKEY".to_string()))
        .model("gpt-4o")
        .max_tokens(200)
        .temperature(0.7)
        .build(&registry)
        .await?;

    // Create and execute a 4-step prompt chain.
    let chain_result = PromptChain::new(&*llm)
        // Step 1: choose a programming language topic.
        .step(
            ChainStepBuilder::new("topic", "Suggest an interesting technical topic to explore among: Rust, Python, JavaScript, Go. Answer with a single word only.", ChainStepMode::Chat)
                .temperature(0.8) // Higher temperature for more variety in topic selection
                .build()
        )
        // Step 2: get advanced features for the chosen language.
        .step(
            ChainStepBuilder::new("features", "List 3 advanced features of {{topic}} that few developers know about. Format: one feature per line.", ChainStepMode::Chat)
                .build()
        )
        // Step 3: generate a code example for one feature.
        .step(
            ChainStepBuilder::new("example", "Choose one of the features listed in {{features}} and show a commented code example that illustrates it.", ChainStepMode::Chat)
                .build()
        )
        // Step 4: get a detailed explanation of the code example.
        .step(
            ChainStepBuilder::new("explanation", "Explain in detail how the code example {{example}} works and why this feature is useful.", ChainStepMode::Chat)
                .max_tokens(500) // Allow longer response for detailed explanation
                .build()
        )
        .run().await?;

    // Display the results from all chain steps.
    println!("Chain results: {:?}", chain_result);

    Ok(())
}
