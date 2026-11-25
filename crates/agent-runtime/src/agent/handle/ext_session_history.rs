//! `querymt/session/history` (older history page) and
//! `querymt/session/toolContent` (complete tool result / arguments).
//!
//! Both read the event journal, for local and remote sessions alike. For a
//! remote session the attaching agent first transfers any part of the needed
//! range that is not local yet, so each event still crosses the mesh once.

use super::utils::ext_json_response;
use super::*;
use crate::session::history_window::{
    HistoryInfo, LoadWindow, ToolContentPart, TruncatedEntry, find_tool_content,
    parse_history_token, read_window,
};
use crate::session::projection::EventJournal;
use crate::session::store::RemoteSessionBookmark;
use querymt_agent_core::events::AgentEvent;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// Request for the turns preceding a position reported by a load or a
/// previous older-history response.
#[typeshare]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionHistoryRequest {
    #[serde(alias = "session_id")]
    session_id: String,
    /// The `history.before` token of the previous response.
    before: String,
    #[serde(default, alias = "load_window")]
    load_window: Option<LoadWindow>,
}

#[typeshare]
#[derive(Debug, Serialize)]
struct SessionHistoryResponse {
    /// Events whose `seq` is their history position, ascending.
    events: Vec<AgentEvent>,
    history: HistoryInfo,
    truncated: Vec<TruncatedEntry>,
    /// Structured prompt blocks for the prompts among `events` (remote
    /// sessions; same shape as the load snapshot's `userPrompts`).
    #[serde(rename = "userPrompts", skip_serializing_if = "Option::is_none")]
    user_prompts: Option<Vec<crate::session::load_snapshot::UserPromptRecord>>,
    /// The `cursor_update` of every position `events` anchor, with the ids a
    /// load snapshot and history replay announce. The transport drops it for a
    /// client that did not advertise `session.cursors`.
    #[serde(rename = "cursorUpdates", skip_serializing_if = "Vec::is_empty")]
    #[typeshare(serialized_as = "any")]
    cursor_updates: Vec<crate::acp::protocol::CursorUpdate>,
}

#[typeshare]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionToolContentRequest {
    #[serde(alias = "session_id")]
    session_id: String,
    #[serde(alias = "tool_call_id")]
    tool_call_id: String,
    part: ToolContentPart,
}

#[typeshare]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionToolContentResponse {
    session_id: String,
    tool_call_id: String,
    part: ToolContentPart,
    content: String,
}

struct HistorySource {
    journal: Arc<dyn EventJournal>,
    bookmark: Option<RemoteSessionBookmark>,
}

impl HistorySource {
    fn node(&self) -> Option<&str> {
        self.bookmark
            .as_ref()
            .map(|bookmark| bookmark.node_id.as_str())
    }
}

fn coded_error(code: i32, name: &str, session_id: &str, message: String) -> Error {
    Error::new(code, message.clone()).data(serde_json::json!({
        "code": name,
        "message": message,
        "sessionId": session_id,
    }))
}

fn session_not_found(session_id: &str) -> Error {
    coded_error(
        -32002,
        "session_not_found",
        session_id,
        format!("session {session_id} not found"),
    )
}

fn history_changed(session_id: &str) -> Error {
    coded_error(
        -32602,
        "history_changed",
        session_id,
        "the session history changed since this position was reported".into(),
    )
}

fn internal(session_id: &str, message: impl ToString) -> Error {
    Error::internal_error().data(serde_json::json!({
        "message": message.to_string(),
        "sessionId": session_id,
    }))
}

fn parse_request<T: serde::de::DeserializeOwned>(req: ExtRequest) -> Result<T, Error> {
    serde_json::from_str(req.params.get())
        .map_err(|e| Error::invalid_params().data(serde_json::json!({ "error": e.to_string() })))
}

