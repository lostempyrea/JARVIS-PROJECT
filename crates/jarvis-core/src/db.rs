//! SQLite persistence layer (`rusqlite` bundled) with WAL mode and FTS5 memory search.
//!
//! All methods lock the internal `std::sync::Mutex<Connection>` strictly inside
//! synchronous scopes so a `MutexGuard` is never held across an `.await` point.

use chrono::Local;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Errors returned by [`Database`] operations.
pub fn is_protected_session_id(id: &str, channel: Option<&str>) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    lower == "hud-main"
        || lower == "telegram"
        || lower.starts_with("telegram-")
        || channel.is_some_and(|ch| ch.trim().eq_ignore_ascii_case("telegram"))
}

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database mutex poisoned")]
    LockPoisoned,
    #[error("io error preparing database directory: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tool_calls_json: Option<String>,
    pub tool_call_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub session_id: Option<String>,
    pub title: String,
    pub status: String,
    pub payload_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApprovalRecord {
    pub id: String,
    pub session_id: String,
    pub tool_name: String,
    pub args_json: String,
    pub reason: String,
    pub status: String,
    pub decision: Option<String>,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: i64,
    pub key: String,
    pub content: String,
    pub tags: String,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Thread-safe handle to the JARVIS-RS SQLite database.
#[derive(Clone, Debug)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// Open (or create) a SQLite database at `path`, applying WAL pragmas and schema migrations.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// Open an isolated in-memory SQLite database (useful for tests).
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, DbError> {
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA foreign_keys = ON;
            ",
        )?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), DbError> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                channel TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                tool_calls_json TEXT,
                tool_call_id TEXT,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session_created
                ON messages(session_id, created_at);

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                payload_json TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pending_approvals (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                args_json TEXT NOT NULL,
                reason TEXT NOT NULL,
                status TEXT NOT NULL,
                decision TEXT,
                created_at TEXT NOT NULL,
                resolved_at TEXT
            );

            CREATE TABLE IF NOT EXISTS memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                key TEXT NOT NULL UNIQUE,
                content TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '',
                source TEXT NOT NULL DEFAULT 'agent',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                key,
                content,
                tags,
                content='memories',
                content_rowid='id',
                tokenize='unicode61'
            );

            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, key, content, tags)
                VALUES (new.id, new.key, new.content, new.tags);
            END;

            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content, tags)
                VALUES ('delete', old.id, old.key, old.content, old.tags);
            END;

            CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content, tags)
                VALUES ('delete', old.id, old.key, old.content, old.tags);
                INSERT INTO memories_fts(rowid, key, content, tags)
                VALUES (new.id, new.key, new.content, new.tags);
            END;
            ",
        )?;
        Ok(())
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T, DbError>) -> Result<T, DbError> {
        let guard = self.conn.lock().map_err(|_| DbError::LockPoisoned)?;
        f(&guard)
    }

    // -------------------------------------------------------------------------
    // Sessions CRUD
    // -------------------------------------------------------------------------

    pub fn create_session(
        &self,
        id: &str,
        title: &str,
        channel: &str,
    ) -> Result<SessionRecord, DbError> {
        let now = Local::now().to_rfc3339();
        let rec = SessionRecord {
            id: id.to_string(),
            title: title.to_string(),
            channel: channel.to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, title, channel, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![rec.id, rec.title, rec.channel, rec.created_at, rec.updated_at],
            )?;
            Ok(rec)
        })
    }

    pub fn ensure_session(
        &self,
        id: &str,
        title: &str,
        channel: &str,
    ) -> Result<SessionRecord, DbError> {
        if let Some(existing) = self.get_session(id)? {
            return Ok(existing);
        }
        self.create_session(id, title, channel)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>, DbError> {
        self.with_conn(|conn| {
            let rec = conn
                .query_row(
                    "SELECT id, title, channel, created_at, updated_at FROM sessions WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(SessionRecord {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            channel: row.get(2)?,
                            created_at: row.get(3)?,
                            updated_at: row.get(4)?,
                        })
                    },
                )
                .optional()?;
            Ok(rec)
        })
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRecord>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, title, channel, created_at, updated_at
                 FROM sessions
                 ORDER BY
                     CASE
                         WHEN id = 'hud-main' THEN 0
                         WHEN channel = 'telegram' OR id LIKE 'telegram%' THEN 1
                         ELSE 2
                     END ASC,
                     updated_at DESC
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit as i64], |row| {
                Ok(SessionRecord {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    channel: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn update_session_title(&self, id: &str, title: &str) -> Result<bool, DbError> {
        let existing = self.get_session(id)?;
        if is_protected_session_id(id, existing.as_ref().map(|s| s.channel.as_str())) {
            return Ok(false);
        }
        let now = Local::now().to_rfc3339();
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
                params![title, now, id],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn delete_session(&self, id: &str) -> Result<bool, DbError> {
        let existing = self.get_session(id)?;
        if is_protected_session_id(id, existing.as_ref().map(|s| s.channel.as_str())) {
            return Ok(false);
        }
        self.with_conn(|conn| {
            let changed = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
            Ok(changed > 0)
        })
    }

    /// Delete all messages belonging to `session_id` while keeping the session record itself.
    pub fn clear_session_messages(&self, session_id: &str) -> Result<usize, DbError> {
        let now = Local::now().to_rfc3339();
        self.with_conn(|conn| {
            let deleted = conn.execute(
                "DELETE FROM messages WHERE session_id = ?1",
                params![session_id],
            )?;
            let _ = conn.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![session_id, now],
            );
            Ok(deleted)
        })
    }

    // -------------------------------------------------------------------------
    // Messages CRUD
    // -------------------------------------------------------------------------

    pub fn add_message(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
        tool_calls_json: Option<&str>,
        tool_call_id: Option<&str>,
    ) -> Result<MessageRecord, DbError> {
        let now = Local::now().to_rfc3339();
        let rec = MessageRecord {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            tool_calls_json: tool_calls_json.map(ToString::to_string),
            tool_call_id: tool_call_id.map(ToString::to_string),
            created_at: now.clone(),
        };
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO messages (id, session_id, role, content, tool_calls_json, tool_call_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    rec.id,
                    rec.session_id,
                    rec.role,
                    rec.content,
                    rec.tool_calls_json,
                    rec.tool_call_id,
                    rec.created_at,
                ],
            )?;
            conn.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![now, session_id],
            )?;
            Ok(rec)
        })
    }

    pub fn list_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<MessageRecord>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, role, content, tool_calls_json, tool_call_id, created_at
                 FROM messages
                 WHERE session_id = ?1
                 ORDER BY rowid DESC
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![session_id, limit as i64], |row| {
                Ok(MessageRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    tool_calls_json: row.get(4)?,
                    tool_call_id: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out.reverse();
            Ok(out)
        })
    }

    pub fn delete_message(&self, id: &str) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            let changed = conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
            Ok(changed > 0)
        })
    }

    // -------------------------------------------------------------------------
    // Tasks CRUD
    // -------------------------------------------------------------------------

    pub fn create_task(
        &self,
        id: &str,
        session_id: Option<&str>,
        title: &str,
        status: &str,
        payload_json: Option<&str>,
    ) -> Result<TaskRecord, DbError> {
        let now = Local::now().to_rfc3339();
        let rec = TaskRecord {
            id: id.to_string(),
            session_id: session_id.map(ToString::to_string),
            title: title.to_string(),
            status: status.to_string(),
            payload_json: payload_json.map(ToString::to_string),
            created_at: now.clone(),
            updated_at: now,
        };
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO tasks (id, session_id, title, status, payload_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    rec.id,
                    rec.session_id,
                    rec.title,
                    rec.status,
                    rec.payload_json,
                    rec.created_at,
                    rec.updated_at,
                ],
            )?;
            Ok(rec)
        })
    }

    pub fn get_task(&self, id: &str) -> Result<Option<TaskRecord>, DbError> {
        self.with_conn(|conn| {
            let rec = conn
                .query_row(
                    "SELECT id, session_id, title, status, payload_json, created_at, updated_at
                     FROM tasks WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(TaskRecord {
                            id: row.get(0)?,
                            session_id: row.get(1)?,
                            title: row.get(2)?,
                            status: row.get(3)?,
                            payload_json: row.get(4)?,
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
                        })
                    },
                )
                .optional()?;
            Ok(rec)
        })
    }

    pub fn list_tasks(&self, status_filter: Option<&str>) -> Result<Vec<TaskRecord>, DbError> {
        self.with_conn(|conn| {
            let mut out = Vec::new();
            if let Some(status) = status_filter {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, title, status, payload_json, created_at, updated_at
                     FROM tasks WHERE status = ?1 ORDER BY updated_at DESC",
                )?;
                let rows = stmt.query_map(params![status], |row| {
                    Ok(TaskRecord {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        title: row.get(2)?,
                        status: row.get(3)?,
                        payload_json: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                })?;
                for row in rows {
                    out.push(row?);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, title, status, payload_json, created_at, updated_at
                     FROM tasks ORDER BY updated_at DESC",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(TaskRecord {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        title: row.get(2)?,
                        status: row.get(3)?,
                        payload_json: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                })?;
                for row in rows {
                    out.push(row?);
                }
            }
            Ok(out)
        })
    }

    pub fn update_task_status(&self, id: &str, status: &str) -> Result<bool, DbError> {
        let now = Local::now().to_rfc3339();
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, id],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn delete_task(&self, id: &str) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            let changed = conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
            Ok(changed > 0)
        })
    }

    // -------------------------------------------------------------------------
    // Pending Approvals CRUD
    // -------------------------------------------------------------------------

    pub fn create_pending_approval(
        &self,
        id: &str,
        session_id: &str,
        tool_name: &str,
        args_json: &str,
        reason: &str,
    ) -> Result<PendingApprovalRecord, DbError> {
        let now = Local::now().to_rfc3339();
        let rec = PendingApprovalRecord {
            id: id.to_string(),
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            args_json: args_json.to_string(),
            reason: reason.to_string(),
            status: "pending".to_string(),
            decision: None,
            created_at: now,
            resolved_at: None,
        };
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO pending_approvals
                 (id, session_id, tool_name, args_json, reason, status, decision, created_at, resolved_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    rec.id,
                    rec.session_id,
                    rec.tool_name,
                    rec.args_json,
                    rec.reason,
                    rec.status,
                    rec.decision,
                    rec.created_at,
                    rec.resolved_at,
                ],
            )?;
            Ok(rec)
        })
    }

    pub fn resolve_pending_approval(
        &self,
        id: &str,
        status: &str,
        decision: &str,
    ) -> Result<bool, DbError> {
        let now = Local::now().to_rfc3339();
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE pending_approvals
                 SET status = ?1, decision = ?2, resolved_at = ?3
                 WHERE id = ?4 AND status = 'pending'",
                params![status, decision, now, id],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn get_pending_approval(&self, id: &str) -> Result<Option<PendingApprovalRecord>, DbError> {
        self.with_conn(|conn| {
            let rec = conn
                .query_row(
                    "SELECT id, session_id, tool_name, args_json, reason, status, decision, created_at, resolved_at
                     FROM pending_approvals WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(PendingApprovalRecord {
                            id: row.get(0)?,
                            session_id: row.get(1)?,
                            tool_name: row.get(2)?,
                            args_json: row.get(3)?,
                            reason: row.get(4)?,
                            status: row.get(5)?,
                            decision: row.get(6)?,
                            created_at: row.get(7)?,
                            resolved_at: row.get(8)?,
                        })
                    },
                )
                .optional()?;
            Ok(rec)
        })
    }

    pub fn list_pending_approvals(&self) -> Result<Vec<PendingApprovalRecord>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, tool_name, args_json, reason, status, decision, created_at, resolved_at
                 FROM pending_approvals
                 WHERE status = 'pending'
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(PendingApprovalRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    tool_name: row.get(2)?,
                    args_json: row.get(3)?,
                    reason: row.get(4)?,
                    status: row.get(5)?,
                    decision: row.get(6)?,
                    created_at: row.get(7)?,
                    resolved_at: row.get(8)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    // -------------------------------------------------------------------------
    // Memories CRUD + FTS5 Search
    // -------------------------------------------------------------------------

    pub fn upsert_memory(
        &self,
        key: &str,
        content: &str,
        tags: &str,
        source: &str,
    ) -> Result<MemoryRecord, DbError> {
        let now = Local::now().to_rfc3339();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO memories (key, content, tags, source, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(key) DO UPDATE SET
                     content = excluded.content,
                     tags = excluded.tags,
                     source = excluded.source,
                     updated_at = excluded.updated_at",
                params![key, content, tags, source, now],
            )?;
            let rec = conn.query_row(
                "SELECT id, key, content, tags, source, created_at, updated_at
                 FROM memories WHERE key = ?1",
                params![key],
                |row| {
                    Ok(MemoryRecord {
                        id: row.get(0)?,
                        key: row.get(1)?,
                        content: row.get(2)?,
                        tags: row.get(3)?,
                        source: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )?;
            Ok(rec)
        })
    }

    pub fn get_memory(&self, key: &str) -> Result<Option<MemoryRecord>, DbError> {
        self.with_conn(|conn| {
            let rec = conn
                .query_row(
                    "SELECT id, key, content, tags, source, created_at, updated_at
                     FROM memories WHERE key = ?1",
                    params![key],
                    |row| {
                        Ok(MemoryRecord {
                            id: row.get(0)?,
                            key: row.get(1)?,
                            content: row.get(2)?,
                            tags: row.get(3)?,
                            source: row.get(4)?,
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
                        })
                    },
                )
                .optional()?;
            Ok(rec)
        })
    }

    pub fn delete_memory(&self, key: &str) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            let changed = conn.execute("DELETE FROM memories WHERE key = ?1", params![key])?;
            Ok(changed > 0)
        })
    }

    pub fn list_memories(&self, limit: usize) -> Result<Vec<MemoryRecord>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, key, content, tags, source, created_at, updated_at
                 FROM memories ORDER BY updated_at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit as i64], |row| {
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    tags: row.get(3)?,
                    source: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// Full-text search over `memories_fts` (`unicode61` tokenizer).
    pub fn search_memories(&self, query: &str, limit: usize) -> Result<Vec<MemoryRecord>, DbError> {
        let fts_query = sanitize_fts_query(query);
        if fts_query.is_empty() {
            return self.list_memories(limit);
        }

        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.key, m.content, m.tags, m.source, m.created_at, m.updated_at
                 FROM memories_fts f
                 JOIN memories m ON m.id = f.rowid
                 WHERE memories_fts MATCH ?1
                 ORDER BY bm25(memories_fts), m.updated_at DESC
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    tags: row.get(3)?,
                    source: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }
}

