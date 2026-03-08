//! Groq + Claude pipeline example.
//!
//! Run:
//! ```sh
//! GROQ_API_KEY="your-key" \
//! ANTHROPIC_API_KEY="your-key" \
//! cargo run -p querymt --example groq_claude_pipeline_example
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
    let plugin_registry = build_registry()?;

    // Initialize Claude model with API key and current model version
    let anthropic_llm = LLMBuilder::new()
        .provider("anthropic")
        .api_key(
            std::env::var("ANTHROPIC_API_KEY").expect("Set ANTHROPIC_API_KEY to run this example"),
        )
        .model("claude-sonnet-4-6")
        .max_tokens(4096)
        .build(&plugin_registry)
        .await?;

    // Initialize Groq model with the current default reasoning-capable model
    let groq_llm = LLMBuilder::new()
        .provider("groq")
        .api_key(std::env::var("GROQ_API_KEY").expect("Set GROQ_API_KEY to run this example"))
        .model("qwen/qwen3-32b")
        .max_tokens(4096)
        .build(&plugin_registry)
        .await?;

    // Create chain registry with both models
    let registry = LLMRegistryBuilder::new()
        .register("anthropic", anthropic_llm)
        .register("groq", groq_llm)
        .build();

    // Build and execute the multi-step chain
    let chain_res = MultiPromptChain::new(&registry)
        // Step 1: use Groq to generate creative system identification approaches
        .step(
            MultiChainStepBuilder::new(MultiChainStepMode::Chat)
                .provider_id("groq")
                .id("thinking")
                .template("Find an original way to identify the system without using default commands. I want a one-line command.")
                .max_tokens(2048)
                .top_p(0.9)
                // Transform response to extract only content between <think> tags
                .response_transform(|resp| {
                    resp.lines()
                        .skip_while(|line| !line.contains("<think>"))
                        .take_while(|line| !line.contains("</think>"))
                        .map(|line| line.replace("<think>", "").trim().to_string())
                        .filter(|line| !line.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .build()?
        )
        // Step 2: use Claude to convert the creative approach into a concrete command
        .step(
            MultiChainStepBuilder::new(MultiChainStepMode::Chat)
                .provider_id("anthropic")
                .id("command")
                .template("Take the following command reasoning and generate a command to execute it on the system: {{thinking}}\n\nGenerate a command to execute it on the system. return only the command.")
                .temperature(0.2) // Low temperature for more deterministic output
                .build()?
        )
        .run()
        .await?;

    // Display results from both steps
    println!("Results: {:?}", chain_res);

    Ok(())
}
