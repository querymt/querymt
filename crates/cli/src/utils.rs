use colored::*;
use log::{log_enabled, Level};
use querymt::chat::{ChatMessage, ImageMime};
use querymt::plugin::HTTPLLMProviderFactory;
use querymt::ToolCall;
use rustyline::{error::ReadlineError, Config, Editor};
use serde_json::Value;
use std::io::{stdout, Write};
use std::path::PathBuf;

use crate::secret_store::SecretStore;

#[derive(Clone, Debug)]
enum ToolAction {
    Accept,
    Deny,
    DenyWithReason,
}

impl ToolAction {
    fn from_key(key: char) -> Option<Self> {
        match key {
            'y' => Some(ToolAction::Accept),
            'n' => Some(ToolAction::Deny),
            'r' => Some(ToolAction::DenyWithReason),
            _ => None,
        }
    }
}

fn display_colored_prompt() -> String {
    format!(
        "{} {} {} {}",
        "??".bright_green(),
        "[y]es".bright_green().bold(),
        "[n]o".bright_red().bold(),
        "[r]eason of denial".bright_yellow().bold(),
    )
}

/// Get the API key for a provider.
///
/// Attempts to retrieve the API key in the following order:
/// 1. OAuth access token from the keyring (if valid/not expired)
/// 2. API key stored in the keyring under the env var name
/// 3. Environment variable (e.g., ANTHROPIC_API_KEY)
pub fn get_provider_api_key<P: HTTPLLMProviderFactory + ?Sized>(
    provider_name: &str,
    factory: &P,
) -> Option<String> {
    let store = SecretStore::new().ok()?;

    // Try OAuth access token first
    if let Some(token) = store.get_valid_access_token(provider_name) {
        return Some(token);
    }

    // Fall back to API key from keyring or env var
    let api_key_name = factory.api_key_name()?;
    store
        .get(&api_key_name)
        .or_else(|| std::env::var(api_key_name).ok())
}

/// parse raw `key=val` into `(String, Value)`
pub fn parse_kv(s: &str) -> Result<(String, Value), String> {
    let (key, raw) = s
        .split_once('=')
        .ok_or_else(|| format!("custom-option must be KEY=VALUE, got `{}`", s))?;
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => Ok((key.to_string(), v)),
        Err(_) => Ok((key.to_string(), Value::String(raw.to_string()))),
    }
}

/// Detects the MIME type of an image from its binary data
pub fn detect_image_mime(data: &[u8]) -> Option<ImageMime> {
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

/// Processes input data and creates appropriate chat messages
pub fn process_input(input: &[u8], prompt: String) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(mime) = detect_image_mime(input) {
        messages.push(ChatMessage::user().content(prompt.clone()).build());
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

/// Prints a separator line
pub fn print_separator() {
    println!("{}", "─".repeat(50).bright_black());
}

/// Check if a JSON value is considered empty
fn is_empty_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.is_empty(),
        serde_json::Value::Array(arr) => arr.is_empty(),
        serde_json::Value::Object(obj) => obj.is_empty(),
        _ => false,
    }
}

/// Format tool arguments for display
fn format_tool_args(args_json: &str) -> String {
    if args_json.trim().is_empty() {
        return "".to_string();
    }
    match serde_json::from_str::<serde_json::Value>(args_json) {
        Ok(value) => {
            if is_empty_value(&value) {
                "".to_string()
            } else if let serde_json::Value::Object(map) = value {
                let mut formatted = Vec::new();
                let mut count = 0;
                let max_display = 3;

                for (key, value) in map.iter() {
                    if is_empty_value(value) {
                        continue;
                    }
                    if count >= max_display {
                        let remaining = map.len() - max_display;
                        if remaining > 0 {
                            formatted.push(format!("+{} more", remaining));
                        }
                        break;
                    }

                    let value_str = match value {
                        serde_json::Value::String(s) => {
                            if s.len() > 20 {
                                format!("{}...", &s[..17])
                            } else {
                                s.clone()
                            }
                        }
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        _ => format!("{:?}", value).chars().take(20).collect::<String>(),
                    };

                    formatted.push(format!("{}={}", key, value_str));
                    count += 1;
                }

                formatted.join(", ")
            } else {
                args_json.chars().take(50).collect::<String>()
            }
        }
        Err(_) => args_json.chars().take(50).collect::<String>(),
    }
}

