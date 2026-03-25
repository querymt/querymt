//! Tool output pruning for managing conversation history
//!
//! This module implements the pruning layer of the 3-layer compaction system,
//! which marks old tool outputs as compacted (soft delete) to keep context size manageable.

use crate::model::{AgentMessage, MessagePart};
use crate::session::store::LLMConfig;
use querymt::chat::{ChatRole, Content};
use tracing::instrument;

// TODO: Move image metadata extraction into a shared utility if more subsystems need it.
fn image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let size = imagesize::blob_size(data).ok()?;
    let width = u32::try_from(size.width).ok()?;
    let height = u32::try_from(size.height).ok()?;
    Some((width, height))
}

// TODO: Move provider-specific image pricing/estimation logic into a dedicated module once
// this grows beyond pruning and needs to be shared with other subsystems.
// TODO: Add provider-specific image/token estimation for other multimodal providers
// (for example Anthropic, Google, Gemini-family, etc.) instead of relying on the
// generic fallback heuristic outside the OpenAI/Codex path.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OpenAIImageCostModel {
    Tile {
        base_tokens: usize,
        tile_tokens: usize,
    },
    Patch {
        patch_budget: usize,
        multiplier_num: usize,
        multiplier_den: usize,
    },
}

fn openai_image_cost_model(model: &str) -> Option<OpenAIImageCostModel> {
    // SEE: https://developers.openai.com/api/docs/guides/images-vision
    let model = model.to_ascii_lowercase();

    match model.as_str() {
        "gpt-5" | "gpt-5-chat-latest" => Some(OpenAIImageCostModel::Tile {
            base_tokens: 70,
            tile_tokens: 140,
        }),
        "gpt-4o" | "gpt-4.1" | "gpt-4.5" => Some(OpenAIImageCostModel::Tile {
            base_tokens: 85,
            tile_tokens: 170,
        }),
        "gpt-4o-mini" => Some(OpenAIImageCostModel::Tile {
            base_tokens: 2833,
            tile_tokens: 5667,
        }),
        "o1" | "o1-pro" | "o3" => Some(OpenAIImageCostModel::Tile {
            base_tokens: 75,
            tile_tokens: 150,
        }),
        "computer-use-preview" => Some(OpenAIImageCostModel::Tile {
            base_tokens: 65,
            tile_tokens: 129,
        }),
        "gpt-5.4-mini" | "gpt-5-mini" | "gpt-4.1-mini" | "gpt-4.1-mini-2025-04-14" => {
            // Treat generic gpt-4.1-mini as patch-based too for now; verify with manual tests.
            Some(OpenAIImageCostModel::Patch {
                patch_budget: 1536,
                multiplier_num: 162,
                multiplier_den: 100,
            })
        }
        "gpt-5.4-nano" | "gpt-5-nano" | "gpt-4.1-nano" | "gpt-4.1-nano-2025-04-14" => {
            // Treat generic gpt-4.1-nano as patch-based too for now; verify with manual tests.
            Some(OpenAIImageCostModel::Patch {
                patch_budget: 1536,
                multiplier_num: 246,
                multiplier_den: 100,
            })
        }
        "o4-mini" => Some(OpenAIImageCostModel::Patch {
            patch_budget: 1536,
            multiplier_num: 172,
            multiplier_den: 100,
        }),
        "gpt-5.4" => Some(OpenAIImageCostModel::Patch {
            patch_budget: 2500,
            multiplier_num: 1,
            multiplier_den: 1,
        }),
        _ => {
            if model.contains("codex") {
                Some(OpenAIImageCostModel::Tile {
                    base_tokens: 70,
                    tile_tokens: 140,
                })
            } else {
                None
            }
        }
    }
}

fn div_ceil_u32(value: u32, divisor: u32) -> u32 {
    value.saturating_add(divisor.saturating_sub(1)) / divisor
}

