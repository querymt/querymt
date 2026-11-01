use super::*;
use crate::config::{SpeechConfig, SpeechOperation, SpeechProfile};
use crate::speech::SpeechService;
use base64::Engine as _;

struct SpeechExtFixture {
    handle: LocalAgentHandle,
    _temp_dir: tempfile::TempDir,
}

fn stt_profile() -> SpeechProfile {
    SpeechProfile {
        id: "local-stt".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        operation: SpeechOperation::Transcription,
        api_key: None,
        base_url: None,
        voice: None,
        voice_description: None,
    }
}

fn tts_profile() -> SpeechProfile {
    SpeechProfile {
        id: "local-tts".into(),
        provider: "mock".into(),
        model: "mock-tts".into(),
        operation: SpeechOperation::Synthesis,
        api_key: None,
        base_url: None,
        voice: None,
        voice_description: None,
    }
}

fn speech_config() -> SpeechConfig {
    SpeechConfig {
        default_transcription: Some("local-stt".into()),
        default_synthesis: None,
        profiles: vec![stt_profile()],
    }
}

fn mixed_speech_config() -> SpeechConfig {
    SpeechConfig {
        default_transcription: Some("local-stt".into()),
        default_synthesis: Some("local-tts".into()),
        profiles: vec![stt_profile(), tts_profile()],
    }
}

fn audio_params(data: &str, mime_type: &str) -> serde_json::Value {
    serde_json::json!({
        "audio": {
            "data": data,
            "mimeType": mime_type,
        }
    })
}

async fn speech_fixture(
    configure_service: bool,
    transcribe_text: Option<&str>,
) -> SpeechExtFixture {
    speech_fixture_with(speech_config(), configure_service, transcribe_text, None).await
}

async fn speech_fixture_with(
    config: SpeechConfig,
    configure_service: bool,
    transcribe_text: Option<&str>,
    synthesize_audio: Option<&[u8]>,
) -> SpeechExtFixture {
    let mut mock = MockLlmProvider::new();
    if let Some(text) = transcribe_text {
        let text = text.to_string();
        mock.expect_transcribe().returning(move |_req| {
            Ok(querymt::stt::SttResponse {
                text: text.clone(),
                language: None,
            })
        });
    }
    if let Some(audio) = synthesize_audio {
        let audio = audio.to_vec();
        mock.expect_speech().returning(move |_req| {
            Ok(querymt::tts::TtsResponse {
                audio: audio.clone(),
                mime_type: Some("audio/wav".into()),
            })
        });
    }
    let shared = SharedLlmProvider::new(mock, vec![]);
    let factory = Arc::new(TestProviderFactory::new(shared));
    let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");
    let plugin_registry = Arc::new(plugin_registry);

    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("create storage"),
    );
    let mut builder = AgentConfigBuilder::new(
        plugin_registry.clone(),
        storage.clone(),
        LLMParams::new().provider("mock").model("mock-model"),
    )
    .with_tool_policy(ToolPolicy::ProviderOnly);
    if configure_service {
        let service =
            SpeechService::new(config, plugin_registry).expect("speech service should construct");
        builder = builder.with_speech_service(service);
    }
    SpeechExtFixture {
        handle: LocalAgentHandle::from_config(Arc::new(builder.build())),
        _temp_dir: temp_dir,
    }
}

