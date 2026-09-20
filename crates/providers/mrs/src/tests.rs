use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatMessageBuilder, ChatRole, Content};
use querymt::completion::CompletionRequest;
use querymt::error::LLMError;

use crate::config::{MistralRSConfig, MistralRSMtpConfig};
use crate::factory::create_factory;
use crate::messages::{PlanStep, plan_message};
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

// The tests below instantiate a real model via `get_provider()`; each load
// peaks at tens of GB on CPU/F32, so they must never run concurrently and are
// skipped by default to keep the default lib test memory-safe. Run them
// explicitly (and serially) with:
//   cargo test -p qmt-mrs --no-default-features --features native --lib -- --ignored --test-threads=1
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "loads the full model; heavy memory/CPU; run explicitly with --ignored"]
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
#[ignore = "loads the full model; heavy memory/CPU; run explicitly with --ignored"]
async fn embedding_provider_requires_embedding_model() {
    let provider = get_provider();
    let err = provider.embed(vec!["foo".into()]).await.unwrap_err();
    assert!(matches!(err, LLMError::InvalidRequest(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "loads the full model; heavy memory/CPU; run explicitly with --ignored"]
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

// --- Emission-plan regression tests (pure; no model instantiation) ---
// The pinned mistralrs RequestBuilder keeps media in private builder-level
// fields, so ordering/grouping correctness is asserted on the pure plan
// produced by `plan_message` instead of on a built request.

/// A minimal valid 1x1 PNG, generated with the `image` crate so decoding in
/// `plan_message` succeeds without hand-crafted binary data.
fn png_bytes() -> Vec<u8> {
    let mut buf = Vec::new();
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

fn msg(content: Vec<Content>) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content,
        cache: None,
    }
}

fn tool_use(id: &str) -> Content {
    Content::ToolUse {
        id: id.to_string(),
        name: "get_weather".to_string(),
        arguments: serde_json::json!({ "city": "Paris" }),
    }
}

fn tool_result(id: &str, content: Vec<Content>) -> Content {
    Content::ToolResult {
        id: id.to_string(),
        name: None,
        is_error: false,
        content,
    }
}

fn step_kind(step: &PlanStep) -> &'static str {
    match step {
        PlanStep::Text { .. } => "text",
        PlanStep::ToolOutput { .. } => "tool_output",
        PlanStep::ToolCalls { .. } => "tool_calls",
        PlanStep::Media { .. } => "media",
    }
}

#[test]
fn tool_result_media_stays_associated_in_source_order() {
    // Regression: two tool results each carrying media must emit
    // tool A, media A, tool B, media B — media must not be pooled into a
    // single trailing message-level vector.
    let message = msg(vec![
        tool_result(
            "call_a",
            vec![
                Content::text("result-a"),
                Content::image("image/png", png_bytes()),
                Content::image("image/png", png_bytes()),
            ],
        ),
        tool_result(
            "call_b",
            vec![
                Content::image("image/png", png_bytes()),
                Content::text("result-b"),
            ],
        ),
    ]);

    let plan = plan_message(&message).unwrap();

    let kinds: Vec<&str> = plan.steps.iter().map(step_kind).collect();
    assert_eq!(kinds, ["tool_output", "media", "tool_output", "media"]);

    match &plan.steps[0] {
        PlanStep::ToolOutput { id, text } => {
            assert_eq!(id, "call_a");
            assert_eq!(text, "result-a");
        }
        other => panic!("expected tool output, got {}", step_kind(other)),
    }
    match &plan.steps[1] {
        PlanStep::Media { images, text, .. } => {
            assert_eq!(images.len(), 2, "tool A's images must stay together");
            assert!(text.is_empty());
        }
        other => panic!("expected media step, got {}", step_kind(other)),
    }
    match &plan.steps[2] {
        PlanStep::ToolOutput { id, text } => {
            assert_eq!(id, "call_b");
            assert_eq!(text, "result-b");
        }
        other => panic!("expected tool output, got {}", step_kind(other)),
    }
    match &plan.steps[3] {
        PlanStep::Media { images, .. } => {
            assert_eq!(
                images.len(),
                1,
                "tool B's image must not be pooled with tool A's"
            );
        }
        other => panic!("expected media step, got {}", step_kind(other)),
    }
}

#[test]
fn nested_audio_and_resource_link_become_fallback_markers() {
    // Nested Audio and ResourceLink cannot be forwarded as media by the
    // pinned builder, but their reference information must survive as
    // explicit text markers instead of being silently dropped.
    let message = msg(vec![tool_result(
        "call_x",
        vec![
            Content::text("status ok"),
            Content::audio("audio/wav", vec![1, 2, 3, 4]),
            Content::ResourceLink {
                uri: "file:///tmp/report.pdf".to_string(),
                name: None,
                description: None,
                mime_type: None,
            },
        ],
    )]);

    let plan = plan_message(&message).unwrap();
    assert_eq!(plan.steps.len(), 1, "no media step for markers-only result");
    match &plan.steps[0] {
        PlanStep::ToolOutput { id, text } => {
            assert_eq!(id, "call_x");
            assert_eq!(
                text,
                "status ok\n\
                 [audio omitted: mime_type=audio/wav, 4 bytes]\n\
                 [resource link: file:///tmp/report.pdf]"
            );
        }
        other => panic!("expected a single tool output, got {}", step_kind(other)),
    }

    // Named links with a MIME type keep their metadata in the marker.
    let named = msg(vec![tool_result(
        "call_y",
        vec![Content::ResourceLink {
            uri: "file:///tmp/report.pdf".to_string(),
            name: Some("report.pdf".to_string()),
            description: None,
            mime_type: Some("application/pdf".to_string()),
        }],
    )]);
    let plan = plan_message(&named).unwrap();
    match &plan.steps[0] {
        PlanStep::ToolOutput { text, .. } => assert_eq!(
            text,
            "[resource link: report.pdf (file:///tmp/report.pdf), application/pdf]"
        ),
        other => panic!("expected a tool output, got {}", step_kind(other)),
    }
}

#[test]
fn nested_pdf_and_image_url_are_rejected_like_top_level() {
    let nested_pdf = msg(vec![tool_result(
        "call_p",
        vec![Content::text("doc"), Content::pdf(vec![1, 2, 3])],
    )]);
    assert!(matches!(
        plan_message(&nested_pdf),
        Err(LLMError::InvalidRequest(_))
    ));

    let nested_url = msg(vec![tool_result(
        "call_u",
        vec![Content::image_url("https://example.com/img.png")],
    )]);
    assert!(matches!(
        plan_message(&nested_url),
        Err(LLMError::InvalidRequest(_))
    ));

    // Top-level policy is unchanged.
    assert!(matches!(
        plan_message(&msg(vec![Content::pdf(vec![1, 2, 3])])),
        Err(LLMError::InvalidRequest(_))
    ));
    assert!(matches!(
        plan_message(&msg(vec![Content::image_url(
            "https://example.com/img.png"
        )])),
        Err(LLMError::InvalidRequest(_))
    ));
}

#[test]
fn tool_use_with_top_level_media_is_rejected() {
    let image_then_tool = msg(vec![
        Content::image("image/png", png_bytes()),
        tool_use("call_1"),
    ]);
    let err = match plan_message(&image_then_tool) {
        Err(err) => err,
        Ok(plan) => panic!("expected InvalidRequest, got {} step(s)", plan.steps.len()),
    };
    match err {
        LLMError::InvalidRequest(text) => {
            assert!(text.contains("tool calls"), "clear error required: {text}");
            assert!(text.contains("media"), "clear error required: {text}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }

    // Order within the message does not matter.
    let tool_then_image = msg(vec![
        tool_use("call_1"),
        Content::image("image/png", png_bytes()),
    ]);
    assert!(matches!(
        plan_message(&tool_then_image),
        Err(LLMError::InvalidRequest(_))
    ));
}

#[test]
fn empty_tool_result_gets_fallback_marker() {
    let plan = plan_message(&msg(vec![tool_result("call_e", vec![])])).unwrap();
    assert_eq!(plan.steps.len(), 1);
    match &plan.steps[0] {
        PlanStep::ToolOutput { id, text } => {
            assert_eq!(id, "call_e");
            assert_eq!(text, "[tool result contained no content]");
        }
        other => panic!("expected tool output, got {}", step_kind(other)),
    }
}

#[test]
fn single_empty_text_tool_result_gets_fallback_marker() {
    // Regression: a lone `Content::text("")` inside a tool result must be
    // skipped so the visible fallback marker is emitted instead of an
    // empty ToolOutput.
    let plan = plan_message(&msg(vec![tool_result("call_s", vec![Content::text("")])])).unwrap();
    assert_eq!(plan.steps.len(), 1);
    match &plan.steps[0] {
        PlanStep::ToolOutput { id, text } => {
            assert_eq!(id, "call_s");
            assert_eq!(text, "[tool result contained no content]");
        }
        other => panic!("expected tool output, got {}", step_kind(other)),
    }
}

#[test]
fn invalid_image_payload_is_rejected() {
    let top_level = msg(vec![Content::image("image/png", vec![0xDE, 0xAD])]);
    assert!(matches!(
        plan_message(&top_level),
        Err(LLMError::InvalidRequest(_))
    ));

    let nested = msg(vec![tool_result(
        "call_i",
        vec![Content::image("image/png", vec![0xDE, 0xAD])],
    )]);
    assert!(matches!(
        plan_message(&nested),
        Err(LLMError::InvalidRequest(_))
    ));
}

#[test]
fn plain_text_media_and_tool_calls_emit_single_steps() {
    // Multiple text blocks join into exactly one text message.
    let plan = plan_message(&msg(vec![Content::text("a"), Content::text("b")])).unwrap();
    assert_eq!(plan.steps.len(), 1);
    match &plan.steps[0] {
        PlanStep::Text { text, .. } => assert_eq!(text, "a\nb"),
        other => panic!("expected text step, got {}", step_kind(other)),
    }

    // Media travels with its caption in a single multimodal message; the
    // text must not be emitted twice.
    let plan = plan_message(&msg(vec![
        Content::image("image/png", png_bytes()),
        Content::text("caption"),
    ]))
    .unwrap();
    assert_eq!(plan.steps.len(), 1);
    match &plan.steps[0] {
        PlanStep::Media { images, text, .. } => {
            assert_eq!(images.len(), 1);
            assert_eq!(text, "caption");
        }
        other => panic!("expected media step, got {}", step_kind(other)),
    }

    // Tool calls keep their text and source order in a single message.
    let plan = plan_message(&msg(vec![
        tool_use("call_1"),
        tool_use("call_2"),
        Content::text("calling"),
    ]))
    .unwrap();
    assert_eq!(plan.steps.len(), 1);
    match &plan.steps[0] {
        PlanStep::ToolCalls {
            text, tool_calls, ..
        } => {
            assert_eq!(text, "calling");
            let ids: Vec<&str> = tool_calls.iter().map(|call| call.id.as_str()).collect();
            assert_eq!(ids, ["call_1", "call_2"]);
        }
        other => panic!("expected tool-calls step, got {}", step_kind(other)),
    }
}
