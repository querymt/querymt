use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Agent::single().provider("openai", "gpt-4").build().await?;

    let server = agent.dashboard();
    let addr = "127.0.0.1:3030";

    println!("Dashboard running at http://{}", addr);
    server.run(addr).await?;
    Ok(())
}
