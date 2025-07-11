use crate::utils::{print_separator, process_input, visualize_tool_call};
use colored::*;
use futures::future::join_all;
use querymt::{
    chat::{ChatMessage, ChatResponse},
    error::LLMError,
    FunctionCall, LLMProvider, ToolCall,
};
use rustyline::error::ReadlineError;
use spinners::{Spinner, Spinners};
use std::io::{self, Read, Write};

pub async fn handle_response(
    messages: &mut Vec<ChatMessage>,
    initial_response: Box<dyn ChatResponse>,
    provider: &Box<dyn LLMProvider>,
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
                visualize_tool_call(&call, None);
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
                        visualize_tool_call(&call, Some(true));
                        (
                            call,
                            serde_json::to_string(&result).map_err(|e| LLMError::from(e)),
                        )
                    }
                    Err(e) => {
                        visualize_tool_call(&call, Some(false));
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
            handle_response(&mut messages, response, provider).await?;
        }
        Err(e) => eprintln!("Error: {}", e),
    }
    Ok(())
}

/// Interactive REPL loop
pub async fn interactive_loop(
    provider: &Box<dyn LLMProvider>,
    provider_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", "qmt - Interactive Chat".bright_blue());
    println!("Provider: {}", provider_name.bright_green());
    println!("{}", "Type 'exit' to quit".bright_black());
    print_separator();

    let mut rl = rustyline::DefaultEditor::new()?;
    let mut messages: Vec<ChatMessage> = Vec::new();

    loop {
        io::stdout().flush()?;
        let readline = rl.readline(&":: ".bold().red().to_string());
        match readline {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("exit") {
                    println!("{}", "👋 Goodbye!".bright_blue());
                    break;
                }
                let _ = rl.add_history_entry(trimmed);

                messages.push(ChatMessage::user().content(trimmed.to_string()).build());
                let mut sp =
                    Spinner::new(Spinners::Dots12, "Thinking...".bright_magenta().to_string());
                match provider.chat(&messages).await {
                    Ok(response) => {
                        sp.stop();
                        handle_response(&mut messages, response, provider).await?;
                    }
                    Err(e) => {
                        sp.stop();
                        eprintln!("{} {}", "Error:".bright_red(), e);
                        print_separator();
                    }
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
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
