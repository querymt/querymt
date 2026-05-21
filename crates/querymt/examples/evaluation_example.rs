//! Multi-provider evaluation example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" \
//! ANTHROPIC_API_KEY="your-key" \
//! GROQ_API_KEY="your-key" \
//! cargo run -p querymt --example evaluation_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    chat::ChatMessage,
    dynamic::PluginRegistryDynamicExt,
    evaluator::{EvalResult, LLMEvaluator},
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
    let registry = build_registry()?;

    // Initialize OpenAI provider
    let openai = registry
        .builder("openai")
        .api_key(std::env::var("OPENAI_API_KEY").expect("Set OPENAI_API_KEY to run this example"))
        .model("gpt-4o")
        .max_tokens(700)
        .build()
        .await?;

    // Initialize Anthropic provider
    let anthropic = registry
        .builder("anthropic")
        .api_key(
            std::env::var("ANTHROPIC_API_KEY").expect("Set ANTHROPIC_API_KEY to run this example"),
        )
        .model("claude-sonnet-4-6")
        .max_tokens(700)
        .build()
        .await?;

    // Initialize Groq provider
    let groq = registry
        .builder("groq")
        .api_key(std::env::var("GROQ_API_KEY").expect("Set GROQ_API_KEY to run this example"))
        .model("openai/gpt-oss-20b")
        .max_tokens(700)
        .build()
        .await?;

    // Create evaluator with multiple scoring functions
    let evaluator = LLMEvaluator::new(vec![openai, anthropic, groq])
        // First scoring function: evaluate code quality and completeness
        .scoring(|response| {
            let mut score = 0.0;

            if response.contains("```") {
                score += 1.0;

                if response.contains("```rust") {
                    score += 2.0;
                }

                if response.contains("use actix_web::") {
                    score += 2.0;
                }
                if response.contains("async fn") {
                    score += 1.0;
                }
                if response.contains("#[derive(") {
                    score += 1.0;
                }
                if response.contains("//") {
                    score += 1.0;
                }
            }

            score
        })
        // Second scoring function: evaluate explanation quality
        .scoring(|response| {
            let mut score = 0.0;

            if response.contains("Here's how it works:") || response.contains("Let me explain:") {
                score += 2.0;
            }

            if response.contains("For example") || response.contains("curl") {
                score += 1.5;
            }

            let words = response.split_whitespace().count();
            if words > 100 {
                score += 1.0;
            }

            score
        });

    // Define the evaluation prompt requesting a Rust microservice implementation
    let messages = vec![
        ChatMessage::user()
            .text(
                "\
            Create a Rust microservice using Actix Web.
            It should have at least two routes:
            1) A GET route returning a simple JSON status.
            2) A POST route that accepts JSON data and responds with a success message.\n\
            Include async usage, data structures with `#[derive(Serialize, Deserialize)]`, \
            and show how to run it.\n\
            Provide code blocks, comments, and a brief explanation of how it works.\
        ",
            )
            .build(),
    ];

    // Run evaluation across all providers
    let results: Vec<EvalResult> = evaluator.evaluate_chat(&messages).await?;

    // Display results with scores
    for (i, item) in results.iter().enumerate() {
        println!("\n=== LLM #{} ===", i + 1);
        println!("Score: {:.2}", item.score);
        println!("Response:\n{}", item.text);
        println!("================\n");
    }

    Ok(())
}
