//! Blind head/tail truncation compressor (the original default).
//!
//! This wraps the existing [`truncate_output`] logic into the
//! [`ToolOutputCompressor`] trait so it can be used interchangeably with
//! other strategies (e.g. squeez).

use async_trait::async_trait;
use querymt::chat::Content;

use crate::config::ToolOutputConfig;
use crate::tools::builtins::helpers::{
    TruncationDirection, format_truncation_message_with_overflow, save_overflow_output,
    truncate_output,
};
use crate::tools::compressor::{CompressionContext, CompressionResult, ToolOutputCompressor};

/// Compressor that truncates output by line count / byte size and saves the
/// full content to an overflow file.
///
/// This is the original Layer 1 behaviour, now behind the
/// [`ToolOutputCompressor`] trait.
pub struct TruncationCompressor {
    config: ToolOutputConfig,
}

impl TruncationCompressor {
    pub fn new(config: ToolOutputConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ToolOutputCompressor for TruncationCompressor {
    async fn compress(&self, raw_text: &str, ctx: &CompressionContext<'_>) -> CompressionResult {
        let original_lines = raw_text.lines().count();
        let original_bytes = raw_text.len();

        let truncation = truncate_output(
            raw_text,
            self.config.max_lines,
            self.config.max_bytes,
            TruncationDirection::Head,
        );

        if !truncation.was_truncated {
            return CompressionResult {
                blocks: vec![Content::text(raw_text)],
                was_compressed: false,
                original_lines,
                original_bytes,
            };
        }

        let overflow = save_overflow_output(
            raw_text,
            &self.config.overflow_storage,
            ctx.session_id,
            ctx.tool_call_id,
            None,
        );

        let suffix = format_truncation_message_with_overflow(
            &truncation,
            TruncationDirection::Head,
            Some(&overflow),
            ctx.tool_hint,
        );

        CompressionResult {
            blocks: vec![Content::text(format!("{}{}", truncation.content, suffix))],
            was_compressed: true,
            original_lines,
            original_bytes,
        }
    }

    fn name(&self) -> &'static str {
        "truncation"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OverflowStorage;

    fn default_config() -> ToolOutputConfig {
        ToolOutputConfig {
            max_lines: 10,
            max_bytes: 500,
            overflow_storage: OverflowStorage::Discard,
            ..Default::default()
        }
    }

    fn ctx<'a>() -> CompressionContext<'a> {
        CompressionContext {
            tool_name: "shell",
            tool_arguments: r#"{"command":"ls"}"#,
            tool_call_id: "call_1",
            session_id: "sess_1",
            tool_hint: None,
        }
    }

    #[tokio::test]
    async fn small_output_passes_through() {
        let c = TruncationCompressor::new(default_config());
        let result = c.compress("line1\nline2\nline3", &ctx()).await;
        assert!(!result.was_compressed);
        assert_eq!(result.original_lines, 3);
        assert_eq!(result.blocks.len(), 1);
    }

    #[tokio::test]
    async fn large_output_is_truncated() {
        let c = TruncationCompressor::new(default_config());
        let big = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = c.compress(&big, &ctx()).await;
        assert!(result.was_compressed);
        assert_eq!(result.original_lines, 50);
        // Truncated to max_lines=10 + footer
        let text = result.blocks[0].as_text().unwrap();
        assert!(text.contains("[Output truncated"));
    }
}
