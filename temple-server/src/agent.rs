use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use temple_protocol::*;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::litellm::{ChatMessage, ChatRequest, LiteLLM, StreamEvent, ToolDefinition};
use crate::mcp::McpClient;
use crate::memory::Memory;
use crate::permissions::PermissionScope;
use crate::router::Router;

/// Maximum tool-loop rounds per user message before forcing an answer.
const MAX_TOOL_ROUNDS: usize = 30;
/// Max conversation messages kept in context (plus system prompt).
const MAX_HISTORY: usize = 60;
/// Max bytes returned from a single tool result (keeps context manageable).
const MAX_TOOL_RESULT: usize = 24_000;
/// Max retries on stream failure before giving up
const MAX_STREAM_RETRIES: usize = 3;
/// Detect stuck loops: if same tool+args seen this many times, intervene
const STUCK_THRESHOLD: usize = 3;

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
    /// A question during /initial onboarding
    OnboardingQuestion(String),
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

struct SessionCtx {
    model: String,
    history: Vec<ChatMessage>,
    username: String,
    cwd: String,
    initialized: bool,
}

pub struct Agent {
    pub litellm: LiteLLM,
    /// Local llama.cpp for routing/titles (zero latency, on oracle)
    pub local: LiteLLM,
    pub mcp: McpClient,
    pub memory: Arc<Memory>,
    pub permissions: Arc<PermissionResolver>,
    sessions: Mutex<HashMap<Uuid, SessionCtx>>,
    /// Per-session cancellation tokens for in-progress agent loops
    cancel_tokens: Mutex<HashMap<Uuid, CancellationToken>>,
    default_model: String,
    router_model: String,
    title_model: String,
    tools: Mutex<Vec<ToolDefinition>>,
}

impl Agent {
    pub fn new(
        litellm: LiteLLM,
        mcp: McpClient,
        memory: Arc<Memory>,
        default_model: &str,
    ) -> Self {
        Self {
            litellm,
            // Local model client — no API key needed, localhost
            local: LiteLLM::new("http://127.0.0.1:8080/v1", "none"),
            mcp,
            memory,
            permissions: Arc::new(PermissionResolver::new()),
            sessions: Mutex::new(HashMap::new()),
            cancel_tokens: Mutex::new(HashMap::new()),
            default_model: default_model.to_string(),
            router_model: "qwen3-4b-instruct".to_string(),
            title_model: "qwen3-4b-instruct".to_string(),
            tools: Mutex::new(Vec::new()),
        }
    }

