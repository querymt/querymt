/// Integration tests for the refactored llama-cpp provider.
///
/// These tests verify that the modular structure compiles and the public API
/// remains compatible.
use qmt_llama_cpp::LlamaCppConfig;
use schemars::schema_for;

#[test]
fn test_config_schema_generation() {
    // Verify that the config schema can be generated (tests serde/schemars integration)
    let schema = schema_for!(LlamaCppConfig);
    assert!(schema.schema.object.is_some());
}

#[test]
fn test_config_serialization() {
    // Verify config can be serialized/deserialized
    let config = LlamaCppConfig {
        model: "/path/to/model.gguf".to_string(),
        max_tokens: Some(512),
        temperature: Some(0.7),
        top_p: Some(0.9),
        top_k: Some(40),
        system: vec!["System prompt".to_string()],
        n_ctx: Some(2048),
        n_batch: Some(512),
        n_threads: Some(4),
        n_threads_batch: Some(4),
        n_gpu_layers: Some(33),
        seed: Some(42),
        chat_template: None,
        use_chat_template: Some(true),
        add_bos: Some(true),
        log: None,
        fast_download: Some(false),
        enable_thinking: Some(true),
        flash_attention: None,
        kv_cache_type_k: Some("q4_0".to_string()),
        kv_cache_type_v: Some("q4_0".to_string()),
        mmproj_path: Some("/path/to/mmproj.gguf".to_string()),
        media_marker: Some("<__media__>".to_string()),
        mmproj_threads: Some(4),
        mmproj_use_gpu: Some(true),
        n_ubatch: Some(4096),
    };

    let json = serde_json::to_string(&config).expect("Failed to serialize config");
    let deserialized: LlamaCppConfig =
        serde_json::from_str(&json).expect("Failed to deserialize config");

    assert_eq!(deserialized.model, "/path/to/model.gguf");
    assert_eq!(deserialized.max_tokens, Some(512));
    assert_eq!(deserialized.kv_cache_type_k, Some("q4_0".to_string()));
    assert_eq!(
        deserialized.mmproj_path,
        Some("/path/to/mmproj.gguf".to_string())
    );
    assert_eq!(deserialized.media_marker, Some("<__media__>".to_string()));
    assert_eq!(deserialized.mmproj_threads, Some(4));
    assert_eq!(deserialized.mmproj_use_gpu, Some(true));
    assert_eq!(deserialized.n_ubatch, Some(4096));
}

#[test]
fn test_module_structure() {
    // This test simply verifies that the modules are properly organized
    // and can be imported. If this compiles, the module structure is correct.

    // The fact that we can use LlamaCppConfig proves:
    // - config module exports are correct
    // - lib.rs re-exports work
    // - serde derives work across modules

    let _: Option<LlamaCppConfig> = None;
}