#[tokio::test]
async fn list_profiles_returns_configured_profiles() {
    let f = speech_fixture(true, None).await;
    let result = ext_method_json(
        &f.handle,
        "querymt/speech/listProfiles",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(result["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(result["profiles"][0]["id"], "local-stt");
    assert_eq!(result["profiles"][0]["operation"], "transcription");
    assert_eq!(result["defaults"]["transcription"], "local-stt");
    assert!(result["defaults"].get("synthesis").is_none());

    let filtered = ext_method_json(
        &f.handle,
        "querymt/speech/listProfiles",
        serde_json::json!({ "operation": "synthesis" }),
    )
    .await;
    assert!(filtered["profiles"].as_array().unwrap().is_empty());
    assert_eq!(filtered["defaults"]["transcription"], "local-stt");
}

#[tokio::test]
async fn list_profiles_filters_synthesis_profiles() {
    let f = speech_fixture_with(mixed_speech_config(), true, None, None).await;
    let result = ext_method_json(
        &f.handle,
        "querymt/speech/listProfiles",
        serde_json::json!({ "operation": "synthesis" }),
    )
    .await;
    assert_eq!(result["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(result["profiles"][0]["id"], "local-tts");
    assert_eq!(result["profiles"][0]["operation"], "synthesis");
    assert_eq!(result["defaults"]["synthesis"], "local-tts");
}

#[tokio::test]
async fn transcribe_returns_text_from_mock_stt() {
    let f = speech_fixture(true, Some("hello world")).await;
    let data = base64::engine::general_purpose::STANDARD.encode(b"hello");
    let result = ext_method_json(
        &f.handle,
        "querymt/speech/transcribe",
        audio_params(&data, "audio/wav"),
    )
    .await;
    assert_eq!(result["text"], "hello world");
    assert_eq!(result["profile"], "local-stt");
    assert!(result.get("language").is_none());
}

#[tokio::test]
async fn transcribe_rejects_unknown_profile() {
    let f = speech_fixture(true, None).await;
    let data = base64::engine::general_purpose::STANDARD.encode(b"hello");
    let mut params = audio_params(&data, "audio/wav");
    params["profile"] = serde_json::json!("missing");
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/speech/transcribe",
            raw_params(&params.to_string()),
        ))
        .await
        .expect_err("unknown profile should be invalid params");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
}

#[tokio::test]
async fn transcribe_rejects_synthesis_profile() {
    let f = speech_fixture_with(mixed_speech_config(), true, None, None).await;
    let data = base64::engine::general_purpose::STANDARD.encode(b"hello");
    let mut params = audio_params(&data, "audio/wav");
    params["profile"] = serde_json::json!("local-tts");
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/speech/transcribe",
            raw_params(&params.to_string()),
        ))
        .await
        .expect_err("synthesis profile should be invalid for transcribe");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
}

#[tokio::test]
async fn transcribe_rejects_invalid_and_empty_audio() {
    let f = speech_fixture(true, None).await;
    for params in [
        audio_params("", "audio/wav"),
        audio_params("!!!", "audio/wav"),
        audio_params(
            &base64::engine::general_purpose::STANDARD.encode(b"hello"),
            "image/png",
        ),
        serde_json::json!({}),
    ] {
        let err = f
            .handle
            .ext_method(crate::acp::protocol::ExtRequest::new(
                "querymt/speech/transcribe",
                raw_params(&params.to_string()),
            ))
            .await
            .expect_err("invalid audio should be invalid params");
        assert_eq!(
            err.code,
            agent_client_protocol::ErrorCode::InvalidParams,
            "{params}"
        );
    }
}

#[tokio::test]
async fn synthesize_returns_audio_from_mock_tts() {
    let audio = b"RIFFWAV";
    let f = speech_fixture_with(mixed_speech_config(), true, None, Some(audio)).await;
    let result = ext_method_json(
        &f.handle,
        "querymt/speech/synthesize",
        serde_json::json!({ "text": "hello world" }),
    )
    .await;
    assert_eq!(result["profile"], "local-tts");
    assert_eq!(result["audio"]["mimeType"], "audio/wav");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(result["audio"]["data"].as_str().unwrap())
        .expect("audio should be base64");
    assert_eq!(decoded, audio);
}

#[tokio::test]
async fn synthesize_rejects_empty_text() {
    let f = speech_fixture_with(mixed_speech_config(), true, None, None).await;
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/speech/synthesize",
            raw_params(r#"{"text":"   "}"#),
        ))
        .await
        .expect_err("empty text should be invalid params");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
}

