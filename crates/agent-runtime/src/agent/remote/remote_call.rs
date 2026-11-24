//! The one client path for asking a remote actor and translating its failure.
//!
//! Every outbound ask negotiates the sharing protocol with the owner, so a
//! guarded owner applies the caller's actual grant instead of the legacy
//! "full grant only" rule. Every failure is translated by [`RemoteCallError`],
//! so an access refusal or a transport failure looks the same on every path.

use crate::agent::file_proxy::FileProxyError;
use crate::agent::undo::UndoError;
use crate::error::AgentError;
use kameo::actor::RemoteActorRef;
use kameo::message::Message;
use kameo::remote::{RemoteActor, RemoteMessage};
use kameo::reply::Reply;
use querymt_remote::request_timeout::{RemoteAskFailure, ask_with_sharing_timeouts};
use querymt_remote::{MeshHandle, RemoteTransportFailure, RemoteTransportFailureKind};
use std::time::Duration;

/// Mailbox and reply bounds of one remote ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallTimeouts {
    pub(crate) mailbox: Duration,
    pub(crate) reply: Duration,
}

impl CallTimeouts {
    /// The same bound for mailbox and reply.
    pub(crate) const fn uniform(timeout: Duration) -> Self {
        Self::new(timeout, timeout)
    }

    pub(crate) const fn new(mailbox: Duration, reply: Duration) -> Self {
        Self { mailbox, reply }
    }
}

/// An error type that can report a failed remote ask.
pub(crate) trait RemoteCallError: Sized {
    /// The owner's access policy refused the request.
    fn access_denied() -> Self;

    /// The request failed in transport.
    fn transport(failure: RemoteTransportFailure) -> Self;

    /// Translate a failed ask whose remote handler error converts into `Self`.
    fn from_remote<E: Into<Self>>(failure: RemoteAskFailure<E>) -> Self {
        match failure {
            RemoteAskFailure::Handler(error) => error.into(),
            RemoteAskFailure::AccessDenied => Self::access_denied(),
            RemoteAskFailure::Transport(failure) => Self::transport(failure),
        }
    }
}

impl RemoteCallError for AgentError {
    fn access_denied() -> Self {
        Self::MeshAccessDenied
    }

    fn transport(failure: RemoteTransportFailure) -> Self {
        Self::from_transport_failure(failure)
    }
}

impl RemoteCallError for agent_client_protocol::Error {
    fn access_denied() -> Self {
        AgentError::MeshAccessDenied.into()
    }

    fn transport(failure: RemoteTransportFailure) -> Self {
        AgentError::from_transport_failure(failure).into()
    }
}

impl RemoteCallError for FileProxyError {
    fn access_denied() -> Self {
        Self::AccessDenied
    }

    fn transport(failure: RemoteTransportFailure) -> Self {
        Self::from_transport_failure(failure)
    }
}

impl RemoteCallError for UndoError {
    fn access_denied() -> Self {
        Self::AccessDenied
    }

    fn transport(failure: RemoteTransportFailure) -> Self {
        Self::from_transport_failure(failure)
    }
}

/// Ask a remote actor over the protocol negotiated with its owner.
///
/// Without a mesh handle (a bare reference built in tests) the plain Kameo
/// ask is used. A reply timeout names the message and its bounds.
///
/// # Errors
///
/// Returns the remote handler error, an access refusal or a typed transport
/// failure, all translated into `T`.
pub(crate) async fn ask_remote<A, M, T>(
    mesh: Option<&MeshHandle>,
    actor: &RemoteActorRef<A>,
    message: &M,
    timeouts: CallTimeouts,
) -> Result<<A::Reply as Reply>::Ok, T>
where
    A: kameo::Actor + Message<M> + RemoteActor + RemoteMessage<M>,
    M: serde::Serialize + Send + Sync + 'static,
    <A::Reply as Reply>::Ok: serde::de::DeserializeOwned,
    <A::Reply as Reply>::Error:
        serde::de::DeserializeOwned + std::fmt::Debug + std::fmt::Display + Into<T>,
    T: RemoteCallError,
{
    let result = match mesh {
        Some(mesh) => {
            ask_with_sharing_timeouts(mesh, actor, message, timeouts.mailbox, timeouts.reply)
                .await
                .map_err(RemoteAskFailure::from)
        }
        None => actor
            .ask(message)
            .mailbox_timeout(timeouts.mailbox)
            .reply_timeout(timeouts.reply)
            .send()
            .await
            .map_err(RemoteAskFailure::from),
    };
    result.map_err(|mut failure| {
        let message_id = <A as RemoteMessage<M>>::REMOTE_ID;
        if let RemoteAskFailure::Transport(transport) = &mut failure {
            if transport.kind == RemoteTransportFailureKind::ReplyTimeout {
                transport.message = format!(
                    "{message_id} timed out (mailbox={}s, reply={}s)",
                    timeouts.mailbox.as_secs(),
                    timeouts.reply.as_secs()
                );
            }
            tracing::debug!(
                message = message_id,
                kind = %transport.kind,
                delivery = %transport.delivery,
                error = %transport.message,
                "remote ask failed in transport"
            );
        } else if matches!(failure, RemoteAskFailure::AccessDenied) {
            tracing::debug!(message = message_id, "remote ask refused by owner policy");
        }
        T::from_remote(failure)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::error::RemoteSendError;
    use querymt_remote::DeliveryCertainty;
    use querymt_remote::mesh_ingress::{SharingCallError, SharingRejection};
    use querymt_remote::request_timeout::SharingAskError;

    fn translate(error: SharingAskError<AgentError>) -> AgentError {
        AgentError::from_remote(error.into())
    }

    #[test]
    fn handler_errors_pass_through_unchanged() {
        let error = translate(SharingAskError::Actor(RemoteSendError::HandlerError(
            AgentError::SessionTimeout {
                details: "handler-reported timeout".into(),
            },
        )));
        assert!(
            matches!(&error, AgentError::SessionTimeout { details } if details == "handler-reported timeout")
        );
    }

    #[test]
    fn access_refusals_are_typed_on_both_protocols() {
        let legacy = translate(SharingAskError::Actor(RemoteSendError::DeserializeMessage(
            "mesh access denied".into(),
        )));
        let v1 = translate(SharingAskError::Sharing(SharingCallError::Rejected(
            SharingRejection::Changed,
        )));
        assert!(matches!(legacy, AgentError::MeshAccessDenied));
        assert!(matches!(v1, AgentError::MeshAccessDenied));
        assert!(matches!(
            UndoError::from_remote::<UndoError>(RemoteAskFailure::AccessDenied),
            UndoError::AccessDenied
        ));
    }

    #[test]
    fn transport_failures_keep_kind_and_delivery() {
        let unknown = translate(SharingAskError::Actor(RemoteSendError::UnknownMessage {
            actor_remote_id: "querymt::RemoteNodeManager".into(),
            message_remote_id: "querymt::ListRemoteProfiles".into(),
        }));
        let failure = unknown
            .transport_failure()
            .expect("typed transport failure");
        assert_eq!(failure.kind, RemoteTransportFailureKind::ProtocolMismatch);
        assert!(failure.proven_not_delivered());

        let busy = translate(SharingAskError::Sharing(SharingCallError::Rejected(
            SharingRejection::Busy,
        )));
        let failure = busy.transport_failure().expect("typed transport failure");
        assert_eq!(failure.kind, RemoteTransportFailureKind::MailboxFull);
        assert_eq!(failure.delivery, DeliveryCertainty::NotDelivered);
    }
}
