use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use temple_protocol::*;
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

use crate::litellm::{ChatMessage, ChatRequest, LiteLLM, StreamEvent, ToolDefinition};
use crate::mcp::McpClient;
use crate::memory::Memory;
use crate::permissions::PermissionScope;

/// Maximum tool-loop rounds per user message before forcing an answer.
const MAX_TOOL_ROUNDS: usize = 12;
/// Max conversation messages kept in context (plus system prompt).
const MAX_HISTORY: usize = 60;
/// Max bytes returned from a single tool result (keeps context manageable).
const MAX_TOOL_RESULT: usize = 24_000;

/// Resolves permission requests from the agent loop via the client.
pub struct PermissionResolver {
    pending: Mutex<HashMap<Uuid, oneshot::Sender<bool>>>,
}

impl PermissionResolver {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub async fn ask(&self, request_id: Uuid) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id, tx);
        rx
    }

    pub async fn resolve(&self, request_id: Uuid, granted: bool) {
        if let Some(tx) = self.pending.lock().await.remove(&request_id) {
            let _ = tx.send(granted);
        }
    }
}

/// Events the agent loop emits toward the client.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Delta(String),
    ToolEvent {
        name: String,
        status: ToolStatus,
        detail: String,
    },
    PermissionNeeded(PermissionRequest),
    Done(ChatStatsData),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ChatStatsData {
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub duration_ms: u64,
    pub context_length: u32,
    pub prefill_tps: f64,
    pub decode_tps: f64,
}

pub struct Agent {
    pub litellm: LiteLLM,
    pub mcp: McpClient,
    pub memory: Arc<Memory>,
    pub permissions: Arc<PermissionResolver>,
    sessions: Mutex<HashMap<Uuid, SessionCtx>>,
    default_model: String,
    tools: Mutex<Vec<ToolDefinition>>,
    warm_model: String,
}

struct SessionCtx {
    model: String,
    history: Vec<ChatMessage>,
}

impl Agent {
    pub fn new(
        litellm: LiteLLM,
        mcp: McpClient,
        memory: Arc<Memory>,
        default_model: &str,
    ) -> Self {
        Self {
            warm_model: default_model.to_string(),
            litellm,
            mcp,
            memory,
            permissions: Arc::new(PermissionResolver::new()),
            sessions: Mutex::new(HashMap::new()),
            default_model: default_model.to_string(),
            tools: Mutex::new(Vec::new()),
        }
    }

    /// Load tool definitions: local tools + MCP tools from litellm.
    pub async fn refresh_tools(&self) {
        let mut tools = local_tools();
        match self.litellm.list_mcp_tools().await {
            Ok(mcp_tools) => {
                tracing::info!("Loaded {} MCP tools from litellm", mcp_tools.len());
                tools.extend(mcp_tools);
            }
            Err(e) => {
                tracing::warn!("MCP tools unavailable: {e} (local tools only)");
            }
        }
        *self.tools.lock().await = tools;
    }

    /// Keep-alive ping so llamaswap never unloads the brain model.
    pub async fn keep_warm(&self) {
        let req = ChatRequest {
            model: self.warm_model.clone(),
            messages: vec![ChatMessage::user("ping")],
            tools: None,
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(1),
            temperature: Some(0.0),
        };
        let _ = self.litellm.chat(req).await;
    }

    pub async fn open_session(&self, session_id: Uuid) {
        let mut sessions = self.sessions.lock().await;
        sessions.insert(
            session_id,
            SessionCtx {
                model: self.default_model.clone(),
                history: Vec::new(),
            },
        );
    }

    pub async fn close_session(&self, session_id: Uuid) {
        self.sessions.lock().await.remove(&session_id);
    }

    pub async fn set_session_model(&self, session_id: Uuid, model: &str) {
        if let Some(s) = self.sessions.lock().await.get_mut(&session_id) {
            s.model = model.to_string();
        }
    }

    pub async fn session_model(&self, session_id: Uuid) -> String {
        self.sessions
            .lock()
            .await
            .get(&session_id)
            .map(|s| s.model.clone())
            .unwrap_or_else(|| self.default_model.clone())
    }

