//! WebSocket transport for gRPC — bridges a WebSocket connection into
//! hyper's Read + Write traits so tonic can use it as a raw transport.
//!
//! The client opens a WSS connection to the server's `/ws/tunnel` endpoint,
//! then runs its full mTLS handshake (Monopsony CA, client cert) inside the
//! WebSocket as an inner TLS layer. Cloudflare or any reverse proxy only
//! sees opaque binary frames.

use futures_util::{SinkExt, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

/// A bidirectional byte stream backed by a WebSocket connection.
///
/// Implements hyper::rt::Read + hyper::rt::Write so it can be used directly
/// as a tonic connector target.
pub struct WsTransport {
    rx: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    tx: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsMessage,
    >,
    read_buf: Vec<u8>,
    closed: bool,
}

impl WsTransport {
    /// Connect to a WebSocket URL (ws:// or wss://).
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        debug!("WS transport: connecting to {}", url);

        let (ws_stream, response) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connect failed: {}", e))?;

        debug!("WS transport: connected, HTTP {}", response.status());

        let (tx, rx) = ws_stream.split();

        Ok(Self {
            rx,
            tx,
            read_buf: Vec::with_capacity(8192),
            closed: false,
        })
    }
}

/// Helper to read from the internal buffer + WebSocket into a generic byte slice.
fn fill_from_ws(
    rx: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    read_buf: &mut Vec<u8>,
    out: &mut [u8],
    cx: &mut Context<'_>,
) -> Poll<std::io::Result<usize>> {
    // If we have buffered data, serve from it first
    if !read_buf.is_empty() {
        let n = std::cmp::min(read_buf.len(), out.len());
        out[..n].copy_from_slice(&read_buf[..n]);
        read_buf.drain(..n);
        return Poll::Ready(Ok(n));
    }

    // Poll the WebSocket for more frames
    match rx.poll_next_unpin(cx) {
        Poll::Ready(Some(Ok(msg))) => {
            match msg {
                WsMessage::Binary(data) => {
                    let n = std::cmp::min(data.len(), out.len());
                    out[..n].copy_from_slice(&data[..n]);
                    // Buffer any excess
                    if n < data.len() {
                        read_buf.extend_from_slice(&data[n..]);
                    }
                    Poll::Ready(Ok(n))
                }
                WsMessage::Ping(_) | WsMessage::Pong(_) => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                WsMessage::Close(_) => {
                    debug!("WS transport: received close frame");
                    Poll::Ready(Ok(0))
                }
                _ => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }
        Poll::Ready(Some(Err(e))) => Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            format!("WebSocket error: {}", e),
        ))),
        Poll::Ready(None) => {
            debug!("WS transport: stream ended");
            Poll::Ready(Ok(0))
        }
        Poll::Pending => Poll::Pending,
    }
}

impl hyper::rt::Read for WsTransport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.closed {
            return Poll::Ready(Ok(()));
        }

        let remaining = buf.remaining();
        if remaining == 0 {
            return Poll::Ready(Ok(()));
        }

        let mut tmp = vec![0u8; remaining];

        match fill_from_ws(&mut this.rx, &mut this.read_buf, &mut tmp, cx) {
            Poll::Ready(Ok(0)) => {
                this.closed = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(n)) => {
                buf.put_slice(&tmp[..n]);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl hyper::rt::Write for WsTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.closed {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "WebSocket is closed",
            )));
        }

        match self.tx.poll_ready_unpin(cx) {
            Poll::Ready(Ok(())) => {
                let data = buf.to_vec();
                match self.tx.start_send_unpin(WsMessage::Binary(data)) {
                    Ok(()) => Poll::Ready(Ok(buf.len())),
                    Err(e) => {
                        warn!("WS transport: send error: {}", e);
                        self.closed = true;
                        Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            format!("WebSocket send error: {}", e),
                        )))
                    }
                }
            }
            Poll::Ready(Err(e)) => {
                self.closed = true;
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    format!("WebSocket poll_ready error: {}", e),
                )))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.tx.poll_flush_unpin(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(std::io::Error::other(format!(
                "WebSocket flush error: {}",
                e
            )))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.tx.poll_close_unpin(cx) {
            Poll::Ready(Ok(())) => {
                self.closed = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(std::io::Error::other(format!(
                "WebSocket close error: {}",
                e
            )))),
            Poll::Pending => Poll::Pending,
        }
    }
}
