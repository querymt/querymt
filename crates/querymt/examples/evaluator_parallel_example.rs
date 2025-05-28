use llm::{
    builder::{LLMBackend, LLMBuilder},
    chat::{ChatMessage, ChatRole},
    evaluator::ParallelEvaluator,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let openai = LLMBuilder::new()
        .backend(LLMBackend::OpenAI)
        .api_key(std::env::var("OPENAI_API_KEY").unwrap_or("openai-key".into()))
        .model("gpt-4o")
        .build()?;
    
    let anthropic = LLMBuilder::new()
        .backend(LLMBackend::Anthropic)
        .api_key(std::env::var("ANTHROPIC_API_KEY").unwrap_or("anthropic-key".into()))
        .model("claude-3-7-sonnet-20250219")
        .build()?;
    
    let google = LLMBuilder::new()
        .backend(LLMBackend::Google)
        .api_key(std::env::var("GOOGLE_API_KEY").unwrap_or("google-key".into()))
        .model("gemini-2.0-flash-exp")
        .build()?;
    
    let evaluator = ParallelEvaluator::new(vec![
        ("openai".to_string(), openai),
        ("anthropic".to_string(), anthropic),
        ("google".to_string(), google),
    ])
    .scoring(|response| {
        response.len() as f32 * 0.1
    })
    .scoring(|response| {
        if response.contains("important") {
            10.0
        } else {
            0.0
        }
    });
    
    let messages = vec![
        ChatMessage {
            role: ChatRole::User,
            message_type: Default::default(),
            content: "Explique-moi la théorie de la relativité d'Einstein".to_string(),
        }
    ];
    
    let results = evaluator.evaluate_chat_parallel(&messages).await?;
    
    for result in &results {
        println!("Provider: {}", result.provider_id);
        println!("Score: {}", result.score);
        println!("Time: {}ms", result.time_ms);
        // println!("Response: {}", result.text);
        println!("---");
    }
    
    if let Some(best) = evaluator.best_response(&results) {
        println!("BEST RESPONSE:");
        println!("Provider: {}", best.provider_id);
        println!("Score: {}", best.score);
        println!("Time: {}ms", best.time_ms);
        // println!("Response: {}", best.text);
    }
    
    Ok(())
}