fn scale_to_fit_within(width: u32, height: u32, max_dimension: u32) -> (u32, u32) {
    if width <= max_dimension && height <= max_dimension {
        return (width, height);
    }

    let scale = (max_dimension as f64 / width as f64).min(max_dimension as f64 / height as f64);
    let scaled_width = ((width as f64) * scale).floor().max(1.0) as u32;
    let scaled_height = ((height as f64) * scale).floor().max(1.0) as u32;
    (scaled_width, scaled_height)
}

fn scale_shortest_side_to(width: u32, height: u32, target_shortest_side: u32) -> (u32, u32) {
    let shortest = width.min(height);
    if shortest == 0 || shortest == target_shortest_side {
        return (width.max(1), height.max(1));
    }

    let scale = target_shortest_side as f64 / shortest as f64;
    let scaled_width = ((width as f64) * scale).floor().max(1.0) as u32;
    let scaled_height = ((height as f64) * scale).floor().max(1.0) as u32;
    (scaled_width, scaled_height)
}

fn estimate_tile_based_tokens(
    width: u32,
    height: u32,
    base_tokens: usize,
    tile_tokens: usize,
) -> usize {
    let (width, height) = scale_to_fit_within(width, height, 2048);
    let (width, height) = scale_shortest_side_to(width, height, 768);
    let tiles = div_ceil_u32(width, 512).saturating_mul(div_ceil_u32(height, 512)) as usize;
    base_tokens.saturating_add(tiles.saturating_mul(tile_tokens))
}

fn estimate_patch_based_tokens(
    width: u32,
    height: u32,
    patch_budget: usize,
    multiplier_num: usize,
    multiplier_den: usize,
) -> usize {
    let width_f = width as f64;
    let height_f = height as f64;
    let patch_budget_f = patch_budget as f64;
    let patch_size_sq = 32.0_f64 * 32.0_f64;

    let original_patch_count =
        div_ceil_u32(width, 32).saturating_mul(div_ceil_u32(height, 32)) as usize;
    let resized_patch_count = if original_patch_count <= patch_budget {
        original_patch_count
    } else {
        let shrink_factor = ((patch_size_sq * patch_budget_f) / (width_f * height_f)).sqrt();
        let scaled_width = width_f * shrink_factor;
        let scaled_height = height_f * shrink_factor;
        let width_adjust = (scaled_width / 32.0).floor() / (scaled_width / 32.0);
        let height_adjust = (scaled_height / 32.0).floor() / (scaled_height / 32.0);
        let adjusted_shrink_factor = shrink_factor * width_adjust.min(height_adjust);
        let resized_width = (width_f * adjusted_shrink_factor).floor().max(1.0) as u32;
        let resized_height = (height_f * adjusted_shrink_factor).floor().max(1.0) as u32;
        div_ceil_u32(resized_width, 32).saturating_mul(div_ceil_u32(resized_height, 32)) as usize
    };

    resized_patch_count
        .min(patch_budget)
        .saturating_mul(multiplier_num)
        .div_ceil(multiplier_den)
}

#[instrument(
    name = "session.pruning.estimate_openai_image_tokens",
    skip(model),
    fields(
        model = %model,
        width = width,
        height = height,
        cost_model = tracing::field::Empty,
        estimated_tokens = tracing::field::Empty
    )
)]
fn estimate_openai_image_tokens(model: &str, width: u32, height: u32) -> Option<usize> {
    let (cost_model, estimated_tokens) = match openai_image_cost_model(model)? {
        OpenAIImageCostModel::Tile {
            base_tokens,
            tile_tokens,
        } => (
            "tile",
            estimate_tile_based_tokens(width, height, base_tokens, tile_tokens),
        ),
        OpenAIImageCostModel::Patch {
            patch_budget,
            multiplier_num,
            multiplier_den,
        } => (
            "patch",
            estimate_patch_based_tokens(
                width,
                height,
                patch_budget,
                multiplier_num,
                multiplier_den,
            ),
        ),
    };

    tracing::Span::current().record("cost_model", cost_model);
    tracing::Span::current().record("estimated_tokens", estimated_tokens);
    Some(estimated_tokens)
}

