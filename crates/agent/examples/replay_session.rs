//! Session Replay Tool
//!
//! This tool allows you to replay any session's LLM calls from a session database.
//! It reconstructs the exact provider configuration, message history, and tool definitions
//! that were used during the original session, then optionally re-executes the LLM call.
//!
//! ## Usage
//!
//! ```bash
//! # Dry-run mode: Display session state without executing
//! cargo run --example replay_session -- /tmp/test.db <session-id>
//!
//! # Execute mode: Re-run the LLM call with verbose logging
//! RUST_LOG=debug cargo run --example replay_session -- /tmp/test.db <session-id> --execute
//!
//! # Replay a specific step (by step number, 1-indexed)
//! cargo run --example replay_session -- /tmp/test.db <session-id> --step 3
//!
//! # Replay a specific step and execute
//! RUST_LOG=debug cargo run --example replay_session -- /tmp/test.db <session-id> --step 3 --execute
//! ```
//!
//! ## What it does
//!
//! 1. Opens the session database
//! 2. Loads the LLM configuration (provider, model, system prompt, params)
//! 3. Loads the message history (up to the specified step or all messages)
//! 4. Loads tool definitions from the session events
//! 5. In dry-run mode: Displays all reconstructed state
//! 6. In execute mode: Rebuilds the provider and calls chat_with_tools()
//!
//! ## Debugging delegations
//!
//! This is especially useful for debugging delegation failures. You can:
//! - See exactly what system prompt and context was sent to the delegate
//! - Verify the tool definitions were correctly passed
//! - Re-run the exact same call with debug logging to see raw model output
//! - Compare the delegate's inputs with what a working REPL session would use

use querymt::chat::ChatMessage;
use querymt::plugin::extism_impl::host::ExtismLoader;
use querymt::plugin::host::PluginRegistry;
use querymt::plugin::host::native::NativeLoader;
use querymt_agent::events::AgentEvent;
use querymt_agent::model::MessagePart;
use querymt_agent::session::projection::EventJournal;
#[cfg(feature = "remote")]
use querymt_agent::session::provider::ProviderRouting;
use querymt_agent::session::provider::build_provider_from_config;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use querymt_agent::session::store::SessionStore;
use std::path::PathBuf;

#[derive(Debug)]
struct ReplayArgs {
    db_path: PathBuf,
    session_id: String,
    execute: bool,
    step: Option<usize>,
}

fn parse_args() -> Result<ReplayArgs, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        print_usage();
        std::process::exit(0);
    }

    if args.len() < 2 {
        return Err("Missing required arguments: <db-path> <session-id>".to_string());
    }

    let db_path = PathBuf::from(&args[0]);
    let session_id = args[1].clone();
    let mut execute = false;
    let mut step = None;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--execute" | "-e" => execute = true,
            "--step" | "-s" => {
                i += 1;
                if i >= args.len() {
                    return Err("--step requires a value".to_string());
                }
                step = Some(
                    args[i]
                        .parse()
                        .map_err(|_| format!("Invalid step number: {}", args[i]))?,
                );
            }
            arg => return Err(format!("Unknown argument: {}", arg)),
        }
        i += 1;
    }

    Ok(ReplayArgs {
        db_path,
        session_id,
        execute,
        step,
    })
}

