//! Changing an agent's team between turns.
//!
//! One agent, one session, three turns. Between the turns the set of
//! subagents the root can delegate to is changed while the agent runs:
//!
//! 1. Turn 1: a plain agent with no subagents does the work itself.
//! 2. A `reviewer` and a `doc_writer` are added; turn 2 can delegate to them.
//! 3. `reviewer` is removed; turn 3 can no longer delegate to it.
//!
//! The root's `delegate` tool lists the current subagents on every turn, so
//! no restart, rebuild or new session is needed. Changes last until the
//! agent stops; they are not written to any configuration.
//!
//! ```text
//! ANTHROPIC_API_KEY=... cargo run -p querymt-agent --example dynamic_subagents
//! # other provider:
//! QMT_PROVIDER=openai QMT_MODEL=gpt-4o QMT_SUBAGENT_MODEL=gpt-4o-mini \
//!     cargo run -p querymt-agent --example dynamic_subagents
//! ```
use querymt_agent::prelude::*;

fn show_team(agent: &Agent) {
    let team = agent.subagents();
    if team.is_empty() {
        println!(
            "   team (generation {}): <empty>",
            agent.roster_generation()
        );
        return;
    }
    println!("   team (generation {}):", agent.roster_generation());
    for s in team {
        println!("     - {:<11} {}", s.id, s.description);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = std::env::var("QMT_PROVIDER").unwrap_or_else(|_| "anthropic".into());
    let model = std::env::var("QMT_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".into());
    let sub_model =
        std::env::var("QMT_SUBAGENT_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".into());

    // A plain agent. `delegate` must be among its tools, otherwise subagents
    // cannot be added to it later.
    let agent = Agent::builder()
        .cwd(".")
        .provider(&provider, &model)
        .system(
            "You coordinate a small team. Delegate review work and documentation \
             work to a fitting subagent when one exists; otherwise do it yourself. \
             Keep answers short.",
        )
        .tools(["delegate", "read_tool", "glob", "search_text"])
        .build()
        .await?;

    agent.on_delegation(|to, objective| {
        println!("   [delegate -> {to}] {objective}");
    });
    agent.on_subagents_changed(|c| {
        println!(
            "   [team] {} {} ({}, generation {})",
            c.kind.label(),
            c.id,
            c.origin,
            c.generation.get()
        );
    });
    agent.on_tool_call(|name, _| {
        if name != "delegate" {
            println!("   [tool] {name}");
        }
    });

    println!("== Turn 1: no subagents ==");
    show_team(&agent);
    println!(
        "{}\n",
        agent
            .chat("What does Cargo.toml in this directory define?")
            .await?
    );

    println!("== Adding subagents ==");
    agent
        .add_subagent("reviewer", |s| {
            s.provider(&provider, &sub_model)
                .description("Reviews code for bugs and unclear design")
                .capabilities(["review"])
                .system("You are a strict, concise code reviewer.")
                .tools(["read_tool", "glob", "search_text"])
        })
        .await?;
    agent
        .add_subagent("doc_writer", |s| {
            s.provider(&provider, &sub_model)
                .description("Writes short user-facing documentation")
                .capabilities(["docs"])
                .system("You write clear, brief documentation.")
                .tools(["read_tool"])
        })
        .await?;
    show_team(&agent);

    println!("\n== Turn 2: the root can delegate ==");
    println!(
        "{}\n",
        agent
            .chat(
                "Get crates/agent/src/prelude.rs reviewed and ask for a two-line doc summary of it."
            )
            .await?
    );

    println!("== Removing the reviewer ==");
    agent.remove_subagent("reviewer").await?;
    show_team(&agent);

    println!("\n== Turn 3: no reviewer any more ==");
    println!(
        "{}\n",
        agent
            .chat("Review crates/agent/src/prelude.rs again.")
            .await?
    );

    // Errors are typed and leave the team unchanged.
    match agent.remove_subagent("reviewer").await {
        Err(RosterError::UnknownAgent(id)) => println!("(as expected, `{id}` is gone)"),
        other => println!("unexpected: {other:?}"),
    }

    agent.shutdown().await;
    Ok(())
}
