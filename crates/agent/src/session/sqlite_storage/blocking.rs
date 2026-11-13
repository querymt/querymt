//! The single helper that runs blocking SQLite work off the async runtime.
//!
//! Every repository and the event journal go through [`with_conn`] (or
//! [`with_conn_sqlite`]) instead of keeping their own `spawn_blocking` copy.
//! The connection mutex is `parking_lot`, so a panic inside the closure does
//! not poison it: only that call fails, with [`SessionError::TaskFailed`],
//! and later calls keep working.

use crate::session::error::{SessionError, SessionResult};
use parking_lot::Mutex;
use rusqlite::Connection;
use std::sync::Arc;

/// Runs `f` with the locked connection on a blocking thread.
///
/// A panicked or cancelled task is reported as [`SessionError::TaskFailed`]
/// carrying the join error as its source.
pub(crate) async fn with_conn<F, R>(conn: &Arc<Mutex<Connection>>, f: F) -> SessionResult<R>
where
    F: FnOnce(&mut Connection) -> SessionResult<R> + Send + 'static,
    R: Send + 'static,
{
    let conn = Arc::clone(conn);
    tokio::task::spawn_blocking(move || {
        let mut guard = conn.lock();
        f(&mut guard)
    })
    .await
    .map_err(SessionError::TaskFailed)?
}

/// Like [`with_conn`] for closures that return a plain `rusqlite::Result`.
pub(crate) async fn with_conn_sqlite<F, R>(conn: &Arc<Mutex<Connection>>, f: F) -> SessionResult<R>
where
    F: FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
    R: Send + 'static,
{
    with_conn(conn, move |conn| f(conn).map_err(SessionError::from)).await
}