/// Prompt user for tool execution approval with optional reason
/// Returns (approved, optional_reason)
pub fn prompt_tool_execution(tool: &ToolCall) -> Result<(bool, Option<String>), std::io::Error> {
    let formatted_args = format_tool_args(&tool.function.arguments);
    let prompt_line = if formatted_args.is_empty() {
        format!("{} {}", "$$".bright_yellow(), tool.function.name.bold())
    } else {
        format!(
            "{} {}({})",
            "$$".bright_yellow(),
            tool.function.name.bold(),
            formatted_args.bright_black()
        )
    };

    println!("{}", prompt_line);
    println!("{}", display_colored_prompt());

    // Create rustyline editor with config
    let config = Config::builder()
        .history_ignore_space(true)
        .completion_type(rustyline::CompletionType::List)
        .edit_mode(rustyline::EditMode::Emacs)
        .build();

    let mut rl: Editor<(), rustyline::history::DefaultHistory> =
        Editor::with_config(config).map_err(std::io::Error::other)?;

    let readline = rl.readline("");
    clear_prev_lines(3);
    match readline {
        Ok(input) => {
            let key = input.chars().next().unwrap_or(' ');
            match ToolAction::from_key(key) {
                Some(ToolAction::Accept) => Ok((true, None)),
                Some(ToolAction::Deny) => Ok((false, Some("Stop immediately.".to_string()))),
                Some(ToolAction::DenyWithReason) => {
                    match rl.readline(format!("{}: ", "reason".bright_yellow().bold()).as_str()) {
                        Ok(reason_input) => {
                            clear_prev_lines(1);
                            let reason = reason_input.trim();
                            if reason.is_empty() {
                                Ok((false, Some("No reason provided".to_string())))
                            } else {
                                Ok((false, Some(reason.to_string())))
                            }
                        }
                        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                            Ok((false, Some("User cancelled".to_string())))
                        }
                        Err(err) => Err(std::io::Error::other(err)),
                    }
                }
                None => Ok((false, None)),
            }
        }
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            Ok((false, Some("User cancelled".to_string())))
        }
        Err(err) => Err(std::io::Error::other(err)),
    }
}

/// Visualize a tool call start/result
pub fn visualize_tool_call(tool: &ToolCall, result: Option<bool>) {
    let base = tool.function.name.bold();
    let (styled, suffix) = if let Some(ok) = result {
        let lines = if log_enabled!(Level::Debug) { 3 } else { 2 };
        clear_prev_lines(lines);
        (
            if ok {
                base.bright_green()
            } else {
                base.bright_red()
            },
            if ok { "generated" } else { "failed" },
        )
    } else {
        (base.bright_blue(), "calling...")
    };

    println!("┌─ {}", styled);
    if log_enabled!(Level::Debug) {
        println!("│ {}", tool.function.arguments);
    }
    println!("└─ {}", suffix);
}

pub fn clear_prev_lines(n: u16) {
    let mut out = stdout();
    let _ = write!(out, "\x1B[{}A", n); // Use `let _ = ...` to ignore the result
    for _ in 0..n {
        let _ = write!(out, "\x1B[2K\x1B[1B");
    }
    let _ = write!(out, "\x1B[{}A", n);
    // It's also good practice to flush stdout to ensure the writes are visible.
    let _ = out.flush();
}

/// Parse server and tool names for proxied servers
/// If proxied through `hyper-mcp`, it uses scoped tools separated by `::` (e.g., "server::tool").
/// Returns Some((effective_server, effective_tool)) or None if invalid format
pub fn parse_tool_names<'a>(
    server_name: &'a str,
    tool_name: &'a str,
) -> Option<(&'a str, &'a str)> {
    match server_name {
        "hyper-mcp" => {
            let parts: Vec<&str> = tool_name.split("::").collect();
            if parts.len() == 2 {
                Some((parts[0], parts[1]))
            } else {
                None
            }
        }
        _ => Some((server_name, tool_name)),
    }
}

#[derive(Debug, Default)]
pub struct ToolLoadingStats {
    pub total_servers: usize,
    pub total_tools: usize,
    pub enabled_tools: usize,
    pub disabled_tools: usize,
}

impl ToolLoadingStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn increment_server(&mut self) {
        self.total_servers += 1;
    }

    pub fn increment_tool(&mut self, enabled: bool) {
        self.total_tools += 1;
        if enabled {
            self.enabled_tools += 1;
        } else {
            self.disabled_tools += 1;
        }
    }

    pub fn log_summary(&self) {
        log::info!(
            "MCP tools loaded: {} servers, {} total tools ({} enabled, {} disabled)",
            self.total_servers,
            self.total_tools,
            self.enabled_tools,
            self.disabled_tools
        );
    }
}

/// Find a config file in the user's home .qmt directory
pub fn find_config_in_home(filenames: &[&str]) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = dirs::home_dir().ok_or("No home directory found")?;
    let config_dir = home.join(".qmt");

    for filename in filenames {
        let candidate = config_dir.join(filename);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(format!("No config file found in {:?}", config_dir).into())
}
