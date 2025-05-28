use async_recursion::async_recursion;
use clap::Parser;
use colored::*;
use querymt::{
    builder::LLMBuilder,
    chat::{ChatMessage, ChatResponse, ImageMime},
    error::LLMError,
    mcp::{adapter::McpToolAdapter, config::Config as McpConfig},
    plugin::{extism_impl::ExtismProviderRegistry, HTTPLLMProviderFactory, ProviderRegistry},
    FunctionCall, LLMProvider, ToolCall,
};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use serde_json::Value;
use spinners::{Spinner, Spinners};
use std::io::{self, IsTerminal, Read, Write};
use std::{fs, path::PathBuf};
use tokio;

mod secret_store;
use secret_store::SecretStore;

/// Command line arguments for the LLM CLI
#[derive(Parser)]
#[clap(
    name = "qmt",
    about = "Interactive CLI interface for chatting with LLM providers",
    allow_hyphen_values = true
)]
struct CliArgs {
    /// Command to execute (chat, set, get, delete, default)
    #[arg(index = 1)]
    command: Option<String>,

    /// Provider string in format "provider:model" or secret key for set/get/delete commands
    #[arg(index = 2)]
    provider_or_key: Option<String>,

    /// Initial prompt or secret value for set command
    #[arg(index = 3)]
    prompt_or_value: Option<String>,

    /// LLM provider name
    #[arg(long)]
    backend: Option<String>,

    /// Model name to use
    #[arg(long)]
    model: Option<String>,

    /// System prompt to set context
    #[arg(long)]
    system: Option<String>,

    /// API key for the provider
    #[arg(long)]
    api_key: Option<String>,

    /// Base URL for the API
    #[arg(long)]
    base_url: Option<String>,

    /// Temperature setting (0.0-1.0)
    #[arg(long)]
    temperature: Option<f32>,

    /// Maximum tokens in the response
    #[arg(long)]
    max_tokens: Option<u32>,

    #[arg(long)]
    mcp_config: Option<String>,

    #[arg(long)]
    provider_config: Option<String>,
}

/// Detects the MIME type of an image from its binary data
///
/// # Arguments
///
/// * `data` - The binary data of the image
///
/// # Returns
///
/// * `Some(ImageMime)` - The detected MIME type if recognized
/// * `None` - If the image format is not recognized
fn detect_image_mime(data: &[u8]) -> Option<ImageMime> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageMime::JPEG)
    } else if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        Some(ImageMime::PNG)
    } else if data.starts_with(&[0x47, 0x49, 0x46]) {
        Some(ImageMime::GIF)
    } else {
        None
    }
}

/// Retrieves provider and model information from various sources
///
/// # Arguments
///
/// * `args` - Command line arguments
///
/// # Returns
///
/// * `Some((provider_name, model_name))` - Provider name and optional model name
/// * `None` - If no provider information could be found
fn get_provider_info(args: &CliArgs) -> Option<(String, Option<String>)> {
    fn split_provider(s: &str) -> (String, Option<String>) {
        match s.split_once(':') {
            Some((p, k)) => (p.to_string(), Some(k.to_string())),
            None => (s.to_string(), None),
        }
    }

    if let Some(default_provider) = SecretStore::new()
        .ok()
        .and_then(|store| store.get_default_provider().cloned())
    {
        println!("Default provider: {}", default_provider);
        return Some(split_provider(&default_provider));
    }

    if let Some(provider_string) = args.provider_or_key.clone() {
        return Some(split_provider(&provider_string));
    }

    args.backend
        .clone()
        .map(|provider| (provider, args.model.clone()))
}

