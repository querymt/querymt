//! Typed remote transport failures with delivery-certainty classification.
//!
//! Remote send errors must not be flattened to strings before the operation
//! layer can decide whether a retry is safe. This module provides the shared
//! classification used by session commands, node-manager calls, and any other
//! capability-bearing remote RPCs.
//!
//! # Classification rules (kameo 0.22 `RemoteSendError`)
//!
//! | Variant                     | Kind              | Delivery      | Rationale                                                        |
//! |-----------------------------|-------------------|---------------|------------------------------------------------------------------|
//! | `HandlerError(E)`           | — (returned as-`E`) | delivered   | the remote handler ran and produced this error; never a transport failure |
//! | `ActorNotRunning`           | `ActorUnavailable` | `NotDelivered` | actor registry proves the actor is not running; handler cannot have been invoked |
//! | `ActorStopped`              | `ActorUnavailable` | `Unknown`      | actor may have stopped mid-processing before replying            |
//! | `UnknownActor`              | `ActorUnavailable` | `NotDelivered` | remote ID not found on either side; no handler invocation possible |
//! | `UnknownMessage`            | `ProtocolMismatch` | `NotDelivered` | actor has no handler for this message; never invoked             |
//! | `BadActorType`              | `ProtocolMismatch` | `NotDelivered` | ID resolved to a different actor type; never invoked             |
//! | `UnsupportedProtocols`      | `ProtocolMismatch` | `NotDelivered` | peer supports none of the requested protocols; request never sent |
//! | `MailboxFull`               | `MailboxFull`      | `NotDelivered` | mailbox timeout expired waiting to enqueue; message never enqueued |
//! | `DialFailure`               | `DialFailure`      | `NotDelivered` | connection could not be established; nothing left this node      |
//! | `SwarmNotBootstrapped`      | `DialFailure`      | `NotDelivered` | no remote network stack available to send with                   |
//! | `NetworkTimeout`            | `NetworkTimeout`   | `Unknown`      | request may have been received and processed (kameo doc)         |
//! | `ConnectionClosed`          | `ConnectionClosed` | `Unknown`      | connection closed in flight; processing state unknowable         |
//! | `ReplyTimeout`              | `ReplyTimeout`     | `Unknown`      | reply wait expired; handler may have completed                   |
//! | `SerializeMessage`          | `Serialization`    | `NotDelivered` | local encode failed before send                                  |
//! | `DeserializeMessage`        | `Serialization`    | `NotDelivered` | remote decode failed; handler never invoked (still non-retryable: retrying cannot help) |
//! | `SerializeReply`            | `Serialization`    | `Unknown`      | handler ran; reply encode failed                                 |
//! | `SerializeHandlerError`     | `Serialization`    | `Unknown`      | handler ran and errored; error encode failed                     |
//! | `DeserializeHandlerError`   | `Serialization`    | `Unknown`      | handler ran and errored; error decode failed                     |
//! | `Io(_)`                     | `ConnectionClosed` | `Unknown`      | IO failure on an outbound stream; in-flight state unknowable     |
//!
//! Conservative defaults:
//!
//! - `NotDelivered` is claimed only when the transport contract proves failure
//!   happened before remote enqueue or handler invocation.
//! - Timeouts and connection closure are always `Unknown`.
//! - [`RemoteTransportFailureKind::ProtocolMismatch`] and
//!   [`RemoteTransportFailureKind::Serialization`] are never worth retrying:
//!   replaying produces the same outcome regardless of connectivity.
//! - The match is exhaustive on purpose: a new kameo variant must be
//!   classified explicitly (compiler-enforced) instead of silently defaulting.
//!
//! # Separation from provider-stream retries
//!
//! `provider_transport::should_retry_remote_send` encodes the permissive
//! *provider streaming* retry policy (stream leases make provider requests
//! resumable, so more variants are safe to replay there). That policy must NOT
//! be reused for session commands; use [`classify_remote_send_error`] plus the
//! operation layer's safety metadata instead.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Whether the transport contract proves the remote side never enqueued or
/// invoked the message. Conservative: only failures with a local proof of
/// pre-delivery may claim [`DeliveryCertainty::NotDelivered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryCertainty {
    /// Proven the message never reached remote enqueue/invocation. Safe input
    /// for a one-shot retry decision after recovery.
    NotDelivered,
    /// The remote side may or may not have processed the message. Non
    /// idempotent operations must not be replayed after such a failure.
    Unknown,
}

