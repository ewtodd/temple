use std::path::PathBuf;
use std::time::Duration;
use temple_protocol::*;

use crate::client;
use crate::tools::execute_local_tool;

/// Run the temple client in headless daemon mode. Connects to the server,
/// opens a session, and executes tool requests locally. Reconnects on
/// disconnect with exponential backoff.
///
/// `pubkey` is an optional SSH ed25519 public key string, used for
/// daemon authentication. When set, the server verifies it is authorized
/// for the claimed username.
pub async fn run(
    server: String,
    cwd: PathBuf,
    client_id: String,
    token: Option<String>,
    force_tls: bool,
    mode: PermissionMode,
    pubkey: Option<String>,
) -> Result<(), String> {
    let cwd_str = cwd.to_string_lossy().to_string();
    let hostname = whoami::fallible::hostname().unwrap_or_else(|_| "unknown".into());
    let username = whoami::username();

    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);

    loop {
        let sess = SessionOpen {
            client_id: client_id.clone(),
            cwd: std::fs::canonicalize(&cwd)
                .unwrap_or_else(|_| cwd.clone())
                .to_string_lossy()
                .into(),
            hostname: hostname.clone(),
            username: username.clone(),
            daemon_pubkey: pubkey.clone(),
        };

        let (conn, _) = match client::connect(&server, &token, sess, force_tls).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("temple-daemon: connect failed: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        // Successfully connected — reset backoff
        backoff = Duration::from_secs(1);

        let tx = conn.write;
        let mut incoming = conn.incoming;

        eprintln!(
            "temple-daemon: connected to {server}, cwd={cwd_str}, mode={mode:?}{}",
            if pubkey.is_some() {
                ""
            } else {
                " (no identity — running without daemon auth)"
            }
        );

        // Main message loop
        while let Some(msg) = incoming.recv().await {
            let ServerMessage::ToolRequest {
                request_id,
                session_id,
                name,
                args_json,
            } = msg
            else {
                // Non-tool messages are ignored in daemon mode
                continue;
            };

            // In YOLO mode, execute immediately. In other modes, apply
            // client-side consent: allow only tools within the daemon's cwd.
            let result = if mode == PermissionMode::Yolo {
                execute_local_tool(&name, &args_json, &cwd_str).await
            } else {
                // Client-side consent for non-YOLO daemon
                let needs_consent = match name.as_str() {
                    "write_file" | "edit_file" => {
                        let args: serde_json::Value =
                            serde_json::from_str(&args_json).unwrap_or_default();
                        let path = args["path"].as_str().unwrap_or("");
                        match crate::tools::resolve_tool_path(path, &cwd_str) {
                            Ok(resolved) => !resolved.starts_with(&cwd_str),
                            Err(_) => true,
                        }
                    }
                    "execute_command" => {
                        let args: serde_json::Value =
                            serde_json::from_str(&args_json).unwrap_or_default();
                        let cmd = args["command"].as_str().unwrap_or("");
                        !temple_protocol::command::is_safe_command(cmd)
                    }
                    _ => false,
                };

                if needs_consent {
                    format!("Error: {name} denied — daemon is not in YOLO mode")
                } else {
                    execute_local_tool(&name, &args_json, &cwd_str).await
                }
            };

            let _ = tx.send(ClientMessage::ToolResult {
                request_id,
                session_id,
                result,
            });
        }

        eprintln!(
            "temple-daemon: disconnected, reconnecting in {:?}...",
            backoff
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}
