use querymt::error::{LLMError, TransportErrorKind};
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

/// Map a kameo remote send error into a provider-pipeline [`LLMError`].
///
/// Classification is delegated to [`crate::transport_failure::classify_remote_send_error`],
/// which is the single source of truth for remote-send semantics. Duplicating
/// the match here previously let connectivity failures such as
/// `RemoteSendError::Io` degrade into `LLMError::IoError` — whose display
/// dropped the underlying cause — and made a mesh timeout look like a provider
/// rejection.
///
/// Handler errors are returned as `Err(E)` so callers keep the structured
/// provider payload.
pub fn remote_send_error_base<E: std::fmt::Display>(
    error: kameo::error::RemoteSendError<E>,
) -> Result<LLMError, E> {
    match crate::transport_failure::classify_remote_send_error(error) {
        Ok(failure) => Ok(transport_failure_to_llm_error(failure)),
        Err(handler) => Err(handler),
    }
}

/// Convert a classified remote transport failure into the provider-facing
/// error type, keeping the transport failure retryable instead of turning it
/// into a semantic rejection.
fn transport_failure_to_llm_error(
    failure: crate::transport_failure::RemoteTransportFailure,
) -> LLMError {
    use crate::transport_failure::RemoteTransportFailureKind as Kind;

    let kind = match failure.kind {
        Kind::ActorUnavailable | Kind::ConnectionClosed => TransportErrorKind::ConnectionClosed,
        Kind::DialFailure => TransportErrorKind::ConnectionRefused,
        Kind::MailboxFull => TransportErrorKind::Other,
        Kind::NetworkTimeout | Kind::ReplyTimeout => TransportErrorKind::Timeout,
        // Wire/protocol incompatibilities are never transient; replaying them
        // cannot succeed until both peers run compatible builds.
        Kind::ProtocolMismatch | Kind::Serialization => TransportErrorKind::ProtocolMismatch,
    };

    LLMError::Transport {
        kind,
        message: failure.message,
    }
}

/// Provider-streaming retry policy for remote sends.
///
/// Provider streams are resumable through stream leases, so compared with the
/// session-command policy in [`crate::transport_failure::classify_remote_send_error`]
/// more variants are safe to replay here. Protocol and serialization failures
/// stay non-retryable: they are deterministic, not transient.
pub fn should_retry_remote_send<E>(error: &kameo::error::RemoteSendError<E>) -> bool {
    use kameo::error::RemoteSendError;

    matches!(
        error,
        RemoteSendError::ActorNotRunning
            | RemoteSendError::ActorStopped
            | RemoteSendError::UnknownActor { .. }
            | RemoteSendError::DialFailure
            | RemoteSendError::ConnectionClosed
            | RemoteSendError::NetworkTimeout
            | RemoteSendError::ReplyTimeout
            | RemoteSendError::Io(_)
    )
}

