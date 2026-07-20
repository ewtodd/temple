use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;
use futures_util::{SinkExt, StreamExt};
use temple_protocol::*;
use uuid::Uuid;

use crate::agent::{Agent, AgentEvent};
use crate::permissions::PermissionScope;
use crate::config::Config;

pub async fn run_server(
    agent: Arc<Agent>,
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
        let config = config.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, agent, config).await {
                tracing::error!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    agent: Arc<Agent>,
    config: Arc<Config>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws = accept_async(stream).await?;
    // Split into independent read/write halves — no shared mutex, no deadlock.
    let (mut ws_write, mut ws_read) = ws.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

    let mut session_id = Uuid::nil();
    let mut permissions: Option<Arc<Mutex<PermissionScope>>> = None;

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
                let scope = PermissionScope::new(
                    std::path::Path::new(&open.cwd),
                    PermissionMode::Default,
                    &config.allowed_dirs,
                )
                .await;
                permissions = Some(Arc::new(Mutex::new(scope)));
                agent.open_session(session_id, &open.username, &open.cwd).await;
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

            ClientMessage::CloseSession { session_id: sid } => {
                agent.close_session(sid).await;
                let _ = tx.send(ServerMessage::SessionClosed { session_id: sid });
                break;
            }

            ClientMessage::ChatInput { session_id: sid, content } => {
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
                            AgentEvent::ToolEvent { name, status, detail } => {
                                ServerMessage::ToolEvent {
                                    session_id: sid,
                                    name,
                                    status,
                                    detail,
                                }
                            }
                            AgentEvent::PermissionNeeded(req) => {
                                ServerMessage::PermissionRequired(req)
                            }
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
                    agent
                        .process_chat(sid, &content, scope, &emit)
                        .await;
                });
            }

            ClientMessage::PermissionReply { request_id, granted, .. } => {
                agent.permissions.resolve(request_id, granted).await;
            }

            ClientMessage::ListModels => {
                match agent.litellm.list_models().await {
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
                }
            }

            ClientMessage::SetModel { session_id: sid, model } => {
                agent.set_session_model(sid, &model).await;
                let _ = tx.send(ServerMessage::ModelChanged { session_id: sid, model });
            }

            ClientMessage::SetPermissionMode { session_id: sid, mode } => {
                if let Some(ref p) = permissions {
                    p.lock().await.set_mode(mode);
                }
                let _ = tx.send(ServerMessage::ModeChanged { session_id: sid, mode });
            }

            ClientMessage::CancelChat { session_id: sid } => {
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