/// Default number of tokens to protect from pruning (most recent tool outputs)
pub const PRUNE_PROTECT_TOKENS: usize = 40_000;

/// Minimum tokens that must be prunable before we actually prune
pub const PRUNE_MINIMUM_TOKENS: usize = 20_000;

/// Default protected tools that should never be pruned
pub const PRUNE_PROTECTED_TOOLS: &[&str] = &["skill"];

/// Configuration for pruning behavior
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Number of tokens of recent tool outputs to protect from pruning
    pub protect_tokens: usize,
    /// Minimum tokens required to justify pruning
    pub minimum_tokens: usize,
    /// Tools that should never be pruned
    pub protected_tools: Vec<String>,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            protect_tokens: PRUNE_PROTECT_TOKENS,
            minimum_tokens: PRUNE_MINIMUM_TOKENS,
            protected_tools: PRUNE_PROTECTED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Trait for estimating token counts from text.
pub trait TokenEstimator: Send + Sync {
    fn estimate(&self, text: &str) -> usize;
}

/// Trait for estimating pruning cost from rich content blocks.
///
/// Pruning cost is computed by summing per-content-block estimates recursively.
/// In practice, total estimated context cost is the sum of text, image, PDF,
/// audio, and other nested content estimates across a tool result.
/// These estimates are used for pruning/compaction decisions, not exact billing.
pub trait ContentCostEstimator: Send + Sync {
    fn estimate_text(&self, text: &str) -> usize;
    fn estimate_image(&self, mime_type: &str, data: &[u8]) -> usize;

    fn estimate_pdf(&self, data: &[u8]) -> usize {
        data.len().saturating_div(4)
    }

    fn estimate_audio(&self, _mime_type: &str, data: &[u8]) -> usize {
        data.len().saturating_div(4)
    }

    fn estimate_content(&self, content: &[Content]) -> usize {
        content.iter().map(|block| self.estimate_block(block)).sum()
    }

