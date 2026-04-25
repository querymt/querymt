use rusqlite::{Connection, params};
use std::collections::HashSet;
use time::OffsetDateTime;

use crate::session::schema;

pub(super) type MigrationFn = fn(&mut Connection) -> Result<(), rusqlite::Error>;

pub(super) struct Migration {
    pub(super) version: &'static str,
    pub(super) apply: MigrationFn,
}

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "0001_initial_reset",
        apply: migration_0001_initial_reset,
    },
    Migration {
        version: "0002_drop_legacy_events",
        apply: migration_0002_drop_legacy_events,
    },
    Migration {
        version: "0003_message_source_model",
        apply: migration_0003_message_source_model,
    },
    Migration {
        version: "0004_add_session_kind",
        apply: migration_0004_add_session_kind,
    },
    Migration {
        version: "0005_add_scheduler_and_knowledge_tables",
        apply: migration_0005_add_scheduler_and_knowledge_tables,
    },
    Migration {
        version: "0006_add_knowledge_fts5",
        apply: migration_0006_add_knowledge_fts5,
    },
    Migration {
        version: "0007_intent_session_id_index",
        apply: migration_0007_intent_session_id_index,
    },
    Migration {
        version: "0008_remote_session_bookmarks",
        apply: migration_0008_remote_session_bookmarks,
    },
    Migration {
        version: "0009_sessions_browse_and_search_indexes",
        apply: migration_0009_sessions_browse_and_search_indexes,
    },
];

pub(super) fn apply_migrations(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version TEXT PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );
        "#,
    )?;

    let applied = load_applied_migrations(conn)?;

    for migration in MIGRATIONS {
        if applied.contains(migration.version) {
            continue;
        }

        (migration.apply)(conn)?;
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![
                migration.version,
                OffsetDateTime::now_utc().unix_timestamp()
            ],
        )?;
    }

    Ok(())
}

fn load_applied_migrations(conn: &Connection) -> Result<HashSet<String>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn migration_0001_initial_reset(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // Migration 0001 intentionally resets all known tables so this release becomes
    // the new baseline for forward-only schema evolution.
    conn.execute_batch(
        r#"
            DROP TABLE IF EXISTS message_tool_calls;
            DROP TABLE IF EXISTS message_binaries;
            DROP TABLE IF EXISTS message_usage;
            DROP TABLE IF EXISTS messages_fts;
            DROP TABLE IF EXISTS message_parts;
            DROP TABLE IF EXISTS messages;
            DROP TABLE IF EXISTS events;
            DROP TABLE IF EXISTS event_journal_seq;
            DROP TABLE IF EXISTS event_journal;
            DROP TABLE IF EXISTS llm_configs;
            DROP TABLE IF EXISTS custom_models;
            DROP TABLE IF EXISTS revert_states;
            DROP TABLE IF EXISTS delegations;
            DROP TABLE IF EXISTS artifacts;
            DROP TABLE IF EXISTS progress_entries;
            DROP TABLE IF EXISTS alternatives;
            DROP TABLE IF EXISTS decisions;
            DROP TABLE IF EXISTS intent_snapshots;
            DROP TABLE IF EXISTS tasks;
            DROP TABLE IF EXISTS sessions;
            DROP TABLE IF EXISTS session_execution_configs;
            DROP TABLE IF EXISTS llm_configs;
        "#,
    )?;

    schema::init_schema(conn)?;
    Ok(())
}

fn migration_0002_drop_legacy_events(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // The legacy `events` table is no longer used. All event reads and writes
    // go through the `event_journal` table exclusively.
    conn.execute_batch(
        r#"
            DROP TABLE IF EXISTS events;
        "#,
    )?;
    Ok(())
}

fn migration_0003_message_source_model(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // Preserve assistant-origin provider/model metadata for cross-model replay logic.
    let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<HashSet<_>, _>>()?;

    if !columns.contains("source_provider") {
        conn.execute("ALTER TABLE messages ADD COLUMN source_provider TEXT", [])?;
    }
    if !columns.contains("source_model") {
        conn.execute("ALTER TABLE messages ADD COLUMN source_model TEXT", [])?;
    }

    Ok(())
}

fn migration_0004_add_session_kind(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // Add optional session kind metadata (e.g. "recurring") for UI labeling.
    let has_session_kind = {
        let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
        let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|name| name == "session_kind")
    };

    if !has_session_kind {
        conn.execute("ALTER TABLE sessions ADD COLUMN session_kind TEXT", [])?;
    }

    Ok(())
}

