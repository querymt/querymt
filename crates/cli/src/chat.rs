use crate::cli_args::{ToolConfig, ToolPolicyState};
use crate::utils::{parse_tool_names, print_separator, process_input, prompt_tool_execution};
use colored::*;
use futures::Stream;
use futures::StreamExt;
use futures::future::join_all;
use querymt::{
    FunctionCall, LLMProvider, ToolCall,
    chat::{ChatMessage, ChatResponse, StreamChunk},
    error::LLMError,
};
use rustyline::{
    Cmd, Config, Editor, EventHandler, KeyCode, KeyEvent, Modifiers,
    completion::FilenameCompleter,
    error::ReadlineError,
    highlight::{CmdKind, Highlighter, MatchingBracketHighlighter},
    hint::HistoryHinter,
    validate::MatchingBracketValidator,
};
use rustyline_derive::{Completer, Helper, Hinter, Validator};
use spinners::{Spinner, Spinners};
use std::collections::HashMap;
use std::pin::Pin;
use std::{
    borrow::Cow,
    io::{self, Read, Write},
};

#[derive(Helper, Completer, Hinter, Validator)]
struct QmtHelper {
    #[rustyline(Completer)]
    completer: FilenameCompleter,
    highlighter: MatchingBracketHighlighter,
    #[rustyline(Validator)]
    validator: MatchingBracketValidator,
    #[rustyline(Hinter)]
    hinter: HistoryHinter,
    colored_prompt: String,
}

impl Highlighter for QmtHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default {
            Cow::Borrowed(&self.colored_prompt)
        } else {
            Cow::Borrowed(prompt)
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned("\x1b[1m".to_owned() + hint + "\x1b[m")
    }

    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        self.highlighter.highlight(line, pos)
    }

    fn highlight_char(&self, line: &str, pos: usize, kind: CmdKind) -> bool {
        self.highlighter.highlight_char(line, pos, kind)
    }
}

pub(crate) enum StreamOrResponse {
    Stream(Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>),
    Response(Box<dyn ChatResponse>),
}

async fn unified_chat(
    messages: &[ChatMessage],
    provider: &Box<dyn LLMProvider>,
) -> Result<StreamOrResponse, LLMError> {
    if provider.supports_streaming() {
        match provider
            .chat_stream_with_tools(messages, provider.tools())
            .await
        {
            Ok(stream) => return Ok(StreamOrResponse::Stream(stream)),
            Err(LLMError::NotImplemented(_)) => {}
            Err(e) => return Err(e),
        }
    }
    let resp = provider.chat_with_tools(messages, provider.tools()).await?;
    Ok(StreamOrResponse::Response(resp))
}

