//! OpenAI tool-calling example.
//!
//! Run:
//! ```sh
//! OPENAI_API_KEY="your-key" cargo run -p querymt --example tool_calling_example
//! ```
//!
//! Optional: set `PROVIDER_CONFIG` to a custom providers file path.

use querymt::{
    builder::{FunctionBuilder, LLMBuilder, ParamBuilder},
    chat::{ChatMessage, Content, Tool},
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
};
use serde_json::{json, Value};

fn build_registry() -> Result<PluginRegistry, Box<dyn std::error::Error>> {
    let cfg_path =
        std::env::var("PROVIDER_CONFIG").unwrap_or_else(|_| "providers.toml".to_string());
    let mut registry = PluginRegistry::from_path(std::path::PathBuf::from(cfg_path))?;
    registry.register_loader(Box::new(ExtismLoader));
    Ok(registry)
}

fn weather_tool() -> Tool {
    FunctionBuilder::new("get_weather")
        .description("Get the current weather in a specific location")
        .param(
            ParamBuilder::new("location")
                .type_of("string")
                .description("City and country, e.g. 'Tokyo, Japan'"),
        )
        .required(["location"])
        .build()
}

fn execute_tool_call(name: &str, args: &Value) -> Value {
    match name {
        "get_weather" => {
            let location = args
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            json!({
                "location": location,
                "temperature_c": 22,
                "conditions": "Partly cloudy",
                "wind_kph": 11
            })
        }
        _ => json!({"error": format!("Unknown tool: {name}")}),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "openai-key".to_string());
    let registry = build_registry()?;

    let llm = LLMBuilder::new()
        .provider("openai")
        .api_key(api_key)
        .model("gpt-4o")
        .max_tokens(512)
        .temperature(0.2)
        .stream(false)
        .build(&registry)
        .await?;

    let tools = vec![weather_tool()];
    let mut messages = vec![ChatMessage::user()
        .text("You are a weather assistant. What is the weather in Tokyo right now?")
        .build()];

    let first_response = llm.chat_with_tools(&messages, Some(&tools)).await?;
    if let Some(tool_calls) = first_response.tool_calls() {
        println!("Model requested {} tool(s)", tool_calls.len());

        let mut assistant_message = ChatMessage::assistant();
        if let Some(text) = first_response.text() {
            if !text.is_empty() {
                assistant_message = assistant_message.text(text);
            }
        }

        for call in &tool_calls {
            let args = serde_json::from_str::<Value>(&call.function.arguments)
                .unwrap_or_else(|_| json!({}));
            assistant_message =
                assistant_message.tool_use(call.id.clone(), call.function.name.clone(), args);
        }
        messages.push(assistant_message.build());

        let mut tool_result_message = ChatMessage::user();
        for call in tool_calls {
            let args = serde_json::from_str::<Value>(&call.function.arguments)
                .unwrap_or_else(|_| json!({}));
            let result = execute_tool_call(&call.function.name, &args);

            tool_result_message = tool_result_message.tool_result(
                call.id,
                Some(call.function.name),
                false,
                vec![Content::text(serde_json::to_string(&result)?)],
            );
        }
        messages.push(tool_result_message.build());

        let final_response = llm.chat_with_tools(&messages, Some(&tools)).await?;
        println!("Final response:\n{}", final_response);
    } else {
        println!("Model answered directly:\n{}", first_response);
    }

    Ok(())
}