#[tokio::test]
async fn synthesize_rejects_transcription_profile() {
    let f = speech_fixture_with(mixed_speech_config(), true, None, None).await;
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/speech/synthesize",
            raw_params(r#"{"profile":"local-stt","text":"hello"}"#),
        ))
        .await
        .expect_err("transcription profile should be invalid for synthesize");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
}

#[tokio::test]
async fn speech_methods_are_absent_without_config() {
    let f = speech_fixture(false, None).await;
    for method in [
        "querymt/speech/listProfiles",
        "querymt/speech/transcribe",
        "querymt/speech/synthesize",
    ] {
        let err = f
            .handle
            .ext_method(crate::acp::protocol::ExtRequest::new(
                method,
                raw_params("{}"),
            ))
            .await
            .expect_err("unconfigured speech should be method not found");
        assert_eq!(err.code, agent_client_protocol::ErrorCode::MethodNotFound);
    }
    let caps = crate::control::capabilities::get_capabilities(&f.handle);
    assert!(
        !caps
            .methods
            .iter()
            .any(|method| method.starts_with("querymt/speech/"))
    );
}

#[tokio::test]
async fn speech_methods_are_advertised_when_configured() {
    let f = speech_fixture(true, None).await;
    let caps = crate::control::capabilities::get_capabilities(&f.handle);
    assert!(
        caps.methods
            .iter()
            .any(|method| method == "querymt/speech/listProfiles")
    );
    assert!(
        caps.methods
            .iter()
            .any(|method| method == "querymt/speech/transcribe")
    );
    assert!(
        !caps
            .methods
            .iter()
            .any(|method| method == "querymt/speech/synthesize")
    );
}

#[tokio::test]
async fn synthesize_is_advertised_when_synthesis_profile_exists() {
    let f = speech_fixture_with(mixed_speech_config(), true, None, None).await;
    let caps = crate::control::capabilities::get_capabilities(&f.handle);
    assert!(
        caps.methods
            .iter()
            .any(|method| method == "querymt/speech/synthesize")
    );
}

fn design_speech_config() -> SpeechConfig {
    let mut profile = tts_profile();
    profile.voice_description = Some("A warm female voice, mid-30s".into());
    SpeechConfig {
        default_transcription: Some("local-stt".into()),
        default_synthesis: Some("local-tts".into()),
        profiles: vec![stt_profile(), profile],
    }
}

fn preset_speech_config() -> SpeechConfig {
    let mut profile = tts_profile();
    profile.voice = Some("vivian".into());
    SpeechConfig {
        default_transcription: Some("local-stt".into()),
        default_synthesis: Some("local-tts".into()),
        profiles: vec![stt_profile(), profile],
    }
}

async fn speech_fixture_capturing_tts(
    config: SpeechConfig,
    audio: &[u8],
) -> (
    SpeechExtFixture,
    Arc<std::sync::Mutex<Option<querymt::tts::TtsRequest>>>,
) {
    let captured = Arc::new(std::sync::Mutex::new(None));
    let captured_for_mock = captured.clone();
    let audio = audio.to_vec();
    let mut mock = MockLlmProvider::new();
    mock.expect_speech().returning(move |req| {
        *captured_for_mock.lock().expect("capture lock") = Some(req.clone());
        Ok(querymt::tts::TtsResponse {
            audio: audio.clone(),
            mime_type: Some("audio/wav".into()),
        })
    });
    let shared = SharedLlmProvider::new(mock, vec![]);
    let factory = Arc::new(TestProviderFactory::new(shared));
    let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");
    let plugin_registry = Arc::new(plugin_registry);
    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("create storage"),
    );
    let service = SpeechService::new(config, plugin_registry.clone())
        .expect("speech service should construct");
    let handle = LocalAgentHandle::from_config(Arc::new(
        AgentConfigBuilder::new(
            plugin_registry,
            storage,
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .with_tool_policy(ToolPolicy::ProviderOnly)
        .with_speech_service(service)
        .build(),
    ));
    (
        SpeechExtFixture {
            handle,
            _temp_dir: temp_dir,
        },
        captured,
    )
}

