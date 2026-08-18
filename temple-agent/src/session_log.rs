//! Append-only session log: the durable record of everything model-visible
//! in a session. One JSONL file per session under
//! `<data_dir>/session-logs/<session_id>.jsonl`. Entries are never mutated
//! or compacted away — compaction mutates the in-memory history, so the log
//! is the only verbatim record of what actually reached the model.
//!
//! Corrupt lines (partial writes from a crash) are skipped on replay; the
//! rest of the file still reads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// One logged event: a timestamp, the owning session, and the event itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionLogEntry {
    pub ts: DateTime<Utc>,
    pub session_id: Uuid,
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// A model-visible (or permission/audit) event in one session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Session created (not replayed on resume).
    SessionOpened {
        user: String,
        kind: String,
        cwd: String,
        model: String,
    },
    /// The raw user turn as submitted. The model-visible user message is
    /// `Preamble` + `UserMessage` joined with `\n\n---\n\n` (the same join
    /// the loop pushes into history).
    UserMessage {
        content: String,
        username: String,
    },
    /// Dynamic context (date, memories, skills) prepended to a user turn.
    Preamble {
        text: String,
    },
    /// The routing decision for one turn: lane description, complexity
    /// class, and the classifier model that refined an ambiguous query.
    ModelRouted {
        model: String,
        complexity: Option<String>,
        refined_by: Option<String>,
    },
    /// Final assistant text for a stage. Stage is `assistant` for the
    /// ordinary loop and pipeline finales, `planner`/`executor`/`reviewer`
    /// for pipeline intermediates. Only the reviewer stage is folded back
    /// into history (as a user message).
    AssistantMessage {
        text: String,
        model: String,
        stage: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    /// Tool outcome as the model saw it (truncated to the history cap).
    ToolResult {
        name: String,
        ok: bool,
        result: String,
    },
    PermissionPrompt {
        request_id: String,
        path: String,
        access: String,
    },
    PermissionResult {
        request_id: String,
        granted: bool,
        note: String,
    },
    SessionClosed {
        reason: String,
    },
    Error {
        message: String,
    },
}

impl SessionLogEntry {
    pub fn new(session_id: Uuid, event: SessionEvent) -> Self {
        Self {
            ts: Utc::now(),
            session_id,
            event,
        }
    }
}

/// Append-only JSONL store for per-session logs.
pub struct SessionLog {
    dir: PathBuf,
}

impl SessionLog {
    /// Open the log directory, creating it if needed.
    pub fn open(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).ok();
        Self { dir }
    }

    /// The log file path for a session.
    pub fn path(&self, session_id: Uuid) -> PathBuf {
        self.dir.join(format!("{session_id}.jsonl"))
    }

    pub fn exists(&self, session_id: Uuid) -> bool {
        self.path(session_id).is_file()
    }

    /// Append one entry as a JSON line. Opens with append semantics so
    /// concurrent appends cannot interleave mid-line.
    pub fn append(&self, entry: &SessionLogEntry) -> Result<(), String> {
        let line = serde_json::to_string(entry).map_err(|e| format!("serialize log entry: {e}"))?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(entry.session_id))
            .map_err(|e| format!("open session log: {e}"))?;
        writeln!(f, "{line}").map_err(|e| format!("write session log: {e}"))
    }

    /// Read every entry in file order, skipping corrupt lines. A missing
    /// file yields an empty vec.
    pub fn replay(&self, session_id: Uuid) -> Vec<SessionLogEntry> {
        let Ok(data) = std::fs::read_to_string(self.path(session_id)) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for line in data.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionLogEntry>(line) {
                Ok(e) => entries.push(e),
                Err(e) => {
                    tracing::warn!("session {session_id}: skipping corrupt log line: {e}");
                }
            }
        }
        entries
    }

    /// Delete a session's log file (session deletion / /clear).
    pub fn delete(&self, session_id: Uuid) {
        let _ = std::fs::remove_file(self.path(session_id));
    }
}

