use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use temple_protocol::*;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::ModelConfig;
use crate::litellm::{ChatMessage, ChatRequest, LiteLLM, StreamEvent, ToolDefinition};
use crate::mcp::McpClient;
use crate::memory::Memory;
use crate::permissions::PermissionScope;
use crate::router::{Route, Router, SessionKind};

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
/// Max planner→executor→reviewer revision rounds
const MAX_REVISION_ROUNDS: usize = 3;

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

/// Accumulates token/timing stats across multiple tool-loop rounds
/// and pipeline revision rounds.
struct StatsAccum {
    total_completion: u32,
    last_prompt_tokens: u32,
    total_prefill_ms: u64,
    total_decode_ms: u64,
    total_prefill_tokens: u32,
}

impl StatsAccum {
    fn new() -> Self {
        Self {
            total_completion: 0,
            last_prompt_tokens: 0,
            total_prefill_ms: 0,
            total_decode_ms: 0,
            total_prefill_tokens: 0,
        }
    }

    fn prefill_tps(&self) -> f64 {
        if self.total_prefill_ms > 0 {
            self.total_prefill_tokens as f64 / (self.total_prefill_ms as f64 / 1000.0)
        } else {
            0.0
        }
    }

    fn decode_tps(&self) -> f64 {
        if self.total_decode_ms > 0 {
            self.total_completion as f64 / (self.total_decode_ms as f64 / 1000.0)
        } else {
            0.0
        }
    }
}

struct SessionCtx {
    model: String,
    history: Vec<ChatMessage>,
    username: String,
    cwd: String,
    initialized: bool,
    kind: SessionKind,
    /// SSH executor for remote tool execution (None = local tools only)
    ssh: Option<Arc<crate::ssh::SshExecutor>>,
    /// SSH target name (e.g. "e-work@e-desktop") for persistence/display
    ssh_target_name: Option<String>,
    /// Person who owns this session (token username, e.g. "ethan").
    owner: String,
    /// Session title (generated after first exchange)
    title: Option<String>,
    /// Whether this session should be persisted to the DB
    persist: bool,
    /// Permission mode, persisted and restored on resume.
    /// SSH sessions can never be Yolo.
    permission_mode: PermissionMode,
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
    pub models: ModelConfig,
    router_model: String,
    title_model: String,
    tools: Mutex<Vec<ToolDefinition>>,
    /// SSH targets for remote tool execution (from config)
    ssh_targets: Vec<crate::config::SshTarget>,
    ssh_key_path: Option<std::path::PathBuf>,
    ssh_bastion: Option<String>,
}

