use querymt::error::{LLMError, LLMErrorPayload, TransportErrorKind};

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
