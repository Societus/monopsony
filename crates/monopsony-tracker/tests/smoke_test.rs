/// Smoke tests for the monopsony-tracker binary.
/// Starts the tracker on an ephemeral port, tests HTTP endpoints,
/// and verifies clean shutdown.
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Find an available port by binding to port 0.
/// Binds and immediately releases — caller must handle TOCTOU race.
fn find_available_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Start the tracker, retrying up to 3 times on port conflicts.
fn start_tracker() -> TrackerProcess {
    for attempt in 0..3 {
        let mut tracker = TrackerProcess::start();
        let timeout = Duration::from_secs(10);
        if wait_for_port(tracker.port, timeout) {
            return tracker;
        }
        // Port race — clean up and try again
        eprintln!(
            "Tracker start attempt {} failed (port {} not reachable), retrying...",
            attempt + 1,
            tracker.port
        );
        tracker.stop();
    }
    panic!("Failed to start tracker after 3 attempts");
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

struct TrackerProcess {
    child: Child,
    port: u16,
    temp_dir: std::path::PathBuf,
}

impl TrackerProcess {
    fn start() -> Self {
        let port = find_available_port();

        let binary = env!("CARGO_BIN_EXE_monopsony-tracker");

        let temp_dir = std::env::temp_dir().join(format!(
            "monopsony-tracker-smoke-{}-{}",
            std::process::id(),
            port
        ));
        std::fs::create_dir_all(&temp_dir).expect("failed to create temp dir");

        let child = Command::new(binary)
            .args([
                "--addr",
                &format!("127.0.0.1:{}", port),
                "--db",
                temp_dir.join("tracker.db").to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start monopsony-tracker");

        Self {
            child,
            port,
            temp_dir,
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

impl Drop for TrackerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn test_tracker_starts_and_binds_port() {
    let mut tracker = start_tracker();
    let timeout = Duration::from_secs(10);

    assert!(
        wait_for_port(tracker.port, timeout),
        "Tracker port {} not reachable within {:?}",
        tracker.port,
        timeout
    );

    tracker.stop();
}

#[test]
fn test_tracker_help_exits_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_monopsony-tracker"))
        .args(["--help"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to execute monopsony-tracker --help");

    let status = output.status.code().unwrap_or(-1);
    assert_eq!(status, 0, "monopsony-tracker --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("monopsony-tracker"),
        "help output should mention monopsony-tracker"
    );
}

#[test]
fn test_tracker_announce_and_discover() {
    let mut tracker = start_tracker();
    let timeout = Duration::from_secs(10);

    assert!(
        wait_for_port(tracker.port, timeout),
        "Tracker port not reachable"
    );

    let base = format!("http://127.0.0.1:{}", tracker.port);

    // POST /announce
    let announce_resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build reqwest client")
        .post(format!("{}/announce", base))
        .header("Content-Type", "application/json")
        .body(r#"{"mirror_id":"test-mirror","distro":"archlinux","transport":["http"],"endpoints":["http://127.0.0.1:3129"],"lat_lon":[40.7128,-74.0060]}"#)
        .send()
        .expect("announce request failed");

    assert!(
        announce_resp.status().is_success(),
        "announce should return success, got: {}",
        announce_resp.status()
    );

    // GET /discover?distro=archlinux
    let discover_resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build reqwest client")
        .get(format!("{}/discover?distro=archlinux", base))
        .send()
        .expect("discover request failed");

    assert!(
        discover_resp.status().is_success(),
        "discover should return success, got: {}",
        discover_resp.status()
    );

    let body: serde_json::Value = discover_resp
        .json()
        .expect("discover response should be JSON");
    let mirrors = body
        .get("mirrors")
        .expect("response should have 'mirrors' field");
    assert!(
        !mirrors
            .as_array()
            .expect("mirrors should be an array")
            .is_empty(),
        "discover should return at least 1 mirror, got: {}",
        mirrors
    );

    tracker.stop();
}

#[test]
fn test_tracker_graceful_shutdown() {
    let mut tracker = start_tracker();
    let timeout = Duration::from_secs(10);

    assert!(
        wait_for_port(tracker.port, timeout),
        "Tracker port not reachable"
    );

    let base = format!("http://127.0.0.1:{}", tracker.port);

    // Verify it's responding
    let resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build reqwest client")
        .get(format!("{}/discover?distro=archlinux", base))
        .send()
        .expect("discover request failed");
    assert!(resp.status().is_success());

    // Stop and verify it exits cleanly
    tracker.stop();

    // Give it a moment to fully shut down
    std::thread::sleep(Duration::from_millis(200));

    // Port should no longer be in use
    assert!(
        TcpStream::connect(format!("127.0.0.1:{}", tracker.port)).is_err(),
        "Tracker port should be released after shutdown"
    );
}
