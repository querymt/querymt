use querymt::error::{LLMError, LLMErrorPayload, TransportErrorKind};
#[cfg(any(feature = "kameo-mesh", test))]
use serde::Serialize;
#[cfg(any(feature = "kameo-mesh", test))]
use std::borrow::Cow;
#[cfg(any(feature = "kameo-mesh", test))]
use std::io::Write;
#[cfg(any(feature = "kameo-mesh", test))]
use std::time::Duration;

// Keep this aligned with every mesh receiver config and the provider preflight.
#[cfg(any(feature = "kameo-mesh", test))]
pub(crate) const MESH_MESSAGE_SIZE_MAXIMUM: u64 = 50 * 1024 * 1024;
#[cfg(any(feature = "kameo-mesh", test))]
const LEGACY_MESH_REQUEST_SIZE_MAXIMUM: u64 = 1024 * 1024;

#[cfg(any(feature = "kameo-mesh", test))]
#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

#[cfg(any(feature = "kameo-mesh", test))]
impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = u64::try_from(buf.len())
            .map_err(|_| std::io::Error::other("buffer length does not fit in u64"))?;
        self.bytes = self
            .bytes
            .checked_add(len)
            .ok_or_else(|| std::io::Error::other("encoded request size overflow"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(any(feature = "kameo-mesh", test))]
fn serialize_message<T: Serialize>(message: &T) -> Result<Vec<u8>, LLMError> {
    rmp_serde::to_vec_named(message).map_err(|error| {
        LLMError::InvalidRequest(format!(
            "failed to serialize remote provider request: {error}"
        ))
    })
}

#[cfg(any(feature = "kameo-mesh", test))]
fn encoded_envelope_size(
    envelope: &kameo::remote::messaging::SwarmRequest,
) -> Result<u64, LLMError> {
    let mut writer = CountingWriter::default();
    cbor4ii::serde::to_writer(&mut writer, envelope).map_err(|error| {
        LLMError::InvalidRequest(format!("failed to encode remote provider request: {error}"))
    })?;
    Ok(writer.bytes)
}

#[cfg(any(feature = "kameo-mesh", test))]
pub(crate) fn remote_ask_request_size<A, M>(
    actor_ref: &kameo::actor::RemoteActorRef<A>,
    message: &M,
    mailbox_timeout: Option<Duration>,
    reply_timeout: Option<Duration>,
) -> Result<u64, LLMError>
where
    A: kameo::Actor + kameo::remote::RemoteActor + kameo::remote::RemoteMessage<M>,
    M: Serialize,
{
    let payload = serialize_message(message)?;
    encoded_envelope_size(&kameo::remote::messaging::SwarmRequest::Ask {
        actor_id: actor_ref.id(),
        actor_remote_id: Cow::Borrowed(<A as kameo::remote::RemoteActor>::REMOTE_ID),
        message_remote_id: Cow::Borrowed(<A as kameo::remote::RemoteMessage<M>>::REMOTE_ID),
        payload,
        mailbox_timeout,
        reply_timeout,
        immediate: false,
    })
}

#[cfg(any(feature = "kameo-mesh", test))]
pub(crate) fn remote_tell_request_size<A, M>(
    actor_ref: &kameo::actor::RemoteActorRef<A>,
    message: &M,
    mailbox_timeout: Option<Duration>,
) -> Result<u64, LLMError>
where
    A: kameo::Actor + kameo::remote::RemoteActor + kameo::remote::RemoteMessage<M>,
    M: Serialize,
{
    let payload = serialize_message(message)?;
    encoded_envelope_size(&kameo::remote::messaging::SwarmRequest::Tell {
        actor_id: actor_ref.id(),
        actor_remote_id: Cow::Borrowed(<A as kameo::remote::RemoteActor>::REMOTE_ID),
        message_remote_id: Cow::Borrowed(<A as kameo::remote::RemoteMessage<M>>::REMOTE_ID),
        payload,
        mailbox_timeout,
        immediate: false,
    })
}

#[cfg(any(feature = "kameo-mesh", test))]
pub(crate) fn ensure_mesh_request_fits(encoded_bytes: u64) -> Result<(), LLMError> {
    if encoded_bytes <= MESH_MESSAGE_SIZE_MAXIMUM {
        return Ok(());
    }

    Err(LLMError::InvalidRequest(format!(
        "remote provider request is {encoded_bytes} bytes after mesh encoding, exceeding the {}-byte limit; reduce conversation history, tool definitions, or attachments",
        MESH_MESSAGE_SIZE_MAXIMUM
    )))
}

#[cfg(any(feature = "kameo-mesh", test))]
pub(crate) fn remap_legacy_oversize_eof(error: LLMError, encoded_bytes: u64) -> LLMError {
    let is_legacy_eof = match &error {
        LLMError::IoError(source) => {
            source.kind() == std::io::ErrorKind::UnexpectedEof
                && legacy_limit_eof_message(source.to_string().as_str())
        }
        _ => false,
    };
    if is_legacy_eof && encoded_bytes > LEGACY_MESH_REQUEST_SIZE_MAXIMUM {
        LLMError::InvalidRequest(format!(
            "remote provider peer rejected the {encoded_bytes}-byte mesh request; it likely uses the legacy {}-byte request limit and must be upgraded",
            LEGACY_MESH_REQUEST_SIZE_MAXIMUM
        ))
    } else {
        error
    }
}

#[cfg(any(feature = "kameo-mesh", test))]
fn legacy_limit_eof_message(message: &str) -> bool {
    message == "Eof { name: \"enum\", expect: Small(1) }"
}

pub fn remote_send_error_base<E>(error: kameo::error::RemoteSendError<E>) -> Result<LLMError, E> {
    use kameo::error::RemoteSendError;

    match error {
        RemoteSendError::ActorNotRunning | RemoteSendError::ActorStopped => {
            Ok(LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                message: "remote actor not running".to_string(),
            })
        }
        RemoteSendError::UnknownActor { .. } | RemoteSendError::UnknownMessage { .. } => {
            Ok(LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                message: "remote actor unavailable".to_string(),
            })
        }
        RemoteSendError::BadActorType => {
            Ok(LLMError::ProviderError("bad remote actor type".to_string()))
        }
        RemoteSendError::MailboxFull => Ok(LLMError::Transport {
            kind: TransportErrorKind::Other,
            message: "remote mailbox full".to_string(),
        }),
        RemoteSendError::ReplyTimeout | RemoteSendError::NetworkTimeout => {
            Ok(LLMError::Transport {
                kind: TransportErrorKind::Timeout,
                message: "network timeout".to_string(),
            })
        }
        RemoteSendError::DialFailure => Ok(LLMError::Transport {
            kind: TransportErrorKind::ConnectionRefused,
            message: "dial failure".to_string(),
        }),
        RemoteSendError::ConnectionClosed => Ok(LLMError::Transport {
            kind: TransportErrorKind::ConnectionClosed,
            message: "connection closed".to_string(),
        }),
        RemoteSendError::UnsupportedProtocols => Ok(LLMError::ProviderError(
            "remote protocol unsupported".to_string(),
        )),
        RemoteSendError::SerializeMessage(err)
        | RemoteSendError::DeserializeMessage(err)
        | RemoteSendError::SerializeReply(err)
        | RemoteSendError::SerializeHandlerError(err)
        | RemoteSendError::DeserializeHandlerError(err) => Ok(LLMError::ProviderError(err)),
        RemoteSendError::SwarmNotBootstrapped => Ok(LLMError::Transport {
            kind: TransportErrorKind::Other,
            message: "swarm not bootstrapped".to_string(),
        }),
        RemoteSendError::Io(Some(err)) => Ok(LLMError::from(err)),
        RemoteSendError::Io(None) => Ok(LLMError::Transport {
            kind: TransportErrorKind::Other,
            message: "remote IO failure".to_string(),
        }),
        RemoteSendError::HandlerError(err) => Err(err),
    }
}

