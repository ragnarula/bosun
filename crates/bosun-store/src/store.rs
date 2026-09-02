//! The control plane's SQLite session store. Synchronous rusqlite calls
//! wrapped for async use: every method runs its work on a blocking thread and
//! returns a future, so the store is usable from axum handlers and the agent
//! loop.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Context;
use bosun_common::session::Block;
use bosun_common::session::Event;
use bosun_common::session::Message;
use bosun_common::session::Permission;
use bosun_common::session::Role;
use bosun_common::session::Session;
use bosun_common::session::SessionState;
use rusqlite::params;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
    #[error("session {id} was not found")]
    SessionNotFound { id: String },
}

/// One recorded model call. `started_at_secs` is when the call was appended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCall {
    pub id: i64,
    pub session_id: String,
    pub model: String,
    pub provider: String,
    pub kind: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost: Option<f64>,
    pub started_at_secs: i64,
}

/// One recorded tool call. `result` and `is_error` are set by
/// `complete_tool_call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: i64,
    pub session_id: String,
    pub call_id: String,
    pub name: String,
    pub args: Value,
    pub result: Option<Value>,
    pub is_error: bool,
}

/// The sessions table is the source of truth for sessions; the node registry
/// keeps only liveness. rusqlite's Connection is not Sync, so one connection
/// is shared behind a std Mutex and serialized through it.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  node TEXT NOT NULL,
  repo_url TEXT,
  git_ref TEXT,
  dir TEXT NOT NULL,
  model TEXT NOT NULL,
  persona TEXT,
  permission TEXT NOT NULL,
  allowed_tools TEXT NOT NULL DEFAULT '*',
  state TEXT NOT NULL,
  created_at_secs INTEGER NOT NULL,
  prompt TEXT
);
CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  role TEXT NOT NULL,
  block TEXT NOT NULL,
  archived INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  call_id TEXT NOT NULL,
  name TEXT NOT NULL,
  args TEXT NOT NULL,
  result TEXT,
  is_error INTEGER NOT NULL DEFAULT 0,
  UNIQUE(session_id, call_id)
);
CREATE TABLE IF NOT EXISTS model_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  model TEXT NOT NULL,
  provider TEXT NOT NULL,
  kind TEXT NOT NULL,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cost REAL,
  started_at_secs INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);
