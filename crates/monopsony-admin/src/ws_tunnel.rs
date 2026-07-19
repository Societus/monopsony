//! WebSocket → TCP relay for tunneling gRPC mTLS through reverse proxies.
//!
//! Accepts a WebSocket upgrade at `/ws/tunnel` and proxies binary frames
//! bidirectionally to a local TCP port (default: the gRPC listener at 50051).
//! The inner TLS handshake, client certificates, and gRPC HTTP/2 framing all
//! pass through as opaque bytes — the reverse proxy (Cloudflare, nginx, etc.)
//! only sees an encrypted WebSocket stream.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// Size of the read buffer when copying TCP → WebSocket.
const TCP_BUFFER_SIZE: usize = 8192;

/// How often to send WebSocket pings to keep the connection alive through
/// proxy idle timeouts (Cloudflare's default is 100 seconds).
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// GET /ws/tunnel — WebSocket upgrade handler.
///
/// Accepts a WebSocket connection and proxies it to the local gRPC server.
/// The client runs its full mTLS handshake inside this WebSocket — the
/// server's gRPC stack handles TLS, client cert verification, and all RPCs
/// exactly as if the client connected via raw TCP.
pub async fn handle_ws_tunnel(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(tunnel_connection)
}

/// Handle a single WebSocket tunnel connection.
async fn tunnel_connection(ws: WebSocket) {
    let grpc_addr = std::env::var("MONOPSONY_GRPC_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50051".to_string());

    let tcp = match TcpStream::connect(&grpc_addr).await {
        Ok(stream) => {
            debug!("WS tunnel: connected to gRPC at {}", grpc_addr);
            stream
        }
        Err(e) => {
            warn!("WS tunnel: failed to connect to gRPC at {}: {}", grpc_addr, e);
            let _ = ws.close().await;
            return;
        }
    };

    let (tcp_rx, tcp_tx) = tcp.into_split();
    let (ws_tx, ws_rx) = ws.split();

    // Spawn the two relay directions
    let ws_to_tcp = tokio::spawn(relay_ws_to_tcp(ws_rx, tcp_tx));
    let tcp_to_ws = tokio::spawn(relay_tcp_to_ws(tcp_rx, ws_tx));

    // Wait for either direction to finish, then abort the other
    tokio::select! {
        result = ws_to_tcp => {
            debug!("WS tunnel: client→server direction closed: {:?}", result);
        }
        result = tcp_to_ws => {
            debug!("WS tunnel: server→client direction closed: {:?}", result);
        }
    }

    info!("WS tunnel: connection closed");
}

/// Relay WebSocket binary frames → TCP socket.
async fn relay_ws_to_tcp(
    mut ws_rx: futures_util::stream::SplitStream<WebSocket>,
    mut tcp_tx: tokio::net::tcp::OwnedWriteHalf,
) {
    while let Some(msg_result) = ws_rx.next().await {
        match msg_result {
            Ok(Message::Binary(data)) => {
                if let Err(e) = tcp_tx.write_all(&data).await {
                    warn!("WS tunnel: TCP write error (client→server): {}", e);
                    break;
                }
            }
            Ok(Message::Text(_)) => {
                warn!("WS tunnel: received text frame, expected binary. Ignoring.");
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                // Handled automatically by axum; ignore
            }
            Ok(Message::Close(_)) => {
                debug!("WS tunnel: client sent close frame");
                break;
            }
            Err(e) => {
                warn!("WS tunnel: WebSocket receive error (client→server): {}", e);
                break;
            }
        }
    }
    let _ = tcp_tx.shutdown().await;
}

/// Relay TCP socket → WebSocket binary frames.
async fn relay_tcp_to_ws(
    mut tcp_rx: tokio::net::tcp::OwnedReadHalf,
    mut ws_tx: futures_util::stream::SplitSink<WebSocket, Message>,
) {
    let mut buf = vec![0u8; TCP_BUFFER_SIZE];
    let mut ping_timer = tokio::time::interval(PING_INTERVAL);
    ping_timer.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            result = tcp_rx.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        debug!("WS tunnel: gRPC server closed connection");
                        break;
                    }
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        if let Err(e) = ws_tx.send(Message::Binary(data)).await {
                            warn!("WS tunnel: WebSocket send error (server→client): {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("WS tunnel: TCP read error (server→client): {}", e);
                        break;
                    }
                }
            }
            _ = ping_timer.tick() => {
                if let Err(e) = ws_tx.send(Message::Ping(vec![1])).await {
                    debug!("WS tunnel: failed to send ping: {}", e);
                    break;
                }
            }
        }
    }
    let _ = ws_tx.close().await;
}
