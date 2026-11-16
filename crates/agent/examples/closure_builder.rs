/// Example demonstrating the closure-based delegate builder API
///
/// This shows how to give one agent several delegates using closures for
/// a clean, self-documenting configuration style.
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build an agent with delegates, using closure-based configuration
    let runner = Agent::builder()
        .cwd(".")
        .db("./delegates.db")
        .provider("openai", "gpt-4")
        .system(
            "You are a principal engineer coordinating specialist agents. \
            Delegate implementation to the coder and research to the researcher.",
        )
        .tools(["delegate", "create_task", "read_tool", "search_text"])
        // Add a coder delegate
        .delegate("coder", |d| {
            d.provider("ollama", "qwen2.5-coder:latest")
                .description("Expert coder for implementation tasks")
                .capabilities(["rust", "python", "typescript"])
                .system(
                    "You are an expert coder. Implement features with clean, \
                well-tested code. Always verify your changes compile.",
                )
                .tools([
                    "edit",
                    "shell",
                    "read_tool",
                    "write_file",
                    "glob",
                    "search_text",
                ])
        })
        // Add a researcher delegate
        .delegate("researcher", |d| {
            d.provider("anthropic", "claude-3-sonnet")
                .description("Research specialist for information gathering")
                .capabilities(["research", "web_search"])
                .system(
                    "You are a research specialist. Find accurate, up-to-date \
                information from reliable sources.",
                )
                .tools(["web_fetch", "read_tool"])
        })
        .delegation(true)
        .delegation_settings(querymt_agent::config::DelegationSettings {
            verification: true,
            ..Default::default()
        })
        .build()
        .await?;

    println!("Agent with delegates ready!");
    println!("- Root agent: OpenAI GPT-4");
    println!("- Coder: Ollama Qwen 2.5 Coder");
    println!("- Researcher: Anthropic Claude 3 Sonnet\n");

    // Register callbacks to see what's happening
    runner.on_tool_call(|name, _args| {
        println!("[TOOL] {}", name);
    });

    runner.on_delegation(|agent, objective| {
        println!("[DELEGATE] {} -> {}", agent, objective);
    });

    // Example interaction
    let response = runner
        .chat("Add a new feature to parse YAML files and explain what Rust async/await does.")
        .await?;

    println!("\n=== Response ===\n{}", response);

    Ok(())
}