";

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create data directory {}", parent.display()))?;
        }
        let conn = rusqlite::Connection::open(path).context("failed to open database")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable WAL mode")?;
        // Deletions of sessions remove dependent rows explicitly in
        // remove_session, so foreign keys stay off.
        conn.pragma_update(None, "foreign_keys", "OFF")
            .context("failed to disable foreign keys")?;
        conn.execute_batch(SCHEMA)
            .context("failed to create tables")?;
        // Additive migrations for databases created by an older schema. The
        // column list is checked first, so the migration does not depend on
        // the wording of SQLite's duplicate-column error. Old rows then mean
        // what the defaults say: '*' for allowed_tools, no persona.
        if !column_exists(&conn, "sessions", "allowed_tools")? {
            conn.execute(
                "ALTER TABLE sessions ADD COLUMN allowed_tools TEXT NOT NULL DEFAULT '*'",
                [],
            )
            .context("failed to add the allowed_tools column")?;
        }
        if !column_exists(&conn, "sessions", "persona")? {
            conn.execute("ALTER TABLE sessions ADD COLUMN persona TEXT", [])
                .context("failed to add the persona column")?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs `f` against the shared connection on a blocking thread.
    async fn with_conn<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, anyhow::Error> + Send + 'static,
    {
        blocking(self.conn.clone(), f).await
    }

    /// Runs `f` on the shared connection after checking the session exists.
    async fn with_session<T, F>(&self, session_id: &str, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection, &str) -> Result<T, anyhow::Error> + Send + 'static,
    {
        let session_id = session_id.to_string();
        blocking(self.conn.clone(), move |conn| {
            ensure_session_exists(conn, &session_id)?;
            f(conn, &session_id)
        })
        .await
    }

    pub async fn create_session(&self, session: &Session) -> Result<(), StoreError> {
        let session = session.clone();
        self.with_conn(move |conn| {
            let permission = serde_json::to_string(&session.permission)?;
            let state = serde_json::to_string(&session.state)?;
            conn.execute(
                "INSERT INTO sessions (id, node, repo_url, git_ref, dir, model, persona, permission, allowed_tools, state, created_at_secs, prompt)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    session.id,
                    session.node,
                    session.repo_url,
                    session.git_ref,
                    session.dir,
                    session.model,
                    session.persona,
                    permission,
                    session.allowed_tools,
                    state,
                    session.created_at_secs,
                    session.prompt,
                ],
            )
            .context("failed to insert session")?;
            Ok(())
        })
        .await
    }

    pub async fn get_session(&self, id: &str) -> Result<Option<Session>, StoreError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, node, repo_url, git_ref, dir, model, persona, permission, allowed_tools, state, created_at_secs, prompt
                     FROM sessions WHERE id = ?1",
                )
                .context("failed to prepare session query")?;
            let mut rows = stmt.query([id]).context("failed to query session")?;
            match rows.next().context("failed to read session row")? {
                Some(row) => Ok(Some(session_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    pub async fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, node, repo_url, git_ref, dir, model, persona, permission, allowed_tools, state, created_at_secs, prompt
                     FROM sessions ORDER BY id",
                )
                .context("failed to prepare session list query")?;
            let mut rows = stmt.query([]).context("failed to query sessions")?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next().context("failed to read session row")? {
                sessions.push(session_from_row(row)?);
            }
            Ok(sessions)
        })
        .await
    }

    /// Updates the session state and emits the matching `Event::State` in one
    /// transaction, so the SSE stream and the sessions table never diverge.
    /// Callers must not append the event separately.
    pub async fn set_state(&self, id: &str, state: SessionState) -> Result<(), StoreError> {
        self.with_session(id, move |conn, session_id| {
            let state_json = serde_json::to_string(&state)?;
            let tx = transaction(conn)?;
            tx.execute(
                "UPDATE sessions SET state = ?1 WHERE id = ?2",
                params![state_json, session_id],
            )
            .context("failed to update session state")?;
            append_event(&tx, session_id, "state", &Event::State { state })?;
            tx.commit().context("failed to commit state change")?;
            Ok(())
        })
        .await
    }

    /// Updates the session permission and emits the matching
    /// `Event::Permission` in one transaction. Callers must not append the
    /// event separately.
    pub async fn set_permission(&self, id: &str, permission: Permission) -> Result<(), StoreError> {
        self.with_session(id, move |conn, session_id| {
            let permission_json = serde_json::to_string(&permission)?;
            let tx = transaction(conn)?;
            tx.execute(
                "UPDATE sessions SET permission = ?1 WHERE id = ?2",
                params![permission_json, session_id],
            )
            .context("failed to update session permission")?;
            append_event(
                &tx,
                session_id,
                "permission",
                &Event::Permission { permission },
            )?;
            tx.commit().context("failed to commit permission change")?;
            Ok(())
        })
        .await
    }

    /// Applies a persona switch: the session's persona, model, permission and
    /// allowed-tools spec are replaced in one transaction, so the event
    /// stream and the sessions table never diverge. The `persona` event is
    /// always emitted; when the new permission differs from the stored one,
    /// the matching `Event::Permission` is emitted in the same transaction.
    /// Callers must not append either event separately.
    pub async fn switch_persona(
        &self,
        id: &str,
        persona: &str,
        model: &str,
        permission: Permission,
        allowed_tools: &str,
    ) -> Result<(), StoreError> {
        let persona = persona.to_string();
        let model = model.to_string();
        let allowed_tools = allowed_tools.to_string();
        self.with_session(id, move |conn, session_id| {
            let permission_json = serde_json::to_string(&permission)?;
            let tx = transaction(conn)?;
            let previous: String = tx
                .query_row(
                    "SELECT permission FROM sessions WHERE id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .context("failed to read the stored permission")?;
            tx.execute(
                "UPDATE sessions SET persona = ?1, model = ?2, permission = ?3, allowed_tools = ?4
                 WHERE id = ?5",
                params![persona, model, permission_json, allowed_tools, session_id],
            )
            .context("failed to update the session persona")?;
            append_event(&tx, session_id, "persona", &Event::Persona { persona })?;
            if previous != permission_json {
                append_event(
                    &tx,
                    session_id,
                    "permission",
                    &Event::Permission { permission },
                )?;
            }
            tx.commit().context("failed to commit persona change")?;
            Ok(())
        })
        .await
    }

    /// Appends the message and the matching `Event::Message` in one
    /// transaction, so the SSE stream and the transcript never diverge.
    pub async fn append_message(
        &self,
        session_id: &str,
        role: Role,
        block: &Block,
    ) -> Result<i64, StoreError> {
        let block = block.clone();
        self.with_session(session_id, move |conn, session_id| {
            let message = Message { role, block };
            let tx = transaction(conn)?;
            tx.execute(
                "INSERT INTO messages (session_id, role, block) VALUES (?1, ?2, ?3)",
                params![
                    session_id,
                    serde_json::to_string(&message.role)?,
                    serde_json::to_string(&message.block)?,
                ],
            )
            .context("failed to insert message")?;
            let message_id = tx.last_insert_rowid();
            append_event(&tx, session_id, "message", &Event::Message { message })?;
            tx.commit().context("failed to commit message")?;
            Ok(message_id)
        })
        .await
    }

    /// Appends an arbitrary event. The store's own write methods emit their
    /// matching events, so this is only for events they do not produce.
    pub async fn append_event(&self, session_id: &str, event: &Event) -> Result<i64, StoreError> {
        let event = event.clone();
        self.with_session(session_id, move |conn, session_id| {
            append_event(conn, session_id, "event", &event)
        })
        .await
    }

    pub async fn events_after(
        &self,
        session_id: &str,
        after: i64,
    ) -> Result<Vec<(i64, Event)>, StoreError> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, payload FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq",
                )
                .context("failed to prepare event query")?;
            let mut rows = stmt
                .query(params![session_id, after])
                .context("failed to query events")?;
            let mut events = Vec::new();
            while let Some(row) = rows.next().context("failed to read event row")? {
                let seq: i64 = row.get("seq")?;
                let payload: String = row.get("payload")?;
                let event: Event =
                    serde_json::from_str(&payload).context("failed to parse event payload")?;
                events.push((seq, event));
            }
            Ok(events)
        })
        .await
    }

    /// Returns the session's messages with their row ids, in insertion order.
    /// The id lets compaction compute the archive boundary after a restart.
    pub async fn messages(
        &self,
        session_id: &str,
        include_archived: bool,
    ) -> Result<Vec<(i64, Message)>, StoreError> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = if include_archived {
                conn.prepare(
                    "SELECT id, role, block FROM messages WHERE session_id = ?1 ORDER BY id",
                )
            } else {
                conn.prepare(
                    "SELECT id, role, block FROM messages WHERE session_id = ?1 AND archived = 0 ORDER BY id",
                )
            }
            .context("failed to prepare message query")?;
            let mut rows = stmt.query([session_id]).context("failed to query messages")?;
            let mut messages = Vec::new();
            while let Some(row) = rows.next().context("failed to read message row")? {
                let id: i64 = row.get("id")?;
                let role: String = row.get("role")?;
                let block: String = row.get("block")?;
                messages.push((
                    id,
                    Message {
                        role: serde_json::from_str(&role).context("failed to parse role")?,
                        block: serde_json::from_str(&block).context("failed to parse block")?,
                    },
                ));
            }
            Ok(messages)
        })
        .await
    }

    pub async fn mark_archived(
        &self,
        session_id: &str,
        upto_message_id: i64,
    ) -> Result<(), StoreError> {
        self.with_session(session_id, move |conn, session_id| {
            conn.execute(
                "UPDATE messages SET archived = 1 WHERE session_id = ?1 AND id <= ?2",
                params![session_id, upto_message_id],
            )
            .context("failed to archive messages")?;
            Ok(())
        })
        .await
    }

    /// Records the tool call row only; the agent loop emits the tool-call
    /// transcript message itself via `append_message`, so no event is written
    /// here or in `complete_tool_call`.
    pub async fn append_tool_call(
        &self,
        session_id: &str,
        call_id: &str,
        name: &str,
        args: &Value,
    ) -> Result<i64, StoreError> {
        let call_id = call_id.to_string();
        let name = name.to_string();
        let args = args.clone();
        self.with_session(session_id, move |conn, session_id| {
            conn.execute(
                "INSERT INTO tool_calls (session_id, call_id, name, args) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, call_id, name, serde_json::to_string(&args)?],
            )
            .context("failed to insert tool call")?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    /// Fills in the result of the tool call identified by the session and the
    /// call id; the UNIQUE(session_id, call_id) index keeps the update scoped
    /// to that one row. See `append_tool_call` for why no event is written.
    pub async fn complete_tool_call(
        &self,
        session_id: &str,
        call_id: &str,
        result: &Value,
        is_error: bool,
    ) -> Result<(), StoreError> {
        let call_id = call_id.to_string();
        let result = result.clone();
        self.with_session(session_id, move |conn, session_id| {
            conn.execute(
                "UPDATE tool_calls SET result = ?1, is_error = ?2 WHERE session_id = ?3 AND call_id = ?4",
                params![serde_json::to_string(&result)?, is_error, session_id, call_id],
            )
            .context("failed to complete tool call")?;
            Ok(())
        })
        .await
    }

    pub async fn tool_calls(&self, session_id: &str) -> Result<Vec<ToolCall>, StoreError> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, call_id, name, args, result, is_error
                     FROM tool_calls WHERE session_id = ?1 ORDER BY id",
                )
                .context("failed to prepare tool call query")?;
            let mut rows = stmt
                .query([session_id])
                .context("failed to query tool calls")?;
            let mut calls = Vec::new();
            while let Some(row) = rows.next().context("failed to read tool call row")? {
                let args: String = row.get("args")?;
                let result: Option<String> = row.get("result")?;
                calls.push(ToolCall {
                    id: row.get("id")?,
                    session_id: row.get("session_id")?,
                    call_id: row.get("call_id")?,
                    name: row.get("name")?,
                    args: serde_json::from_str(&args).context("failed to parse tool call args")?,
                    result: result
                        .map(|raw| {
                            serde_json::from_str(&raw).context("failed to parse tool call result")
                        })
                        .transpose()?,
                    is_error: row.get("is_error")?,
                });
            }
            Ok(calls)
        })
        .await
    }

    // The argument count is fixed by the store API: one value per column.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_model_call(
        &self,
        session_id: &str,
        model: &str,
        provider: &str,
        kind: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost: Option<f64>,
    ) -> Result<i64, StoreError> {
        let model = model.to_string();
        let provider = provider.to_string();
        let kind = kind.to_string();
        self.with_session(session_id, move |conn, session_id| {
            let started_at_secs = bosun_common::time::unix_secs(SystemTime::now());
            let tx = transaction(conn)?;
            tx.execute(
                "INSERT INTO model_calls (session_id, model, provider, kind, input_tokens, output_tokens, cost, started_at_secs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session_id,
                    model,
                    provider,
                    kind,
                    input_tokens,
                    output_tokens,
                    cost,
                    started_at_secs,
                ],
            )
            .context("failed to insert model call")?;
            let model_call_id = tx.last_insert_rowid();
            append_event(&tx, session_id, "model call", &Event::ModelCall {
                model,
                provider,
                kind,
                input_tokens,
                output_tokens,
                cost,
            })?;
            tx.commit().context("failed to commit model call")?;
            Ok(model_call_id)
        })
        .await
    }

    pub async fn model_calls(&self, session_id: &str) -> Result<Vec<ModelCall>, StoreError> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, model, provider, kind, input_tokens, output_tokens, cost, started_at_secs
                     FROM model_calls WHERE session_id = ?1 ORDER BY id",
                )
                .context("failed to prepare model call query")?;
            let mut rows = stmt
                .query([session_id])
                .context("failed to query model calls")?;
            let mut calls = Vec::new();
            while let Some(row) = rows.next().context("failed to read model call row")? {
                calls.push(ModelCall {
                    id: row.get("id")?,
                    session_id: row.get("session_id")?,
                    model: row.get("model")?,
                    provider: row.get("provider")?,
                    kind: row.get("kind")?,
                    input_tokens: row.get("input_tokens")?,
                    output_tokens: row.get("output_tokens")?,
                    cost: row.get("cost")?,
                    started_at_secs: row.get("started_at_secs")?,
                });
            }
            Ok(calls)
        })
        .await
    }

    /// Deletes the session and its messages, events, tool calls and model
    /// calls in one transaction.
    pub async fn remove_session(&self, id: &str) -> Result<(), StoreError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let tx = transaction(conn)?;
            tx.execute("DELETE FROM events WHERE session_id = ?1", [&id])
                .context("failed to delete session events")?;
            tx.execute("DELETE FROM messages WHERE session_id = ?1", [&id])
                .context("failed to delete session messages")?;
            tx.execute("DELETE FROM tool_calls WHERE session_id = ?1", [&id])
                .context("failed to delete session tool calls")?;
            tx.execute("DELETE FROM model_calls WHERE session_id = ?1", [&id])
                .context("failed to delete session model calls")?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", [&id])
                .context("failed to delete session")?;
            tx.commit().context("failed to commit session removal")?;
            Ok(())
        })
        .await
    }
}

