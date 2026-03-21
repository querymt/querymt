//! Database schema initialization used by the migration framework.
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
            session_kind TEXT,
            fork_point_type TEXT,
            fork_point_ref TEXT,
            fork_instructions TEXT,
            provider_node_id TEXT,
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

        -- Session execution config snapshot (runtime knobs persisted per session)
        CREATE TABLE IF NOT EXISTS session_execution_configs (
            id INTEGER PRIMARY KEY,
            session_id INTEGER NOT NULL UNIQUE,
            config_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_session_execution_configs_session
            ON session_execution_configs(session_id);

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

        -- Event journal (durable events with full envelope semantics)
        CREATE TABLE IF NOT EXISTS event_journal (
            event_id TEXT PRIMARY KEY,
            stream_seq INTEGER UNIQUE NOT NULL,
            session_id TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            origin TEXT NOT NULL DEFAULT 'local',
            source_node TEXT,
            kind TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_event_journal_session_seq
            ON event_journal(session_id, stream_seq);
        CREATE INDEX IF NOT EXISTS idx_event_journal_timestamp
            ON event_journal(timestamp);
        CREATE INDEX IF NOT EXISTS idx_event_journal_origin
            ON event_journal(origin, source_node);

        -- Sequence generator for stream_seq (single-row table)
        CREATE TABLE IF NOT EXISTS event_journal_seq (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            next_seq INTEGER NOT NULL DEFAULT 1
        );
        INSERT OR IGNORE INTO event_journal_seq (id, next_seq) VALUES (1, 1);

        -- Revert states for undo/redo support
        CREATE TABLE IF NOT EXISTS revert_states (
            id INTEGER PRIMARY KEY,
            public_id TEXT NOT NULL UNIQUE,
            session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            message_id TEXT NOT NULL,
            snapshot_id TEXT NOT NULL,
            backend_id TEXT NOT NULL DEFAULT 'git',
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
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

        -- User-managed custom local models per provider
        CREATE TABLE IF NOT EXISTS custom_models (
            id INTEGER PRIMARY KEY,
            provider TEXT NOT NULL,
            model_id TEXT NOT NULL,
            display_name TEXT NOT NULL,
            config_json TEXT NOT NULL,
            source_type TEXT NOT NULL,
            source_ref TEXT,
            family TEXT,
            quant TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(provider, model_id)
        );

        CREATE INDEX IF NOT EXISTS idx_custom_models_provider
            ON custom_models(provider);

        -- ========================================================================
        -- TIER 1: SCHEDULES (with public_id) — Autonomous scheduled work
        -- ========================================================================

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

        -- Scheduler lease table (single-row, DB-backed leadership)
        CREATE TABLE IF NOT EXISTS scheduler_lease (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            owner_id TEXT NOT NULL,
            acquired_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
        );

        -- ========================================================================
        -- TIER 1: KNOWLEDGE LAYER (with public_id) — Pluggable structured memory
        -- ========================================================================

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
