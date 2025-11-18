use crate::cli_args::{ToolConfig, ToolPolicyState};
use crate::commands::{
    builtin::*, completer::QmtCompleter, CommandLoader, CommandRegistry, CommandResult,
};
use crate::utils::{parse_tool_names, print_separator, process_input, prompt_tool_execution};
use colored::*;
use futures::future::join_all;
use querymt::{
    chat::{ChatMessage, ChatResponse},
    error::LLMError,
    FunctionCall, LLMProvider, ToolCall,
};
use reedline::{
    default_emacs_keybindings, ColumnarMenu, EditCommand, Emacs, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, Signal,
};
use spinners::{Spinner, Spinners};
use std::{
    borrow::Cow,
    io::{self, Read, Write},
    sync::Arc,
};

/// Custom prompt for querymt CLI
struct QmtPrompt {
    prompt_text: String,
}

impl QmtPrompt {
    fn new() -> Self {
        Self {
            prompt_text: ":: ".to_string(),
        }
    }
}

impl Prompt for QmtPrompt {
    fn render_prompt_left(&self) -> Cow<str> {
        Cow::Borrowed(&self.prompt_text)
    }

    fn render_prompt_right(&self) -> Cow<str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: reedline::PromptEditMode) -> Cow<str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<str> {
        Cow::Borrowed("... ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<str> {
        match history_search.status {
            reedline::PromptHistorySearchStatus::Passing => Cow::Borrowed("(search) "),
            reedline::PromptHistorySearchStatus::Failing => Cow::Borrowed("(failing search) "),
        }
    }
}

pub async fn handle_response(
    messages: &mut Vec<ChatMessage>,
    initial_response: Box<dyn ChatResponse>,
    provider: &Box<dyn LLMProvider>,
    tool_config: &ToolConfig,
) -> Result<(), LLMError> {
    let mut current_response = initial_response;

    loop {
        if let Some(usage) = current_response.usage() {
            log::info!(
                "Tokens usage (in/out): {}/{}",
                usage.input_tokens,
                usage.output_tokens
            );
        }
        // Clear the current line
        print!("\r\x1B[K");

        if let Some(tool_calls) = current_response.tool_calls() {
            messages.push(
                ChatMessage::assistant()
                    .tool_use(tool_calls.clone())
                    .content(current_response.text().unwrap_or_default())
                    .build(),
            );

            let tool_futures = tool_calls.into_iter().map(|call| async {
                // Security check for tool execution
                let tool_name = call.function.name.clone();

                // Get server name from the tool for security checking
                let server_name = match provider.tool_server_name(&tool_name) {
                    Some(s) => s,
                    None => return (call, Err(LLMError::ToolConfigError(format!("Server name can't be get from tool `{}`.", tool_name)))),
                };

                let (effective_server, effective_tool) = match parse_tool_names(server_name, &tool_name) {
                    Some((s, t)) => (s, t),
                    None => return (call, Err(LLMError::ToolConfigError(format!("Invalid tool format for server {}", server_name)))),
                };

                log::debug!("serve: {}, tool: {}", effective_server, effective_tool);

                let state = tool_config.tools.as_ref()
                    .and_then(|tools| tools.get(effective_server))
                    .and_then(|server_tools| server_tools.get(effective_tool))
                    .or_else(|| tool_config.default.as_ref())
                    .unwrap_or(&ToolPolicyState::Ask);

                match state {
                    ToolPolicyState::Ask => {
                        match prompt_tool_execution(&call) {
                            Ok((true, _)) => {
                                // User approved, continue with execution
                            }
                            Ok((false, reason)) => {
                                // User denied, provide informative response to LLM
                                let denial_message = match reason {
                                    Some(r) =>
                                        format!("Reason: {}.", r),

                                    None =>

                                    "".to_string()
                                };
                                let name = call.function.name.clone();
                                return (
                                    call,
                                    Ok(format!(
                                        "Tool execution denied by user. The user chose not to execute the '{}' tool. {}",
                                        name,denial_message
                                    ).trim().to_string()),
                                );
                            }
                            Err(e) => {
                                return (
                                    call,
                                    Err(LLMError::InvalidRequest(format!("Failed to read user input: {}", e))),
                                );
                            }
                        }
                    }
                    ToolPolicyState::Allow => {
                        // No prompt, proceed directly
                    }
                    ToolPolicyState::Deny => {
                        let denial_message = format!(
                            "Tool execution denied by configuration. The '{}' tool is not allowed.",
                            call.function.name
                        );
                        return (
                            call,
                            Ok(denial_message),
                        );
                    }
                }

                let args: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
                    Ok(args) => args,
                    Err(e) => {
                        return (
                            call,
                            Err(LLMError::InvalidRequest(format!("bad args JSON: {}", e))),
                        )
                    }
                };

                match provider.call_tool(&call.function.name, args).await {
                    Ok(result) => {
                        log::debug!(
                            "Tool response: {}",
                            serde_json::to_string_pretty(&result).unwrap_or_default()
                        );
                        {
                            let result_str = match serde_json::to_string(&result) {
                                Ok(s) => s,
                                Err(e) => return (call, Err(LLMError::from(e))),
                            };
                            (
                                call,
                                Ok(result_str),
                            )
                        }
                    }
                    Err(e) => {
                        log::error!("Error while calling tool: {}", e);
                        (call, Err(e))
                    }
                }
            });

            let tool_results_from_futures = join_all(tool_futures).await;

            let tool_results = tool_results_from_futures
                .into_iter()
                .map(|(call, result)| {
                    let tool_res_str = match result {
                        Ok(res_str) => res_str,
                        Err(e) => e.to_string(),
                    };
                    ToolCall {
                        id: call.id.clone(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: call.function.name.clone(),
                            arguments: tool_res_str,
                        },
                    }
                })
                .collect::<Vec<_>>();

            messages.push(ChatMessage::user().tool_result(tool_results).build());
            let mut sp = Spinner::new(Spinners::Dots12, "Thinking...".bright_magenta().to_string());
            match provider.chat(&messages).await {
                Ok(resp) => {
                    sp.stop();
                    current_response = resp;
                }
                Err(e) => {
                    sp.stop();
                    println!("{}", "> Assistant: (no response)".bright_red());
                    return Err(e);
                }
            }
        } else if let Some(text) = current_response.text() {
            println!("{} {}", "> Assistant:".bright_green(), text);
            messages.push(ChatMessage::assistant().content(text).build());
            break;
        } else {
            println!("{}", "> Assistant: (no response)".bright_red());
            break;
        }
    }
    print_separator();
    Ok(())
}