impl LocalAgentHandle {
    /// Where a session's journal lives: remote sessions through their
    /// bookmark, local ones through the handle that owns them.
    async fn history_source(&self, session_id: &str) -> Result<HistorySource, Error> {
        let bookmark = self
            .config
            .provider
            .history_store()
            .get_remote_session_bookmark(session_id)
            .await
            .map_err(|error| internal(session_id, error))?;
        if bookmark.is_some() {
            return Ok(HistorySource {
                journal: self.config.event_sink.journal().clone(),
                bookmark,
            });
        }
        let profile_handle = self.profile_session_op_handle(session_id).await?;
        let handle: &LocalAgentHandle = profile_handle.as_deref().unwrap_or(self);
        let exists = handle
            .config
            .provider
            .history_store()
            .get_session(session_id)
            .await
            .map_err(|error| internal(session_id, error))?
            .is_some();
        if !exists {
            return Err(session_not_found(session_id));
        }
        Ok(HistorySource {
            journal: handle.config.event_sink.journal().clone(),
            bookmark: None,
        })
    }

    async fn history_epoch_of(
        &self,
        session_id: &str,
        source: &HistorySource,
    ) -> Result<u64, Error> {
        match source.node() {
            Some(node) => source
                .journal
                .remote_history_epoch(session_id, node)
                .await
                .map(|epoch| epoch.unwrap_or(0)),
            None => source.journal.history_epoch(session_id).await,
        }
        .map_err(|error| internal(session_id, error))
    }

