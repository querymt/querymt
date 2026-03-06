//! Multimodal support for llama.cpp provider.
//!
//! This module provides utilities for handling vision and audio models via llama.cpp's
//! MTMD (multimodal) API. It supports:
//! - Explicit mmproj loading via `mmproj_path` config
//! - Auto-discovery of mmproj files from Hugging Face repos
//! - Auto-detection of built-in vision models via rope type heuristic
//! - Image extraction and conversion from ChatMessages
//! - Bitmap management for MTMD API

use crate::config::LlamaCppConfig;
use llama_cpp_2::model::{LlamaModel, RopeType};
use llama_cpp_2::mtmd::{MtmdBitmap, MtmdContext, MtmdContextParams, mtmd_default_marker};
use querymt::chat::{ChatMessage, Content};
use querymt::error::LLMError;
use querymt_provider_common::{
    ModelRef, ModelRefError, parse_model_ref, resolve_hf_model_fast, resolve_hf_model_sync,
};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Wrapper around MtmdContext with provider-specific utilities.
#[derive(Clone)]
pub(crate) struct MultimodalContext {
    pub(crate) ctx: Arc<MtmdContext>,
    pub(crate) marker: String,
}

impl MultimodalContext {
    /// Initialize multimodal context from config.
    ///
    /// Resolution strategy (first match wins):
    /// 1. Explicit `mmproj_path` in config
    /// 2. Auto-discover mmproj from the Hugging Face repo the model was loaded from
    /// 3. Rope-type heuristic (Vision/MRope) — logs a warning, still requires mmproj
    /// 4. No multimodal support
    pub fn new(
        model: &LlamaModel,
        cfg: &LlamaCppConfig,
        model_hf_repo: Option<&str>,
    ) -> Result<Option<Self>, LLMError> {
        if cfg.text_only.unwrap_or(false) {
            log::info!("text_only mode: skipping multimodal projection loading");
            return Ok(None);
        }

        let marker = cfg
            .media_marker
            .clone()
            .unwrap_or_else(|| mtmd_default_marker().to_string());

        if let Some(ctx) = Self::load_projection(model, cfg, model_hf_repo)? {
            Ok(Some(Self {
                ctx: Arc::new(ctx),
                marker,
            }))
        } else {
            Ok(None)
        }
    }

    /// Check if model likely has built-in vision support via rope type heuristic.
    ///
    /// Models with Vision or MRope rope types often have multimodal projectors
    /// (e.g., Llama 3.2 Vision, Qwen2-VL). This is an informational heuristic —
    /// even these models require a separate mmproj file for the MTMD API.
    fn detect_builtin_vision(model: &LlamaModel) -> bool {
        matches!(
            model.rope_type(),
            Some(RopeType::Vision) | Some(RopeType::MRope)
        )
    }

