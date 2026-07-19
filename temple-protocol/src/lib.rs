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
    /// Onboarding: user requests initial setup. Agent asks questions,
    /// stores answers as user-scoped memories.
    Initialize { session_id: Uuid },
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
    /// Server is shutting down
    Shutdown,
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
