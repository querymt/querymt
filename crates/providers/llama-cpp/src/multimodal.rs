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
use querymt::chat::{ChatMessage, ImageMime, MessageType};
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
    pub mime: ImageMime,
    pub is_audio: bool,
}

impl MediaAttachment {
    /// Validate that the image data matches the claimed MIME type.
    fn validate_mime(&self) -> Result<(), LLMError> {
        let is_valid = match self.mime {
            ImageMime::JPEG => infer::image::is_jpeg(&self.data),
            ImageMime::PNG => infer::image::is_png(&self.data),
            ImageMime::GIF => infer::image::is_gif(&self.data),
            ImageMime::WEBP => infer::image::is_webp(&self.data),
            // For any other MIME types (enum is non-exhaustive), skip validation
            _ => return Ok(()),
        };

        if is_valid {
            Ok(())
        } else {
            // Try to detect actual type for better error message
            let actual = infer::get(&self.data)
                .map(|t| t.mime_type().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            Err(LLMError::ProviderError(format!(
                "Image data does not match MIME type {:?}. Detected: {}",
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
        match &msg.message_type {
            MessageType::Image((mime, data)) => {
                attachments.push(MediaAttachment {
                    data: data.clone(),
                    mime: mime.clone(),
                    is_audio: false,
                });
            }
            MessageType::ImageURL(_url) => {
                // Future: fetch from URL
                // For now, skip or error in conversion
                log::warn!("ImageURL not yet supported, skipping");
            }
            _ => {}
        }
    }

    attachments
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::ChatRole;

    #[test]
    fn test_extract_media_no_images() {
        let messages = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::Text,
            content: "Hello".to_string(),
            thinking: None,
            cache: None,
        }];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 0);
    }

    #[test]
    fn test_extract_media_with_images() {
        let messages = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::Image((ImageMime::JPEG, vec![0xFF, 0xD8, 0xFF])),
            content: "Describe this".to_string(),
            thinking: None,
            cache: None,
        }];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 1);
        assert!(!media[0].is_audio);
        assert!(matches!(media[0].mime, ImageMime::JPEG));
    }

    #[test]
    fn test_extract_media_multiple_images() {
        let messages = vec![
            ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Image((ImageMime::JPEG, vec![0xFF, 0xD8, 0xFF])),
                content: "First image".to_string(),
                thinking: None,
                cache: None,
            },
            ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Text,
                content: "Some text".to_string(),
                thinking: None,
                cache: None,
            },
            ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Image((ImageMime::PNG, vec![0x89, 0x50, 0x4E, 0x47])),
                content: "Second image".to_string(),
                thinking: None,
                cache: None,
            },
        ];

        let media = extract_media(&messages);
        assert_eq!(media.len(), 2);
        assert!(matches!(media[0].mime, ImageMime::JPEG));
        assert!(matches!(media[1].mime, ImageMime::PNG));
    }
}