#[tokio::test]
async fn list_profiles_includes_voice_description() {
    let f = speech_fixture_with(design_speech_config(), true, None, None).await;
    let result = ext_method_json(
        &f.handle,
        "querymt/speech/listProfiles",
        serde_json::json!({ "operation": "synthesis" }),
    )
    .await;
    assert_eq!(
        result["profiles"][0]["voice_description"],
        "A warm female voice, mid-30s"
    );
}

#[tokio::test]
async fn synthesize_uses_profile_voice_description() {
    let (f, captured) = speech_fixture_capturing_tts(design_speech_config(), b"RIFFWAV").await;
    ext_method_json(
        &f.handle,
        "querymt/speech/synthesize",
        serde_json::json!({ "text": "hello world" }),
    )
    .await;
    let request = captured.lock().expect("capture lock").clone().unwrap();
    assert_eq!(
        request.voice_config,
        Some(querymt::tts::VoiceConfig::design(
            "A warm female voice, mid-30s"
        ))
    );
}

#[tokio::test]
async fn synthesize_request_voice_overrides_profile_description() {
    let (f, captured) = speech_fixture_capturing_tts(design_speech_config(), b"RIFFWAV").await;
    ext_method_json(
        &f.handle,
        "querymt/speech/synthesize",
        serde_json::json!({ "text": "hello world", "voice": "vivian" }),
    )
    .await;
    let request = captured.lock().expect("capture lock").clone().unwrap();
    assert_eq!(
        request.voice_config,
        Some(querymt::tts::VoiceConfig::preset("vivian"))
    );
}

#[tokio::test]
async fn list_profiles_includes_voice() {
    let f = speech_fixture_with(preset_speech_config(), true, None, None).await;
    let result = ext_method_json(
        &f.handle,
        "querymt/speech/listProfiles",
        serde_json::json!({ "operation": "synthesis" }),
    )
    .await;
    assert_eq!(result["profiles"][0]["voice"], "vivian");
}

#[tokio::test]
async fn synthesize_uses_profile_voice() {
    let (f, captured) = speech_fixture_capturing_tts(preset_speech_config(), b"RIFFWAV").await;
    ext_method_json(
        &f.handle,
        "querymt/speech/synthesize",
        serde_json::json!({ "text": "hello world" }),
    )
    .await;
    let request = captured.lock().expect("capture lock").clone().unwrap();
    assert_eq!(
        request.voice_config,
        Some(querymt::tts::VoiceConfig::preset("vivian"))
    );
}

#[tokio::test]
async fn synthesize_request_voice_overrides_profile_voice() {
    let (f, captured) = speech_fixture_capturing_tts(preset_speech_config(), b"RIFFWAV").await;
    ext_method_json(
        &f.handle,
        "querymt/speech/synthesize",
        serde_json::json!({ "text": "hello world", "voice": "ryan" }),
    )
    .await;
    let request = captured.lock().expect("capture lock").clone().unwrap();
    assert_eq!(
        request.voice_config,
        Some(querymt::tts::VoiceConfig::preset("ryan"))
    );
}

#[tokio::test]
async fn synthesize_profile_voice_wins_over_voice_description() {
    let mut profile = tts_profile();
    profile.voice = Some("vivian".into());
    profile.voice_description = Some("A warm female voice, mid-30s".into());
    let config = SpeechConfig {
        default_transcription: Some("local-stt".into()),
        default_synthesis: Some("local-tts".into()),
        profiles: vec![stt_profile(), profile],
    };
    let (f, captured) = speech_fixture_capturing_tts(config, b"RIFFWAV").await;
    ext_method_json(
        &f.handle,
        "querymt/speech/synthesize",
        serde_json::json!({ "text": "hello world" }),
    )
    .await;
    let request = captured.lock().expect("capture lock").clone().unwrap();
    assert_eq!(
        request.voice_config,
        Some(querymt::tts::VoiceConfig::preset("vivian"))
    );
}
