use querymt::LLMProvider;
use querymt::chat::{ChatMessageBuilder, ChatProvider, ChatRole};
use querymt::completion::CompletionRequest;
use querymt::error::LLMError;

use crate::config::MistralRSConfig;
use crate::factory::MistralRSFactory;

fn get_provider() -> Box<dyn LLMProvider> {
    let factory = MistralRSFactory {};
    let cfg = MistralRSConfig {
        model: "microsoft/Phi-3.5-mini-instruct".to_string(),
        model_kind: None,
        tools: None,
        tool_choice: None,
        tok_model_id: None,
        gguf_files: None,
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
        paged_attn: None,
        paged_attn_block_size: None,
        paged_attn_gpu_mem: None,
        paged_attn_gpu_mem_usage: None,
        paged_attn_context_len: None,
        paged_attn_cache_type: None,
    };

    let json_cfg = serde_json::to_string(&cfg).unwrap();
    factory.from_config(&json_cfg).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn mrs_chat_integration_test() {
    let provider = get_provider();
    let messages = vec![
        ChatMessageBuilder::new(ChatRole::User)
            .content("Hello?")
            .build(),
    ];

    let _resp = provider.chat(&messages).await.unwrap();
}

#[tokio::test]
async fn embedding_provider_requires_embedding_model() {
    let provider = get_provider();
    let err = provider.embed(vec!["foo".into()]).await.unwrap_err();
    assert!(matches!(err, LLMError::InvalidRequest(_)));
}

#[tokio::test]
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
