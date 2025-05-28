// Import required modules from the LLM library for OpenAI integration
use llm::{
    builder::{FunctionBuilder, LLMBuilder, ParamBuilder},
    chat::ChatMessage, // Chat-related structures
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get OpenAI API key from environment variable or use test key as fallback
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or("sk-TESTKEY".into());

    // Initialize and configure the LLM client
    let llm = LLMBuilder::new()
        .provider("openai".to_string())
        .api_key(api_key) // Set the API key
        .model("gpt-4o")
        .max_tokens(512) // Limit response length
        .temperature(0.7) // Control response randomness (0.0-1.0)
        .stream(false) // Disable streaming responses
        .function(
            FunctionBuilder::new("get_weather")
                .description("Get the current weather in a specific location")
                .param(ParamBuilder::new("location").type_of("string").description(
                    "The city and state/country, e.g., 'San Francisco, CA' or 'Tokyo, Japan'",
                ))
                .required(vec!["location".to_string()]),
        )
        .build()
        .expect("Failed to build LLM");

    // Prepare conversation history with example messages
    let messages = vec![ChatMessage::user().content("You are a weather assistant. What is the weather in Tokyo? Use the tools that you have available").build()];

    // Send chat request and handle the response
    // this returns the response as a string. The tool call is also returned as a serialized string. We can deserialize if needed.
    match llm.chat_with_tools(&messages, llm.tools()).await {
        Ok(text) => println!("Chat response:\n{}", text),
        Err(e) => eprintln!("Chat error: {}", e),
    }

    Ok(())
}
