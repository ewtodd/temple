use crate::agent::Agent;
use crate::config::Config;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WEB_HTML: &str = include_str!("web.html");

/// Start a tiny HTTP server serving the web client. Binds to 127.0.0.1:8080
/// — Caddy reverse-proxies public traffic to this.
pub async fn serve(
    _agent: Arc<Agent>,
    _config: Arc<Config>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!("Web client available at http://0.0.0.0:8080");

    loop {
        let (mut stream, _peer) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            if let Ok(n) = stream.read(&mut buf).await {
                if n == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buf[..n]);
                let response = if request.starts_with("GET /") {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        WEB_HTML.len(),
                        WEB_HTML
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
    }
}
