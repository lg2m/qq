use std::path::PathBuf;

use qq_protocol::StoreId;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::sessions::SessionRuntimeError;

pub(in crate::sessions) fn open_database(
    path: &PathBuf,
) -> Result<(Connection, StoreId), SessionRuntimeError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(SessionRuntimeError::Persistence);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SessionRuntimeError::Persistence),
    }
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| SessionRuntimeError::Persistence)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS workspaces (
                 id TEXT PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE,
                 next_sequence INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                 parent_id TEXT REFERENCES sessions(id),
                 owner_run_id TEXT,
                 title TEXT NOT NULL,
                 status TEXT NOT NULL,
                 active_run_id TEXT,
                 queued_prompts INTEGER NOT NULL DEFAULT 0,
                 model TEXT,
                 max_output_tokens INTEGER,
                 organization TEXT,
                 approval_mode TEXT NOT NULL DEFAULT 'ask',
                 context_tokens INTEGER,
                 estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                 cost_known INTEGER NOT NULL DEFAULT 1,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
              CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 command_id TEXT NOT NULL UNIQUE,
                 user_message_id TEXT NOT NULL,
                 assistant_message_id TEXT NOT NULL,
                  status TEXT NOT NULL,
                  kind TEXT NOT NULL DEFAULT 'prompt',
                  auto_compaction INTEGER NOT NULL DEFAULT 0,
                  cancel_requested INTEGER NOT NULL DEFAULT 0,
                  prompt_identity_json TEXT,
                   outcome_json TEXT,
                   usage_json TEXT,
                   context_tokens INTEGER,
                   estimated_cost_usd_nanos INTEGER,
                 created_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 finished_at_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 ordinal INTEGER NOT NULL,
                 turn_ordinal INTEGER NOT NULL DEFAULT 0,
                 role TEXT NOT NULL,
                 state TEXT NOT NULL,
                 output TEXT NOT NULL DEFAULT '',
                 refusal TEXT NOT NULL DEFAULT '',
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, ordinal)
             );
             CREATE TABLE IF NOT EXISTS events (
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                 sequence INTEGER NOT NULL,
                 envelope_json TEXT NOT NULL,
                 PRIMARY KEY(workspace_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS commands (
                 id TEXT PRIMARY KEY,
                 request_json TEXT NOT NULL,
                 receipt_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS sessions_workspace_updated
                 ON sessions(workspace_id, updated_at_ms DESC);
              CREATE INDEX IF NOT EXISTS runs_ready
                  ON runs(status, created_at_ms);
              CREATE INDEX IF NOT EXISTS runs_session_started
                  ON runs(session_id, started_at_ms);
             CREATE INDEX IF NOT EXISTS messages_session_ordinal
                 ON messages(session_id, ordinal);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let schema_version = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    match schema_version.as_deref() {
        None => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            create_session_compactions_table(&transaction)?;
            transaction
                .execute(
                    "INSERT INTO metadata(key, value) VALUES ('schema_version', '10')",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("1") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if !has_column(&transaction, "runs", "cancel_requested")? {
                transaction
                    .execute(
                        "ALTER TABLE runs ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0",
                        [],
                    )
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            for statement in [
                "ALTER TABLE sessions ADD COLUMN estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0",
                "ALTER TABLE sessions ADD COLUMN cost_known INTEGER NOT NULL DEFAULT 1",
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                "ALTER TABLE runs ADD COLUMN usage_json TEXT",
                "ALTER TABLE runs ADD COLUMN estimated_cost_usd_nanos INTEGER",
            ] {
                transaction
                    .execute(statement, [])
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            transaction
                .execute("UPDATE sessions SET cost_known = 0", [])
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("2") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .execute(
                    "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("3") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            for statement in [
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                "ALTER TABLE tool_calls ADD COLUMN approval_resolution TEXT",
                "ALTER TABLE tool_calls ADD COLUMN resolved_at_ms INTEGER",
            ] {
                transaction
                    .execute(statement, [])
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("4") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("5") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("6") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("7") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("8") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("9") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("10" | "11" | "12" | "13" | "14") => {}
        Some(_) => return Err(SessionRuntimeError::Persistence),
    }
    if !matches!(schema_version.as_deref(), Some("11" | "12" | "13" | "14")) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_sessions_context_tokens_column(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '11' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !matches!(schema_version.as_deref(), Some("12" | "13" | "14")) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_sessions_owner_run_id_column(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '12' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !matches!(schema_version.as_deref(), Some("13" | "14")) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_runs_prompt_identity_column(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '13' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if schema_version.as_deref() != Some("14") {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_model_turn_audit_columns(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '14' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_model_turn_audit_schema(&connection)?;
    let stored = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'store_id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let store_id = match stored {
        Some(value) => value
            .parse()
            .map_err(|_| SessionRuntimeError::Persistence)?,
        None => {
            let id = StoreId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            connection
                .execute(
                    "INSERT INTO metadata(key, value) VALUES ('store_id', ?1)",
                    [id.to_string()],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            id
        }
    };
    Ok((connection, store_id))
}

fn create_tool_tables(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE model_turns (
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 model_json TEXT,
                 usage_json TEXT,
                 estimated_cost_usd_nanos INTEGER,
                 completed_at_ms INTEGER,
                 PRIMARY KEY(run_id, turn_ordinal)
             );
             CREATE TABLE tool_calls (
                 id TEXT PRIMARY KEY,
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 turn_ordinal INTEGER NOT NULL,
                 call_ordinal INTEGER NOT NULL,
                 provider_call_id TEXT NOT NULL,
                 name TEXT NOT NULL,
                 arguments_json TEXT NOT NULL,
                 state TEXT NOT NULL,
                 result TEXT,
                 is_error INTEGER NOT NULL DEFAULT 0,
                 display_json TEXT,
                 approval_resolution TEXT,
                 requested_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 resolved_at_ms INTEGER,
                 finished_at_ms INTEGER,
                 UNIQUE(run_id, turn_ordinal, provider_call_id),
                 UNIQUE(run_id, turn_ordinal, call_ordinal)
             );
             CREATE INDEX tool_calls_run_ordinal
                 ON tool_calls(run_id, turn_ordinal, call_ordinal);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn add_model_turn_audit_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS model_turns (
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 model_json TEXT,
                 usage_json TEXT,
                 estimated_cost_usd_nanos INTEGER,
                 completed_at_ms INTEGER,
                 PRIMARY KEY(run_id, turn_ordinal)
             );",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    for (column, declaration) in [
        ("model_json", "TEXT"),
        ("usage_json", "TEXT"),
        ("estimated_cost_usd_nanos", "INTEGER"),
        ("completed_at_ms", "INTEGER"),
    ] {
        if !has_column(connection, "model_turns", column)? {
            connection
                .execute(
                    &format!("ALTER TABLE model_turns ADD COLUMN {column} {declaration}"),
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
    }
    Ok(())
}

fn validate_model_turn_audit_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for column in [
        "run_id",
        "turn_ordinal",
        "assistant_content_json",
        "model_json",
        "usage_json",
        "estimated_cost_usd_nanos",
        "completed_at_ms",
    ] {
        if !has_column(connection, "model_turns", column)? {
            return Err(SessionRuntimeError::Persistence);
        }
    }
    Ok(())
}

/// Adds `messages.turn_ordinal` for stores created before per-turn assistant
/// messages. Existing rows keep 0: legacy runs render as one message with the
/// run's calls grouped after it.
fn add_messages_turn_ordinal_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "messages", "turn_ordinal")? {
        connection
            .execute(
                "ALTER TABLE messages ADD COLUMN turn_ordinal INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds `tool_calls.display_json` for stores created before tool calls
/// carried a UI display payload. Existing rows keep NULL: legacy results
/// render from their bounded result string alone.
fn add_tool_calls_display_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "tool_calls", "display_json")? {
        connection
            .execute("ALTER TABLE tool_calls ADD COLUMN display_json TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

fn create_grant_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE session_grants (
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 kind TEXT NOT NULL,
                 value TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, kind, value)
             );",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn create_session_files_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE session_files (
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 path TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, path)
             );",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

/// One committed compaction: the structured summary and the message-ordinal
/// cutoff it replaces. Assembly reads the newest row; older rows are bounded
/// history retained for a future rollback command.
fn create_session_compactions_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE session_compactions (
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 run_id TEXT NOT NULL,
                 summary TEXT NOT NULL,
                 cutoff_ordinal INTEGER NOT NULL,
                 before_bytes INTEGER NOT NULL,
                 after_bytes INTEGER NOT NULL,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE INDEX session_compactions_session
                 ON session_compactions(session_id, created_at_ms DESC);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

/// Adds `runs.kind` for stores created before internal compaction runs.
/// Existing rows keep 'prompt'.
fn add_runs_kind_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "kind")? {
        connection
            .execute(
                "ALTER TABLE runs ADD COLUMN kind TEXT NOT NULL DEFAULT 'prompt'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds `runs.context_tokens` for stores created before the last model
/// turn's input-token total was persisted separately from the run's summed
/// billing usage. Existing rows keep NULL: clients fall back to the sum.
fn add_runs_context_tokens_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "context_tokens")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN context_tokens INTEGER", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds authoritative session occupancy without guessing from historical
/// run billing totals. Existing sessions remain unknown until a prompt turn
/// reports usage.
fn add_sessions_context_tokens_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "context_tokens")? {
        connection
            .execute("ALTER TABLE sessions ADD COLUMN context_tokens INTEGER", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Records the parent run that owns a spawned child. Publicly created child
/// sessions and historical rows remain unowned (NULL).
fn add_sessions_owner_run_id_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "owner_run_id")? {
        connection
            .execute("ALTER TABLE sessions ADD COLUMN owner_run_id TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds `runs.auto_compaction` for stores created before threshold-triggered
/// compaction. 1 marks a compaction run the runtime claimed automatically at
/// a context threshold; 0 (every existing row) is a user-requested run.
fn add_runs_auto_compaction_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "auto_compaction")? {
        connection
            .execute(
                "ALTER TABLE runs ADD COLUMN auto_compaction INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds the stable prompt and workspace-instruction identity prepared before
/// provider work. Existing runs remain unknown rather than being assigned the
/// current prompt after the fact.
fn add_runs_prompt_identity_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "prompt_identity_json")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN prompt_identity_json TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

pub(in crate::sessions) fn has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, SessionRuntimeError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(columns.iter().any(|candidate| candidate == column))
}
