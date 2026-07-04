//! Client-side proxy that forwards package manager requests to the Monopsony server.
//!
//! Architecture:
//! - Package manager (pacman/apt/apk) sends requests to 127.0.0.1:3129
//! - Client proxy connects TCP to the server's HTTP forward proxy (port 3128)
//! - Client proxy sends the original absolute URI in the request line (forward-proxy semantics)
//! - Server proxy checks cache, fetches from upstream on miss, caches result
//! - Client proxy returns response to package manager
//!
//! IMPORTANT: On any failure (server unreachable, timeout, error), we return 503.
//! The package manager must retry later. We do NOT fall back to direct upstream
//! fetching — that would defeat the caching purpose.

use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

pub struct ProxyConfig {
    /// Server's HTTP forward proxy address (e.g. "10.10.20.58:3128")
    pub server_addr: String,
    /// Timeout before returning 503 to client
    pub timeout_ms: u64,
}

pub struct ClientProxyState {
    config: ProxyConfig,
}

impl ClientProxyState {
    pub fn new(config: ProxyConfig) -> Self {
        Self { config }
    }

    /// Forward request to the server's HTTP forward proxy using raw HTTP/1.1.
    ///
    /// A forward proxy expects the **absolute URI** in the request line:
    ///   `GET http://mirror.example.com/path HTTP/1.1`
    ///
    /// We connect TCP directly to the server proxy and send the request with
    /// the original absolute URI preserved. This is the standard HTTP forward
    /// proxy chaining mechanism (RFC 7230 §5.3.2).
    ///
    /// Returns 503 on any error (no upstream fallback).
    pub async fn forward_to_server(
        &self,
        request: Request<Body>,
    ) -> Result<Response<Body>, anyhow::Error> {
        let method = request.method().clone();
        let uri = request.uri().clone();
        let original_uri = uri.to_string();

        // Package managers configured with a proxy send absolute URIs.
        // If the URI lacks a scheme, we can't forward to the server proxy.
        if uri.scheme().is_none() {
            anyhow::bail!(
                "Client sent non-proxy request (no scheme in URI): {}",
                original_uri
            );
        }

        debug!(
            "Forwarding to server proxy {} : {} {}",
            self.config.server_addr, method, original_uri
        );

        // Connect TCP to the server's forward proxy
        let stream = TcpStream::connect(&self.config.server_addr).await?;
        let io = TokioIo::new(stream);

        // Perform HTTP/1.1 handshake on the raw TCP connection
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

        // Spawn the connection task to drive the HTTP state machine
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("Server proxy connection ended: {}", e);
            }
        });

        // Build the forwarding request.
        // KEY: We preserve the original absolute URI in the request line.
        // hyper's low-level send_request does NOT parse the URI for host extraction
        // (unlike Client::request), so it sends the raw absolute URI to the server.
        let response = sender.send_request(request).await?;

        // Read the full response body so the connection can be reused/closed cleanly
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await?.to_bytes();

        let mut response = Response::from_parts(parts, Body::from(bytes));
        response
            .headers_mut()
            .insert("X-Monopsony", "forwarded".parse().unwrap());

        Ok(response)
    }
}

async fn proxy_handler(
    State(state): State<Arc<ClientProxyState>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let timeout = Duration::from_millis(state.config.timeout_ms);

    match tokio::time::timeout(timeout, state.forward_to_server(request)).await {
        Ok(Ok(response)) => {
            debug!("Server proxy response: {:?}", response.status());
            response
        }
        Ok(Err(e)) => {
            error!("Server proxy request failed: {}", e);
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Server proxy unavailable - please retry later"))
                .unwrap_or_else(|_| Response::new(Body::from("Service unavailable")))
        }
        Err(_) => {
            warn!("Server proxy request timed out");
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Server proxy timed out - please retry later"))
                .unwrap_or_else(|_| Response::new(Body::from("Service unavailable")))
        }
    }
}

pub async fn run_proxy(listen_addr: SocketAddr, config: ProxyConfig) -> anyhow::Result<()> {
    let state = Arc::new(ClientProxyState::new(config));

    let app = axum::Router::new()
        .route("/*path", axum::routing::any(proxy_handler))
        .route("/", axum::routing::any(proxy_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    info!("Client proxy listening on {}", listen_addr);

    axum::serve(listener, app).await?;

    Ok(())
}
