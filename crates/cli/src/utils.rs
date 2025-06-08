use colored::*;
use log::{log_enabled, Level};
use querymt::chat::{ChatMessage, ImageMime};
use querymt::plugin::HTTPLLMProviderFactory;
use querymt::ToolCall;
use serde_json::Value;
use std::io::{stdout, Write};

use crate::secret_store::SecretStore;

pub fn get_provider_api_key<P: HTTPLLMProviderFactory + ?Sized>(provider: &P) -> Option<String> {
    let store = SecretStore::new().ok()?;
    let api_key_name = provider.api_key_name()?;
    store
        .get(&api_key_name)
        .cloned()
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

/// Visualize a tool call start/result
pub fn visualize_tool_call(tool: &ToolCall, result: Option<bool>) {
    fn clear_prev_lines(n: u16) {
        let mut out = stdout();
        let _ = write!(out, "\x1B[{}A", n); // Use `let _ = ...` to ignore the result
        for _ in 0..n {
            let _ = write!(out, "\x1B[2K\x1B[1B");
        }
        let _ = write!(out, "\x1B[{}A", n);
        // It's also good practice to flush stdout to ensure the writes are visible.
        let _ = out.flush();
    }

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