/// Retrieves the appropriate API key for the specified backend
///
/// # Arguments
///
/// * `backend` - The LLM backend to get the API key for
/// * `args` - Command line arguments that may contain an API key
///
/// # Returns
///
/// * `Some(String)` - The API key if found
/// * `None` - If no API key could be found
///
fn get_api_key(
    provider: &String,
    args: &CliArgs,
    registry: &Box<dyn ProviderRegistry>,
) -> Option<String> {
    args.api_key.clone().or_else(|| {
        if let Some(provider_factory) = registry.get(provider) {
            get_provider_api_key(provider_factory.as_http()?)
        } else {
            None
        }
    })
}

fn get_provider_api_key<P: HTTPLLMProviderFactory + ?Sized>(provider: &P) -> Option<String> {
    let store = SecretStore::new().ok()?;
    let api_key_name = provider.api_key_name()?;
    store
        .get(&api_key_name)
        .cloned()
        .or_else(|| std::env::var(api_key_name).ok())
}

/// Processes input data and creates appropriate chat messages
///
/// # Arguments
///
/// * `input` - Binary input data that might contain an image
/// * `prompt` - Text prompt to include in the message
///
/// # Returns
///
/// * `Vec<ChatMessage>` - Vector of chat messages ready to be sent to the LLM
fn process_input(input: &[u8], prompt: String) -> Vec<ChatMessage> {
    let mut messages = Vec::new();

    if !input.is_empty() && detect_image_mime(input).is_some() {
        let mime = detect_image_mime(input).unwrap();
        messages.push(ChatMessage::user().content(prompt).build());
        messages.push(ChatMessage::user().image(mime, input.to_vec()).build());
    } else if !input.is_empty() {
        let input_str = String::from_utf8_lossy(input);
        messages.push(
            ChatMessage::user()
                .content(format!("{}\n\n{}", prompt, input_str))
                .build(),
        );
    } else {
        messages.push(ChatMessage::user().content(prompt).build());
    }
    messages
}

#[async_recursion]
async fn handle_response(
    messages: &mut Vec<ChatMessage>,
    response: Box<dyn ChatResponse>,
    provider: &Box<dyn LLMProvider>,
) -> Result<(), LLMError> {
    print!("\r\x1B[K");
    if let Some(tool_calls) = response.tool_calls() {
        // Process each tool call
        let mut tool_results = Vec::new();

        for call in &tool_calls {
            println!("Tool call: {}", call.function.name);
            println!("Arguments: {}", call.function.arguments);

            let args: Value = serde_json::from_str(&call.function.arguments)
                .map_err(|e| LLMError::InvalidRequest(format!("bad args JSON: {}", e)))?;

            messages.push(
                ChatMessage::assistant()
                    .tool_use(tool_calls.clone())
                    .content(response.text().unwrap_or_default())
                    .build(),
            );
            let tool_res = match provider.call_tool(&call.function.name, args).await {
                Ok(result) => {
                    println!("Tool response: {}", serde_json::to_string_pretty(&result)?);
                    serde_json::to_string(&result)?
                }
                Err(e) => {
                    println!("Error while calling tool: {}", e.to_string());
                    e.to_string()
                }
            };
            tool_results.push(ToolCall {
                id: call.id.clone(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: call.function.name.clone(),
                    arguments: tool_res,
                },
            });
        }
        messages.push(ChatMessage::user().tool_result(tool_results).build());
        let mut sp = Spinner::new(Spinners::Dots12, "Thinking...".bright_magenta().to_string());
        match provider.chat(&messages).await {
            Ok(resp) => {
                sp.stop();
                handle_response(messages, resp, provider).await?;
            }
            _ => {
                sp.stop();
                println!("{}", "> Assistant: (no response)".bright_red());
            }
        }
    } else if let Some(text) = response.text() {
        println!("{} {}", "> Assistant:".bright_green(), text);
        let assistant_message = ChatMessage::assistant().content(text).build();
        messages.push(assistant_message);
    } else {
        println!("{}", "> Assistant: (no response)".bright_red());
    }
    println!("{}", "─".repeat(50).bright_black());
    Ok(())
}