/// Project the log onto (role, text) turns, mirroring the transcript the
/// client replays: user messages and final assistant text, with reviewer
/// feedback folded in as a user turn (as the loop pushes it into history).
/// Pipeline planner and superseded executor stages are not turns.
pub fn transcript(entries: &[SessionLogEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::UserMessage { content, .. } => Some(("user".into(), content.clone())),
            SessionEvent::AssistantMessage { text, stage, .. } => match stage.as_str() {
                "reviewer" => Some(("user".into(), text.clone())),
                "assistant" | "executor" => Some(("assistant".into(), text.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Join a preamble and user message exactly as the loop prepends dynamic
/// context (`{preamble}\n\n---\n\n{content}`).
pub fn join_user_turn(preamble: &str, content: &str) -> String {
    if preamble.is_empty() {
        content.to_string()
    } else {
        format!("{preamble}\n\n---\n\n{content}")
    }
}

/// Human-readable access kind for the log.
pub fn access_kind_str(kind: &temple_protocol::AccessKind) -> String {
    match kind {
        temple_protocol::AccessKind::Read => "read",
        temple_protocol::AccessKind::Write => "write",
        temple_protocol::AccessKind::Execute => "execute",
        temple_protocol::AccessKind::ReadDir => "readdir",
    }
    .into()
}

/// Log directory under a data dir (db parent), shared by all sessions.
pub fn default_log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("session-logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session_id: Uuid, event: SessionEvent) -> SessionLogEntry {
        SessionLogEntry::new(session_id, event)
    }

    #[test]
    fn append_and_replay_round_trip() {
        let dir = std::env::temp_dir().join(format!("temple-log-test-{}", Uuid::new_v4()));
        let log = SessionLog::open(dir.clone());
        let sid = Uuid::new_v4();

        let events = vec![
            SessionEvent::SessionOpened {
                user: "e-play".into(),
                kind: "interactive".into(),
                cwd: "/tmp".into(),
                model: "qwen3.6-27b".into(),
            },
            SessionEvent::UserMessage {
                content: "hello".into(),
                username: "e-play".into(),
            },
            SessionEvent::AssistantMessage {
                text: "hi there".into(),
                model: "qwen3.6-27b".into(),
                stage: "assistant".into(),
            },
        ];
        for e in &events {
            log.append(&entry(sid, e.clone())).unwrap();
        }

        let replayed = log.replay(sid);
        assert_eq!(replayed.len(), 3);
        for (got, want) in replayed.iter().zip(&events) {
            assert_eq!(got.event, *want);
            assert_eq!(got.session_id, sid);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let dir = std::env::temp_dir().join(format!("temple-log-test-{}", Uuid::new_v4()));
        let log = SessionLog::open(dir.clone());
        let sid = Uuid::new_v4();

        log.append(&entry(
            sid,
            SessionEvent::UserMessage {
                content: "first".into(),
                username: "u".into(),
            },
        ))
        .unwrap();
        // Simulate a crash mid-write: a truncated JSON line.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(log.path(sid))
                .unwrap();
            writeln!(f, r#"{{"ts":"2026-08-18T00:00:00Z","session_id":""#).unwrap();
        }
        log.append(&entry(
            sid,
            SessionEvent::UserMessage {
                content: "second".into(),
                username: "u".into(),
            },
        ))
        .unwrap();

        let replayed = log.replay(sid);
        assert_eq!(replayed.len(), 2);
        assert_eq!(
            replayed[1].event,
            SessionEvent::UserMessage {
                content: "second".into(),
                username: "u".into(),
            }
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_replays_empty() {
        let dir = std::env::temp_dir().join(format!("temple-log-test-{}", Uuid::new_v4()));
        let log = SessionLog::open(dir.clone());
        assert!(log.replay(Uuid::new_v4()).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn user_turn_join_matches_loop_format() {
        let joined = join_user_turn("Today is Monday.", "read the file");
        assert_eq!(joined, "Today is Monday.\n\n---\n\nread the file");
        assert_eq!(join_user_turn("", "plain"), "plain");
    }

    #[test]
    fn transcript_projects_turns() {
        let sid = Uuid::new_v4();
        let entries = vec![
            entry(
                sid,
                SessionEvent::UserMessage {
                    content: "fix it".into(),
                    username: "e-play".into(),
                },
            ),
            entry(
                sid,
                SessionEvent::AssistantMessage {
                    text: "plan one".into(),
                    model: "planner".into(),
                    stage: "planner".into(),
                },
            ),
            entry(
                sid,
                SessionEvent::AssistantMessage {
                    text: "done".into(),
                    model: "executor".into(),
                    stage: "executor".into(),
                },
            ),
            entry(
                sid,
                SessionEvent::AssistantMessage {
                    text: "redo it".into(),
                    model: "reviewer".into(),
                    stage: "reviewer".into(),
                },
            ),
            entry(
                sid,
                SessionEvent::AssistantMessage {
                    text: "final".into(),
                    model: "qwen".into(),
                    stage: "assistant".into(),
                },
            ),
        ];
        assert_eq!(
            transcript(&entries),
            vec![
                ("user".to_string(), "fix it".to_string()),
                ("assistant".to_string(), "done".to_string()),
                ("user".to_string(), "redo it".to_string()),
                ("assistant".to_string(), "final".to_string()),
            ]
        );
    }
}