/// Convert arbitrary user query text into safe FTS5 quoted tokens joined with OR/AND.
/// Uses space-separated quoted terms (implicit AND in FTS5) so punctuation cannot cause syntax errors.
fn sanitize_fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|token| {
            let escaped = token.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pragmas_wal_and_foreign_keys_enabled() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();

        db.with_conn(|conn| {
            let mode: String = conn.query_row("PRAGMA journal_mode;", [], |r| r.get(0))?;
            let fk: i64 = conn.query_row("PRAGMA foreign_keys;", [], |r| r.get(0))?;
            let timeout: i64 = conn.query_row("PRAGMA busy_timeout;", [], |r| r.get(0))?;
            assert_eq!(mode.to_lowercase(), "wal");
            assert_eq!(fk, 1);
            assert_eq!(timeout, 5000);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sessions_and_messages_crud_and_cascade() {
        let db = Database::open_in_memory().unwrap();
        let sess = db.create_session("s1", "First chat", "desktop").unwrap();
        assert_eq!(sess.id, "s1");

        db.add_message("m1", "s1", "user", "hello", None, None)
            .unwrap();
        db.add_message("m2", "s1", "assistant", "hi there", None, None)
            .unwrap();

        let msgs = db.list_messages("s1", 50).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hello");
        assert_eq!(msgs[1].content, "hi there");

        assert!(db.update_session_title("s1", "Renamed").unwrap());
        assert_eq!(db.get_session("s1").unwrap().unwrap().title, "Renamed");

        assert!(db.delete_session("s1").unwrap());
        assert!(db.list_messages("s1", 50).unwrap().is_empty());
    }

    #[test]
    fn tasks_and_pending_approvals_crud() {
        let db = Database::open_in_memory().unwrap();
        db.create_task("t1", None, "Index workspace", "queued", Some("{}"))
            .unwrap();
        assert_eq!(db.list_tasks(Some("queued")).unwrap().len(), 1);
        assert!(db.update_task_status("t1", "completed").unwrap());
        assert_eq!(db.get_task("t1").unwrap().unwrap().status, "completed");
        assert!(db.delete_task("t1").unwrap());

        db.create_pending_approval("ap1", "s1", "terminal", r#"{"command":"rm -rf"}"#, "destructive")
            .unwrap();
        assert_eq!(db.list_pending_approvals().unwrap().len(), 1);
        assert!(db.resolve_pending_approval("ap1", "resolved", "denied").unwrap());
        // Resolving a second time must return false.
        assert!(!db.resolve_pending_approval("ap1", "resolved", "approved").unwrap());
        assert!(db.list_pending_approvals().unwrap().is_empty());
        assert_eq!(
            db.get_pending_approval("ap1").unwrap().unwrap().decision.as_deref(),
            Some("denied")
        );
    }

    #[test]
    fn memories_fts5_insert_update_delete_triggers() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_memory("coffee_pref", "User likes Vietnamese iced coffee (Cà phê sữa đá)", "preference,vietnam", "agent")
            .unwrap();
        db.upsert_memory("rust_env", "Workspace uses rustc 1.89.0 on Windows", "dev,rust", "agent")
            .unwrap();

        // Unicode61 FTS5 search
        let res = db.search_memories("Cà phê", 10).unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].key, "coffee_pref");

        // Update should refresh FTS index (old term removed, new term searchable)
        db.upsert_memory("coffee_pref", "User switched to matcha latte", "preference", "user")
            .unwrap();
        assert!(db.search_memories("Vietnamese", 10).unwrap().is_empty());
        assert_eq!(db.search_memories("matcha", 10).unwrap().len(), 1);

        // Delete should remove from FTS index
        assert!(db.delete_memory("coffee_pref").unwrap());
        assert!(db.search_memories("matcha", 10).unwrap().is_empty());
    }

    #[test]
    fn list_messages_returns_newest_limit_in_chronological_order() {
        let db = Database::open_in_memory().unwrap();
        db.create_session("s_many", "Long Session", "desktop")
            .unwrap();

        for i in 0..65 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            db.add_message(
                &format!("m-{i}"),
                "s_many",
                role,
                &format!("message-{i}"),
                None,
                None,
            )
            .unwrap();
        }

        let msgs = db.list_messages("s_many", 50).unwrap();
        assert_eq!(msgs.len(), 50);
        assert_eq!(msgs.first().unwrap().content, "message-15");
        assert_eq!(msgs.last().unwrap().content, "message-64");
        for (idx, msg) in msgs.iter().enumerate() {
            assert_eq!(msg.content, format!("message-{}", idx + 15));
        }
    }

    #[test]
    fn clear_session_messages_keeps_session_and_preserves_other_sessions() {
        let db = Database::open_in_memory().unwrap();
        db.create_session("s1", "Session 1", "desktop").unwrap();
        db.create_session("s2", "Session 2", "desktop").unwrap();

        db.add_message("m1", "s1", "user", "hello s1", None, None).unwrap();
        db.add_message("m2", "s2", "user", "hello s2", None, None).unwrap();

        assert_eq!(db.list_messages("s1", 10).unwrap().len(), 1);
        assert_eq!(db.list_messages("s2", 10).unwrap().len(), 1);

        let deleted_count = db.clear_session_messages("s1").unwrap();
        assert_eq!(deleted_count, 1);

        // Session 1 still exists, but has 0 messages
        assert!(db.get_session("s1").unwrap().is_some());
        assert!(db.list_messages("s1", 10).unwrap().is_empty());

        // Session 2 and its messages are completely intact
        assert!(db.get_session("s2").unwrap().is_some());
        let s2_msgs = db.list_messages("s2", 10).unwrap();
        assert_eq!(s2_msgs.len(), 1);
        assert_eq!(s2_msgs[0].content, "hello s2");
    }

    #[test]
    fn protected_primary_and_telegram_sessions_cannot_be_deleted_or_renamed_but_can_be_cleared() {
        let db = Database::open_in_memory().unwrap();
        db.create_session("hud-main", "Primary HUD", "hud").unwrap();
        db.create_session("telegram-12345", "Telegram", "telegram").unwrap();
        db.create_session("hud-custom", "Custom", "hud").unwrap();

        db.add_message("m1", "hud-main", "user", "hello hud", None, None).unwrap();
        db.add_message("m2", "telegram-12345", "user", "hello telegram", None, None).unwrap();

        assert_eq!(db.delete_session("hud-main").unwrap(), false);
        assert!(db.get_session("hud-main").unwrap().is_some());

        assert_eq!(db.delete_session("telegram-12345").unwrap(), false);
        assert!(db.get_session("telegram-12345").unwrap().is_some());

        assert_eq!(db.update_session_title("hud-main", "New").unwrap(), false);
        assert_eq!(db.update_session_title("telegram-12345", "New").unwrap(), false);

        assert_eq!(db.clear_session_messages("hud-main").unwrap(), 1);
        assert_eq!(db.clear_session_messages("telegram-12345").unwrap(), 1);
        assert!(db.get_session("hud-main").unwrap().is_some());
        assert!(db.get_session("telegram-12345").unwrap().is_some());

        assert_eq!(db.delete_session("hud-custom").unwrap(), true);
        assert!(db.get_session("hud-custom").unwrap().is_none());
    }
}