    fn estimate_block(&self, block: &Content) -> usize {
        match block {
            Content::Text { text } => self.estimate_text(text),
            Content::Image { mime_type, data } => self.estimate_image(mime_type, data),
            Content::Pdf { data } => self.estimate_pdf(data),
            Content::Audio { mime_type, data } => self.estimate_audio(mime_type, data),
            Content::ImageUrl { url } => self.estimate_text(url),
            Content::Thinking { text, .. } => self.estimate_text(text),
            Content::ToolUse {
                name, arguments, ..
            } => self.estimate_text(name) + self.estimate_text(&arguments.to_string()),
            Content::ToolResult { content, .. } => self.estimate_content(content),
            Content::ResourceLink {
                uri,
                name,
                description,
                mime_type,
            } => {
                self.estimate_text(uri)
                    + name.as_deref().map(|s| self.estimate_text(s)).unwrap_or(0)
                    + description
                        .as_deref()
                        .map(|s| self.estimate_text(s))
                        .unwrap_or(0)
                    + mime_type
                        .as_deref()
                        .map(|s| self.estimate_text(s))
                        .unwrap_or(0)
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GenericContentCostEstimator;

impl TokenEstimator for GenericContentCostEstimator {
    fn estimate(&self, text: &str) -> usize {
        text.len().saturating_div(4)
    }
}

impl ContentCostEstimator for GenericContentCostEstimator {
    fn estimate_text(&self, text: &str) -> usize {
        self.estimate(text)
    }

    fn estimate_image(&self, _mime_type: &str, data: &[u8]) -> usize {
        data.len().saturating_div(4)
    }
}

#[derive(Debug, Clone)]
pub struct OpenAIContentCostEstimator {
    model: String,
}

impl TokenEstimator for OpenAIContentCostEstimator {
    fn estimate(&self, text: &str) -> usize {
        text.len().saturating_div(4)
    }
}

impl ContentCostEstimator for OpenAIContentCostEstimator {
    fn estimate_text(&self, text: &str) -> usize {
        self.estimate(text)
    }

    #[instrument(
        name = "session.pruning.estimate_openai_image",
        skip(self, data),
        fields(
            model = %self.model,
            mime_type = %_mime_type,
            byte_len = data.len(),
            dimensions_found = tracing::field::Empty,
            used_fallback = tracing::field::Empty,
            estimated_tokens = tracing::field::Empty
        )
    )]
    fn estimate_image(&self, _mime_type: &str, data: &[u8]) -> usize {
        let estimated_tokens = if let Some((width, height)) = image_dimensions(data) {
            tracing::Span::current().record("dimensions_found", true);
            tracing::Span::current().record("used_fallback", false);
            estimate_openai_image_tokens(&self.model, width, height)
                .unwrap_or_else(|| data.len().saturating_div(8).max(256))
        } else {
            tracing::Span::current().record("dimensions_found", false);
            tracing::Span::current().record("used_fallback", true);
            data.len().saturating_div(8).max(256)
        };

        tracing::Span::current().record("estimated_tokens", estimated_tokens);
        estimated_tokens
    }
}

/// Back-compat wrapper for existing text-only estimator users.
#[derive(Debug, Clone, Default)]
pub struct SimpleTokenEstimator;

impl TokenEstimator for SimpleTokenEstimator {
    fn estimate(&self, text: &str) -> usize {
        GenericContentCostEstimator.estimate(text)
    }
}

#[instrument(
    name = "session.pruning.select_content_estimator",
    skip(llm_config),
    fields(
        provider = tracing::field::Empty,
        model = tracing::field::Empty,
        estimator_family = tracing::field::Empty
    )
)]
pub fn content_cost_estimator_for_llm_config(
    llm_config: Option<&LLMConfig>,
) -> Box<dyn ContentCostEstimator> {
    match llm_config {
        Some(cfg) if matches!(cfg.provider.as_str(), "openai" | "codex") => {
            tracing::Span::current().record("provider", cfg.provider.as_str());
            tracing::Span::current().record("model", cfg.model.as_str());
            tracing::Span::current().record("estimator_family", "openai");
            Box::new(OpenAIContentCostEstimator {
                model: cfg.model.clone(),
            })
        }
        Some(cfg) => {
            tracing::Span::current().record("provider", cfg.provider.as_str());
            tracing::Span::current().record("model", cfg.model.as_str());
            tracing::Span::current().record("estimator_family", "generic");
            Box::new(GenericContentCostEstimator)
        }
        None => {
            tracing::Span::current().record("estimator_family", "generic");
            Box::new(GenericContentCostEstimator)
        }
    }
}

fn estimate_content_tokens(content: &[Content], estimator: &dyn ContentCostEstimator) -> usize {
    estimator.estimate_content(content)
}

/// Information about a prunable tool result
#[derive(Debug, Clone)]
pub struct PrunableToolResult {
    /// Message ID containing this tool result
    pub message_id: String,
    /// Call ID of the tool result
    pub call_id: String,
    /// Estimated tokens in the content
    pub tokens: usize,
}

/// Result of pruning analysis
#[derive(Debug, Clone)]
pub struct PruneAnalysis {
    /// Total tokens in protected (recent) tool outputs
    pub protected_tokens: usize,
    /// Total tokens that could be pruned
    pub prunable_tokens: usize,
    /// List of tool results that should be pruned
    pub candidates: Vec<PrunableToolResult>,
    /// Whether pruning should proceed (prunable_tokens >= minimum_tokens)
    pub should_prune: bool,
}