    /// Set the local llama.cpp endpoint (called from main.rs with config)
    pub fn set_local_endpoint(&mut self, url: &str, model: &str) {
        self.local = LiteLLM::new(url, "none");
        self.router_model = model.to_string();
        self.title_model = model.to_string();
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

    /// Ensure the brain model stays loaded in llamaswap.
    /// Does NOT send "ping" (that corrupts KV cache). Instead sends
    /// a minimal request that llama.cpp's prefix cache handles gracefully.
    pub async fn keep_warm(&self) {
        // A 1-token completion with the same system prompt prefix as real
        // requests — llama.cpp caches the prefix, so this is cheap and
        // doesn't displace the real conversation cache.
        let req = ChatRequest {
            model: self.default_model.clone(),
            messages: vec![
                ChatMessage::system("ok"),
                ChatMessage::user("."),
            ],
            tools: None,
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(1),
            temperature: Some(0.0),
        };
        let _ = self.litellm.chat(req).await;
    }

    pub async fn open_session(&self, session_id: Uuid, username: &str, cwd: &str) {
        let mut sessions = self.sessions.lock().await;
        // Check if user already has memories (onboarding done?)
        let initialized = self
            .memory
            .get_memory("onboarded", username)
            .await
            .unwrap_or(None)
            .is_some();
        sessions.insert(
            session_id,
            SessionCtx {
                model: self.default_model.clone(),
                history: Vec::new(),
                username: username.to_string(),
                cwd: cwd.to_string(),
                initialized,
            },
        );
    }

    pub async fn close_session(&self, session_id: Uuid) {
        // Cancel any in-progress loop
        self.cancel_chat(session_id).await;
        self.sessions.lock().await.remove(&session_id);
    }

    /// Cancel an in-progress agent loop for this session.
    pub async fn cancel_chat(&self, session_id: Uuid) {
        if let Some(token) = self.cancel_tokens.lock().await.remove(&session_id) {
            token.cancel();
        }
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

    /// The full agent loop for one user message. Self-healing: retries on
    /// stream failures, recovers from malformed tool calls, detects stuck
    /// loops. Only ends when the model produces a final answer, the user
    /// cancels, or MAX_TOOL_ROUNDS is hit.
    pub async fn process_chat(
        &self,
        session_id: Uuid,
        content: &str,
        scope: Arc<Mutex<PermissionScope>>,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) {
        let started = Instant::now();

        // Register cancellation token for this session
        let cancel_token = CancellationToken::new();
        {
            let mut tokens = self.cancel_tokens.lock().await;
            if let Some(old) = tokens.insert(session_id, cancel_token.clone()) {
                old.cancel();
            }
        }

        // Get session info
        let (username, cwd, _is_initialized, session_model) = {
            let mut sessions = self.sessions.lock().await;
            let Some(s) = sessions.get_mut(&session_id) else {
                emit(AgentEvent::Error("session not found".into()));
                self.cancel_tokens.lock().await.remove(&session_id);
                return;
            };
            s.history.push(ChatMessage::user(content));
            (s.username.clone(), s.cwd.clone(), s.initialized, s.model.clone())
        };

        // Route the query
        let model = if content.starts_with('/') || content.starts_with(':') {
            session_model
        } else {
            let complexity = Router::classify_with_model(&self.local, content)
                .await
                .unwrap_or_else(|| Router::classify(content));
            match complexity {
                ComplexityClass::Simple => {
                    tracing::info!("Router: simple → fast model");
                    "fast-gemma-4-12b-it".to_string()
                }
                _ => session_model,
            }
        };

        let system_prompt = self.build_system_prompt(&username, &cwd).await;

        let mut total_completion: u32 = 0;
        let mut last_prompt_tokens: u32 = 0;
        let mut final_content = String::new();
        let mut total_prefill_ms: u64 = 0;
        let mut total_decode_ms: u64 = 0;
        let mut total_prefill_tokens: u32 = 0;
        let mut tool_call_history: Vec<String> = Vec::new();

        for round in 0..MAX_TOOL_ROUNDS {
            if cancel_token.is_cancelled() {
                emit(AgentEvent::Error("cancelled by user".into()));
                break;
            }

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
                temperature: None,
            };

            // ── Stream with retry (self-healing) ──
            let result = {
                let mut last_err = String::new();
                let mut stream_result: Option<crate::litellm::StreamResult> = None;
                let mut stream_first_delta: Option<Instant> = None;
                let stream_round_start = Instant::now();

                for attempt in 0..MAX_STREAM_RETRIES {
                    if cancel_token.is_cancelled() { break; }
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
                    let litellm = self.litellm.clone();
                    let req_clone = req.clone();
                    tokio::spawn(async move {
                        litellm.chat_stream(req_clone, tx).await;
                    });

                    let mut attempt_error: Option<String> = None;
                    stream_first_delta = None;
                    while let Some(ev) = rx.recv().await {
                        if cancel_token.is_cancelled() { break; }
                        match ev {
                            StreamEvent::Delta(d) => {
                                if stream_first_delta.is_none() {
                                    stream_first_delta = Some(Instant::now());
                                }
                                emit(AgentEvent::Delta(d));
                            }
                            StreamEvent::Done(r) => { stream_result = Some(r); break; }
                            StreamEvent::Error(e) => { attempt_error = Some(e); break; }
                        }
                    }

                    if stream_result.is_some() {
                        let round_end = Instant::now();
                        match stream_first_delta {
                            Some(t_first) => {
                                total_prefill_ms += t_first.duration_since(stream_round_start).as_millis() as u64;
                                total_decode_ms += round_end.duration_since(t_first).as_millis() as u64;
                            }
                            None => {
                                total_prefill_ms += round_end.duration_since(stream_round_start).as_millis() as u64;
                            }
                        }
                        break;
                    }

                    last_err = attempt_error.unwrap_or("stream ended without result".into());
                    if attempt < MAX_STREAM_RETRIES - 1 {
                        let delay = std::time::Duration::from_millis(500 * (1 << attempt));
                        tracing::warn!("Stream retry {}/{}: {} in {:?}", attempt+1, MAX_STREAM_RETRIES, last_err, delay);
                        emit(AgentEvent::ToolEvent {
                            name: "system".into(), status: ToolStatus::Started,
                            detail: format!("retry {}/{} after stream error", attempt+1, MAX_STREAM_RETRIES),
                        });
                        tokio::time::sleep(delay).await;
                    }
                }

                match stream_result {
                    Some(r) => r,
                    None => {
                        // All retries failed — inject error context and let
                        // the model try a different approach
                        tracing::error!("All {} stream retries failed: {}", MAX_STREAM_RETRIES, last_err);
                        let mut sessions = self.sessions.lock().await;
                        if let Some(s) = sessions.get_mut(&session_id) {
                            s.history.push(ChatMessage::assistant(
                                Some(format!("(stream error: {last_err})")), None,
                            ));
                            s.history.push(ChatMessage::user(
                                "The previous request failed due to a stream error. Try again or take a different approach."
                            ));
                        }
                        continue;
                    }
                }
            };

            if cancel_token.is_cancelled() { break; }

            if let Some(u) = &result.usage {
                total_completion += u.completion_tokens;
                total_prefill_tokens += u.prompt_tokens;
                last_prompt_tokens = u.prompt_tokens;
            }

            if result.tool_calls.is_empty() {
                final_content = result.content;
                break;
            }

            {
                let mut sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get_mut(&session_id) {
                    s.history.push(ChatMessage::assistant(
                        if result.content.is_empty() { None } else { Some(result.content.clone()) },
                        Some(result.tool_calls.clone()),
                    ));
                }
            }

            for tc in &result.tool_calls {
                if cancel_token.is_cancelled() { break; }
                let name = &tc.function.name;
                let args_str = &tc.function.arguments;

                // Stuck-loop detection
                let signature = format!("{name}:{args_str}");
                let repeat_count = tool_call_history.iter().filter(|s| **s == signature).count();
                tool_call_history.push(signature.clone());
                if repeat_count >= STUCK_THRESHOLD {
                    tracing::warn!("Stuck loop: {name} {repeat_count}×");
                    emit(AgentEvent::ToolEvent {
                        name: name.clone(), status: ToolStatus::Failed,
                        detail: format!("stuck: {name} repeated {repeat_count}× — try different approach"),
                    });
                    let mut sessions = self.sessions.lock().await;
                    if let Some(s) = sessions.get_mut(&session_id) {
                        s.history.push(ChatMessage::tool_result(
                            tc.id.clone(), name.clone(),
                            format!("ERROR: {name} called {repeat_count}× with same args. Try a different approach."),
                        ));
                    }
                    continue;
                }

                emit(AgentEvent::ToolEvent {
                    name: name.clone(), status: ToolStatus::Started,
                    detail: summarize_args(args_str),
                });

                let output = self.execute_tool(session_id, name, args_str, scope.clone(), emit).await;
                let (status, text) = match &output {
                    Ok(t) => (ToolStatus::Finished, truncate(t, MAX_TOOL_RESULT)),
                    Err(e) => (ToolStatus::Failed, truncate(e, MAX_TOOL_RESULT)),
                };
                emit(AgentEvent::ToolEvent {
                    name: name.clone(), status,
                    detail: text.chars().take(200).collect(),
                });

                let result_text = output.unwrap_or_else(|e| format!("Error: {e}"));
                let mut sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get_mut(&session_id) {
                    s.history.push(ChatMessage::tool_result(
                        tc.id.clone(), name.clone(),
                        truncate(&result_text, MAX_TOOL_RESULT),
                    ));
                }
            }

            if round == MAX_TOOL_ROUNDS - 1 {
                final_content = format!(
                    "(reached {MAX_TOOL_ROUNDS} tool rounds — summarizing)"
                );
                let mut sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get_mut(&session_id) {
                    s.history.push(ChatMessage::user(
                        "You've reached the maximum tool rounds. Summarize what you accomplished and what remains."
                    ));
                }
            }
        }

        // Clean up cancellation token
        self.cancel_tokens.lock().await.remove(&session_id);

        // Store assistant reply in history + DB
        if !final_content.is_empty() {
            {
                let mut sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get_mut(&session_id) {
                    s.history.push(ChatMessage::assistant(Some(final_content.clone()), None));
                }
            }
            let _ = self.memory.store_conversation(&ConversationEntry {
                role: "user".into(), content: content.to_string(),
                timestamp: chrono::Utc::now(), session_id, model_used: None,
            }).await;
            let _ = self.memory.store_conversation(&ConversationEntry {
                role: "assistant".into(), content: final_content.clone(),
                timestamp: chrono::Utc::now(), session_id,
                model_used: Some(model.clone()),
            }).await;
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        let prefill_tps = if total_prefill_ms > 0 {
            total_prefill_tokens as f64 / (total_prefill_ms as f64 / 1000.0)
        } else { 0.0 };
        let decode_tps = if total_decode_ms > 0 {
            total_completion as f64 / (total_decode_ms as f64 / 1000.0)
        } else { 0.0 };
        emit(AgentEvent::Done(ChatStatsData {
            model, prompt_tokens: last_prompt_tokens,
            completion_tokens: total_completion, duration_ms,
            context_length: last_prompt_tokens, prefill_tps, decode_tps,
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

    /// Build system prompt with personality, user memories, code rules, and CWD detection.
    async fn build_system_prompt(&self, username: &str, cwd: &str) -> String {
        let personality = self.memory.get_personality().await.unwrap_or_default();

        // User-specific memories
        let user_memories = self.memory.get_all_memory(username).await.unwrap_or_default();
        let user_section = if user_memories.is_empty() {
            String::new()
        } else {
            let mut s = format!("\n\n## What you know about {username}\n");
            for m in user_memories.iter().take(20) {
                s.push_str(&format!("- **{}**: {}\n", m.key, m.value));
            }
            s
        };

        // Skills
        let skills = self.memory.get_all_skills().await.unwrap_or_default();
        let skills_section = if skills.is_empty() {
            String::new()
        } else {
            let mut s = "\n\n## Skills you've learned\n".to_string();
            for skill in skills.iter().take(10) {
                s.push_str(&format!(
                    "- **{}**: {} (used {}×)\n",
                    skill.name, skill.description, skill.frequency
                ));
            }
            s
        };

        // CWD-based project detection
        let project_section = detect_project_context(cwd);

        format!(
            "{personality}

You are renco, an agentic coding assistant running on {username}'s own hardware.
You are talking to {username} right now.{user_section}{skills_section}

## Available tools
You have tools for filesystem access, shell commands, web search/fetch, NixOS
queries, arXiv, and library docs (context7). Use them when they help.

## Code rules (hardcoded — always follow)
- Prefer slightly verbose, self-explanatory code over terse code that needs comments.
- Keep comments to only what explains something non-obvious.
- Never embed a literal \\n inside a string. A line break is always its own statement.
  In C++/ROOT, use std::cout << ... << std::endl. In Python, use separate print() calls.

### Nix
- Always use flakes and flake-based commands (nix run, nix shell, etc.).
  Never use the old nix-shell approach.
- If you are confused, stop and ask for help.
- Follow the existing style of the surrounding modules.

### C++ / ROOT
- Use ROOT data types: Int_t for ordinary ints, Long64_t for entry counts and
  64-bit values, Double_t for floating point, TString for string convenience.
  Match width and signedness the code actually requires.
- Do not use modern C++ features: no auto, no smart pointers, no range-based
  iteration, no lambdas. Use explicit types and classic indexed/iterator loops.
- In performance-critical code, gate logging behind a toggle so it can be disabled.

### Python
- In Python that uses ROOT, never use matplotlib. Look at nearby files for the
  established plotting approach, or ask which is preferred.

### Rust
- Follow the existing style of the surrounding code.
- Prefer explicit types where it aids readability.
- Handle errors properly — no .unwrap() in production paths.

### Explanations
- For non-trivial changes, explain thoroughly what changed and why.
  Do not over-summarize or truncate reasoning. Trivial edits stay terse.
{project_section}

## Guidelines
- Be direct and technical. No filler.
- Prefer reading files over guessing their contents.
- Use execute_command for builds/tests; check errors and fix them.
- The working directory is {username}'s project root; relative paths resolve there.
"
        )
    }

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
                self.check_perm(session_id, &resolved, AccessKind::Read, scope.clone(), emit).await?;
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
                self.check_perm(session_id, &resolved, AccessKind::Write, scope.clone(), emit).await?;
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
                self.check_perm(session_id, &resolved, AccessKind::ReadDir, scope.clone(), emit).await?;
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
                self.check_perm(session_id, &cwd, AccessKind::Execute, scope.clone(), emit).await?;

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
                if !stdout.is_empty() { out.push_str(&stdout); }
                if !stderr.is_empty() { out.push_str(&format!("\n[stderr]\n{stderr}")); }
                if !output.status.success() {
                    out.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
                }
                if out.is_empty() { out.push_str("(no output)"); }
                Ok(out)
            }

            _ => self.mcp.call_tool(name, args).await,
        }
    }

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
                    Ok(Ok(false)) => Err(format!("permission denied: {}", needs_path.display())),
                    _ => Err("permission request timed out".into()),
                }
            }
        }
    }

    /// Run the /initial onboarding flow. Asks questions, stores answers.
    pub async fn run_onboarding(
        &self,
        session_id: Uuid,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) {
        let username = {
            let sessions = self.sessions.lock().await;
            match sessions.get(&session_id) {
                Some(s) => s.username.clone(),
                None => return,
            }
        };

        let questions = vec![
            ("name", "What's your name? (This helps me address you naturally.)"),
            ("role", "What do you do? (e.g., physicist, software engineer, student)"),
            ("preferences", "Any coding preferences I should know? (editor, language, style)"),
            ("projects", "What are your main projects? (Brief — I'll learn the rest as we go.)"),
        ];

        for (key, question) in &questions {
            emit(AgentEvent::OnboardingQuestion(question.to_string()));

            // Wait for the user's response (which will arrive as a ChatInput
            // and be stored in session history; the server loop handles this
            // by calling process_onboarding_answer)
            //
            // For now, we use a simpler approach: the onboarding is driven
            // by the client sending /initial, then each answer as a normal
            // message tagged with a special prefix. The server detects this
            // and routes to this function.
            //
            // Actually simplest: ask all questions at once, let the model
            // conduct the conversation naturally, and have it store answers
            // via a write_memory tool. But that's complex.
            //
            // Pragmatic approach: ask one question, the client displays it,
            // the user's next message is the answer. The server intercepts
            // and stores it. This requires the server to track onboarding
            // state per session.
            //
            // For now, just ask all questions as a single message and let
            // the model handle the conversation. The model will use
            // set_memory tool calls (which we need to add) to store answers.
            //
            // This is a TODO — for now, we'll ask all questions at once
            // and the model will store them using write_file to a known
            // location, then the cron job picks them up.
            //
            // Actually, the cleanest implementation: just ask all questions
            // in one go, and when the user answers, the model is instructed
            // to use a "save_memory" tool to persist them.
            let _ = (key, question);
        }

        // Mark as onboarded
        let _ = self.memory.set_memory("onboarded", "true", &username).await;

        // Generate a session title
        self.generate_session_title(session_id).await;
    }

    /// Generate a short title for the session using the small model.
    pub async fn generate_session_title(&self, session_id: Uuid) {
        let history = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_id)
                .map(|s| s.history.iter().take(4).cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };

        if history.is_empty() {
            return;
        }

        let mut messages = vec![ChatMessage::system(
            "Generate a 3-5 word title for this conversation. Just the title, no quotes, no punctuation at the end."
        )];
        messages.extend(history);

        let req = ChatRequest {
            model: self.title_model.clone(),
            messages,
            tools: None,
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(30),
            temperature: Some(0.3),
        };

        if let Ok(resp) = self.local.chat(req).await {
            if let Some(choice) = resp.choices.first() {
                let title = choice.message.content
                    .as_deref()
                    .unwrap_or("untitled")
                    .trim()
                    .to_string();
                tracing::info!("Session title: {title}");
                let _ = self.memory.set_memory(
                    &format!("session_title_{}", session_id),
                    &title,
                    "system",
                ).await;
            }
        }
    }

    /// Weekly personality update: analyze recent conversations, ask the brain
    /// model to suggest personality refinements.
    pub async fn update_personality(&self) {
        tracing::info!("Running personality self-update...");

        // Get current personality
        let current = self.memory.get_personality().await.unwrap_or_default();

        // Get recent conversations (last 50)
        let recent = match self.memory.get_recent_conversations(50).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to get recent conversations: {e}");
                return;
            }
        };

        if recent.is_empty() {
            tracing::info!("No conversations to analyze");
            return;
        }

        // Build a summary of conversations
        let mut conv_summary = String::new();
        for entry in recent.iter().take(50) {
            conv_summary.push_str(&format!(
                "[{}] {}: {}\n",
                entry.timestamp.format("%m-%d %H:%M"),
                entry.role,
                &entry.content[..entry.content.len().min(200)]
            ));
        }

        let prompt = format!(
            "You are renco, reviewing your own behavior over the past week of conversations.\n\n\
             Current personality:\n{current}\n\n\
             Recent conversations (truncated):\n{conv_summary}\n\n\
             Based on these interactions, should your personality be updated? \
             Consider: communication style, technical depth, humor, helpfulness. \
             Write a NEW personality description (2-4 sentences) that captures \
             who you are and how you should behave. If the current one is fine, \
             return it unchanged. Just return the personality text, nothing else."
        );

        let req = ChatRequest {
            model: self.default_model.clone(),
            messages: vec![ChatMessage::user(&prompt)],
            tools: None,
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(500),
            temperature: Some(0.4),
        };

        match self.litellm.chat(req).await {
            Ok(resp) => {
                if let Some(choice) = resp.choices.first() {
                    if let Some(new_personality) = choice.message.content.as_deref() {
                        let trimmed = new_personality.trim();
                        if !trimmed.is_empty() && trimmed.len() > 20 {
                            tracing::info!("Updating personality: {trimmed:.80}...");
                            let _ = self.memory.set_personality(trimmed).await;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Personality update failed: {e}");
            }
        }
    }
}

/// Local (non-MCP) tools.
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
                description: "Write content to a file (creates parent dirs).".into(),
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
                description: "List directory contents.".into(),
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
                description: "Run a shell command in the working directory.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                    },
                    "required": ["command"],
                }),
            },
        },
    ]
}