/// Handle piped input or single-shot chat
pub async fn chat_pipe(
    provider: &Box<dyn LLMProvider>,
    prompt: Option<&String>,
    tool_config: &ToolConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;

    let prompt = if let Some(p) = prompt {
        p.clone()
    } else {
        String::from_utf8_lossy(&input).to_string()
    };

    let mut messages = process_input(&input, prompt);
    match provider.chat_with_tools(&messages, provider.tools()).await {
        Ok(response) => {
            handle_response(&mut messages, response, provider, tool_config).await?;
        }
        Err(e) => eprintln!("Error: {}", e),
    }
    Ok(())
}

/// Interactive REPL loop
pub async fn interactive_loop(
    provider: &Box<dyn LLMProvider>,
    provider_name: &str,
    tool_config: &ToolConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", "qmt - Interactive Chat".bright_blue());
    println!("Provider: {}", provider_name.bright_green());
    println!("{}", "Type 'exit' or '/exit' to quit".bright_black());
    println!("{}", "Type '/help' for available commands".bright_black());
    print_separator();

    // Initialize command registry
    let mut registry = CommandRegistry::new();

    // Register built-in commands
    registry.register_builtin(Arc::new(HelpCommand));
    registry.register_builtin(Arc::new(McpCommand));
    registry.register_builtin(Arc::new(ClearCommand));
    registry.register_builtin(Arc::new(ExitCommand));

    // Load markdown commands
    match CommandLoader::new() {
        Ok(loader) => match loader.load_commands() {
            Ok(commands) => {
                for cmd in commands {
                    log::info!("Loaded custom command: /{}", cmd.name());
                    registry.register_markdown(cmd);
                }
            }
            Err(e) => {
                log::warn!("Failed to load custom commands: {}", e);
            }
        },
        Err(e) => {
            log::warn!("Failed to initialize command loader: {}", e);
        }
    }

    let registry_arc = Arc::new(registry);

    // Setup reedline with completion menu
    let completer = Box::new(QmtCompleter::new(registry_arc.clone()));

    let completion_menu = Box::new(
        ColumnarMenu::default()
            .with_name("completion_menu")
            .with_columns(1)
            .with_column_width(None)
            .with_column_padding(2)
    );

    let mut keybindings = default_emacs_keybindings();
    // Ctrl+Enter or Alt+Enter inserts a newline for multiline input
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    keybindings.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    // Bind Tab to activate completion menu
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::Menu("completion_menu".to_string()),
    );
    // Bind Shift+Tab to go backwards in menu
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );

    let edit_mode = Box::new(Emacs::new(keybindings));

    // Setup history
    let history = Box::new(
        FileBackedHistory::with_file(1000, dirs::cache_dir()
            .map(|mut p| {
                p.push("querymt");
                std::fs::create_dir_all(&p).ok();
                p.push("history.txt");
                p
            })
            .unwrap_or_else(|| std::path::PathBuf::from("history.txt")))
            .expect("Failed to create history file"),
    );

    let mut line_editor = Reedline::create()
        .with_completer(completer)
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_edit_mode(edit_mode)
        .with_history(history);

    let prompt = QmtPrompt::new();
    let mut messages: Vec<ChatMessage> = Vec::new();

    loop {
        io::stdout().flush()?;
        let sig = line_editor.read_line(&prompt);
        match sig {
            Ok(Signal::Success(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("exit") {
                    println!("{}", "👋 Goodbye!".bright_blue());
                    break;
                }

                // Check if this is a slash command
                if let Some((cmd_name, args)) = CommandRegistry::parse_command_line(trimmed) {
                    match handle_slash_command(
                        &registry_arc,
                        cmd_name,
                        args,
                        provider,
                        &mut messages,
                    )
                    .await
                    {
                        Ok(CommandResult::Exit) => {
                            println!("{}", "👋 Goodbye!".bright_blue());
                            break;
                        }
                        Ok(CommandResult::Success(output)) => {
                            if !output.is_empty() {
                                println!("{}", output);
                            }
                            print_separator();
                        }
                        Ok(CommandResult::Error(err)) => {
                            eprintln!("{} {}", "Error:".bright_red(), err);
                            print_separator();
                        }
                        Ok(CommandResult::Continue) => {
                            // Command wants to continue with normal chat flow
                            // This shouldn't happen in the current implementation
                        }
                        Err(e) => {
                            eprintln!("{} {}", "Command error:".bright_red(), e);
                            print_separator();
                        }
                    }
                    continue;
                }

                // Normal chat flow
                messages.push(ChatMessage::user().content(trimmed.to_string()).build());
                let mut sp =
                    Spinner::new(Spinners::Dots12, "Thinking...".bright_magenta().to_string());
                match provider.chat(&messages).await {
                    Ok(response) => {
                        sp.stop();
                        handle_response(&mut messages, response, provider, tool_config).await?;
                    }
                    Err(e) => {
                        sp.stop();
                        eprintln!("{} {}", "Error:".bright_red(), e);
                        print_separator();
                    }
                }
            }
            Ok(Signal::CtrlC) => {
                println!("\n{}", "👋 Goodbye!".bright_blue());
                break;
            }
            Ok(Signal::CtrlD) => {
                println!("\n{}", "👋 Goodbye!".bright_blue());
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

/// Handle slash command execution (built-in and markdown)
async fn handle_slash_command(
    registry: &Arc<CommandRegistry>,
    cmd_name: &str,
    args: Vec<String>,
    provider: &Box<dyn LLMProvider>,
    messages: &mut Vec<ChatMessage>,
) -> Result<CommandResult, Box<dyn std::error::Error>> {
    use crate::commands::CommandSource;

    match registry.get(cmd_name) {
        Some(CommandSource::BuiltIn(cmd)) => {
            // Check if command requires async execution
            if cmd.is_async() {
                Ok(cmd.execute_async(args).await?)
            } else {
                Ok(cmd.execute(args)?)
            }
        }
        Some(CommandSource::Markdown(md_cmd)) => {
            // Execute markdown command by sending to LLM
            let prompt = md_cmd.substitute_arguments(&args);

            // Add the prompt as a user message
            messages.push(ChatMessage::user().content(prompt.clone()).build());

            let mut sp =
                Spinner::new(Spinners::Dots12, "Executing command...".bright_magenta().to_string());

            match provider.chat(messages).await {
                Ok(response) => {
                    sp.stop();
                    // Display the response
                    if let Some(text) = response.text() {
                        messages.push(ChatMessage::assistant().content(text.clone()).build());
                        println!("{}", text);
                    }
                    Ok(CommandResult::Success(String::new()))
                }
                Err(e) => {
                    sp.stop();
                    Ok(CommandResult::Error(format!("{}", e)))
                }
            }
        }
        None => Ok(CommandResult::Error(format!(
            "Unknown command: /{}. Type '/help' for available commands.",
            cmd_name
        ))),
    }
}
