use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use temple_protocol::*;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_hdr_async;
use uuid::Uuid;

use crate::agent::{Agent, AgentEvent};
use crate::auth::check_token;
use crate::config::Config;
use crate::memory::Memory;
use crate::permissions::PermissionScope;

pub async fn run_server(
    agent: Arc<Agent>,
    memory: Arc<Memory>,
    config: Arc<Config>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(&config.listen).await?;
    tracing::info!("Temple server listening on {}", config.listen);

    // Keep-alive: ping the warm model every 30s so llamaswap doesn't unload it
    let keep_alive = agent.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            keep_alive.keep_warm().await;
        }
    });

    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!("New connection from {peer}");

        let agent = agent.clone();
        let memory = memory.clone();
        let config = config.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, agent, memory, config).await {
                tracing::error!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    agent: Arc<Agent>,
    memory: Arc<Memory>,
    config: Arc<Config>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The person who authenticated (from token file) — sessions are
    // owned/listed under this name, not the client's system username.
    let mut auth_owner: Option<String> = None;

    let ws = if let Some(ref token_file) = config.auth_token_file {
        let token_file = token_file.clone();
        // Capture the token from the callback via shared state
        let captured_token = Arc::new(Mutex::new(None::<String>));
        let ct = captured_token.clone();
        let tf = token_file.clone();
        #[allow(clippy::result_large_err)] // tungstenite's callback signature is fixed.
        let callback = move |req: &tokio_tungstenite::tungstenite::handshake::server::Request, response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            if let Some(auth) = req.headers().get("Authorization") {
                let auth_str = auth.to_str().unwrap_or("");
                if let Some(token) = auth_str.strip_prefix("Bearer ") {
                    if check_token(&tf, token).is_ok() {
                        let ct = ct.clone();
                        if let Ok(mut guard) = ct.try_lock() {
                            *guard = Some(token.to_string());
                        }
                        return Ok(response);
                    }
                }
            }
            // Reject with 401
            let mut resp = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::default();
            *resp.status_mut() = http_status_unauthorized();
            Err(resp)
        };
        let ws = match accept_hdr_async(stream, callback).await {
            Ok(ws) => ws,
            Err(_) => {
                tracing::warn!("Rejected unauthorized connection");
                return Ok(());
            }
        };
        // Resolve the person from the captured token
        if let Some(token) = captured_token.lock().await.as_deref() {
            if let Ok(user) = check_token(&token_file, token) {
                auth_owner = Some(user.username);
            }
        }
        ws
    } else {
        // No auth configured — accept all (LAN-only mode)
        tokio_tungstenite::accept_async(stream).await?
    };

    // Split into independent read/write halves — no shared mutex, no deadlock.
    let (mut ws_write, mut ws_read) = ws.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

    let mut session_id = Uuid::nil();
    let mut permissions: Option<Arc<Mutex<PermissionScope>>> = None;
    let mut client_cwd: Option<String> = None;

    // Writer task: forwards channel messages to the socket
    let send_task = tokio::spawn(async move {
        use tokio_tungstenite::tungstenite::Message;
        while let Some(msg) = rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    tracing::error!("serialize: {e}");
                    continue;
                }
            };
            if ws_write.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    });

    // Reader loop
    loop {
        let msg = ws_read.next().await;

        let Some(Ok(text_msg)) = msg else {
            tracing::info!("connection closed");
            break;
        };

        let text = match text_msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };

        let client_msg: ClientMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("invalid client message: {e} — {text}");
                continue;
            }
        };

        match client_msg {
            ClientMessage::OpenSession(open) => {
                session_id = Uuid::new_v4();
                if auth_owner.is_none() {
                    auth_owner = Some(open.username.clone());
                }
                // Auto-create a persisted Coding session (no SSH — tools run
                // client-side via ToolRequest, so every `temple` invocation
                // gets working tools and sessions show up in /sessions).
                match agent
                    .new_persisted_session(
                        auth_owner.as_deref().unwrap_or(&open.username),
                        None,
                        None,
                        Some(&open.username),
                        Some(&open.cwd),
                    )
                    .await
                {
                    Ok(sid) => {
                        session_id = sid;
                    }
                    Err(e) => {
                        tracing::warn!("new_persisted_session failed: {e}");
                        agent
                            .open_session(
                                session_id,
                                auth_owner.as_deref().unwrap_or(&open.username),
                                &open.cwd,
                                crate::router::SessionKind::Interactive,
                                None,
                            )
                            .await;
                    }
                }
                client_cwd = Some(open.cwd.clone());
                let scope = PermissionScope::new(
                    std::path::Path::new(&open.cwd),
                    PermissionMode::Default,
                    &config.allowed_dirs,
                )
                .await;
                permissions = Some(Arc::new(Mutex::new(scope)));
                let info = SessionInfo {
                    session_id,
                    client_id: open.client_id.clone(),
                    cwd: open.cwd,
                    hostname: open.hostname,
                    username: open.username,
                    opened_at: chrono::Utc::now(),
                    permissions: PermissionMode::Default,
                };
                let _ = tx.send(ServerMessage::SessionOpened { session_id, info });
            }

            ClientMessage::ListSessions => {
                let owner = auth_owner.clone().unwrap_or_default();
                match memory.list_sessions(&owner, 10).await {
                    Ok(rows) => {
                        let sessions = rows
                            .into_iter()
                            .map(|r| SessionMeta {
                                id: r.id,
                                title: r.title,
                                ssh_target: r.ssh_target,
                                cwd: r.cwd,
                                mode: r.mode,
                                updated_at: r.updated_at,
                            })
                            .collect();
                        let _ = tx.send(ServerMessage::SessionList { sessions });
                    }
                    Err(e) => {
                        let _ = tx.send(ServerMessage::ChatError {
                            session_id,
                            error: format!("list sessions: {e}"),
                        });
                    }
                }
            }

            ClientMessage::ResumeSession { session_id: sid } => {
                let owner = auth_owner.clone().unwrap_or_default();
                match agent.resume_session(sid, &owner).await {
                    Ok((meta, transcript)) => {
                        session_id = sid;
                        // Scope the permission check to the client's actual
                        // working directory (or the session's persisted cwd
                        // for SSH targets / backward compat).
                        let effective_cwd = client_cwd
                            .as_deref()
                            .filter(|c| !c.is_empty())
                            .or_else(|| {
                                (!meta.cwd.is_empty() && meta.cwd != "/var/lib/temple")
                                    .then(|| meta.cwd.as_str())
                            })
                            .unwrap_or(".");
                        let scope = PermissionScope::new(
                            std::path::Path::new(effective_cwd),
                            PermissionMode::Default,
                            &config.allowed_dirs,
                        )
                        .await;
                        permissions = Some(Arc::new(Mutex::new(scope)));
                        let _ = tx.send(ServerMessage::SessionResumed {
                            session_id: sid,
                            meta,
                            transcript,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(ServerMessage::ChatError {
                            session_id,
                            error: format!("resume: {e}"),
                        });
                    }
                }
            }

            ClientMessage::NewSession {
                ssh_target,
                start_dir,
            } => {
                let owner = auth_owner.clone().unwrap_or_default();
                match agent
                    .new_persisted_session(
                        &owner,
                        ssh_target.as_deref(),
                        start_dir.as_deref(),
                        None,
                        client_cwd.as_deref(),
                    )
                    .await
                {
                    Ok(sid) => {
                        session_id = sid;
                        // For SSH targets the scope is irrelevant (SSH
                        // permission checks run against $HOME on the remote
                        // host). For local sessions, recreate the scope from
                        // the client's actual cwd (not /var/lib/temple).
                        if client_cwd.is_some() {
                            // Client-side session: use the stored client cwd
                            let scope = PermissionScope::new(
                                std::path::Path::new(
                                    client_cwd.as_deref().unwrap_or("."),
                                ),
                                PermissionMode::Default,
                                &config.allowed_dirs,
                            )
                            .await;
                            permissions = Some(Arc::new(Mutex::new(scope)));
                        } else if permissions.is_none() {
                            let cwd = agent
                                .session_cwd(sid)
                                .await
                                .unwrap_or_else(|| ".".into());
                            let scope = PermissionScope::new(
                                std::path::Path::new(&cwd),
                                PermissionMode::Default,
                                &config.allowed_dirs,
                            )
                            .await;
                            permissions = Some(Arc::new(Mutex::new(scope)));
                        }
                        let meta = SessionMeta {
                            id: sid,
                            title: None,
                            ssh_target: ssh_target.clone(),
                            cwd: String::new(),
                            mode: "default".into(),
                            updated_at: String::new(),
                        };
                        let _ = tx.send(ServerMessage::SessionResumed {
                            session_id: sid,
                            meta,
                            transcript: Vec::new(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(ServerMessage::ChatError {
                            session_id,
                            error: format!("new session: {e}"),
                        });
                    }
                }
            }

            ClientMessage::DeleteSession { session_id: sid } => {
                // Ownership check: a client may only delete its own
                // sessions (or any session if admin).
                let owner = auth_owner.clone().unwrap_or_default();
                let is_admin = memory.is_admin_username(&owner).await.unwrap_or(false);
                let session_owner = agent.session_owner(sid).await;
                if !is_admin && session_owner.as_deref() != Some(owner.as_str()) {
                    let _ = tx.send(ServerMessage::ChatError {
                        session_id: sid,
                        error: "session belongs to another user".into(),
                    });
                    continue;
                }
                // Close in-memory session if loaded, then remove from DB.
                agent.close_session(sid).await;
                match memory.delete_session(sid).await {
                    Ok(_) => {
                        let _ = tx.send(ServerMessage::SessionDeleted { session_id: sid });
                    }
                    Err(e) => {
                        let _ = tx.send(ServerMessage::ChatError {
                            session_id,
                            error: format!("delete session: {e}"),
                        });
                    }
                }
            }

            ClientMessage::ClearSessions { account } => {
                // /clear can wipe other users' sessions — admin only
                let owner = auth_owner.clone().unwrap_or_default();
                let is_admin = memory.is_admin_username(&owner).await.unwrap_or(false);
                if !is_admin {
                    let _ = tx.send(ServerMessage::ChatError {
                        session_id,
                        error: "admin only".into(),
                    });
                    continue;
                }
                // Unload in-memory sessions FIRST (without persisting) —
                // otherwise the next turn re-persists them and the deleted
                // rows resurrect.
                let dropped = agent.drop_sessions_for(&account).await;
                match memory.clear_sessions(&account).await {
                    Ok(n) => {
                        let _ = tx.send(ServerMessage::ChatDelta {
                            session_id,
                            delta: format!("deleted {n} sessions for {account} ({dropped} unloaded from memory)\n"),
                            done: true,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(ServerMessage::ChatError {
                            session_id,
                            error: format!("clear sessions: {e}"),
                        });
                    }
                }
            }

            ClientMessage::CloseSession { session_id: sid } => {
                agent.close_session(sid).await;
                let _ = tx.send(ServerMessage::SessionClosed { session_id: sid });
                break;
            }

            ClientMessage::ChatInput {
                session_id: sid,
                content,
            } => {
                // Session ownership: a connection may only drive its own
                // active session. Without this check, any client that
                // learns another session's UUID could inject chat into it —
                // executed with THIS connection's permission scope.
                if sid != session_id {
                    let _ = tx.send(ServerMessage::ChatError {
                        session_id: sid,
                        error: "session id mismatch — open/resume a session first".into(),
                    });
                    continue;
                }
                let Some(scope) = permissions.clone() else {
                    let _ = tx.send(ServerMessage::ChatError {
                        session_id: sid,
                        error: "no session open".into(),
                    });
                    continue;
                };

                // Run agent loop in a task; forward events to this client
                let agent = agent.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let emit = move |ev: AgentEvent| {
                        let msg = match ev {
                            AgentEvent::Delta(d) => ServerMessage::ChatDelta {
                                session_id: sid,
                                delta: d,
                                done: false,
                            },
                            AgentEvent::ToolEvent {
                                name,
                                status,
                                detail,
                            } => ServerMessage::ToolEvent {
                                session_id: sid,
                                name,
                                status,
                                detail,
                            },
                            AgentEvent::PermissionNeeded(req) => {
                                ServerMessage::PermissionRequired(req)
                            }
                            AgentEvent::StreamReset => ServerMessage::ChatReset { session_id: sid },
                            AgentEvent::ToolRequestNeeded {
                                request_id,
                                name,
                                args_json,
                            } => ServerMessage::ToolRequest {
                                request_id,
                                session_id: sid,
                                name,
                                args_json,
                            },
                            AgentEvent::TodoUpdated(items) => ServerMessage::TodoUpdate {
                                session_id: sid,
                                items,
                            },
                            AgentEvent::Done(stats) => {
                                let _ = tx.send(ServerMessage::ChatDelta {
                                    session_id: sid,
                                    delta: String::new(),
                                    done: true,
                                });
                                ServerMessage::ChatStats {
                                    session_id: sid,
                                    model: stats.model,
                                    prompt_tokens: stats.prompt_tokens,
                                    completion_tokens: stats.completion_tokens,
                                    duration_ms: stats.duration_ms,
                                    tokens_per_second: stats.decode_tps,
                                    context_length: stats.context_length,
                                    prefill_tps: stats.prefill_tps,
                                    decode_tps: stats.decode_tps,
                                }
                            }
                            AgentEvent::Error(e) => ServerMessage::ChatError {
                                session_id: sid,
                                error: e,
                            },
                        };
                        let _ = tx.send(msg);
                    };
                    agent.process_chat(sid, &content, scope, &emit, None).await;
                });
            }

            ClientMessage::PermissionReply {
                request_id,
                session_id: sid,
                granted,
            } => {
                // Only resolve prompts belonging to this connection's
                // session — a client must not answer another session's
                // permission prompts.
                if sid == session_id {
                    agent.permissions.resolve(request_id, granted).await;
                }
            }

            ClientMessage::ToolResult {
                request_id,
                session_id: sid,
                result,
            } => {
                if sid == session_id {
                    agent.resolve_tool(request_id, result).await;
                }
            }

            ClientMessage::ListModels => match agent.litellm.list_models().await {
                Ok(models) => {
                    let infos: Vec<ModelInfo> = models
                        .into_iter()
                        .map(|id| ModelInfo {
                            id,
                            provider: "litellm".into(),
                            description: None,
                            capabilities: vec![],
                        })
                        .collect();
                    let _ = tx.send(ServerMessage::ModelList { models: infos });
                }
                Err(e) => {
                    let _ = tx.send(ServerMessage::ChatError {
                        session_id,
                        error: format!("model list failed: {e}"),
                    });
                }
            },

            ClientMessage::SetModel {
                session_id: sid,
                model,
            } => {
                if sid != session_id {
                    continue;
                }
                if model == "auto" {
                    agent.reset_session_model(sid).await;
                    let default = agent.session_model(sid).await;
                    let _ = tx.send(ServerMessage::ModelChanged {
                        session_id: sid,
                        model: default,
                    });
                } else {
                    agent.set_session_model(sid, &model).await;
                    let _ = tx.send(ServerMessage::ModelChanged {
                        session_id: sid,
                        model,
                    });
                }
            }

            ClientMessage::SetPermissionMode {
                session_id: sid,
                mode,
            } => {
                if sid != session_id {
                    continue;
                }
                if let Some(ref p) = permissions {
                    p.lock().await.set_mode(mode);
                }
                agent.set_session_mode(sid, mode).await;
                let _ = tx.send(ServerMessage::ModeChanged {
                    session_id: sid,
                    mode,
                });
            }

            ClientMessage::CancelChat { session_id: sid } => {
                if sid != session_id {
                    continue;
                }
                agent.cancel_chat(sid).await;
                let _ = tx.send(ServerMessage::ChatCancelled { session_id: sid });
            }

            ClientMessage::Ping => {
                let _ = tx.send(ServerMessage::Pong);
            }
        }
    }

    if session_id != Uuid::nil() {
        agent.close_session(session_id).await;
    }
    send_task.abort();
    Ok(())
}

/// Build a 401 Unauthorized HTTP status response.
fn http_status_unauthorized() -> tokio_tungstenite::tungstenite::http::StatusCode {
    tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED
}