pub fn remote_send_error_to_llm_error_no_handler(
    error: kameo::error::RemoteSendError<kameo::error::Infallible>,
) -> LLMError {
    match remote_send_error_base(error) {
        Ok(err) => err,
        Err(never) => match never {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::error::RemoteSendError;
    use kameo::remote::messaging::SwarmRequest;
    use querymt::chat::{ChatInputPart, ChatMessage, MediaKind, MediaPart, MediaSource};
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

    /// Build an inline PNG image input part.
    fn png_part(data: Vec<u8>) -> ChatInputPart {
        ChatInputPart::attachment(
            MediaPart::new(
                MediaKind::Image,
                Some("image/png".parse().expect("valid media type")),
                MediaSource::Inline { data },
            )
            .expect("valid inline attachment"),
        )
    }

    fn wire_bytes(request: &SwarmRequest) -> Vec<u8> {
        cbor4ii::serde::to_vec(Vec::new(), request).unwrap()
    }

    #[test]
    fn encoded_envelope_size_matches_the_real_cbor_codec() {
        let messages = vec![
            ChatMessage::from_user_parts(vec![png_part(
                (0..258_708).map(|i| (i % 256) as u8).collect(),
            )]),
            ChatMessage::from_user_parts(vec![png_part(
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
        assert!(!should_retry_remote_send::<String>(
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
        assert!(matches!(
            err,
            LLMError::Transport {
                kind: TransportErrorKind::ProtocolMismatch,
                ..
            }
        ));

        let err = remote_send_error_base::<String>(RemoteSendError::UnknownMessage {
            actor_remote_id: Cow::Borrowed("actor"),
            message_remote_id: Cow::Borrowed("message"),
        })
        .unwrap();
        assert!(matches!(
            err,
            LLMError::Transport {
                kind: TransportErrorKind::ProtocolMismatch,
                ..
            }
        ));

        let err = remote_send_error_base::<String>(RemoteSendError::SerializeReply(
            "serialize fail".to_string(),
        ))
        .unwrap();
        let LLMError::Transport {
            kind: TransportErrorKind::ProtocolMismatch,
            message,
        } = &err
        else {
            panic!("expected protocol mismatch transport error, got {err:?}")
        };
        assert!(message.contains("failed to serialize reply"));
        assert!(message.contains("serialize fail"));
        assert!(!err.is_retryable());
    }

    #[test]
    fn remote_send_error_base_preserves_handler_error() {
        let err = remote_send_error_base(RemoteSendError::HandlerError("handler boom".to_string()))
            .expect_err("handler errors should bubble up");
        assert_eq!(err, "handler boom");
    }

    #[test]
    fn wire_protocol_failures_map_to_non_retryable_protocol_mismatch() {
        let err = remote_send_error_base::<String>(RemoteSendError::DeserializeMessage(
            "invalid type: map, expected field identifier".to_string(),
        ))
        .unwrap();
        let LLMError::Transport { kind, message } = &err else {
            panic!("expected transport error, got {err:?}")
        };
        assert_eq!(*kind, TransportErrorKind::ProtocolMismatch);
        assert!(!err.is_retryable());
        let restored = LLMError::from_payload(err.to_payload());
        assert!(!restored.is_retryable());
        assert!(matches!(
            restored,
            LLMError::Transport {
                kind: TransportErrorKind::ProtocolMismatch,
                ..
            }
        ));
        assert!(message.contains("failed to deserialize message"));
        assert!(message.contains("invalid type: map, expected field identifier"));
    }

    #[test]
    fn io_failures_stay_retryable_transport_errors_with_cause() {
        // A mesh I/O failure (for example an inbound stream timing out) is a
        // connectivity problem: it must stay retryable and keep kameo's message
        // instead of degrading into an opaque provider rejection.
        let err = remote_send_error_base::<String>(RemoteSendError::Io(Some(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ))))
        .unwrap();
        let LLMError::Transport { kind, message } = &err else {
            panic!("expected transport error, got {err:?}")
        };
        assert_eq!(*kind, TransportErrorKind::ConnectionClosed);
        assert!(err.is_retryable());
        assert!(message.contains("timed out"));

        let none = remote_send_error_base::<String>(RemoteSendError::Io(None)).unwrap();
        assert!(none.is_retryable());
        assert!(matches!(none, LLMError::Transport { .. }));
    }

    #[test]
    fn connectivity_failures_stay_retryable_transport_errors() {
        let cases = [
            (RemoteSendError::NetworkTimeout, TransportErrorKind::Timeout),
            (RemoteSendError::ReplyTimeout, TransportErrorKind::Timeout),
            (
                RemoteSendError::DialFailure,
                TransportErrorKind::ConnectionRefused,
            ),
            (
                RemoteSendError::ConnectionClosed,
                TransportErrorKind::ConnectionClosed,
            ),
            (
                RemoteSendError::ActorNotRunning,
                TransportErrorKind::ConnectionClosed,
            ),
        ];
        for (error, expected_kind) in cases {
            let err = remote_send_error_base::<String>(error).unwrap();
            let LLMError::Transport { kind, .. } = &err else {
                panic!("expected transport error, got {err:?}")
            };
            assert_eq!(*kind, expected_kind);
            assert!(err.is_retryable(), "{err:?} should stay retryable");
        }
    }

    #[test]
    fn provider_stream_retry_policy_replays_connectivity_but_not_protocol_failures() {
        assert!(should_retry_remote_send::<String>(&RemoteSendError::Io(
            Some(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out"
            ))
        )));
        assert!(should_retry_remote_send::<String>(
            &RemoteSendError::NetworkTimeout
        ));
        assert!(!should_retry_remote_send::<String>(
            &RemoteSendError::DeserializeMessage("bad wire".to_string())
        ));
        assert!(!should_retry_remote_send::<String>(
            &RemoteSendError::BadActorType
        ));
    }
}
