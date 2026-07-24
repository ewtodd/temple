use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Arc;
use temple_protocol::{ConversationEntry, MemoryEntry, Skill};
use tokio::sync::Mutex;
use uuid::Uuid;

pub struct Memory {
    conn: Arc<Mutex<Connection>>,
}

impl Memory {
    pub async fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
        }
        let conn = Connection::open(path)?;
        // Use PRAGMA user_version to track schema migrations so we don't
        // run ALTER TABLE blindly every startup (which logs spurious errors
        // when columns already exist).
        let mut user_version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0);

        // Base schema (version 0) — idempotent CREATE TABLE IF NOT EXISTS.
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS conversations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                model_used TEXT
            );
            CREATE TABLE IF NOT EXISTS memory_store (
                id TEXT PRIMARY KEY,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                scope TEXT NOT NULL DEFAULT 'global',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS skills (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                description TEXT NOT NULL,
                pattern TEXT NOT NULL,
                source_session TEXT,
                frequency INTEGER NOT NULL DEFAULT 1,
                last_used TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_conversations_session ON conversations(session_id);
            CREATE INDEX IF NOT EXISTS idx_memory_key ON memory_store(key);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_key_scope ON memory_store(key, scope);
            CREATE TABLE IF NOT EXISTS signal_users (
                username TEXT PRIMARY KEY,
                phone TEXT NOT NULL,
                uuid TEXT,
                verified_at TEXT,
                admin TEXT NOT NULL DEFAULT 'no',
                priority INTEGER NOT NULL DEFAULT -1
            );
            CREATE INDEX IF NOT EXISTS idx_signal_users_uuid ON signal_users(uuid);
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                username TEXT NOT NULL,
                account TEXT,
                ssh_target TEXT,
                cwd TEXT NOT NULL,
                mode TEXT NOT NULL DEFAULT 'default',
                kind TEXT NOT NULL DEFAULT 'interactive',
                title TEXT,
                history TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_username ON sessions(username);
            CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_account ON sessions(account);
            CREATE TABLE IF NOT EXISTS documents (
                id TEXT PRIMARY KEY,
                filename TEXT NOT NULL,
                content TEXT NOT NULL,
                username TEXT NOT NULL,
                mime_type TEXT NOT NULL DEFAULT 'text/plain',
                size INTEGER NOT NULL DEFAULT 0,
                uploaded_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_documents_username ON documents(username);
            ",
        )?;

        // Detect columns that already exist (pre-migration-version databases
        // may have had blind ALTER TABLE on every startup). Each check must
        // cover the migration's FULL effect — a partial match must NOT bump
        // the version, or later migrations get skipped.
        let has_column = |table: &str, col: &str| -> bool {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).ok();
            stmt.as_mut().is_some_and(|s| {
                s.query_map([], |row| row.get::<_, String>(1))
                    .ok()
                    .is_some_and(|rows| rows.filter_map(|r| r.ok()).any(|name| name == col))
            })
        };

        if user_version < 1 && has_column("sessions", "account") {
            user_version = 1;
        }
        if user_version < 2
            && has_column("signal_users", "admin")
            && has_column("signal_users", "priority")
        {
            user_version = 2;
        }
        if user_version < 3 && has_column("sessions", "todos") {
            user_version = 3;
        }

        // Persist the detected starting point
        if user_version > 0 {
            let _ = conn.execute_batch(&format!("PRAGMA user_version = {user_version}"));
        }

        // ── Incremental migrations (only run once, each transactional) ──
        let migrate = |ver: i32, sql: &str| -> rusqlite::Result<()> {
            if user_version < ver {
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(sql)?;
                tx.execute_batch(&format!("PRAGMA user_version = {ver}"))?;
                tx.commit()?;
                tracing::info!("memory: migrated to version {ver}");
            }
            Ok(())
        };

        // Version 1: add account column to sessions + its index
        migrate(
            1,
            "ALTER TABLE sessions ADD COLUMN account TEXT;\
             CREATE INDEX IF NOT EXISTS idx_sessions_account ON sessions(account);",
        )?;

        // Version 2: add admin and priority columns to signal_users
        migrate(
            2,
            "ALTER TABLE signal_users ADD COLUMN admin TEXT DEFAULT 'no';\
             ALTER TABLE signal_users ADD COLUMN priority INTEGER DEFAULT -1;",
        )?;

        // Version 3: add todos column to sessions (per-session task lists)
        migrate(
            3,
            "ALTER TABLE sessions ADD COLUMN todos TEXT NOT NULL DEFAULT '[]';",
        )?;

        // Version 4: add documents table for user-uploaded files
        migrate(
            4,
            "CREATE TABLE IF NOT EXISTS documents (\
                id TEXT PRIMARY KEY,\
                filename TEXT NOT NULL,\
                content TEXT NOT NULL,\
                username TEXT NOT NULL,\
                mime_type TEXT NOT NULL DEFAULT 'text/plain',\
                size INTEGER NOT NULL DEFAULT 0,\
                uploaded_at TEXT NOT NULL\
            );\
            CREATE INDEX IF NOT EXISTS idx_documents_username ON documents(username);",
        )?;

        // Version 5: FTS5 fulltext search on documents
        migrate(
            5,
            "CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(\
                filename, content, content=documents, content_rowid=rowid\
            );\
            CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN\
                INSERT INTO documents_fts(rowid, filename, content) VALUES (new.rowid, new.filename, new.content);\
            END;\
            CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN\
                INSERT INTO documents_fts(documents_fts, rowid, filename, content) VALUES ('delete', old.rowid, old.filename, old.content);\
            END;\
            CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE ON documents BEGIN\
                INSERT INTO documents_fts(documents_fts, rowid, filename, content) VALUES ('delete', old.rowid, old.filename, old.content);\
                INSERT INTO documents_fts(rowid, filename, content) VALUES (new.rowid, new.filename, new.content);\
            END;",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    // ── Conversations ──

    pub async fn store_conversation(&self, entry: &ConversationEntry) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO conversations (session_id, role, content, timestamp, model_used) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                entry.session_id.to_string(),
                entry.role,
                entry.content,
                entry.timestamp.to_rfc3339(),
                entry.model_used.as_deref(),
            ],
        )?;
        Ok(())
    }

    #[allow(dead_code)] // Public API for session inspection tooling.
    pub async fn get_session_history(
        &self,
        session_id: Uuid,
        limit: i64,
    ) -> rusqlite::Result<Vec<ConversationEntry>> {
        let conn = self.conn.lock().await;
        // Newest N rows, returned in chronological order — `ORDER BY id ASC
        // LIMIT N` would return the conversation's BEGINNING, not its most
        // recent context.
        let mut stmt = conn.prepare(
            "SELECT session_id, role, content, timestamp, model_used FROM (\
                SELECT id, session_id, role, content, timestamp, model_used \
                FROM conversations WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2\
             ) ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id.to_string(), limit], |row| {
            Ok(ConversationEntry {
                session_id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                role: row.get(1)?,
                content: row.get(2)?,
                timestamp: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(3)?)
                    .map(|d| d.to_utc())
                    .unwrap_or_else(|_| chrono::Utc::now()),
                model_used: row.get(4)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    // ── Persistent memory ──

    pub async fn set_memory(&self, key: &str, value: &str, scope: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_store (id, key, value, scope, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
             ON CONFLICT(key, scope) DO UPDATE SET value = EXCLUDED.value, updated_at = EXCLUDED.updated_at",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), key, value, scope, now],
        )?;
        Ok(())
    }

    pub async fn get_memory(&self, key: &str, scope: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT value FROM memory_store WHERE key = ?1 AND scope = ?2 ORDER BY updated_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![key, scope])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    pub async fn get_all_memory(&self, scope: &str) -> rusqlite::Result<Vec<MemoryEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, key, value, scope, created_at, updated_at FROM memory_store WHERE scope = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![scope], |row| {
            Ok(MemoryEntry {
                id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                key: row.get(1)?,
                value: row.get(2)?,
                scope: row.get(3)?,
                created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(4)?)
                    .map(|d| d.to_utc())
                    .unwrap_or_else(|_| chrono::Utc::now()),
                updated_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(5)?)
                    .map(|d| d.to_utc())
                    .unwrap_or_else(|_| chrono::Utc::now()),
            })
        })?;
        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    // ── Skills ──

    pub async fn upsert_skill(&self, skill: &Skill) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        let last_used = skill.last_used.map(|d| d.to_rfc3339());
        conn.execute(
            "INSERT INTO skills (name, description, pattern, source_session, frequency, last_used) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET
               description = EXCLUDED.description,
               pattern = EXCLUDED.pattern,
               frequency = skills.frequency + 1,
               last_used = ?6",
            params![
                skill.name,
                skill.description,
                skill.pattern,
                skill.source_session.map(|s| s.to_string()),
                skill.frequency,
                last_used,
            ],
        )?;
        Ok(())
    }

    pub async fn get_all_skills(&self) -> rusqlite::Result<Vec<Skill>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT name, description, pattern, source_session, frequency, last_used FROM skills ORDER BY frequency DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Skill {
                name: row.get(0)?,
                description: row.get(1)?,
                pattern: row.get(2)?,
                source_session: row
                    .get::<_, Option<String>>(3)?
                    .and_then(|s| Uuid::parse_str(&s).ok()),
                frequency: row.get(4)?,
                last_used: row
                    .get::<_, Option<String>>(5)?
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.to_utc()),
            })
        })?;
        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    // ── Personality ──

    /// Get recent conversations across all sessions (for personality updates).
    pub async fn get_recent_conversations(
        &self,
        limit: i64,
    ) -> rusqlite::Result<Vec<ConversationEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT session_id, role, content, timestamp, model_used FROM conversations ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(ConversationEntry {
                session_id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                role: row.get(1)?,
                content: row.get(2)?,
                timestamp: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(3)?)
                    .map(|d| d.to_utc())
                    .unwrap_or_else(|_| chrono::Utc::now()),
                model_used: row.get(4)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    pub async fn get_personality(&self) -> rusqlite::Result<String> {
        let val = self.get_memory("renco_personality", "system").await?;
        Ok(val.unwrap_or_else(|| {
            "You are renco, an autonomous AI agent running on temple harness. \
             You are helpful, direct, and technically precise. You maintain \
             a continuous presence and learn from every interaction."
                .to_string()
        }))
    }

    pub async fn set_personality(&self, text: &str) -> rusqlite::Result<()> {
        self.set_memory("renco_personality", text, "system").await
    }

    // ── Signal users ──

    /// Upsert a signal user's phone number (from token file).
    pub async fn upsert_signal_user_phone(
        &self,
        username: &str,
        phone: &str,
        admin: bool,
        priority: i32,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO signal_users (username, phone, admin, priority) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(username) DO UPDATE SET phone = EXCLUDED.phone, \
             admin = EXCLUDED.admin, priority = EXCLUDED.priority",
            params![username, phone, if admin { "yes" } else { "no" }, priority],
        )?;
        Ok(())
    }

    /// Request-queue priority for a user — lower runs first
    /// (0 ethan, 1 valarie, -1 default for everyone else).
    pub async fn get_user_priority(&self, username: &str) -> rusqlite::Result<i32> {
        let conn = self.conn.lock().await;
        let priority = conn
            .query_row(
                "SELECT priority FROM signal_users WHERE username = ?1",
                params![username],
                |row| row.get::<_, Option<i32>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(-1);
        Ok(priority)
    }

    /// Whether a token-file user is an admin (for /clear, /broadcast).
    pub async fn is_admin_username(&self, username: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().await;
        let admin: Option<String> = conn
            .query_row(
                "SELECT admin FROM signal_users WHERE username = ?1",
                params![username],
                |row| row.get(0),
            )
            .optional()?;
        Ok(admin.as_deref() == Some("yes"))
    }

    /// Record a verified UUID for a signal user.
    pub async fn set_signal_user_uuid(&self, username: &str, uuid: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE signal_users SET uuid = ?2, verified_at = ?3 WHERE username = ?1",
            params![username, uuid, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Get all verified signal users (phone + UUID).
    pub async fn get_signal_users(
        &self,
    ) -> rusqlite::Result<Vec<(String, String, Option<String>)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT username, phone, uuid FROM signal_users")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    /// Find a signal user by phone or UUID.
    pub async fn find_signal_user(
        &self,
        identifier: &str,
    ) -> rusqlite::Result<Option<(String, String, Option<String>)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT username, phone, uuid FROM signal_users WHERE phone = ?1 OR uuid = ?1",
        )?;
        let mut rows = stmt.query(params![identifier])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?))),
            None => Ok(None),
        }
    }

    // ── Persistent sessions ──

    /// Create or update a session row.
    pub async fn save_session(&self, s: &PersistedSession) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO sessions (id, username, ssh_target, account, cwd, mode, kind, title, history, todos, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11) \
             ON CONFLICT(id) DO UPDATE SET \
               ssh_target = EXCLUDED.ssh_target, \
               account = EXCLUDED.account, \
               cwd = EXCLUDED.cwd, \
               mode = EXCLUDED.mode, \
               title = EXCLUDED.title, \
               history = EXCLUDED.history, \
               todos = EXCLUDED.todos, \
               updated_at = EXCLUDED.updated_at",
            params![
                s.id.to_string(),
                s.username,
                s.ssh_target,
                s.account,
                s.cwd,
                s.mode,
                s.kind,
                s.title,
                s.history_json,
                s.todos_json,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Load a session by id.
    pub async fn load_session(&self, id: Uuid) -> rusqlite::Result<Option<PersistedSession>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, username, ssh_target, account, cwd, mode, kind, title, history, todos FROM sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.to_string()])?;
        match rows.next()? {
            Some(row) => Ok(Some(PersistedSession {
                id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                username: row.get(1)?,
                ssh_target: row.get(2)?,
                account: row.get(3)?,
                cwd: row.get(4)?,
                mode: row.get(5)?,
                kind: row.get(6)?,
                title: row.get(7)?,
                history_json: row.get(8)?,
                todos_json: row.get(9)?,
            })),
            None => Ok(None),
        }
    }

    /// List recent sessions for a user (most recent first).
    pub async fn list_sessions(
        &self,
        username: &str,
        limit: i64,
    ) -> rusqlite::Result<Vec<SessionSummary>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, username, ssh_target, account, cwd, mode, title, updated_at FROM sessions \
             WHERE username = ?1 ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![username, limit], |row| {
            Ok(SessionSummary {
                id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                username: row.get(1)?,
                ssh_target: row.get(2)?,
                account: row.get(3)?,
                cwd: row.get(4)?,
                mode: row.get(5)?,
                title: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    /// Delete all sessions for a user — matches the token identity
    /// (username, e.g. "ethan") OR a machine account (e.g. "e-play").
    /// Signal-created sessions have account NULL, so matching only the
    /// account column silently deletes nothing for them.
    pub async fn clear_sessions(&self, account: &str) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().await;
        // Match by username (owner), machine account, exact ssh target, or
        // target account prefix — so `/clear ethan`, `/clear e-play`, and
        // `/clear e-work@e-desktop` all do what the user means.
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM conversations WHERE session_id IN (\
                SELECT id FROM sessions \
                WHERE account = ?1 OR username = ?1 OR ssh_target = ?1 \
                   OR ssh_target LIKE ?1 || '@%'\
             )",
            params![account],
        )?;
        let n = tx.execute(
            "DELETE FROM sessions \
             WHERE account = ?1 OR username = ?1 OR ssh_target = ?1 \
                OR ssh_target LIKE ?1 || '@%'",
            params![account],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Delete a single session by id. Also removes its conversation history.
    pub async fn delete_session(&self, id: Uuid) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        // Transactional — a crash between the two DELETEs must not orphan
        // history rows.
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM conversations WHERE session_id = ?1",
            params![id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Nuke ALL sessions and conversations. Returns count of deleted sessions.
    pub async fn nuke_sessions(&self) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        let count = tx.execute("DELETE FROM sessions", [])?;
        tx.execute("DELETE FROM conversations", [])?;
        tx.commit()?;
        Ok(count)
    }

    /// Get all admin users' phone numbers + UUIDs.
    pub async fn get_admins(&self) -> rusqlite::Result<Vec<(String, Option<String>)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT phone, uuid FROM signal_users WHERE admin = 'yes'")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    // ── Documents ──

    pub async fn upload_document(
        &self,
        filename: &str,
        content: &str,
        username: &str,
        mime_type: &str,
    ) -> rusqlite::Result<Uuid> {
        let id = Uuid::new_v4();
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO documents (id, filename, content, username, mime_type, size, uploaded_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id.to_string(),
                filename,
                content,
                username,
                mime_type,
                content.len() as i64,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(id)
    }

    pub async fn list_documents(
        &self,
        username: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<DocRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, filename, username, mime_type, size, uploaded_at \
             FROM documents WHERE username = ?1 \
             ORDER BY uploaded_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![username, limit as i64], |row| {
            Ok(DocRow {
                id: row.get::<_, String>(0)?.parse().unwrap_or_default(),
                filename: row.get(1)?,
                username: row.get(2)?,
                mime_type: row.get(3)?,
                size: row.get(4)?,
                uploaded_at: row.get(5)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    pub async fn search_documents(
        &self,
        username: &str,
        query: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<DocRow>> {
        let conn = self.conn.lock().await;
        // Try FTS5 MATCH first; fall back to LIKE if FTS is empty
        let fts_query = query
            .split_whitespace()
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut stmt = conn.prepare(
            "SELECT d.id, d.filename, d.username, d.mime_type, d.size, d.uploaded_at \
             FROM documents d \
             JOIN documents_fts fts ON d.rowid = fts.rowid \
             WHERE d.username = ?1 AND documents_fts MATCH ?2 \
             ORDER BY rank LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![username, fts_query, limit as i64], |row| {
            Ok(DocRow {
                id: row.get::<_, String>(0)?.parse().unwrap_or_default(),
                filename: row.get(1)?,
                username: row.get(2)?,
                mime_type: row.get(3)?,
                size: row.get(4)?,
                uploaded_at: row.get(5)?,
            })
        });
        let mut results = match rows {
            Ok(r) => {
                let mut v = Vec::new();
                for r in r.flatten() {
                    v.push(r);
                }
                if !v.is_empty() {
                    return Ok(v);
                }
                v
            }
            Err(_) => Vec::new(),
        };

        // Fallback to LIKE for partial matches FTS doesn't catch
        let like = format!("%{query}%");
        let mut stmt = conn.prepare(
            "SELECT id, filename, username, mime_type, size, uploaded_at \
             FROM documents WHERE username = ?1 AND (filename LIKE ?2 OR content LIKE ?2) \
             ORDER BY uploaded_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![username, like, limit as i64], |row| {
            Ok(DocRow {
                id: row.get::<_, String>(0)?.parse().unwrap_or_default(),
                filename: row.get(1)?,
                username: row.get(2)?,
                mime_type: row.get(3)?,
                size: row.get(4)?,
                uploaded_at: row.get(5)?,
            })
        })?;
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    pub async fn get_document(&self, id: Uuid) -> rusqlite::Result<Option<DocFull>> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT id, filename, content, username, mime_type, size, uploaded_at \
             FROM documents WHERE id = ?1",
            params![id.to_string()],
            |row| {
                Ok(DocFull {
                    id: row.get::<_, String>(0)?.parse().unwrap_or_default(),
                    filename: row.get(1)?,
                    content: row.get(2)?,
                    username: row.get(3)?,
                    mime_type: row.get(4)?,
                    size: row.get(5)?,
                    uploaded_at: row.get(6)?,
                })
            },
        )
        .optional()
    }

    pub async fn delete_document(&self, id: Uuid, username: &str) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "DELETE FROM documents WHERE id = ?1 AND username = ?2",
            params![id.to_string(), username],
        )?;
        Ok(n)
    }
}

/// A document stored by a user (Signal/web upload).
#[derive(Debug, Clone)]
pub struct DocRow {
    pub id: Uuid,
    pub filename: String,
    pub username: String,
    pub mime_type: String,
    pub size: i64,
    pub uploaded_at: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DocFull {
    pub id: Uuid,
    pub filename: String,
    pub content: String,
    pub username: String,
    pub mime_type: String,
    pub size: i64,
    pub uploaded_at: String,
}

/// A session persisted to the DB — survives server restarts, shared
/// between TUI and Signal.
#[derive(Debug, Clone)]
pub struct PersistedSession {
    pub id: Uuid,
    pub username: String,
    pub ssh_target: Option<String>,
    pub account: Option<String>,
    pub cwd: String,
    pub mode: String,
    pub kind: String,
    pub title: Option<String>,
    pub history_json: String,
    pub todos_json: String,
}

/// Summary for session listing.
#[derive(Debug, Clone)]
#[allow(dead_code)] // username/account used by list endpoints downstream.
pub struct SessionSummary {
    pub id: Uuid,
    pub username: String,
    pub ssh_target: Option<String>,
    pub account: Option<String>,
    pub cwd: String,
    pub mode: String,
    pub title: Option<String>,
    pub updated_at: String,
}
