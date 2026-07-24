use std::sync::{Arc, Mutex};
use std::time::Instant;
use temple_protocol::*;
use uuid::Uuid;

/// A pending y/N prompt. Two kinds: server-side permission requests
/// (reply goes back over the wire) and client-side tool consent (the
/// server asked us to run something dangerous locally — the reply
/// executes or denies it right here).
#[derive(Debug, Clone)]
pub enum PromptKind {
    /// Server asked permission for a path/command — y/N goes to the server.
    Server { request_id: Uuid, session_id: Uuid },
    /// Server asked the CLIENT to execute something risky — y executes it
    /// locally and returns the ToolResult; N denies it.
    Local {
        request_id: Uuid,
        session_id: Uuid,
        name: String,
        args_json: String,
    },
}

#[derive(Debug, Clone)]
pub struct PromptState {
    pub text: String,
    pub at: Instant,
    pub kind: PromptKind,
}

/// Internal-only messages routed from the UI thread to the Tokio
/// WebSocket thread. These must never be serialized to the wire.
#[derive(Debug)]
pub enum InternalCmd {
    /// Execute a local tool on the Tokio thread and send the result
    /// string back via the responder channel. Uses std mpsc so the
    /// UI thread can block without an async runtime.
    ExecuteLocalTool {
        name: String,
        args_json: String,
        cwd: String,
        responder: std::sync::mpsc::SyncSender<String>,
    },
}

#[derive(Debug, Clone)]
pub enum ChatEntry {
    /// User message
    User(String),
    /// Assistant message (streams in), with optional stats footer
    Assistant {
        content: String,
        stats: Option<String>,
    },
    /// System/info line (command output, session notices — cyan)
    System(String),
    /// Error line (red)
    Error(String),
    /// Tool activity
    Tool {
        name: String,
        status: ToolStatus,
        detail: String,
    },
    /// Session task list (replaced in place on each update)
    Todo {
        items: Vec<temple_protocol::TodoItem>,
    },
}

pub struct AppState {
    pub session_id: Uuid,
    pub entries: Vec<ChatEntry>,
    pub status: String,
    pub model: String,
    pub prompt: String,
    /// Character index into `prompt` (0..=prompt.chars().count())
    pub prompt_cursor: usize,
    pub permission: Option<PromptState>,
    pub running: bool,
    pub editor_pending: bool,
    /// Working directory for local tool execution
    pub cwd: String,
    /// lines to scroll up from bottom (0 = follow tail)
    pub scroll: usize,
    /// mouse selection anchor (line, col) while dragging
    pub sel_anchor: Option<(usize, usize)>,
    /// finished/active selection ((line,col),(line,col)) normalized
    pub selection: Option<((usize, usize), (usize, usize))>,
    /// plain-text of currently visible chat lines (for hit-testing)
    pub last_lines: Vec<String>,
    /// transient banner text (e.g. "copied 142 chars")
    pub banner: Option<(String, Instant)>,
    /// previously sent prompts, recalled with Up/Down
    pub input_history: Vec<String>,
    /// position while cycling input history (None = editing new prompt)
    pub history_pos: Option<usize>,
    /// stashed in-progress prompt while cycling history
    pub history_stash: String,
    /// last session listing (for /session <n> selection)
    pub last_sessions: Vec<SessionMeta>,
    /// an agent loop is in flight (set on send, cleared on stats/error)
    pub working: bool,
    /// when the current agent loop started (for the elapsed indicator)
    pub work_started: Option<Instant>,
    /// total display lines at last render (scroll anchoring)
    pub last_total: usize,
    /// Current permission mode (from server), shown in the status bar and
    /// cycled with Shift+Tab.
    pub mode: PermissionMode,
    /// Available models for tab-completion (populated on connect + /model)
    pub available_models: Vec<String>,
    /// Session search overlay: active + search text
    pub session_search: Option<String>,
    /// Selected index in the session search overlay (keyboard nav)
    pub session_search_idx: usize,
}

impl AppState {
    pub fn new(cwd: String) -> Self {
        Self {
            session_id: Uuid::nil(),
            entries: Vec::new(),
            status: "connecting\u{2026}".into(),
            model: String::new(),
            prompt: String::new(),
            prompt_cursor: 0,
            permission: None,
            running: true,
            editor_pending: false,
            cwd,
            scroll: 0,
            sel_anchor: None,
            selection: None,
            last_lines: Vec::new(),
            banner: None,
            input_history: Vec::new(),
            history_pos: None,
            history_stash: String::new(),
            last_sessions: Vec::new(),
            working: false,
            work_started: None,
            last_total: 0,
            mode: PermissionMode::Default,
            available_models: Vec::new(),
            session_search: None,
            session_search_idx: 0,
        }
    }
}

pub type SharedState = Arc<Mutex<AppState>>;
