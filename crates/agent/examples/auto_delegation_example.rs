use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let quorum = Agent::multi()
        .cwd(".")
        .planner(|p| {
            p.provider("openai", "gpt-4")
                .system("Delegate research tasks to specialists.")
                .tools(["delegate", "read_tool"])
        })
        .delegate("researcher", |d| {
            d.provider("openai", "gpt-4o-mini")
                .description("Research specialist")
                .capabilities(["research", "web_search"])
                .tools(["web_fetch", "read_tool"])
        })
        .with_delegation(true)
        .build()
        .await?;

    let result = quorum
        .chat("Summarize recent Rust async improvements.")
        .await?;
    println!("Result: {}", result);
    Ok(())
}
