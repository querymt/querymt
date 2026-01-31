//! Database schema initialization (breaking changes allowed, no migrations)
//!
//! Hybrid ID Strategy:
//! - INTEGER PRIMARY KEYS for fast internal operations and foreign keys
//! - UUID v7 `public_id` columns for external exposure (sessions, tasks, delegations, messages)
//! - Internal-only tables skip public_id to save space

use rusqlite::Connection;

/// Initialize the hybrid INTEGER PK + UUID v7 public_id schema
pub fn init_schema(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        -- ========================================================================
        -- TIER 1: EXTERNALLY EXPOSED TABLES (with public_id)
        -- ========================================================================

        -- Sessions - heavily exposed externally
        CREATE TABLE IF NOT EXISTS sessions (
            id INTEGER PRIMARY KEY,
            public_id TEXT UNIQUE NOT NULL,
            name TEXT,
            cwd TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            current_intent_snapshot_id INTEGER,
            active_task_id INTEGER,
            llm_config_id INTEGER,
            parent_session_id INTEGER,
            fork_origin TEXT,
            fork_point_type TEXT,
            fork_point_ref TEXT,
            fork_instructions TEXT,
            FOREIGN KEY(parent_session_id) REFERENCES sessions(id) ON DELETE SET NULL,
            FOREIGN KEY(llm_config_id) REFERENCES llm_configs(id) ON DELETE SET NULL,
            FOREIGN KEY(current_intent_snapshot_id) REFERENCES intent_snapshots(id) ON DELETE SET NULL,
            FOREIGN KEY(active_task_id) REFERENCES tasks(id) ON DELETE SET NULL
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_public_id ON sessions(public_id);
        CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);

        -- Tasks - referenced externally in UIs/APIs
        CREATE TABLE IF NOT EXISTS tasks (
            id INTEGER PRIMARY KEY,
            public_id TEXT UNIQUE NOT NULL,
            session_id INTEGER NOT NULL,
            kind TEXT NOT NULL,
            status TEXT NOT NULL,
            expected_deliverable TEXT,
            acceptance_criteria TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_public_id ON tasks(public_id);
        CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id);
        CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status) WHERE status IN ('active', 'paused');

        -- Delegations - external agent references
        CREATE TABLE IF NOT EXISTS delegations (
            id INTEGER PRIMARY KEY,
            public_id TEXT UNIQUE NOT NULL,
            session_id INTEGER NOT NULL,
            task_id INTEGER,
            target_agent_id TEXT NOT NULL,
            objective TEXT NOT NULL,
            objective_hash TEXT NOT NULL,
            context TEXT,
            constraints TEXT,
            expected_output TEXT,
            verification_spec TEXT,  -- Structured verification specification (JSON)
            planning_summary TEXT,  -- AI-generated summary of parent planning conversation
            status TEXT NOT NULL,
            retry_count INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            completed_at TEXT,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE SET NULL
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_delegations_public_id ON delegations(public_id);
        CREATE INDEX IF NOT EXISTS idx_delegations_session ON delegations(session_id);
        CREATE INDEX IF NOT EXISTS idx_delegations_hash ON delegations(objective_hash, session_id, status);

        -- Legacy messages table (with public_id for API compatibility)
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY,
            public_id TEXT UNIQUE NOT NULL,
            session_id INTEGER NOT NULL,
            role TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            parent_message_id INTEGER,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY(parent_message_id) REFERENCES messages(id) ON DELETE SET NULL
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_public_id ON messages(public_id);
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);

        -- ========================================================================
        -- TIER 2: INTERNAL TABLES (no public_id)
        -- ========================================================================

        -- LLM Configs - internal configuration
        CREATE TABLE IF NOT EXISTS llm_configs (
            id INTEGER PRIMARY KEY,
            name TEXT,
            provider TEXT NOT NULL,
            model TEXT NOT NULL,
            params TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(provider, model, params)
        );

        CREATE INDEX IF NOT EXISTS idx_llm_configs_provider_model ON llm_configs(provider, model);

        -- Intent snapshots - internal state tracking
        CREATE TABLE IF NOT EXISTS intent_snapshots (
            id INTEGER PRIMARY KEY,
            session_id INTEGER NOT NULL,
            task_id INTEGER,
            summary TEXT NOT NULL,
            constraints TEXT,
            next_step_hint TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE SET NULL
        );

        CREATE INDEX IF NOT EXISTS idx_intent_session ON intent_snapshots(session_id);

        -- Decisions - internal tracking
        CREATE TABLE IF NOT EXISTS decisions (
            id INTEGER PRIMARY KEY,
            session_id INTEGER NOT NULL,
            task_id INTEGER,
            description TEXT NOT NULL,
            rationale TEXT,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE SET NULL
        );

        CREATE INDEX IF NOT EXISTS idx_decisions_session ON decisions(session_id);

        -- Alternatives - internal tracking
        CREATE TABLE IF NOT EXISTS alternatives (
            id INTEGER PRIMARY KEY,
            session_id INTEGER NOT NULL,
            task_id INTEGER,
            description TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE SET NULL
        );

        -- Progress entries - internal logging
        CREATE TABLE IF NOT EXISTS progress_entries (
            id INTEGER PRIMARY KEY,
            session_id INTEGER NOT NULL,
            task_id INTEGER,
            kind TEXT NOT NULL,
            content TEXT NOT NULL,
            metadata TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE SET NULL
        );

        CREATE INDEX IF NOT EXISTS idx_progress_session ON progress_entries(session_id);
        CREATE INDEX IF NOT EXISTS idx_progress_task ON progress_entries(task_id) WHERE task_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_progress_created ON progress_entries(created_at);
        CREATE INDEX IF NOT EXISTS idx_progress_kind ON progress_entries(session_id, kind);

        -- Artifacts - internal file tracking
        CREATE TABLE IF NOT EXISTS artifacts (
            id INTEGER PRIMARY KEY,
            session_id INTEGER NOT NULL,
            task_id INTEGER,
            kind TEXT NOT NULL,
            uri TEXT,
            path TEXT,
            summary TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE SET NULL
        );

        -- Events - internal audit log
        CREATE TABLE IF NOT EXISTS events (
            seq INTEGER PRIMARY KEY,
            timestamp INTEGER NOT NULL,
            session_id TEXT NOT NULL,
            kind TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
        CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);

        -- Revert states for undo/redo support
        CREATE TABLE IF NOT EXISTS revert_states (
            id INTEGER PRIMARY KEY,
            public_id TEXT NOT NULL UNIQUE,
            session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            message_id TEXT NOT NULL,
            snapshot_id TEXT NOT NULL,
            backend_id TEXT NOT NULL DEFAULT 'git',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(session_id)
        );

        CREATE INDEX IF NOT EXISTS idx_revert_states_session ON revert_states(session_id);

        -- Legacy message parts
        CREATE TABLE IF NOT EXISTS message_parts (
            id INTEGER PRIMARY KEY,
            message_id INTEGER NOT NULL,
            part_type TEXT NOT NULL,
            content_json TEXT NOT NULL,
            sort_order INTEGER NOT NULL,
            FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_message_parts_message ON message_parts(message_id, sort_order);
        "#,
    )?;

    Ok(())
}
