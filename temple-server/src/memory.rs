use rusqlite::{params, Connection};
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
        // Run migrations without lock — we own the connection
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
                verified_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_signal_users_uuid ON signal_users(uuid);
            ",
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

    pub async fn get_session_history(
        &self,
        session_id: Uuid,
        limit: i64,
    ) -> rusqlite::Result<Vec<ConversationEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT session_id, role, content, timestamp, model_used FROM conversations WHERE session_id = ?1 ORDER BY id ASC LIMIT ?2",
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
        for row in rows {
            if let Ok(r) = row {
                results.push(r);
            }
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
        for row in rows {
            if let Ok(r) = row {
                results.push(r);
            }
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
        for row in rows {
            if let Ok(r) = row {
                results.push(r);
            }
        }
        Ok(results)
    }

    // ── Personality ──

    /// Get recent conversations across all sessions (for personality updates).
    pub async fn get_recent_conversations(&self, limit: i64) -> rusqlite::Result<Vec<ConversationEntry>> {
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
        for row in rows {
            if let Ok(r) = row {
                results.push(r);
            }
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
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO signal_users (username, phone) VALUES (?1, ?2) \
             ON CONFLICT(username) DO UPDATE SET phone = EXCLUDED.phone",
            params![username, phone],
        )?;
        Ok(())
    }

    /// Record a verified UUID for a signal user.
    pub async fn set_signal_user_uuid(
        &self,
        username: &str,
        uuid: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE signal_users SET uuid = ?2, verified_at = ?3 WHERE username = ?1",
            params![username, uuid, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Get all verified signal users (phone + UUID).
    pub async fn get_signal_users(&self) -> rusqlite::Result<Vec<(String, String, Option<String>)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT username, phone, uuid FROM signal_users",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut results = Vec::new();
        for row in rows {
            if let Ok(r) = row {
                results.push(r);
            }
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
            Some(row) => Ok(Some((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
            ))),
            None => Ok(None),
        }
    }
}
