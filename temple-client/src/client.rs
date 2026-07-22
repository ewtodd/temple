use futures_util::{SinkExt, StreamExt};
use temple_protocol::*;
use tokio::sync::mpsc;
/// WebSocket client — connects to temple-server, returns channels.
use tokio_tungstenite::{
    connect_async, connect_async_tls_with_config, tungstenite::client::IntoClientRequest, Connector,
};

pub struct TempleConnection {
    pub write: mpsc::UnboundedSender<ClientMessage>,
    pub incoming: mpsc::UnboundedReceiver<ServerMessage>,
}

pub async fn connect(
    server_addr: &str,
    token: &Option<String>,
    session_open: SessionOpen,
    force_tls: bool,
) -> Result<(TempleConnection, tokio::task::JoinHandle<()>), String> {
    // Build the WebSocket URL — detect TLS if the server addr starts with
    // https://, or when the user passed --tls.
    let (url, use_tls) = if server_addr.starts_with("https://") {
        let host = server_addr.trim_start_matches("https://");
        (format!("wss://{host}"), true)
    } else if server_addr.starts_with("http://") {
        let host = server_addr.trim_start_matches("http://");
        (format!("ws://{host}"), false)
    } else if force_tls {
        (format!("wss://{server_addr}"), true)
    } else {
        (format!("ws://{server_addr}"), false)
    };

    // Plaintext is fine for loopback and trusted LAN links, but the user
    // should know the bearer token and tool traffic are unencrypted — a
    // warning beats a silent cleartext RCE channel.
    if !use_tls {
        let host = server_addr
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let host_only = host.split([':', '/']).next().unwrap_or(host);
        let is_local = matches!(host_only, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
            || host_only.starts_with("10.")
            || host_only.starts_with("192.168.")
            || host_only.starts_with("172.16.")
            || host_only.ends_with(".local")
            || host_only.ends_with(".lan");
        if !is_local {
            eprintln!(
                "warning: connecting to {host_only} over PLAINTEXT ws:// — the auth token and tool traffic are unencrypted. Use https:// or --tls."
            );
        }
    }

    let mut request = url
        .into_client_request()
        .map_err(|e| format!("invalid request: {e}"))?;

    // Add Authorization header if token is set
    if let Some(t) = token {
        let header_value = format!("Bearer {t}");
        request.headers_mut().insert(
            "Authorization",
            header_value
                .parse()
                .map_err(|e| format!("header parse: {e}"))?,
        );
    }

    // Use TLS connector for wss:// connections
    let (ws_stream, _) = if use_tls {
        let connector = Connector::NativeTls(
            native_tls::TlsConnector::new().map_err(|e| format!("tls connector: {e}"))?,
        );
        connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .map_err(|e| format!("connect failed: {e}"))?
    } else {
        connect_async(request)
            .await
            .map_err(|e| format!("connect failed: {e}"))?
    };

    // Split — no shared mutex, no deadlock.
    let (mut ws_write, mut ws_read) = ws_stream.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let handle = tokio::spawn(async move {
        use tokio_tungstenite::tungstenite::Message;
        while let Some(msg) = rx.recv().await {
            let json = serde_json::to_string(&msg).unwrap_or_default();
            if ws_write.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                    if let Ok(m) = serde_json::from_str::<ServerMessage>(&text) {
                        if incoming_tx.send(m).is_err() {
                            break;
                        }
                    }
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    let _ = tx.send(ClientMessage::OpenSession(session_open));

    Ok((
        TempleConnection {
            write: tx,
            incoming: incoming_rx,
        },
        handle,
    ))
}
