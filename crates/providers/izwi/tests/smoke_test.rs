use futures::executor::block_on;
use qmt_izwi::{IzwiConfig, create_provider};
use querymt::error::LLMError;
use querymt::{stt::SttRequest, tts::TtsRequest};

fn make_provider() -> (Box<dyn querymt::LLMProvider>, tempfile::TempDir) {
    let models = tempfile::tempdir().expect("model directory");
    let provider = create_provider(IzwiConfig {
        models_dir: Some(
            models
                .path()
                .to_str()
                .expect("UTF-8 model directory")
                .into(),
        ),
        ..IzwiConfig::default()
    })
    .expect("provider construction should succeed");
    (provider, models)
}

#[test]
fn smoke_provider_construction() {
    let (_provider, _models) = make_provider();
}

#[test]
fn smoke_transcribe_rejects_empty_audio() {
    let (provider, _models) = make_provider();
    let req = SttRequest::new();
    let err = block_on(provider.transcribe(&req)).expect_err("empty audio should fail");
    match err {
        LLMError::InvalidRequest(message) => {
            assert!(message.contains("audio is empty"));
        }
        other => panic!("expected InvalidRequest, got {other}"),
    }
}

#[test]
fn smoke_speech_rejects_empty_text() {
    let (provider, _models) = make_provider();
    let req = TtsRequest::new();
    let err = block_on(provider.speech(&req)).expect_err("empty text should fail");
    match err {
        LLMError::InvalidRequest(message) => {
            assert!(message.contains("text is empty"));
        }
        other => panic!("expected InvalidRequest, got {other}"),
    }
}

#[test]
fn smoke_speech_rejects_unsupported_format() {
    let (provider, _models) = make_provider();
    let req = TtsRequest::new().text("hello").format("mp3");
    let err = block_on(provider.speech(&req)).expect_err("unsupported format should fail");
    match err {
        LLMError::InvalidRequest(message) => {
            assert!(message.contains("Unsupported izwi TTS format"));
        }
        other => panic!("expected InvalidRequest, got {other}"),
    }
}
