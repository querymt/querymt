use crate::cli_args::{ToolConfig, ToolPolicyState};
use crate::utils::{parse_tool_names, print_separator, process_input, prompt_tool_execution};
use colored::*;
use futures::Stream;
use futures::StreamExt;
use futures::future::join_all;
use querymt::{
    LLMProvider,
    chat::{
        ChatMessage, ChatMessagePartDelta, ChatOutput, ChatStreamAccumulator, ChatStreamFinish,
        StreamChunk, StructuredStreamEvent, ToolResultPart, chunk_is_terminal,
    },
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
    Response(ChatOutput),
}

#[derive(Debug, PartialEq, Eq)]
enum StreamDisplayDelta<'a> {
    Text(&'a str),
    Thinking(&'a str),
}

#[derive(Default)]
struct CliStreamState {
    accumulator: ChatStreamAccumulator,
    displayed_text: bool,
    displayed_thinking: bool,
}

impl CliStreamState {
    fn push<'a>(
        &mut self,
        chunk: &'a StreamChunk,
    ) -> Result<Option<StreamDisplayDelta<'a>>, LLMError> {
        self.accumulator
            .push(chunk)
            .map_err(|error| LLMError::ProviderError(error.to_string()))?;

        let display = match chunk {
            StreamChunk::Structured(StructuredStreamEvent::MessagePartDelta {
                delta:
                    ChatMessagePartDelta::Text { delta } | ChatMessagePartDelta::Refusal { delta },
                ..
            }) => {
                self.displayed_text = true;
                Some(StreamDisplayDelta::Text(delta))
            }
            StreamChunk::Structured(StructuredStreamEvent::ReasoningPartDelta {
                delta, ..
            }) => {
                self.displayed_thinking = true;
                Some(StreamDisplayDelta::Thinking(delta))
            }
            StreamChunk::Text(delta) if !self.accumulator.is_structured() => {
                self.displayed_text = true;
                Some(StreamDisplayDelta::Text(delta))
            }
            StreamChunk::Thinking(delta) if !self.accumulator.is_structured() => {
                self.displayed_thinking = true;
                Some(StreamDisplayDelta::Thinking(delta))
            }
            _ => None,
        };
        Ok(display)
    }

    fn finish(self) -> Result<(ChatOutput, bool, bool, bool), LLMError> {
        let displayed_text = self.displayed_text;
        let displayed_thinking = self.displayed_thinking;
        match self.accumulator.finish() {
            ChatStreamFinish::Completed(output) => {
                Ok((output, displayed_text, displayed_thinking, true))
            }
            ChatStreamFinish::Incomplete { output, detail } => {
                log::warn!("Provider returned an incomplete response: {:?}", detail);
                Ok((output, displayed_text, displayed_thinking, false))
            }
            ChatStreamFinish::Failed { error, .. } => {
                Err(LLMError::ProviderError(error.to_string()))
            }
        }
    }
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
        let (output, execute_tools) = match current {
            StreamOrResponse::Stream(mut stream) => {
                let mut state = CliStreamState::default();

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
                            let chunk = chunk_res?;
                            let terminal = chunk_is_terminal(&chunk);
                            if let Some(delta) = state.push(&chunk)? {
                                match delta {
                                    StreamDisplayDelta::Text(text) => print!("{}", text),
                                    StreamDisplayDelta::Thinking(text) => print!("{}", text.dimmed()),
                                }
                                io::stdout().flush().ok();
                            }
                            if terminal {
                                break;
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

                let (output, displayed_text, displayed_thinking, execute_tools) = state.finish()?;
                if !displayed_thinking && let Some(thinking) = output.thinking() {
                    print!("{}", thinking.dimmed());
                }
                if !displayed_text && let Some(text) = output.text() {
                    print!("{}", text);
                }
                println!();
                (output, execute_tools)
            }
            StreamOrResponse::Response(resp) => {
                if let Some(mut sp) = spinner.take() {
                    sp.stop();
                    print!("\r\x1B[K");
                }

                if let Some(usage) = resp.usage.clone() {
                    log::info!(
                        "Tokens usage (in/out): {}/{}",
                        usage.input_tokens,
                        usage.output_tokens
                    );
                }
                (resp, true)
            }
        };
        let text = output.text();
        let tool_calls = execute_tools.then(|| output.tool_calls()).flatten();

        if let Some(ref tcalls) = tool_calls {
            messages.push(ChatMessage::from_assistant_output(output));

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
                                Ok(vec![ToolResultPart::Text {
                                    text: format!(
                                        "Tool execution denied by user. The user chose not to execute the '{}' tool. {}",
                                        tool_name, denial_message
                                    )
                                    .trim()
                                    .to_string(),
                                }]),
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
                                Ok(vec![ToolResultPart::Text {
                                    text: format!(
                                        "Tool execution denied by configuration. The '{}' tool is not allowed.",
                                        tool_name
                                    ),
                                }]),
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
                    Ok(content_blocks) => (call, Ok(content_blocks)),
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
            let mut tool_result_builder = ChatMessage::user();
            for (call, result) in tool_results_from_futures {
                let (result_parts, is_error) = match result {
                    Ok(parts) => {
                        let text_preview: String = parts
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join("\n");
                        log::debug!("Tool {} result: {}", call.function.name, text_preview);
                        (parts, false)
                    }
                    Err(e) => {
                        log::debug!("Tool {} error: {}", call.function.name, e);
                        (
                            vec![ToolResultPart::Text {
                                text: e.to_string(),
                            }],
                            true,
                        )
                    }
                };
                tool_result_builder = tool_result_builder.tool_result(
                    call.id.clone(),
                    Some(call.function.name.clone()),
                    is_error,
                    result_parts,
                );
            }
            messages.push(tool_result_builder.build());

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
            messages.push(ChatMessage::from_assistant_output(output));
            break;
        }
    }
    print_separator();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::{
        ChatMessageItem, ChatMessagePart, ChatOutputItem, ChatOutputStatus, ChatReasoningItem,
        ChatReasoningPart, ChatRole, FinishReason, ReasoningPartKind,
    };

    fn message(text: &str) -> ChatOutputItem {
        ChatOutputItem::Message(ChatMessageItem {
            id: Some("msg_1".into()),
            role: ChatRole::Assistant,
            phase: None,
            status: Some(ChatOutputStatus::Completed),
            parts: vec![ChatMessagePart::Text {
                text: text.into(),
                annotations: Vec::new(),
                extensions: Default::default(),
            }],
            extensions: Default::default(),
        })
    }

    fn metadata() -> StreamChunk {
        StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
            response_id: Some("resp_1".into()),
            status: Some(ChatOutputStatus::InProgress),
            usage: None,
            finish_reason: None,
            provenance: None,
        })
    }

    fn terminal(status: ChatOutputStatus) -> StreamChunk {
        StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
            status,
            usage: None,
            finish_reason: Some(FinishReason::Stop),
            detail: None,
        })
    }

    #[test]
    fn structured_text_delta_is_displayed_and_finalized() {
        let mut state = CliStreamState::default();
        state.push(&metadata()).unwrap();
        state
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemStarted {
                    output_index: 0,
                    item: message(""),
                },
            ))
            .unwrap();
        let delta = StreamChunk::Structured(StructuredStreamEvent::MessagePartDelta {
            output_index: 0,
            content_index: 0,
            delta: ChatMessagePartDelta::Text {
                delta: "Paris".into(),
            },
        });
        assert_eq!(
            state.push(&delta).unwrap(),
            Some(StreamDisplayDelta::Text("Paris"))
        );
        state
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: message("Paris"),
                },
            ))
            .unwrap();
        state.push(&terminal(ChatOutputStatus::Completed)).unwrap();

        let (output, displayed_text, _, execute_tools) = state.finish().unwrap();
        assert_eq!(output.text().as_deref(), Some("Paris"));
        assert!(displayed_text);
        assert!(execute_tools);
    }

    #[test]
    fn final_snapshot_supplies_text_when_no_delta_was_streamed() {
        let mut state = CliStreamState::default();
        state.push(&metadata()).unwrap();
        state
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: message("Madrid"),
                },
            ))
            .unwrap();
        state.push(&terminal(ChatOutputStatus::Completed)).unwrap();

        let (output, displayed_text, _, _) = state.finish().unwrap();
        assert_eq!(output.text().as_deref(), Some("Madrid"));
        assert!(!displayed_text);
    }

    #[test]
    fn structured_reasoning_stays_separate_from_answer_text() {
        let mut state = CliStreamState::default();
        state.push(&metadata()).unwrap();
        state
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemStarted {
                    output_index: 0,
                    item: ChatOutputItem::Reasoning(ChatReasoningItem {
                        id: Some("reasoning_1".into()),
                        summary: vec![ChatReasoningPart::text("")],
                        content: Vec::new(),
                        encrypted_content: None,
                        signature: None,
                        status: Some(ChatOutputStatus::Completed),
                        extensions: Default::default(),
                    }),
                },
            ))
            .unwrap();
        let reasoning_delta = StreamChunk::Structured(StructuredStreamEvent::ReasoningPartDelta {
            output_index: 0,
            part: ReasoningPartKind::Summary,
            part_index: 0,
            delta: "thinking".into(),
        });
        assert_eq!(
            state.push(&reasoning_delta).unwrap(),
            Some(StreamDisplayDelta::Thinking("thinking"))
        );
        state
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: ChatOutputItem::Reasoning(ChatReasoningItem {
                        id: Some("reasoning_1".into()),
                        summary: vec![ChatReasoningPart::text("thinking")],
                        content: Vec::new(),
                        encrypted_content: None,
                        signature: None,
                        status: Some(ChatOutputStatus::Completed),
                        extensions: Default::default(),
                    }),
                },
            ))
            .unwrap();
        state.push(&terminal(ChatOutputStatus::Completed)).unwrap();

        let (output, _, displayed_thinking, _) = state.finish().unwrap();
        assert_eq!(output.thinking().as_deref(), Some("thinking"));
        assert_eq!(output.text(), None);
        assert!(displayed_thinking);
    }

    #[test]
    fn structured_mode_ignores_legacy_display_projections() {
        let mut state = CliStreamState::default();
        state.push(&metadata()).unwrap();
        assert_eq!(
            state.push(&StreamChunk::Text("duplicate".into())).unwrap(),
            None
        );
        state
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: message("canonical"),
                },
            ))
            .unwrap();
        state.push(&terminal(ChatOutputStatus::Completed)).unwrap();

        let (output, displayed_text, _, _) = state.finish().unwrap();
        assert_eq!(output.text().as_deref(), Some("canonical"));
        assert!(!displayed_text);
    }

    #[test]
    fn stream_without_terminal_is_not_a_successful_empty_response() {
        let mut state = CliStreamState::default();
        state.push(&metadata()).unwrap();
        let error = state.finish().unwrap_err();
        assert!(error.to_string().contains("non-terminal status"));
    }

    #[test]
    fn structured_function_call_is_retained_once_and_enabled_after_completion() {
        use querymt::chat::ChatFunctionCallItem;

        let call = ChatOutputItem::FunctionCall(ChatFunctionCallItem {
            item_id: Some("item_1".into()),
            call_id: "call_1".into(),
            name: "distance".into(),
            arguments: r#"{"from":"Madrid","to":"Paris"}"#.into(),
            status: Some(ChatOutputStatus::Completed),
            extensions: Default::default(),
        });
        let mut state = CliStreamState::default();
        state.push(&metadata()).unwrap();
        state
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: call.clone(),
                },
            ))
            .unwrap();
        assert_eq!(
            state
                .push(&StreamChunk::ToolUseComplete {
                    index: 0,
                    tool_call: querymt::ToolCall {
                        id: "call_1".into(),
                        call_type: "function".into(),
                        function: querymt::FunctionCall {
                            name: "distance".into(),
                            arguments: r#"{"from":"Madrid","to":"Paris"}"#.into(),
                        },
                    },
                })
                .unwrap(),
            None
        );
        state.push(&terminal(ChatOutputStatus::Completed)).unwrap();

        let (output, _, _, execute_tools) = state.finish().unwrap();
        let calls = output.tool_calls().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert!(execute_tools);
    }

    #[test]
    fn incomplete_response_retains_partial_output_but_disables_tools() {
        let mut state = CliStreamState::default();
        state.push(&metadata()).unwrap();
        state
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: message("partial"),
                },
            ))
            .unwrap();
        state.push(&terminal(ChatOutputStatus::Incomplete)).unwrap();

        let (output, _, _, execute_tools) = state.finish().unwrap();
        assert_eq!(output.text().as_deref(), Some("partial"));
        assert!(!execute_tools);
    }

    #[test]
    fn legacy_streams_still_render_and_accumulate() {
        let mut state = CliStreamState::default();
        assert_eq!(
            state.push(&StreamChunk::Text("legacy".into())).unwrap(),
            Some(StreamDisplayDelta::Text("legacy"))
        );
        state
            .push(&StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            })
            .unwrap();

        let (output, displayed_text, _, execute_tools) = state.finish().unwrap();
        assert_eq!(output.text().as_deref(), Some("legacy"));
        assert!(displayed_text);
        assert!(execute_tools);
    }
}

/// Handle piped input or single-shot chat
#[tracing::instrument(name = "cli.chat.pipe", skip_all)]
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
#[tracing::instrument(name = "cli.chat.interactive", skip_all)]
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

                messages.push(ChatMessage::user().text(trimmed.to_string()).build());
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
