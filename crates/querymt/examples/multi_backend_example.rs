//! Multi-provider chain example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" \
//! ANTHROPIC_API_KEY="your-key" \
//! GROQ_API_KEY="your-key" \
//! cargo run -p querymt --example multi_backend_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    builder::LLMBuilder,
    chain::{LLMRegistryBuilder, MultiChainStepBuilder, MultiChainStepMode, MultiPromptChain},
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

    // Initialize OpenAI provider with API key and model settings.
    let openai_llm = LLMBuilder::new()
        .provider("openai")
        .api_key(std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "sk-OPENAI".to_string()))
        .model("gpt-4o")
        .max_tokens(512)
        .build(&registry)
        .await?;

    // Initialize Anthropic provider with API key and model settings.
    let anthropic_llm = LLMBuilder::new()
        .provider("anthropic")
        .api_key(std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| "anthro-key".to_string()))
        .model("claude-sonnet-4-6")
        .max_tokens(512)
        .build(&registry)
        .await?;

    // Initialize Groq provider with API key and model settings.
    let groq_llm = LLMBuilder::new()
        .provider("groq")
        .api_key(std::env::var("GROQ_API_KEY").unwrap_or_else(|_| "gsk-TESTKEY".to_string()))
        .model("openai/gpt-oss-20b")
        .max_tokens(512)
        .build(&registry)
        .await?;

    // Create registry to manage multiple providers.
    let registry = LLMRegistryBuilder::new()
        .register("openai", openai_llm)
        .register("anthropic", anthropic_llm)
        .register("groq", groq_llm)
        .build();

    // Build multi-step chain using different providers.
    let chain_res = MultiPromptChain::new(&registry)
        // Step 1: use OpenAI to analyze a code problem.
        .step(
            MultiChainStepBuilder::new(MultiChainStepMode::Chat)
                .provider_id("openai")
                .id("analysis")
                .template("Analyze this Rust code and identify potential performance issues:\n```rust\nfn process_data(data: Vec<i32>) -> Vec<i32> {\n    data.iter().map(|x| x * 2).collect()\n}```")
                .temperature(0.7)
                .build()?
        )
        // Step 2: use Anthropic to suggest optimizations based on analysis.
        .step(
            MultiChainStepBuilder::new(MultiChainStepMode::Chat)
                .provider_id("anthropic")
                .id("optimization")
                .template("Here is a code analysis: {{analysis}}\n\nSuggest concrete optimizations to improve performance, explaining why they would be beneficial.")
                .max_tokens(500)
                .top_p(0.9)
                .build()?
        )
        // Step 3: use Groq to generate optimized code.
        .step(
            MultiChainStepBuilder::new(MultiChainStepMode::Chat)
                .provider_id("groq")
                .id("implementation")
                .template("Taking into account these optimization suggestions: {{optimization}}\n\nGenerate an optimized version of the code in Rust with explanatory comments.")
                .temperature(0.2)
                .build()?
        )
        .run().await?;

    // Display results from all steps.
    println!("Results: {:?}", chain_res);

    Ok(())
}
