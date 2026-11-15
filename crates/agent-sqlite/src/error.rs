//! Classification of `rusqlite` errors into the database-neutral core errors.
//!
//! `SessionError::Database` carries a boxed source plus a `DbErrorKind`; this
//! module is where a `rusqlite::Error` is turned into that pair. The orphan
//! rule forbids `From<rusqlite::Error>` for a core type from the sqlite
//! package, so call sites use `.db()` / `.knowledge_db()` instead of a bare `?`.

use querymt_agent_core::knowledge::KnowledgeError;
use querymt_agent_core::session::error::{DbError, DbErrorKind, SessionError, SessionResult};

/// Classify a `rusqlite` error.
pub fn classify(err: &rusqlite::Error) -> DbErrorKind {
    match err {
        rusqlite::Error::QueryReturnedNoRows => DbErrorKind::NoRows,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            DbErrorKind::Busy
        }
        _ => DbErrorKind::Other,
    }
}

/// Convert a `rusqlite::Error` into a `SessionError::Database`.
pub fn session_db_error(err: rusqlite::Error) -> SessionError {
    let kind = classify(&err);
    SessionError::Database(DbError::new(err, kind))
}

/// Result extension for storage code that returns `rusqlite` errors.
pub trait SqliteResultExt<T> {
    /// Map a `rusqlite` error into `SessionError::Database`.
    fn db(self) -> SessionResult<T>;
    /// Map a `rusqlite` error into `KnowledgeError::DatabaseError`.
    fn knowledge_db(self) -> Result<T, KnowledgeError>;
}

impl<T> SqliteResultExt<T> for Result<T, rusqlite::Error> {
    fn db(self) -> SessionResult<T> {
        self.map_err(session_db_error)
    }

    fn knowledge_db(self) -> Result<T, KnowledgeError> {
        self.map_err(|err| KnowledgeError::DatabaseError(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::error::LLMError;
    use std::error::Error as _;

    fn failure(code: i32, msg: Option<&str>) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), msg.map(str::to_string))
    }

    #[test]
    fn storage_error_converted_for_a_provider_caller_keeps_its_source() {
        let err = session_db_error(failure(
            rusqlite::ffi::SQLITE_BUSY,
            Some("distinctive busy marker"),
        ));

        // SessionError -> DbError -> rusqlite::Error.
        let db = err.source().expect("database error has a source");
        let inner = db.source().expect("DbError exposes the driver error");
        assert!(inner.downcast_ref::<rusqlite::Error>().is_some());
        assert!(err.is_transient());

        // `LLMError` stores strings only, so the cause must be in its text.
        let llm: LLMError = err.into();
        assert!(
            llm.to_string().contains("distinctive busy marker"),
            "LLM error text lost the rusqlite message: {llm}"
        );
    }

    #[test]
    fn is_transient_only_for_busy_and_locked() {
        let e = |code| session_db_error(failure(code, None));
        assert!(e(rusqlite::ffi::SQLITE_BUSY).is_transient());
        assert!(e(rusqlite::ffi::SQLITE_LOCKED).is_transient());
        assert!(!e(rusqlite::ffi::SQLITE_CONSTRAINT).is_transient());
        assert!(!session_db_error(rusqlite::Error::QueryReturnedNoRows).is_transient());
        assert!(!SessionError::Other("x".into()).is_transient());
    }

    #[test]
    fn sqlite_error_classification_is_preserved() {
        assert!(session_db_error(failure(rusqlite::ffi::SQLITE_BUSY, None)).is_transient());
        let no_rows = session_db_error(rusqlite::Error::QueryReturnedNoRows);
        assert!(no_rows.is_no_rows());
        assert!(!no_rows.is_transient());

        // A constraint-violation message round-trips through db_message_contains
        // with the same text as rusqlite::Error::to_string().
        let constraint = failure(rusqlite::ffi::SQLITE_CONSTRAINT, Some("UNIQUE failed: x.y"));
        let raw_text = constraint.to_string();
        let err = session_db_error(constraint);
        assert!(err.db_message_contains("UNIQUE failed: x.y"));
        assert!(err.db_message_contains(&raw_text));

        // Display text is unchanged.
        assert_eq!(err.to_string(), format!("Database error: {raw_text}"));

        // The kind is stored, and the driver error is reachable by downcast.
        let SessionError::Database(db) = &err else {
            panic!("expected Database variant");
        };
        assert_eq!(db.kind(), DbErrorKind::Other);
        let inner = db.source().expect("boxed source");
        assert!(inner.downcast_ref::<rusqlite::Error>().is_some());
    }
}