    /// The full agent loop for one user message.
    /// Streams deltas in real time, executes tools, loops until done.
    pub async fn process_chat(
        &self,
        session_id: Uuid,
        content: &str,
        scope: Arc<Mutex<PermissionScope>>,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) {
        let started = Instant::now();

        // Append user message to history
        {
            let mut sessions = self.sessions.lock().await;
            let Some(s) = sessions.get_mut(&session_id) else {
                emit(AgentEvent::Error("session not found".into()));
                return;
            };
            s.history.push(ChatMessage::user(content));
        }

        let model = self.session_model(session_id).await;
        let system_prompt = self.build_system_prompt().await;

        let mut total_completion: u32 = 0;
        let mut last_prompt_tokens: u32 = 0;
        let mut final_content = String::new();
        // Separate prefill/decode timing (approximated per round):
        // prefill ≈ request start → first content delta (TTFT, includes network)
        // decode  ≈ first content delta → stream end
        let mut total_prefill_ms: u64 = 0;
        let mut total_decode_ms: u64 = 0;
        let mut total_prefill_tokens: u32 = 0;

        for round in 0..MAX_TOOL_ROUNDS {
            // Build request messages: system + history
            let mut messages = vec![ChatMessage::system(system_prompt.clone())];
            {
                let sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get(&session_id) {
                    let start = s.history.len().saturating_sub(MAX_HISTORY);
                    messages.extend(s.history[start..].iter().cloned());
                }
            }

            let tools = self.tools.lock().await.clone();
            let req = ChatRequest {
                model: model.clone(),
                messages,
                tools: if tools.is_empty() { None } else { Some(tools) },
                stream: Some(true),
                stream_options: None,
                max_tokens: Some(16384),
                temperature: None, // use litellm model's configured sampling
            };

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
            let litellm = self.litellm.clone();
            let round_start = Instant::now();
            tokio::spawn(async move {
                litellm.chat_stream(req, tx).await;
            });

            // Drain stream events, forwarding deltas to client
            let mut result: Option<crate::litellm::StreamResult> = None;
            let mut stream_error: Option<String> = None;
            let mut first_delta_at: Option<Instant> = None;
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Delta(d) => {
                        if first_delta_at.is_none() {
                            first_delta_at = Some(Instant::now());
                        }
                        emit(AgentEvent::Delta(d));
                    }
                    StreamEvent::Done(r) => {
                        result = Some(r);
                        break;
                    }
                    StreamEvent::Error(e) => {
                        stream_error = Some(e);
                        break;
                    }
                }
            }

            // Round timing
            let round_end = Instant::now();
            match first_delta_at {
                Some(t_first) => {
                    total_prefill_ms += t_first.duration_since(round_start).as_millis() as u64;
                    total_decode_ms += round_end.duration_since(t_first).as_millis() as u64;
                }
                None => {
                    // No content (pure tool-call round): all prefill
                    total_prefill_ms += round_end.duration_since(round_start).as_millis() as u64;
                }
            }

            if let Some(e) = stream_error {
                emit(AgentEvent::Error(format!("stream failed: {e}")));
                self.rollback_last_user_message(session_id).await;
                return;
            }

            let Some(result) = result else {
                emit(AgentEvent::Error("stream ended without result".into()));
                self.rollback_last_user_message(session_id).await;
                return;
            };

            if let Some(u) = &result.usage {
                total_completion += u.completion_tokens;
                total_prefill_tokens += u.prompt_tokens;
                last_prompt_tokens = u.prompt_tokens;
            }

            // No tool calls → we're done
            if result.tool_calls.is_empty() {
                final_content = result.content;
                break;
            }

            // Record assistant message with tool calls
            {
                let mut sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get_mut(&session_id) {
                    s.history.push(ChatMessage::assistant(
                        if result.content.is_empty() {
                            None
                        } else {
                            Some(result.content.clone())
                        },
                        Some(result.tool_calls.clone()),
                    ));
                }
            }

            // Execute each tool call
            for tc in &result.tool_calls {
                let name = &tc.function.name;
                emit(AgentEvent::ToolEvent {
                    name: name.clone(),
                    status: ToolStatus::Started,
                    detail: summarize_args(&tc.function.arguments),
                });

                let output = self
                    .execute_tool(session_id, name, &tc.function.arguments, scope.clone(), &emit)
                    .await;

                let (status, text) = match &output {
                    Ok(t) => (ToolStatus::Finished, truncate(t, MAX_TOOL_RESULT)),
                    Err(e) => (ToolStatus::Failed, truncate(e, MAX_TOOL_RESULT)),
                };

                emit(AgentEvent::ToolEvent {
                    name: name.clone(),
                    status,
                    detail: text.chars().take(200).collect(),
                });

                let result_text = output.unwrap_or_else(|e| format!("Error: {e}"));
                let mut sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get_mut(&session_id) {
                    s.history.push(ChatMessage::tool_result(
                        tc.id.clone(),
                        name.clone(),
                        truncate(&result_text, MAX_TOOL_RESULT),
                    ));
                }
            }

