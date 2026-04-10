//! LLM-powered tool output compressor using the squeez protocol.
//!
//! [Squeez](https://github.com/KRLabsOrg/squeez) is a task-conditioned tool
//! output pruner trained on SWE-bench data. Given a query and raw tool output
//! it returns only the lines the agent needs to read next.
//!
//! This compressor wraps a [`ChatProvider`] (typically backed by a squeez GGUF
//! model via llama-cpp) and delegates to a fallback compressor when the output
//! is too small to benefit or when the model call fails.

use async_trait::async_trait;
use querymt::chat::{ChatMessage, ChatRole, Content};
use std::sync::Arc;

use crate::tools::compressor::{CompressionContext, CompressionResult, ToolOutputCompressor};

/// System prompt expected by squeez models.
///
/// This must be configured on the [`ChatProvider`] (e.g. via
/// `LlamaCppConfig::system`). It is exposed here as documentation and for
/// test assertions.
pub const SQUEEZ_SYSTEM_PROMPT: &str = "\
    You prune verbose tool output for a coding agent. \
    Given a focused extraction query and one tool output, return only the \
    smallest verbatim evidence block(s) the agent should read next. \
    Return the kept text inside <relevant_lines> tags. \
    Do not rewrite, summarize, or invent lines.";

/// Maximum characters of tool arguments included in the squeez query.
const MAX_TASK_CHARS: usize = 3000;

/// Maximum characters of tool output sent to squeez.
/// Beyond this the input is pre-truncated so it fits the model's context.
const DEFAULT_MAX_INPUT_CHARS: usize = 24_000;

/// Configuration knobs for [`SqueezCompressor`].
#[derive(Debug, Clone)]
pub struct SqueezCompressorConfig {
    /// Minimum output lines before squeez is invoked.
    pub min_lines: usize,
    /// Minimum output bytes before squeez is invoked.
    pub min_bytes: usize,
    /// Maximum input characters sent to the model.
    pub max_input_chars: usize,
}

impl Default for SqueezCompressorConfig {
    fn default() -> Self {
        Self {
            min_lines: 100,
            min_bytes: 4096,
            max_input_chars: DEFAULT_MAX_INPUT_CHARS,
        }
    }
}

/// Compressor that uses a squeez model to extract only the relevant lines
/// from verbose tool output.
///
/// Falls back to a wrapped [`ToolOutputCompressor`] (normally
/// [`TruncationCompressor`](super::compressor_truncation::TruncationCompressor))
/// when:
/// - the output is below the configured thresholds, or
/// - the model call fails or returns empty/unparseable output.
pub struct SqueezCompressor {
    provider: Arc<dyn querymt::chat::ChatProvider>,
    fallback: Arc<dyn ToolOutputCompressor>,
    config: SqueezCompressorConfig,
}

impl SqueezCompressor {
    pub fn new(
        provider: Arc<dyn querymt::chat::ChatProvider>,
        fallback: Arc<dyn ToolOutputCompressor>,
        config: SqueezCompressorConfig,
    ) -> Self {
        Self {
            provider,
            fallback,
            config,
        }
    }

    /// Build the user message using the squeez XML protocol.
    fn build_user_content(task: &str, tool_output: &str) -> String {
        if task.is_empty() {
            format!("<tool_output>\n{tool_output}\n</tool_output>")
        } else {
            format!("<query>\n{task}\n</query>\n<tool_output>\n{tool_output}\n</tool_output>")
        }
    }

    /// Build task description from tool call metadata.
    fn build_task(ctx: &CompressionContext<'_>) -> String {
        let args = if ctx.tool_arguments.len() > MAX_TASK_CHARS {
            &ctx.tool_arguments[..MAX_TASK_CHARS]
        } else {
            ctx.tool_arguments
        };
        format!("Agent called '{}': {}", ctx.tool_name, args)
    }

