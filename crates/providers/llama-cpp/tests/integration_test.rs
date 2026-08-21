/// Integration tests for the refactored llama-cpp provider.
///
/// These tests verify that the modular structure compiles and the public API
/// remains compatible.
use qmt_llama_cpp::{LlamaCppConfig, SpeculativeConfig, SpeculativeType};
use schemars::schema_for;

#[test]
fn test_config_schema_generation() {
    // Verify that the config schema can be generated (tests serde/schemars integration)
    let schema = schema_for!(LlamaCppConfig);
    let schema = schema.as_value();
    assert!(schema["properties"]["speculative"].is_object());
    let schema_json = serde_json::to_string(schema).unwrap();
    assert!(schema_json.contains("mtp"));
    assert!(!schema_json.contains("draft-dflash"));
}

#[test]
fn test_config_serialization() {
    // Verify config can be serialized/deserialized
    let config = LlamaCppConfig {
        model: "/path/to/model.gguf".to_string(),
        max_tokens: Some(512),
        temperature: Some(0.7),
        top_p: Some(0.9),
        min_p: Some(0.0),
        top_k: Some(40),
        repeat_penalty: None,
        presence_penalty: None,
        frequency_penalty: None,
        penalty_last_n: None,
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
        enable_thinking: Some(true),
        reasoning_effort: Some(querymt::chat::ReasoningEffort::High),
        flash_attention: None,
        kv_cache_type_k: Some("q4_0".to_string()),
        kv_cache_type_v: Some("q4_0".to_string()),
        mmproj_path: Some("/path/to/mmproj.gguf".to_string()),
        media_marker: Some("<__media__>".to_string()),
        mmproj_threads: Some(4),
        mmproj_use_gpu: Some(true),
        n_ubatch: Some(4096),
        text_only: None,
        speculative: None,
        backend_sampling: None,
        json_schema: None,
    };

    let json = serde_json::to_string(&config).expect("Failed to serialize config");
    let deserialized: LlamaCppConfig =
        serde_json::from_str(&json).expect("Failed to deserialize config");

    assert_eq!(deserialized.model, "/path/to/model.gguf");
    assert_eq!(deserialized.max_tokens, Some(512));
    assert_eq!(
        deserialized.reasoning_effort,
        Some(querymt::chat::ReasoningEffort::High)
    );
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
fn test_speculative_config_serialization() {
    let speculative = SpeculativeConfig {
        kind: SpeculativeType::Mtp,
        model: None,
        n_max: Some(3),
        n_min: Some(0),
        p_min: Some(0.0),
        n_gpu_layers: None,
    };

    let json = serde_json::to_value(&speculative).unwrap();
    assert_eq!(json["type"], "mtp");
    assert_eq!(json["n_max"], 3);

    let parsed: SpeculativeConfig = serde_json::from_value(serde_json::json!({
        "type": "mtp",
        "n_max": 7
    }))
    .unwrap();
    assert_eq!(parsed.kind, SpeculativeType::Mtp);
    assert_eq!(parsed.n_max, Some(7));
}

#[test]
fn test_speculative_config_requires_supported_type() {
    assert!(serde_json::from_value::<SpeculativeConfig>(serde_json::json!({"n_max": 3})).is_err());
    assert!(
        serde_json::from_value::<SpeculativeConfig>(serde_json::json!({
            "type": "draft-dflash",
            "model": "draft.gguf"
        }))
        .is_err()
    );
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
