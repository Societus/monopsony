use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode, Uri},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tracing::{debug, error, info, warn};

pub struct ProxyConfig {
    pub server_addr: String,
    pub fallback_timeout_ms: u64,
}

pub struct ClientProxyState {
    config: ProxyConfig,
    http_client: Client<HttpConnector, Body>,
    /// URL patterns from available package manager backends.
    /// If empty, all traffic is passed through directly.
    url_patterns: Vec<monopsony_common::package_manager::UrlPattern>,
}

impl ClientProxyState {
    pub fn new(config: ProxyConfig) -> Self {
        let http_client = Client::builder(TokioExecutor::new()).build_http();
        let url_patterns = monopsony_common::package_manager::get_available_backends()
            .into_iter()
            .flat_map(|b| b.upstream_url_patterns())
            .collect();
        Self {
            config,
            http_client,
            url_patterns,
        }
    }

    pub async fn forward_to_server(
        &self,
        request: Request<Body>,
    ) -> Result<Response<Body>, anyhow::Error> {
        let uri = request.uri().clone();
        let server_url = format!("http://{}{}", self.config.server_addr, uri);

        let mut server_request = Request::builder()
            .method(request.method().clone())
            .uri(&server_url);

        for (name, value) in request.headers() {
            if matches!(
                name.as_str(),
                "host" | "connection" | "keep-alive" | "proxy-authenticate"
                    | "proxy-authorization" | "te" | "trailers" | "transfer-encoding"
                    | "upgrade"
            ) {
                continue;
            }
            server_request = server_request.header(name, value);
        }

        let server_request = server_request.body(request.into_body())?;

        debug!("Forwarding to server: {}", server_url);
        let response = self.http_client.request(server_request).await?;

        let (parts, body) = response.into_parts();
        let bytes = body.collect().await?.to_bytes();
        Ok(Response::from_parts(parts, Body::from(bytes)))
    }

    pub async fn fetch_upstream_direct(
        &self,
        request: Request<Body>,
    ) -> Result<Response<Body>, anyhow::Error> {
        let uri = request.uri().clone();
        let url = if uri.scheme().is_some() {
            uri.to_string()
        } else {
            format!("http://{}", uri)
        };

        let mut upstream_request = Request::builder()
            .method(request.method().clone())
            .uri(&url);

        for (name, value) in request.headers() {
            if matches!(
                name.as_str(),
                "host" | "connection" | "keep-alive" | "proxy-authenticate"
                    | "proxy-authorization" | "te" | "trailers" | "transfer-encoding"
                    | "upgrade"
            ) {
                continue;
            }
            upstream_request = upstream_request.header(name, value);
        }

        let upstream_request = upstream_request.body(request.into_body())?;

        debug!("Fetching upstream directly: {}", url);
        let response = self.http_client.request(upstream_request).await?;

        let (parts, body) = response.into_parts();
        let bytes = body.collect().await?.to_bytes();
        Ok(Response::from_parts(parts, Body::from(bytes)))
    }
}

