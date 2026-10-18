use crate::session::error::{SessionError, SessionResult};
use crate::session::lifecycle::{SessionLifecycle, SessionLifecyclePatch};
use crate::session::store::SessionStore;

pub(crate) async fn read_stored_session_lifecycle(
    store: &dyn SessionStore,
    session_id: &str,
) -> SessionResult<SessionLifecycle> {
    validate_session_id(session_id)?;
    ensure_lifecycle_support(store)?;
    store.get_session_lifecycle(session_id).await
}

pub(crate) async fn patch_stored_session_lifecycle(
    store: &dyn SessionStore,
    session_id: &str,
    patch: &SessionLifecyclePatch,
) -> SessionResult<SessionLifecycle> {
    validate_session_id(session_id)?;
    patch.validate()?;
    ensure_lifecycle_support(store)?;
    store.patch_session_lifecycle(session_id, patch).await
}

fn validate_session_id(session_id: &str) -> SessionResult<()> {
    if session_id.is_empty() {
        return Err(SessionError::InvalidOperation(
            "session id must not be empty".into(),
        ));
    }
    Ok(())
}

fn ensure_lifecycle_support(store: &dyn SessionStore) -> SessionResult<()> {
    if !store.supports_session_lifecycle() {
        return Err(SessionError::InvalidOperation(
            "Session lifecycle is unsupported by this store".into(),
        ));
    }
    Ok(())
}

impl super::LocalAgentHandle {
    pub(crate) fn supports_session_lifecycle_contract(&self) -> bool {
        let session_store = self.config.provider.history_store();
        let Some(view_store) = self.config.storage.view_store() else {
            return false;
        };
        session_store.supports_session_lifecycle()
            && view_store.supports_session_lifecycle()
            && view_store.supports_session_lifecycle_filtering()
    }

    pub(crate) async fn get_session_lifecycle_unloaded(
        &self,
        session_id: &str,
    ) -> SessionResult<SessionLifecycle> {
        let store = self.config.provider.history_store();
        validate_session_id(session_id)?;
        match crate::session::location::resolve_session_location(store.as_ref(), session_id).await?
        {
            crate::session::location::SessionLocation::Conflict { .. } => {
                Err(SessionError::InvalidOperation(format!(
                    "Session location conflict: {session_id} exists locally and as a remote bookmark"
                )))
            }
            crate::session::location::SessionLocation::Local
            | crate::session::location::SessionLocation::NotFound => {
                read_stored_session_lifecycle(store.as_ref(), session_id).await
            }
            crate::session::location::SessionLocation::Remote { bookmark } => {
                #[cfg(feature = "remote")]
                {
                    let node_manager = self
                        .find_node_manager(&bookmark.node_id)
                        .await
                        .map_err(remote_lifecycle_error)?;
                    self.get_remote_session_lifecycle(&node_manager, session_id.to_owned())
                        .await
                        .map_err(remote_lifecycle_error)
                }
                #[cfg(not(feature = "remote"))]
                {
                    let _ = bookmark;
                    Err(SessionError::InvalidOperation(
                        "Remote session lifecycle requires the remote feature".into(),
                    ))
                }
            }
        }
    }

    pub(crate) async fn patch_session_lifecycle_unloaded(
        &self,
        session_id: &str,
        patch: &SessionLifecyclePatch,
    ) -> SessionResult<SessionLifecycle> {
        let store = self.config.provider.history_store();
        validate_session_id(session_id)?;
        match crate::session::location::resolve_session_location(store.as_ref(), session_id).await?
        {
            crate::session::location::SessionLocation::Conflict { .. } => {
                Err(SessionError::InvalidOperation(format!(
                    "Session location conflict: {session_id} exists locally and as a remote bookmark"
                )))
            }
            crate::session::location::SessionLocation::Local
            | crate::session::location::SessionLocation::NotFound => {
                patch_stored_session_lifecycle(store.as_ref(), session_id, patch).await
            }
            crate::session::location::SessionLocation::Remote { bookmark } => {
                #[cfg(feature = "remote")]
                {
                    let node_manager = self
                        .find_node_manager(&bookmark.node_id)
                        .await
                        .map_err(remote_lifecycle_error)?;
                    self.patch_remote_session_lifecycle(
                        &node_manager,
                        session_id.to_owned(),
                        patch.clone(),
                    )
                    .await
                    .map_err(remote_lifecycle_error)
                }
                #[cfg(not(feature = "remote"))]
                {
                    let _ = bookmark;
                    Err(SessionError::InvalidOperation(
                        "Remote session lifecycle requires the remote feature".into(),
                    ))
                }
            }
        }
    }
}

#[cfg(feature = "remote")]
fn remote_lifecycle_error(error: agent_client_protocol::Error) -> SessionError {
    SessionError::InvalidOperation(format!(
        "Remote session lifecycle owner request failed: {error}"
    ))
}