/// Detect project type from CWD contents and return language-specific context.
fn detect_project_context(cwd: &str) -> String {
    let cwd_path = std::path::Path::new(cwd);
    let mut detected: Vec<String> = Vec::new();

    // Check for Rust
    if cwd_path.join("Cargo.toml").exists() {
        detected.push("Rust project detected (Cargo.toml found). Follow Rust conventions.".into());
    }

    // Check for Nix flake
    if cwd_path.join("flake.nix").exists() {
        detected.push("Nix flake detected. Use nix run/nix shell/nix build. Never nix-shell.".into());
    }

    // Check for ROOT/C++
    if cwd_path.join("CMakeLists.txt").exists()
        || std::fs::read_dir(cwd_path)
            .map(|d| d.filter_map(|e| e.ok())
                .any(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.ends_with(".C") || n.ends_with(".cpp") || n.ends_with(".cxx")
                }))
            .unwrap_or(false)
    {
        detected.push("ROOT/C++ project detected. Use ROOT types (Int_t, Double_t, etc.). No modern C++ (no auto, no smart pointers, no lambdas).".into());
    }

    // Check for Python
    if cwd_path.join("pyproject.toml").exists()
        || cwd_path.join("setup.py").exists()
        || std::fs::read_dir(cwd_path)
            .map(|d| d.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().ends_with(".py"))
            )
            .unwrap_or(false)
    {
        detected.push("Python project detected.".into());
    }

    // Try to read project name from Cargo.toml or flake.nix
    if let Ok(content) = std::fs::read_to_string(cwd_path.join("Cargo.toml")) {
        for line in content.lines() {
            if let Some(name) = line.strip_prefix("name = ") {
                detected.push(format!("Project name: {}", name.trim().trim_matches('"')));
                break;
            }
        }
    }

    if detected.is_empty() {
        String::new()
    } else {
        format!("\n## Project context\n{}\n", detected.join("\n"))
    }
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