    /// Call the squeez model and parse the `<relevant_lines>` response.
    async fn call_squeez(&self, task: &str, tool_output: &str) -> Result<String, String> {
        let user_content = Self::build_user_content(task, tool_output);
        let messages = vec![ChatMessage {
            role: ChatRole::User,
            content: vec![Content::text(&user_content)],
            cache: None,
        }];

        let response = self
            .provider
            .chat(&messages)
            .await
            .map_err(|e| format!("squeez model call failed: {e}"))?;

        let text = response.text().unwrap_or_default();

        // Prefer the structured XML extraction.
        if let Some(extracted) = parse_relevant_lines(&text) {
            return Ok(extracted);
        }

        // Some models omit the <relevant_lines> wrapper.  Accept the raw
        // response when it is substantial enough to be a real extraction
        // (not a short refusal or error message).
        if !text.is_empty() && text.len() > 50 {
            log::debug!(
                "squeez omitted <relevant_lines> tags, using raw response (len={})",
                text.len(),
            );
            return Ok(text.trim().to_string());
        }

        Err(if text.is_empty() {
            "squeez returned empty response".to_string()
        } else {
            format!("squeez response too short to be useful (len={})", text.len())
        })
    }
}

#[async_trait]
impl ToolOutputCompressor for SqueezCompressor {
    async fn compress(&self, raw_text: &str, ctx: &CompressionContext<'_>) -> CompressionResult {
        let lines = raw_text.lines().count();
        let bytes = raw_text.len();

        // Gate: skip squeez for small outputs — no benefit, only latency.
        if lines < self.config.min_lines && bytes < self.config.min_bytes {
            return self.fallback.compress(raw_text, ctx).await;
        }

        // Pre-truncate if the output would blow past the model's context window.
        let input = if raw_text.len() > self.config.max_input_chars {
            &raw_text[..self.config.max_input_chars]
        } else {
            raw_text
        };

        let task = Self::build_task(ctx);

        match self.call_squeez(&task, input).await {
            Ok(filtered) if !filtered.is_empty() => {
                let filtered_lines = filtered.lines().count();
                let pct = if bytes > 0 {
                    (1.0 - filtered.len() as f64 / bytes as f64) * 100.0
                } else {
                    0.0
                };
                let footer = format!(
                    "\n\n[squeez: {lines} -> {filtered_lines} lines, {pct:.0}% compression]"
                );

                CompressionResult {
                    blocks: vec![Content::text(format!("{filtered}{footer}"))],
                    was_compressed: true,
                    original_lines: lines,
                    original_bytes: bytes,
                }
            }
            Ok(_) => {
                log::warn!("squeez returned empty extraction, falling back to truncation");
                self.fallback.compress(raw_text, ctx).await
            }
            Err(e) => {
                log::warn!("squeez failed ({e}), falling back to truncation");
                self.fallback.compress(raw_text, ctx).await
            }
        }
    }

    fn name(&self) -> &'static str {
        "squeez"
    }
}

// ─── XML parsing ────────────────────────────────────────────────────────────

/// Extract the content between `<relevant_lines>` and `</relevant_lines>`.
fn parse_relevant_lines(text: &str) -> Option<String> {
    let start_tag = "<relevant_lines>";
    let end_tag = "</relevant_lines>";
    let start = text.find(start_tag)? + start_tag.len();
    let end = text[start..].find(end_tag)? + start;
    let inner = text[start..end].trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_relevant_lines_basic() {
        let input = "some preamble\n<relevant_lines>\nline A\nline B\n</relevant_lines>\ntrailer";
        assert_eq!(
            parse_relevant_lines(input),
            Some("line A\nline B".to_string())
        );
    }

    #[test]
    fn parse_relevant_lines_empty_tags() {
        assert_eq!(
            parse_relevant_lines("<relevant_lines>\n</relevant_lines>"),
            None
        );
    }

    #[test]
    fn parse_relevant_lines_no_tags() {
        assert_eq!(parse_relevant_lines("just some text"), None);
    }

    #[test]
    fn build_user_content_with_task() {
        let content = SqueezCompressor::build_user_content("fix auth", "error on line 42");
        assert!(content.contains("<query>"));
        assert!(content.contains("fix auth"));
        assert!(content.contains("<tool_output>"));
    }

    #[test]
    fn build_user_content_without_task() {
        let content = SqueezCompressor::build_user_content("", "error on line 42");
        assert!(!content.contains("<query>"));
        assert!(content.contains("<tool_output>"));
    }

    #[test]
    fn build_task_truncates_long_args() {
        let long_args = "x".repeat(5000);
        let ctx = CompressionContext {
            tool_name: "shell",
            tool_arguments: &long_args,
            tool_call_id: "c1",
            session_id: "s1",
            tool_hint: None,
        };
        let task = SqueezCompressor::build_task(&ctx);
        // Should be capped at MAX_TASK_CHARS for the args portion
        assert!(task.len() < 3200);
    }
}