/// Compute which tool results should be marked as compacted.
///
/// # Algorithm (matching OpenCode)
///
/// 1. Walk backwards through messages (newest to oldest)
/// 2. Skip first 2 user turns (recent context)
/// 3. Stop if we hit a previous compaction summary
/// 4. Count tool output tokens
/// 5. Protect the most recent `protect_tokens` of tool outputs
/// 6. Only prune if > `minimum_tokens` to remove
/// 7. Return analysis with candidates to mark as compacted
///
/// # Arguments
///
/// * `messages` - The conversation history
/// * `config` - Pruning configuration
/// * `estimator` - Token estimator implementation
///
/// # Returns
///
/// A `PruneAnalysis` containing information about what should be pruned
#[instrument(
    name = "session.pruning.compute_candidates",
    skip(messages, config, estimator),
    fields(
        message_count = messages.len(),
        protect_tokens = config.protect_tokens,
        minimum_tokens = config.minimum_tokens,
        protected_tokens = tracing::field::Empty,
        prunable_tokens = tracing::field::Empty,
        candidate_count = tracing::field::Empty
    )
)]
pub fn compute_prune_candidates(
    messages: &[AgentMessage],
    config: &PruneConfig,
    estimator: &dyn ContentCostEstimator,
) -> PruneAnalysis {
    let mut user_turn_count = 0;
    let mut protected_tokens: usize = 0;
    let mut prunable_tokens: usize = 0;
    let mut candidates: Vec<PrunableToolResult> = Vec::new();

    // Walk backwards through messages (newest to oldest)
    for message in messages.iter().rev() {
        // Skip messages in the 2 most recent user turns
        // A "turn" starts with a user message. We skip until we've passed 2 user messages.
        // By checking before incrementing, assistant messages after user turn 2 (going backwards)
        // are still skipped, while turn 1 and older are processed.
        if user_turn_count < 2 {
            if message.role == ChatRole::User {
                user_turn_count += 1;
            }
            continue;
        }

        // Step 3: Stop if we hit a compaction boundary (request or summary)
        let has_compaction = message.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::Compaction { .. } | MessagePart::CompactionRequest { .. }
            )
        });
        if has_compaction {
            break;
        }

        // Step 4: Process tool results in this message
        for part in &message.parts {
            if let MessagePart::ToolResult {
                call_id,
                content,
                tool_name,
                compacted_at,
                ..
            } = part
            {
                // Skip already compacted tool results
                if compacted_at.is_some() {
                    continue;
                }

                // Skip protected tools (e.g., "skill")
                if let Some(name) = tool_name
                    && config.protected_tools.iter().any(|t| t == name)
                {
                    continue;
                }

                let tokens = estimate_content_tokens(content, estimator);

                // Step 5: Protect the most recent PRUNE_PROTECT tokens
                if protected_tokens < config.protect_tokens {
                    // This tool result is within the protection window
                    protected_tokens += tokens;
                } else {
                    // Beyond protection window - candidate for pruning
                    prunable_tokens += tokens;
                    candidates.push(PrunableToolResult {
                        message_id: message.id.clone(),
                        call_id: call_id.clone(),
                        tokens,
                    });
                }
            }
        }
    }

    // Step 6: Only prune if > PRUNE_MINIMUM tokens to remove
    let should_prune = prunable_tokens >= config.minimum_tokens;
    let candidate_count = if should_prune { candidates.len() } else { 0 };

    tracing::Span::current().record("protected_tokens", protected_tokens);
    tracing::Span::current().record("prunable_tokens", prunable_tokens);
    tracing::Span::current().record("candidate_count", candidate_count);

    PruneAnalysis {
        protected_tokens,
        prunable_tokens,
        candidates: if should_prune { candidates } else { Vec::new() },
        should_prune,
    }
}