impl DeliveryCertainty {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotDelivered => "not_delivered",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for DeliveryCertainty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Transport-level failure category for remote RPCs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTransportFailureKind {
    /// The remote actor is not available (not running, stopped, or unknown ID).
    ActorUnavailable,
    /// No connection to the peer could be established (dial or missing swarm).
    DialFailure,
    /// The connection or outbound stream failed mid-request.
    ConnectionClosed,
    /// The remote mailbox did not accept the message before timeout.
    MailboxFull,
    /// The request timed out on the network before a response was received.
    NetworkTimeout,
    /// The reply wait timed out; the handler may still have completed.
    ReplyTimeout,
    /// Actor/message/protocol incompatibility. Never transient.
    ProtocolMismatch,
    /// Serialize/deserialize failure. Never transient.
    Serialization,
}

impl RemoteTransportFailureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ActorUnavailable => "actor_unavailable",
            Self::DialFailure => "dial_failure",
            Self::ConnectionClosed => "connection_closed",
            Self::MailboxFull => "mailbox_full",
            Self::NetworkTimeout => "network_timeout",
            Self::ReplyTimeout => "reply_timeout",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::Serialization => "serialization",
        }
    }
}

impl fmt::Display for RemoteTransportFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A classified remote transport failure.
///
/// `message` carries the human-readable detail (kameo's original display text)
/// so user-facing errors stay unchanged at the textual level; `kind` and
/// `delivery` carry the machine-decidable semantics the operation layer
/// needs. Callers must never parse `message` to make routing decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTransportFailure {
    pub kind: RemoteTransportFailureKind,
    pub delivery: DeliveryCertainty,
    pub message: String,
}

impl RemoteTransportFailure {
    pub fn new(
        kind: RemoteTransportFailureKind,
        delivery: DeliveryCertainty,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            delivery,
            message: message.into(),
        }
    }

    /// True when the transport proven the message was never enqueued or
    /// invoked on the remote side.
    pub fn proven_not_delivered(&self) -> bool {
        self.delivery == DeliveryCertainty::NotDelivered
    }

    /// True when replaying this message after a reconnect could possibly
    /// succeed. Protocol mismatches and serialization failures cannot be
    /// fixed by reconnecting, so they are never retry candidates.
    pub fn is_retryable_kind(&self) -> bool {
        !matches!(
            self.kind,
            RemoteTransportFailureKind::ProtocolMismatch
                | RemoteTransportFailureKind::Serialization
        )
    }
}

impl fmt::Display for RemoteTransportFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "remote transport failure (kind={}, delivery={}): {}",
            self.kind, self.delivery, self.message
        )
    }
}

impl std::error::Error for RemoteTransportFailure {}

/// Classify a kameo remote send error.
///
/// Handler errors are returned as `Err(E)`: they are *delivered* errors from
/// the remote handler and must be preserved separately from transport
/// failures (callers, e.g. session control, depend on structured handler
/// payloads such as provider errors and validation rejections).
pub fn classify_remote_send_error<E: fmt::Display>(
    error: kameo::error::RemoteSendError<E>,
) -> Result<RemoteTransportFailure, E> {
    use DeliveryCertainty::{NotDelivered, Unknown};
    use RemoteTransportFailureKind as Kind;
    use kameo::error::RemoteSendError as RSE;

    match error {
        RSE::HandlerError(err) => Err(err),
        other => {
            let (kind, delivery) = match &other {
                RSE::ActorNotRunning | RSE::UnknownActor { .. } => {
                    (Kind::ActorUnavailable, NotDelivered)
                }
                RSE::ActorStopped => (Kind::ActorUnavailable, Unknown),
                RSE::UnknownMessage { .. } | RSE::BadActorType | RSE::UnsupportedProtocols => {
                    (Kind::ProtocolMismatch, NotDelivered)
                }
                RSE::MailboxFull => (Kind::MailboxFull, NotDelivered),
                RSE::ReplyTimeout => (Kind::ReplyTimeout, Unknown),
                RSE::SerializeMessage(_) | RSE::DeserializeMessage(_) => {
                    (Kind::Serialization, NotDelivered)
                }
                RSE::SerializeReply(_)
                | RSE::SerializeHandlerError(_)
                | RSE::DeserializeHandlerError(_) => (Kind::Serialization, Unknown),
                RSE::SwarmNotBootstrapped | RSE::DialFailure => (Kind::DialFailure, NotDelivered),
                RSE::NetworkTimeout => (Kind::NetworkTimeout, Unknown),
                RSE::ConnectionClosed | RSE::Io(_) => (Kind::ConnectionClosed, Unknown),
                RSE::HandlerError(_) => {
                    unreachable!("handler errors are returned before classification")
                }
            };
            Ok(RemoteTransportFailure {
                kind,
                delivery,
                message: other.to_string(),
            })
        }
    }
}