            if round == MAX_TOOL_ROUNDS - 1 {
                final_content = format!(
                    "(stopped after {MAX_TOOL_ROUNDS} tool rounds — task may be incomplete)"
                );
            }
        }

        // Store assistant reply in history + DB
        if !final_content.is_empty() {
            let mut sessions = self.sessions.lock().await;
            if let Some(s) = sessions.get_mut(&session_id) {
                s.history.push(ChatMessage::assistant(
                    Some(final_content.clone()),
                    None,
                ));
            }
            drop(sessions);

            // Persist to DB (user + assistant) for skills extraction later
            let _ = self.memory.store_conversation(&ConversationEntry {
                role: "user".into(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                session_id,
                model_used: None,
            }).await;
            let _ = self.memory.store_conversation(&ConversationEntry {
                role: "assistant".into(),
                content: final_content.clone(),
                timestamp: chrono::Utc::now(),
                session_id,
                model_used: Some(model.clone()),
            }).await;
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        let prefill_tps = if total_prefill_ms > 0 {
            total_prefill_tokens as f64 / (total_prefill_ms as f64 / 1000.0)
        } else {
            0.0
        };
        let decode_tps = if total_decode_ms > 0 {
            total_completion as f64 / (total_decode_ms as f64 / 1000.0)
        } else {
            0.0
        };
        emit(AgentEvent::Done(ChatStatsData {
            model,
            prompt_tokens: last_prompt_tokens,
            completion_tokens: total_completion,
            duration_ms,
            context_length: last_prompt_tokens,
            prefill_tps,
            decode_tps,
        }));
    }

    async fn rollback_last_user_message(&self, session_id: Uuid) {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get_mut(&session_id) {
            if s.history.last().map(|m| m.role.as_str()) == Some("user") {
                s.history.pop();
            }
        }
    }

    async fn build_system_prompt(&self) -> String {
        let personality = self.memory.get_personality().await.unwrap_or_default();
        let skills = self.memory.get_all_skills().await.unwrap_or_default();

        let mut prompt = format!(
            "{personality}\n\n\
            You are renco, an agentic coding assistant running on the user's own hardware.\n\
            You have tools for filesystem access, shell commands, web search/fetch, NixOS \
            queries, arXiv, and library docs (context7). Use them when they help.\n\n\
            Guidelines:\n\
            - Be direct and technical. No filler.\n\
            - Prefer reading files over guessing their contents.\n\
            - Use execute_command for builds/tests; check errors and fix them.\n\
            - The working directory is the user's project root; relative paths resolve there.\n"
        );

        if !skills.is_empty() {
            prompt.push_str("\n## Skills you've learned\n");
            for skill in skills.iter().take(10) {
                prompt.push_str(&format!(
                    "- **{}**: {} (used {}×)\n",
                    skill.name, skill.description, skill.frequency
                ));
            }
        }
        prompt
    }

    /// Execute a tool with permission enforcement.
    async fn execute_tool(
        &self,
        session_id: Uuid,
        name: &str,
        args_json: &str,
        scope: Arc<Mutex<PermissionScope>>,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<String, String> {
        let args = LiteLLM::recover_tool_call(args_json)
            .ok_or_else(|| format!("cannot parse arguments for {name}"))?;

        match name {
            "read_file" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                let resolved = {
                    let s = scope.lock().await;
                    s.resolve(std::path::Path::new(path))
                };
                self.check_perm(session_id, &resolved, AccessKind::Read, scope.clone(), emit)
                    .await?;
                tokio::fs::read_to_string(&resolved)
                    .await
                    .map_err(|e| format!("read {}: {e}", resolved.display()))
            }

            "write_file" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                let content = args["content"].as_str().ok_or("missing content")?;
                let resolved = {
                    let s = scope.lock().await;
                    s.resolve(std::path::Path::new(path))
                };
                self.check_perm(session_id, &resolved, AccessKind::Write, scope.clone(), emit)
                    .await?;
                if let Some(parent) = resolved.parent() {
                    tokio::fs::create_dir_all(parent).await.ok();
                }
                tokio::fs::write(&resolved, content)
                    .await
                    .map_err(|e| format!("write {}: {e}", resolved.display()))?;
                Ok(format!("wrote {} ({} bytes)", resolved.display(), content.len()))
            }

            "list_dir" => {
                let path = args["path"].as_str().unwrap_or(".");
                let resolved = {
                    let s = scope.lock().await;
                    s.resolve(std::path::Path::new(path))
                };
                self.check_perm(session_id, &resolved, AccessKind::ReadDir, scope.clone(), emit)
                    .await?;
                let mut entries = tokio::fs::read_dir(&resolved)
                    .await
                    .map_err(|e| format!("readdir {}: {e}", resolved.display()))?;
                let mut out = Vec::new();
                while let Ok(Some(e)) = entries.next_entry().await {
                    let name = e.file_name().to_string_lossy().to_string();
                    let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    out.push(format!("{}{}", name, if is_dir { "/" } else { "" }));
                }
                Ok(out.join("\n"))
            }

            "execute_command" => {
                let command = args["command"].as_str().ok_or("missing command")?;
                let cwd = {
                    let s = scope.lock().await;
                    s.cwd()
                };
                // Command execution requires Execute permission on CWD
                self.check_perm(session_id, &cwd, AccessKind::Execute, scope.clone(), emit)
                    .await?;

                let output = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .current_dir(&cwd)
                    .output()
                    .await
                    .map_err(|e| format!("spawn: {e}"))?;

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let mut out = String::new();
                if !stdout.is_empty() {
                    out.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    out.push_str(&format!("\n[stderr]\n{stderr}"));
                }
                if !output.status.success() {
                    out.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
                }
                if out.is_empty() {
                    out.push_str("(no output)");
                }
                Ok(out)
            }

            // Everything else: try MCP
            _ => self.mcp.call_tool(name, args).await,
        }
    }

    /// Check permission for a path; if it needs asking, emit a request to the
    /// client and await the user's reply.
    async fn check_perm(
        &self,
        session_id: Uuid,
        path: &std::path::Path,
        access: AccessKind,
        scope: Arc<Mutex<PermissionScope>>,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<(), String> {
        let verdict = {
            let s = scope.lock().await;
            s.check_access(path, access).map(|_| ()).map_err(|p| p)
        };

        match verdict {
            Ok(()) => Ok(()),
            Err(needs_path) => {
                let request_id = Uuid::new_v4();
                emit(AgentEvent::PermissionNeeded(PermissionRequest {
                    request_id,
                    session_id,
                    path: needs_path.display().to_string(),
                    access,
                }));

                let rx = self.permissions.ask(request_id).await;
                match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
                    Ok(Ok(true)) => Ok(()),
                    Ok(Ok(false)) => Err(format!(
                        "permission denied: {}",
                        needs_path.display()
                    )),
                    _ => Err("permission request timed out".into()),
                }
            }
        }
    }
}