    pub(super) async fn handle_ext_session_history(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: SessionHistoryRequest = parse_request(req)?;
        let session_id = parsed.session_id.as_str();
        let Some((token_epoch, position)) = parse_history_token(&parsed.before) else {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "invalid history token",
                "sessionId": session_id,
            })));
        };
        let source = self.history_source(session_id).await?;
        let epoch = self.history_epoch_of(session_id, &source).await?;
        if epoch != token_epoch {
            return Err(history_changed(session_id));
        }
        let window = parsed.load_window.unwrap_or_default().clamped();

        let (read, remote_deadline) = match source.bookmark.as_ref() {
            #[cfg(feature = "remote")]
            Some(bookmark) => {
                self.read_remote_history_page(
                    session_id, bookmark, &source, position, epoch, &window,
                )
                .await?
            }
            _ => (
                read_window(
                    source.journal.as_ref(),
                    session_id,
                    source.node(),
                    Some(position),
                    epoch,
                    &window,
                )
                .await
                .map_err(|error| internal(session_id, error))?,
                None::<tokio::time::Instant>,
            ),
        };
        // A rewrite while the range was being transferred invalidates it.
        if self.history_epoch_of(session_id, &source).await? != token_epoch {
            return Err(history_changed(session_id));
        }
        #[cfg(feature = "remote")]
        let user_prompts = match remote_deadline {
            Some(deadline) if tokio::time::Instant::now() < deadline => Some(
                self.remote_prompt_records_for(
                    session_id,
                    &read.events,
                    deadline.min(tokio::time::Instant::now() + std::time::Duration::from_secs(10)),
                )
                .await?,
            ),
            _ => None,
        };
        #[cfg(not(feature = "remote"))]
        let user_prompts = {
            let _ = remote_deadline;
            None
        };
        // Always with compaction cursors; the transport narrows them to the
        // connection's capabilities (`acp::shared::scope_cursor_updates`).
        let cursor_updates = querymt_agent_core::replay::session_cursor_updates(&read.events, true);
        ext_json_response(&SessionHistoryResponse {
            cursor_updates,
            events: read.events,
            history: read.history,
            truncated: read.truncated,
            user_prompts,
        })
    }

    /// Read the page before `position` of a remote session, transferring the
    /// uncovered part of its range first (the window never spans a gap).
    #[cfg(feature = "remote")]
    async fn read_remote_history_page(
        &self,
        session_id: &str,
        bookmark: &RemoteSessionBookmark,
        source: &HistorySource,
        position: i64,
        epoch: u64,
        window: &LoadWindow,
    ) -> Result<
        (
            crate::session::history_window::WindowRead,
            Option<tokio::time::Instant>,
        ),
        Error,
    > {
        use crate::agent::remote::event_backfill::WINDOW_CHUNK_EVENTS;
        let (sync, deadline) = self.remote_history_sync(session_id, bookmark).await?;
        let state = sync.state().clone();
        let mut span = WINDOW_CHUNK_EVENTS as i64;
        loop {
            let read = read_window(
                source.journal.as_ref(),
                session_id,
                source.node(),
                Some(position),
                epoch,
                window,
            )
            .await
            .map_err(|error| internal(session_id, error))?;
            let end = position - 1;
            let needed_start = if read.history.complete {
                1
            } else {
                read.events.first().map_or(position, |event| event.seq)
            };
            if end < 1 || needed_start > end || state.is_covered(needed_start, end) {
                if state.history_denied() {
                    return Err(crate::error::AgentError::MeshAccessDenied.into());
                }
                return Ok((read, Some(deadline)));
            }
            let start = (position - span).max(1);
            let fetched = tokio::time::timeout_at(deadline, sync.ensure_covered(start, end)).await;
            match fetched {
                Ok(Ok(())) => {}
                Ok(Err(crate::error::AgentError::MeshAccessDenied)) => {
                    state.note_history_denial();
                    return Err(crate::error::AgentError::MeshAccessDenied.into());
                }
                Ok(Err(error)) => {
                    if self.history_epoch_of(session_id, source).await? != epoch {
                        return Err(history_changed(session_id));
                    }
                    return Err(transfer_failed(session_id, error.to_string()));
                }
                Err(_) => {
                    return Err(transfer_failed(
                        session_id,
                        "history transfer timed out".into(),
                    ));
                }
            }
            // Re-read: the window may now start earlier than first estimated.
            span += WINDOW_CHUNK_EVENTS as i64;
        }
    }

    #[cfg(feature = "remote")]
    async fn remote_history_sync(
        &self,
        session_id: &str,
        bookmark: &RemoteSessionBookmark,
    ) -> Result<
        (
            crate::agent::remote::event_backfill::RemoteSync,
            tokio::time::Instant,
        ),
        Error,
    > {
        use super::remote_connect::RemoteConnectReason;
        let connected = self
            .ensure_remote_session_connected(
                session_id,
                Some(&bookmark.node_id),
                RemoteConnectReason::Open,
            )
            .await
            .map_err(|error| error.to_acp_error())?;
        let state = self
            .config
            .event_sink
            .remote_sync(session_id, &bookmark.node_id);
        // The authorization is the first call through a reused attachment;
        // a dead one is replaced once (within the same budget).
        let authorize = async move {
            match Self::authorize_remote_history(&state, &connected.session_ref).await {
                Ok(()) => Ok(connected),
                Err(error) if connected.reused_attachment_failed(&error) => {
                    log::info!(
                        "remote session {session_id} history: reused attachment_id={} failed \
                         ({error}); replacing it",
                        connected.attachment_id,
                    );
                    let recovered = self
                        .replace_failed_remote_attachment(
                            session_id,
                            connected.attachment_id,
                            connected.session_ref.remote_node_id(),
                            false,
                        )
                        .await
                        .map_err(|error| error.to_acp_error())?;
                    Self::authorize_remote_history(&state, &recovered.session_ref).await?;
                    Ok(recovered)
                }
                Err(error) => Err(Error::from(error)),
            }
        };
        let (connected, deadline) = authorize_before_history_transfer(
            session_id,
            super::model_registry::remote_transfer_budget(),
            authorize,
        )
        .await?;
        let sync = crate::agent::remote::event_backfill::RemoteSync::new(
            self.config.event_sink.clone(),
            connected.session_ref,
            session_id,
            &bookmark.node_id,
            &bookmark.peer_label,
        );
        Ok((sync, deadline))
    }

    /// Make a remote session's whole history local (tool calls may sit
    /// anywhere in it).
    #[cfg(feature = "remote")]
    async fn ensure_remote_history_complete(
        &self,
        session_id: &str,
        bookmark: &RemoteSessionBookmark,
    ) -> Result<(), Error> {
        let (sync, deadline) = self.remote_history_sync(session_id, bookmark).await?;
        let state = sync.state().clone();
        state
            .hydrate()
            .await
            .map_err(|error| internal(session_id, error))?;
        let work = async {
            if state.tip() == 0 && !state.has_data() {
                sync.open(false).await?;
            }
            let tip = state.tip();
            if tip > 0 {
                sync.ensure_covered(1, tip).await?;
            }
            Ok::<(), crate::error::AgentError>(())
        };
        match tokio::time::timeout_at(deadline, work).await {
            Ok(Ok(())) if !state.history_denied() => Ok(()),
            Ok(Ok(())) => Err(crate::error::AgentError::MeshAccessDenied.into()),
            Ok(Err(crate::error::AgentError::MeshAccessDenied)) => {
                state.note_history_denial();
                Err(crate::error::AgentError::MeshAccessDenied.into())
            }
            Ok(Err(error)) => Err(transfer_failed(session_id, error.to_string())),
            Err(_) => Err(transfer_failed(
                session_id,
                "history transfer timed out".into(),
            )),
        }
    }

    pub(super) async fn handle_ext_session_tool_content(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: SessionToolContentRequest = parse_request(req)?;
        let session_id = parsed.session_id.as_str();
        let source = self.history_source(session_id).await?;
        #[cfg(feature = "remote")]
        if let Some(bookmark) = source.bookmark.as_ref() {
            self.ensure_remote_history_complete(session_id, bookmark)
                .await?;
        }
        let content = find_tool_content(
            source.journal.as_ref(),
            session_id,
            source.node(),
            &parsed.tool_call_id,
            parsed.part,
        )
        .await
        .map_err(|error| internal(session_id, error))?;
        let Some(content) = content else {
            return Err(coded_error(
                -32002,
                "tool_call_not_found",
                session_id,
                format!(
                    "tool call {} not found in session {session_id}",
                    parsed.tool_call_id
                ),
            ));
        };
        ext_json_response(&SessionToolContentResponse {
            session_id: parsed.session_id,
            tool_call_id: parsed.tool_call_id,
            part: parsed.part,
            content,
        })
    }
}