/// Classify a remote send error whose handler error type is `Infallible`.
///
/// Convenience wrapper for fire-and-forget messages that cannot produce a
/// handler error.
pub fn classify_infallible_remote_send_error(
    error: kameo::error::RemoteSendError<kameo::error::Infallible>,
) -> RemoteTransportFailure {
    match classify_remote_send_error(error) {
        Ok(failure) => failure,
        Err(never) => match never {},
    }
}

/// Classify a remote send error and override the message for reply timeouts.
///
/// Session-control callers know which operation timed out (and the configured
/// bounds); that context is more useful than the generic kameo text. Delivery
/// certainty and kind are unaffected — a reply timeout stays `Unknown`.
pub fn classify_remote_send_error_with_timeout_message<E: fmt::Display>(
    error: kameo::error::RemoteSendError<E>,
    timeout_message: impl Into<String>,
) -> Result<RemoteTransportFailure, E> {
    let timeout_message = timeout_message.into();
    classify_remote_send_error(error).map(|mut failure| {
        if failure.kind == RemoteTransportFailureKind::ReplyTimeout {
            failure.message = timeout_message;
        }
        failure
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::error::RemoteSendError;
    use std::borrow::Cow;

    type Klass = RemoteTransportFailureKind;
    type Cert = DeliveryCertainty;

    fn classify<E>(e: RemoteSendError<E>) -> (Klass, Cert)
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        let failure =
            classify_remote_send_error(e).expect("expected a transport failure, got handler error");
        (failure.kind, failure.delivery)
    }

    #[test]
    fn handler_errors_pass_through_unmodified() {
        let err = classify_remote_send_error(RemoteSendError::HandlerError("handler boom"))
            .expect_err("handler errors must not be classified as transport failures");
        assert_eq!(err, "handler boom");
    }

    #[test]
    fn actor_unavailable_classification() {
        assert_eq!(
            classify::<String>(RemoteSendError::ActorNotRunning),
            (Klass::ActorUnavailable, Cert::NotDelivered)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::UnknownActor {
                actor_remote_id: Cow::Borrowed("session.abc"),
            }),
            (Klass::ActorUnavailable, Cert::NotDelivered)
        );
        // Conservative: actor may have stopped mid-processing.
        assert_eq!(
            classify::<String>(RemoteSendError::ActorStopped),
            (Klass::ActorUnavailable, Cert::Unknown)
        );
    }

    #[test]
    fn protocol_mismatch_classification_not_delivered_non_retryable() {
        assert_eq!(
            classify::<String>(RemoteSendError::UnknownMessage {
                actor_remote_id: Cow::Borrowed("session.abc"),
                message_remote_id: Cow::Borrowed("Prompt"),
            }),
            (Klass::ProtocolMismatch, Cert::NotDelivered)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::BadActorType),
            (Klass::ProtocolMismatch, Cert::NotDelivered)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::UnsupportedProtocols),
            (Klass::ProtocolMismatch, Cert::NotDelivered)
        );

        let failure = classify_remote_send_error::<String>(RemoteSendError::BadActorType)
            .expect("transport failure");
        assert!(!failure.is_retryable_kind());
        assert!(failure.proven_not_delivered());
    }

    #[test]
    fn mailbox_full_is_pre_enqueue_failure() {
        assert_eq!(
            classify::<String>(RemoteSendError::MailboxFull),
            (Klass::MailboxFull, Cert::NotDelivered)
        );
    }

    #[test]
    fn dial_failures_are_pre_send() {
        assert_eq!(
            classify::<String>(RemoteSendError::DialFailure),
            (Klass::DialFailure, Cert::NotDelivered)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::SwarmNotBootstrapped),
            (Klass::DialFailure, Cert::NotDelivered)
        );
    }

    #[test]
    fn timeouts_and_lost_connection_are_ambiguous() {
        assert_eq!(
            classify::<String>(RemoteSendError::ReplyTimeout),
            (Klass::ReplyTimeout, Cert::Unknown)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::NetworkTimeout),
            (Klass::NetworkTimeout, Cert::Unknown)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::ConnectionClosed),
            (Klass::ConnectionClosed, Cert::Unknown)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::Io(Some(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pipe"
            )))),
            (Klass::ConnectionClosed, Cert::Unknown)
        );

        for e in [
            RemoteSendError::<String>::ReplyTimeout,
            RemoteSendError::NetworkTimeout,
            RemoteSendError::ConnectionClosed,
            RemoteSendError::Io(None),
        ] {
            let failure = classify_remote_send_error(e).expect("transport failure");
            assert!(!failure.proven_not_delivered());
            assert!(failure.is_retryable_kind());
        }
    }

    #[test]
    fn serialization_classification() {
        // Pre-send local encode failure and remote decode failure: the handler
        // was never invoked, but retrying cannot change the outcome.
        assert_eq!(
            classify::<String>(RemoteSendError::SerializeMessage("bad msg".to_string())),
            (Klass::Serialization, Cert::NotDelivered)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::DeserializeMessage("bad msg".to_string())),
            (Klass::Serialization, Cert::NotDelivered)
        );
        // Reply/error encode/decode failures happen after the handler ran.
        assert_eq!(
            classify::<String>(RemoteSendError::SerializeReply("bad reply".to_string())),
            (Klass::Serialization, Cert::Unknown)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::SerializeHandlerError("bad".to_string())),
            (Klass::Serialization, Cert::Unknown)
        );
        assert_eq!(
            classify::<String>(RemoteSendError::DeserializeHandlerError("bad".to_string())),
            (Klass::Serialization, Cert::Unknown)
        );

        let failure = classify_remote_send_error::<String>(RemoteSendError::SerializeMessage(
            "m".to_string(),
        ))
        .expect("transport failure");
        assert!(!failure.is_retryable_kind());
    }

    #[test]
    fn message_preserves_original_display_text() {
        let failure = classify_remote_send_error::<String>(RemoteSendError::ConnectionClosed)
            .expect("transport failure");
        assert!(!failure.message.is_empty());
        assert_eq!(
            failure.message,
            RemoteSendError::<String>::ConnectionClosed.to_string()
        );
    }

    #[test]
    fn infallible_wrapper_never_returns_handler_error() {
        let failure = classify_infallible_remote_send_error(RemoteSendError::ReplyTimeout);
        assert_eq!(failure.kind, Klass::ReplyTimeout);
        assert_eq!(failure.delivery, Cert::Unknown);
    }

    #[test]
    fn timeout_message_override_only_applies_to_reply_timeout() {
        let failure = classify_remote_send_error_with_timeout_message::<String>(
            RemoteSendError::ReplyTimeout,
            "SubmitInput timed out on remote session",
        )
        .expect("transport failure");
        assert_eq!(failure.kind, Klass::ReplyTimeout);
        assert_eq!(failure.delivery, Cert::Unknown);
        assert_eq!(failure.message, "SubmitInput timed out on remote session");

        let failure = classify_remote_send_error_with_timeout_message::<String>(
            RemoteSendError::ConnectionClosed,
            "SubmitInput timed out on remote session",
        )
        .expect("transport failure");
        assert_eq!(failure.kind, Klass::ConnectionClosed);
        assert_ne!(failure.message, "SubmitInput timed out on remote session");
    }

    #[test]
    fn serde_roundtrip_uses_stable_snake_case_codes() {
        let failure = RemoteTransportFailure::new(
            RemoteTransportFailureKind::ConnectionClosed,
            DeliveryCertainty::Unknown,
            "lost link",
        );
        let json = serde_json::to_value(&failure).unwrap();
        assert_eq!(json["kind"], "connection_closed");
        assert_eq!(json["delivery"], "unknown");
        assert_eq!(json["message"], "lost link");

        let parsed: RemoteTransportFailure = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, failure);
    }

    #[test]
    fn display_format_is_stable_for_logs_and_user_messages() {
        let failure = RemoteTransportFailure::new(
            RemoteTransportFailureKind::ReplyTimeout,
            DeliveryCertainty::Unknown,
            "SubmitInput timed out on remote session",
        );
        assert_eq!(
            failure.to_string(),
            "remote transport failure (kind=reply_timeout, delivery=unknown): SubmitInput timed out on remote session"
        );
    }
}
