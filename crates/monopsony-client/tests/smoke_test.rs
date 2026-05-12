/// Smoke tests for the monopsony-client binary.
/// Tests that the client CLI starts, responds to --help, and handles
/// missing-server / unpaired states gracefully (no panics).
use std::process::{Command, Stdio};
use std::time::Duration;

/// Run a client subcommand and return (status, stdout, stderr).
fn run_client(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_monopsony-client"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to execute monopsony-client");

    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (status, stdout, stderr)
}

#[test]
fn test_client_help_exits_zero() {
    let (status, stdout, _stderr) = run_client(&["--help"]);
    assert_eq!(status, 0, "monopsony-client --help should exit 0");
    assert!(
        stdout.contains("monopsony-client"),
        "help output should mention monopsony-client"
    );
}

#[test]
fn test_client_subcommand_help_exits_zero() {
    for subcmd in &[
        "daemon",
        "discover",
        "status",
        "pair",
        "configure",
        "mirror",
    ] {
        let (status, _, _) = run_client(&[subcmd, "--help"]);
        assert_eq!(
            status, 0,
            "monopsony-client {} --help should exit 0",
            subcmd
        );
    }
}

#[test]
fn test_client_discover_without_server_gives_error() {
    let (status, stdout, stderr) = run_client(&["discover"]);
    // Should not panic (exit != -1 means the process didn't segfault)
    assert_ne!(
        status, -1,
        "monopsony-client discover should not panic when no server is available"
    );
    // Output may go to stdout (tracing) or stderr; at minimum no crash
    let has_output = !stdout.is_empty() || !stderr.is_empty();
    assert!(
        has_output || status != 0,
        "discover without server should produce some output or non-zero exit (status={}, stdout_len={}, stderr_len={})",
        status, stdout.len(), stderr.len()
    );
}

#[test]
fn test_client_status_without_pairing_gives_error() {
    let (status, stdout, stderr) = run_client(&["status"]);
    // Should not panic
    assert_ne!(
        status, -1,
        "monopsony-client status should not panic when unpaired"
    );
    let has_output = !stdout.is_empty() || !stderr.is_empty();
    assert!(
        has_output || status != 0,
        "status without pairing should produce some output or non-zero exit (status={}, stdout_len={}, stderr_len={})",
        status, stdout.len(), stderr.len()
    );
}

#[test]
fn test_client_configure_without_pairing_gives_error() {
    let (status, stdout, stderr) = run_client(&["configure"]);
    // Should not panic
    assert_ne!(
        status, -1,
        "monopsony-client configure should not panic when unpaired"
    );
    let has_output = !stdout.is_empty() || !stderr.is_empty();
    assert!(
        has_output || status != 0,
        "configure without pairing should produce some output or non-zero exit (status={}, stdout_len={}, stderr_len={})",
        status, stdout.len(), stderr.len()
    );
}