fn transaction(
    conn: &mut rusqlite::Connection,
) -> Result<rusqlite::Transaction<'_>, anyhow::Error> {
    conn.transaction().context("failed to begin transaction")
}

fn append_event(
    conn: &rusqlite::Connection,
    session_id: &str,
    label: &str,
    event: &Event,
) -> Result<i64, anyhow::Error> {
    conn.execute(
        "INSERT INTO events (session_id, payload) VALUES (?1, ?2)",
        params![session_id, serde_json::to_string(event)?],
    )
    .with_context(|| format!("failed to append {label} event"))?;
    Ok(conn.last_insert_rowid())
}

/// Runs `f` against the shared connection on a blocking thread, converting the
/// join error and the inner error into `StoreError`. Keeps the store usable
/// from async code without blocking the runtime.
async fn blocking<T, F>(conn: Arc<Mutex<rusqlite::Connection>>, f: F) -> Result<T, StoreError>
where
    T: Send + 'static,
    F: FnOnce(&mut rusqlite::Connection) -> Result<T, anyhow::Error> + Send + 'static,
{
    let result = tokio::task::spawn_blocking(move || {
        let mut guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    })
    .await
    .context("store task failed")?;
    match result {
        Ok(value) => Ok(value),
        // A store error (a missing session) is returned verbatim instead of
        // being wrapped in Internal.
        Err(error) => match error.downcast::<StoreError>() {
            Ok(store_error) => Err(store_error),
            Err(error) => Err(StoreError::Internal(error)),
        },
    }
}

