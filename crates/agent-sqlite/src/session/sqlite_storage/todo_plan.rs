//! Persisted todo plan of each session (`session_todo_plans`).
//!
//! `session/list` reports a session's current todo plan without loading its
//! journal. The plan is the projection every other path already uses: the
//! entries of the last `todowrite` `ToolCallStart` whose arguments are a todo
//! list (`todo_plan_of`). It is kept current when such an event is journaled,
//! recomputed when history is cut back, and backfilled once by migration
//! `0041_session_todo_plans`.

use querymt_agent_core::protocol::PlanEntry;
use querymt_agent_core::replay::{TODO_WRITE_TOOL_NAMES, todo_entries_from_arguments};
use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;

/// Upsert a session's plan, or delete its row when `entries` is empty.
pub(super) fn store_todo_plan(
    conn: &Connection,
    session_db_id: i64,
    entries: &[PlanEntry],
) -> Result<(), rusqlite::Error> {
    if entries.is_empty() {
        conn.execute(
            "DELETE FROM session_todo_plans WHERE session_id = ?1",
            params![session_db_id],
        )?;
        return Ok(());
    }
    let entries_json = serde_json::to_string(entries)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let updated_at = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO session_todo_plans (session_id, entries_json, updated_at) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(session_id) DO UPDATE SET \
             entries_json = excluded.entries_json, updated_at = excluded.updated_at",
        params![session_db_id, entries_json, updated_at],
    )?;
    Ok(())
}

/// Project the plan of one journaled `todowrite` call (`todo_plan_of`) onto
/// the session's row. A session without a row (journal events of an unknown
/// session) is ignored.
pub(super) fn project_todo_plan(
    conn: &Connection,
    public_session_id: &str,
    entries: &[PlanEntry],
) -> Result<(), rusqlite::Error> {
    let session_db_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM sessions WHERE public_id = ?1",
            params![public_session_id],
            |row| row.get(0),
        )
        .optional()?;
    match session_db_id {
        Some(id) => store_todo_plan(conn, id, entries),
        None => Ok(()),
    }
}

/// Rebuild a session's row from its journal: the last `todowrite` call whose
/// arguments are a todo list decides, and no such call leaves no row. Used by
/// the migration backfill and after history is cut back.
pub(super) fn recompute_todo_plan(
    conn: &Connection,
    session_db_id: i64,
) -> Result<(), rusqlite::Error> {
    let public_id: Option<String> = conn
        .query_row(
            "SELECT public_id FROM sessions WHERE id = ?1",
            params![session_db_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(public_id) = public_id else {
        return Ok(());
    };

    let tool_names = serde_json::to_string(&TODO_WRITE_TOOL_NAMES)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload_json, '$.data.arguments') \
         FROM event_journal \
         WHERE session_id = ?1 AND kind = 'tool_call_start' \
           AND json_extract(payload_json, '$.data.tool_name') \
               IN (SELECT value FROM json_each(?2)) \
         ORDER BY stream_seq DESC",
    )?;
    let mut rows = stmt.query(params![public_id, tool_names])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let Some(arguments) = row.get::<_, Option<String>>(0)? else {
            continue;
        };
        if let Some(found) = todo_entries_from_arguments(&arguments) {
            entries = found;
            break;
        }
    }
    drop(rows);
    drop(stmt);
    store_todo_plan(conn, session_db_id, &entries)
}

/// Rebuild the rows of every session; the migration backfill.
pub(super) fn recompute_all_todo_plans(conn: &Connection) -> Result<(), rusqlite::Error> {
    let ids = {
        let mut stmt = conn.prepare("SELECT id FROM sessions")?;
        stmt.query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for id in ids {
        recompute_todo_plan(conn, id)?;
    }
    Ok(())
}
