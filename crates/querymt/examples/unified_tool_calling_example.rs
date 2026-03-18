//! Unified tool-calling example across providers.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example unified_tool_calling_example -- openai simple
//! ```
//!
//! ```sh
//! ANTHROPIC_API_KEY="your-key" cargo run -p querymt --example unified_tool_calling_example -- anthropic multi
//! ```
//!
//! ```sh
//! GOOGLE_API_KEY="your-key" cargo run -p querymt --example unified_tool_calling_example -- google choice
//! ```
//!
//! ```sh
//! OLLAMA_URL="http://127.0.0.1:11434" \
//! OLLAMA_MODEL="qwen3:0.6b" \
//! cargo run -p querymt --example unified_tool_calling_example -- ollama simple
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.
//! Scenarios: `simple`, `multi`, `choice`.

use querymt::{
    LLMProvider, ToolCall,
    builder::{FunctionBuilder, LLMBuilder, ParamBuilder},
    chat::{ChatMessage, Content, Tool, ToolChoice},
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
};
use serde_json::{Value, json};
use std::{env, error::Error};

type ExampleResult<T> = Result<T, Box<dyn Error>>;

fn build_registry() -> ExampleResult<PluginRegistry> {
    let cfg_path = env::var("PROVIDER_CONFIG").unwrap_or_else(|_| "providers.toml".to_string());
    let mut registry = PluginRegistry::from_path(std::path::PathBuf::from(cfg_path))?;
    registry.register_loader(Box::new(ExtismLoader));
    Ok(registry)
}

fn sample_tools() -> Vec<Tool> {
    vec![
        FunctionBuilder::new("get_weather")
            .description("Get the weather for a city")
            .param(
                ParamBuilder::new("location")
                    .type_of("string")
                    .description("City and country, e.g. Tokyo, Japan"),
            )
            .required(["location"])
            .build(),
        FunctionBuilder::new("get_current_time")
            .description("Get local time for a timezone")
            .param(
                ParamBuilder::new("timezone")
                    .type_of("string")
                    .description("Timezone like UTC, JST, EST"),
            )
            .required(["timezone"])
            .build(),
        FunctionBuilder::new("search_restaurants")
            .description("Find restaurants by location and optional cuisine")
            .param(
                ParamBuilder::new("location")
                    .type_of("string")
                    .description("City or neighborhood"),
            )
            .param(
                ParamBuilder::new("cuisine")
                    .type_of("string")
                    .description("Optional cuisine filter"),
            )
            .required(["location"])
            .build(),
    ]
}

async fn create_llm(
    provider_name: &str,
    tool_choice: Option<ToolChoice>,
) -> ExampleResult<Box<dyn LLMProvider>> {
    let provider = provider_name.to_lowercase();
    let registry = build_registry()?;

    let mut builder = LLMBuilder::new()
        .provider(provider.clone())
        .max_tokens(1024)
        .temperature(0.2)
        .stream(false);

    builder = match provider.as_str() {
        "openai" => builder
            .api_key(env::var("OPENAI_API_KEY").expect("Set OPENAI_API_KEY to run this example"))
            .model(env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string())),
        "anthropic" => builder
            .api_key(
                env::var("ANTHROPIC_API_KEY").expect("Set ANTHROPIC_API_KEY to run this example"),
            )
            .model(env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string())),
        "google" => builder
            .api_key(env::var("GOOGLE_API_KEY").expect("Set GOOGLE_API_KEY to run this example"))
            .model(
                env::var("GOOGLE_MODEL").unwrap_or_else(|_| "gemini-3-flash-preview".to_string()),
            ),
        "ollama" => {
            let mut b = builder
                .base_url(
                    env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
                )
                .model(env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:0.6b".to_string()));
            if let Ok(api_key) = env::var("OLLAMA_API_KEY") {
                b = b.api_key(api_key);
            }
            b
        }
        _ => {
            return Err(format!(
                "Unsupported provider '{provider}'. Use one of: openai, anthropic, google, ollama"
            )
            .into());
        }
    };

    if let Some(choice) = tool_choice {
        builder = builder.tool_choice(choice);
    }

    Ok(builder.build(&registry).await?)
}

fn parse_args(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
}