fn print_usage() {
    eprintln!(
        r#"Session Replay Tool

USAGE:
    replay_session <db-path> <session-id> [OPTIONS]

ARGS:
    <db-path>       Path to the session database (e.g., /tmp/test.db)
    <session-id>    Session ID to replay (UUID format)

OPTIONS:
    --execute, -e   Execute the LLM call (default: dry-run only)
    --step N, -s N  Replay only up to step N (1-indexed, default: all steps)
    --help, -h      Print this help message

EXAMPLES:
    # Display session state (dry-run)
    replay_session /tmp/test.db 019c32ba-fb40-7c82-ba5c-ccfac0c312ac

    # Re-execute the LLM call with debug logging
    RUST_LOG=debug replay_session /tmp/test.db <session-id> --execute

    # Replay the first 3 steps only
    replay_session /tmp/test.db <session-id> --step 3

DEBUGGING DELEGATIONS:
    To debug a delegation, use the delegate's session ID (found in the parent
    session's delegation_completed event). This will show you exactly what
    prompt, tools, and context the delegate received.
"#
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = parse_args()?;

    println!("🔍 Loading session from: {}", args.db_path.display());
    println!("📋 Session ID: {}", args.session_id);
    if let Some(step) = args.step {
        println!("🎯 Replaying up to step: {}", step);
    }
    println!();

    // Open database read-only (skip migrations to preserve data)
    let store = SqliteStorage::connect_with_options(args.db_path.clone(), false).await?;

    // Load session
    let session = store
        .get_session(&args.session_id)
        .await?
        .ok_or_else(|| format!("Session not found: {}", args.session_id))?;

    println!("✅ Session found: {}", session.public_id);
    if let Some(name) = &session.name {
        println!("   Name: {}", name);
    }
    if let Some(created_at) = session.created_at {
        println!("   Created: {}", created_at);
    }
    println!();

    // Load LLM config
    let llm_config = store
        .get_session_llm_config(&args.session_id)
        .await?
        .ok_or_else(|| format!("No LLM config found for session {}", args.session_id))?;

    println!("🤖 LLM Configuration:");
    println!("   Provider: {}", llm_config.provider);
    println!("   Model: {}", llm_config.model);

    // Parse params to show system prompt and other settings
    let params: serde_json::Value = llm_config
        .params
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));

    if let Some(system) = params.get("system").and_then(|s| s.as_array()) {
        println!("   System prompt parts: {}", system.len());
        for (i, part) in system.iter().enumerate() {
            if let Some(s) = part.as_str() {
                println!("      Part {}: {} chars", i + 1, s.len());
                // Show first 100 chars
                let preview = if s.len() > 100 {
                    format!("{}...", &s[..100])
                } else {
                    s.to_string()
                };
                println!("         Preview: {}", preview.replace('\n', " "));
            }
        }
    }

    // Show other params
    if let Some(obj) = params.as_object() {
        for (key, value) in obj {
            if key != "system" {
                println!("   {}: {}", key, value);
            }
        }
    }
    println!();

    // Load message history
    let history = store.get_history(&args.session_id).await?;

    // Filter messages based on step if specified
    let messages_to_replay = if let Some(step_limit) = args.step {
        // Count steps (LLM requests)
        let events = load_events(&store, &args.session_id).await?;
        let step_message_ids = find_messages_up_to_step(&events, step_limit);

        history
            .into_iter()
            .filter(|m| step_message_ids.contains(&m.id))
            .collect::<Vec<_>>()
    } else {
        history
    };

    // Convert to ChatMessages (same logic as SessionHandle::history())
    let chat_messages: Vec<ChatMessage> = messages_to_replay
        .iter()
        .filter(|m| {
            // Filter out snapshot-only messages
            m.parts.iter().any(|p| {
                !matches!(
                    p,
                    MessagePart::TurnSnapshotStart { .. } | MessagePart::TurnSnapshotPatch { .. }
                )
            })
        })
        .map(|m| m.to_chat_message())
        .collect();

    println!("💬 Message History: {} messages", chat_messages.len());
    for (i, msg) in chat_messages.iter().enumerate() {
        println!(
            "   [{}] {:?}: {} chars{}",
            i + 1,
            msg.role,
            msg.content.len(),
            if !msg.content.is_empty() {
                format!(
                    " - Preview: {}",
                    msg.content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect::<String>()
                )
            } else {
                String::new()
            }
        );
    }
    println!();

    // Load tool definitions from events
    let tools = load_tools_from_events(&store, &args.session_id).await?;

    println!("🔧 Tools Available: {}", tools.len());
    for tool in &tools {
        let tool_json = serde_json::to_value(tool)?;
        if let Some(func) = tool_json.get("function").and_then(|f| f.as_object())
            && let Some(name) = func.get("name").and_then(|n| n.as_str())
        {
            println!("   - {}", name);
        }
    }
    println!();

    // Execute mode
    if args.execute {
        println!("▶️  Executing LLM call...");
        println!();

        // Initialize plugin registry
        let cfg_path = querymt_utils::providers::get_providers_config(None).await?;
        let mut registry = PluginRegistry::from_path(cfg_path)?;
        registry.register_loader(Box::new(ExtismLoader));
        registry.register_loader(Box::new(NativeLoader));

        // Build provider from config
        let provider = build_provider_from_config(
            &registry,
            &llm_config.provider,
            &llm_config.model,
            Some(&params),
            None, // No API key override
            #[cfg(feature = "remote")]
            ProviderRouting {
                provider_node_id: None, // local
                mesh_handle: None,      // not available in example
                allow_mesh_fallback: false,
            },
        )
        .await?;

        // Tools are already in querymt::chat::Tool format from the events

        // Call chat_with_tools
        match provider.chat_with_tools(&chat_messages, Some(&tools)).await {
            Ok(response) => {
                println!("✅ LLM Response:");
                if let Some(text) = response.text() {
                    println!("   Content: {}", text);
                } else {
                    println!("   Content: (empty)");
                }

                if let Some(tool_calls) = response.tool_calls() {
                    println!("   Tool calls: {}", tool_calls.len());
                    for tc in tool_calls {
                        println!("      - {}", tc.function.name);
                    }
                } else {
                    println!("   Tool calls: (none)");
                }

                if let Some(usage) = response.usage() {
                    println!(
                        "   Tokens: {} input, {} output",
                        usage.input_tokens, usage.output_tokens
                    );
                }

                if let Some(thinking) = response.thinking() {
                    println!("   Thinking: {} chars", thinking.len());
                }
            }
            Err(e) => {
                eprintln!("❌ LLM call failed: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        println!("ℹ️  Dry-run mode. Use --execute to re-run the LLM call.");
    }

    Ok(())
}

/// Load all events for a session
async fn load_events(
    store: &SqliteStorage,
    session_id: &str,
) -> Result<Vec<AgentEvent>, Box<dyn std::error::Error>> {
    let events = store
        .load_session_stream(session_id, None, None)
        .await?
        .into_iter()
        .map(AgentEvent::from)
        .collect();
    Ok(events)
}

/// Find all message IDs up to a given step number
fn find_messages_up_to_step(events: &[AgentEvent], step_limit: usize) -> Vec<String> {
    use querymt_agent::events::AgentEventKind;

    let mut message_ids = Vec::new();
    let mut step_count = 0;

    for event in events {
        match &event.kind {
            AgentEventKind::LlmRequestStart { .. } => {
                step_count += 1;
                if step_count > step_limit {
                    break;
                }
            }
            AgentEventKind::UserMessageStored { .. } => {
                // UserMessageStored doesn't have message_id in the event
                // We'll rely on message order in the DB
            }
            AgentEventKind::AssistantMessageStored { message_id, .. } => {
                if step_count <= step_limit
                    && let Some(id) = message_id
                {
                    message_ids.push(id.clone());
                }
            }
            _ => {}
        }
    }

    message_ids
}

/// Load tool definitions from the tools_available event
async fn load_tools_from_events(
    store: &SqliteStorage,
    session_id: &str,
) -> Result<Vec<querymt::chat::Tool>, Box<dyn std::error::Error>> {
    use querymt_agent::events::AgentEventKind;

    let events: Vec<AgentEvent> = store
        .load_session_stream(session_id, None, None)
        .await?
        .into_iter()
        .map(AgentEvent::from)
        .collect();

    // Find the last tools_available event
    for event in events.iter().rev() {
        if let AgentEventKind::ToolsAvailable { tools, .. } = &event.kind {
            return Ok(tools.clone());
        }
    }

    Ok(Vec::new())
}