/// Checks that the session row exists, so writes to dependent tables fail
/// with `SessionNotFound` instead of silently inserting orphan rows.
fn ensure_session_exists(conn: &rusqlite::Connection, id: &str) -> Result<(), anyhow::Error> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )
        .context("failed to check session existence")?;
    if exists {
        Ok(())
    } else {
        Err(anyhow::Error::new(StoreError::SessionNotFound {
            id: id.to_string(),
        }))
    }
}

/// Whether `column` is one of `table`'s columns, for additive migrations.
fn column_exists(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, anyhow::Error> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .context("failed to prepare the table_info query")?;
    let mut rows = stmt.query([]).context("failed to query table_info")?;
    while let Some(row) = rows.next().context("failed to read table_info row")? {
        let name: String = row.get("name").context("table_info has no name column")?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn session_from_row(row: &rusqlite::Row) -> Result<Session, anyhow::Error> {
    let permission: String = row.get("permission")?;
    let state: String = row.get("state")?;
    Ok(Session {
        id: row.get("id")?,
        node: row.get("node")?,
        repo_url: row.get("repo_url")?,
        git_ref: row.get("git_ref")?,
        dir: row.get("dir")?,
        model: row.get("model")?,
        persona: row.get("persona")?,
        permission: serde_json::from_str(&permission).context("failed to parse permission")?,
        allowed_tools: row.get("allowed_tools")?,
        state: serde_json::from_str(&state).context("failed to parse state")?,
        created_at_secs: row.get("created_at_secs")?,
        prompt: row.get("prompt")?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            node: "node-1".to_string(),
            repo_url: Some("https://github.com/example/repo".to_string()),
            git_ref: Some("main".to_string()),
            dir: "/work/repo".to_string(),
            model: "claude".to_string(),
            persona: Some("coder".to_string()),
            permission: Permission::ReadWrite,
            allowed_tools: "shell, file/read".to_string(),
            state: SessionState::Running,
            created_at_secs: 1_700_000_000,
            prompt: Some("finish the feature".to_string()),
        }
    }

    fn assert_session_eq(actual: &Session, expected: &Session) {
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.node, expected.node);
        assert_eq!(actual.repo_url, expected.repo_url);
        assert_eq!(actual.git_ref, expected.git_ref);
        assert_eq!(actual.dir, expected.dir);
        assert_eq!(actual.model, expected.model);
        assert_eq!(actual.persona, expected.persona);
        assert_eq!(actual.permission, expected.permission);
        assert_eq!(actual.allowed_tools, expected.allowed_tools);
        assert_eq!(actual.state, expected.state);
        assert_eq!(actual.created_at_secs, expected.created_at_secs);
        assert_eq!(actual.prompt, expected.prompt);
    }

    #[tokio::test]
    async fn open_creates_file_enables_wal_and_creates_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let store = Store::open(&path).unwrap();

        assert!(path.exists());

        let conn = rusqlite::Connection::open(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");

        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap();
        let mut tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables.sort();
        assert_eq!(
            tables,
            [
                "events",
                "messages",
                "model_calls",
                "sessions",
                "tool_calls"
            ]
        );

        drop(store);
    }

    #[tokio::test]
    async fn sessions_round_trip_and_list_is_sorted_by_id() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();

        store.create_session(&session("b")).await.unwrap();
        store.create_session(&session("a")).await.unwrap();
        store.create_session(&session("c")).await.unwrap();

        assert_session_eq(
            &store.get_session("a").await.unwrap().unwrap(),
            &session("a"),
        );
        assert!(store.get_session("missing").await.unwrap().is_none());

        let listed = store.list_sessions().await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        for s in &listed {
            assert_session_eq(s, &session(&s.id));
        }
    }

    #[tokio::test]
    async fn set_state_and_set_permission_persist() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();

        store.create_session(&session("a")).await.unwrap();

        store
            .set_state("a", SessionState::WaitingForInput)
            .await
            .unwrap();
        store
            .set_permission("a", Permission::ReadOnly)
            .await
            .unwrap();

        let s = store.get_session("a").await.unwrap().unwrap();
        assert_eq!(s.state, SessionState::WaitingForInput);
        assert_eq!(s.permission, Permission::ReadOnly);

        let events = store.events_after("a", 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0].1, Event::State { state } if *state == SessionState::WaitingForInput)
        );
        assert!(
            matches!(&events[1].1, Event::Permission { permission } if *permission == Permission::ReadOnly)
        );
    }

    #[tokio::test]
    async fn switch_persona_replaces_the_persona_fields_in_one_row() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        store
            .switch_persona(
                "a",
                "reviewer",
                "cheap",
                Permission::ReadOnly,
                "file/read, grep",
            )
            .await
            .unwrap();

        let stored = store.get_session("a").await.unwrap().unwrap();
        assert_eq!(stored.persona.as_deref(), Some("reviewer"));
        assert_eq!(stored.model, "cheap");
        assert_eq!(stored.permission, Permission::ReadOnly);
        assert_eq!(stored.allowed_tools, "file/read, grep");
        assert_eq!(
            stored.state,
            session("a").state,
            "a switch touches only the persona fields"
        );
    }

    #[tokio::test]
    async fn switch_persona_emits_a_persona_event_and_a_permission_event_on_change() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        store
            .switch_persona("a", "reviewer", "cheap", Permission::ReadOnly, "file/read")
            .await
            .unwrap();

        let events = store.events_after("a", 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0].1,
            Event::Persona { persona } if persona == "reviewer"
        ));
        assert!(matches!(
            &events[1].1,
            Event::Permission { permission } if *permission == Permission::ReadOnly
        ));
    }

    #[tokio::test]
    async fn switch_persona_with_the_same_permission_emits_no_permission_event() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        store
            .switch_persona("a", "architect", "claude", Permission::ReadWrite, "*")
            .await
            .unwrap();

        let stored = store.get_session("a").await.unwrap().unwrap();
        assert_eq!(stored.persona.as_deref(), Some("architect"));
        assert_eq!(stored.permission, Permission::ReadWrite);

        let events = store.events_after("a", 0).await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].1,
            Event::Persona { persona } if persona == "architect"
        ));
    }

    #[tokio::test]
    async fn switch_persona_on_a_missing_session_is_session_not_found() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();

        let error = store
            .switch_persona("ghost", "reviewer", "cheap", Permission::ReadOnly, "*")
            .await
            .unwrap_err();
        assert!(matches!(error, StoreError::SessionNotFound { id } if id == "ghost"));
    }

    #[tokio::test]
    async fn append_message_inserts_message_and_event() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        let first = store
            .append_message(
                "a",
                Role::User,
                &Block::Text {
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();
        let second = store
            .append_message(
                "a",
                Role::Assistant,
                &Block::ToolCall {
                    id: "call-1".into(),
                    name: "shell".into(),
                    args: json!({"command": "ls"}),
                },
            )
            .await
            .unwrap();
        assert!(second > first);

        let events = store.events_after("a", 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].0, events[1].0), (1, 2));
        assert!(matches!(&events[0].1, Event::Message { message } if message.role == Role::User));
        assert!(
            matches!(&events[1].1, Event::Message { message } if message.role == Role::Assistant)
        );

        let messages = store.messages("a", true).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!((messages[0].0, messages[1].0), (first, second));
        assert_eq!(messages[0].1.role, Role::User);
        assert!(matches!(&messages[0].1.block, Block::Text { text } if text == "hello"));
        assert_eq!(messages[1].1.role, Role::Assistant);
    }

    #[tokio::test]
    async fn events_after_replays_only_newer_events() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        for text in ["one", "two", "three"] {
            store
                .append_message("a", Role::User, &Block::Text { text: text.into() })
                .await
                .unwrap();
        }

        let events = store.events_after("a", 1).await.unwrap();
        let seqs: Vec<i64> = events.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, [2, 3]);
        assert!(store.events_after("a", 3).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mark_archived_hides_archived_messages() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        let old = store
            .append_message("a", Role::User, &Block::Text { text: "old".into() })
            .await
            .unwrap();
        store
            .append_message("a", Role::Assistant, &Block::Text { text: "new".into() })
            .await
            .unwrap();

        store.mark_archived("a", old).await.unwrap();

        let active = store.messages("a", false).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, old + 1);
        assert!(matches!(&active[0].1.block, Block::Text { text } if text == "new"));

        let all = store.messages("a", true).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].0, all[1].0), (old, old + 1));
    }

    #[tokio::test]
    async fn tool_calls_append_and_complete_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let store = Store::open(&path).unwrap();
        store.create_session(&session("a")).await.unwrap();

        let id = store
            .append_tool_call("a", "call-1", "shell", &json!({"command": "ls"}))
            .await
            .unwrap();
        store
            .complete_tool_call("a", "call-1", &json!({"exit": 0}), false)
            .await
            .unwrap();

        let calls = store.tool_calls("a").await.unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.id, id);
        assert_eq!(call.session_id, "a");
        assert_eq!(call.call_id, "call-1");
        assert_eq!(call.name, "shell");
        assert_eq!(call.args, json!({"command": "ls"}));
        assert_eq!(call.result, Some(json!({"exit": 0})));
        assert!(!call.is_error);

        // Tool calls stay row-only: no transcript events are written.
        assert!(store.events_after("a", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn model_calls_append_and_list_round_trip() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let id = store
            .append_model_call(
                "a",
                "claude",
                "anthropic",
                "completion",
                Some(100),
                Some(50),
                Some(0.001),
            )
            .await
            .unwrap();

        let calls = store.model_calls("a").await.unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.id, id);
        assert_eq!(call.session_id, "a");
        assert_eq!(call.model, "claude");
        assert_eq!(call.provider, "anthropic");
        assert_eq!(call.kind, "completion");
        assert_eq!(call.input_tokens, Some(100));
        assert_eq!(call.output_tokens, Some(50));
        assert_eq!(call.cost, Some(0.001));
        assert!(call.started_at_secs >= before);

        let events = store.events_after("a", 0).await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].1,
            Event::ModelCall {
                model,
                provider,
                kind,
                input_tokens,
                output_tokens,
                cost,
            } if model == "claude"
                && provider == "anthropic"
                && kind == "completion"
                && *input_tokens == Some(100)
                && *output_tokens == Some(50)
                && *cost == Some(0.001)
        ));
    }

    #[tokio::test]
    async fn remove_session_deletes_session_and_dependent_rows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let store = Store::open(&path).unwrap();
        store.create_session(&session("a")).await.unwrap();
        store.create_session(&session("b")).await.unwrap();

        store
            .append_message(
                "a",
                Role::User,
                &Block::Text {
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();
        store.set_state("a", SessionState::Running).await.unwrap();
        store
            .append_tool_call("a", "call-1", "shell", &json!({"command": "ls"}))
            .await
            .unwrap();
        store
            .append_model_call("a", "claude", "anthropic", "completion", None, None, None)
            .await
            .unwrap();

        store.remove_session("a").await.unwrap();

        assert!(store.get_session("a").await.unwrap().is_none());
        assert!(store.messages("a", true).await.unwrap().is_empty());
        assert!(store.events_after("a", 0).await.unwrap().is_empty());
        assert!(store.model_calls("a").await.unwrap().is_empty());
        assert!(store.get_session("b").await.unwrap().is_some());

        let conn = rusqlite::Connection::open(&path).unwrap();
        let tool_calls: i64 = conn
            .query_row("SELECT COUNT(*) FROM tool_calls", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tool_calls, 0);
    }

    #[tokio::test]
    async fn data_survives_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");

        {
            let store = Store::open(&path).unwrap();
            store.create_session(&session("a")).await.unwrap();
            store
                .append_message(
                    "a",
                    Role::User,
                    &Block::Text {
                        text: "hello".into(),
                    },
                )
                .await
                .unwrap();
            store
                .append_model_call(
                    "a",
                    "claude",
                    "anthropic",
                    "completion",
                    Some(1),
                    Some(2),
                    None,
                )
                .await
                .unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_session_eq(
            &store.get_session("a").await.unwrap().unwrap(),
            &session("a"),
        );
        assert_eq!(store.messages("a", true).await.unwrap().len(), 1);
        assert_eq!(store.events_after("a", 0).await.unwrap().len(), 2);
        assert_eq!(store.model_calls("a").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn writes_to_missing_session_return_session_not_found() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();

        let err = store
            .append_message("missing", Role::User, &Block::Text { text: "hi".into() })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::SessionNotFound { id } if id == "missing"),
            "unexpected error: {err}"
        );

        let err = store
            .set_state("missing", SessionState::Running)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound { ref id } if id == "missing"));

        let err = store
            .set_permission("missing", Permission::ReadOnly)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound { ref id } if id == "missing"));

        let err = store
            .append_event(
                "missing",
                &Event::State {
                    state: SessionState::Running,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound { ref id } if id == "missing"));

        let err = store
            .append_tool_call("missing", "call-1", "shell", &json!({"command": "ls"}))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound { ref id } if id == "missing"));

        let err = store
            .complete_tool_call("missing", "call-1", &json!({"exit": 0}), false)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound { ref id } if id == "missing"));

        let err = store
            .append_model_call(
                "missing",
                "claude",
                "anthropic",
                "completion",
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound { ref id } if id == "missing"));

        let err = store.mark_archived("missing", 1).await.unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound { ref id } if id == "missing"));

        // A failed write must not leave any rows behind.
        assert!(store.messages("missing", true).await.unwrap().is_empty());
        assert!(store.events_after("missing", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_database_without_the_allowed_tools_column_is_migrated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                   id TEXT PRIMARY KEY,
                   node TEXT NOT NULL,
                   repo_url TEXT,
                   git_ref TEXT,
                   dir TEXT NOT NULL,
                   model TEXT NOT NULL,
                   permission TEXT NOT NULL,
                   state TEXT NOT NULL,
                   created_at_secs INTEGER NOT NULL,
                   prompt TEXT
                 );
                 INSERT INTO sessions (id, node, dir, model, permission, state, created_at_secs, prompt)
                 VALUES ('old', 'node-1', '/work', 'claude', '\"read_write\"', '\"waiting_for_input\"', 1700000000, NULL);",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let session = store.get_session("old").await.unwrap().unwrap();
        assert_eq!(
            session.allowed_tools, "*",
            "a pre-persona row means every tool is allowed"
        );
        assert_eq!(
            session.persona, None,
            "a pre-persona row has no persona and falls back to the default prompt"
        );
    }

    #[tokio::test]
    async fn a_database_without_the_persona_column_is_migrated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            // The mid-S1 shape: the allowed_tools column exists, persona does
            // not yet.
            conn.execute_batch(
                "CREATE TABLE sessions (
                   id TEXT PRIMARY KEY,
                   node TEXT NOT NULL,
                   repo_url TEXT,
                   git_ref TEXT,
                   dir TEXT NOT NULL,
                   model TEXT NOT NULL,
                   permission TEXT NOT NULL,
                   allowed_tools TEXT NOT NULL DEFAULT '*',
                   state TEXT NOT NULL,
                   created_at_secs INTEGER NOT NULL,
                   prompt TEXT
                 );
                 INSERT INTO sessions (id, node, dir, model, permission, state, created_at_secs, prompt)
                 VALUES ('old', 'node-1', '/work', 'claude', '\"read_write\"', '\"waiting_for_input\"', 1700000000, NULL);",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let session = store.get_session("old").await.unwrap().unwrap();
        assert_eq!(session.allowed_tools, "*");
        assert_eq!(session.persona, None);
    }

    #[tokio::test]
    async fn reopening_an_up_to_date_database_is_a_no_op() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let store = Store::open(&path).unwrap();
            store.create_session(&session("s1")).await.unwrap();
        }
        // A second open must not fail trying to re-add existing columns.
        let store = Store::open(&path).unwrap();
        let session = store.get_session("s1").await.unwrap().unwrap();
        assert_eq!(session.persona.as_deref(), Some("coder"));
    }

    #[tokio::test]
    async fn events_after_returns_only_the_sessions_own_events() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();
        store.create_session(&session("b")).await.unwrap();

        store
            .append_message("a", Role::User, &Block::Text { text: "a1".into() })
            .await
            .unwrap();
        store
            .append_message("b", Role::User, &Block::Text { text: "b1".into() })
            .await
            .unwrap();
        store
            .append_message("a", Role::User, &Block::Text { text: "a2".into() })
            .await
            .unwrap();
        store
            .append_message("b", Role::User, &Block::Text { text: "b2".into() })
            .await
            .unwrap();

        // Global seq values are interleaved, but each session sees only its
        // own events, in seq order.
        let events = store.events_after("a", 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].0, events[1].0), (1, 3));
        assert!(
            matches!(&events[0].1, Event::Message { message } if matches!(&message.block, Block::Text { text } if text == "a1"))
        );
        assert!(
            matches!(&events[1].1, Event::Message { message } if matches!(&message.block, Block::Text { text } if text == "a2"))
        );

        let events = store.events_after("b", 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].0, events[1].0), (2, 4));

        // A session's cursor never sees another session's events, even at the
        // boundary between them.
        assert!(store.events_after("a", 3).await.unwrap().is_empty());
    }
}
