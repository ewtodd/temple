use std::path::PathBuf;
use std::time::Duration;
use temple_protocol::*;

use crate::client;
use crate::tools::execute_local_tool;

/// Run the temple client in headless daemon mode. Connects to the server,
/// opens a session, and executes tool requests locally. Reconnects on
/// disconnect with exponential backoff.
///
/// `pubkey` is an SSH ed25519 public key string (required). The server
/// verifies it is authorized for the claimed owner.
pub async fn run(
    server: String,
    cwd: PathBuf,
    client_id: String,
    pubkey: String,
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
            daemon_pubkey: Some(pubkey.clone()),
        };

        let (conn, _) = match client::connect(&server, &None, sess, false).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("temple-daemon: connect failed: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        backoff = Duration::from_secs(1);

        let tx = conn.write;
        let mut incoming = conn.incoming;

        eprintln!("temple-daemon: connected to {server}, cwd={cwd_str}",);

        // Main message loop
        while let Some(msg) = incoming.recv().await {
            let ServerMessage::ToolRequest {
                request_id,
                session_id,
                name,
                args_json,
            } = msg
            else {
                continue;
            };

            // Execute every tool immediately — permission decisions come
            // from the session, not the daemon.
            let result =
                execute_local_tool(&name, &args_json, &cwd_str, None, request_id, session_id).await;

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
