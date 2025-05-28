use llm::{
    builder::{LLMBackend, LLMBuilder},
    chat::ChatMessage,
};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // Get Ollama server URL from environment variable or use default localhost
    let base_url = "https://ollama.wik.ing"; //std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into());

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .backend(LLMBackend::Ollama) // Use Ollama as the LLM backend
        .base_url(base_url) // Set the Ollama server URL
        .model("qwq:32b")
        .max_tokens(1000) // Set maximum response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .build()
        .expect("Failed to build LLM (Ollama)");

    // Prepare conversation history with example messages
    let messages = vec![
        ChatMessage::user()
            .content("Hello, how do I run a local LLM in Rust?")
            .build(),
        ChatMessage::assistant()
            .content("One way is to use Ollama with a local model!")
            .build(),
        ChatMessage::user()
            .content("Tell me more about that")
            .build(),
    ];

    // Use futures::executor::block_on to run the async operation synchronously.
    let chat_response = futures::executor::block_on(llm.chat(&messages))?;
    // Print the response from the chat
    println!("Ollama chat response:\n{}", chat_response);

    Ok(())
}