    /// Load MTMD projection, trying each strategy in priority order.
    fn load_projection(
        model: &LlamaModel,
        cfg: &LlamaCppConfig,
        model_hf_repo: Option<&str>,
    ) -> Result<Option<MtmdContext>, LLMError> {
        let fast = cfg.fast_download.unwrap_or(false);

        // 1. Explicit mmproj_path takes precedence
        if let Some(ref path_str) = cfg.mmproj_path {
            let path = Self::resolve_mmproj_path(path_str, fast)?;
            let ctx = Self::init_mtmd_from_path(&path, model, cfg)?;
            log::info!(
                "Multimodal projection loaded: vision={}, audio={}",
                ctx.support_vision(),
                ctx.support_audio()
            );
            return Ok(Some(ctx));
        }

        // 2. Auto-discover mmproj from the HF repo the model came from
        if let Some(repo) = model_hf_repo {
            match querymt_provider_common::discover_mmproj_in_hf_repo(repo) {
                Ok(Some(mmproj_filename)) => {
                    log::info!(
                        "Auto-discovered multimodal projection in {}: {}",
                        repo,
                        mmproj_filename
                    );
                    log::info!("Downloading {} from {}...", mmproj_filename, repo);
                    match querymt_provider_common::resolve_hf_mmproj(repo, &mmproj_filename, fast)
                        .map_err(Self::map_model_ref_error)
                    {
                        Ok(path) => {
                            let ctx = Self::init_mtmd_from_path(&path, model, cfg)?;
                            log::info!(
                                "Multimodal projection loaded: vision={}, audio={}",
                                ctx.support_vision(),
                                ctx.support_audio()
                            );
                            return Ok(Some(ctx));
                        }
                        Err(e) => {
                            // Non-fatal: log and fall through to other strategies
                            log::warn!(
                                "Failed to download auto-discovered mmproj '{}' from {}: {}. \
                                 Set mmproj_path explicitly to enable vision.",
                                mmproj_filename,
                                repo,
                                e
                            );
                        }
                    }
                }
                Ok(None) => {
                    log::debug!("No mmproj files found in HF repo {}", repo);
                }
                Err(e) => {
                    // Non-fatal: offline, auth issues, etc.
                    log::debug!("Could not query HF repo {} for mmproj files: {}", repo, e);
                }
            }
        }

        // 3. Rope-type heuristic — model architecture suggests vision capability
        if Self::detect_builtin_vision(model) {
            log::warn!(
                "Model rope type ({:?}) suggests vision capability but no mmproj file was found. \
                 Set mmproj_path in config to enable multimodal support.",
                model.rope_type()
            );
            return Ok(None);
        }

        // 4. No multimodal support detected
        log::debug!(
            "No multimodal support detected for this model (rope_type={:?})",
            model.rope_type()
        );
        Ok(None)
    }

    /// Build MtmdContextParams from config and initialize an MtmdContext from a local path.
    fn init_mtmd_from_path(
        path: &Path,
        model: &LlamaModel,
        cfg: &LlamaCppConfig,
    ) -> Result<MtmdContext, LLMError> {
        let media_marker_cstr =
            CString::new(cfg.media_marker.as_deref().unwrap_or(mtmd_default_marker()))
                .map_err(|e| LLMError::InvalidRequest(format!("Invalid media marker: {}", e)))?;

        let use_gpu = cfg
            .mmproj_use_gpu
            // When n_gpu_layers is None the model backend defaults to full GPU offload
            // (e.g. Metal on Apple Silicon), so the mmproj should follow suit.
            .unwrap_or_else(|| cfg.n_gpu_layers.map_or(true, |n| n > 0));

        let params = MtmdContextParams {
            use_gpu,
            print_timings: false,
            n_threads: cfg.mmproj_threads.unwrap_or(cfg.n_threads.unwrap_or(4)),
            media_marker: media_marker_cstr,
        };

        log::info!(
            "Loading multimodal projection from: {} (use_gpu={})",
            path.display(),
            use_gpu
        );
        MtmdContext::init_from_file(&path.to_string_lossy(), model, &params)
            .map_err(|e| LLMError::ProviderError(format!("Failed to load mmproj: {}", e)))
    }

    /// Resolve an explicit mmproj_path value (local path or hf: ref) to a local PathBuf.
    fn resolve_mmproj_path(raw: &str, fast: bool) -> Result<PathBuf, LLMError> {
        let model_ref = parse_model_ref(raw).map_err(Self::map_model_ref_error)?;
        match model_ref {
            ModelRef::LocalPath(path) => {
                if !path.exists() {
                    return Err(LLMError::InvalidRequest(format!(
                        "Multimodal projection file does not exist: {}",
                        path.display()
                    )));
                }
                Ok(path)
            }
            ModelRef::Hf(hf_ref) => {
                if fast {
                    resolve_hf_model_fast(&hf_ref).map_err(Self::map_model_ref_error)
                } else {
                    resolve_hf_model_sync(&hf_ref).map_err(Self::map_model_ref_error)
                }
            }
            ModelRef::HfRepo(repo) => Err(LLMError::InvalidRequest(format!(
                "mmproj_path must include a file selector for Hugging Face repos: {}:<filename.gguf>",
                repo
            ))),
        }
    }

    fn map_model_ref_error(err: ModelRefError) -> LLMError {
        match err {
            ModelRefError::Invalid(msg) => LLMError::InvalidRequest(msg),
            ModelRefError::Download(msg) => LLMError::ProviderError(msg),
        }
    }

