use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Session & Connection ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOpen {
    pub client_id: String,
    pub cwd: String,
    pub hostname: String,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: Uuid,
    pub client_id: String,
    pub cwd: String,
    pub hostname: String,
    pub username: String,
    pub opened_at: DateTime<Utc>,
    pub permissions: PermissionMode,
}

// ── Permissions ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Unrestricted access to CWD
    Default,
    /// Always prompts for dirs outside CWD
    Ask,
    /// Read-only CWD, ask for writes
    Lockdown,
    /// Full system access, no prompts
    Yolo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub path: String,
    pub access: AccessKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessKind {
    Read,
    Write,
    Execute,
    ReadDir,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionResponse {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub granted: bool,
    pub mode: PermissionMode,
    pub path: String,
}

// ── Messages (client ↔ server) ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    OpenSession(SessionOpen),
    CloseSession { session_id: Uuid },
    ChatInput {
        session_id: Uuid,
        content: String,
    },
    PermissionReply {
        request_id: Uuid,
        session_id: Uuid,
        granted: bool,
    },
    ListModels,
    SetModel { session_id: Uuid, model: String },
    SetPermissionMode {
        session_id: Uuid,
        mode: PermissionMode,
    },
    /// Cancel an in-progress agent loop for this session
    CancelChat { session_id: Uuid },
    /// Result of a tool execution requested by the server
    ToolResult {
        request_id: Uuid,
        session_id: Uuid,
        result: String,
    },
    /// List persisted sessions for the authenticated user
    ListSessions,
    /// Resume a persisted session (loads history + SSH target)
    ResumeSession { session_id: Uuid },
    /// Create a new session, optionally bound to an SSH target
    /// (e.g. "e-work@e-desktop"). No target = local/quick session.
    NewSession { ssh_target: Option<String> },
    /// Clear all sessions for a specific account (e.g. "e-play")
    ClearSessions { account: String },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    SessionOpened {
        session_id: Uuid,
        info: SessionInfo,
    },
    SessionClosed { session_id: Uuid },
    /// Streaming content delta from the model
    ChatDelta {
        session_id: Uuid,
        delta: String,
        done: bool,
    },
    ChatError {
        session_id: Uuid,
        error: String,
    },
    /// A tool was invoked by the agent (so TUI can show activity)
    ToolEvent {
        session_id: Uuid,
        name: String,
        status: ToolStatus,
        detail: String,
    },
    ModelList {
        models: Vec<ModelInfo>,
    },
    ModelChanged {
        session_id: Uuid,
        model: String,
    },
    PermissionRequired(PermissionRequest),
    PermissionResult(PermissionResponse),
    ModeChanged {
        session_id: Uuid,
        mode: PermissionMode,
    },
    Pong,
    /// Agent loop was cancelled by the user
    ChatCancelled { session_id: Uuid },
    /// The in-progress assistant message is being regenerated after a stream
    /// error — the client should drop the partial assistant entry; the
    /// deltas that follow rebuild it from scratch.
    ChatReset { session_id: Uuid },
    /// Server is shutting down
    Shutdown,
    /// Persisted sessions for the authenticated user
    SessionList {
        sessions: Vec<SessionMeta>,
    },
    /// A session was resumed — includes replayed history for display
    SessionResumed {
        session_id: Uuid,
        meta: SessionMeta,
        /// (role, content) pairs of prior user/assistant turns
        transcript: Vec<(String, String)>,
    },
    /// Server requests the client to execute a local tool.
    ToolRequest {
        request_id: Uuid,
        session_id: Uuid,
        name: String,
        args_json: String,
    },
    /// Client response with the result of a tool execution.
    ToolResult {
        request_id: Uuid,
        session_id: Uuid,
        result: String, // Ok result or "Error: ..." prefix
    },
    /// Chat statistics (sent after final delta)
    ChatStats {
        session_id: Uuid,
        model: String,
        prompt_tokens: u32,
        completion_tokens: u32,
        duration_ms: u64,
        /// decode speed (kept for backwards compat, == decode_tps)
        tokens_per_second: f64,
        context_length: u32,
        /// prompt ingestion speed (approx: time to first token)
        prefill_tps: f64,
        /// generation speed (approx: first token → stream end)
        decode_tps: f64,
    },
}

/// Summary of a persisted session (for listing and resuming).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: Uuid,
    pub title: Option<String>,
    pub ssh_target: Option<String>,
    pub cwd: String,
    pub mode: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Started,
    Finished,
    Failed,
}

// ── Model info ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub description: Option<String>,
    pub capabilities: Vec<String>,
}

// ── Agent / Memory types ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub session_id: Uuid,
    pub model_used: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Uuid,
    pub key: String,
    pub value: String,
    pub scope: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub pattern: String,
    pub source_session: Option<Uuid>,
    pub frequency: u32,
    pub last_used: Option<DateTime<Utc>>,
}

// ── Router decision ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterDecision {
    pub target_model: String,
    pub target_host: String,
    pub reasoning: String,
    pub complexity: ComplexityClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComplexityClass {
    Simple,   // factual, short answer, quick lookup
    Medium,   // chat, summarization, explanation
    Complex,  // coding, reasoning, multi-step
    Critical, // deep reasoning, novel problems
}
