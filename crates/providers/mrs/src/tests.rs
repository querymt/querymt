use querymt::LLMProvider;
use querymt::chat::{ChatMessageBuilder, ChatRole};
use querymt::completion::CompletionRequest;
use querymt::error::LLMError;

use crate::config::{MistralRSConfig, MistralRSMtpConfig};
use crate::factory::create_factory;
use crate::model::{model_cache_key, mtp_config, paged_attn_config};

fn test_config() -> MistralRSConfig {
    MistralRSConfig {
        model: "microsoft/Phi-3.5-mini-instruct".to_string(),
        model_kind: None,
        tools: None,
        tool_choice: None,
        tok_model_id: None,
        hf_revision: None,
        token_source: None,
        chat_template: None,
        tokenizer_json: None,
        jinja_explicit: None,
        hf_cache_path: None,
        loader_type: None,
        dtype: None,
        topology: None,
        isq: None,
        imatrix: None,
        calibration_file: None,
        max_edge: None,
        force_cpu: None,
        device_map: None,
        max_num_seqs: None,
        no_kv_cache: None,
        prefix_cache_n: None,
        throughput_logging: None,
        mtp: None,
        paged_attn: None,
        paged_attn_block_size: None,
        paged_attn_gpu_mem: None,
        paged_attn_gpu_mem_usage: None,
        paged_attn_context_len: None,
        paged_attn_cache_type: None,
        speech_loader_type: None,
        speech_dac_model_id: None,
    }
}

fn get_provider() -> Box<dyn LLMProvider> {
    let factory = create_factory();
    let json_cfg = serde_json::to_string(&test_config()).unwrap();
    factory.from_config(&json_cfg).unwrap()
}

#[test]
fn mtp_config_supports_builtin_and_external_models() {
    let mut cfg = test_config();
    cfg.mtp = Some(MistralRSMtpConfig {
        model: None,
        n_predict: Some(3),
    });
    let builtin = mtp_config(&cfg).unwrap().unwrap();
    assert!(builtin.is_builtin());
    assert_eq!(builtin.n_predict, Some(3));

    cfg.mtp = Some(MistralRSMtpConfig {
        model: Some("incoai/Qwen3.8-27B-DFlash2".into()),
        n_predict: None,
    });
    let external = mtp_config(&cfg).unwrap().unwrap();
    assert!(!external.is_builtin());
    assert_eq!(
        external.model.as_deref(),
        Some("incoai/Qwen3.8-27B-DFlash2")
    );
    assert_eq!(external.n_predict, None);
}

#[test]
fn mtp_config_enables_paged_attention_and_changes_cache_identity() {
    let base = test_config();
    let mut mtp = test_config();
    mtp.mtp = Some(MistralRSMtpConfig {
        model: None,
        n_predict: Some(3),
    });

    assert!(paged_attn_config(&base).unwrap().is_none());
    assert!(paged_attn_config(&mtp).unwrap().is_some());
    assert_ne!(
        model_cache_key(&base).unwrap(),
        model_cache_key(&mtp).unwrap()
    );
}

#[test]
fn mtp_rejects_invalid_or_incompatible_config() {
    let mut cfg = test_config();
    cfg.mtp = Some(MistralRSMtpConfig {
        model: None,
        n_predict: Some(0),
    });
    assert!(matches!(mtp_config(&cfg), Err(LLMError::InvalidRequest(_))));

    cfg.mtp = Some(MistralRSMtpConfig {
        model: Some("  ".into()),
        n_predict: None,
    });
    assert!(matches!(mtp_config(&cfg), Err(LLMError::InvalidRequest(_))));

    cfg.mtp = Some(MistralRSMtpConfig {
        model: None,
        n_predict: None,
    });
    cfg.paged_attn = Some(false);
    assert!(matches!(
        paged_attn_config(&cfg),
        Err(LLMError::InvalidRequest(_))
    ));

    cfg.paged_attn = None;
    cfg.no_kv_cache = Some(true);
    assert!(matches!(
        paged_attn_config(&cfg),
        Err(LLMError::InvalidRequest(_))
    ));
}

#[test]
fn mtp_config_rejects_unknown_fields() {
    let mut value = serde_json::to_value(test_config()).unwrap();
    value["mtp"] = serde_json::json!({"n_max": 3});

    let error = match serde_json::from_value::<MistralRSConfig>(value) {
        Ok(_) => panic!("unknown MTP fields should be rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unknown field `n_max`"));
}

#[test]
fn request_options_do_not_change_cache_identity() {
    let mut configured = test_config();
    configured.tool_choice = Some(querymt::chat::ToolChoice::Auto);

    assert_eq!(
        model_cache_key(&test_config()).unwrap(),
        model_cache_key(&configured).unwrap()
    );
}

#[test]
fn malformed_config_is_non_retryable_json_error() {
    let error = create_factory()
        .from_config("{")
        .err()
        .expect("config should be rejected");

    assert!(!error.is_retryable());
    assert!(matches!(error, LLMError::JsonError(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn mrs_chat_integration_test() {
    let provider = get_provider();
    let messages = vec![
        ChatMessageBuilder::new(ChatRole::User)
            .text("Hello?")
            .build(),
    ];

    let _resp = provider.chat(&messages).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn embedding_provider_requires_embedding_model() {
    let provider = get_provider();
    let err = provider.embed(vec!["foo".into()]).await.unwrap_err();
    assert!(matches!(err, LLMError::InvalidRequest(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn completion_provider_is_currently_unimplemented() {
    let provider = get_provider();
    let dummy_req = CompletionRequest {
        prompt: "test".into(),
        max_tokens: None,
        temperature: None,
        suffix: None,
    };
    let err = provider.complete(&dummy_req).await.unwrap_err();
    assert!(matches!(err, LLMError::NotImplemented(_)));
}