    /// Get the media marker string for this context.
    pub fn marker(&self) -> &str {
        &self.marker
    }
}

/// Represents an image or audio attachment extracted from messages.
pub(crate) struct MediaAttachment {
    pub data: Vec<u8>,
    pub mime: String,
    pub is_audio: bool,
}

impl MediaAttachment {
    /// Validate that the image data matches the claimed MIME type.
    fn validate_mime(&self) -> Result<(), LLMError> {
        let is_valid = match self.mime.as_str() {
            "image/jpeg" => infer::image::is_jpeg(&self.data),
            "image/png" => infer::image::is_png(&self.data),
            "image/gif" => infer::image::is_gif(&self.data),
            "image/webp" => infer::image::is_webp(&self.data),
            // For unknown/other MIME types, skip strict validation.
            _ => return Ok(()),
        };

        if is_valid {
            Ok(())
        } else {
            let actual = infer::get(&self.data)
                .map(|t| t.mime_type().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            Err(LLMError::ProviderError(format!(
                "Image data does not match MIME type {}. Detected: {}",
                self.mime, actual
            )))
        }
    }

    /// Create a bitmap from this attachment.
    pub fn to_bitmap(&self, ctx: &MtmdContext) -> Result<MtmdBitmap, LLMError> {
        if self.is_audio {
            // Audio support - future enhancement
            return Err(LLMError::NotImplemented(
                "Audio input not yet supported".into(),
            ));
        }

        // Validate MIME type matches actual image data
        self.validate_mime()?;

        // Create bitmap from image buffer
        MtmdBitmap::from_buffer(ctx, &self.data)
            .map_err(|e| LLMError::ProviderError(format!("Failed to create image bitmap: {}", e)))
    }
}

/// Extract media attachments from messages.
/// Returns Vec of attachments in the order they appear in messages.
pub(crate) fn extract_media(messages: &[ChatMessage]) -> Vec<MediaAttachment> {
    let mut attachments = Vec::new();

    for msg in messages {
        for block in &msg.content {
            match block {
                Content::Image { mime_type, data } => {
                    attachments.push(MediaAttachment {
                        data: data.clone(),
                        mime: mime_type.clone(),
                        is_audio: false,
                    });
                }
                Content::ImageUrl { .. } => {
                    // Future: fetch from URL
                    log::warn!("ImageURL not yet supported, skipping");
                }
                Content::ToolResult { content, .. } => {
                    // Also extract images nested in tool results.
                    for inner in content {
                        if let Content::Image { mime_type, data } = inner {
                            attachments.push(MediaAttachment {
                                data: data.clone(),
                                mime: mime_type.clone(),
                                is_audio: false,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    attachments
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::ChatRole;

    fn user_msg(blocks: Vec<Content>) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: blocks,
            cache: None,
        }
    }

    #[test]
    fn extract_media_no_images() {
        let messages = vec![user_msg(vec![Content::text("Hello")])];
        let media = extract_media(&messages);
        assert_eq!(media.len(), 0);
    }

    #[test]
    fn extract_media_single_image() {
        let messages = vec![user_msg(vec![
            Content::image("image/jpeg", vec![0xFF, 0xD8, 0xFF]),
            Content::text("Describe this"),
        ])];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 1);
        assert!(!media[0].is_audio);
        assert_eq!(media[0].mime, "image/jpeg");
        assert_eq!(media[0].data, vec![0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn extract_media_multiple_images_across_messages() {
        let messages = vec![
            user_msg(vec![
                Content::image("image/jpeg", vec![0xFF, 0xD8, 0xFF]),
                Content::text("First image"),
            ]),
            user_msg(vec![Content::text("Some text")]),
            user_msg(vec![
                Content::image("image/png", vec![0x89, 0x50, 0x4E, 0x47]),
                Content::text("Second image"),
            ]),
        ];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].mime, "image/jpeg");
        assert_eq!(media[1].mime, "image/png");
    }

    #[test]
    fn extract_media_multiple_images_in_single_message() {
        let messages = vec![user_msg(vec![
            Content::image("image/png", vec![1]),
            Content::image("image/jpeg", vec![2]),
            Content::image("image/png", vec![3]),
            Content::text("Three images"),
        ])];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 3);
        assert_eq!(media[0].data, vec![1]);
        assert_eq!(media[1].data, vec![2]);
        assert_eq!(media[2].data, vec![3]);
    }

    #[test]
    fn extract_media_skips_image_url() {
        let messages = vec![user_msg(vec![
            Content::image_url("https://example.com/photo.jpg"),
            Content::text("Describe this"),
        ])];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 0);
    }

    #[test]
    fn extract_media_from_tool_result() {
        let messages = vec![user_msg(vec![Content::ToolResult {
            id: "call_1".to_string(),
            name: Some("photos".to_string()),
            is_error: false,
            content: vec![
                Content::text("metadata"),
                Content::image("image/png", vec![0x89, 0x50]),
            ],
        }])];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].mime, "image/png");
        assert_eq!(media[0].data, vec![0x89, 0x50]);
    }

    #[test]
    fn extract_media_tool_result_multiple_images() {
        let messages = vec![user_msg(vec![Content::ToolResult {
            id: "call_1".to_string(),
            name: Some("photos_search".to_string()),
            is_error: false,
            content: vec![
                Content::text("metadata"),
                Content::image("image/png", vec![1]),
                Content::image("image/jpeg", vec![2]),
            ],
        }])];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].data, vec![1]);
        assert_eq!(media[1].data, vec![2]);
    }

    #[test]
    fn extract_media_tool_result_skips_image_url() {
        let messages = vec![user_msg(vec![Content::ToolResult {
            id: "call_1".to_string(),
            name: Some("tool".to_string()),
            is_error: false,
            content: vec![
                Content::text("result"),
                Content::image_url("https://example.com/img.png"),
            ],
        }])];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 0);
    }

