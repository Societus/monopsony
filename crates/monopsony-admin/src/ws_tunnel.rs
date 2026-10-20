//! WebSocket → TCP relay for tunneling gRPC mTLS and pairing through
//! reverse proxies (Cloudflare Tunnels, nginx, Caddy, etc.).
//!
//! Two endpoints share the same dumb-pipe relay logic:
//!
//! | Endpoint     | Targets             | Purpose                              |
//! |--------------|----------------------|--------------------------------------|
//! | `/ws/tunnel` | gRPC listener (50051)| Control plane: heartbeats, inventory |
//! | `/ws/pair`   | Pairing port (50052) | Initial client pairing exchange      |
//!
//! Both carry opaque binary frames — the reverse proxy only sees an
//! encrypted WebSocket stream. For `/ws/tunnel`, the client runs its full
//! mTLS handshake (Monopsony CA + client cert) inside the WebSocket. For
//! `/ws/pair`, the plaintext length-prefixed protobuf pairing exchange is
//! relayed to the local pairing listener.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// Size of the read buffer when copying TCP → WebSocket.
const TCP_BUFFER_SIZE: usize = 8192;

/// How often to send WebSocket pings to keep the connection alive through
/// proxy idle timeouts (Cloudflare's default is 100 seconds).
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Default port for the gRPC listener (the `/ws/tunnel` target).
const GRPC_DEFAULT_PORT: u16 = 50051;

/// Default port for the pairing listener (the `/ws/pair` target).
const PAIRING_DEFAULT_PORT: u16 = 50052;

/// Maximum concurrent `/ws/pair` WebSocket relay connections.
/// Prevents connection-exhaustion attacks on the pairing port.
const MAX_WS_PAIR_CONNECTIONS: usize = 10;

/// Active `/ws/pair` WebSocket relay connections counter.
static WS_PAIR_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// GET /ws/tunnel — WebSocket upgrade handler for the gRPC control plane.
///
/// Accepts a WebSocket connection and proxies it to the local gRPC server.
/// The client runs its full mTLS handshake inside this WebSocket — the
/// server's gRPC stack handles TLS, client cert verification, and all RPCs
/// exactly as if the client connected via raw TCP.
pub async fn handle_ws_tunnel(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|socket| tunnel_connection(socket, GRPC_DEFAULT_PORT, "gRPC tunnel"))
}

/// GET /ws/pair — WebSocket upgrade handler for the pairing exchange.
///
/// Relays the plaintext pairing exchange (length-prefixed protobuf) to the
/// local pairing listener on port 50052. The pairing listener only accepts
/// one connection per session and validates the pairing code before issuing
/// certificates — this endpoint is a dumb pipe that doesn't interpret the
/// frames.
///
/// Not behind auth middleware: the pairing code itself is the credential.
/// Concurrency capped to prevent connection-exhaustion attacks.
pub async fn handle_ws_pair(ws: WebSocketUpgrade) -> impl IntoResponse {
    let current = WS_PAIR_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
    if current >= MAX_WS_PAIR_CONNECTIONS {
        WS_PAIR_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
        return axum::response::Response::builder()
            .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
            .header("Content-Type", "text/plain")
            .body(axum::body::Body::from("pairing relay at capacity"))
            .unwrap();
    }

    ws.on_upgrade(move |socket| {
        async move {
            tunnel_connection(socket, PAIRING_DEFAULT_PORT, "pairing tunnel").await;
            WS_PAIR_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
        }
    })
}

/// Handle a single WebSocket tunnel connection, relaying bidirectionally
/// to `127.0.0.1:{target_port}`.
///
/// `label` is used in log messages to distinguish tunnel vs pairing relay.
async fn tunnel_connection(ws: WebSocket, target_port: u16, label: &'static str) {
    let tcp_addr = format!("127.0.0.1:{}", target_port);

    let tcp = match TcpStream::connect(&tcp_addr).await {
        Ok(stream) => {
            debug!("WS {}: connected to {}", label, tcp_addr);
            stream
        }
        Err(e) => {
            warn!("WS {}: failed to connect to {}: {}", label, tcp_addr, e);
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
            debug!("WS {}: client→server direction closed: {:?}", label, result);
        }
        result = tcp_to_ws => {
            debug!("WS {}: server→client direction closed: {:?}", label, result);
        }
    }

    info!("WS {}: connection closed", label);
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
