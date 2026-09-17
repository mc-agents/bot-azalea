use std::rc::Rc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::warn;

use crate::bot::Bot;

/// `/healthz` answers while the process is alive; `/readyz` only once the server has accepted our
/// hello, because that probe is the only thing outside the protocol that knows whether a bot is
/// linked. Two routes and a status line do not need an HTTP framework.
pub async fn serve(bot: Rc<Bot>) {
    let listener = match TcpListener::bind(("0.0.0.0", bot.config.health_port)).await {
        Ok(listener) => listener,
        Err(failure) => {
            warn!("no health endpoint on port {}: {failure}", bot.config.health_port);
            return;
        }
    };

    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let mut request = [0u8; 256];
        let read = stream.read(&mut request).await.unwrap_or(0);
        let line = String::from_utf8_lossy(&request[..read]);

        let ok = if line.starts_with("GET /readyz") {
            bot.linked()
        } else {
            line.starts_with("GET /healthz")
        };
        let response = if ok {
            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
        } else {
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n"
        };
        let _ = stream.write_all(response.as_bytes()).await;
    }
}
