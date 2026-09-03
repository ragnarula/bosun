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
use bosun_common::session::InterruptCause;
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

/// A session's pending raised ask, one per session that raised: the session
/// that surfaced a question awaits its answer. The row names the DIRECT child
/// whose question the session raised, at every level: the one-pending-raise
/// gate's refusal names that child, `message_child` to it clears the row, and
/// its next authored event proves the question closed and clears the row too.
/// `origin_leaf` is the session whose own question the raise carries — where
/// a user's answer routes — which at the root is not the direct child when
/// the question was re-raised up a chain. `ask_message_id` is the surfaced
/// Ask block's row in the raiser's own thread, so the answer can be recorded
/// on it. The row outlives compaction, which archives the Ask block without
/// deleting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    pub session_id: String,
    pub child_id: String,
    pub origin_leaf: String,
    pub question: String,
    pub ask_message_id: i64,
}

/// What routing a user's answer to a pending raised ask did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteAnswer {
    /// No raised ask was pending; the answer was not routed to a session.
    NoBinding,
    /// The answer was appended to the origin leaf's thread; the caller must
    /// wake that leaf's loop.
    Routed { leaf_id: String },
    /// The origin leaf is gone; the stale binding was cleared and the answer
    /// was not routed.
    LeafGone { leaf_id: String },
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
  prompt TEXT,
  parent_id TEXT,
  owner_id TEXT NOT NULL,
  interrupt_cause TEXT
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
CREATE TABLE IF NOT EXISTS pending_asks (
  session_id TEXT PRIMARY KEY,
  child_id TEXT NOT NULL,
  origin_leaf TEXT NOT NULL,
  question TEXT NOT NULL,
  ask_message_id INTEGER NOT NULL
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
        // what the defaults say: '*' for allowed_tools, no persona, and, for
        // rows older than the session tree, no parent and the session as its
        // own owner.
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
        if !column_exists(&conn, "sessions", "parent_id")? {
            conn.execute("ALTER TABLE sessions ADD COLUMN parent_id TEXT", [])
                .context("failed to add the parent_id column")?;
        }
        if !column_exists(&conn, "sessions", "owner_id")? {
            // SQLite cannot add a NOT NULL column without a constant default,
            // so the column is added nullable and existing rows are backfilled
            // with their own id: every pre-tree session is a root and owns
            // itself. New rows always carry an owner.
            conn.execute("ALTER TABLE sessions ADD COLUMN owner_id TEXT", [])
                .context("failed to add the owner_id column")?;
            conn.execute(
                "UPDATE sessions SET owner_id = id WHERE owner_id IS NULL",
                [],
            )
            .context("failed to backfill the owner_id column")?;
        }
        if !column_exists(&conn, "sessions", "interrupt_cause")? {
            conn.execute("ALTER TABLE sessions ADD COLUMN interrupt_cause TEXT", [])
                .context("failed to add the interrupt_cause column")?;
        }
        if !column_exists(&conn, "pending_asks", "origin_leaf")? {
            // Rows written before the origin column held the origin leaf in
            // child_id, so the leaf is copied over. The direct child such a
            // row raised is not recoverable, but the column's job — naming
            // where a user's answer routes — is what child_id held.
            conn.execute("ALTER TABLE pending_asks ADD COLUMN origin_leaf TEXT", [])
                .context("failed to add the origin_leaf column")?;
            conn.execute(
                "UPDATE pending_asks SET origin_leaf = child_id WHERE origin_leaf IS NULL",
                [],
            )
            .context("failed to backfill the origin_leaf column")?;
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
            let interrupt_cause = session
                .interrupt_cause
                .map(|cause| serde_json::to_string(&cause))
                .transpose()?;
            conn.execute(
                "INSERT INTO sessions (id, node, repo_url, git_ref, dir, model, persona, permission, allowed_tools, state, created_at_secs, prompt, parent_id, owner_id, interrupt_cause)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                    session.parent_id,
                    session.owner_id,
                    interrupt_cause,
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
                    "SELECT id, node, repo_url, git_ref, dir, model, persona, permission, allowed_tools, state, created_at_secs, prompt, parent_id, owner_id, interrupt_cause
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
                    "SELECT id, node, repo_url, git_ref, dir, model, persona, permission, allowed_tools, state, created_at_secs, prompt, parent_id, owner_id, interrupt_cause
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

    /// The sessions `parent_id` spawned, ordered by id. The loop reads this
    /// per wake to build the parent's manifest of live children.
    pub async fn child_sessions(&self, parent_id: &str) -> Result<Vec<Session>, StoreError> {
        let parent_id = parent_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, node, repo_url, git_ref, dir, model, persona, permission, allowed_tools, state, created_at_secs, prompt, parent_id, owner_id, interrupt_cause
                     FROM sessions WHERE parent_id = ?1 ORDER BY id",
                )
                .context("failed to prepare child session query")?;
            let mut rows = stmt
                .query([parent_id])
                .context("failed to query child sessions")?;
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

    /// Marks the session interrupted, recording why, and emits the matching
    /// `Event::State` in one transaction, so the SSE stream, the sessions
    /// table and the recorded cause never diverge. Callers must not append
    /// the event separately. A later `set_state` keeps the recorded cause,
    /// which is why the session was last interrupted.
    pub async fn mark_interrupted(
        &self,
        id: &str,
        cause: InterruptCause,
    ) -> Result<(), StoreError> {
        self.with_session(id, move |conn, session_id| {
            let cause_json = serde_json::to_string(&cause)?;
            let tx = transaction(conn)?;
            tx.execute(
                "UPDATE sessions SET state = ?1, interrupt_cause = ?2 WHERE id = ?3",
                params![
                    serde_json::to_string(&SessionState::Interrupted)?,
                    cause_json,
                    session_id,
                ],
            )
            .context("failed to mark the session interrupted")?;
            append_event(
                &tx,
                session_id,
                "state",
                &Event::State {
                    state: SessionState::Interrupted,
                },
            )?;
            tx.commit().context("failed to commit the interrupt")?;
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
            let message_id = insert_message(&tx, session_id, &message)?;
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

    /// The session's most recent message, when it has one. The loop reads
    /// this to validate that a child a root wants to bind actually has its
    /// own unanswered question at the end of its thread.
    pub async fn last_message(&self, session_id: &str) -> Result<Option<Message>, StoreError> {
        self.with_session(session_id, move |conn, session_id| {
            let row = conn.query_row(
                "SELECT role, block FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                [session_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            );
            match row {
                Ok((role, block)) => Ok(Some(Message {
                    role: serde_json::from_str(&role).context("failed to parse role")?,
                    block: serde_json::from_str(&block).context("failed to parse block")?,
                })),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .await
    }

    /// The session's pending raised ask — the direct child whose question it
    /// surfaced, that question's origin leaf, and the surfaced Ask block's
    /// row — when one is.
    pub async fn get_pending_ask(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingAsk>, StoreError> {
        self.with_session(session_id, move |conn, session_id| {
            read_pending_ask(conn, session_id)
        })
        .await
    }

    /// Records that the session raised `question` as an Ask block in its own
    /// thread at `ask_message_id`, bound to the direct child `child_id` whose
    /// question it raised and to that question's `origin_leaf`. There is one
    /// raised ask per session, so recording again replaces the earlier one.
    pub async fn set_pending_ask(
        &self,
        session_id: &str,
        child_id: &str,
        origin_leaf: &str,
        question: &str,
        ask_message_id: i64,
    ) -> Result<(), StoreError> {
        let child_id = child_id.to_string();
        let origin_leaf = origin_leaf.to_string();
        let question = question.to_string();
        self.with_session(session_id, move |conn, session_id| {
            conn.execute(
                "INSERT INTO pending_asks (session_id, child_id, origin_leaf, question, ask_message_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id) DO UPDATE SET
                   child_id = excluded.child_id,
                   origin_leaf = excluded.origin_leaf,
                   question = excluded.question,
                   ask_message_id = excluded.ask_message_id",
                params![session_id, child_id, origin_leaf, question, ask_message_id],
            )
            .context("failed to store the pending ask")?;
            Ok(())
        })
        .await
    }

    /// Drops the session's pending raised ask, when one is. Called when the
    /// ask is answered, when the raised child's next authored event proves
    /// its question closed, or when the session's model takes the raised ask
    /// over by messaging its raised child.
    pub async fn clear_pending_ask(&self, session_id: &str) -> Result<(), StoreError> {
        self.with_session(session_id, move |conn, session_id| {
            conn.execute(
                "DELETE FROM pending_asks WHERE session_id = ?1",
                [session_id],
            )
            .context("failed to clear the pending ask")?;
            Ok(())
        })
        .await
    }

    /// Routes a user's answer to the origin leaf whose raised ask is pending
    /// for this session, in one transaction: the text is appended verbatim to
    /// the leaf's thread, the answer is recorded on the surfaced Ask block,
    /// and the binding is cleared. Routing and clearing in one transaction
    /// means a crash cannot route the answer twice. Waking the leaf's loop is
    /// the caller's to do after a [`RouteAnswer::Routed`]. The row's direct
    /// child — the session the raiser messages to cancel — is not the answer
    /// target when the question was re-raised up a chain, so routing follows
    /// the row's origin leaf.
    pub async fn route_answer(
        &self,
        session_id: &str,
        answer: &str,
    ) -> Result<RouteAnswer, StoreError> {
        let answer = answer.to_string();
        self.with_session(session_id, move |conn, session_id| {
            let tx = transaction(conn)?;
            let Some(binding) = read_pending_ask(&tx, session_id)? else {
                return Ok(RouteAnswer::NoBinding);
            };
            let leaf_id = binding.origin_leaf;
            let leaf_exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                    [&leaf_id],
                    |row| row.get(0),
                )
                .context("failed to check the origin leaf session")?;
            if !leaf_exists {
                tx.execute(
                    "DELETE FROM pending_asks WHERE session_id = ?1",
                    [session_id],
                )
                .context("failed to clear a stale pending ask")?;
                tx.commit()
                    .context("failed to commit the stale-ask clear")?;
                return Ok(RouteAnswer::LeafGone { leaf_id });
            }
            insert_message(
                &tx,
                &leaf_id,
                &Message {
                    role: Role::User,
                    block: Block::Text {
                        text: answer.clone(),
                    },
                },
            )?;
            record_ask_answer(&tx, session_id, binding.ask_message_id, &answer)?;
            tx.execute(
                "DELETE FROM pending_asks WHERE session_id = ?1",
                [session_id],
            )
            .context("failed to clear the pending ask")?;
            tx.commit().context("failed to commit the routed answer")?;
            Ok(RouteAnswer::Routed { leaf_id })
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
            tx.execute(
                "DELETE FROM pending_asks WHERE session_id = ?1 OR child_id = ?1 OR origin_leaf = ?1",
                [&id],
            )
            .context("failed to delete the session's pending asks")?;
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

/// Inserts a message row and its matching `Event::Message` inside the
/// caller's transaction, so a write that spans sessions stays atomic.
fn insert_message(
    tx: &rusqlite::Transaction,
    session_id: &str,
    message: &Message,
) -> Result<i64, anyhow::Error> {
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
    append_event(
        tx,
        session_id,
        "message",
        &Event::Message {
            message: message.clone(),
        },
    )?;
    Ok(message_id)
}

/// The session's pending raised ask row, when it has one.
fn read_pending_ask(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<PendingAsk>, anyhow::Error> {
    let row = conn.query_row(
        "SELECT session_id, child_id, origin_leaf, question, ask_message_id
         FROM pending_asks WHERE session_id = ?1",
        [session_id],
        |row| {
            Ok(PendingAsk {
                session_id: row.get(0)?,
                child_id: row.get(1)?,
                origin_leaf: row.get(2)?,
                question: row.get(3)?,
                ask_message_id: row.get(4)?,
            })
        },
    );
    match row {
        Ok(pending) => Ok(Some(pending)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Records the user's answer on the surfaced Ask block the binding names.
/// Compaction archives the block's row but never deletes it, so the update
/// always finds it; a block that is no longer an ask, or one that already
/// carries an answer, is left alone.
fn record_ask_answer(
    conn: &rusqlite::Connection,
    session_id: &str,
    message_id: i64,
    answer: &str,
) -> Result<(), anyhow::Error> {
    let row = conn.query_row(
        "SELECT block FROM messages WHERE id = ?1 AND session_id = ?2",
        params![message_id, session_id],
        |row| row.get::<_, String>(0),
    );
    let raw = match row {
        Ok(raw) => raw,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let block: Block = serde_json::from_str(&raw).context("failed to parse message block")?;
    let Block::Ask {
        message,
        options,
        child_id,
        answer: recorded,
    } = block
    else {
        return Ok(());
    };
    if recorded.is_some() {
        return Ok(());
    }
    let answered = Block::Ask {
        message,
        options,
        child_id,
        answer: Some(answer.to_string()),
    };
    conn.execute(
        "UPDATE messages SET block = ?1 WHERE id = ?2 AND session_id = ?3",
        params![
            serde_json::to_string(&answered).context("failed to serialize the answered ask")?,
            message_id,
            session_id,
        ],
    )
    .context("failed to record the ask answer")?;
    Ok(())
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
    let interrupt_cause: Option<String> = row.get("interrupt_cause")?;
    Ok(Session {
        id: row.get("id")?,
        node: row.get("node")?,
        repo_url: row.get("repo_url")?,
        git_ref: row.get("git_ref")?,
        dir: row.get("dir")?,
        model: row.get("model")?,
        persona: row.get("persona")?,
        parent_id: row.get("parent_id")?,
        owner_id: row.get("owner_id")?,
        permission: serde_json::from_str(&permission).context("failed to parse permission")?,
        allowed_tools: row.get("allowed_tools")?,
        state: serde_json::from_str(&state).context("failed to parse state")?,
        interrupt_cause: interrupt_cause
            .map(|raw| serde_json::from_str(&raw).context("failed to parse interrupt cause"))
            .transpose()?,
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
            parent_id: None,
            owner_id: id.to_string(),
            permission: Permission::ReadWrite,
            allowed_tools: "shell, file/read".to_string(),
            state: SessionState::Running,
            interrupt_cause: None,
            created_at_secs: 1_700_000_000,
            prompt: Some("finish the feature".to_string()),
        }
    }

    /// A child of `session(owner)`: born on the parent's node and directory.
    fn child_session(id: &str, owner: &str) -> Session {
        let parent = session(owner);
        Session {
            id: id.to_string(),
            repo_url: None,
            git_ref: None,
            dir: parent.dir.clone(),
            persona: Some("reviewer".to_string()),
            parent_id: Some(parent.id),
            owner_id: owner.to_string(),
            permission: Permission::ReadOnly,
            allowed_tools: "file/read, grep".to_string(),
            state: SessionState::Creating,
            prompt: Some("review the change".to_string()),
            ..session(id)
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
        assert_eq!(actual.parent_id, expected.parent_id);
        assert_eq!(actual.owner_id, expected.owner_id);
        assert_eq!(actual.permission, expected.permission);
        assert_eq!(actual.allowed_tools, expected.allowed_tools);
        assert_eq!(actual.state, expected.state);
        assert_eq!(actual.interrupt_cause, expected.interrupt_cause);
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
                "pending_asks",
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
    async fn a_child_session_round_trips_with_its_tree_fields() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();

        store.create_session(&session("root-1")).await.unwrap();
        store
            .create_session(&child_session("child-1", "root-1"))
            .await
            .unwrap();

        let root = store.get_session("root-1").await.unwrap().unwrap();
        assert_eq!(root.parent_id, None);
        assert_eq!(root.owner_id, "root-1", "a root session owns itself");

        let child = store.get_session("child-1").await.unwrap().unwrap();
        assert_eq!(child.parent_id.as_deref(), Some("root-1"));
        assert_eq!(child.owner_id, "root-1");
        assert_eq!(
            child.node, root.node,
            "a child is born on its parent's node"
        );
        assert_eq!(
            child.dir, root.dir,
            "a child is born on its parent's directory"
        );
        assert_eq!(child.state, SessionState::Creating);

        let listed = store.list_sessions().await.unwrap();
        let fields: Vec<(&str, Option<&str>, &str)> = listed
            .iter()
            .map(|s| (s.id.as_str(), s.parent_id.as_deref(), s.owner_id.as_str()))
            .collect();
        assert_eq!(
            fields,
            [
                ("child-1", Some("root-1"), "root-1"),
                ("root-1", None, "root-1")
            ]
        );
    }

    #[tokio::test]
    async fn mark_interrupted_records_the_cause_and_emits_one_state_event() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        store
            .mark_interrupted("a", InterruptCause::User)
            .await
            .unwrap();

        let stored = store.get_session("a").await.unwrap().unwrap();
        assert_eq!(stored.state, SessionState::Interrupted);
        assert_eq!(stored.interrupt_cause, Some(InterruptCause::User));

        let events = store.events_after("a", 0).await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].1,
            Event::State { state } if *state == SessionState::Interrupted
        ));

        // A later interruption replaces the cause; leaving the state keeps it.
        store
            .mark_interrupted("a", InterruptCause::Crash)
            .await
            .unwrap();
        let stored = store.get_session("a").await.unwrap().unwrap();
        assert_eq!(stored.interrupt_cause, Some(InterruptCause::Crash));

        store
            .set_state("a", SessionState::WaitingForInput)
            .await
            .unwrap();
        let stored = store.get_session("a").await.unwrap().unwrap();
        assert_eq!(stored.state, SessionState::WaitingForInput);
        assert_eq!(
            stored.interrupt_cause,
            Some(InterruptCause::Crash),
            "the recorded cause survives until the next interruption"
        );
    }

    #[tokio::test]
    async fn mark_interrupted_on_a_missing_session_is_session_not_found() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();

        let error = store
            .mark_interrupted("ghost", InterruptCause::User)
            .await
            .unwrap_err();
        assert!(matches!(error, StoreError::SessionNotFound { id } if id == "ghost"));
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
    async fn a_database_without_the_tree_columns_is_migrated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            // The post-S1 shape: allowed_tools and persona exist, the session
            // tree columns do not yet.
            conn.execute_batch(
                "CREATE TABLE sessions (
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
                 INSERT INTO sessions (id, node, dir, model, persona, permission, state, created_at_secs, prompt)
                 VALUES ('old', 'node-1', '/work', 'claude', 'coder', '\"read_write\"', '\"waiting_for_input\"', 1700000000, NULL);",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let session = store.get_session("old").await.unwrap().unwrap();
        assert_eq!(session.persona.as_deref(), Some("coder"));
        assert_eq!(
            session.owner_id, "old",
            "a pre-tree row is a root and is backfilled as its own owner"
        );
        assert_eq!(session.parent_id, None);
        assert_eq!(session.interrupt_cause, None);
    }

    #[tokio::test]
    async fn a_pending_ask_without_the_origin_leaf_column_is_migrated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            // The pre-split shape: pending_asks exists without origin_leaf,
            // and its child_id holds the origin leaf the answer routes to.
            conn.execute_batch(
                "CREATE TABLE sessions (
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
                   prompt TEXT,
                   parent_id TEXT,
                   owner_id TEXT,
                   interrupt_cause TEXT
                 );
                 INSERT INTO sessions (id, node, dir, model, permission, state, created_at_secs, owner_id)
                 VALUES ('root-1', 'node-1', '/work', 'claude', '\"read_write\"', '\"waiting_for_input\"', 1700000000, 'root-1');
                 CREATE TABLE pending_asks (
                   session_id TEXT PRIMARY KEY,
                   child_id TEXT NOT NULL,
                   question TEXT NOT NULL,
                   ask_message_id INTEGER NOT NULL
                 );
                 INSERT INTO pending_asks (session_id, child_id, question, ask_message_id)
                 VALUES ('root-1', 'the-leaf', 'may I push?', 1);",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let pending = store.get_pending_ask("root-1").await.unwrap().unwrap();
        assert_eq!(
            pending.origin_leaf, "the-leaf",
            "a pre-split row's child_id was its origin leaf, so the migration copies it over"
        );
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
        assert_eq!(
            session.owner_id, "s1",
            "a row written by the current schema owns itself"
        );
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

    /// The surfaced Ask block a root records when it passes a child's
    /// question to the user.
    fn surfaced_ask(child_id: &str, question: &str) -> Block {
        Block::Ask {
            message: question.into(),
            options: vec!["yes".into(), "no".into()],
            child_id: Some(child_id.into()),
            answer: None,
        }
    }

    #[tokio::test]
    async fn pending_ask_round_trips_and_setting_replaces() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-1")).await.unwrap();
        store
            .create_session(&child_session("mid-1", "root-1"))
            .await
            .unwrap();
        let mut leaf = child_session("leaf-1", "mid-1");
        leaf.owner_id = "root-1".into();
        store.create_session(&leaf).await.unwrap();
        let first = store
            .append_message(
                "root-1",
                Role::Assistant,
                &surfaced_ask("mid-1", "may I push?"),
            )
            .await
            .unwrap();
        let second = store
            .append_message(
                "root-1",
                Role::Assistant,
                &surfaced_ask("mid-1", "may I merge?"),
            )
            .await
            .unwrap();

        assert!(store.get_pending_ask("root-1").await.unwrap().is_none());

        store
            .set_pending_ask("root-1", "mid-1", "leaf-1", "may I push?", first)
            .await
            .unwrap();
        let pending = store.get_pending_ask("root-1").await.unwrap().unwrap();
        assert_eq!(pending.session_id, "root-1");
        assert_eq!(
            pending.child_id, "mid-1",
            "the row names the direct child the raiser can message"
        );
        assert_eq!(
            pending.origin_leaf, "leaf-1",
            "the row names the origin leaf the answer routes to"
        );
        assert_eq!(pending.question, "may I push?");
        assert_eq!(pending.ask_message_id, first);

        // A root surfaces one question at a time: a second surface replaces
        // the first binding.
        store
            .set_pending_ask("root-1", "mid-1", "leaf-1", "may I merge?", second)
            .await
            .unwrap();
        let pending = store.get_pending_ask("root-1").await.unwrap().unwrap();
        assert_eq!(pending.question, "may I merge?");
        assert_eq!(pending.ask_message_id, second);

        store.clear_pending_ask("root-1").await.unwrap();
        assert!(store.get_pending_ask("root-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn route_answer_appends_the_answer_to_the_origin_leaf_records_it_on_the_ask_and_clears_the_binding()
     {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-1")).await.unwrap();
        store
            .create_session(&child_session("mid-1", "root-1"))
            .await
            .unwrap();
        let mut leaf = child_session("leaf-1", "mid-1");
        leaf.owner_id = "root-1".into();
        store.create_session(&leaf).await.unwrap();
        store
            .append_message(
                "leaf-1",
                Role::User,
                &Block::Text {
                    text: "implement the change".into(),
                },
            )
            .await
            .unwrap();
        // The root surfaced the question of its direct child mid-1, which had
        // re-raised the leaf's own question, so the row names mid-1 as the
        // child the root can message and leaf-1 as the origin the answer
        // routes to.
        let ask_id = store
            .append_message(
                "root-1",
                Role::Assistant,
                &surfaced_ask("mid-1", "may I push?"),
            )
            .await
            .unwrap();
        store
            .set_pending_ask("root-1", "mid-1", "leaf-1", "may I push?", ask_id)
            .await
            .unwrap();

        let routed = store
            .route_answer("root-1", "yes, push to main")
            .await
            .unwrap();
        assert_eq!(
            routed,
            RouteAnswer::Routed {
                leaf_id: "leaf-1".into()
            }
        );

        // The answer lands verbatim in the origin leaf's thread as its next
        // user message, exactly as a `message_child` message would; the
        // mid-level session's thread is untouched.
        let leaf_messages = store.messages("leaf-1", false).await.unwrap();
        let (_, last) = leaf_messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(
            matches!(&last.block, Block::Text { text } if text == "yes, push to main"),
            "the answer is the leaf's next message: {leaf_messages:?}"
        );
        assert!(
            !store
                .messages("mid-1", false)
                .await
                .unwrap()
                .iter()
                .any(|(_, message)| matches!(&message.block, Block::Text { text } if text == "yes, push to main")),
            "the answer bypasses the mid-level session that re-raised it"
        );

        // The answer is recorded on the surfaced Ask block and the binding is
        // gone, so a retry cannot route the answer twice.
        let root_messages = store.messages("root-1", false).await.unwrap();
        let ask = root_messages
            .iter()
            .find_map(|(_, message)| match &message.block {
                Block::Ask {
                    child_id: Some(child_id),
                    answer,
                    ..
                } if child_id == "mid-1" => Some(answer),
                _ => None,
            })
            .expect("the surfaced ask block exists");
        assert_eq!(ask.as_deref(), Some("yes, push to main"));
        assert!(store.get_pending_ask("root-1").await.unwrap().is_none());
        assert_eq!(
            store.route_answer("root-1", "again").await.unwrap(),
            RouteAnswer::NoBinding,
            "a second answer routes nowhere"
        );
    }

    #[tokio::test]
    async fn route_answer_without_a_binding_appends_nothing() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-1")).await.unwrap();

        assert_eq!(
            store.route_answer("root-1", "hello").await.unwrap(),
            RouteAnswer::NoBinding
        );
        assert!(store.messages("root-1", false).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn route_answer_to_a_gone_origin_leaf_clears_the_stale_binding() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("root-1")).await.unwrap();
        store
            .create_session(&child_session("mid-1", "root-1"))
            .await
            .unwrap();
        let ask_id = store
            .append_message(
                "root-1",
                Role::Assistant,
                &surfaced_ask("mid-1", "may I push?"),
            )
            .await
            .unwrap();
        // The origin leaf was stopped between the surface and the answer, so
        // the binding routes to a session that no longer exists while the
        // direct child it names is still there.
        store
            .set_pending_ask("root-1", "mid-1", "gone-leaf", "may I push?", ask_id)
            .await
            .unwrap();

        assert_eq!(
            store.route_answer("root-1", "yes").await.unwrap(),
            RouteAnswer::LeafGone {
                leaf_id: "gone-leaf".into()
            }
        );
        assert!(
            store.get_pending_ask("root-1").await.unwrap().is_none(),
            "a stale binding is cleared"
        );
        let root_messages = store.messages("root-1", false).await.unwrap();
        assert_eq!(
            root_messages.len(),
            1,
            "the answer is not appended to the root: {root_messages:?}"
        );
        assert!(
            matches!(&root_messages[0].1.block, Block::Ask { answer: None, .. }),
            "the surfaced ask of a gone leaf keeps no answer"
        );
        assert!(
            !store.messages("mid-1", false).await.unwrap().iter().any(
                |(_, message)| matches!(&message.block, Block::Text { text } if text == "yes")
            ),
            "the answer is not routed to the direct child as a substitute"
        );
    }

    #[tokio::test]
    async fn last_message_returns_the_newest_message_or_none() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store.create_session(&session("a")).await.unwrap();

        assert!(store.last_message("a").await.unwrap().is_none());
        store
            .append_message("a", Role::User, &Block::Text { text: "one".into() })
            .await
            .unwrap();
        store
            .append_message(
                "a",
                Role::Assistant,
                &Block::Ask {
                    message: "may I push?".into(),
                    options: vec![],
                    child_id: None,
                    answer: None,
                },
            )
            .await
            .unwrap();
        let last = store.last_message("a").await.unwrap().unwrap();
        assert_eq!(last.role, Role::Assistant);
        assert!(
            matches!(&last.block, Block::Ask { message, .. } if message == "may I push?"),
            "the child's pending ask is its last message"
        );

        let error = store.last_message("ghost").await.unwrap_err();
        assert!(matches!(error, StoreError::SessionNotFound { id } if id == "ghost"));
    }

    #[tokio::test]
    async fn remove_session_clears_pending_asks_bound_to_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let store = Store::open(&path).unwrap();
        store.create_session(&session("root-1")).await.unwrap();
        store
            .create_session(&child_session("mid-1", "root-1"))
            .await
            .unwrap();
        let mut leaf = child_session("leaf-1", "mid-1");
        leaf.owner_id = "root-1".into();
        store.create_session(&leaf).await.unwrap();
        let ask_id = store
            .append_message(
                "root-1",
                Role::Assistant,
                &surfaced_ask("mid-1", "may I push?"),
            )
            .await
            .unwrap();
        store
            .set_pending_ask("root-1", "mid-1", "leaf-1", "may I push?", ask_id)
            .await
            .unwrap();
        let mid_ask_id = store
            .append_message(
                "mid-1",
                Role::Assistant,
                &surfaced_ask("leaf-1", "may I push?"),
            )
            .await
            .unwrap();
        store
            .set_pending_ask("mid-1", "leaf-1", "leaf-1", "may I push?", mid_ask_id)
            .await
            .unwrap();

        // Stopping the origin leaf clears every binding that routes to it:
        // the root's row that names it as the origin, and the mid-level row
        // that raised it directly. An answer to a stopped leaf must not route
        // into the void.
        store.remove_session("leaf-1").await.unwrap();
        assert!(store.get_pending_ask("root-1").await.unwrap().is_none());
        assert!(store.get_pending_ask("mid-1").await.unwrap().is_none());

        // Stopping the root clears its own binding too.
        store
            .set_pending_ask("root-1", "mid-1", "leaf-1", "may I push?", ask_id)
            .await
            .unwrap();
        store.remove_session("root-1").await.unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_asks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pending, 0);
    }
}