fn execute_tool_call(call: &ToolCall) -> Value {
    let args = parse_args(&call.function.arguments);

    match call.function.name.as_str() {
        "get_weather" => {
            let location = args
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or("unknown location");
            json!({
                "location": location,
                "temperature_c": 22,
                "conditions": "Partly cloudy",
                "humidity": "65%"
            })
        }
        "get_current_time" => {
            let timezone = args
                .get("timezone")
                .and_then(Value::as_str)
                .unwrap_or("UTC");
            json!({
                "timezone": timezone,
                "time": "14:30",
                "date": "2026-03-07"
            })
        }
        "search_restaurants" => {
            let location = args
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or("unknown location");
            let cuisine = args.get("cuisine").and_then(Value::as_str).unwrap_or("any");
            json!({
                "location": location,
                "cuisine": cuisine,
                "restaurants": [
                    {"name": "Sushi Dai", "rating": 4.8},
                    {"name": "Tsukiji Sushisay", "rating": 4.6},
                    {"name": "Sukiyabashi Jiro", "rating": 4.9}
                ]
            })
        }
        _ => json!({"error": format!("Unknown function: {}", call.function.name)}),
    }
}

fn build_tool_result_message(tool_calls: &[ToolCall]) -> ExampleResult<ChatMessage> {
    let mut result_message = ChatMessage::user();

    for call in tool_calls {
        let result = execute_tool_call(call);
        result_message = result_message.tool_result(
            call.id.clone(),
            Some(call.function.name.clone()),
            false,
            vec![Content::text(serde_json::to_string(&result)?)],
        );
    }

    Ok(result_message.build())
}

async fn run_until_final_answer(
    llm: &dyn LLMProvider,
    tools: &[Tool],
    conversation: &mut Vec<ChatMessage>,
) -> ExampleResult<String> {
    loop {
        let response = llm.chat_with_tools(conversation, Some(tools)).await?;

        if let Some(tool_calls) = response.tool_calls() {
            println!("Model requested {} tool(s)", tool_calls.len());
            for call in &tool_calls {
                println!("- {}({})", call.function.name, call.function.arguments);
            }

            conversation.push(ChatMessage::from(response.as_ref()));
            conversation.push(build_tool_result_message(&tool_calls)?);
            continue;
        }

        let final_text = response.text().unwrap_or_default();
        conversation.push(ChatMessage::assistant().text(final_text.clone()).build());
        return Ok(final_text);
    }
}

async fn run_simple_scenario(llm: &dyn LLMProvider, tools: &[Tool]) -> ExampleResult<()> {
    println!("SCENARIO: simple");
    let mut conversation = vec![
        ChatMessage::user()
            .text("What's the weather in Tokyo? Use tools if needed.")
            .build(),
    ];

    let final_text = run_until_final_answer(llm, tools, &mut conversation).await?;
    println!("Assistant: {}", final_text);
    Ok(())
}

async fn run_multi_turn_scenario(llm: &dyn LLMProvider, tools: &[Tool]) -> ExampleResult<()> {
    println!("SCENARIO: multi");
    let mut conversation = Vec::new();

    let prompts = [
        "I'm planning a trip to Tokyo. What's the weather like there?",
        "What time is it there right now?",
        "Recommend a few sushi restaurants nearby.",
    ];

    for prompt in prompts {
        println!("\nUser: {}", prompt);
        conversation.push(ChatMessage::user().text(prompt).build());
        let final_text = run_until_final_answer(llm, tools, &mut conversation).await?;
        println!("Assistant: {}", final_text);
    }

    Ok(())
}

async fn run_tool_choice_scenario(provider_name: &str, tools: &[Tool]) -> ExampleResult<()> {
    println!("SCENARIO: choice");
    let query = "What's the weather in Tokyo and what time is it there?";

    let choices = [
        ("auto", ToolChoice::Auto),
        ("any", ToolChoice::Any),
        (
            "specific(get_weather)",
            ToolChoice::Tool("get_weather".to_string()),
        ),
        ("none", ToolChoice::None),
    ];

    for (label, choice) in choices {
        println!("\n--- Tool choice: {} ---", label);
        let llm = create_llm(provider_name, Some(choice)).await?;
        let mut conversation = vec![ChatMessage::user().text(query).build()];
        let final_text = run_until_final_answer(llm.as_ref(), tools, &mut conversation).await?;
        println!("Assistant: {}", final_text);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> ExampleResult<()> {
    let args: Vec<String> = env::args().collect();
    let provider = args.get(1).map(String::as_str).unwrap_or("openai");
    let scenario = args.get(2).map(String::as_str).unwrap_or("simple");

    println!("=== Unified Tool Calling Example ===");
    println!("Provider: {}", provider);
    println!("Scenario: {}", scenario);
    println!("====================================");

    let tools = sample_tools();

    match scenario {
        "simple" => {
            let llm = create_llm(provider, None).await?;
            run_simple_scenario(llm.as_ref(), &tools).await?
        }
        "multi" => {
            let llm = create_llm(provider, None).await?;
            run_multi_turn_scenario(llm.as_ref(), &tools).await?
        }
        "choice" => run_tool_choice_scenario(provider, &tools).await?,
        _ => {
            return Err(
                format!("Unknown scenario '{scenario}'. Use one of: simple, multi, choice").into(),
            );
        }
    }

    Ok(())
}