async fn get_provider_registry(args: &CliArgs) -> Result<Box<dyn ProviderRegistry>, LLMError> {
    let registry = match args.provider_config.clone() {
        Some(cfg) => ExtismProviderRegistry::new(cfg).await,
        None => {
            let home_dir = dirs::home_dir().expect("Could not find home directory");
            let config_dir = home_dir.join(".qmt");

            if !config_dir.exists() {
                fs::create_dir_all(config_dir.clone())
                    .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
                return Err(LLMError::InvalidRequest(
                    "Config file for providers is missing. Please provide one!".to_string(),
                ));
            }

            let mut config_file: Option<PathBuf> = None;
            for &name in &["providers.json", "providers.toml", "providers.yaml"] {
                let cfg_file = config_dir.join(name);
                if cfg_file.is_file() {
                    config_file = Some(cfg_file);
                    break;
                }
            }

            ExtismProviderRegistry::new(
                config_file.expect("Cannot find config file for providers, please provide one!"),
            )
            .await
        }
    };

    Ok(Box::new(
        registry.map_err(|e| LLMError::PluginError(e.to_string()))?,
    ))
}

/// Main entry point for the LLM CLI application
///
/// Handles command parsing, provider configuration, and interactive chat functionality.
/// Supports various commands for managing secrets and default providers.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = CliArgs::parse();

    let registry = get_provider_registry(&args).await?;

    if let Some(cmd) = args.command.as_deref() {
        match cmd {
            "set" => {
                if let (Some(key), Some(value)) = (
                    args.provider_or_key.as_deref(),
                    args.prompt_or_value.as_deref(),
                ) {
                    let mut store = SecretStore::new()?;
                    store.set(key, value)?;
                    println!("{} Secret '{}' has been set.", "✓".bright_green(), key);
                    return Ok(());
                }
                eprintln!("{} Usage: llm set <key> <value>", "Error:".bright_red());
                return Ok(());
            }
            "get" => {
                if let Some(key) = args.provider_or_key.as_deref() {
                    let store = SecretStore::new()?;
                    match store.get(key) {
                        Some(value) => println!("{}: {}", key, value),
                        None => println!("{} Secret '{}' not found", "!".bright_yellow(), key),
                    }
                    return Ok(());
                }
                eprintln!("{} Usage: llm get <key>", "Error:".bright_red());
                return Ok(());
            }
            "delete" => {
                if let Some(key) = args.provider_or_key.as_deref() {
                    let mut store = SecretStore::new()?;
                    store.delete(key)?;
                    println!("{} Secret '{}' has been deleted.", "✓".bright_green(), key);
                    return Ok(());
                }
                eprintln!("{} Usage: llm delete <key>", "Error:".bright_red());
                return Ok(());
            }
            "chat" => {}
            "providers" => {
                for factory in registry.list() {
                    println!("- {}", factory.name());
                }
                return Ok(());
            }
            "models" => {
                let mut cfg: Value = Default::default();
                for factory in registry.list() {
                    print!("{}: ", factory.name());
                    let models = match factory.as_http() {
                        Some(http_factory) => {
                            match get_provider_api_key(http_factory) {
                                Some(api_key) => cfg = serde_json::json!({"api_key": api_key}),
                                _ => (),
                            }
                            factory.list_models(&cfg).await
                        }
                        None => factory.list_models(&cfg).await,
                    };

                    match models {
                        Ok(models) if !models.is_empty() => {
                            println!();
                            for model in models {
                                println!("  - {}", model);
                            }
                        }
                        Ok(_) => {
                            println!("(no models returned)");
                        }
                        Err(e) => {
                            println!("error listing models: {}", e);
                        }
                    }
                }
                return Ok(());
            }
            "default" => {
                if let Some(provider) = args.provider_or_key.as_deref() {
                    let mut store = SecretStore::new()?;
                    store.set_default_provider(provider)?;
                    return Ok(());
                } else if args.prompt_or_value.is_none() {
                    let store = SecretStore::new()?;
                    match store.get_default_provider() {
                        Some(provider) => println!("Default provider: {}", provider),
                        None => println!("{} No default provider set", "!".bright_yellow()),
                    }
                    return Ok(());
                }
                eprintln!(
                    "{} Usage: llm default <provider:model>",
                    "Error:".bright_red()
                );
                return Ok(());
            }
            _ => {}
        }
    }

    let (provider_name, model_name) = get_provider_info(&args)
        .ok_or_else(|| "No provider specified. Use --provider, provider:model argument, or set a default provider with 'llm default <provider:model>'")?;

    let mut builder = LLMBuilder::new().provider(provider_name.clone());
    if let Some(model) = model_name.or(args.model.clone()) {
        builder = builder.model(model);
    }

    if let Some(system) = args.system.clone() {
        builder = builder.system(system);
    }

    if let Some(key) = get_api_key(&provider_name, &args, &registry) {
        builder = builder.api_key(key);
    }

    if let Some(url) = args.base_url.clone() {
        builder = builder.base_url(url);
    }

    if let Some(temp) = args.temperature {
        builder = builder.temperature(temp);
    }

    if let Some(mt) = args.max_tokens {
        builder = builder.max_tokens(mt);
    }

    if let Some(mcp_cfg) = args.mcp_config {
        let c = McpConfig::load(mcp_cfg).await?;
        let mcp_clients = c.create_mcp_clients().await?;
        for (_, client) in mcp_clients {
            let server = client.peer().clone();
            let tools = server.list_all_tools().await?;
            for tool in tools
                .into_iter()
                .map(|tool| McpToolAdapter::try_new(tool, server.clone()))
            {
                match tool {
                    Ok(adapter) => {
                        builder = builder.add_tool(adapter);
                    }
                    Err(err) => {
                        println!("Skipping tool because adapter creation failed: {}", err);
                    }
                }
            }
        }
    }

    let provider = builder
        .build(registry)
        .map_err(|e| format!("Failed to build provider: {}", e))?;

    let is_pipe = !io::stdin().is_terminal();

    if is_pipe || args.prompt_or_value.is_some() {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;

        let prompt = if let Some(p) = args.prompt_or_value {
            p
        } else {
            String::from_utf8_lossy(&input).to_string()
        };

        let messages = process_input(&input, prompt);

        match provider.chat_with_tools(&messages, provider.tools()).await {
            Ok(response) => {
                if let Some(text) = response.text() {
                    println!("{}", text);
                }
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
        return Ok(());
    }

    println!("{}", "llm - Interactive Chat".bright_cyan());
    println!("Provider: {}", provider_name.bright_green());
    println!("{}", "Type 'exit' to quit".bright_black());
    println!("{}", "─".repeat(50).bright_black());

    let mut rl = DefaultEditor::new()?;
    let mut messages: Vec<ChatMessage> = Vec::new();

    loop {
        io::stdout().flush()?;
        let readline = rl.readline("> ");
        match readline {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.to_lowercase() == "exit" {
                    println!("{}", "👋 Goodbye!".bright_cyan());
                    break;
                }
                let _ = rl.add_history_entry(trimmed);

                let user_message = ChatMessage::user().content(trimmed.to_string()).build();
                messages.push(user_message);

                let mut sp =
                    Spinner::new(Spinners::Dots12, "Thinking...".bright_magenta().to_string());

                match provider.chat(&messages).await {
                    Ok(response) => {
                        sp.stop();
                        handle_response(&mut messages, response, &provider).await?;
                    }
                    Err(e) => {
                        sp.stop();
                        eprintln!("{} {}", "Error:".bright_red(), e);
                        println!("{}", "─".repeat(50).bright_black());
                    }
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                println!("\n{}", "👋 Goodbye!".bright_cyan());
                break;
            }
            Err(err) => {
                eprintln!("{} {:?}", "Error:".bright_red(), err);
                break;
            }
        }
    }

    Ok(())
}