    /// Verify that extract_media and messages_to_json agree on media count.
    /// This is the critical invariant for MTMD tokenization.
    #[test]
    fn extract_media_count_matches_messages_to_json_count() {
        use crate::config::LlamaCppConfig;
        use crate::messages::messages_to_json;

        let cfg = LlamaCppConfig {
            model: "test.gguf".to_string(),
            system: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            min_p: None,
            top_k: None,
            n_ctx: None,
            n_batch: None,
            n_threads: None,
            n_threads_batch: None,
            n_gpu_layers: None,
            seed: None,
            chat_template: None,
            use_chat_template: None,
            add_bos: None,
            log: None,
            fast_download: None,
            enable_thinking: None,
            flash_attention: None,
            kv_cache_type_k: None,
            kv_cache_type_v: None,
            mmproj_path: None,
            media_marker: None,
            mmproj_threads: None,
            mmproj_use_gpu: None,
            n_ubatch: None,
            text_only: None,
            json_schema: None,
        };

        // Case: multiple top-level images + tool result with nested images
        let messages = vec![
            user_msg(vec![
                Content::image("image/png", vec![1]),
                Content::image("image/jpeg", vec![2]),
                Content::text("Two images above"),
            ]),
            user_msg(vec![Content::ToolResult {
                id: "call_1".to_string(),
                name: Some("photos".to_string()),
                is_error: false,
                content: vec![
                    Content::text("metadata"),
                    Content::image("image/png", vec![3]),
                    Content::image("image/png", vec![4]),
                ],
            }]),
            user_msg(vec![
                Content::image("image/png", vec![5]),
                Content::text("One more"),
                // ImageUrl should be skipped in both paths
                Content::image_url("https://example.com/skip.jpg"),
            ]),
        ];

        let extracted = extract_media(&messages);
        let (_, marker_count) = messages_to_json(&cfg, &messages, Some("<__media__>")).unwrap();

        assert_eq!(
            extracted.len(),
            marker_count,
            "extract_media count ({}) must equal messages_to_json marker count ({})",
            extracted.len(),
            marker_count
        );
        assert_eq!(extracted.len(), 5); // 2 top-level + 2 nested + 1 top-level
    }
}