fn migration_0005_add_scheduler_and_knowledge_tables(
    conn: &mut Connection,
) -> Result<(), rusqlite::Error> {
    // Add tables introduced after the 0001 baseline that existing databases may
    // be missing. All statements use IF NOT EXISTS / IF NOT EXISTS so this is
    // safe to run against fresh installs where init_schema already created them.
    conn.execute_batch(
        r#"
            CREATE TABLE IF NOT EXISTS schedules (
                id INTEGER PRIMARY KEY,
                public_id TEXT UNIQUE NOT NULL,
                task_id INTEGER NOT NULL,
                task_public_id TEXT NOT NULL,
                session_id INTEGER NOT NULL,
                session_public_id TEXT NOT NULL,
                trigger_json TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'armed',
                last_run_at TEXT,
                next_run_at TEXT,
                run_count INTEGER NOT NULL DEFAULT 0,
                consecutive_failures INTEGER NOT NULL DEFAULT 0,
                config_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE,
                FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_schedules_state_next_run
                ON schedules(state, next_run_at) WHERE state = 'armed';
            CREATE INDEX IF NOT EXISTS idx_schedules_running_last_run
                ON schedules(last_run_at) WHERE state = 'running';
            CREATE INDEX IF NOT EXISTS idx_schedules_terminal_updated
                ON schedules(state, updated_at) WHERE state IN ('failed', 'exhausted');
            CREATE INDEX IF NOT EXISTS idx_schedules_session_public
                ON schedules(session_public_id);
            CREATE INDEX IF NOT EXISTS idx_schedules_task_public
                ON schedules(task_public_id);

            CREATE TABLE IF NOT EXISTS scheduler_lease (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                owner_id TEXT NOT NULL,
                acquired_at TEXT NOT NULL,
                expires_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS knowledge_entries (
                id INTEGER PRIMARY KEY,
                public_id TEXT UNIQUE NOT NULL,
                scope TEXT NOT NULL,
                source TEXT NOT NULL,
                raw_text TEXT,
                summary TEXT NOT NULL,
                entities_json TEXT NOT NULL DEFAULT '[]',
                topics_json TEXT NOT NULL DEFAULT '[]',
                connections_json TEXT NOT NULL DEFAULT '[]',
                importance REAL NOT NULL DEFAULT 0.5,
                consolidated_at TEXT,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_knowledge_entries_scope
                ON knowledge_entries(scope);
            CREATE INDEX IF NOT EXISTS idx_knowledge_entries_unconsolidated
                ON knowledge_entries(scope, consolidated_at)
                WHERE consolidated_at IS NULL;
            CREATE INDEX IF NOT EXISTS idx_knowledge_entries_created
                ON knowledge_entries(scope, created_at);

            CREATE TABLE IF NOT EXISTS knowledge_consolidations (
                id INTEGER PRIMARY KEY,
                public_id TEXT UNIQUE NOT NULL,
                scope TEXT NOT NULL,
                source_entry_public_ids_json TEXT NOT NULL,
                summary TEXT NOT NULL,
                insight TEXT NOT NULL,
                connections_json TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_knowledge_consolidations_scope
                ON knowledge_consolidations(scope);
            CREATE INDEX IF NOT EXISTS idx_knowledge_consolidations_created
                ON knowledge_consolidations(scope, created_at);

            CREATE TABLE IF NOT EXISTS knowledge_ingestion_log (
                id INTEGER PRIMARY KEY,
                scope TEXT NOT NULL,
                source_key TEXT NOT NULL,
                processed_at TEXT NOT NULL,
                UNIQUE(scope, source_key)
            );
        "#,
    )?;
    Ok(())
}

fn migration_0007_intent_session_id_index(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // Add composite index on intent_snapshots(session_id, id) so the correlated
    // MIN(id) subquery in get_session_list_view resolves in O(1) per session
    // instead of scanning all intents for that session.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_intent_session_id ON intent_snapshots(session_id, id);",
    )
}

fn migration_0006_add_knowledge_fts5(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // Add FTS5 full-text search indexes for the knowledge layer.
    // Uses external-content tables backed by the base knowledge tables,
    // with porter-stemming unicode61 tokenizer for English stemming.
    conn.execute_batch(
        r#"
            -- FTS5 virtual tables (external content, porter stemming)
            CREATE VIRTUAL TABLE IF NOT EXISTS knowledge_entries_fts USING fts5(
                summary, raw_text, source,
                content='knowledge_entries',
                content_rowid='id',
                tokenize='porter unicode61'
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS knowledge_consolidations_fts USING fts5(
                summary, insight,
                content='knowledge_consolidations',
                content_rowid='id',
                tokenize='porter unicode61'
            );

            -- Sync triggers for knowledge_entries
            CREATE TRIGGER IF NOT EXISTS knowledge_entries_ai AFTER INSERT ON knowledge_entries BEGIN
                INSERT INTO knowledge_entries_fts(rowid, summary, raw_text, source)
                VALUES (new.id, new.summary, COALESCE(new.raw_text, ''), new.source);
            END;

            CREATE TRIGGER IF NOT EXISTS knowledge_entries_au AFTER UPDATE ON knowledge_entries BEGIN
                INSERT INTO knowledge_entries_fts(knowledge_entries_fts, rowid, summary, raw_text, source)
                VALUES ('delete', old.id, old.summary, COALESCE(old.raw_text, ''), old.source);
                INSERT INTO knowledge_entries_fts(rowid, summary, raw_text, source)
                VALUES (new.id, new.summary, COALESCE(new.raw_text, ''), new.source);
            END;

            CREATE TRIGGER IF NOT EXISTS knowledge_entries_ad AFTER DELETE ON knowledge_entries BEGIN
                INSERT INTO knowledge_entries_fts(knowledge_entries_fts, rowid, summary, raw_text, source)
                VALUES ('delete', old.id, old.summary, COALESCE(old.raw_text, ''), old.source);
            END;

            -- Sync triggers for knowledge_consolidations
            CREATE TRIGGER IF NOT EXISTS knowledge_consolidations_ai AFTER INSERT ON knowledge_consolidations BEGIN
                INSERT INTO knowledge_consolidations_fts(rowid, summary, insight)
                VALUES (new.id, new.summary, new.insight);
            END;

            CREATE TRIGGER IF NOT EXISTS knowledge_consolidations_au AFTER UPDATE ON knowledge_consolidations BEGIN
                INSERT INTO knowledge_consolidations_fts(knowledge_consolidations_fts, rowid, summary, insight)
                VALUES ('delete', old.id, old.summary, old.insight);
                INSERT INTO knowledge_consolidations_fts(rowid, summary, insight)
                VALUES (new.id, new.summary, new.insight);
            END;

            CREATE TRIGGER IF NOT EXISTS knowledge_consolidations_ad AFTER DELETE ON knowledge_consolidations BEGIN
                INSERT INTO knowledge_consolidations_fts(knowledge_consolidations_fts, rowid, summary, insight)
                VALUES ('delete', old.id, old.summary, old.insight);
            END;
        "#,
    )?;

    // Rebuild FTS indexes from any existing data in the base tables.
    // This is a no-op on fresh installs but populates the index for
    // databases that already had knowledge entries before this migration.
    conn.execute_batch(
        r#"
            INSERT INTO knowledge_entries_fts(knowledge_entries_fts) VALUES('rebuild');
            INSERT INTO knowledge_consolidations_fts(knowledge_consolidations_fts) VALUES('rebuild');
        "#,
    )?;

    Ok(())
}

fn migration_0008_remote_session_bookmarks(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
            CREATE TABLE IF NOT EXISTS remote_session_bookmarks (
                session_id TEXT PRIMARY KEY,
                node_id    TEXT NOT NULL,
                peer_label TEXT NOT NULL,
                cwd        TEXT,
                created_at INTEGER NOT NULL DEFAULT 0,
                title      TEXT
            );
        "#,
    )?;
    Ok(())
}

fn migration_0009_sessions_browse_and_search_indexes(
    conn: &mut Connection,
) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
            CREATE INDEX IF NOT EXISTS idx_sessions_cwd_updated ON sessions(cwd, updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);

            DROP TRIGGER IF EXISTS sessions_ai;
            DROP TRIGGER IF EXISTS sessions_au;
            DROP TRIGGER IF EXISTS sessions_ad;
            DROP TABLE IF EXISTS sessions_fts;

            CREATE VIRTUAL TABLE sessions_fts USING fts5(
                public_id,
                name,
                cwd,
                title,
                content='',
                tokenize='porter unicode61'
            );

            CREATE TRIGGER sessions_ai AFTER INSERT ON sessions BEGIN
                INSERT INTO sessions_fts(rowid, public_id, name, cwd, title)
                VALUES (
                    new.id,
                    new.public_id,
                    COALESCE(new.name, ''),
                    COALESCE(new.cwd, ''),
                    COALESCE((SELECT i.summary FROM intent_snapshots i WHERE i.session_id = new.id ORDER BY i.id ASC LIMIT 1), '')
                );
            END;

            CREATE TRIGGER sessions_au AFTER UPDATE ON sessions BEGIN
                INSERT INTO sessions_fts(sessions_fts, rowid) VALUES ('delete', old.id);
                INSERT INTO sessions_fts(rowid, public_id, name, cwd, title)
                VALUES (
                    new.id, new.public_id, COALESCE(new.name, ''), COALESCE(new.cwd, ''),
                    COALESCE((SELECT i.summary FROM intent_snapshots i WHERE i.session_id = new.id ORDER BY i.id ASC LIMIT 1), '')
                );
            END;

            CREATE TRIGGER sessions_ad AFTER DELETE ON sessions BEGIN
                INSERT INTO sessions_fts(sessions_fts, rowid) VALUES ('delete', old.id);
            END;

            INSERT INTO sessions_fts(rowid, public_id, name, cwd, title)
            SELECT s.id, s.public_id, COALESCE(s.name, ''), COALESCE(s.cwd, ''),
                   COALESCE((SELECT i.summary FROM intent_snapshots i
                             WHERE i.session_id = s.id ORDER BY i.id ASC LIMIT 1), '')
            FROM sessions s;
        "#,
    )?;

    Ok(())
}
