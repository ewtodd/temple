/// WebSocket client — connects to temple-server, returns channels.
use tokio_tungstenite::connect_async;
use futures_util::{SinkExt, StreamExt};
use temple_protocol::*;
use tokio::sync::mpsc;

pub struct TempleConnection {
    pub write: mpsc::UnboundedSender<ClientMessage>,
    pub incoming: mpsc::UnboundedReceiver<ServerMessage>,
}

pub async fn connect(
    server_addr: &str,
    session_open: SessionOpen,
) -> Result<(TempleConnection, tokio::task::JoinHandle<()>), String> {
    let url = format!("ws://{server_addr}");
    let (ws_stream, _) = connect_async(&url)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;

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

    Ok((TempleConnection { write: tx, incoming: incoming_rx }, handle))
}