/// Run `authorization` within the history transfer budget; returns its value
/// and the deadline the transfer itself must keep.
#[cfg(feature = "remote")]
async fn authorize_before_history_transfer<T>(
    session_id: &str,
    budget: std::time::Duration,
    authorization: impl Future<Output = Result<T, Error>>,
) -> Result<(T, tokio::time::Instant), Error> {
    let deadline = tokio::time::Instant::now() + budget;
    let value = tokio::time::timeout_at(deadline, authorization)
        .await
        .map_err(|_| transfer_failed(session_id, "history authorization timed out".into()))??;
    Ok((value, deadline))
}

#[cfg(all(test, feature = "remote"))]
#[tokio::test(start_paused = true)]
async fn history_transfer_deadline_includes_authorization_time() {
    use std::time::Duration;
    use tokio::time::{Instant, sleep, timeout_at};

    let start = Instant::now();
    let ((), deadline) =
        authorize_before_history_transfer("session", Duration::from_secs(10), async {
            sleep(Duration::from_secs(6)).await;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        timeout_at(deadline, sleep(Duration::from_secs(5)))
            .await
            .is_err()
    );
    assert_eq!(Instant::now(), start + Duration::from_secs(10));
}

#[cfg(feature = "remote")]
fn transfer_failed(session_id: &str, message: String) -> Error {
    Error::from(crate::error::AgentError::RemoteTransferFailed {
        session_id: session_id.to_string(),
        message,
    })
}