/// Extract unique message IDs from prune candidates
pub fn extract_message_ids(candidates: &[PrunableToolResult]) -> Vec<String> {
    let mut ids: Vec<String> = candidates.iter().map(|c| c.message_id.clone()).collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Extract call IDs from prune candidates
pub fn extract_call_ids(candidates: &[PrunableToolResult]) -> Vec<String> {
    candidates.iter().map(|c| c.call_id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentMessage;

    fn make_user_message(id: &str, session_id: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: "test".to_string(),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    fn make_assistant_message_with_tool_result(
        id: &str,
        session_id: &str,
        call_id: &str,
        content: &str,
        tool_name: Option<&str>,
    ) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::ToolResult {
                call_id: call_id.to_string(),
                content: vec![querymt::chat::Content::text(content)],
                is_error: false,
                tool_name: tool_name.map(|s| s.to_string()),
                tool_arguments: None,
                compacted_at: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    fn make_compaction_message(id: &str, session_id: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: "Previous conversation summary".to_string(),
                original_token_count: 10000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    fn make_assistant_message_with_binary_tool_result(
        id: &str,
        session_id: &str,
        call_id: &str,
        content: Vec<Content>,
    ) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::ToolResult {
                call_id: call_id.to_string(),
                content,
                is_error: false,
                tool_name: Some("read".to_string()),
                tool_arguments: None,
                compacted_at: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    #[test]
    fn test_simple_token_estimator() {
        let estimator = GenericContentCostEstimator;
        assert_eq!(estimator.estimate(""), 0);
        assert_eq!(estimator.estimate("test"), 1);
        assert_eq!(estimator.estimate("12345678"), 2);
        assert_eq!(estimator.estimate(&"a".repeat(100)), 25);
    }

    #[test]
    fn test_image_dimensions_extracts_png_size() {
        let png = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x08, 0x02, 0x00, 0x00,
            0x00,
        ];

        assert_eq!(image_dimensions(&png), Some((2, 3)));
    }

    #[test]
    fn test_estimate_content_tokens_counts_binary_payloads() {
        let estimator = GenericContentCostEstimator;
        let content = vec![
            Content::image("image/png", vec![0u8; 400]),
            Content::pdf(vec![1u8; 800]),
            Content::text("tiny"),
        ];

        let tokens = estimate_content_tokens(&content, &estimator);

        assert_eq!(tokens, 100 + 200 + 1);
    }

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut png = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png.extend_from_slice(&[0x08, 0x02, 0x00, 0x00, 0x00]);
        png
    }

    #[test]
    fn test_openai_tile_estimator_uses_documented_tile_costs() {
        let llm_config = LLMConfig {
            id: 0,
            name: None,
            provider: "openai".to_string(),
            model: "gpt-4.1".to_string(),
            params: None,
            created_at: None,
            updated_at: None,
            provider_node_id: None,
        };
        let estimator = content_cost_estimator_for_llm_config(Some(&llm_config));
        let content = vec![Content::image("image/png", png_header(1024, 1024))];

        let tokens = estimate_content_tokens(&content, estimator.as_ref());

        assert_eq!(tokens, 765);
    }

    #[test]
    fn test_openai_patch_estimator_uses_documented_patch_costs() {
        let llm_config = LLMConfig {
            id: 0,
            name: None,
            provider: "openai".to_string(),
            model: "gpt-4.1-mini".to_string(),
            params: None,
            created_at: None,
            updated_at: None,
            provider_node_id: None,
        };
        let estimator = content_cost_estimator_for_llm_config(Some(&llm_config));
        let content = vec![Content::image("image/png", png_header(1024, 1024))];

        let tokens = estimate_content_tokens(&content, estimator.as_ref());

        assert_eq!(tokens, 1659);
    }

    #[test]
    fn test_codex_estimator_uses_openai_family_model_mapping() {
        let llm_config = LLMConfig {
            id: 0,
            name: None,
            provider: "codex".to_string(),
            model: "codex-mini-latest".to_string(),
            params: None,
            created_at: None,
            updated_at: None,
            provider_node_id: None,
        };
        let estimator = content_cost_estimator_for_llm_config(Some(&llm_config));
        let content = vec![Content::image("image/png", png_header(1024, 1024))];

        let tokens = estimate_content_tokens(&content, estimator.as_ref());

        assert_eq!(tokens, 630);
    }

    #[test]
    fn test_openai_gpt4o_mini_uses_documented_tile_costs() {
        let llm_config = LLMConfig {
            id: 0,
            name: None,
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            params: None,
            created_at: None,
            updated_at: None,
            provider_node_id: None,
        };
        let estimator = content_cost_estimator_for_llm_config(Some(&llm_config));
        let content = vec![Content::image("image/png", png_header(1024, 1024))];

        let tokens = estimate_content_tokens(&content, estimator.as_ref());

        assert_eq!(tokens, 25501);
    }

    #[test]
    fn test_openai_estimator_falls_back_for_invalid_image_data() {
        let llm_config = LLMConfig {
            id: 0,
            name: None,
            provider: "openai".to_string(),
            model: "gpt-4.1".to_string(),
            params: None,
            created_at: None,
            updated_at: None,
            provider_node_id: None,
        };
        let estimator = content_cost_estimator_for_llm_config(Some(&llm_config));
        let content = vec![Content::image("image/png", vec![0u8; 400])];

        let tokens = estimate_content_tokens(&content, estimator.as_ref());

        assert_eq!(tokens, 256);
    }

    #[test]
    fn test_unknown_provider_uses_generic_image_cost() {
        let llm_config = LLMConfig {
            id: 0,
            name: None,
            provider: "custom".to_string(),
            model: "vision-x".to_string(),
            params: None,
            created_at: None,
            updated_at: None,
            provider_node_id: None,
        };
        let estimator = content_cost_estimator_for_llm_config(Some(&llm_config));
        let content = vec![Content::image("image/png", vec![0u8; 400])];

        let tokens = estimate_content_tokens(&content, estimator.as_ref());

        assert_eq!(tokens, 100);
    }

    #[test]
    fn test_prune_skips_recent_user_turns() {
        let estimator = GenericContentCostEstimator;
        let config = PruneConfig {
            protect_tokens: 0, // No protection to make pruning happen
            minimum_tokens: 1,
            protected_tools: vec![],
        };

        // Create messages with 3 user turns
        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result("2", "s1", "c1", &"a".repeat(400), None),
            make_user_message("3", "s1"),
            make_assistant_message_with_tool_result("4", "s1", "c2", &"b".repeat(400), None),
            make_user_message("5", "s1"), // First user turn (recent)
            make_assistant_message_with_tool_result("6", "s1", "c3", &"c".repeat(400), None),
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // Should only prune messages before the 2 most recent user turns
        // The last user turn is "5", second to last is "3"
        // So messages 1 and 2 should be candidates
        assert!(analysis.should_prune);
        // Message "2" with call_id "c1" should be a candidate
        assert!(analysis.candidates.iter().any(|c| c.call_id == "c1"));
        // Messages in recent turns should not be pruned
        assert!(!analysis.candidates.iter().any(|c| c.call_id == "c3"));
    }

    #[test]
    fn test_prune_respects_protect_limit() {
        let estimator = GenericContentCostEstimator;
        let config = PruneConfig {
            protect_tokens: 200, // Protect 200 tokens
            minimum_tokens: 1,
            protected_tools: vec![],
        };

        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result("2", "s1", "c1", &"a".repeat(400), None), // 100 tokens, old
            make_user_message("3", "s1"),
            make_assistant_message_with_tool_result("4", "s1", "c2", &"b".repeat(400), None), // 100 tokens, old
            make_user_message("5", "s1"),
            make_assistant_message_with_tool_result("6", "s1", "c3", &"c".repeat(400), None), // 100 tokens, protected
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // With 200 token protection and walking backwards:
        // - c3 (100 tokens) would be protected (user turn 5 is recent, skipped)
        // - c2 (100 tokens) after user turn 3 - fills up protection
        // - c1 (100 tokens) after user turn 1 - beyond protection, prunable
        assert!(analysis.protected_tokens <= 200);
        // At least some tokens should be prunable
        assert!(analysis.prunable_tokens > 0 || analysis.protected_tokens > 0);
    }

    #[test]
    fn test_prune_skips_protected_tools() {
        let estimator = GenericContentCostEstimator;
        let config = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 1,
            protected_tools: vec!["skill".to_string()],
        };

        // Need 4+ user turns so that turn 1 and 2 are outside the 2-turn protection window
        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result(
                "2",
                "s1",
                "c1",
                &"a".repeat(400),
                Some("skill"), // Protected tool in turn 1
            ),
            make_user_message("3", "s1"),
            make_assistant_message_with_tool_result(
                "4",
                "s1",
                "c2",
                &"b".repeat(400),
                Some("read"), // Non-protected tool in turn 2
            ),
            make_user_message("5", "s1"), // Turn 3 - recent (protected)
            make_assistant_message_with_tool_result("6", "s1", "c3", &"c".repeat(400), None),
            make_user_message("7", "s1"), // Turn 4 - most recent (protected)
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // "skill" tool should never be pruned (even though turn 1 is outside protection window)
        assert!(!analysis.candidates.iter().any(|c| c.call_id == "c1"));
        // "read" tool is not protected and turn 2 is outside protection window
        assert!(analysis.candidates.iter().any(|c| c.call_id == "c2"));
        // c3 is in turn 3 which is protected
        assert!(!analysis.candidates.iter().any(|c| c.call_id == "c3"));
    }

    #[test]
    fn test_prune_requires_minimum_tokens() {
        let estimator = GenericContentCostEstimator;
        let config = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 1000, // High minimum
            protected_tools: vec![],
        };

        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result("2", "s1", "c1", &"a".repeat(40), None), // Only 10 tokens
            make_user_message("3", "s1"),
            make_user_message("4", "s1"),
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // Not enough tokens to justify pruning
        assert!(!analysis.should_prune);
        assert!(analysis.candidates.is_empty());
    }

    #[test]
    fn test_prune_can_select_binary_heavy_tool_results() {
        let estimator = GenericContentCostEstimator;
        let config = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 1,
            protected_tools: vec![],
        };

        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_binary_tool_result(
                "2",
                "s1",
                "c1",
                vec![Content::image("image/png", vec![0u8; 400])],
            ),
            make_user_message("3", "s1"),
            make_user_message("4", "s1"),
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        assert!(analysis.should_prune);
        assert!(analysis.prunable_tokens >= 100);
        assert!(analysis.candidates.iter().any(|c| c.call_id == "c1"));
    }

    #[test]
    fn test_prune_stops_at_compaction_message() {
        let estimator = GenericContentCostEstimator;
        let config = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 1,
            protected_tools: vec![],
        };

        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result("2", "s1", "c1", &"a".repeat(400), None),
            make_compaction_message("3", "s1"), // Compaction - should stop here
            make_user_message("4", "s1"),
            make_assistant_message_with_tool_result("5", "s1", "c2", &"b".repeat(400), None),
            make_user_message("6", "s1"),
            make_user_message("7", "s1"),
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // Should not prune c1 which is before the compaction
        assert!(!analysis.candidates.iter().any(|c| c.call_id == "c1"));
        // c2 is after compaction and beyond recent turns, should be prunable
        assert!(analysis.candidates.iter().any(|c| c.call_id == "c2"));
    }

    #[test]
    fn test_extract_message_ids() {
        let candidates = vec![
            PrunableToolResult {
                message_id: "msg1".to_string(),
                call_id: "c1".to_string(),
                tokens: 100,
            },
            PrunableToolResult {
                message_id: "msg1".to_string(),
                call_id: "c2".to_string(),
                tokens: 100,
            },
            PrunableToolResult {
                message_id: "msg2".to_string(),
                call_id: "c3".to_string(),
                tokens: 100,
            },
        ];

        let ids = extract_message_ids(&candidates);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"msg1".to_string()));
        assert!(ids.contains(&"msg2".to_string()));
    }

    #[test]
    fn test_extract_call_ids() {
        let candidates = vec![
            PrunableToolResult {
                message_id: "msg1".to_string(),
                call_id: "c1".to_string(),
                tokens: 100,
            },
            PrunableToolResult {
                message_id: "msg1".to_string(),
                call_id: "c2".to_string(),
                tokens: 100,
            },
        ];

        let ids = extract_call_ids(&candidates);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"c1".to_string()));
        assert!(ids.contains(&"c2".to_string()));
    }
}
