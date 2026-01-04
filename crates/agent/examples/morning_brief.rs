use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = Agent::single()
        .provider("openai", "gpt-4")
        .api_key(std::env::var("OPENAI_API_KEY")?)
        .system("You are a concise market analyst.")
        .build()
        .await?;

    let response = agent
        .chat("Draft a morning brief for NVDA, AAPL, and MSFT.")
        .await?;
    println!("{}", response);
    Ok(())
}
