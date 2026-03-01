use futures::executor::block_on;
use qmt_izwi::{IzwiConfig, create_provider};
use querymt::error::LLMError;
use querymt::{stt::SttRequest, tts::TtsRequest};

fn make_provider() -> Box<dyn querymt::LLMProvider> {
    create_provider(IzwiConfig::default()).expect("provider construction should succeed")
}

#[test]
fn smoke_provider_construction() {
    let _provider = make_provider();
}

#[test]
fn smoke_transcribe_rejects_empty_audio() {
    let provider = make_provider();
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
    let provider = make_provider();
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
    let provider = make_provider();
    let req = TtsRequest::new().text("hello").format("mp3");
    let err = block_on(provider.speech(&req)).expect_err("unsupported format should fail");
    match err {
        LLMError::InvalidRequest(message) => {
            assert!(message.contains("Unsupported izwi TTS format"));
        }
        other => panic!("expected InvalidRequest, got {other}"),
    }
}
