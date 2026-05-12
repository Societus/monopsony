/// Smoke test: starts the server binary with --no-db on ephemeral ports,
/// verifies it binds and responds to health endpoint, then shuts down.
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Find an available port by binding to port 0.
fn find_available_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Wait for a TCP port to become connectable, with timeout.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct ServerProcess {
    child: Child,
    http_port: u16,
    metrics_port: u16,
    mirror_port: u16,
    grpc_port: u16,
    temp_dir: std::path::PathBuf,
}

impl ServerProcess {
    fn start() -> Self {
        let http_port = find_available_port();
        let metrics_port = find_available_port();
        let mirror_port = find_available_port();
        let grpc_port = find_available_port();

        let binary = env!("CARGO_BIN_EXE_monopsony-server");

        let temp_dir = std::env::temp_dir().join(format!(
            "monopsony-smoke-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).expect("failed to create temp dir");

        let child = Command::new(binary)
            .args([
                "--no-db",
                "--http-addr",
                &format!("127.0.0.1:{}", http_port),
                "--metrics-addr",
                &format!("127.0.0.1:{}", metrics_port),
                "--mirror-addr",
                &format!("127.0.0.1:{}", mirror_port),
                "--grpc-addr",
                &format!("127.0.0.1:{}", grpc_port),
                "--cache-dir",
                temp_dir.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start monopsony-server");

        Self {
            child,
            http_port,
            metrics_port,
            mirror_port,
            grpc_port,
            temp_dir,
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn test_server_starts_and_binds_ports() {
    let mut server = ServerProcess::start();
    let timeout = Duration::from_secs(10);

    // Core ports that always bind:
    assert!(
        wait_for_port(server.http_port, timeout),
        "HTTP proxy port {} not reachable within {:?}",
        server.http_port,
        timeout
    );
    assert!(
        wait_for_port(server.metrics_port, timeout),
        "Metrics port {} not reachable within {:?}",
        server.metrics_port,
        timeout
    );

    // Mirror and gRPC ports require a database. With --no-db they don't start.
    // Just log their status.
    if wait_for_port(server.mirror_port, Duration::from_secs(2)) {
        eprintln!("  Mirror port {} is up", server.mirror_port);
    } else {
        eprintln!("  Mirror port {} not bound (requires database)", server.mirror_port);
    }
    if wait_for_port(server.grpc_port, Duration::from_secs(2)) {
        eprintln!("  gRPC port {} is up", server.grpc_port);
    } else {
        eprintln!("  gRPC port {} not bound (requires database)", server.grpc_port);
    }

    server.stop();
}

#[tokio::test]
async fn test_server_health_endpoint() {
    let mut server = ServerProcess::start();
    let timeout = Duration::from_secs(10);

    assert!(
        wait_for_port(server.metrics_port, timeout),
        "Metrics port not reachable"
    );

    let url = format!("http://127.0.0.1:{}/health", server.metrics_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build reqwest client");

    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to reach health endpoint");

    assert!(
        resp.status().is_success(),
        "Health endpoint returned non-success: {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("health response should be JSON");
    assert_eq!(body["status"], "healthy", "Health status should be 'healthy'");

    server.stop();
}

#[tokio::test]
async fn test_server_proxy_passes_through_non_package_urls() {
    let mut server = ServerProcess::start();
    let timeout = Duration::from_secs(10);

    assert!(
        wait_for_port(server.http_port, timeout),
        "HTTP proxy port not reachable"
    );

    let url = format!("http://127.0.0.1:{}/", server.http_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build reqwest client");

    let result = client.get(&url).send().await;

    // Server should respond (even with an error) but not crash.
    match result {
        Ok(resp) => {
            assert!(
                resp.status().as_u16() >= 400,
                "Expected error status for root path, got: {}",
                resp.status()
            );
        }
        Err(_) => {
            // Timeout/connection refused also acceptable
        }
    }

    // Verify server is still alive
    assert!(
        wait_for_port(server.metrics_port, Duration::from_secs(2)),
        "Server died after proxy request"
    );

    server.stop();
}
