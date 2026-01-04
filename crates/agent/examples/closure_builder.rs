/// Example demonstrating the closure-based multi-agent builder API
///
/// This shows how to configure a multi-agent quorum using closures for
/// a clean, self-documenting configuration style.
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build a multi-agent quorum with closure-based configuration
    let runner = Agent::multi()
        .cwd(".")
        .db("./quorum.db")
        // Configure the planner agent
        .planner(|p| {
            p.provider("openai", "gpt-4")
                .system(
                    "You are a principal engineer coordinating specialist agents. \
                Delegate implementation to the coder and research to the researcher.",
                )
                .tools(["delegate", "create_task", "read_file", "search_text"])
        })
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
                    "read_file",
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
                .tools(["web_fetch", "read_file"])
        })
        // Enable delegation and verification
        .with_defaults()
        .build()
        .await?;

    println!("Multi-agent quorum ready!");
    println!("- Planner: OpenAI GPT-4");
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