/// Local (non-MCP) tools the agent can use directly.
fn local_tools() -> Vec<ToolDefinition> {
    use crate::litellm::ToolFunctionDef;
    vec![
        ToolDefinition {
            type_field: "function".into(),
            function: ToolFunctionDef {
                name: "read_file".into(),
                description: "Read a file's contents. Relative paths resolve from the working directory.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path to read"},
                    },
                    "required": ["path"],
                }),
            },
        },
        ToolDefinition {
            type_field: "function".into(),
            function: ToolFunctionDef {
                name: "write_file".into(),
                description: "Write content to a file (creates parent dirs). Relative paths resolve from the working directory.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"},
                    },
                    "required": ["path", "content"],
                }),
            },
        },
        ToolDefinition {
            type_field: "function".into(),
            function: ToolFunctionDef {
                name: "list_dir".into(),
                description: "List directory contents. Defaults to the working directory.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                    },
                }),
            },
        },
        ToolDefinition {
            type_field: "function".into(),
            function: ToolFunctionDef {
                name: "execute_command".into(),
                description: "Run a shell command in the working directory. Returns stdout, stderr, and exit code.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"},
                    },
                    "required": ["command"],
                }),
            },
        },
    ]
}

fn summarize_args(args_json: &str) -> String {
    let v = LiteLLM::recover_tool_call(args_json);
    match v {
        Some(val) => {
            let s = serde_json::to_string(&val).unwrap_or_default();
            s.chars().take(120).collect()
        }
        None => args_json.chars().take(120).collect(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…[truncated {} bytes]", &s[..max], s.len() - max)
    }
}
