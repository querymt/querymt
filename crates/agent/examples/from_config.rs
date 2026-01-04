use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command line arguments
    let mut args = std::env::args().skip(1);
    let config_path = args.next().unwrap_or_else(|| {
        eprintln!("Usage: cargo run --example from_config <config_file>");
        eprintln!("Examples:");
        eprintln!("  cargo run --example from_config examples/confs/single_agent.toml");
        eprintln!("  cargo run --example from_config examples/confs/multi_agent.toml");
        std::process::exit(1);
    });

    println!("Loading agent from: {}", config_path);

    // Load agent/quorum from config file
    // This automatically detects whether it's a single agent or multi-agent config
    let runner = from_config(&config_path).await?;

    println!("Agent loaded successfully!\n");

    // Register event callbacks to see what's happening
    runner.on_tool_call(|name, args| {
        println!("[TOOL CALL] {} with args: {:?}", name, args);
    });

    runner.on_message(|role, content| {
        println!("[{}] {}", role.to_uppercase(), content);
    });

    runner.on_error(|message| {
        eprintln!("[ERROR] {}", message);
    });

    // Interactive chat loop
    println!("Enter your message (or 'quit' to exit):");
    loop {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "quit" || input == "exit" {
            println!("Goodbye!");
            break;
        }

        match runner.chat(input).await {
            Ok(response) => {
                println!("\nAgent: {}\n", response);
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }

    Ok(())
}
