//! Integration tests for multimodal (vision) support.
//!
//! These tests require actual vision models and are skipped unless
//! environment variables are set:
//!
//! - `TEST_VL_MODEL`: Model reference (local path or hf:<repo>:<file> or <repo>:<quant>)
//! - `TEST_MMPROJ_PATH`: Path or hf: ref for mmproj file (optional — tests auto-discovery)
//! - `TEST_IMAGE_PATH`: Path to a JPEG or PNG test image
//!
//! # Running Tests
//!
//! ```bash
//! # With explicit mmproj:
//! TEST_VL_MODEL="unsloth/Qwen3-VL-8B-Instruct-GGUF:UD-Q6_K_XL" \
//! TEST_MMPROJ_PATH="hf:unsloth/Qwen3-VL-8B-Instruct-GGUF:mmproj-F16.gguf" \
//! TEST_IMAGE_PATH=path/to/test.jpg \
//! cargo test --package qmt-llama-cpp --test multimodal_test -- --nocapture
//!
//! # With auto-discovery (no TEST_MMPROJ_PATH):
//! TEST_VL_MODEL="unsloth/Qwen3-VL-8B-Instruct-GGUF:UD-Q6_K_XL" \
//! TEST_IMAGE_PATH=path/to/test.jpg \
//! cargo test --package qmt-llama-cpp --test multimodal_test -- --nocapture
//! ```

use qmt_llama_cpp::{LlamaCppConfig, create_provider};
use querymt::chat::{ChatMessage, ChatRole, ImageMime, MessageType};
use std::env;

/// Returns (model, mmproj_path_opt, image_path) or None to skip.
fn test_env() -> Option<(String, Option<String>, String)> {
    let model = env::var("TEST_VL_MODEL").ok()?;
    let image = env::var("TEST_IMAGE_PATH").ok()?;
    let mmproj = env::var("TEST_MMPROJ_PATH").ok(); // optional
    Some((model, mmproj, image))
}

const SKIP_MSG: &str = "Skipping — set TEST_VL_MODEL and TEST_IMAGE_PATH to run";

fn make_provider(model: String, mmproj_path: Option<String>) -> Box<dyn querymt::LLMProvider> {
    let cfg = LlamaCppConfig {
        model,
        mmproj_path,
        n_ctx: Some(4096),
        n_gpu_layers: Some(0),
        max_tokens: Some(100),
        temperature: None,
        top_p: None,
        min_p: None,
        top_k: None,
        system: vec![],
        n_batch: None,
        n_threads: None,
        n_threads_batch: None,
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
        media_marker: None,
        mmproj_threads: None,
        mmproj_use_gpu: None,
        n_ubatch: None,
        text_only: None,
    };
    create_provider(cfg).expect("Failed to create provider")
}

#[tokio::test]
async fn test_vision_chat_basic() {
    let Some((model, mmproj_path, image_path)) = test_env() else {
        println!("{}", SKIP_MSG);
        return;
    };

    let provider = make_provider(model, mmproj_path);
    let image_data = std::fs::read(&image_path).expect("Failed to read test image");

    let messages = vec![ChatMessage {
        role: ChatRole::User,
        message_type: MessageType::Image((ImageMime::JPEG, image_data)),
        content: "What's in this image?".to_string(),
        thinking: None,
        cache: None,
    }];

    let response = provider.chat(&messages).await.expect("Chat failed");
    let text = response.text().unwrap_or_default();
    let usage = response.usage().unwrap_or_default();

    assert!(!text.is_empty(), "Response should not be empty");
    assert!(text.len() > 10, "Response should be substantial");
    assert!(usage.input_tokens > 0, "Should have input tokens");
    assert!(usage.output_tokens > 0, "Should have output tokens");

    println!("Response length: {} chars", text.len());
    println!("Input tokens: {}", usage.input_tokens);
    println!("Output tokens: {}", usage.output_tokens);
}

#[tokio::test]
async fn test_vision_streaming() {
    let Some((model, mmproj_path, image_path)) = test_env() else {
        println!("{}", SKIP_MSG);
        return;
    };

    use futures::StreamExt;

    let provider = make_provider(model, mmproj_path);
    let image_data = std::fs::read(&image_path).expect("Failed to read test image");

    let messages = vec![ChatMessage {
        role: ChatRole::User,
        message_type: MessageType::Image((ImageMime::JPEG, image_data)),
        content: "Describe this image briefly.".to_string(),
        thinking: None,
        cache: None,
    }];

    let mut stream = provider
        .chat_stream(&messages)
        .await
        .expect("stream failed");

    let mut text_chunks: Vec<String> = Vec::new();
    let mut got_usage = false;
    let mut got_done = false;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(querymt::chat::StreamChunk::Text(t)) => text_chunks.push(t),
            Ok(querymt::chat::StreamChunk::Usage(u)) => {
                assert!(u.input_tokens > 0);
                assert!(u.output_tokens > 0);
                got_usage = true;
            }
            Ok(querymt::chat::StreamChunk::Done { .. }) => got_done = true,
            Err(e) => panic!("Stream error: {}", e),
            _ => {}
        }
    }

    let full_text = text_chunks.join("");
    assert!(!full_text.is_empty(), "Should receive text chunks");
    assert!(got_usage, "Should receive usage");
    assert!(got_done, "Should receive done signal");

    println!(
        "Chunks: {}, total: {} chars",
        text_chunks.len(),
        full_text.len()
    );
}

#[tokio::test]
async fn test_text_only_with_vision_model() {
    let Some((model, mmproj_path, _)) = test_env() else {
        println!("{}", SKIP_MSG);
        return;
    };

    let provider = make_provider(model, mmproj_path);

    let messages = vec![ChatMessage {
        role: ChatRole::User,
        message_type: MessageType::Text,
        content: "What is 2+2?".to_string(),
        thinking: None,
        cache: None,
    }];

    let response = provider.chat(&messages).await.expect("Chat failed");
    let text = response.text().unwrap_or_default();

    assert!(!text.is_empty(), "Should get text response");
    assert!(
        response.usage().map_or(false, |u| u.output_tokens > 0),
        "Should generate tokens"
    );

    println!("Response: {}", &text[..text.len().min(100)]);
}

#[test]
fn test_config_with_multimodal_fields() {
    let config = LlamaCppConfig {
        model: "/path/to/model.gguf".to_string(),
        mmproj_path: Some("/path/to/mmproj.gguf".to_string()),
        media_marker: Some("<start_of_image>".to_string()),
        mmproj_threads: Some(8),
        mmproj_use_gpu: Some(true),
        n_ctx: Some(8192),
        n_gpu_layers: Some(33),
        max_tokens: Some(512),
        temperature: None,
        top_p: None,
        min_p: None,
        top_k: None,
        system: vec![],
        n_batch: None,
        n_threads: None,
        n_threads_batch: None,
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
        n_ubatch: None,
        text_only: None,
    };

    let json = serde_json::to_string(&config).expect("serialize");
    let back: LlamaCppConfig = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.mmproj_path.as_deref(), Some("/path/to/mmproj.gguf"));
    assert_eq!(back.media_marker.as_deref(), Some("<start_of_image>"));
    assert_eq!(back.mmproj_threads, Some(8));
    assert_eq!(back.mmproj_use_gpu, Some(true));
}