pub async fn handle_any_response(
    messages: &mut Vec<ChatMessage>,
    initial: StreamOrResponse,
    provider: &Box<dyn LLMProvider>,
    tool_config: &ToolConfig,
    mut spinner: Option<Spinner>,
) -> Result<(), LLMError> {
    let mut current = initial;
    let mut header_printed = false;

    loop {
        let is_stream = matches!(current, StreamOrResponse::Stream(_));
        let (text, tool_calls, _usage) = match current {
            StreamOrResponse::Stream(mut stream) => {
                let mut full_text = String::new();
                let mut tool_calls_map: HashMap<usize, (String, String, String)> = HashMap::new();

                if let Some(mut sp) = spinner.take() {
                    sp.stop();
                    print!("\r\x1B[K");
                    if !header_printed {
                        print!("{}", "> Assistant: ".bright_green());
                        io::stdout().flush().ok();
                        header_printed = true;
                    }
                }

                loop {
                    tokio::select! {
                        chunk = stream.next() => {
                            let Some(chunk_res) = chunk else {
                                break;
                            };
                            match chunk_res? {
                                StreamChunk::Text(t) => {
                                    log::trace!("Received stream text chunk: {} bytes", t.len());
                                    print!("{}", t);
                                    io::stdout().flush().ok();
                                    full_text.push_str(&t);
                                }
                                StreamChunk::ToolUseStart { index, id, name } => {
                                    log::debug!("Received tool use start: {} (idx {})", name, index);
                                    tool_calls_map.insert(index, (id, name, String::new()));
                                }
                                StreamChunk::ToolUseInputDelta {
                                    index,
                                    partial_json,
                                } => {
                                    log::trace!(
                                        "Received tool use input delta: {} bytes (idx {})",
                                        partial_json.len(),
                                        index
                                    );
                                    if let Some(entry) = tool_calls_map.get_mut(&index) {
                                        entry.2.push_str(&partial_json);
                                    }
                                }
                                StreamChunk::ToolUseComplete { .. } => {
                                    log::debug!("Received tool use complete");
                                }
                                StreamChunk::Usage(usage) => {
                                    log::debug!(
                                        "Usage: input={}, output={}",
                                        usage.input_tokens,
                                        usage.output_tokens
                                    );
                                }
                                StreamChunk::Done { stop_reason } => {
                                    log::debug!("Stream done: stop_reason={}", stop_reason);
                                    println!();
                                    break;
                                }
                            }
                        }
                        _ = tokio::signal::ctrl_c() => {
                            println!();
                            println!("{}", "Interrupted.".bright_yellow());
                            print_separator();
                            return Ok(());
                        }
                    }
                }

                if let Some(mut sp) = spinner.take() {
                    sp.stop();
                    print!("\r\x1B[K");
                }

                let tool_calls = if tool_calls_map.is_empty() {
                    None
                } else {
                    let mut calls: Vec<_> = tool_calls_map.into_iter().collect();
                    calls.sort_by_key(|(idx, _)| *idx);
                    Some(
                        calls
                            .into_iter()
                            .map(|(_, (id, name, arguments))| ToolCall {
                                id,
                                call_type: "function".to_string(),
                                function: FunctionCall { name, arguments },
                            })
                            .collect(),
                    )
                };

                (Some(full_text), tool_calls, None)
            }
            StreamOrResponse::Response(resp) => {
                if let Some(mut sp) = spinner.take() {
                    sp.stop();
                    print!("\r\x1B[K");
                }

                if let Some(usage) = resp.usage() {
                    log::info!(
                        "Tokens usage (in/out): {}/{}",
                        usage.input_tokens,
                        usage.output_tokens
                    );
                }
                (resp.text(), resp.tool_calls(), resp.usage())
            }
        };

        if let Some(ref tcalls) = tool_calls {
            messages.push(
                ChatMessage::assistant()
                    .tool_use(tcalls.clone())
                    .content(text.clone().unwrap_or_default())
                    .build(),
            );

            let tool_futures = tcalls.clone().into_iter().map(|call| async {
                let tool_name = call.function.name.clone();
                let server_name = match provider.tool_server_name(&tool_name) {
                    Some(s) => s,
                    None => {
                        return (
                            call,
                            Err(LLMError::ToolConfigError(format!(
                                "Server name can't be get from tool `{}`.",
                                tool_name
                            ))),
                        )
                    }
                };

                let (effective_server, effective_tool) =
                    match parse_tool_names(server_name, &tool_name) {
                        Some((s, t)) => (s, t),
                        None => {
                            return (
                                call,
                                Err(LLMError::ToolConfigError(format!(
                                    "Invalid tool format for server {}",
                                    server_name
                                ))),
                            )
                        }
                    };

                let state = tool_config
                    .tools
                    .as_ref()
                    .and_then(|tools| tools.get(effective_server))
                    .and_then(|server_tools| server_tools.get(effective_tool))
                    .or(tool_config.default.as_ref())
                    .unwrap_or(&ToolPolicyState::Ask);

                match state {
                    ToolPolicyState::Ask => match prompt_tool_execution(&call) {
                        Ok((true, _)) => {}
                        Ok((false, reason)) => {
                            let denial_message = match reason {
                                Some(r) => format!("Reason: {}.", r),
                                None => "".to_string(),
                            };
                            return (
                                call,
                                Ok(format!(
                                    "Tool execution denied by user. The user chose not to execute the '{}' tool. {}",
                                    tool_name, denial_message
                                )
                                .trim()
                                .to_string()),
                            );
                        }
                        Err(e) => {
                            return (
                                call,
                                Err(LLMError::InvalidRequest(format!(
                                    "Failed to read user input: {}",
                                    e
                                ))),
                            );
                        }
                    },
                    ToolPolicyState::Allow => {}
                    ToolPolicyState::Deny => {
                        return (
                            call,
                            Ok(format!(
                                "Tool execution denied by configuration. The '{}' tool is not allowed.",
                                tool_name
                            )),
                        );
                    }
                }

                let args_str = if call.function.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    call.function.arguments.clone()
                };

                let args: serde_json::Value = match serde_json::from_str(&args_str) {
                    Ok(args) => args,
                    Err(e) => {
                        return (
                            call,
                            Err(LLMError::InvalidRequest(format!("bad args JSON (input: '{}'): {}", args_str, e))),
                        )
                    }
                };

                match provider.call_tool(&call.function.name, args).await {
                    Ok(result) => {
                        let result_str = match serde_json::to_string(&result) {
                            Ok(s) => s,
                            Err(e) => return (call, Err(LLMError::from(e))),
                        };
                        (call, Ok(result_str))
                    }
                    Err(e) => (call, Err(e)),
                }
            });

            let tool_results_from_futures = {
                if let Some(mut sp) = spinner.take() {
                    sp.stop();
                    print!("\r\x1B[K");
                }
                spinner = Some(Spinner::new(
                    Spinners::Dots12,
                    "Executing tools...".bright_cyan().to_string(),
                ));
                let res = join_all(tool_futures).await;
                if let Some(mut sp) = spinner.take() {
                    sp.stop();
                    print!("\r\x1B[K");
                }
                res
            };
            let tool_results = tool_results_from_futures
                .into_iter()
                .map(|(call, result)| {
                    let tool_res_str = match result {
                        Ok(res_str) => res_str,
                        Err(e) => e.to_string(),
                    };
                    log::debug!("Tool {} result: {}", call.function.name, tool_res_str);
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

            // For the next turn, show thinking spinner if not streaming
            spinner = Some(Spinner::new(
                Spinners::Dots12,
                "Thinking...".bright_magenta().to_string(),
            ));
            current = unified_chat(messages, provider).await?;
        } else {
            if let Some(mut sp) = spinner.take() {
                sp.stop();
                print!("\r\x1B[K");
            }
            if !is_stream {
                if !header_printed {
                    println!(
                        "{} {}",
                        "> Assistant:".bright_green(),
                        text.as_deref().unwrap_or_default()
                    );
                } else {
                    println!("{}", text.as_deref().unwrap_or_default());
                }
            }
            messages.push(
                ChatMessage::assistant()
                    .content(text.unwrap_or_default())
                    .build(),
            );
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
    let initial = unified_chat(&messages, provider).await?;
    handle_any_response(&mut messages, initial, provider, tool_config, None).await?;
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
    println!("{}", "Type 'exit' to quit".bright_black());
    print_separator();

    let prompt_prefix = ":: ".bold().red().to_string();
    let helper = QmtHelper {
        completer: FilenameCompleter::new(),
        highlighter: MatchingBracketHighlighter::new(),
        validator: MatchingBracketValidator::new(),
        hinter: HistoryHinter::new(),
        colored_prompt: prompt_prefix.clone(),
    };

    let config = Config::builder()
        .history_ignore_space(true)
        .completion_type(rustyline::CompletionType::List)
        .build();
    let mut rl = Editor::with_config(config)?;
    rl.set_helper(Some(helper));
    rl.bind_sequence(
        KeyEvent(KeyCode::Enter, Modifiers::ALT),
        EventHandler::Simple(Cmd::Newline),
    );
    let mut messages: Vec<ChatMessage> = Vec::new();

    loop {
        io::stdout().flush()?;
        let readline = rl.readline(&prompt_prefix);
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
                let chat_fut = unified_chat(&messages, provider);
                tokio::select! {
                    res = chat_fut => {
                        match res {
                            Ok(initial) => {
                                handle_any_response(
                                    &mut messages,
                                    initial,
                                    provider,
                                    tool_config,
                                    Some(sp),
                                )
                                .await?;
                            }
                            Err(e) => {
                                sp.stop();
                                eprintln!("{} {}", "Error:".bright_red(), e);
                                print_separator();
                            }
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        sp.stop();
                        println!();
                        println!("{}", "Interrupted.".bright_yellow());
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
