# Slice 4 spec — Manual serve loop with CONNECT intercept (both proxies)

FIRST read `docs/plans/sprint19-phase1/context-pack-common.md`, then this. This
slice REPLACES the middleware approach, which is proven unworkable (see §Root cause).

## Root cause (fully diagnosed, do not re-derive)
1. Authority-form CONNECT URIs have an empty path → axum router needs
   `.fallback(handler)` to dispatch them (already added, KEEP).
2. axum 0.7's `RouteFuture::poll` (crates registry: axum-0.7.9/src/routing/route.rs
   ~line 216 `set_content_length`) injects `Content-Length` into EVERY top-level
   route/fallback response. It runs OUTSIDE any Router-level middleware, so it
   cannot be stripped by `.layer(...)` (verified empirically: middleware saw
   empty headers, header still appeared).
3. hyper's h1 encoder FORBIDS Content-Length/Transfer-Encoding on a 2xx response
   to CONNECT (RFC 7231; hyper-1.8.1/src/proto/h1/role.rs:376 "Sending
   content-length ... on 2xx response to CONNECT is forbidden") → encode error
   `UnexpectedHeader` → connection closes → `hyper::upgrade::on` yields
   "operation was canceled" → client sees connection-closed-before-200.
4. Raw hyper `serve_connection(...).with_upgrades()` and hyper-util
   `auto::Builder::serve_connection_with_upgrades` with a PLAIN service work
   perfectly (verified by tests/connect_repro_raw.rs and connect_repro_auto.rs —
   both PASS, tunnel echoes). Only the axum Router path fails.

## The fix
Replace `axum::serve(...)` with a manual accept loop in BOTH proxies. A thin
hyper service intercepts CONNECT before the axum Router; everything else is
forwarded into the Router unchanged.

### Pattern (works — proven by connect_repro_auto.rs)
```rust
loop {
    let (stream, _peer) = listener.accept().await?;
    let svc = ProxyHyperService { router: app.clone(), /* state, limiter, auth_state */ };
    tokio::spawn(async move {
        let _ = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::new())
            .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(stream), svc)
            .await;
    });
}
```
```rust
struct ProxyHyperService { router: axum::Router, /* ... */ }

impl hyper::service::Service<Request<hyper::body::Incoming>> for ProxyHyperService {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<hyper::body::Incoming>) -> Self::Future {
        if req.method() == http::Method::CONNECT {
            // Tunnel path: bypass the Router entirely (its RouteFuture would
            // inject Content-Length and kill the tunnel). Convert Incoming ->
            // axum Body and run the CONNECT handler on the raw request.
            let (parts, incoming) = req.into_parts();
            let req = Request::from_parts(parts, Body::new(incoming));
            let fut = handle_connect(/* args from self */ req);
            Box::pin(async move { Ok(fut.await) })
        } else {
            // Normal path: through the axum Router (routes + auth middleware
            // + cache logic untouched).
            let mut router = self.router.clone();
            Box::pin(async move {
                let (parts, incoming) = req.into_parts();
                let req = Request::from_parts(parts, Body::new(incoming));
                use tower::Service; // Router<()>: tower Service, Error = Infallible
                let res = router.call(req).await.unwrap_or_else(|e| match e {});
                Ok(res)
            })
        }
    }
}
```
NOTE: `Router::call` needs `tower::Service` in scope; Router's error type is
Infallible so `unwrap_or_else(|e| match e {})` is total. The Router must be
`Router<()>` (state fully applied — `create_proxy_router` already returns that;
`with_state((state, rate_limiter))` is applied inside it).
Clone the Router per connection (Router clone is cheap, Arc-based).

### Server side (crates/monopsony-proxy/src/proxy.rs)
- Add `pub async fn serve_proxy_router(listener: tokio::net::TcpListener,
  app: axum::Router, state: Arc<ProxyState>,
  rate_limiter: Option<Arc<crate::security::RateLimiter>>,
  auth_state: Arc<monopsony_control::auth::AuthState>) -> anyhow::Result<()>`
  implementing the loop above.
- Move CONNECT handling into a standalone `async fn handle_connect_request(
  state, rate_limiter, auth_state, peer_addr: SocketAddr, req: Request<Body>)
  -> Response<Body>`:
  - acquire the rate-limiter slot per CONNECT (same acquire as
    handle_proxy_request does; release via the existing ConnectSlotGuard moved
    into the tunnel task),
  - run the SAME auth middleware the Router would have run for this request —
    call `monopsony_control::auth::auth_middleware` manually:
    build `Next` with a passthrough service that calls the CONNECT handler:
    `let svc = tower::service_fn(move |req: Request<Body>| async move { Ok::<_, Infallible>(connect_inner(req).await) });`
    `let next = Next::new(BoxCloneService::new(svc));` (import from
    axum::middleware::Next / tower::util::BoxCloneService; match the exact
    signature used at proxy.rs line ~200 where from_fn_with_state wires it —
    read auth_middleware's signature first and mirror it).
    If auth_middleware is not practically callable, replicate its check inline
    for AuthMode::Open (no-op) and Token/mTLS (reject 401/403) — read
    crates/monopsony-control/src/auth.rs and mirror ONLY what it does for a
    request without/with valid credentials. Keep it minimal but honest.
  - then: enabled/deny/SSRF/dial checks (already in handle_connect) + tunnel.
- `run_proxy` uses `serve_proxy_router` instead of `axum::serve`.
- DELETE `strip_connect_length_headers` middleware + its `.layer(...)` wiring
  (wrong layer, dead weight).
- REMOVE all `eprintln!("DBG-CONNECT ...")` debug lines (there are 3).

### Client side (crates/monopsony-client/src/proxy.rs)
- Same manual loop in `run_proxy` (no auth middleware on client — CONNECT
  handler: `handle_connect(state, req)`; else Router). The client Router is
  built inside run_proxy; hoist it so both paths share it.
- DELETE the client `strip_connect_length_headers` + its `.layer(...)`.
- Client `handle_connect`/`forward_connect_to_server` stay as-is (they're correct).

### Tests (crates/monopsony-proxy/tests/streaming_tests.rs)
- `spawn_proxy_router` currently does `axum::serve(...)` — switch it to
  `monopsony_proxy::proxy::serve_proxy_router(listener, app, state, None,
  auth_state)` so tests exercise the REAL serve path.
- No other test changes; the 4 CONNECT tests should flip green.

### Cleanup
- `git checkout` / delete these repro files (do NOT commit them):
  crates/monopsony-proxy/tests/connect_repro.rs,
  connect_repro_raw.rs, connect_repro_auto.rs, connect_repro_axauto.rs

## Verify ONCE, from repo root:
cargo test -p monopsony-proxy --test streaming_tests 2>&1 | tail -5 && cargo check -p monopsony-client 2>&1 | tail -3 && cargo test -p monopsony-client 2>&1 | grep -E "test result|FAILED" | head -6

## Report honestly: files changed, real outputs, not_done, deviations. NO COMMITS.