async fn proxy_handler(
    State(state): State<Arc<ClientProxyState>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let method = request.method().clone();
    let uri = request.uri().clone();

    // Build a full URL for pattern matching (incoming URI may lack scheme/host)
    let full_url = match (uri.scheme_str(), uri.host()) {
        (Some(scheme), Some(host)) => {
            let port = uri.port_u16().map(|p| format!(":{}", p)).unwrap_or_default();
            format!("{}://{}{}{}", scheme, host, port, uri.path())
        }
        _ => uri.to_string(),
    };

    let is_package_url = !state.url_patterns.is_empty()
        && state.url_patterns.iter().any(|p| p.matches(&full_url));

    if !is_package_url {
        debug!("Passing through non-package URL: {}", full_url);
        return match state.fetch_upstream_direct(request).await {
            Ok(resp) => resp.into_response(),
            Err(e) => {
                error!("Passthrough failed for {}: {}", full_url, e);
                Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("Upstream passthrough failed"))
                    .unwrap_or_else(|_| {
                        Response::new(Body::from("Upstream passthrough failed"))
                    })
            }
        };
    }

    debug!("Client proxy request (package): {} {}", method, uri);

    let timeout = Duration::from_millis(state.config.fallback_timeout_ms);

    match tokio::time::timeout(timeout, state.forward_to_server(request)).await {
        Ok(Ok(response)) if response.status().is_success() => {
            debug!("Server cache HIT for: {}", uri);
            let (parts, body) = response.into_parts();
            let mut response = Response::from_parts(parts, body);
            response
                .headers_mut()
                .insert("X-Monopsony", "HIT".parse().unwrap());
            response
        }
        Ok(Ok(response)) => {
            let status = response.status();
            if status.is_server_error() || status == StatusCode::BAD_GATEWAY {
                warn!(
                    "Server returned {}, falling back to upstream for: {}",
                    status, uri
                );
                let request = rebuild_request(method, &uri, response);
                match state.fetch_upstream_direct(request).await {
                    Ok(upstream_response) => {
                        let (parts, body) = upstream_response.into_parts();
                        let mut resp = Response::from_parts(parts, body);
                        resp.headers_mut()
                            .insert("X-Monopsony", "MISS".parse().unwrap());
                        resp
                    }
                    Err(e) => {
                        error!("Upstream fallback failed for {}: {}", uri, e);
                        Response::builder()
                            .status(StatusCode::BAD_GATEWAY)
                            .body(Body::from("Both server and upstream failed"))
                            .unwrap_or_else(|_| {
                                Response::new(Body::from("Both server and upstream failed"))
                            })
                    }
                }
            } else {
                response
            }
        }
        Ok(Err(e)) => {
            warn!("Server request failed for {}: {}, falling back", uri, e);
            let request = rebuild_request_from_scratch(method, &uri);
            match state.fetch_upstream_direct(request).await {
                Ok(upstream_response) => {
                    let (parts, body) = upstream_response.into_parts();
                    let mut resp = Response::from_parts(parts, body);
                    resp.headers_mut()
                        .insert("X-Monopsony", "MISS".parse().unwrap());
                    resp
                }
                Err(e2) => {
                    error!("Upstream fallback also failed for {}: {}", uri, e2);
                    Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .body(Body::from("Both server and upstream failed"))
                        .unwrap_or_else(|_| {
                            Response::new(Body::from("Both server and upstream failed"))
                        })
                }
            }
        }
        Err(_) => {
            warn!(
                "Server request timed out for: {}, falling back to upstream",
                uri
            );
            let request = rebuild_request_from_scratch(method, &uri);
            match state.fetch_upstream_direct(request).await {
                Ok(upstream_response) => {
                    let (parts, body) = upstream_response.into_parts();
                    let mut resp = Response::from_parts(parts, body);
                    resp.headers_mut()
                        .insert("X-Monopsony", "MISS".parse().unwrap());
                    resp
                }
                Err(e) => {
                    error!("Upstream fallback failed for {}: {}", uri, e);
                    Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .body(Body::from("Server timeout and upstream failed"))
                        .unwrap_or_else(|_| {
                            Response::new(Body::from("Server timeout and upstream failed"))
                        })
                }
            }
        }
    }
}

fn rebuild_request(
    method: axum::http::Method,
    uri: &Uri,
    response: Response<Body>,
) -> Request<Body> {
    let url = if uri.scheme().is_some() {
        uri.to_string()
    } else {
        format!("http://{}", uri)
    };

    let mut builder = Request::builder().method(method).uri(&url);
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    builder.body(response.into_body()).unwrap()
}

fn rebuild_request_from_scratch(method: axum::http::Method, uri: &Uri) -> Request<Body> {
    let url = if uri.scheme().is_some() {
        uri.to_string()
    } else {
        format!("http://{}", uri)
    };
    Request::builder()
        .method(method)
        .uri(&url)
        .body(Body::empty())
        .unwrap()
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