pub fn remote_send_error_to_llm_error_no_handler(
    error: kameo::error::RemoteSendError<kameo::error::Infallible>,
) -> LLMError {
    match remote_send_error_base(error) {
        Ok(err) => err,
        Err(never) => match never {},
    }
}

pub fn decode_payload_handler_error(reason: &str) -> LLMError {
    serde_json::from_str::<LLMErrorPayload>(reason)
        .map(LLMError::from_payload)
        .unwrap_or_else(|_| LLMError::ProviderError(reason.to_string()))
}

pub fn should_retry_remote_send<E>(error: &kameo::error::RemoteSendError<E>) -> bool {
    use kameo::error::RemoteSendError;

    matches!(
        error,
        RemoteSendError::ActorNotRunning
            | RemoteSendError::ActorStopped
            | RemoteSendError::UnknownActor { .. }
            | RemoteSendError::UnknownMessage { .. }
            | RemoteSendError::DialFailure
            | RemoteSendError::ConnectionClosed
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::error::RemoteSendError;
    use kameo::remote::messaging::SwarmRequest;
    use querymt::chat::{ChatMessage, Content};
    use std::borrow::Cow;

    fn tell_request<T: Serialize>(message: &T) -> SwarmRequest {
        SwarmRequest::Tell {
            actor_id: kameo::actor::ActorId::new(7),
            actor_remote_id: Cow::Borrowed("test-actor"),
            message_remote_id: Cow::Borrowed("test-message"),
            payload: rmp_serde::to_vec_named(message).unwrap(),
            mailbox_timeout: None,
            immediate: false,
        }
    }

    fn wire_bytes(request: &SwarmRequest) -> Vec<u8> {
        cbor4ii::serde::to_vec(Vec::new(), request).unwrap()
    }

    #[test]
    fn encoded_envelope_size_matches_the_real_cbor_codec() {
        let messages = vec![
            ChatMessage::from_user(vec![Content::image(
                "image/png",
                (0..258_708).map(|i| (i % 256) as u8).collect(),
            )]),
            ChatMessage::from_user(vec![Content::image(
                "image/png",
                (0..250_180).map(|i| (i % 256) as u8).collect(),
            )]),
        ];
        let request = tell_request(&messages);
        let actual_size = wire_bytes(&request).len() as u64;

        assert!(actual_size > LEGACY_MESH_REQUEST_SIZE_MAXIMUM);
        assert!(actual_size <= MESH_MESSAGE_SIZE_MAXIMUM);
        assert_eq!(encoded_envelope_size(&request).unwrap(), actual_size);
    }

    #[test]
    fn oversize_preflight_is_actionable_and_not_retryable() {
        ensure_mesh_request_fits(MESH_MESSAGE_SIZE_MAXIMUM).unwrap();
        let error = ensure_mesh_request_fits(MESH_MESSAGE_SIZE_MAXIMUM + 1).unwrap_err();

        assert!(!error.is_retryable());
        assert!(matches!(error, LLMError::InvalidRequest(_)));
        assert!(error.to_string().contains("52428801 bytes"));
        assert!(error.to_string().contains("52428800-byte limit"));
    }

    #[test]
    fn only_large_exact_enum_eof_is_remapped_as_legacy_limit() {
        let eof = || {
            LLMError::IoError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Eof { name: \"enum\", expect: Small(1) }",
            ))
        };

        let remapped = remap_legacy_oversize_eof(eof(), LEGACY_MESH_REQUEST_SIZE_MAXIMUM + 1);
        assert!(matches!(remapped, LLMError::InvalidRequest(_)));
        assert!(!remapped.is_retryable());

        let small = remap_legacy_oversize_eof(eof(), LEGACY_MESH_REQUEST_SIZE_MAXIMUM);
        assert!(matches!(small, LLMError::IoError(_)));
        let unrelated = remap_legacy_oversize_eof(
            LLMError::IoError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unrelated EOF",
            )),
            LEGACY_MESH_REQUEST_SIZE_MAXIMUM + 1,
        );
        assert!(matches!(unrelated, LLMError::IoError(_)));
    }

    #[test]
    fn retry_classifier_only_retries_connection_recovery_cases() {
        assert!(should_retry_remote_send::<String>(
            &RemoteSendError::ActorNotRunning
        ));
        assert!(should_retry_remote_send::<String>(
            &RemoteSendError::ActorStopped
        ));
        assert!(should_retry_remote_send::<String>(
            &RemoteSendError::UnknownActor {
                actor_remote_id: Cow::Borrowed("actor"),
            }
        ));
        assert!(should_retry_remote_send::<String>(
            &RemoteSendError::UnknownMessage {
                actor_remote_id: Cow::Borrowed("actor"),
                message_remote_id: Cow::Borrowed("message"),
            }
        ));
        assert!(should_retry_remote_send::<String>(
            &RemoteSendError::DialFailure
        ));
        assert!(should_retry_remote_send::<String>(
            &RemoteSendError::ConnectionClosed
        ));
        assert!(!should_retry_remote_send::<String>(
            &RemoteSendError::MailboxFull
        ));
        assert!(!should_retry_remote_send::<String>(
            &RemoteSendError::BadActorType
        ));
    }

    #[test]
    fn remote_send_error_base_maps_transport_and_provider_failures() {
        let err = remote_send_error_base::<String>(RemoteSendError::ReplyTimeout).unwrap();
        assert!(matches!(
            err,
            LLMError::Transport {
                kind: TransportErrorKind::Timeout,
                ..
            }
        ));

        let err = remote_send_error_base::<String>(RemoteSendError::BadActorType).unwrap();
        assert!(matches!(err, LLMError::ProviderError(ref msg) if msg == "bad remote actor type"));

        let err = remote_send_error_base::<String>(RemoteSendError::SerializeReply(
            "serialize fail".to_string(),
        ))
        .unwrap();
        assert!(matches!(err, LLMError::ProviderError(ref msg) if msg == "serialize fail"));
    }

    #[test]
    fn remote_send_error_base_preserves_handler_error() {
        let err = remote_send_error_base(RemoteSendError::HandlerError("handler boom".to_string()))
            .expect_err("handler errors should bubble up");
        assert_eq!(err, "handler boom");
    }

    #[test]
    fn decode_payload_handler_error_parses_json_payload_when_available() {
        let payload = serde_json::to_string(&LLMErrorPayload::Transport {
            kind: TransportErrorKind::ConnectionClosed,
            message: "lost link".to_string(),
        })
        .unwrap();
        let err = decode_payload_handler_error(&payload);
        assert!(matches!(
            err,
            LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                ref message,
            } if message == "lost link"
        ));

        let fallback = decode_payload_handler_error("plain failure");
        assert!(matches!(
            fallback,
            LLMError::ProviderError(ref message) if message == "plain failure"
        ));
    }
}
