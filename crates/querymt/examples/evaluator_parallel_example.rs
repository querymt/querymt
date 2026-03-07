//! Parallel evaluator example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" \
//! ANTHROPIC_API_KEY="your-key" \
//! GOOGLE_API_KEY="your-key" \
//! cargo run -p querymt --example evaluator_parallel_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    builder::LLMBuilder,
    chat::ChatMessage,
    evaluator::{ParallelEvalResult, ParallelEvaluator},
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

    let openai = LLMBuilder::new()
        .provider("openai")
        .api_key(std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "openai-key".to_string()))
        .model("gpt-4o")
        .max_tokens(512)
        .build(&registry)
        .await?;

    let anthropic = LLMBuilder::new()
        .provider("anthropic")
        .api_key(std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| "anthropic-key".to_string()))
        .model("claude-sonnet-4-6")
        .max_tokens(512)
        .build(&registry)
        .await?;

    let google = LLMBuilder::new()
        .provider("google")
        .api_key(std::env::var("GOOGLE_API_KEY").unwrap_or_else(|_| "google-key".to_string()))
        .model("gemini-3-flash-preview")
        .max_tokens(512)
        .build(&registry)
        .await?;

    let evaluator = ParallelEvaluator::new(vec![
        ("openai".to_string(), openai),
        ("anthropic".to_string(), anthropic),
        ("google".to_string(), google),
    ])
    .scoring(|response| response.len() as f32 * 0.1)
    .scoring(|response| {
        if response.contains("important") {
            10.0
        } else {
            0.0
        }
    });

    let messages = vec![ChatMessage::user()
        .text("Explain Einstein's theory of relativity in simple terms.")
        .build()];

    let results: Vec<ParallelEvalResult> = evaluator.evaluate_chat_parallel(&messages).await?;

    for result in &results {
        println!("Provider: {}", result.provider_id);
        println!("Score: {}", result.score);
        println!("Time: {}ms", result.time_ms);
        println!("---");
    }

    if let Some(best) = evaluator.best_response(&results) {
        println!("BEST RESPONSE:");
        println!("Provider: {}", best.provider_id);
        println!("Score: {}", best.score);
        println!("Time: {}ms", best.time_ms);
    }

    Ok(())
}