impl Agent {
    pub fn new(
        litellm: LiteLLM,
        mcp: McpClient,
        memory: Arc<Memory>,
        models: ModelConfig,
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
            router_model: "qwen3-4b-instruct".to_string(),
            title_model: "qwen3-4b-instruct".to_string(),
            models,
            tools: Mutex::new(Vec::new()),
            ssh_targets: Vec::new(),
            ssh_key_path: None,
            ssh_bastion: None,
        }
    }

    /// Set the local llama.cpp endpoint (called from main.rs with config)
    pub fn set_local_endpoint(&mut self, url: &str, model: &str) {
        self.local = LiteLLM::new(url, "none");
        self.router_model = model.to_string();
        self.title_model = model.to_string();
    }

    /// Set the SSH execution config (called from main.rs with config)
    pub fn set_ssh_config(
        &mut self,
        targets: Vec<crate::config::SshTarget>,
        bastion: Option<String>,
        key_path: Option<std::path::PathBuf>,
    ) {
        self.ssh_targets = targets;
        self.ssh_bastion = bastion;
        self.ssh_key_path = key_path;
    }

    /// List configured SSH target names.
    pub fn ssh_target_names(&self) -> Vec<String> {
        self.ssh_targets.iter().map(|t| t.name.clone()).collect()
    }

    /// Build an SSH executor for a named target. Returns None if the target
    /// isn't configured or no key path is set.
    pub fn make_ssh(&self, target_name: &str) -> Option<Arc<crate::ssh::SshExecutor>> {
        let target = self.ssh_targets.iter().find(|t| t.name == target_name)?;
        let key = self.ssh_key_path.clone()
            .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/temple/ssh_key"));
        Some(Arc::new(crate::ssh::SshExecutor::new(
            target.clone(),
            self.ssh_bastion.clone(),
            key,
        )))
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
            model: self.models.default_model.clone(),
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

    pub async fn open_session(
        &self,
        session_id: Uuid,
        username: &str,
        cwd: &str,
        kind: SessionKind,
        ssh: Option<Arc<crate::ssh::SshExecutor>>,
    ) {
        let mut sessions = self.sessions.lock().await;
        let initialized = self
            .memory
            .get_memory("onboarded", username)
            .await
            .unwrap_or(None)
            .is_some();
        sessions.insert(
            session_id,
            SessionCtx {
                model: self.models.default_model.clone(),
                history: Vec::new(),
                username: username.to_string(),
                cwd: cwd.to_string(),
                initialized,
                kind,
                ssh,
                ssh_target_name: None,
                owner: username.to_string(),
                title: None,
                persist: false,
                permission_mode: PermissionMode::Default,
            },
        );
    }

    /// Create a new persisted session for `owner`, optionally bound to an
    /// SSH target. Returns the new session id, or Err if the target is
    /// unknown.
    pub async fn new_persisted_session(
        &self,
        owner: &str,
        ssh_target: Option<&str>,
    ) -> Result<Uuid, String> {
        let session_id = Uuid::new_v4();
        let (ssh, kind, cwd, full_name) = match ssh_target {
            Some(name) => {
                // Prefix-match against configured targets (Signal strips @)
                let target = self.ssh_targets.iter().find(|t| {
                    t.name == name
                        || t.name.starts_with(&format!("{name}@"))
                        || t.account == name
                }).cloned()
                .ok_or_else(|| format!(
                    "unknown ssh target: {name} (available: {})",
                    self.ssh_target_names().join(", ")
                ))?;
                let ssh = self.make_ssh(&target.name)
                    .ok_or_else(|| "ssh key not configured".to_string())?;
                let cwd = ssh.home_dir().await.unwrap_or_else(|_| "~".into());
                (Some(ssh), SessionKind::Coding, cwd, target.name)
            }
            None => (None, SessionKind::Quick, "/var/lib/temple".to_string(), "quick".to_string()),
        };

        let mut sessions = self.sessions.lock().await;
        sessions.insert(
            session_id,
            SessionCtx {
                model: self.models.default_model.clone(),
                history: Vec::new(),
                username: owner.to_string(),
                cwd,
                initialized: true,
                kind,
                ssh,
                ssh_target_name: Some(full_name),
                owner: owner.to_string(),
                title: None,
                persist: true,
                permission_mode: PermissionMode::Default,
            },
        );
        drop(sessions);
        self.persist_session(session_id).await;
        Ok(session_id)
    }

    /// Resume a persisted session from the DB. Loads history and
    /// reconstructs the SSH executor. Returns the transcript for replay.
    pub async fn resume_session(
        &self,
        session_id: Uuid,
        owner: &str,
    ) -> Result<(temple_protocol::SessionMeta, Vec<(String, String)>), String> {
        let row = self.memory.load_session(session_id).await
            .map_err(|e| format!("load session: {e}"))?
            .ok_or("session not found")?;

        if row.username != owner {
            return Err("session belongs to another user".into());
        }

        let history: Vec<ChatMessage> = serde_json::from_str(&row.history_json)
            .unwrap_or_default();

        let ssh = row.ssh_target.as_deref().and_then(|n| self.make_ssh(n));
        let kind = if ssh.is_some() { SessionKind::Coding } else { SessionKind::Quick };

        // Build transcript of user/assistant text turns for client replay
        let transcript: Vec<(String, String)> = history.iter()
            .filter_map(|m| {
                let content = m.content.clone()?;
                if (m.role == "user" || m.role == "assistant")
                    && !content.is_empty()
                    && m.tool_call_id.is_none()
                {
                    Some((m.role.clone(), content))
                } else {
                    None
                }
            })
            .collect();

        let meta = temple_protocol::SessionMeta {
            id: session_id,
            title: row.title.clone(),
            ssh_target: row.ssh_target.clone(),
            cwd: row.cwd.clone(),
            mode: row.mode.clone(),
            updated_at: String::new(),
        };

        let mut sessions = self.sessions.lock().await;
        sessions.insert(
            session_id,
            SessionCtx {
                model: self.models.default_model.clone(),
                history,
                username: row.username.clone(),
                cwd: row.cwd,
                initialized: true,
                kind,
                ssh,
                ssh_target_name: row.ssh_target,
                owner: row.username,
                title: row.title,
                persist: true,
                permission_mode: PermissionMode::Default,
            },
        );

        Ok((meta, transcript))
    }

    /// Persist a session's current state to the DB (no-op for
    /// non-persistent sessions like the raw TUI-connect default).
    pub async fn persist_session(&self, session_id: Uuid) {
        let row = {
            let sessions = self.sessions.lock().await;
            let Some(s) = sessions.get(&session_id) else { return };
            if !s.persist {
                return;
            }
            crate::memory::PersistedSession {
                id: session_id,
                username: s.owner.clone(),
                ssh_target: s.ssh_target_name.clone(),
                cwd: s.cwd.clone(),
                mode: format!("{:?}", s.permission_mode).to_lowercase(),
                kind: match s.kind {
                    SessionKind::Coding => "coding".into(),
                    SessionKind::Quick => "quick".into(),
                    SessionKind::System => "system".into(),
                },
                title: s.title.clone(),
                history_json: serde_json::to_string(&s.history).unwrap_or_else(|_| "[]".into()),
            }
        };
        if let Err(e) = self.memory.save_session(&row).await {
            tracing::warn!("persist session {session_id}: {e}");
        }
    }

    /// Generate and store a title for a session if it doesn't have one yet.
    pub async fn ensure_session_title(&self, session_id: Uuid) {
        let needs_title = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_id)
                .map(|s| s.persist && s.title.is_none() && !s.history.is_empty())
                .unwrap_or(false)
        };
        if !needs_title {
            return;
        }

        let history = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_id)
                .map(|s| s.history.iter().take(4).cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };

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
                if let Some(content) = choice.message.content.as_deref() {
                    let title = content.trim().to_string();
                    if !title.is_empty() {
                        let mut sessions = self.sessions.lock().await;
                        if let Some(s) = sessions.get_mut(&session_id) {
                            s.title = Some(title);
                        }
                    }
                }
            }
        }
    }

    /// Get display metadata for an in-memory session (for preambles).
    pub async fn session_display(&self, session_id: Uuid) -> Option<(Option<String>, Option<String>)> {
        let sessions = self.sessions.lock().await;
        sessions.get(&session_id)
            .map(|s| (s.ssh_target_name.clone(), s.title.clone()))
    }

    /// Check whether a session is currently loaded in memory.
    pub async fn has_session(&self, session_id: Uuid) -> bool {
        self.sessions.lock().await.contains_key(&session_id)
    }

    pub async fn close_session(&self, session_id: Uuid) {
        // Cancel any in-progress loop, persist state, then unload
        self.cancel_chat(session_id).await;
        self.persist_session(session_id).await;
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

    /// Set the permission mode for a session. SSH sessions cannot use
    /// Yolo — silently downgraded. Persisted immediately if durable.
    pub async fn set_session_mode(&self, session_id: Uuid, mode: PermissionMode) {
        let effective = {
            let mut sessions = self.sessions.lock().await;
            if let Some(s) = sessions.get_mut(&session_id) {
                let effective = if s.ssh.is_some() && mode == PermissionMode::Yolo {
                    tracing::warn!("Yolo rejected for SSH session {session_id} — using default");
                    PermissionMode::Default
                } else {
                    mode
                };
                s.permission_mode = effective;
                effective
            } else {
                return;
            }
        };
        // No need to re-read — we know it might be persistable
        self.persist_session(session_id).await;
        let _ = effective;
    }

    pub async fn session_model(&self, session_id: Uuid) -> String {
        self.sessions
            .lock()
            .await
            .get(&session_id)
            .map(|s| s.model.clone())
            .unwrap_or_else(|| self.models.default_model.clone())
    }

    /// The full agent loop for one user message. Routes the query, then
    /// either runs a direct tool loop or a planner→executor→reviewer pipeline.
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
        let (username, cwd, _is_initialized, session_model, session_kind, ssh_executor) = {
            let mut sessions = self.sessions.lock().await;
            let Some(s) = sessions.get_mut(&session_id) else {
                emit(AgentEvent::Error("session not found".into()));
                self.cancel_tokens.lock().await.remove(&session_id);
                return;
            };
            s.history.push(ChatMessage::user(content));
            (
                s.username.clone(), s.cwd.clone(), s.initialized,
                s.model.clone(), s.kind, s.ssh.clone(),
            )
        };

        // Route the query — heuristics only (skip local model call for speed)
        let route = if content.starts_with('/') || content.starts_with(':') {
            Route::Direct { model: session_model }
        } else {
            let complexity = Router::classify(content);
            tracing::info!("Router: {complexity:?} (kind={session_kind:?})");
            Router::route(complexity, &self.models, session_kind)
        };

        let base_system_prompt = self.build_system_prompt(&username, &cwd, session_kind).await;

        // Stats accumulators across all rounds
        let mut stats = StatsAccum::new();

        let (final_content, model_for_stats) = match &route {
            Route::Direct { model } => {
                let mut tool_history = Vec::new();
                let content = self.run_tool_loop(
                    session_id, model, &base_system_prompt, &cancel_token,
                    &scope, ssh_executor.as_ref(), emit, &mut stats, &mut tool_history,
                ).await;
                (content, model.clone())
            }
            Route::Pipeline { planner, executor, reviewer } => {
                self.run_pipeline(
                    session_id, content, &base_system_prompt,
                    planner, executor, reviewer,
                    &cancel_token, &scope, ssh_executor.as_ref(), emit, &mut stats,
                ).await
            }
        };

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
                model_used: Some(model_for_stats.clone()),
            }).await;
        }

        // Persist the session (history + title) after each turn
        self.ensure_session_title(session_id).await;
        self.persist_session(session_id).await;

        let duration_ms = started.elapsed().as_millis() as u64;
        emit(AgentEvent::Done(ChatStatsData {
            model: model_for_stats,
            prompt_tokens: stats.last_prompt_tokens,
            completion_tokens: stats.total_completion,
            duration_ms,
            context_length: stats.last_prompt_tokens,
            prefill_tps: stats.prefill_tps(),
            decode_tps: stats.decode_tps(),
        }));
    }

    /// Run the planner→executor→reviewer pipeline for Complex queries.
    /// Returns (final_content, model_name_for_stats).
    async fn run_pipeline(
        &self,
        session_id: Uuid,
        user_prompt: &str,
        base_system_prompt: &str,
        planner: &str,
        executor: &str,
        reviewer: &str,
        cancel_token: &CancellationToken,
        scope: &Arc<Mutex<PermissionScope>>,
        ssh: Option<&Arc<crate::ssh::SshExecutor>>,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        stats: &mut StatsAccum,
    ) -> (String, String) {
        let mut tool_history = Vec::new();

        // ── 1. Planner: decompose the user prompt into specific instructions ──
        if cancel_token.is_cancelled() {
            return (String::new(), executor.to_string());
        }

        emit(AgentEvent::ToolEvent {
            name: "planner".into(), status: ToolStatus::Started,
            detail: "decomposing prompt".into(),
        });

        let plan = self.call_planner(planner, user_prompt, cancel_token).await;
        if cancel_token.is_cancelled() {
            return (String::new(), executor.to_string());
        }

        emit(AgentEvent::ToolEvent {
            name: "planner".into(), status: ToolStatus::Finished,
            detail: plan.chars().take(200).collect(),
        });

        // Inject the plan into the system prompt for the executor
        let executor_prompt = format!(
            "{base_system_prompt}\n\n## Plan from planner\nThe planner has decomposed the user's request into the following steps. Follow this plan:\n\n{plan}"
        );

        // ── 2. Executor: run the tool loop with the plan ──
        let mut executor_output = self.run_tool_loop(
            session_id, executor, &executor_prompt, cancel_token,
            scope, ssh, emit, stats, &mut tool_history,
        ).await;

        // ── 3. Reviewer: evaluate the executor's output ──
        for revision_round in 0..MAX_REVISION_ROUNDS {
            if cancel_token.is_cancelled() {
                break;
            }

            emit(AgentEvent::ToolEvent {
                name: "reviewer".into(), status: ToolStatus::Started,
                detail: format!("review round {}", revision_round + 1),
            });

            let (approved, feedback) = self.call_reviewer(
                reviewer, user_prompt, &executor_output, cancel_token,
            ).await;

            if cancel_token.is_cancelled() {
                break;
            }

            if approved {
                emit(AgentEvent::ToolEvent {
                    name: "reviewer".into(), status: ToolStatus::Finished,
                    detail: "approved".into(),
                });
                break;
            }

            emit(AgentEvent::ToolEvent {
                name: "reviewer".into(), status: ToolStatus::Finished,
                detail: format!("revision needed: {}", feedback.chars().take(200).collect::<String>()),
            });

            // Inject revision feedback as a user message in session history
            {
                let mut sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get_mut(&session_id) {
                    s.history.push(ChatMessage::assistant(
                        Some(executor_output.clone()), None,
                    ));
                    s.history.push(ChatMessage::user(&format!(
                        "The reviewer has feedback on your work. Please revise:\n\n{feedback}"
                    )));
                }
            }

            // Run executor again with the revision feedback
            executor_output = self.run_tool_loop(
                session_id, executor, &executor_prompt, cancel_token,
                scope, ssh, emit, stats, &mut tool_history,
            ).await;
        }

        (executor_output, executor.to_string())
    }

    /// Call the planner model to decompose a user prompt into a structured plan.
    async fn call_planner(
        &self,
        model: &str,
        user_prompt: &str,
        _cancel_token: &CancellationToken,
    ) -> String {
        let req = ChatRequest {
            model: model.to_string(),
            messages: vec![
                ChatMessage::system(
                    "You are a planning assistant. Given a user's request, decompose it into \
                     specific, actionable steps for a coding agent. Be concise but precise. \
                     Include:\n\
                     - What files to read or modify\n\
                     - What commands to run\n\
                     - What the expected outcome looks like\n\
                     - Potential pitfalls to watch for\n\
                     Output just the plan, no preamble."
                ),
                ChatMessage::user(user_prompt),
            ],
            tools: None,
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(2048),
            temperature: Some(0.3),
        };

        match self.litellm.chat(req).await {
            Ok(resp) => {
                if let Some(choice) = resp.choices.first() {
                    if let Some(content) = choice.message.content.as_deref() {
                        return content.trim().to_string();
                    }
                }
                "No plan generated — proceed directly.".into()
            }
            Err(e) => {
                tracing::warn!("Planner call failed: {e}");
                format!("(planner error: {e} — proceed directly)")
            }
        }
    }

    /// Call the reviewer model to evaluate the executor's output.
    /// Returns (approved, feedback). feedback is empty if approved.
    async fn call_reviewer(
        &self,
        model: &str,
        user_prompt: &str,
        executor_output: &str,
        _cancel_token: &CancellationToken,
    ) -> (bool, String) {
        let req = ChatRequest {
            model: model.to_string(),
            messages: vec![
                ChatMessage::system(
                    "You are a code reviewer. Evaluate whether the assistant's response \
                     correctly and completely addresses the user's request.\n\n\
                     Respond with EXACTLY one of:\n\
                     - APPROVED\n\
                     - REVISE: <specific feedback on what to fix>\n\n\
                     Be strict but fair. Only approve if the work is correct and complete."
                ),
                ChatMessage::user(&format!(
                    "## Original user request\n{user_prompt}\n\n\
                     ## Assistant's response\n{executor_output}"
                )),
            ],
            tools: None,
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(1024),
            temperature: Some(0.2),
        };

        match self.litellm.chat(req).await {
            Ok(resp) => {
                if let Some(choice) = resp.choices.first() {
                    if let Some(content) = choice.message.content.as_deref() {
                        let trimmed = content.trim();
                        if trimmed.to_uppercase().starts_with("APPROVED") {
                            return (true, String::new());
                        }
                        if let Some(feedback) = trimmed.strip_prefix("REVISE:") {
                            return (false, feedback.trim().to_string());
                        }
                        // Unexpected format — treat as revision request
                        return (false, trimmed.to_string());
                    }
                }
                (true, String::new())
            }
            Err(e) => {
                tracing::warn!("Reviewer call failed: {e}");
                (true, String::new())
            }
        }
    }

    /// Run the streaming tool-use loop. Sends messages to the model, streams
    /// deltas to the client, executes tool calls, and returns the final
    /// content when the model produces a non-tool-call response.
    async fn run_tool_loop(
        &self,
        session_id: Uuid,
        model: &str,
        system_prompt: &str,
        cancel_token: &CancellationToken,
        scope: &Arc<Mutex<PermissionScope>>,
        ssh: Option<&Arc<crate::ssh::SshExecutor>>,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        stats: &mut StatsAccum,
        tool_call_history: &mut Vec<String>,
    ) -> String {
        let mut final_content = String::new();

        for round in 0..MAX_TOOL_ROUNDS {
            if cancel_token.is_cancelled() {
                emit(AgentEvent::Error("cancelled by user".into()));
                break;
            }

            let mut messages = vec![ChatMessage::system(system_prompt.to_string())];
            {
                let sessions = self.sessions.lock().await;
                if let Some(s) = sessions.get(&session_id) {
                    let start = s.history.len().saturating_sub(MAX_HISTORY);
                    messages.extend(s.history[start..].iter().cloned());
                }
            }

            let tools = self.tools.lock().await.clone();
            let req = ChatRequest {
                model: model.to_string(),
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
                    let stream_handle = tokio::spawn(async move {
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
                    // Always abort the stream task — either it finished naturally
                    // (handle is already done) or we broke out due to cancel/error.
                    stream_handle.abort();

                    if stream_result.is_some() {
                        let round_end = Instant::now();
                        match stream_first_delta {
                            Some(t_first) => {
                                stats.total_prefill_ms += t_first.duration_since(stream_round_start).as_millis() as u64;
                                stats.total_decode_ms += round_end.duration_since(t_first).as_millis() as u64;
                            }
                            None => {
                                stats.total_prefill_ms += round_end.duration_since(stream_round_start).as_millis() as u64;
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
                stats.total_completion += u.completion_tokens;
                stats.total_prefill_tokens += u.prompt_tokens;
                stats.last_prompt_tokens = u.prompt_tokens;
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

                let output = self.execute_tool(session_id, name, args_str, scope.clone(), ssh, emit).await;
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

        final_content
    }

    /// Build system prompt with personality, user memories, code rules, and CWD detection.
    async fn build_system_prompt(&self, username: &str, cwd: &str, kind: SessionKind) -> String {
        let personality = self.memory.get_personality().await.unwrap_or_default();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();

        // User-specific memories (always included)
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

        // Skills (always included — they're useful in any context)
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

        match kind {
            SessionKind::Quick => {
                // Minimal prompt for Signal quick questions / research.
                // No coding rules, no project context, no filesystem tools.
                format!(
                    "{personality}

You are renco, an agentic assistant running on temple harness.
You are talking to {username} right now.
Current date: {now}{user_section}{skills_section}

## Available tools
You have tools for web search/fetch, NixOS queries, arXiv paper search,
library docs (context7), and persistent memory (save_memory). Use them
when they help. Use `save_memory` to remember facts about the user,
their preferences, or anything worth recalling in future sessions.

## Guidelines
- Be direct and concise. No filler.
- For factual questions, use web_search to get current information.
- For library/framework questions, use context7.
- For NixOS questions, use the nixos tool.
- For research questions, use arxiv search.
"
                )
            }
            SessionKind::Coding => {
                // Full prompt for coding sessions (TUI/SSH with filesystem access).
                let project_section = detect_project_context(cwd).await;
                format!(
                    "{personality}

You are renco, an agentic coding assistant running on temple harness.
You are talking to {username} right now.
Current date: {now}{user_section}{skills_section}

## Available tools
You have tools for filesystem access, shell commands, persistent memory, web
search/fetch, NixOS queries, arXiv, and library docs (context7). Use them when
they help. Use `save_memory` to remember facts about the user, their
preferences, ongoing projects, or anything worth recalling in future sessions.

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
- The working directory is the user's project root; relative paths resolve there.
"
                )
            }
            SessionKind::System => {
                // System prompt for cron jobs — operates on oracle's local FS.
                format!(
                    "{personality}

You are renco, running a scheduled maintenance task on temple harness.
Current date: {now}

## Available tools
You have filesystem access, shell commands, and persistent memory.
"
                )
            }
        }
    }

    async fn execute_tool(
        &self,
        session_id: Uuid,
        name: &str,
        args_json: &str,
        scope: Arc<Mutex<PermissionScope>>,
        ssh: Option<&Arc<crate::ssh::SshExecutor>>,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<String, String> {
        let args = LiteLLM::recover_tool_call(args_json)
            .ok_or_else(|| format!("cannot parse arguments for {name}"))?;

        match name {
            "read_file" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                if let Some(ssh) = ssh {
                    let resolved = ssh.resolve_path(path).await?;
                    self.check_ssh_perm(session_id, &resolved, AccessKind::Read, ssh, emit).await?;
                    ssh.read_file(&resolved).await
                } else {
                    let resolved = {
                        let s = scope.lock().await;
                        s.resolve(std::path::Path::new(path))
                    };
                    self.check_perm(session_id, &resolved, AccessKind::Read, scope.clone(), emit).await?;
                    tokio::fs::read_to_string(&resolved)
                        .await
                        .map_err(|e| format!("read {}: {e}", resolved.display()))
                }
            }

            "write_file" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                let content = args["content"].as_str().ok_or("missing content")?;
                if let Some(ssh) = ssh {
                    let resolved = ssh.resolve_path(path).await?;
                    self.check_ssh_perm(session_id, &resolved, AccessKind::Write, ssh, emit).await?;
                    ssh.write_file(&resolved, content).await
                } else {
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
            }

            "list_dir" => {
                let path = args["path"].as_str().unwrap_or(".");
                if let Some(ssh) = ssh {
                    let resolved = ssh.resolve_path(path).await?;
                    self.check_ssh_perm(session_id, &resolved, AccessKind::ReadDir, ssh, emit).await?;
                    ssh.list_dir(&resolved).await
                } else {
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
            }

            "execute_command" => {
                let command = args["command"].as_str().ok_or("missing command")?;
                if let Some(ssh) = ssh {
                    // SSH sessions: Yolo is never allowed, dangerous commands always prompt
                    if is_dangerous_command(command) {
                        self.check_ssh_perm(session_id, command, AccessKind::Execute, ssh, emit).await?;
                    }
                    if !is_safe_command(command) {
                        self.check_ssh_perm(session_id, command, AccessKind::Execute, ssh, emit).await?;
                    }
                    ssh.execute(command).await
                } else {
                    let cwd = {
                        let s = scope.lock().await;
                        s.cwd()
                    };
                    if is_dangerous_command(command) {
                        self.check_perm(session_id, &cwd, AccessKind::Execute, scope.clone(), emit).await?;
                    }
                    if !is_safe_command(command) {
                        self.check_perm(session_id, &cwd, AccessKind::Execute, scope.clone(), emit).await?;
                    }
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
            }

            "save_memory" => {
                let key = args["key"].as_str().ok_or("missing key")?;
                let value = args["value"].as_str().ok_or("missing value")?;
                let username = {
                    let sessions = self.sessions.lock().await;
                    sessions.get(&session_id)
                        .map(|s| s.username.clone())
                        .unwrap_or_else(|| "global".into())
                };
                self.memory.set_memory(key, value, &username).await
                    .map_err(|e| format!("save_memory: {e}"))?;
                Ok(format!("Saved memory: {key} = {value}"))
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
            s.check_access(path, access).await
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

    /// Check permissions for SSH-executed tools. Always prompts for
    /// paths outside $HOME and /tmp. Yolo is never allowed.
    async fn check_ssh_perm(
        &self,
        session_id: Uuid,
        target: &str,
        access: AccessKind,
        ssh: &crate::ssh::SshExecutor,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<(), String> {
        // Get the remote home dir to check against
        let home = ssh.home_dir().await.unwrap_or_default();
        let allowed = ssh.is_path_allowed(target, &home);

        if allowed {
            return Ok(());
        }

        // Not in allowed dirs — prompt the user
        let request_id = Uuid::new_v4();
        emit(AgentEvent::PermissionNeeded(PermissionRequest {
            request_id,
            session_id,
            path: format!("{} @ {}", target, ssh.target_name()),
            access,
        }));

        let rx = self.permissions.ask(request_id).await;
        match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(format!("permission denied: {target}")),
            _ => Err("permission request timed out".into()),
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
            model: self.models.default_model.clone(),
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
        ToolDefinition {
            type_field: "function".into(),
            function: ToolFunctionDef {
                name: "save_memory".into(),
                description: "Save a key-value memory that persists across sessions. Use this to remember facts about the user, their preferences, ongoing projects, or anything worth recalling later.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": {"type": "string", "description": "Short identifier (e.g. 'name', 'preferred_editor', 'project_x')."},
                        "value": {"type": "string", "description": "The value to remember."},
                    },
                    "required": ["key", "value"],
                }),
            },
        },
    ]
}

/// Detect project type from CWD contents and return language-specific context.
async fn detect_project_context(cwd: &str) -> String {
    let cwd_path = std::path::Path::new(cwd);
    let mut detected: Vec<String> = Vec::new();

    // Check for Rust
    if tokio::fs::try_exists(cwd_path.join("Cargo.toml")).await.unwrap_or(false) {
        detected.push("Rust project detected (Cargo.toml found). Follow Rust conventions.".into());
    }

    // Check for Nix flake
    if tokio::fs::try_exists(cwd_path.join("flake.nix")).await.unwrap_or(false) {
        detected.push("Nix flake detected. Use nix run/nix shell/nix build. Never nix-shell.".into());
    }

    // Check for ROOT/C++
    let has_cpp = tokio::fs::try_exists(cwd_path.join("CMakeLists.txt")).await.unwrap_or(false);
    let has_cpp_ext = if !has_cpp {
        let mut entries = match tokio::fs::read_dir(cwd_path).await {
            Ok(e) => Some(e),
            Err(_) => None,
        };
        let mut found = false;
        if let Some(entries) = entries.as_mut() {
            while let Ok(Some(e)) = entries.next_entry().await {
                let n = e.file_name().to_string_lossy().to_string();
                if n.ends_with(".C") || n.ends_with(".cpp") || n.ends_with(".cxx") {
                    found = true;
                    break;
                }
            }
        }
        found
    } else {
        true
    };
    if has_cpp_ext {
        detected.push("ROOT/C++ project detected. Use ROOT types (Int_t, Double_t, etc.). No modern C++ (no auto, no smart pointers, no lambdas).".into());
    }

    // Check for Python
    let has_py = tokio::fs::try_exists(cwd_path.join("pyproject.toml")).await.unwrap_or(false)
        || tokio::fs::try_exists(cwd_path.join("setup.py")).await.unwrap_or(false);
    let has_py_ext = if !has_py {
        let mut entries = match tokio::fs::read_dir(cwd_path).await {
            Ok(e) => Some(e),
            Err(_) => None,
        };
        let mut found = false;
        if let Some(entries) = entries.as_mut() {
            while let Ok(Some(e)) = entries.next_entry().await {
                if e.file_name().to_string_lossy().ends_with(".py") {
                    found = true;
                    break;
                }
            }
        }
        found
    } else {
        true
    };
    if has_py_ext {
        detected.push("Python project detected.".into());
    }

    // Try to read project name from Cargo.toml
    if let Ok(content) = tokio::fs::read_to_string(cwd_path.join("Cargo.toml")).await {
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

/// Read-only commands that are safe to run without prompting.
const SAFE_COMMANDS: &[&str] = &[
    "ls", "cat", "head", "tail", "grep", "rg", "find", "fd", "tree",
    "pwd", "whoami", "hostname", "uname", "date", "uptime",
    "wc", "sort", "uniq", "diff", "cmp", "file", "stat", "du", "df",
    "git", "cargo", "rustc", "nix", "nixos-version",
    "python", "python3", "pip", "pip3",
    "echo", "printf", "which", "type", "env", "printenv",
    "test", "true", "false",
    "head", "less", "more",
    "man", "info",
    "ps", "top", "htop", "free", "lspci", "lsusb", "lsblk",
];

/// Patterns that indicate a dangerous command — always prompt.
const DANGEROUS_PATTERNS: &[&str] = &[
    "rm -rf", "rm -fr", "rm -r /", "rm -rf /",
    "mkfs", "dd if=", "dd of=/dev/",
    "shutdown", "reboot", "halt", "poweroff",
    ":(){", "fork bomb",
    "> /dev/sd", "> /dev/nvme", "> /dev/mmc",
    "chmod -R 777", "chmod 777",
    "systemctl stop", "systemctl disable",
    "kill -9", "killall",
    "pkill", "kill -KILL",
    "iptables -F", "ip6tables -F",
    "mount", "umount",
    "fdisk", "parted", "wipefs",
    "shred", "scrub",
    "git push --force", "git push -f",
    "nix-collect-garbage -d",
    "chmod", "chown",
    "visudo",
    "passwd",
    "useradd", "userdel", "usermod",
    "groupadd", "groupdel",
];

/// Returns true if the command is safe to run without a permission prompt.
fn is_safe_command(command: &str) -> bool {
    let trimmed = command.trim();
    // Get the first word (handle pipes/redirects by splitting on them)
    let first_word = trimmed
        .split(|c: char| c.is_whitespace() || c == '|' || c == ';' || c == '&')
        .next()
        .unwrap_or("")
        .trim();
    SAFE_COMMANDS.iter().any(|&safe| first_word == safe)
}

/// Returns true if the command matches a dangerous pattern and should
/// always prompt, even in Yolo mode.
fn is_dangerous_command(command: &str) -> bool {
    let lower = command.to_lowercase();
    DANGEROUS_PATTERNS.iter().any(|&pat| lower.contains(pat))
}
